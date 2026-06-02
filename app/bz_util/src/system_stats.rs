/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use crate::threads::available_parallelism;

use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use crate::os::host_cpu_usage::HostCpuUsage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemMemoryStats {
    pub total: u64,
    pub available: u64,
}

pub struct UnixSystemStats {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
}

impl UnixSystemStats {
    #[cfg(unix)]
    pub fn get() -> Option<Self> {
        let mut loadavg: [f64; 3] = [0.0, 0.0, 0.0];
        if unsafe { libc::getloadavg(&mut loadavg[0], 3) } != 3 {
            // This doesn't seem to set errno (or at least it's not documented to do so).
            return None;
        }
        Some(Self {
            load1: loadavg[0],
            load5: loadavg[1],
            load15: loadavg[2],
        })
    }

    #[cfg(not(unix))]
    pub fn get() -> Option<Self> {
        None
    }
}

/// Returns the number of CPU cores on the system.
#[cfg(unix)]
pub fn num_cores() -> usize {
    use std::sync::OnceLock;
    static NUM_CORES: OnceLock<usize> = OnceLock::new();
    *NUM_CORES.get_or_init(|| {
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if n < 1 {
            available_parallelism()
        } else {
            n as usize
        }
    })
}

#[cfg(not(unix))]
pub fn num_cores() -> usize {
    available_parallelism()
}

pub fn system_memory_stats() -> u64 {
    system_memory_stats_detailed().total
}

pub fn system_memory_stats_detailed() -> SystemMemoryStats {
    if let Ok(Some(bytes)) = bz_env::env::bz_env!("BUCK2_TEST_FAKE_SYSTEM_TOTAL_MEMORY", type=u64, applicability=testing)
    {
        let available = bz_env::env::bz_env!("BUCK2_TEST_FAKE_SYSTEM_AVAILABLE_MEMORY", type=u64, applicability=testing)
            .ok()
            .flatten()
            .unwrap_or(bytes);
        return SystemMemoryStats {
            total: bytes,
            available,
        };
    }

    use sysinfo::MemoryRefreshKind;
    use sysinfo::RefreshKind;
    use sysinfo::System;

    let system = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
    );
    SystemMemoryStats {
        total: system.total_memory(),
        available: system.available_memory(),
    }
}

#[derive(Debug)]
struct SystemCpuUsageSample {
    timestamp: Instant,
    usage: HostCpuUsage,
}

pub fn system_cpu_usage() -> Option<f64> {
    static PREVIOUS: OnceLock<Mutex<Option<SystemCpuUsageSample>>> = OnceLock::new();

    let current = SystemCpuUsageSample {
        timestamp: Instant::now(),
        usage: HostCpuUsage::get().ok()?,
    };

    let mut previous = PREVIOUS.get_or_init(|| Mutex::new(None)).lock().ok()?;
    let previous = previous.replace(SystemCpuUsageSample {
        timestamp: current.timestamp,
        usage: current.usage.clone(),
    })?;

    system_cpu_usage_from_samples(&previous, &current)
}

fn system_cpu_usage_from_samples(
    previous: &SystemCpuUsageSample,
    current: &SystemCpuUsageSample,
) -> Option<f64> {
    let elapsed = current
        .timestamp
        .checked_duration_since(previous.timestamp)?;
    let elapsed_millis = duration_millis(elapsed)?;
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

fn duration_millis(duration: Duration) -> Option<u64> {
    duration.as_millis().try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::SystemCpuUsageSample;
    use super::system_cpu_usage_from_samples;
    use super::system_memory_stats;
    use super::system_memory_stats_detailed;
    use crate::os::host_cpu_usage::HostCpuUsage;
    use std::time::Duration;
    use std::time::Instant;

    #[test]
    fn get_system_memory_stats() {
        let total_mem = system_memory_stats();
        // sysinfo returns zero when fails to retrieve data
        assert!(total_mem > 0);
    }

    #[test]
    fn get_detailed_system_memory_stats() {
        let memory = system_memory_stats_detailed();
        // sysinfo returns zero when fails to retrieve data
        assert!(memory.total > 0);
        assert!(memory.available > 0);
    }

    #[test]
    fn get_system_cpu_usage_from_samples() {
        let start = Instant::now();
        let previous = SystemCpuUsageSample {
            timestamp: start,
            usage: HostCpuUsage {
                user_millis: 1000,
                system_millis: 500,
            },
        };
        let current = SystemCpuUsageSample {
            timestamp: start + Duration::from_millis(100),
            usage: HostCpuUsage {
                user_millis: 1250,
                system_millis: 650,
            },
        };

        assert_eq!(
            system_cpu_usage_from_samples(&previous, &current),
            Some(4.0)
        );
    }
}
