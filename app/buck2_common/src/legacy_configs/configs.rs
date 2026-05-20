/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Display;
use std::io::BufRead;
use std::sync::Arc;

use allocative::Allocative;
use buck2_cli_proto::ConfigOverride;
use buck2_core::cells::cell_root_path::CellRootPath;
use buck2_core::cells::external::BZLMOD_EXTERNAL_CELL_KIND;
use buck2_core::cells::external::BZLMOD_GENERATED_EXTERNAL_CELL_KIND;
use buck2_core::cells::external::external_cell_source_path;
use buck2_core::cells::external::register_bzlmod_cell_canonical_repo_name_for_cell;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_hash::StdBuckHashMap;
use dupe::Dupe;
use pagable::Pagable;
use starlark_map::sorted_map::SortedMap;

use super::cells::ExternalPathBuckconfigData;
use crate::legacy_configs::args::ResolvedConfigFile;
use crate::legacy_configs::args::ResolvedLegacyConfigArg;
use crate::legacy_configs::file_ops::ConfigParserFileOps;
use crate::legacy_configs::file_ops::ConfigPath;
use crate::legacy_configs::key::BuckconfigKeyRef;
use crate::legacy_configs::parser::LegacyConfigParser;

#[derive(Clone, Dupe, Debug, Allocative, Pagable)]
pub struct LegacyBuckConfig(pub(crate) Arc<ConfigData>);

#[derive(Debug, Allocative, Pagable)]
pub(crate) struct ConfigData {
    pub(crate) values: SortedMap<String, LegacyBuckConfigSection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub(crate) enum ResolvedValue {
    // A placeholder used before we do resolution.
    Unknown,
    // Indicates that there's no resolution required, the resolved value and raw value are the same.
    Literal,
    // The resolved value for non-literals.
    Resolved(String),
}

#[derive(Debug, PartialEq, Eq, Allocative, Pagable)]
pub(crate) struct ConfigFileLocation {
    pub(crate) path: String,
    pub(crate) include_source: Option<Location>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub(crate) struct ConfigFileLocationWithLine {
    pub(crate) source_file: Arc<ConfigFileLocation>,
    pub(crate) line: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub(crate) enum Location {
    File(ConfigFileLocationWithLine),
    CommandLineArgument,
}

impl Location {
    pub(crate) fn as_legacy_buck_config_location(&self) -> LegacyBuckConfigLocation<'_> {
        match self {
            Self::File(x) => LegacyBuckConfigLocation::File(&x.source_file.path, x.line),
            Self::CommandLineArgument => LegacyBuckConfigLocation::CommandLineArgument,
        }
    }
}

// Represents a config section and key only, for example, `cxx.compiler`.
#[derive(Clone, Debug)]
pub struct ConfigSectionAndKey {
    //  TODO(scottcao): Add cell_path
    pub section: String,
    pub key: String,
}

#[derive(buck2_error::Error, Debug)]
#[buck2(input)]
pub(crate) enum ConfigArgumentParseError {
    #[error("Could not find section separator (`.`) in pair `{0}`")]
    NoSectionDotSeparator(String),
    #[error("Could not find equals sign (`=`) in pair `{0}`")]
    NoEqualsSeparator(String),

    #[error("Expected key-value in format of `section.key=value` but only got `{0}`")]
    MissingData(String),

    #[error("Contains whitespace in key-value pair `{0}`")]
    WhitespaceInKeyOrValue(String),

    #[error("Specifying cells via cli config overrides is banned (`{0}.key=value`)")]
    CellOverrideViaCliConfig(&'static str),
}

// Parses config key in the format `section.key`
pub fn parse_config_section_and_key(
    raw_section_and_key: &str,
    raw_arg_in_err: Option<&str>, // Used in error strings to preserve the original config argument, not just section and key
) -> buck2_error::Result<ConfigSectionAndKey> {
    let raw_arg = raw_arg_in_err.unwrap_or(raw_section_and_key);
    let (raw_section, raw_key) = raw_section_and_key
        .split_once('.')
        .ok_or_else(|| ConfigArgumentParseError::NoSectionDotSeparator(raw_arg.to_owned()))?;

    // We only trim the section + key, whitespace in values needs to be preserved. For example,
    // Buck can be invoked with --config section.key="Some Value" that contains important whitespace.
    let trimmed_section = raw_section.trim_start();
    if trimmed_section.find(char::is_whitespace).is_some()
        || raw_key.find(char::is_whitespace).is_some()
    {
        return Err(ConfigArgumentParseError::WhitespaceInKeyOrValue(raw_arg.to_owned()).into());
    }

    if trimmed_section.is_empty() || raw_key.is_empty() {
        return Err(ConfigArgumentParseError::MissingData(raw_arg.to_owned()).into());
    }

    Ok(ConfigSectionAndKey {
        section: trimmed_section.to_owned(),
        key: raw_key.to_owned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative, Pagable)]
pub(crate) struct ConfigValue {
    raw_value: String,
    pub(crate) resolved_value: ResolvedValue,
    pub(crate) source: Location,
}

#[derive(Debug, Default, Clone, Allocative, Pagable)]
pub struct LegacyBuckConfigSection {
    pub(crate) values: SortedMap<String, ConfigValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub(crate) enum BazelCompatExternalModule {
    Registry(BazelCompatRegistryModule),
    Generated(BazelCompatGeneratedModule),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Allocative, Pagable)]
pub(crate) struct BazelCompatCellAlias {
    pub alias: String,
    pub cell_name: String,
}

impl BazelCompatCellAlias {
    pub(crate) fn actual_root_cell_name<'a>(&'a self, root_cell_name: &'a str) -> &'a str {
        if self.cell_name == "root" {
            root_cell_name
        } else {
            self.cell_name.as_str()
        }
    }

    pub(crate) fn with_actual_root_cell(&self, root_cell_name: &str) -> Self {
        Self {
            alias: self.alias.clone(),
            cell_name: self.actual_root_cell_name(root_cell_name).to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub(crate) struct BazelCompatRegistryModule {
    pub cell_name: String,
    pub aliases: Vec<String>,
    pub module_name: String,
    pub version: String,
    pub canonical_repo_name: String,
    pub local_path: Option<String>,
    pub url: String,
    pub urls_json: String,
    pub integrity: String,
    pub strip_prefix: Option<String>,
    pub archive_type: Option<String>,
    pub patches_json: String,
    pub overlays_json: String,
    pub patch_strip: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub(crate) struct BazelCompatGeneratedModule {
    pub cell_name: String,
    pub aliases: Vec<String>,
    pub canonical_repo_name: String,
    pub generator_json: String,
}

impl BazelCompatExternalModule {
    pub(crate) fn cell_name(&self) -> &str {
        match self {
            Self::Registry(module) => &module.cell_name,
            Self::Generated(module) => &module.cell_name,
        }
    }

    pub(crate) fn canonical_repo_name(&self) -> &str {
        match self {
            Self::Registry(module) => &module.canonical_repo_name,
            Self::Generated(module) => &module.canonical_repo_name,
        }
    }

    fn external_cell_kind(&self) -> &'static str {
        match self {
            Self::Registry(_) => BZLMOD_EXTERNAL_CELL_KIND,
            Self::Generated(_) => BZLMOD_GENERATED_EXTERNAL_CELL_KIND,
        }
    }
}

impl ConfigValue {
    pub(crate) fn new_raw(source: ConfigFileLocationWithLine, value: String) -> Self {
        Self {
            raw_value: value,
            resolved_value: ResolvedValue::Unknown,
            source: Location::File(source),
        }
    }

    pub(crate) fn new_raw_arg(raw_value: String) -> Self {
        Self {
            raw_value,
            resolved_value: ResolvedValue::Unknown,
            source: Location::CommandLineArgument,
        }
    }

    pub(crate) fn raw_value(&self) -> &str {
        &self.raw_value
    }

    pub(crate) fn as_str(&self) -> &str {
        match &self.resolved_value {
            ResolvedValue::Literal => &self.raw_value,
            ResolvedValue::Resolved(v) => v,
            ResolvedValue::Unknown => {
                unreachable!("cannot call as_str() until all values are resolved")
            }
        }
    }
}

pub struct LegacyBuckConfigValue<'a> {
    pub(crate) value: &'a ConfigValue,
}

#[derive(PartialEq, Debug)]
pub enum LegacyBuckConfigLocation<'a> {
    File(&'a str, usize),
    CommandLineArgument,
}

impl Display for LegacyBuckConfigLocation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(file, line) => {
                write!(f, "at {file}:{line}")
            }
            Self::CommandLineArgument => {
                write!(f, "on the command line")
            }
        }
    }
}

impl<'a> LegacyBuckConfigValue<'a> {
    pub fn as_str(&self) -> &'a str {
        self.value.as_str()
    }

    pub fn raw_value(&self) -> &str {
        self.value.raw_value()
    }

    pub fn location(&self) -> LegacyBuckConfigLocation<'_> {
        match &self.value.source {
            Location::File(file) => {
                LegacyBuckConfigLocation::File(&file.source_file.path, file.line)
            }
            Location::CommandLineArgument => LegacyBuckConfigLocation::CommandLineArgument,
        }
    }

    pub fn location_stack(&self) -> Vec<LegacyBuckConfigLocation<'_>> {
        let mut res = Vec::new();
        let mut location = Some(&self.value.source);

        while let Some(loc) = location.take() {
            match &loc {
                Location::File(loc) => {
                    res.push(LegacyBuckConfigLocation::File(
                        &loc.source_file.path,
                        loc.line,
                    ));
                    location = loc.source_file.include_source.as_ref();
                }
                Location::CommandLineArgument => {
                    // No stack
                }
            }
        }
        res
    }
}

#[derive(Default)]
pub(crate) struct BazelCompatBazelrcOptions {
    pub(crate) copt: Vec<String>,
    pub(crate) conlyopt: Vec<String>,
    pub(crate) cxxopt: Vec<String>,
    pub(crate) host_copt: Vec<String>,
    pub(crate) host_conlyopt: Vec<String>,
    pub(crate) host_cxxopt: Vec<String>,
    pub(crate) per_file_copt: Vec<String>,
    pub(crate) macos_minimum_os: Vec<String>,
    pub(crate) host_macos_minimum_os: Vec<String>,
    pub(crate) command_line_build_settings: Vec<String>,
}

impl LegacyBuckConfig {
    pub fn empty() -> Self {
        Self(Arc::new(ConfigData {
            values: SortedMap::new(),
        }))
    }

    pub(crate) fn with_bazel_compat_defaults(
        &self,
        current_cell_aliases: &[BazelCompatCellAlias],
        external_modules: &[BazelCompatExternalModule],
        registered_toolchains: &[String],
        bazelrc_options: &BazelCompatBazelrcOptions,
    ) -> Self {
        for module in external_modules {
            register_bzlmod_cell_canonical_repo_name_for_cell(
                module.cell_name(),
                module.canonical_repo_name(),
            );
        }

        self.with_bazel_compat_defaults_inner(
            current_cell_aliases,
            external_modules,
            registered_toolchains,
            bazelrc_options,
        )
    }

    pub(crate) fn with_bazel_compat_cell_defaults(
        &self,
        current_cell_aliases: &[BazelCompatCellAlias],
        registered_toolchains: &[String],
        bazelrc_options: &BazelCompatBazelrcOptions,
    ) -> Self {
        self.with_bazel_compat_defaults_inner(
            current_cell_aliases,
            &[],
            registered_toolchains,
            bazelrc_options,
        )
    }

    pub(crate) fn with_bazel_compat_startup_defaults(&self) -> Self {
        self.with_bazel_compat_defaults_inner(&[], &[], &[], &BazelCompatBazelrcOptions::default())
    }

    fn with_bazel_compat_defaults_inner(
        &self,
        current_cell_aliases: &[BazelCompatCellAlias],
        external_modules: &[BazelCompatExternalModule],
        registered_toolchains: &[String],
        bazelrc_options: &BazelCompatBazelrcOptions,
    ) -> Self {
        const BAZEL_COMPAT_DEFAULTS: &[(&str, &[(&str, &str)])] = &[
            (
                "cells",
                &[
                    ("root", "."),
                    ("prelude", "prelude"),
                    ("bazel_tools", "bazel_tools"),
                ],
            ),
            (
                "cell_aliases",
                &[
                    ("config", "prelude"),
                    ("ovr_config", "prelude"),
                    ("fbcode", "prelude"),
                    ("fbcode_macros", "prelude"),
                    ("fbsource", "prelude"),
                    ("toolchains", "prelude"),
                ],
            ),
            (
                "external_cells",
                &[("prelude", "bundled"), ("bazel_tools", "bundled")],
            ),
            (
                "buildfile",
                &[
                    ("name_v2", "BUILD.bazel,BUILD"),
                    ("includes", "prelude//bazel/prelude.bzl"),
                ],
            ),
            (
                "parser",
                &[(
                    "target_platform_detector_spec",
                    "target:root//...->platforms//host:host",
                )],
            ),
            ("bazel", &[("compatibility", "true")]),
            (
                "buck2",
                &[
                    ("file_watcher", "fs_hash_crawler"),
                    ("share_action_paths", "true"),
                    ("sqlite_incremental_state", "false"),
                    ("sqlite_materializer_state", "true"),
                    ("starlark_max_callstack_size", "1000"),
                ],
            ),
        ];

        fn synthetic_config_value(raw_value: &str) -> ConfigValue {
            ConfigValue {
                raw_value: raw_value.to_owned(),
                resolved_value: ResolvedValue::Literal,
                source: Location::CommandLineArgument,
            }
        }

        let configured_root_cell = {
            let cells = self
                .get_section("cells")
                .or_else(|| self.get_section("repositories"));
            let root_alias = self
                .get(BuckconfigKeyRef {
                    section: "cell_aliases",
                    property: "root",
                })
                .or_else(|| {
                    self.get(BuckconfigKeyRef {
                        section: "repository_aliases",
                        property: "root",
                    })
                });
            root_alias
                .filter(|root_alias| {
                    cells
                        .map(|cells| cells.iter().any(|(name, _)| name == *root_alias))
                        .unwrap_or(false)
                })
                .map(str::to_owned)
                .or_else(|| {
                    cells.and_then(|cells| {
                        cells.iter().find_map(|(name, path)| {
                            (path.as_str() == ".").then(|| name.to_owned())
                        })
                    })
                })
        };

        let mut values: BTreeMap<String, LegacyBuckConfigSection> = self
            .0
            .values
            .iter()
            .map(|(section, section_data)| (section.clone(), section_data.clone()))
            .collect();
        for (section_name, section_defaults) in BAZEL_COMPAT_DEFAULTS {
            let is_cell_aliases = *section_name == "cell_aliases";
            let section = values.entry((*section_name).to_owned()).or_default();
            let mut section_values: BTreeMap<String, ConfigValue> = section
                .values
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            for (key, value) in *section_defaults {
                if *section_name == "cells" && *key == "root" && configured_root_cell.is_some() {
                    continue;
                }
                let value = if *section_name == "parser" && *key == "target_platform_detector_spec"
                {
                    format!(
                        "target:{}//...->platforms//host:host",
                        configured_root_cell.as_deref().unwrap_or("root")
                    )
                } else {
                    (*value).to_owned()
                };
                section_values
                    .entry((*key).to_owned())
                    .or_insert_with(|| synthetic_config_value(&value));
            }
            if *section_name == "cells" {
                for module in external_modules {
                    section_values
                        .entry(module.cell_name().to_owned())
                        .or_insert_with(|| {
                            synthetic_config_value(&external_cell_source_path(
                                module.external_cell_kind(),
                                module.canonical_repo_name(),
                            ))
                        });
                }
            }
            if is_cell_aliases {
                if let Some(root_cell) = configured_root_cell.as_deref()
                    && root_cell != "root"
                {
                    section_values
                        .entry("root".to_owned())
                        .or_insert_with(|| synthetic_config_value(root_cell));
                }
                for alias in current_cell_aliases {
                    section_values.insert(
                        alias.alias.clone(),
                        synthetic_config_value(&alias.cell_name),
                    );
                }
            }
            if *section_name == "external_cells" {
                for module in external_modules {
                    section_values
                        .entry(module.cell_name().to_owned())
                        .or_insert_with(|| synthetic_config_value(module.external_cell_kind()));
                }
            }
            section.values = SortedMap::from_iter(section_values);
        }

        if !registered_toolchains.is_empty() {
            let section = values.entry("bazel".to_owned()).or_default();
            let mut section_values: BTreeMap<String, ConfigValue> = section
                .values
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            section_values
                .entry("registered_toolchains".to_owned())
                .or_insert_with(|| synthetic_config_value(&registered_toolchains.join(",")));
            section.values = SortedMap::from_iter(section_values);
        }

        let bazelrc_option_values = [
            ("copt", &bazelrc_options.copt),
            ("conlyopt", &bazelrc_options.conlyopt),
            ("cxxopt", &bazelrc_options.cxxopt),
            ("host_copt", &bazelrc_options.host_copt),
            ("host_conlyopt", &bazelrc_options.host_conlyopt),
            ("host_cxxopt", &bazelrc_options.host_cxxopt),
            ("per_file_copt", &bazelrc_options.per_file_copt),
            ("macos_minimum_os", &bazelrc_options.macos_minimum_os),
            (
                "host_macos_minimum_os",
                &bazelrc_options.host_macos_minimum_os,
            ),
        ];
        if bazelrc_option_values
            .iter()
            .any(|(_, values)| !values.is_empty())
            || !bazelrc_options.command_line_build_settings.is_empty()
        {
            let section = values.entry("bazel".to_owned()).or_default();
            let mut section_values: BTreeMap<String, ConfigValue> = section
                .values
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            for (key, values) in bazelrc_option_values {
                if !values.is_empty() {
                    section_values
                        .entry(key.to_owned())
                        .or_insert_with(|| synthetic_config_value(&values.join("\n")));
                }
            }
            if !bazelrc_options.command_line_build_settings.is_empty() {
                section_values
                    .entry("command_line_build_settings".to_owned())
                    .or_insert_with(|| {
                        synthetic_config_value(
                            &bazelrc_options.command_line_build_settings.join("\n"),
                        )
                    });
            }
            section.values = SortedMap::from_iter(section_values);
        }

        for module in external_modules {
            let section = values
                .entry(format!("external_cell_{}", module.cell_name()))
                .or_default();
            let mut section_values: BTreeMap<String, ConfigValue> = section
                .values
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            match module {
                BazelCompatExternalModule::Registry(module) => {
                    for (key, value) in [
                        ("module_name", module.module_name.as_str()),
                        ("version", module.version.as_str()),
                        ("canonical_repo_name", module.canonical_repo_name.as_str()),
                        ("url", module.url.as_str()),
                        ("urls", module.urls_json.as_str()),
                        ("integrity", module.integrity.as_str()),
                        ("patches", module.patches_json.as_str()),
                        ("overlays", module.overlays_json.as_str()),
                    ] {
                        section_values
                            .entry(key.to_owned())
                            .or_insert_with(|| synthetic_config_value(value));
                    }
                    if let Some(strip_prefix) = &module.strip_prefix {
                        section_values
                            .entry("strip_prefix".to_owned())
                            .or_insert_with(|| synthetic_config_value(strip_prefix));
                    }
                    if let Some(local_path) = &module.local_path {
                        section_values
                            .entry("local_path".to_owned())
                            .or_insert_with(|| synthetic_config_value(local_path));
                    }
                    if let Some(archive_type) = &module.archive_type {
                        section_values
                            .entry("archive_type".to_owned())
                            .or_insert_with(|| synthetic_config_value(archive_type));
                    }
                    section_values
                        .entry("patch_strip".to_owned())
                        .or_insert_with(|| synthetic_config_value(&module.patch_strip.to_string()));
                }
                BazelCompatExternalModule::Generated(module) => {
                    for (key, value) in [
                        ("canonical_repo_name", module.canonical_repo_name.as_str()),
                        ("generator", module.generator_json.as_str()),
                    ] {
                        section_values
                            .entry(key.to_owned())
                            .or_insert_with(|| synthetic_config_value(value));
                    }
                }
            }
            section.values = SortedMap::from_iter(section_values);
        }

        Self(Arc::new(ConfigData {
            values: SortedMap::from_iter(values),
        }))
    }

    pub fn filter_values<F>(&self, filter: F) -> Self
    where
        F: Fn(&BuckconfigKeyRef) -> bool,
    {
        let values = self
            .0
            .values
            .iter()
            .filter_map(|(section, section_data)| {
                let values: SortedMap<_, _> = section_data
                    .values
                    .iter()
                    .filter(|(property, _)| filter(&BuckconfigKeyRef { section, property }))
                    .map(|(property, value)| (property.clone(), value.clone()))
                    .collect();
                if values.is_empty() {
                    None
                } else {
                    Some((section.clone(), LegacyBuckConfigSection { values }))
                }
            })
            .collect();
        Self(Arc::new(ConfigData { values }))
    }

    pub(crate) async fn start_parse_for_external_files(
        config_paths: &[ConfigPath],
        file_ops: &mut dyn ConfigParserFileOps,
        follow_includes: bool,
    ) -> buck2_error::Result<Vec<ExternalPathBuckconfigData>> {
        let mut external_path_configs = Vec::new();
        for main_config_file in config_paths {
            let mut parser = LegacyConfigParser::new();
            parser
                .parse_file(main_config_file, None, follow_includes, file_ops)
                .await?;
            external_path_configs.push(ExternalPathBuckconfigData {
                origin_path: main_config_file.clone(),
                parse_state: parser,
            });
        }
        Ok(external_path_configs)
    }

    pub(crate) async fn finish_parse(
        external_path_configs: Vec<ExternalPathBuckconfigData>,
        main_config_files: &[ConfigPath],
        current_cell: &CellRootPath,
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[ResolvedLegacyConfigArg],
        follow_includes: bool,
    ) -> buck2_error::Result<Self> {
        let mut parser = LegacyConfigParser::combine(external_path_configs);
        for main_config_file in main_config_files {
            parser
                .parse_file(main_config_file, None, follow_includes, file_ops)
                .await?;
        }

        for config_arg in config_args {
            match config_arg {
                ResolvedLegacyConfigArg::Flag(config_value) => {
                    parser.apply_config_arg(config_value, current_cell)?
                }
                ResolvedLegacyConfigArg::File(ResolvedConfigFile::Project(path)) => {
                    parser
                        .parse_file(
                            &ConfigPath::Project(path.to_owned()),
                            Some(Location::CommandLineArgument),
                            follow_includes,
                            file_ops,
                        )
                        .await?
                }
                ResolvedLegacyConfigArg::File(ResolvedConfigFile::Global(other)) => {
                    parser.join(&other.parser)
                }
            };
        }

        parser.finish()
    }
}

pub mod testing {
    use std::cmp::min;

    use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;

    use super::*;
    use crate::legacy_configs::args::resolve_config_args;
    use crate::legacy_configs::file_ops::ConfigDirEntry;

    pub fn parse(data: &[(&str, &str)], path: &str) -> buck2_error::Result<LegacyBuckConfig> {
        parse_with_config_args(data, path, &[])
    }

    pub fn parse_with_config_args(
        data: &[(&str, &str)],
        cell_path: &str,
        config_args: &[ConfigOverride],
    ) -> buck2_error::Result<LegacyBuckConfig> {
        let mut file_ops = TestConfigParserFileOps::new(data)?;
        let path = ProjectRelativePath::new(cell_path)?;
        futures::executor::block_on(async {
            // As long as people don't pass config files, making up values here is ok
            let processed_config_args = resolve_config_args(config_args, &mut file_ops).await?;
            LegacyBuckConfig::finish_parse(
                Vec::new(),
                &[ConfigPath::Project(path.to_owned())],
                CellRootPath::new(ProjectRelativePath::empty()),
                &mut file_ops,
                &processed_config_args,
                true,
            )
            .await
        })
    }

    pub struct TestConfigParserFileOps {
        data: StdBuckHashMap<ProjectRelativePathBuf, String>,
    }

    impl TestConfigParserFileOps {
        pub fn new(data: &[(&str, &str)]) -> buck2_error::Result<Self> {
            let mut holder_data = StdBuckHashMap::default();
            for (file, content) in data {
                holder_data.insert(
                    ProjectRelativePath::new(*file)?.to_owned(),
                    (*content).to_owned(),
                );
            }
            Ok(TestConfigParserFileOps { data: holder_data })
        }
    }

    #[async_trait::async_trait]
    #[allow(private_interfaces)]
    impl ConfigParserFileOps for TestConfigParserFileOps {
        async fn read_file_lines_if_exists(
            &mut self,
            path: &ConfigPath,
        ) -> buck2_error::Result<Option<Vec<String>>> {
            let ConfigPath::Project(path) = path else {
                return Ok(None);
            };
            let Some(content) = self.data.get(path) else {
                return Ok(None);
            };
            // Need a Read implementation that owns the bytes.
            struct StringReader(Vec<u8>, usize);
            impl std::io::Read for StringReader {
                fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
                    let remaining = self.0.len() - self.1;
                    let to_return = min(remaining, buf.len());
                    buf[..to_return].clone_from_slice(&self.0[self.1..self.1 + to_return]);
                    self.1 += to_return;
                    Ok(to_return)
                }
            }
            let file = std::io::BufReader::new(StringReader(content.to_owned().into_bytes(), 0));

            Ok(Some(
                file.lines()
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(buck2_error::Error::from)?,
            ))
        }

        async fn read_dir(
            &mut self,
            _path: &ConfigPath,
        ) -> buck2_error::Result<Vec<ConfigDirEntry>> {
            // This is only used for listing files in `buckconfig.d` directories, which we can just
            // say are always empty in tests
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use buck2_core::cells::cell_root_path::CellRootPathBuf;
    use indoc::indoc;
    use itertools::Itertools;

    use super::testing::*;
    use super::*;
    use crate::legacy_configs::key::BuckconfigKeyRef;

    pub(crate) fn assert_config_value(
        config: &LegacyBuckConfig,
        section: &str,
        key: &str,
        expected: &str,
    ) {
        match config.get_section(section) {
            None => {
                panic!(
                    "Expected config to have section `{}`, but had sections `<{}>`",
                    section,
                    config.sections().join(", ")
                );
            }
            Some(values) => match values.get(key) {
                None => panic!(
                    "Expected section `{}` to have key `{}`, but had keys `<{}>`",
                    section,
                    key,
                    values.keys().join(", ")
                ),
                Some(v) if v.as_str() != expected => {
                    panic!(
                        "Expected `{}.{}` to have value `{}`. Got `{}`.",
                        section,
                        key,
                        expected,
                        v.as_str()
                    );
                }
                _ => {}
            },
        }
    }

    fn assert_config_value_is_empty(config: &LegacyBuckConfig, section: &str, key: &str) {
        if let Some(values) = config.get_section(section)
            && let Some(v) = values.get(key)
        {
            panic!(
                "Expected `{}.{}` to not exist. Got `{}` for value.",
                section,
                key,
                v.as_str()
            );
        }
    }

    #[test]
    fn test_simple() -> buck2_error::Result<()> {
        let config = parse(
            &[(
                "config",
                indoc!(
                    r#"
            [section]
                int = 1
                string = hello
                multiline = hello \
                            world\
                            !

                # this is a comment
                commented = okay

            [new_section]
                overridden = 1

            [another_section]
                some_val = 2

            [new_section]
                reopened = ok
                # override overridden
                overridden = 3

                    # note trailing whitespace
                    [bad_formatting]

            value                 =             1
        "#
                ),
            )],
            "config",
        )?;

        assert_eq!(
            None,
            config.get(BuckconfigKeyRef {
                section: "section",
                property: "missing"
            })
        );
        assert_eq!(
            None,
            config.get(BuckconfigKeyRef {
                section: "missing",
                property: "int"
            })
        );
        assert_config_value(&config, "section", "int", "1");
        assert_config_value(&config, "section", "string", "hello");
        // Note that lines are all trimmed, so leading whitespace after a newline is
        // dropped.
        assert_config_value(&config, "section", "multiline", "hello world!");
        assert_config_value(&config, "section", "commented", "okay");
        assert_config_value(&config, "another_section", "some_val", "2");
        assert_config_value(&config, "new_section", "reopened", "ok");
        assert_config_value(&config, "new_section", "overridden", "3");
        assert_config_value(&config, "bad_formatting", "value", "1");
        Ok(())
    }

    #[test]
    fn test_comments() -> buck2_error::Result<()> {
        let config = parse(
            &[(
                "config",
                indoc!(
                    r#"
            [section1] # stuff
                key1 = value1
            [section2#name]
                key2 = value2
        "#
                ),
            )],
            "config",
        )?;
        assert_config_value(&config, "section1", "key1", "value1");
        assert_config_value(&config, "section2#name", "key2", "value2");
        Ok(())
    }

    #[test]
    fn test_references() -> buck2_error::Result<()> {
        let config = parse(
            &[(
                "config",
                indoc!(
                    r#"

            [section1]
                ref1_1 = ref1_1<$(config section3.ref3_2)>

            [section2]
                ref2_1 = ref2_1<$(config section3.ref3_1)>
                ref2_2 = ref2_2<$(config section2.ref2_1)>
            [section3]
                ref3_1 = ref3_1<$(config section1.ref1_1), $(config section3.ref3_2)>
                ref3_2 = ref3_2

            [simple]
                s1 = $(config simple.s2)$(config simple.s2)$(config simple.s2)
                s2 = $(config simple.s3)$(config simple.s3)$(config simple.s3)
                s3 = x
        "#
                ),
            )],
            "config",
        )?;

        assert_config_value(
            &config,
            "section2",
            "ref2_2",
            "ref2_2<ref2_1<ref3_1<ref1_1<ref3_2>, ref3_2>>>",
        );

        assert_config_value(&config, "simple", "s1", "xxxxxxxxx");
        Ok(())
    }

    #[test]
    fn test_reference_cycle() -> buck2_error::Result<()> {
        let res = parse(
            &[(
                "config",
                indoc!(
                    r#"

            [x]
                a = $(config x.b)
                b = $(config x.c)
                c = $(config x.d)
                d = $(config x.e)
                e = $(config x.f)
                f = $(config x.g)
                g = $(config x.d)
        "#
                ),
            )],
            "config",
        );

        match res {
            Ok(_) => panic!("Expected failure."),
            Err(e) => {
                let message = e.to_string();
                let cycle = "`x.d` -> `x.e` -> `x.f` -> `x.g` -> `x.d`";
                assert!(
                    message.contains(cycle),
                    "Expected error to contain \"{cycle}\", but was `{message}`"
                );
            }
        }

        Ok(())
    }

    #[test]
    fn test_includes() -> buck2_error::Result<()> {
        let config = parse(
            &[
                (
                    "base",
                    indoc!(
                        r#"
                            base = okay!
                        "#
                    ),
                ),
                (
                    "section",
                    indoc!(
                        r#"
                            [section]
                        "#
                    ),
                ),
                (
                    "some/deep/dir/includes_base",
                    indoc!(
                        r#"
                            <file:../../../base>
                        "#
                    ),
                ),
                (
                    "includes_section",
                    indoc!(
                        r#"
                            <file:section>
                        "#
                    ),
                ),
                (
                    "config",
                    indoc!(
                        r#"
                        # use a couple optional includes in here to ensure those work when the file exists.
                        [opened_section]
                            # include into an already open section
                            <?file:base>
                        # start a section with an include
                        <?file:includes_section>
                             key = wild
                             <?file:some/deep/dir/includes_base>
                        [other_section]
                        # ensure can reopen section with an include
                        <file:section>
                              other_key=wildtoo

                        # Check that an optional include for a file that doesn't exist is okay.
                        <?file:this_file_doesnt_exist>
                        "#
                    ),
                ),
                (
                    "test_bad_include",
                    indoc!(
                        r#"
                        <file:this_file_doesnt_exist>
                        "#
                    ),
                ),
            ],
            "config",
        )?;

        assert_config_value(&config, "opened_section", "base", "okay!");
        assert_config_value(&config, "section", "base", "okay!");
        // Note that lines are all trimmed, so leading whitespace after a newline is
        // dropped.
        assert_config_value(&config, "section", "key", "wild");
        assert_config_value(&config, "section", "other_key", "wildtoo");
        Ok(())
    }

    #[test]
    fn test_config_args_ordering() -> buck2_error::Result<()> {
        let config_args = vec![
            ConfigOverride::flag_no_cell("apple.key=value1"),
            ConfigOverride::flag_no_cell("apple.key=value2"),
        ];
        let config = parse_with_config_args(&[("config", indoc!(r#""#))], "config", &config_args)?;
        assert_config_value(&config, "apple", "key", "value2");

        Ok(())
    }

    #[test]
    fn test_config_args_empty() -> buck2_error::Result<()> {
        let config_args = vec![ConfigOverride::flag_no_cell("apple.key=")];
        let config = parse_with_config_args(&[("config", indoc!(r#""#))], "config", &config_args)?;
        assert_config_value_is_empty(&config, "apple", "key");

        Ok(())
    }

    #[test]
    fn test_config_args_overwrite_config_file() -> buck2_error::Result<()> {
        let config_args = vec![ConfigOverride::flag_no_cell("apple.key=value2")];
        let config = parse_with_config_args(
            &[(
                "config",
                indoc!(
                    r#"
            [apple]
                key = value1
        "#
                ),
            )],
            "config",
            &config_args,
        )?;

        assert_config_value(&config, "apple", "key", "value2");

        let apple_section = config.get_section("apple").unwrap();
        let key_value = apple_section.get("key").unwrap();
        assert_eq!(
            key_value.location(),
            LegacyBuckConfigLocation::CommandLineArgument
        );

        Ok(())
    }

    #[test]
    fn test_section_and_key() -> buck2_error::Result<()> {
        // Valid Formats

        let normal_section_and_key = parse_config_section_and_key("apple.key", None)?;

        assert_eq!("apple", normal_section_and_key.section);
        assert_eq!("key", normal_section_and_key.key);

        // Whitespace

        let section_leading_whitespace = parse_config_section_and_key("  apple.key", None)?;
        assert_eq!("apple", section_leading_whitespace.section);
        assert_eq!("key", section_leading_whitespace.key);

        let pair_with_whitespace_in_key = parse_config_section_and_key("apple. key", None);
        assert!(pair_with_whitespace_in_key.is_err());

        // Invalid Formats

        let pair_without_dot = parse_config_section_and_key("applekey", None);
        assert!(pair_without_dot.is_err());

        Ok(())
    }

    #[test]
    fn test_config_file_args_overwrite_config_file() -> buck2_error::Result<()> {
        let config_args = vec![
            ConfigOverride::flag_no_cell("apple.key=value3"),
            ConfigOverride::file("cli-config", Some(CellRootPathBuf::testing_new(""))),
        ];
        let config = parse_with_config_args(
            &[
                (
                    ".buckconfig",
                    indoc!(
                        r#"
                            [cells]
                              root = .

                            [apple]
                              key = value1
                        "#
                    ),
                ),
                (
                    "cli-config",
                    indoc!(
                        r#"
            [apple]
                key = value2
        "#
                    ),
                ),
            ],
            ".buckconfig",
            &config_args,
        )?;

        assert_config_value(&config, "apple", "key", "value2");

        let apple_section = config.get_section("apple").unwrap();
        let key_value = apple_section.get("key").unwrap();
        let expected_path = LegacyBuckConfigLocation::File("cli-config", 2);
        assert_eq!(key_value.location(), expected_path);

        Ok(())
    }

    #[test]
    fn test_config_args_cell_in_value() -> buck2_error::Result<()> {
        let config_args = vec![ConfigOverride::flag_no_cell("apple.key=foo//value1")];
        let config = parse_with_config_args(&[("config", indoc!(r#""#))], "config", &config_args)?;
        assert_config_value(&config, "apple", "key", "foo//value1");

        Ok(())
    }
}
