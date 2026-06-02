/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use async_trait::async_trait;
use bz_core::target::configured_target_label::ConfiguredTargetLabel;
use bz_util::late_binding::LateBinding;
use dice::DiceComputations;

#[async_trait]
pub trait ValidationImpl: Send + Sync + 'static {
    /// Validate a given configured target node and any transitive nodes.
    async fn validate_target_node_transitively(
        &self,
        ctx: &mut DiceComputations<'_>,
        target: ConfiguredTargetLabel,
    ) -> Result<(), bz_error::Error>;
}

pub static VALIDATION_IMPL: LateBinding<&'static dyn ValidationImpl> =
    LateBinding::new("VALIDATION_IMPL");
