use std::collections::BTreeMap;
use std::sync::Arc;

use bz_common::dice::cells::HasCellResolver;
use bz_common::legacy_configs::dice::HasLegacyConfigs;
use bz_common::legacy_configs::key::BuckconfigKeyRef;
use bz_common::legacy_configs::view::LegacyBuckConfigView;
use bz_core::configuration::data::BazelBuildSettingValue;
use bz_core::configuration::data::ConfigurationData;
use bz_core::provider::label::ProvidersLabel;
use bz_error::BuckErrorContext;
use dice::DiceComputations;

#[derive(Debug, bz_error::Error)]
#[buck2(input)]
enum BazelCommandLineOptionsError {
    #[error("Malformed Bazel command-line build setting entry `{0}`")]
    MalformedBuildSettingEntry(String),
    #[error("Unsupported Bazel command-line build setting entry kind `{0}`")]
    UnsupportedBuildSettingEntryKind(String),
}

const BAZEL_CPU_OPTION: &str = "//command_line_option:cpu";
const BAZEL_HOST_CPU_OPTION: &str = "//command_line_option:host_cpu";
const BAZEL_LINKOPT_OPTION: &str = "//command_line_option:linkopt";
const BAZEL_HOST_LINKOPT_OPTION: &str = "//command_line_option:host_linkopt";
const BAZEL_PLATFORMS_OPTION: &str = "//command_line_option:platforms";
const BAZEL_JAVA_LANGUAGE_VERSION: &str = "//command_line_option:java_language_version";
const BAZEL_JAVA_RUNTIME_VERSION: &str = "//command_line_option:java_runtime_version";
const BAZEL_TOOL_JAVA_LANGUAGE_VERSION: &str = "//command_line_option:tool_java_language_version";
const BAZEL_TOOL_JAVA_RUNTIME_VERSION: &str = "//command_line_option:tool_java_runtime_version";

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
) -> bz_error::Result<BTreeMap<String, BazelBuildSettingValue>> {
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

fn exec_bazel_command_line_build_settings(
    mut settings: BTreeMap<String, BazelBuildSettingValue>,
    execution_platform_cfg: &ConfigurationData,
) -> bz_error::Result<BTreeMap<String, BazelBuildSettingValue>> {
    if let Some(host_cpu) = settings.get(BAZEL_HOST_CPU_OPTION).cloned() {
        settings.insert(BAZEL_CPU_OPTION.to_owned(), host_cpu);
    } else {
        settings.remove(BAZEL_CPU_OPTION);
    }

    if let Some(host_linkopt) = settings.get(BAZEL_HOST_LINKOPT_OPTION).cloned() {
        settings.insert(BAZEL_LINKOPT_OPTION.to_owned(), host_linkopt);
    } else {
        settings.remove(BAZEL_LINKOPT_OPTION);
    }

    if let Some(tool_java_language_version) =
        settings.get(BAZEL_TOOL_JAVA_LANGUAGE_VERSION).cloned()
    {
        settings.insert(
            BAZEL_JAVA_LANGUAGE_VERSION.to_owned(),
            tool_java_language_version,
        );
    }
    if let Some(tool_java_runtime_version) = settings.get(BAZEL_TOOL_JAVA_RUNTIME_VERSION).cloned()
    {
        settings.insert(
            BAZEL_JAVA_RUNTIME_VERSION.to_owned(),
            tool_java_runtime_version,
        );
    }

    settings.insert(
        BAZEL_PLATFORMS_OPTION.to_owned(),
        BazelBuildSettingValue::StringList(vec![execution_platform_cfg.label()?.to_owned()]),
    );

    Ok(settings)
}

async fn apply_bazel_command_line_build_settings_impl(
    cfg: ConfigurationData,
    settings: BTreeMap<String, BazelBuildSettingValue>,
) -> bz_error::Result<ConfigurationData> {
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
        cfg.label()?.to_owned(),
        data,
        cfg.is_marked_as_exec_platform(),
    )
}

pub(crate) async fn apply_bazel_command_line_build_settings(
    ctx: &mut DiceComputations<'_>,
    cfg: ConfigurationData,
) -> bz_error::Result<ConfigurationData> {
    if !cfg.is_bound() {
        return Ok(cfg);
    }

    let settings = bazel_command_line_build_settings(ctx).await?;
    apply_bazel_command_line_build_settings_impl(cfg, settings).await
}

pub(crate) async fn apply_bazel_exec_command_line_build_settings(
    ctx: &mut DiceComputations<'_>,
    cfg: ConfigurationData,
    execution_platform_cfg: &ConfigurationData,
) -> bz_error::Result<ConfigurationData> {
    if !cfg.is_bound() {
        return Ok(cfg);
    }

    let settings = bazel_command_line_build_settings(ctx).await?;
    let settings = exec_bazel_command_line_build_settings(settings, execution_platform_cfg)?;
    apply_bazel_command_line_build_settings_impl(cfg, settings).await
}
