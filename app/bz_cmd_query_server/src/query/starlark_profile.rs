/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::path::Path;

use bz_core::package::PackageLabel;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_events::dispatch::console_message;
use bz_fs::paths::abs_path::AbsPath;
use bz_interpreter::starlark_profiler::data::StarlarkProfileDataAndStats;
use bz_node::nodes::frontend::TargetGraphCalculation;
use bz_profile::write_starlark_profile;
use dice::DiceComputations;
use futures::FutureExt;

pub(crate) async fn write_query_profile_for_targets(
    ctx: &mut DiceComputations<'_>,
    _profile_mode: bz_cli_proto::ProfileMode,
    output_path: Option<&str>,
    targets: impl IntoIterator<Item = PackageLabel>,
) -> bz_error::Result<()> {
    let output_path =
        output_path.ok_or_else(|| internal_error!("Outut path must be set for profile mode"))?;
    let output_path = AbsPath::new(Path::new(output_path))
        .buck_error_context("Output path must be set to absolute path by the client")?;
    do_write_query_profile_for_targets(ctx, output_path, Vec::from_iter(targets))
        .boxed()
        .await
}

async fn do_write_query_profile_for_targets(
    ctx: &mut DiceComputations<'_>,
    output_path: &AbsPath,
    mut targets: Vec<PackageLabel>,
) -> bz_error::Result<()> {
    // We want stable output.
    targets.sort();
    targets.dedup();

    let mut profiles = Vec::new();
    let mut target_names = Vec::new();
    for target in targets {
        // This should be already cached.
        target_names.push(target.to_string());
        let eval_results = ctx.get_interpreter_results(target).await?;
        let profile = eval_results
            .starlark_profile
            .as_ref()
            .ok_or_else(|| internal_error!("Starlark profile must be set"))?;
        let profile = StarlarkProfileDataAndStats::downcast(&**profile)?;
        profiles.push(profile.clone());
    }
    let profile = StarlarkProfileDataAndStats::merge(&profiles)?;

    write_starlark_profile(&profile, &target_names, output_path)?;

    console_message(format!(
        "Starlark profile data is written to {}",
        output_path.display()
    ));

    Ok(())
}
