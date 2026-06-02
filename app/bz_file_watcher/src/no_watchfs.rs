use allocative::Allocative;
use async_trait::async_trait;
use bz_common::file_ops::dice::KnownFileStateInvalidationStats;
use bz_common::file_ops::dice::invalidate_changed_file_state;
use bz_events::dispatch::span_async;
use dice::DiceTransactionUpdater;

use crate::file_watcher::FileWatcher;
use crate::mergebase::Mergebase;

#[derive(Allocative)]
pub(crate) struct NoWatchFs;

impl NoWatchFs {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FileWatcher for NoWatchFs {
    async fn sync(
        &self,
        mut dice: DiceTransactionUpdater,
    ) -> bz_error::Result<(DiceTransactionUpdater, Mergebase)> {
        span_async(
            bz_data::FileWatcherStart {
                provider: bz_data::FileWatcherProvider::NoWatchFs as i32,
            },
            async {
                let (stats, res) = match invalidate_changed_file_state(&mut dice).await {
                    Ok(stats) => (
                        Some(no_watchfs_file_watcher_stats(stats)),
                        Ok((dice, Mergebase::default())),
                    ),
                    Err(e) => (None, Err(e)),
                };
                (res, bz_data::FileWatcherEnd { stats })
            },
        )
        .await
    }
}

fn no_watchfs_file_watcher_stats(
    stats: KnownFileStateInvalidationStats,
) -> bz_data::FileWatcherStats {
    let total = stats.total() as u64;
    bz_data::FileWatcherStats {
        events_total: total,
        events_processed: total,
        events: stats.events,
        incomplete_events_reason: Some(format!(
            "no-watchfs invalidated keys: read_files={}, read_dirs={}, paths={}, exists_matching_exact_case={}; timings_us: introspection={}, file_ops={}, file_state={}, read_dirs={}, metadata={}, full_check={}",
            stats.read_files,
            stats.read_dirs,
            stats.paths,
            stats.exists_matching_exact_case,
            stats.timings.introspection_us,
            stats.timings.file_ops_us,
            stats.timings.file_state_us,
            stats.timings.read_dirs_us,
            stats.timings.metadata_us,
            stats.timings.full_check_us
        )),
        ..Default::default()
    }
}
