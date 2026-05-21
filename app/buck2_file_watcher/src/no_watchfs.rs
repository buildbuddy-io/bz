/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use allocative::Allocative;
use async_trait::async_trait;
use buck2_common::file_ops::dice::KnownFileStateInvalidationStats;
use buck2_common::file_ops::dice::invalidate_changed_file_state;
use buck2_events::dispatch::span_async;
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
    ) -> buck2_error::Result<(DiceTransactionUpdater, Mergebase)> {
        span_async(
            buck2_data::FileWatcherStart {
                provider: buck2_data::FileWatcherProvider::NoWatchFs as i32,
            },
            async {
                let (stats, res) = match invalidate_changed_file_state(&mut dice).await {
                    Ok(stats) => (
                        Some(no_watchfs_file_watcher_stats(stats)),
                        Ok((dice, Mergebase::default())),
                    ),
                    Err(e) => (None, Err(e)),
                };
                (res, buck2_data::FileWatcherEnd { stats })
            },
        )
        .await
    }
}

fn no_watchfs_file_watcher_stats(
    stats: KnownFileStateInvalidationStats,
) -> buck2_data::FileWatcherStats {
    let total = stats.total() as u64;
    buck2_data::FileWatcherStats {
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
