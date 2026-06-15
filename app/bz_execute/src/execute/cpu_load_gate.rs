use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use bz_util::os::host_cpu_usage::HostCpuUsage;
use once_cell::sync::Lazy;
use tokio::sync::Notify;

const CPU_LOAD_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const CPU_LOAD_SCHEDULING_WINDOW: Duration = Duration::from_secs(5);
const BAZEL_CPU_MIN_NECESSARY_RATIO: f64 = 0.6;
const REMOTE_ACTION_BUILDING_CPU: f64 = 1.0;
const MAX_TASKS_PER_CPU: usize = 3;

static RESOURCE_MANAGER: Lazy<RemoteActionBuildingResourceManager> =
    Lazy::new(RemoteActionBuildingResourceManager::new);

struct CpuUsageSample {
    timestamp: Instant,
    usage: HostCpuUsage,
}

#[derive(Default)]
struct CpuUsageSampler {
    previous: Option<CpuUsageSample>,
    last_usage: f64,
}

impl CpuUsageSampler {
    fn current_usage(&mut self, now: Instant) -> f64 {
        if let Some(previous) = self.previous.as_ref()
            && now.duration_since(previous.timestamp) < CPU_LOAD_SAMPLE_INTERVAL
        {
            return self.last_usage;
        }

        let current = CpuUsageSample {
            timestamp: now,
            usage: match HostCpuUsage::get() {
                Ok(usage) => usage,
                Err(_) => return self.last_usage,
            },
        };

        if let Some(previous) = self.previous.as_ref()
            && let Some(usage) = cpu_usage_from_samples(previous, &current)
            && usage.is_finite()
        {
            self.last_usage = usage.max(0.0);
        }
        self.previous = Some(current);
        self.last_usage
    }
}

struct RemoteActionBuildingResourceManager {
    state: Mutex<ResourceManagerState>,
    notify: Notify,
}

impl RemoteActionBuildingResourceManager {
    fn new() -> Self {
        Self {
            state: Mutex::new(ResourceManagerState::new(Instant::now())),
            notify: Notify::new(),
        }
    }

    fn try_acquire(&self) -> AcquireAttempt {
        let now = Instant::now();
        let available_cpus = bz_util::threads::available_parallelism().max(1);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let current_cpu_usage = state.cpu_usage_sampler.current_usage(now);

        if let Some(request_id) =
            state.try_acquire_with_usage(now, available_cpus, current_cpu_usage)
        {
            AcquireAttempt::Granted(request_id)
        } else {
            AcquireAttempt::WaitUntil(state.next_window_update)
        }
    }

    fn release(&self, request_id: usize) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.release(request_id);
        }
        self.notify.notify_waiters();
    }
}

struct ResourceManagerState {
    next_request_id: usize,
    running_actions: usize,
    next_window_update: Instant,
    window_estimation_cpu: f64,
    window_request_ids: HashSet<usize>,
    cpu_usage_sampler: CpuUsageSampler,
}

impl ResourceManagerState {
    fn new(now: Instant) -> Self {
        Self {
            next_request_id: 0,
            running_actions: 0,
            next_window_update: now + CPU_LOAD_SCHEDULING_WINDOW,
            window_estimation_cpu: 0.0,
            window_request_ids: HashSet::new(),
            cpu_usage_sampler: CpuUsageSampler::default(),
        }
    }

    fn roll_window(&mut self, now: Instant) {
        let mut rolled = false;
        while now >= self.next_window_update {
            self.next_window_update = self
                .next_window_update
                .checked_add(CPU_LOAD_SCHEDULING_WINDOW)
                .unwrap_or(now + CPU_LOAD_SCHEDULING_WINDOW);
            rolled = true;
        }
        if rolled {
            self.window_request_ids.clear();
            self.window_estimation_cpu = 0.0;
        }
    }

    fn try_acquire_with_usage(
        &mut self,
        now: Instant,
        available_cpus: usize,
        current_cpu_usage: f64,
    ) -> Option<usize> {
        self.roll_window(now);
        if !self.cpu_available(available_cpus, current_cpu_usage) {
            return None;
        }

        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.running_actions += 1;
        self.window_request_ids.insert(request_id);
        self.window_estimation_cpu += REMOTE_ACTION_BUILDING_CPU;
        Some(request_id)
    }

    fn release(&mut self, request_id: usize) {
        self.running_actions = self.running_actions.saturating_sub(1);
        if self.window_request_ids.remove(&request_id) {
            self.window_estimation_cpu =
                (self.window_estimation_cpu - REMOTE_ACTION_BUILDING_CPU).max(0.0);
        }
    }

    fn cpu_available(&self, available_cpus: usize, current_cpu_usage: f64) -> bool {
        let available_cpus = available_cpus.max(1);
        if self.running_actions >= available_cpus.saturating_mul(MAX_TASKS_PER_CPU) {
            return false;
        }
        if !current_cpu_usage.is_finite() {
            return true;
        }

        let requested = REMOTE_ACTION_BUILDING_CPU * BAZEL_CPU_MIN_NECESSARY_RATIO;
        self.window_estimation_cpu + current_cpu_usage.max(0.0) + requested <= available_cpus as f64
    }
}

enum AcquireAttempt {
    Granted(usize),
    WaitUntil(Instant),
}

pub(crate) struct RemoteActionBuildingCpuPermit {
    request_id: usize,
}

impl Drop for RemoteActionBuildingCpuPermit {
    fn drop(&mut self) {
        RESOURCE_MANAGER.release(self.request_id);
    }
}

pub(crate) async fn acquire_remote_action_building_cpu_permit() -> RemoteActionBuildingCpuPermit {
    loop {
        let notified = RESOURCE_MANAGER.notify.notified();
        match RESOURCE_MANAGER.try_acquire() {
            AcquireAttempt::Granted(request_id) => {
                return RemoteActionBuildingCpuPermit { request_id };
            }
            AcquireAttempt::WaitUntil(next_window_update) => {
                let sleep_for = next_window_update.saturating_duration_since(Instant::now());
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(sleep_for) => {
                        RESOURCE_MANAGER.notify.notify_waiters();
                    }
                }
            }
        }
    }
}

fn cpu_usage_from_samples(previous: &CpuUsageSample, current: &CpuUsageSample) -> Option<f64> {
    let elapsed = current
        .timestamp
        .checked_duration_since(previous.timestamp)?;
    let elapsed_millis: u64 = elapsed.as_millis().try_into().ok()?;
    if elapsed_millis == 0 {
        return None;
    }

    let user_millis = current
        .usage
        .user_millis
        .checked_sub(previous.usage.user_millis)?;
    let system_millis = current
        .usage
        .system_millis
        .checked_sub(previous.usage.system_millis)?;
    let busy_millis = user_millis.checked_add(system_millis)?;

    Some(busy_millis as f64 / elapsed_millis as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_while_window_estimation_is_full() {
        let now = Instant::now();
        let mut state = ResourceManagerState::new(now);

        assert_eq!(state.try_acquire_with_usage(now, 2, 0.1), Some(0));
        assert_eq!(state.try_acquire_with_usage(now, 2, 0.1), None);
    }

    #[test]
    fn blocks_first_task_under_high_cpu_pressure() {
        let now = Instant::now();
        let mut state = ResourceManagerState::new(now);

        assert_eq!(state.try_acquire_with_usage(now, 2, 1.5), None);
    }

    #[test]
    fn blocks_when_cpu_load_is_too_high_after_window_rolls() {
        let now = Instant::now();
        let mut state = ResourceManagerState::new(now);

        assert_eq!(state.try_acquire_with_usage(now, 2, 0.1), Some(0));
        let after_window = now + CPU_LOAD_SCHEDULING_WINDOW;

        assert_eq!(state.try_acquire_with_usage(after_window, 2, 1.5), None);
    }

    #[test]
    fn succeeds_when_cpu_load_is_low_after_window_rolls() {
        let now = Instant::now();
        let mut state = ResourceManagerState::new(now);

        assert_eq!(state.try_acquire_with_usage(now, 2, 0.1), Some(0));
        let after_window = now + CPU_LOAD_SCHEDULING_WINDOW;

        assert_eq!(state.try_acquire_with_usage(after_window, 2, 0.1), Some(1));
    }

    #[test]
    fn caps_running_actions_at_bazel_max_actions_per_cpu() {
        let now = Instant::now();
        let mut state = ResourceManagerState::new(now);

        for i in 0..MAX_TASKS_PER_CPU {
            let window = now + CPU_LOAD_SCHEDULING_WINDOW * (i as u32);
            assert_eq!(state.try_acquire_with_usage(window, 1, 0.0), Some(i));
        }

        let next_window = now + CPU_LOAD_SCHEDULING_WINDOW * (MAX_TASKS_PER_CPU as u32);
        assert_eq!(state.try_acquire_with_usage(next_window, 1, 0.0), None);
    }

    #[test]
    fn release_removes_current_window_estimation() {
        let now = Instant::now();
        let mut state = ResourceManagerState::new(now);

        let request_id = state.try_acquire_with_usage(now, 1, 0.0).unwrap();
        state.release(request_id);

        assert_eq!(state.try_acquire_with_usage(now, 1, 0.0), Some(1));
    }

    #[test]
    fn release_after_window_roll_does_not_underflow_window_estimation() {
        let now = Instant::now();
        let mut state = ResourceManagerState::new(now);

        let request_id = state.try_acquire_with_usage(now, 1, 0.0).unwrap();
        let after_window = now + CPU_LOAD_SCHEDULING_WINDOW;
        assert_eq!(state.try_acquire_with_usage(after_window, 1, 0.0), Some(1));

        state.release(request_id);
        assert_eq!(state.window_estimation_cpu, REMOTE_ACTION_BUILDING_CPU);
    }
}
