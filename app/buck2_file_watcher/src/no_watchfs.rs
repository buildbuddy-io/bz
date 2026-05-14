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
use buck2_common::file_ops::dice::invalidate_all_known_file_state;
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
                let (stats, res) = match invalidate_all_known_file_state(&mut dice) {
                    Ok(_stats) => (Some(Default::default()), Ok((dice, Mergebase::default()))),
                    Err(e) => (None, Err(e)),
                };
                (res, buck2_data::FileWatcherEnd { stats })
            },
        )
        .await
    }
}
