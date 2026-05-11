/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;

use allocative::Allocative;
use buck2_build_api::interpreter::rule_defs::context::bazel_analysis_context_declare_file;
use buck2_build_api::interpreter::rule_defs::depset::bazel_depset_is_singleton;
use buck2_build_api::interpreter::rule_defs::provider::builtin::default_info::BazelRunfiles;
use buck2_build_api::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_with_generated_inits_empty_files_supplier;
use buck2_core::cells::external::bzlmod_all_cell_aliases;
use buck2_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use buck2_core::package::PackageLabel;
use buck2_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use buck2_interpreter::types::target_label::StarlarkConfiguredTargetLabel;
use buck2_interpreter::types::target_label::StarlarkTargetLabel;
use fancy_regex::Regex;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::none::NoneOr;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelPythonError {
    #[error("Invalid py_internal regex `{pattern}`: {error}")]
    InvalidRegex { pattern: String, error: String },
    #[error("Error matching py_internal regex `{pattern}`: {error}")]
    RegexMatch { pattern: String, error: String },
    #[error("py_internal.get_label_repo_runfiles_path expected Label, got `{0}`")]
    ExpectedLabel(String),
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelPyInternal;

impl fmt::Display for BazelPyInternal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("py_internal")
    }
}

starlark::starlark_simple_value!(BazelPyInternal);

#[starlark_value(type = "py_internal")]
impl<'v> StarlarkValue<'v> for BazelPyInternal {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_py_internal_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "get_current_os_name".to_owned(),
            "get_label_repo_runfiles_path".to_owned(),
            "get_legacy_external_runfiles".to_owned(),
            "is_bzlmod_enabled".to_owned(),
            "is_singleton_depset".to_owned(),
            "is_tool_configuration".to_owned(),
            "declare_constant_metadata_file".to_owned(),
            "create_repo_mapping_manifest".to_owned(),
            "make_runfiles_respect_legacy_external_runfiles".to_owned(),
            "merge_runfiles_with_generated_inits_empty_files_supplier".to_owned(),
            "regex_match".to_owned(),
            "runfiles_enabled".to_owned(),
            "stamp_binaries".to_owned(),
        ]
    }
}

fn bazel_repo_name_for_cell(cell: &str) -> String {
    if cell == "root" {
        return String::new();
    }
    bzlmod_canonical_repo_name_for_cell(cell).unwrap_or_else(|| cell.to_owned())
}

fn label_repo_runfiles_path(package: PackageLabel) -> String {
    let cell = package.cell_name();
    let package_path = package.cell_relative_path().as_str();
    if cell.as_str() == "root" {
        return package_path.to_owned();
    }
    let repo = bazel_repo_name_for_cell(cell.as_str());
    if package_path.is_empty() {
        format!("../{repo}")
    } else {
        format!("../{repo}/{package_path}")
    }
}

fn label_value_repo_runfiles_path(label: Value<'_>) -> buck2_error::Result<String> {
    if let Some(label) = StarlarkConfiguredProvidersLabel::from_value(label) {
        return Ok(label_repo_runfiles_path(label.label().target().pkg()));
    }
    if let Some(label) = StarlarkProvidersLabel::from_value(label) {
        return Ok(label_repo_runfiles_path(label.label().target().pkg()));
    }
    if let Some(label) = StarlarkConfiguredTargetLabel::from_value(label) {
        return Ok(label_repo_runfiles_path(label.label().pkg()));
    }
    if let Some(label) = StarlarkTargetLabel::from_value(label) {
        return Ok(label_repo_runfiles_path(label.label().pkg()));
    }
    Err(BazelPythonError::ExpectedLabel(label.to_string_for_type_error()).into())
}

fn repo_mapping_source_repo_name_for_cell(cell: &str) -> String {
    if cell == "root" {
        String::new()
    } else {
        bazel_repo_name_for_cell(cell)
    }
}

fn repo_mapping_target_repo_directory_for_cell(cell: &str) -> String {
    if cell == "root" {
        "_main".to_owned()
    } else {
        bazel_repo_name_for_cell(cell)
    }
}

fn repo_mapping_manifest_content() -> String {
    let mut content = String::new();
    for (source_cell, aliases) in bzlmod_all_cell_aliases() {
        let source = repo_mapping_source_repo_name_for_cell(&source_cell);
        for (apparent_name, target_cell) in aliases {
            if apparent_name.is_empty() {
                continue;
            }
            let target = repo_mapping_target_repo_directory_for_cell(&target_cell);
            content.push_str(&source);
            content.push(',');
            content.push_str(&apparent_name);
            content.push(',');
            content.push_str(&target);
            content.push('\n');
        }
    }
    content
}

fn ctx_is_tool_configuration<'v>(ctx: Value<'v>, heap: Heap<'v>) -> starlark::Result<bool> {
    let label = ctx.get_attr_error("label", heap)?;
    if label.is_none() {
        return Ok(false);
    }
    let label = StarlarkConfiguredProvidersLabel::from_value(label).ok_or_else(|| {
        buck2_error::Error::from(BazelPythonError::ExpectedLabel(
            label.to_string_for_type_error(),
        ))
    })?;
    Ok(label.label().target().cfg().is_marked_as_exec_platform())
}

#[starlark_module]
fn bazel_py_internal_methods(builder: &mut MethodsBuilder) {
    fn is_singleton_depset<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        value: Value<'v>,
    ) -> starlark::Result<bool> {
        bazel_depset_is_singleton(value)
    }

    fn regex_match<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        subject: &str,
        pattern: &str,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let normalized_pattern = pattern
            .strip_prefix("(?U)")
            .or_else(|| pattern.strip_prefix("(?u)"))
            .unwrap_or(pattern);
        let anchored = format!("^(?:{normalized_pattern})$");
        let regex = Regex::new(&anchored).map_err(|error| {
            buck2_error::Error::from(BazelPythonError::InvalidRegex {
                pattern: pattern.to_owned(),
                error: error.to_string(),
            })
        })?;
        regex
            .is_match(subject)
            .map_err(|error| BazelPythonError::RegexMatch {
                pattern: pattern.to_owned(),
                error: error.to_string(),
            })
            .map_err(|error| buck2_error::Error::from(error).into())
    }

    fn get_current_os_name(
        #[starlark(this)] _this: &BazelPyInternal,
    ) -> starlark::Result<&'static str> {
        Ok(match std::env::consts::OS {
            "macos" => "osx",
            "freebsd" => "freebsd",
            "openbsd" => "openbsd",
            "linux" => "linux",
            "windows" => "windows",
            _ => "unknown",
        })
    }

    fn get_label_repo_runfiles_path<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        label: Value<'v>,
    ) -> starlark::Result<String> {
        label_value_repo_runfiles_path(label).map_err(Into::into)
    }

    fn is_tool_configuration<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        ctx: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<bool> {
        ctx_is_tool_configuration(ctx, heap)
    }

    fn merge_runfiles_with_generated_inits_empty_files_supplier<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] runfiles: &BazelRunfiles<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<BazelRunfiles<'v>> {
        let _ = ctx;
        bazel_runfiles_with_generated_inits_empty_files_supplier(heap, runfiles)
    }

    fn make_runfiles_respect_legacy_external_runfiles<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(require = pos)] _ctx: Value<'v>,
        #[starlark(require = pos)] runfiles: &BazelRunfiles<'v>,
    ) -> starlark::Result<BazelRunfiles<'v>> {
        Ok(runfiles.clone())
    }

    fn declare_constant_metadata_file<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] root: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = root;
        bazel_analysis_context_declare_file(ctx, name, heap)
    }

    fn create_repo_mapping_manifest<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] runfiles: &BazelRunfiles<'v>,
        #[starlark(require = named)] output: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let _ = runfiles;
        let heap = eval.heap();
        let actions = ctx.get_attr_error("actions", heap)?;
        let write = actions.get_attr_error("write", heap)?;
        let content = heap.alloc_str(&repo_mapping_manifest_content()).to_value();
        eval.eval_function(write, &[output, content], &[])?;
        Ok(NoneType)
    }

    fn get_legacy_external_runfiles<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn is_bzlmod_enabled<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn runfiles_enabled<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(!cfg!(windows))
    }

    fn stamp_binaries<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(false)
    }
}

pub(crate) fn register_bazel_python_globals(builder: &mut GlobalsBuilder) {
    builder.set("py_internal", BazelPyInternal);
}
