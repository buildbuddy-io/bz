/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::target::label::label::TargetLabel;

fn bazel_repo_prefix_for_cell(cell_name: &str) -> Option<String> {
    if cell_name == "root" {
        Some("@@".to_owned())
    } else if cell_name == "bazel_tools" {
        Some("@@bazel_tools".to_owned())
    } else {
        bzlmod_canonical_repo_name_for_cell(cell_name).map(|repo| format!("@@{repo}"))
    }
}

pub(crate) fn bazel_label_string_for_target(label: &TargetLabel) -> Option<String> {
    let package = label.pkg().cell_relative_path().as_str();
    if label.pkg().cell_name().as_str() == "root" && package == "command_line_option" {
        return Some(format!("//command_line_option:{}", label.name()));
    }

    let repo = bazel_repo_prefix_for_cell(label.pkg().cell_name().as_str())?;
    Some(format!("{repo}//{package}:{}", label.name()))
}

pub(crate) fn starlark_providers_label_str(label: &ProvidersLabel) -> String {
    let target =
        bazel_label_string_for_target(label.target()).unwrap_or_else(|| label.target().to_string());
    format!("{target}{}", label.name())
}

pub(crate) fn starlark_configured_providers_label_str(label: &ConfiguredProvidersLabel) -> String {
    starlark_providers_label_str(&label.unconfigured())
}
