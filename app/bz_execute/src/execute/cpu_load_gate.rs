use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use bz_util::os::host_cpu_usage::HostCpuUsage;
use once_cell::sync::Lazy;

const CPU_LOAD_POLL_INTERVAL: Duration = Duration::from_millis(50);
const BAZEL_CPU_MIN_NECESSARY_RATIO: f64 = 0.6;
const REMOTE_ACTION_BUILDING_CPU: f64 = 1.0;
const MAX_TASKS_PER_CPU: usize = 3;

static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static CPU_USAGE_SAMPLER: Lazy<Mutex<CpuUsageSampler>> =
    Lazy::new(|| Mutex::new(CpuUsageSampler::default()));

struct CpuUsageSample {
    timestamp: Instant,
    usage: HostCpuUsage,
}

#[derive(Default)]
struct CpuUsageSampler {
    previous: Option<CpuUsageSample>,
    last_usage: Option<f64>,
}

enum CpuUsage {
    Available(f64),
    Pending,
    Unavailable,
}

pub(crate) struct RemoteActionBuildingCpuPermit;

impl Drop for RemoteActionBuildingCpuPermit {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) async fn acquire_remote_action_building_cpu_permit() -> RemoteActionBuildingCpuPermit {
    loop {
        let available_cpus = bz_util::threads::available_parallelism().max(1);
        let in_flight = IN_FLIGHT.load(Ordering::Acquire);
        let cpu_usage = sample_cpu_usage();
        if in_flight > 0 && matches!(cpu_usage, CpuUsage::Pending) {
            tokio::time::sleep(CPU_LOAD_POLL_INTERVAL).await;
            continue;
        }
        let cpu_usage = match cpu_usage {
            CpuUsage::Available(usage) => Some(usage),
            CpuUsage::Pending | CpuUsage::Unavailable => None,
        };

        if remote_action_building_cpu_available(available_cpus, in_flight, cpu_usage)
            && IN_FLIGHT
                .compare_exchange(
                    in_flight,
                    in_flight + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        {
            return RemoteActionBuildingCpuPermit;
        }

        tokio::time::sleep(CPU_LOAD_POLL_INTERVAL).await;
    }
}

fn sample_cpu_usage() -> CpuUsage {
    let current = CpuUsageSample {
        timestamp: Instant::now(),
        usage: match HostCpuUsage::get() {
            Ok(usage) => usage,
            Err(_) => return CpuUsage::Unavailable,
        },
    };

    let Ok(mut sampler) = CPU_USAGE_SAMPLER.lock() else {
        return CpuUsage::Unavailable;
    };
    let usage = sampler
        .previous
        .as_ref()
        .and_then(|previous| cpu_usage_from_samples(previous, &current));
    sampler.previous = Some(current);

    if let Some(usage) = usage
        && usage.is_finite()
    {
        sampler.last_usage = Some(usage);
    }

    if let Some(usage) = sampler.last_usage {
        CpuUsage::Available(usage)
    } else {
        CpuUsage::Pending
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

fn remote_action_building_cpu_available(
    available_cpus: usize,
    in_flight: usize,
    cpu_usage: Option<f64>,
) -> bool {
    let available_cpus = available_cpus.max(1);
    if in_flight == 0 {
        return true;
    }
    if in_flight >= available_cpus.saturating_mul(MAX_TASKS_PER_CPU) {
        return false;
    }

    let Some(cpu_usage) = cpu_usage else {
        return true;
    };
    if !cpu_usage.is_finite() {
        return true;
    }

    let cpu_usage = cpu_usage.max(0.0);
    let window_estimation = in_flight as f64 * REMOTE_ACTION_BUILDING_CPU;
    let requested = REMOTE_ACTION_BUILDING_CPU * BAZEL_CPU_MIN_NECESSARY_RATIO;

    cpu_usage + window_estimation + requested <= available_cpus as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_first_task_under_high_cpu_pressure() {
        assert!(remote_action_building_cpu_available(10, 0, Some(10.0)));
    }

    #[test]
    fn blocks_additional_tasks_under_high_cpu_pressure() {
        assert!(!remote_action_building_cpu_available(10, 1, Some(9.5)));
    }

    #[test]
    fn accounts_for_in_flight_cpu_window() {
        assert!(remote_action_building_cpu_available(10, 9, Some(0.0)));
        assert!(!remote_action_building_cpu_available(10, 10, Some(0.0)));
    }

    #[test]
    fn caps_in_flight_tasks_at_bazel_max_actions_per_cpu() {
        assert!(!remote_action_building_cpu_available(10, 30, None));
    }
}
