/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::sync::Arc;

use buck2_common::dice::cells::HasCellResolver;
use buck2_common::legacy_configs::dice::HasLegacyConfigs;
use buck2_common::legacy_configs::key::BuckconfigKeyRef;
use buck2_common::legacy_configs::view::LegacyBuckConfigView;
use buck2_core::configuration::data::BazelBuildSettingValue;
use buck2_core::configuration::data::ConfigurationData;
use buck2_core::provider::label::ProvidersLabel;
use buck2_error::BuckErrorContext;
use dice::DiceComputations;

#[derive(Debug, buck2_error::Error)]
#[buck2(input)]
enum BazelCommandLineOptionsError {
    #[error("Malformed Bazel command-line build setting entry `{0}`")]
    MalformedBuildSettingEntry(String),
    #[error("Unsupported Bazel command-line build setting entry kind `{0}`")]
    UnsupportedBuildSettingEntryKind(String),
}

fn bazel_config_lines(value: Option<Arc<str>>) -> Vec<String> {
    value
        .as_deref()
        .map(|value| {
            value
                .split('\n')
                .filter(|value| !value.is_empty())
                .map(|value| value.to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn push_bazel_build_setting(
    settings: &mut BTreeMap<String, BazelBuildSettingValue>,
    key: String,
    value: BazelBuildSettingValue,
) {
    match value {
        BazelBuildSettingValue::StringList(mut values) => {
            match settings.get_mut(&key) {
                Some(BazelBuildSettingValue::StringList(existing)) => existing.append(&mut values),
                _ => {
                    settings.insert(key, BazelBuildSettingValue::StringList(values));
                }
            };
        }
        value => {
            settings.insert(key, value);
        }
    }
}

async fn bazel_command_line_build_settings(
    ctx: &mut DiceComputations<'_>,
) -> buck2_error::Result<BTreeMap<String, BazelBuildSettingValue>> {
    let entries = {
        let root_config = ctx.get_legacy_root_config_on_dice().await?;
        let mut config = root_config.view(ctx);
        bazel_config_lines(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "command_line_build_settings",
        })?)
    };
    if entries.is_empty() {
        return Ok(BTreeMap::new());
    }

    let cell_resolver = ctx.get_cell_resolver().await?;
    let root_cell = cell_resolver.root_cell();
    let cell_alias_resolver = ctx.get_cell_alias_resolver(root_cell).await?;

    let mut settings = BTreeMap::new();
    for entry in entries {
        let mut parts = entry.splitn(3, '\t');
        let kind = parts.next().unwrap_or_default();
        let raw_key = parts.next().ok_or_else(|| {
            BazelCommandLineOptionsError::MalformedBuildSettingEntry(entry.clone())
        })?;
        let raw_value = parts.next().ok_or_else(|| {
            BazelCommandLineOptionsError::MalformedBuildSettingEntry(entry.clone())
        })?;
        let key = if raw_key.starts_with("//command_line_option:") {
            raw_key.to_owned()
        } else {
            ProvidersLabel::parse(raw_key, root_cell, &cell_resolver, &cell_alias_resolver)
                .with_buck_error_context(|| {
                    format!("Parsing Bazel command-line build setting `{raw_key}`")
                })?
                .to_string()
        };
        let value = match kind {
            "bool" => BazelBuildSettingValue::Bool(matches!(raw_value, "true" | "True" | "1")),
            "list" => BazelBuildSettingValue::StringList(vec![raw_value.to_owned()]),
            "string" => BazelBuildSettingValue::String(raw_value.to_owned()),
            _ => {
                return Err(
                    BazelCommandLineOptionsError::UnsupportedBuildSettingEntryKind(kind.to_owned())
                        .into(),
                );
            }
        };
        push_bazel_build_setting(&mut settings, key, value);
    }
    Ok(settings)
}

pub(crate) async fn apply_bazel_command_line_build_settings(
    ctx: &mut DiceComputations<'_>,
    cfg: ConfigurationData,
) -> buck2_error::Result<ConfigurationData> {
    if !cfg.is_bound() {
        return Ok(cfg);
    }

    let settings = bazel_command_line_build_settings(ctx).await?;
    if settings.is_empty() {
        return Ok(cfg);
    }

    let original_data = cfg.data()?.clone();
    let mut data = original_data.clone();
    for (key, value) in settings {
        data.build_settings.insert(key, value);
    }
    if data == original_data {
        return Ok(cfg);
    }

    ConfigurationData::from_platform(
        format!("{}-bazelrc", cfg.label()?),
        data,
        cfg.is_marked_as_exec_platform(),
    )
}
