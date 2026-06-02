/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::future::Future;

pub(crate) async fn dice_state_update_stage<T, Fut>(
    stage: impl Into<String>,
    fut: Fut,
) -> buck2_error::Result<T>
where
    Fut: Future<Output = buck2_error::Result<T>>,
{
    buck2_events::dispatch::span_async(
        buck2_data::DiceStateUpdateStageStart {
            stage: stage.into(),
        },
        async { (fut.await, buck2_data::DiceStateUpdateStageEnd {}) },
    )
    .await
}
