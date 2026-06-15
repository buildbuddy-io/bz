use bz_event_observer::humanized::CommaSeparatedCount;
use bz_event_observer::humanized::HumanizedBytesPerSecond;
use bz_event_observer::re_state::ReState;
use bz_event_observer::two_snapshots::TwoSnapshots;
use superconsole::Component;
use superconsole::Dimensions;
use superconsole::DrawMode;
use superconsole::Line;
use superconsole::Lines;
use superconsole::Span;
use superconsole::style::Color;

use crate::subscribers::superconsole::SuperConsoleState;
use crate::subscribers::superconsole::SystemResourceUsage;

const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;

pub(crate) struct ResourceHeader<'s> {
    pub(crate) state: &'s SuperConsoleState,
    pub(crate) re_state: &'s ReState,
    pub(crate) two_snapshots: &'s TwoSnapshots,
}

impl Component for ResourceHeader<'_> {
    type Error = bz_error::Error;

    fn draw_unchecked(&self, _dimensions: Dimensions, mode: DrawMode) -> bz_error::Result<Lines> {
        if matches!(mode, DrawMode::Final) {
            return Ok(Lines::new());
        }

        let system_resource_usage = self.state.system_resource_usage();
        let line = format!(
            "{}  {}  {}  {}",
            render_cpu_usage(self.state),
            render_memory_usage(system_resource_usage),
            render_network_usage(self.re_state, self.two_snapshots, mode),
            render_disk_io_usage(system_resource_usage),
        );
        let stats_line = Line::from_iter([Span::new_colored_lossy(&line, Color::Grey)]);
        Ok(Lines(vec![stats_line]))
    }
}

fn render_network_usage(
    re_state: &ReState,
    two_snapshots: &TwoSnapshots,
    mode: DrawMode,
) -> String {
    re_state
        .render_header(two_snapshots, mode)
        .unwrap_or_else(|| "Upload: 0B  Download: 0B".to_owned())
}

fn render_cpu_usage(state: &SuperConsoleState) -> String {
    let max_cpu_cores = bz_util::system_stats::num_cores();
    if let Some(host_cpu_cores) =
        host_cpu_usage_cores(state.simple_console.observer.two_snapshots())
        && max_cpu_cores > 0
        && host_cpu_cores.is_finite()
    {
        let host_cpu_cores = host_cpu_cores.max(0.0);
        let cpu_percent = host_cpu_cores * 100.0 / max_cpu_cores as f64;
        format!(
            "System CPU: {:.1}/{} cores ({:.0}%)",
            host_cpu_cores,
            max_cpu_cores,
            cpu_percent.max(0.0),
        )
    } else if max_cpu_cores > 0 {
        format!("System CPU: --/{max_cpu_cores} cores (--%)")
    } else {
        "System CPU: --/-- cores (--%)".to_owned()
    }
}

fn render_memory_usage(system_resource_usage: SystemResourceUsage) -> String {
    if let SystemResourceUsage {
        memory_used_bytes: Some(memory_used_bytes),
        memory_total_bytes: Some(memory_total_bytes),
        ..
    } = system_resource_usage
        && memory_total_bytes > 0
    {
        let memory_percent = memory_used_bytes as f64 * 100.0 / memory_total_bytes as f64;
        format!(
            "Mem: {}/{}GiB ({:.0}%)",
            format_gib(memory_used_bytes),
            format_gib(memory_total_bytes),
            memory_percent.max(0.0),
        )
    } else {
        "Mem: --/--GiB (--%)".to_owned()
    }
}

fn render_disk_io_usage(system_resource_usage: SystemResourceUsage) -> String {
    if let Some(disk_io) = system_resource_usage.disk_io {
        format!(
            "Disk: R: {} ({} IOPS)  W: {} ({} IOPS)",
            HumanizedBytesPerSecond::new(disk_io.read_bytes_per_second),
            format_iops(disk_io.read_operations_per_second),
            HumanizedBytesPerSecond::new(disk_io.write_bytes_per_second),
            format_iops(disk_io.write_operations_per_second),
        )
    } else {
        "Disk: R: --/s (-- IOPS)  W: --/s (-- IOPS)".to_owned()
    }
}

fn format_iops(operations_per_second: Option<u64>) -> String {
    operations_per_second
        .map(|operations_per_second| CommaSeparatedCount::new(operations_per_second).to_string())
        .unwrap_or_else(|| "--".to_owned())
}

fn host_cpu_usage_cores(two_snapshots: &TwoSnapshots) -> Option<f64> {
    let (previous_time, previous_snapshot) = two_snapshots.penultimate.as_ref()?;
    let (current_time, current_snapshot) = two_snapshots.last.as_ref()?;
    let elapsed = current_time.duration_since(*previous_time).ok()?;
    if elapsed.is_zero() {
        return None;
    }

    let previous_cpu_ms = previous_snapshot
        .host_cpu_usage_user_ms?
        .checked_add(previous_snapshot.host_cpu_usage_system_ms?)?;
    let current_cpu_ms = current_snapshot
        .host_cpu_usage_user_ms?
        .checked_add(current_snapshot.host_cpu_usage_system_ms?)?;
    let cpu_delta_ms = current_cpu_ms.checked_sub(previous_cpu_ms)?;
    Some(cpu_delta_ms as f64 / elapsed.as_secs_f64() / 1000.0)
}

fn format_gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / BYTES_PER_GIB)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::SystemTime;

    use super::*;

    #[test]
    fn test_host_cpu_usage_cores() {
        let t0 = SystemTime::UNIX_EPOCH;
        let mut two_snapshots = TwoSnapshots::default();
        two_snapshots.update(
            t0,
            &bz_data::Snapshot {
                host_cpu_usage_user_ms: Some(1000),
                host_cpu_usage_system_ms: Some(2000),
                ..Default::default()
            },
        );
        two_snapshots.update(
            t0 + Duration::from_secs(2),
            &bz_data::Snapshot {
                host_cpu_usage_user_ms: Some(4000),
                host_cpu_usage_system_ms: Some(6000),
                ..Default::default()
            },
        );

        let cores = host_cpu_usage_cores(&two_snapshots).unwrap();
        assert!(
            (cores - 3.5).abs() < f64::EPSILON,
            "expected 3.5 cores, got {cores}"
        );
    }

    #[test]
    fn test_render_memory_usage() {
        let gib = 1024_u64.pow(3);
        assert_eq!(
            render_memory_usage(SystemResourceUsage {
                memory_used_bytes: Some(16 * gib),
                memory_total_bytes: Some(64 * gib),
                disk_io: None,
            }),
            "Mem: 16.0/64.0GiB (25%)"
        );
        assert_eq!(
            render_memory_usage(SystemResourceUsage {
                memory_used_bytes: None,
                memory_total_bytes: None,
                disk_io: None,
            }),
            "Mem: --/--GiB (--%)"
        );
    }

    #[test]
    fn test_render_disk_io_usage() {
        assert_eq!(
            render_disk_io_usage(SystemResourceUsage {
                memory_used_bytes: None,
                memory_total_bytes: None,
                disk_io: Some(bz_util::system_stats::SystemDiskIoStats {
                    read_bytes_per_second: 1024,
                    write_bytes_per_second: 1024 * 1024,
                    read_operations_per_second: Some(2),
                    write_operations_per_second: Some(3000),
                }),
            }),
            "Disk: R: 1.0KiB/s (2 IOPS)  W: 1.0MiB/s (3,000 IOPS)"
        );
        assert_eq!(
            render_disk_io_usage(SystemResourceUsage {
                memory_used_bytes: None,
                memory_total_bytes: None,
                disk_io: None,
            }),
            "Disk: R: --/s (-- IOPS)  W: --/s (-- IOPS)"
        );
    }
}
