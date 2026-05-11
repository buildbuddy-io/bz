/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::SystemTime;

use allocative::Allocative;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::file_ops::dice::DiceFileComputations;
use buck2_common::file_ops::error::FileReadErrorContext;
use buck2_common::file_ops::metadata::RawPathMetadata;
use buck2_core::bzl::ImportPath;
use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::alias::NonEmptyCellAlias;
use buck2_core::cells::build_file_cell::BuildFileCell;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::external::BzlmodModuleExtensionRepoSetup;
use buck2_core::cells::external::BzlmodRepositoryRuleInvocationSetup;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use buck2_core::cells::external::bzlmod_cell_name;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePathBuf;
use buck2_core::target::label::interner::ConcurrentTargetLabelInterner;
use buck2_interpreter::file_loader::LoadedModule;
use buck2_interpreter::load_module::InterpreterCalculation;
use buck2_interpreter::paths::module::StarlarkModulePath;
use buck2_interpreter::paths::path::OwnedStarlarkPath;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use buck2_node::attrs::attr::Attribute;
use buck2_node::attrs::attr::CoercedValue;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::configurable::AttrIsConfigurable;
use buck2_node::attrs::fmt_context::AttrFmtContext;
use buck2_node::bzl_or_bxl_path::BzlOrBxlPath;
use buck2_node::rule_type::StarlarkRuleType;
use derive_more::Display;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::Key;
use dice::NoValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use itertools::Itertools;
use pagable::Pagable;
use pagable::pagable_typetag;
use serde::Deserialize;
use serde::Serialize;
use sha1::Sha1;
use sha2::Digest;
use sha2::Sha256;
use sha2::Sha384;
use sha2::Sha512;
use starlark::any::ProvidesStaticType;
use starlark::docs::DocFunction;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::DocStringKind;
use starlark::environment::Globals;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::eval::ParametersSpec;
use starlark::eval::ParametersSpecParam;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::syntax::AstModule;
use starlark::syntax::Dialect;
use starlark::typing::ParamSpec;
use starlark::typing::Ty;
use starlark::values::AllocValue;
use starlark::values::Freeze;
use starlark::values::FreezeResult;
use starlark::values::Freezer;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;
use starlark::values::typing::StarlarkCallable;
use starlark_map::small_map::SmallMap;

use crate::attrs::AttributeCoerceExt;
use crate::attrs::coerce::ctx::BuildAttrCoercionContext;
use crate::attrs::starlark_attribute::StarlarkAttribute;
use crate::interpreter::build_context::BazelModuleExtensionEvaluationResult;
use crate::interpreter::build_context::BazelRepositoryRuleInvocation;
use crate::interpreter::build_context::BuildContext;
use crate::interpreter::build_context::PerFileTypeContext;
use crate::interpreter::dice_calculation_delegate::HasCalculationDelegate;
use crate::rule::NAME_ATTRIBUTE_FIELD;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
pub(crate) enum BazelRepositoryError {
    #[error("`{0}` is not a valid repository rule attribute name")]
    InvalidRepositoryRuleAttributeName(String),
    #[error("`repository_rule` requires an implementation function")]
    MissingRepositoryRuleImplementation,
    #[error("`{0}` can only be declared in bzl files")]
    NotInBzl(&'static str),
    #[error(
        "repository rules can only be called from within module extension implementation functions"
    )]
    RepositoryRuleCalledOutsideModuleExtension,
    #[error("repository rule calls require a `name` argument")]
    RepositoryRuleMissingName,
    #[error("repository rule `name` argument must be a string, got `{0}`")]
    RepositoryRuleNameMustBeString(String),
    #[error("attempting to instantiate a non-exported repository rule")]
    RepositoryRuleNotExported,
    #[error(
        "repository_rule `{0}` was defined in a BXL file; bzlmod repository execution only supports .bzl repository rules"
    )]
    RepositoryRuleBxlUnsupported(String),
    #[error("repository_rule `{rule}` was not found in `{path}`")]
    RepositoryRuleSymbolMissing { path: String, rule: String },
    #[error("`{path}` export `{rule}` must be a repository_rule, got `{got}`")]
    RepositoryRuleSymbolWrongType {
        path: String,
        rule: String,
        got: String,
    },
    #[error("repository_rule `{rule}` has no attribute `{attr}`")]
    RepositoryRuleUnknownAttribute { rule: String, attr: String },
    #[error("repository_ctx output path expected string or path, got `{0}`")]
    RepositoryCtxOutputPathUnsupportedValue(String),
    #[error("repository_ctx.template could not read `{path}`: {error}")]
    RepositoryCtxTemplateReadFile { path: String, error: String },
    #[error("repository_ctx could not write `{path}`: {error}")]
    RepositoryCtxWriteFile { path: String, error: String },
    #[error("repository_ctx could not delete `{path}`: {error}")]
    RepositoryCtxDeletePath { path: String, error: String },
    #[error("repository_ctx.patch could not apply `{patch}`: {error}")]
    RepositoryCtxPatch { patch: String, error: String },
    #[error("repository_ctx could not symlink `{link}` to `{target}`: {error}")]
    RepositoryCtxSymlink {
        target: String,
        link: String,
        error: String,
    },
    #[error("repository_ctx.download_and_extract could not extract `{archive}`: {error}")]
    RepositoryCtxExtractArchive { archive: String, error: String },
    #[error("Program argument of repository_ctx.which may not contain a / or a \\ (`{0}` given)")]
    RepositoryCtxWhichInvalidProgram(String),
    #[error("Program argument of repository_ctx.which may not be empty")]
    RepositoryCtxWhichEmptyProgram,
    #[error("repository_ctx.execute requires at least one argument")]
    RepositoryCtxExecuteEmptyArguments,
    #[error("repository_ctx.execute failed to run `{program}`: {error}")]
    RepositoryCtxExecuteFailed { program: String, error: String },
    #[error("repository_path.get_child expected string arguments, got `{0}`")]
    RepositoryPathGetChildNonString(String),
    #[error("repository_path.readdir could not read `{path}`: {error}")]
    RepositoryPathReaddir { path: String, error: String },
    #[error("repository_path.realpath could not canonicalize `{path}`: {error}")]
    RepositoryPathRealpath { path: String, error: String },
    #[error("attempting to instantiate a non-exported module extension")]
    ModuleExtensionNotExported,
    #[error("expected module extension `{0}` to return None or extension_metadata, got `{1}`")]
    InvalidModuleExtensionReturn(String, String),
    #[error("`tag_classes[{0}]` must be a tag_class object, got `{1}`")]
    InvalidTagClass(String, String),
    #[error("module extension `{extension}` was not found in `{path}`")]
    ModuleExtensionSymbolMissing { path: String, extension: String },
    #[error("`{path}` export `{extension}` must be a module_extension, got `{got}`")]
    ModuleExtensionSymbolWrongType {
        path: String,
        extension: String,
        got: String,
    },
    #[error("invalid bzlmod module extension usage data")]
    InvalidModuleExtensionUsageData,
    #[error("module extension `{extension}` has no tag class `{tag}`")]
    UnknownModuleExtensionTag { extension: String, tag: String },
    #[error("`tag_classes[{tag}]` must be a frozen tag_class object, got `{got}`")]
    InvalidFrozenTagClass { tag: String, got: String },
    #[error("module extension tag `{tag}` is missing required attribute `{attr}`")]
    MissingModuleExtensionTagAttribute { tag: String, attr: String },
    #[error("could not read evaluated bzlmod tag expression `{0}`")]
    MissingEvaluatedTagExpression(String),
    #[error("module_ctx.path expected string, Label, or path, got `{0}`")]
    ModuleCtxPathUnsupportedValue(String),
    #[error("error reading `{path}`: {error}")]
    ModuleCtxReadFile { path: String, error: String },
    #[error("module_ctx.download expected string or iterable of strings for `url`, got `{0}`")]
    ModuleCtxDownloadUrlUnsupportedValue(String),
    #[error("module_ctx.download requires at least one URL")]
    ModuleCtxDownloadNoUrls,
    #[error("module_ctx.download `{field}` is not implemented")]
    ModuleCtxDownloadUnsupportedField { field: &'static str },
    #[error("module_ctx.download failed for {urls:?}: {error}")]
    ModuleCtxDownloadFailed { urls: Vec<String>, error: String },
    #[error("module_ctx.download expected either `sha256` or `integrity`, but not both")]
    ModuleCtxDownloadConflictingChecksums,
    #[error("module_ctx.download unsupported integrity `{0}`")]
    ModuleCtxDownloadUnsupportedIntegrity(String),
    #[error("module_ctx.download checksum mismatch for `{path}`: expected {expected}, got {got}")]
    ModuleCtxDownloadChecksumMismatch {
        path: String,
        expected: String,
        got: String,
    },
    #[error("module_ctx.download could not write `{path}`: {error}")]
    ModuleCtxDownloadWriteFile { path: String, error: String },
}

fn current_bzl_path<'v>(
    eval: &Evaluator<'v, '_, '_>,
    symbol: &'static str,
) -> buck2_error::Result<BzlOrBxlPath> {
    let build_context = BuildContext::from_context(eval)?;
    match &build_context.additional {
        PerFileTypeContext::Bzl(bzl_path) => Ok(BzlOrBxlPath::Bzl(bzl_path.bzl_path.clone())),
        _ => Err(BazelRepositoryError::NotInBzl(symbol).into()),
    }
}

fn doc_string(doc: NoneOr<&str>) -> Option<String> {
    doc.into_option().map(|doc| doc.trim().to_owned())
}

fn record_repository_rule_invocation<'v>(
    rule_id: &StarlarkRuleType,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let build_context = BuildContext::from_context(eval)?;
    let recorder = build_context
        .bazel_repository_rule_recorder
        .ok_or_else(|| {
            buck2_error::Error::from(
                BazelRepositoryError::RepositoryRuleCalledOutsideModuleExtension,
            )
        })?;

    args.no_positional_args(eval.heap())?;

    let mut name = None;
    let mut attrs = Vec::new();
    let mut label_deps = BTreeSet::new();
    for (attr_name, attr_value) in args.names_map()? {
        let attr_name = attr_name.as_str();
        if attr_name == NAME_ATTRIBUTE_FIELD {
            let Some(name_value) = attr_value.unpack_str() else {
                return Err(buck2_error::Error::from(
                    BazelRepositoryError::RepositoryRuleNameMustBeString(
                        attr_value.get_type().to_owned(),
                    ),
                )
                .into());
            };
            name = Some(name_value.to_owned());
        } else {
            collect_repository_rule_label_deps(
                attr_value,
                &mut label_deps,
                build_context.cell_info().cell_alias_resolver(),
            )?;
            attrs.push((
                attr_name.to_owned(),
                repository_rule_attr_expression(attr_value)?,
            ));
        }
    }
    let name = name
        .ok_or_else(|| buck2_error::Error::from(BazelRepositoryError::RepositoryRuleMissingName))?;
    attrs.sort_by(|(left, _), (right, _)| left.cmp(right));

    recorder.record(BazelRepositoryRuleInvocation {
        rule_id: rule_id.clone(),
        original_name: name.clone(),
        name,
        attrs,
        label_deps: label_deps.into_iter().collect(),
    });

    Ok(Value::new_none())
}

fn repository_rule_attr_expression(value: Value<'_>) -> starlark::Result<String> {
    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
        return repository_rule_label_attr_expression(&label);
    }
    if let Some(string) = value.unpack_str() {
        return serde_json::to_string(string).map_err(|e| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "failed to serialize repository_rule string attr: {e}"
            )
            .into()
        });
    }
    if let Some(dict) = DictRef::from_value(value) {
        let mut entries = Vec::new();
        for (key, value) in dict.iter() {
            entries.push(format!(
                "{}: {}",
                repository_rule_attr_expression(key)?,
                repository_rule_attr_expression(value)?
            ));
        }
        return Ok(format!("{{{}}}", entries.join(", ")));
    }
    if let Some(list) = ListRef::from_value(value) {
        let values = list
            .iter()
            .map(repository_rule_attr_expression)
            .collect::<starlark::Result<Vec<_>>>()?;
        return Ok(format!("[{}]", values.join(", ")));
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        let values = tuple
            .iter()
            .map(repository_rule_attr_expression)
            .collect::<starlark::Result<Vec<_>>>()?;
        if values.len() == 1 {
            return Ok(format!("({},)", values[0]));
        }
        return Ok(format!("({})", values.join(", ")));
    }
    Ok(value.to_repr())
}

fn repository_rule_label_attr_expression(
    label: &StarlarkProvidersLabel,
) -> starlark::Result<String> {
    let target = label.label().target();
    let cell_name = target.pkg().cell_name();
    let cell_name = cell_name.as_str();
    let repo_name = if cell_name == "root" {
        String::new()
    } else if cell_name == "bazel_tools" {
        "bazel_tools".to_owned()
    } else {
        bzlmod_canonical_repo_name_for_cell(cell_name).unwrap_or_else(|| cell_name.to_owned())
    };
    let package = target.pkg().cell_relative_path().as_str();
    let name = target.name().as_str();
    let label = if repo_name.is_empty() {
        format!("//{package}:{name}")
    } else {
        format!("@@{repo_name}//{package}:{name}")
    };
    Ok(format!(
        "Label({})",
        serde_json::to_string(&label).map_err(|e| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "failed to serialize repository_rule label attr: {e}"
            )
        })?
    ))
}

fn collect_repository_rule_label_deps(
    value: Value<'_>,
    label_deps: &mut BTreeSet<String>,
    cell_alias_resolver: &CellAliasResolver,
) -> starlark::Result<()> {
    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
        label_deps.insert(label.label().target().pkg().cell_name().as_str().to_owned());
        return Ok(());
    }
    if value.unpack_str().is_some() {
        return Ok(());
    }
    if let Some(dict) = DictRef::from_value(value) {
        for (key, value) in dict.iter() {
            collect_repository_rule_label_deps(key, label_deps, cell_alias_resolver)?;
            collect_repository_rule_label_deps(value, label_deps, cell_alias_resolver)?;
        }
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        for value in list.iter() {
            collect_repository_rule_label_deps(value, label_deps, cell_alias_resolver)?;
        }
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        for value in tuple.iter() {
            collect_repository_rule_label_deps(value, label_deps, cell_alias_resolver)?;
        }
    }
    Ok(())
}

fn collect_repository_rule_string_label_dep(
    value: &str,
    label_deps: &mut BTreeSet<String>,
    cell_alias_resolver: &CellAliasResolver,
) {
    let Some((canonical, repo_name)) = repository_rule_string_repo_name(value) else {
        collect_repository_rule_string_label_template_deps(value, label_deps, cell_alias_resolver);
        return;
    };
    if canonical || repo_name.contains('+') {
        label_deps.insert(bzlmod_cell_name(repo_name));
        return;
    }
    if let Ok(cell_name) = cell_alias_resolver.resolve(repo_name) {
        label_deps.insert(cell_name.as_str().to_owned());
    }
}

fn collect_repository_rule_string_label_template_deps(
    value: &str,
    label_deps: &mut BTreeSet<String>,
    cell_alias_resolver: &CellAliasResolver,
) {
    let Some((canonical, repo_template)) = repository_rule_string_repo_name_template(value) else {
        return;
    };
    if canonical {
        return;
    }
    collect_repository_rule_repo_template_label_deps(
        repo_template,
        label_deps,
        cell_alias_resolver,
    );
}

fn collect_repository_rule_repo_template_label_deps(
    repo_template: &str,
    label_deps: &mut BTreeSet<String>,
    cell_alias_resolver: &CellAliasResolver,
) {
    for (alias, cell_name) in cell_alias_resolver.mappings() {
        if repository_rule_repo_template_matches(repo_template, alias.as_str()) {
            label_deps.insert(cell_name.as_str().to_owned());
        }
    }
}

fn collect_repository_rule_string_label_deps_from_expression(
    expression: &str,
    label_deps: &mut BTreeSet<String>,
    cell_alias_resolver: &CellAliasResolver,
) {
    for value in repository_rule_string_literals(expression) {
        collect_repository_rule_string_label_dep(&value, label_deps, cell_alias_resolver);
    }
}

fn repository_rule_string_literals(expression: &str) -> Vec<String> {
    let bytes = expression.as_bytes();
    let mut values = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let quote = bytes[index];
        if quote != b'"' && quote != b'\'' {
            index += 1;
            continue;
        }
        index += 1;
        let mut value = String::new();
        while index < bytes.len() {
            let byte = bytes[index];
            index += 1;
            if byte == quote {
                values.push(value);
                break;
            }
            if byte == b'\\' && index < bytes.len() {
                value.push(bytes[index] as char);
                index += 1;
            } else {
                value.push(byte as char);
            }
        }
    }
    values
}

fn repository_rule_string_repo_name(value: &str) -> Option<(bool, &str)> {
    if value.chars().any(char::is_whitespace) {
        return None;
    }
    let (canonical, rest) = if let Some(rest) = value.strip_prefix("@@") {
        (true, rest)
    } else if let Some(rest) = value.strip_prefix('@') {
        (false, rest)
    } else {
        return None;
    };
    let repo_name = rest
        .split_once("//")
        .map_or(rest, |(repo_name, _)| repo_name);
    if repo_name.is_empty() {
        return None;
    }
    if !repo_name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '+'))
    {
        return None;
    }
    Some((canonical, repo_name))
}

fn repository_rule_string_repo_name_template(value: &str) -> Option<(bool, &str)> {
    if value.chars().any(char::is_whitespace) {
        return None;
    }
    let (canonical, rest) = if let Some(rest) = value.strip_prefix("@@") {
        (true, rest)
    } else if let Some(rest) = value.strip_prefix('@') {
        (false, rest)
    } else {
        return None;
    };
    let repo_name = rest.split_once("//")?.0;
    if repo_name.is_empty() || !repo_name.contains('{') || !repo_name.contains('}') {
        return None;
    }
    if !repo_name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '+' | '{' | '}'))
    {
        return None;
    }
    Some((canonical, repo_name))
}

#[derive(Debug)]
struct RepositoryRuleRepoTemplate {
    segments: Vec<String>,
    starts_with_wildcard: bool,
    ends_with_wildcard: bool,
}

fn repository_rule_repo_template(value: &str) -> Option<RepositoryRuleRepoTemplate> {
    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut chars = value.chars().peekable();
    let mut starts_with_wildcard = false;
    let mut ends_with_wildcard = false;
    let mut saw_wildcard = false;
    let mut saw_literal = false;

    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                if matches!(chars.peek(), Some('{')) {
                    chars.next();
                    segment.push('{');
                    saw_literal = true;
                    ends_with_wildcard = false;
                    continue;
                }
                if segments.is_empty() && segment.is_empty() {
                    starts_with_wildcard = true;
                }
                segments.push(std::mem::take(&mut segment));
                saw_wildcard = true;
                ends_with_wildcard = true;
                let mut closed = false;
                for placeholder_ch in chars.by_ref() {
                    if placeholder_ch == '}' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return None;
                }
            }
            '}' => {
                if matches!(chars.peek(), Some('}')) {
                    chars.next();
                    segment.push('}');
                    saw_literal = true;
                    ends_with_wildcard = false;
                } else {
                    return None;
                }
            }
            _ => {
                segment.push(ch);
                saw_literal = true;
                ends_with_wildcard = false;
            }
        }
    }
    segments.push(segment);

    if !saw_wildcard || !saw_literal {
        return None;
    }

    Some(RepositoryRuleRepoTemplate {
        segments,
        starts_with_wildcard,
        ends_with_wildcard,
    })
}

fn repository_rule_repo_template_matches(template: &str, repo_name: &str) -> bool {
    let Some(template) = repository_rule_repo_template(template) else {
        return false;
    };
    let mut offset = 0usize;
    let mut first_literal = true;

    for segment in template
        .segments
        .iter()
        .filter(|segment| !segment.is_empty())
    {
        if first_literal && !template.starts_with_wildcard {
            let Some(rest) = repo_name[offset..].strip_prefix(segment) else {
                return false;
            };
            offset = repo_name.len() - rest.len();
        } else {
            let Some(found) = repo_name[offset..].find(segment) else {
                return false;
            };
            offset += found + segment.len();
        }
        first_literal = false;
    }

    template.ends_with_wildcard || offset == repo_name.len()
}

fn repository_rule_dynamic_label_attr_names(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    collect_repository_rule_attr_names_after(source, "Label(ctx.attr.", &mut names);
    collect_repository_rule_attr_names_after(source, "ctx.path(ctx.attr.", &mut names);
    names
}

fn repository_rule_dynamic_repo_name_attr_names(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut alias_locals = BTreeSet::new();
    collect_repository_rule_repo_name_label_attr_names(source, &mut names, &mut alias_locals);
    if !alias_locals.is_empty() {
        collect_repository_rule_ctx_attr_alias_attrs(source, &alias_locals, &mut names);
    }
    names
}

fn collect_repository_rule_repo_name_label_attr_names(
    source: &str,
    names: &mut BTreeSet<String>,
    alias_locals: &mut BTreeSet<String>,
) {
    let bytes = source.as_bytes();
    let mut offset = 0usize;
    while let Some(found) = source[offset..].find("Label") {
        let mut index = offset + found + "Label".len();
        if !repository_rule_parse_label_repo_name_prefix(bytes, &mut index) {
            offset = index;
            continue;
        }
        if let Some((attr, end)) = repository_rule_parse_ctx_attr_name(source, index) {
            names.insert(attr);
            offset = end;
            continue;
        }
        if let Some((local, end)) = repository_rule_parse_identifier(source, index) {
            alias_locals.insert(local.to_owned());
            offset = end;
            continue;
        }
        offset = index;
    }
}

fn repository_rule_parse_label_repo_name_prefix(bytes: &[u8], index: &mut usize) -> bool {
    repository_rule_skip_ascii_whitespace(bytes, index);
    if !repository_rule_consume_byte(bytes, index, b'(') {
        return false;
    }
    repository_rule_skip_ascii_whitespace(bytes, index);
    let Some(quote @ (b'"' | b'\'')) = bytes.get(*index).copied() else {
        return false;
    };
    *index += 1;
    if !repository_rule_consume_byte(bytes, index, b'@') {
        return false;
    }
    let _canonical = repository_rule_consume_byte(bytes, index, b'@');
    if !repository_rule_consume_byte(bytes, index, quote) {
        return false;
    }
    repository_rule_skip_ascii_whitespace(bytes, index);
    if !repository_rule_consume_byte(bytes, index, b'+') {
        return false;
    }
    repository_rule_skip_ascii_whitespace(bytes, index);
    true
}

fn repository_rule_parse_ctx_attr_name(source: &str, mut index: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    repository_rule_skip_ascii_whitespace(bytes, &mut index);
    if !repository_rule_consume_keyword(source, &mut index, "ctx") {
        return None;
    }
    repository_rule_skip_ascii_whitespace(bytes, &mut index);
    if !repository_rule_consume_byte(bytes, &mut index, b'.') {
        return None;
    }
    repository_rule_skip_ascii_whitespace(bytes, &mut index);
    if !repository_rule_consume_keyword(source, &mut index, "attr") {
        return None;
    }
    repository_rule_skip_ascii_whitespace(bytes, &mut index);
    if !repository_rule_consume_byte(bytes, &mut index, b'.') {
        return None;
    }
    repository_rule_skip_ascii_whitespace(bytes, &mut index);
    let (name, index) = repository_rule_parse_identifier(source, index)?;
    Some((name.to_owned(), index))
}

fn repository_rule_source_uses_unresolved_dynamic_label(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut offset = 0usize;
    while let Some(found) = source[offset..].find("Label") {
        let mut index = offset + found + "Label".len();
        repository_rule_skip_ascii_whitespace(bytes, &mut index);
        if !repository_rule_consume_byte(bytes, &mut index, b'(') {
            offset = index;
            continue;
        }
        repository_rule_skip_ascii_whitespace(bytes, &mut index);
        let Some(quote @ (b'"' | b'\'')) = bytes.get(index).copied() else {
            offset = index;
            continue;
        };
        index += 1;
        if bytes.get(index) == Some(&b'{') {
            return true;
        }
        while index < bytes.len() && bytes[index] != quote {
            index += 1;
        }
        offset = index;
    }
    false
}

fn repository_rule_skip_ascii_whitespace(bytes: &[u8], index: &mut usize) {
    while *index < bytes.len() && bytes[*index].is_ascii_whitespace() {
        *index += 1;
    }
}

fn repository_rule_consume_byte(bytes: &[u8], index: &mut usize, expected: u8) -> bool {
    if bytes.get(*index) == Some(&expected) {
        *index += 1;
        true
    } else {
        false
    }
}

fn repository_rule_consume_keyword(source: &str, index: &mut usize, keyword: &str) -> bool {
    let Some(rest) = source.get(*index..) else {
        return false;
    };
    let Some(after) = rest.strip_prefix(keyword) else {
        return false;
    };
    if after
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return false;
    }
    *index += keyword.len();
    true
}

fn repository_rule_parse_identifier(source: &str, index: usize) -> Option<(&str, usize)> {
    let rest = source.get(index..)?;
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    let mut end = index + first.len_utf8();
    for (offset, ch) in chars {
        if !ch.is_ascii_alphanumeric() && ch != '_' {
            return Some((&source[index..index + offset], index + offset));
        }
        end = index + offset + ch.len_utf8();
    }
    Some((&source[index..end], end))
}

fn collect_repository_rule_ctx_attr_alias_attrs(
    source: &str,
    alias_locals: &BTreeSet<String>,
    names: &mut BTreeSet<String>,
) {
    if alias_locals.is_empty() {
        return;
    }
    for line in source.lines() {
        let line = line.trim();
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        let left = left.trim();
        if !alias_locals.contains(left) {
            continue;
        }
        let right = right.trim();
        let Some((attr, _end)) = repository_rule_parse_ctx_attr_name(right, 0) else {
            continue;
        };
        names.insert(attr);
    }
}

fn collect_repository_rule_attr_names_after(
    source: &str,
    pattern: &str,
    names: &mut BTreeSet<String>,
) {
    let mut offset = 0usize;
    while let Some(found) = source[offset..].find(pattern) {
        let start = offset + found + pattern.len();
        let name = source[start..]
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .collect::<String>();
        let name_len = name.len();
        if !name.is_empty() {
            names.insert(name);
        }
        offset = start.saturating_add(name_len);
    }
}

fn empty_dict_value<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocDict(Vec::<(Value<'v>, Value<'v>)>::new()))
}

fn bazel_host_os_name() -> &'static str {
    match env::consts::OS {
        "macos" => "mac os x",
        "windows" => "windows",
        other => other,
    }
}

fn host_environ<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocDict(env::vars_os().map(|(key, value)| {
        (
            key.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        )
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buck2_hash::StdBuckHashMap;

    #[test]
    fn test_repository_rule_string_repo_name() {
        assert_eq!(
            repository_rule_string_repo_name("@@rules_go++go_sdk+main___download_0//:ROOT"),
            Some((true, "rules_go++go_sdk+main___download_0"))
        );
        assert_eq!(
            repository_rule_string_repo_name("@rules_go++go_sdk+main___download_0"),
            Some((false, "rules_go++go_sdk+main___download_0"))
        );
        assert_eq!(repository_rule_string_repo_name("@types/node"), None);
        assert_eq!(
            repository_rule_string_repo_name("https://example.com/@repo"),
            None
        );
        assert_eq!(
            repository_rule_string_repo_name_template("@yq_{}//:yq{}"),
            Some((false, "yq_{}"))
        );
        assert!(repository_rule_repo_template_matches(
            "yq_{}",
            "yq_darwin_arm64"
        ));
        assert!(!repository_rule_repo_template_matches(
            "yq_{}",
            "nodejs_darwin_arm64"
        ));
        assert!(!repository_rule_repo_template_matches("{}", "anything"));
    }

    #[test]
    fn test_repository_rule_string_label_template_deps() {
        let mut aliases = StdBuckHashMap::default();
        aliases.insert(
            NonEmptyCellAlias::new("yq_darwin_arm64".to_owned()).unwrap(),
            CellName::testing_new("bzlmod_yq_darwin_arm64"),
        );
        aliases.insert(
            NonEmptyCellAlias::new("yq_linux_amd64".to_owned()).unwrap(),
            CellName::testing_new("bzlmod_yq_linux_amd64"),
        );
        aliases.insert(
            NonEmptyCellAlias::new("nodejs_darwin_arm64".to_owned()).unwrap(),
            CellName::testing_new("bzlmod_nodejs_darwin_arm64"),
        );
        let resolver = CellAliasResolver::new(CellName::testing_new("current"), aliases).unwrap();
        let mut label_deps = BTreeSet::new();

        collect_bzlmod_module_extension_string_label_deps(
            r#"host_yq = Label("@yq_{}//:yq{}".format(platform, extension))"#,
            &mut label_deps,
            &resolver,
        );

        assert!(label_deps.contains("bzlmod_yq_darwin_arm64"));
        assert!(label_deps.contains("bzlmod_yq_linux_amd64"));
        assert!(!label_deps.contains("bzlmod_nodejs_darwin_arm64"));
    }

    #[test]
    fn test_repository_rule_source_uses_unresolved_dynamic_label() {
        assert!(repository_rule_source_uses_unresolved_dynamic_label(
            r#"Label("{repo}//:BUILD.bazel".format(repo = repo))"#
        ));
        assert!(repository_rule_source_uses_unresolved_dynamic_label(
            r#"Label ( "{repo}//:BUILD.bazel".format(repo = repo))"#
        ));
        assert!(!repository_rule_source_uses_unresolved_dynamic_label(
            r#"Label("@yq_{}//:yq{}".format(platform, extension))"#
        ));
    }

    #[test]
    fn test_repository_rule_dynamic_label_attr_names() {
        let source = r#"
def _impl(ctx):
    root = Label(ctx.attr.root_files[platform])
    config = ctx.path(ctx.attr.config)
    go_sdk_name = ctx.attr.go_sdk_name
    go_sdk_label = Label("@" + go_sdk_name + "//:ROOT")
    direct = Label("@@" + ctx.attr.direct_repo + "//:ROOT")
    spaced = Label ( "@@" + ctx . attr . spaced_repo + "//:ROOT")
    ctx.file("go_tools.bzl", "GO_TOOLS = {k: Label(v) for k, v in %r}" % ctx.attr.tool_targets)
"#;
        let names = repository_rule_dynamic_label_attr_names(source);
        assert!(names.contains("root_files"));
        assert!(names.contains("config"));
        assert!(!names.contains("tool_targets"));
        let names = repository_rule_dynamic_repo_name_attr_names(source);
        assert!(names.contains("go_sdk_name"));
        assert!(names.contains("direct_repo"));
        assert!(names.contains("spaced_repo"));
        assert!(!names.contains("tool_targets"));
    }

    #[test]
    fn test_repository_rule_loaded_module_scan_skips_prelude() {
        assert!(!repository_rule_should_scan_loaded_module_cell("prelude"));
        assert!(repository_rule_should_scan_loaded_module_cell(
            "bazel_tools"
        ));
        assert!(repository_rule_should_scan_loaded_module_cell(
            "bzlmod_rules_go_"
        ));
    }

    #[test]
    fn test_repository_ctx_external_input_dep_includes_path() {
        assert_eq!(
            repository_ctx_external_input_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod/gazelle+/internal/list_repository_tools_srcs.go",
            )),
            Some(RepositoryPathLabelDep::cell_path(
                "bzlmod_gazelle_".to_owned(),
                "internal/list_repository_tools_srcs.go".to_owned(),
            ))
        );
        assert_eq!(
            repository_ctx_external_input_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0/bin/go",
            )),
            Some(RepositoryPathLabelDep::cell_path(
                "bzlmod_rules_go__go_sdk_main___download_0".to_owned(),
                "bin/go".to_owned(),
            ))
        );
        assert_eq!(
            repository_ctx_external_input_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod_generated/repo.repository_ctx/file",
            )),
            None
        );
        assert_eq!(
            repository_ctx_external_input_tree_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod/gazelle+",
            )),
            Some(RepositoryPathLabelDep::tree(
                "bzlmod_gazelle_".to_owned(),
                None,
            ))
        );
        assert_eq!(
            repository_ctx_external_input_tree_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod/gazelle+/internal",
            )),
            Some(RepositoryPathLabelDep::tree(
                "bzlmod_gazelle_".to_owned(),
                Some("internal".to_owned()),
            ))
        );
    }

    #[test]
    fn test_repository_ctx_command_path_preserves_external_assignment_prefix() {
        let working_dir =
            "buck-out/v2/external_cells/bzlmod_generated/gazelle++deps+tools.repository_ctx";
        let rewritten = repository_ctx_command_path(
            "GOROOT=/repo/buck-out/buildbuddy-source-file-1/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0",
            working_dir,
        );
        assert!(rewritten.starts_with("GOROOT="));
        assert!(rewritten.contains(
            "/buck-out/v2/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0"
        ));

        let rewritten = repository_ctx_command_path(
            "/repo/buck-out/buildbuddy-source-file-1/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0/bin/go",
            working_dir,
        );
        assert!(rewritten.ends_with(
            "/buck-out/v2/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0/bin/go"
        ));
    }

    #[test]
    fn test_repository_rule_string_literals() {
        assert_eq!(
            repository_rule_string_literals(
                r#"{"host": "@@rules_go++go_sdk+download_0//:ROOT", "plain": "value"}"#
            ),
            vec![
                "host".to_owned(),
                "@@rules_go++go_sdk+download_0//:ROOT".to_owned(),
                "plain".to_owned(),
                "value".to_owned()
            ]
        );
    }

    #[test]
    fn test_module_ctx_checksum_from_sha384_integrity() {
        let integrity = "sha384-ZoEgzfCLmDk7eoKdJSoq/nny1iX3Cq9mMJ3gnPZ2ejhKMxSgHUQIa7MREToxYl6Z";
        let checksum = module_ctx_checksum_from_integrity(integrity)
            .unwrap()
            .unwrap();
        assert_eq!(checksum.kind, ModuleCtxChecksumKind::Sha384);
        assert_eq!(checksum.hex.len(), 96);
        assert_eq!(
            module_ctx_integrity_from_checksum(&checksum).unwrap(),
            integrity
        );
    }
}

fn validate_module_extension_return<'v>(
    extension_id: &StarlarkRuleType,
    value: Value<'v>,
) -> starlark::Result<Value<'v>> {
    if value.is_none()
        || value
            .downcast_ref::<StarlarkModuleExtensionMetadata>()
            .is_some()
    {
        return Ok(value);
    }
    Err(
        buck2_error::Error::from(BazelRepositoryError::InvalidModuleExtensionReturn(
            extension_id.to_string(),
            value.get_type().to_owned(),
        ))
        .into(),
    )
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionEvaluationConfig {
    root_module_has_non_dev_dependency: bool,
    modules: Vec<BzlmodModuleExtensionModuleConfig>,
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionModuleConfig {
    name: String,
    version: String,
    #[allow(dead_code)]
    canonical_repo_name: String,
    is_root: bool,
    constants: Vec<(String, String)>,
    tags: Vec<BzlmodModuleExtensionTagConfig>,
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionTagConfig {
    tag_name: String,
    dev_dependency: bool,
    bindings: Vec<(String, String)>,
    kwargs: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BazelRepositoryGeneratedFile {
    pub path: String,
    pub content: String,
    pub executable: bool,
}

pub(crate) enum BazelRepositoryRuleEvaluation {
    Success(Vec<BazelRepositoryGeneratedFile>),
    NeedsPathLabelDeps {
        label_deps: Vec<RepositoryPathLabelDep>,
        error: String,
    },
}

pub enum BazelModuleExtensionEvaluation {
    Success(BazelModuleExtensionEvaluationResult),
    NeedsPathLabelDeps {
        label_deps: Vec<RepositoryPathLabelDep>,
        error: String,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Freeze, Allocative)]
pub struct RepositoryPathLabelDep {
    cell_name: String,
    path: Option<String>,
    recursive: bool,
}

impl RepositoryPathLabelDep {
    fn cell(cell_name: String) -> Self {
        Self {
            cell_name,
            path: None,
            recursive: false,
        }
    }

    fn cell_path(cell_name: String, path: String) -> Self {
        Self {
            cell_name,
            path: Some(path),
            recursive: false,
        }
    }

    fn tree(cell_name: String, path: Option<String>) -> Self {
        Self {
            cell_name,
            path,
            recursive: true,
        }
    }
}

pub async fn evaluate_bzlmod_module_extension_repo(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
    module_ctx_working_dir: &str,
    current_canonical_repo_name: Option<&str>,
    cancellation: &CancellationContext,
) -> buck2_error::Result<BazelModuleExtensionEvaluationResult> {
    let extension_cell_path = CellPath::new(
        CellName::unchecked_new(&setup.extension_bzl_cell)?,
        CellRelativePathBuf::try_from(setup.extension_bzl_path.to_string())?,
    );
    let extension_path = ImportPath::new_same_cell(extension_cell_path)?;
    materialize_bzlmod_module_extension_source_label_deps(
        ctx,
        &extension_path,
        setup,
        current_canonical_repo_name,
    )
    .await?;
    let extension_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(&extension_path))
        .await?;
    materialize_bzlmod_module_extension_loaded_module_label_deps(
        ctx,
        &extension_path,
        current_canonical_repo_name,
    )
    .await?;
    let mut materialized_path_label_deps = BTreeSet::new();
    loop {
        let mut interpreter = ctx
            .get_interpreter_calculator(OwnedStarlarkPath::LoadFile(extension_path.clone()))
            .await?;
        match interpreter
            .eval_bzlmod_module_extension(
                &extension_path,
                &extension_module,
                &setup.extension_name,
                &setup.extension_usages_json,
                module_ctx_working_dir,
                cancellation,
            )
            .await?
        {
            BazelModuleExtensionEvaluation::Success(result) => return Ok(result),
            BazelModuleExtensionEvaluation::NeedsPathLabelDeps { label_deps, error } => {
                let new_label_deps = label_deps
                    .into_iter()
                    .filter(|dep| materialized_path_label_deps.insert(dep.clone()))
                    .collect::<Vec<_>>();
                if new_label_deps.is_empty() {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "module_extension `{}%{}` failed after materializing module_ctx path labels: {}",
                        extension_path,
                        setup.extension_name,
                        error
                    ));
                }
                materialize_repository_rule_path_label_deps(
                    ctx,
                    &new_label_deps,
                    LabelDepMaterialization::AllExternal,
                )
                .await?;
                repository_ctx_clean_working_dir(module_ctx_working_dir)?;
            }
        }
    }
}

async fn materialize_bzlmod_module_extension_source_label_deps(
    ctx: &mut DiceComputations<'_>,
    extension_path: &ImportPath,
    setup: &BzlmodModuleExtensionRepoSetup,
    current_canonical_repo_name: Option<&str>,
) -> buck2_error::Result<()> {
    let scan = repository_rule_source_label_dep_scan_for_path(ctx, extension_path).await?;
    let extension_cell_alias_resolver = ctx
        .get_cell_alias_resolver(extension_path.path().cell())
        .await?;
    let mut label_deps = BTreeSet::new();
    collect_repository_rule_label_literal_deps(
        &scan.labels,
        &mut label_deps,
        &extension_cell_alias_resolver,
    );

    let config: BzlmodModuleExtensionEvaluationConfig =
        serde_json::from_str(&setup.extension_usages_json).map_err(|e| {
            buck2_error::Error::from(BazelRepositoryError::InvalidModuleExtensionUsageData)
                .context(format!("JSON parse error: {e}"))
        })?;
    for module in config.modules {
        let module_cell = bzlmod_module_cell_name_from_config(ctx, &module).await?;
        let module_alias_resolver = ctx.get_cell_alias_resolver(module_cell).await?;
        for (_name, expression) in &module.constants {
            collect_repository_rule_string_label_deps_from_expression(
                expression,
                &mut label_deps,
                &module_alias_resolver,
            );
        }
        for tag in &module.tags {
            for (_name, expression) in &tag.bindings {
                collect_repository_rule_string_label_deps_from_expression(
                    expression,
                    &mut label_deps,
                    &module_alias_resolver,
                );
            }
            for (_name, expression) in &tag.kwargs {
                collect_repository_rule_string_label_deps_from_expression(
                    expression,
                    &mut label_deps,
                    &module_alias_resolver,
                );
            }
        }
    }
    if let Some(current_canonical_repo_name) = current_canonical_repo_name {
        // A concrete generated repo can be mentioned by its own extension source. It
        // cannot be materialized before the extension has emitted its repository_rule.
        label_deps.remove(&bzlmod_cell_name(current_canonical_repo_name));
    }
    let label_deps = label_deps.into_iter().collect::<Vec<_>>();
    materialize_repository_rule_label_deps(
        ctx,
        &label_deps,
        LabelDepMaterialization::NonGeneratedExternalOnly,
    )
    .await
}

async fn materialize_bzlmod_module_extension_loaded_module_label_deps(
    ctx: &mut DiceComputations<'_>,
    extension_path: &ImportPath,
    current_canonical_repo_name: Option<&str>,
) -> buck2_error::Result<()> {
    let loaded_paths = repository_rule_loaded_module_load_paths(ctx, extension_path).await?;

    let mut label_deps = BTreeSet::new();
    for path in loaded_paths.iter() {
        let scan = repository_rule_source_label_dep_scan_for_path(ctx, path).await?;
        let cell_alias_resolver = ctx.get_cell_alias_resolver(path.path().cell()).await?;
        collect_repository_rule_label_literal_deps(
            &scan.labels,
            &mut label_deps,
            &cell_alias_resolver,
        );
    }
    if let Some(current_canonical_repo_name) = current_canonical_repo_name {
        label_deps.remove(&bzlmod_cell_name(current_canonical_repo_name));
    }
    let label_deps = label_deps.into_iter().collect::<Vec<_>>();
    materialize_repository_rule_label_deps(
        ctx,
        &label_deps,
        LabelDepMaterialization::NonGeneratedExternalOnly,
    )
    .await
}

fn collect_loaded_module_load_paths(
    module: &LoadedModule,
    seen: &mut BTreeSet<String>,
    paths: &mut Vec<ImportPath>,
) {
    for loaded in module.loaded_modules().map.values() {
        let key = loaded.path().to_string();
        if !seen.insert(key) {
            continue;
        }
        if let StarlarkModulePath::LoadFile(path) = loaded.path() {
            if !repository_rule_should_scan_loaded_module_cell(path.path().cell().as_str()) {
                continue;
            }
            paths.push(path.clone());
        }
        collect_loaded_module_load_paths(loaded, seen, paths);
    }
}

fn repository_rule_should_scan_loaded_module_cell(cell_name: &str) -> bool {
    cell_name != "prelude"
}

async fn bzlmod_module_cell_name_from_config(
    ctx: &mut DiceComputations<'_>,
    module_config: &BzlmodModuleExtensionModuleConfig,
) -> buck2_error::Result<CellName> {
    if module_config.is_root {
        return Ok(ctx.get_cell_resolver().await?.root_cell());
    }
    if module_config.canonical_repo_name == "bazel_tools" {
        return CellName::unchecked_new("bazel_tools");
    }
    CellName::unchecked_new(&bzlmod_cell_name(&module_config.canonical_repo_name))
}

#[cfg(test)]
fn collect_bzlmod_module_extension_string_label_deps(
    source: &str,
    label_deps: &mut BTreeSet<String>,
    cell_alias_resolver: &CellAliasResolver,
) {
    for label in repository_rule_label_literals(source) {
        match label {
            RepositoryRuleLabelLiteral::Apparent(repo_name) => {
                if let Ok(cell_name) = cell_alias_resolver.resolve(&repo_name) {
                    label_deps.insert(cell_name.as_str().to_owned());
                }
            }
            RepositoryRuleLabelLiteral::ApparentTemplate(repo_template) => {
                collect_repository_rule_repo_template_label_deps(
                    &repo_template,
                    label_deps,
                    cell_alias_resolver,
                );
            }
            RepositoryRuleLabelLiteral::Canonical(canonical_repo_name) => {
                label_deps.insert(bzlmod_cell_name(&canonical_repo_name));
            }
        }
    }
}

pub async fn evaluate_bzlmod_repository_rule(
    ctx: &mut DiceComputations<'_>,
    invocation: &BazelRepositoryRuleInvocation,
    repository_ctx_working_dir: &str,
    cancellation: &CancellationContext,
) -> buck2_error::Result<Vec<BazelRepositoryGeneratedFile>> {
    let rule_path = match &invocation.rule_id.path {
        BzlOrBxlPath::Bzl(path) => path,
        BzlOrBxlPath::Bxl(_) => {
            return Err(BazelRepositoryError::RepositoryRuleBxlUnsupported(
                invocation.rule_id.to_string(),
            )
            .into());
        }
    };
    materialize_repository_rule_label_deps(
        ctx,
        &invocation.label_deps,
        LabelDepMaterialization::AllExternal,
    )
    .await?;
    materialize_repository_rule_source_label_deps(ctx, rule_path, invocation).await?;
    let rule_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(rule_path))
        .await?;
    materialize_repository_rule_loaded_module_label_deps(ctx, rule_path, invocation).await?;
    let mut materialized_path_label_deps = BTreeSet::new();
    loop {
        let mut interpreter = ctx
            .get_interpreter_calculator(OwnedStarlarkPath::LoadFile(rule_path.clone()))
            .await?;
        match interpreter
            .eval_bzlmod_repository_rule(
                rule_path,
                &rule_module,
                invocation,
                repository_ctx_working_dir,
                cancellation,
            )
            .await?
        {
            BazelRepositoryRuleEvaluation::Success(files) => return Ok(files),
            BazelRepositoryRuleEvaluation::NeedsPathLabelDeps { label_deps, error } => {
                let new_label_deps = label_deps
                    .into_iter()
                    .filter(|dep| materialized_path_label_deps.insert(dep.clone()))
                    .collect::<Vec<_>>();
                if new_label_deps.is_empty() {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "repository_rule `{}` failed after materializing repository_ctx path labels: {}",
                        invocation.rule_id,
                        error
                    ));
                }
                materialize_repository_rule_path_label_deps(
                    ctx,
                    &new_label_deps,
                    LabelDepMaterialization::AllExternal,
                )
                .await?;
                repository_ctx_clean_working_dir(repository_ctx_working_dir)?;
            }
        }
    }
}

pub async fn repository_rule_uses_unresolved_dynamic_label(
    ctx: &mut DiceComputations<'_>,
    invocation: &BazelRepositoryRuleInvocation,
) -> buck2_error::Result<bool> {
    let rule_path = match &invocation.rule_id.path {
        BzlOrBxlPath::Bzl(path) => path,
        BzlOrBxlPath::Bxl(_) => return Ok(false),
    };
    let source = DiceFileComputations::read_file(ctx, rule_path.path().as_ref())
        .await
        .with_package_context_information(rule_path.path().path().to_string())?;
    if repository_rule_source_uses_unresolved_dynamic_label(&source) {
        return Ok(true);
    }

    let loaded_paths = repository_rule_loaded_module_load_paths(ctx, rule_path).await?;
    for path in loaded_paths.iter() {
        let source = DiceFileComputations::read_file(ctx, path.path().as_ref())
            .await
            .with_package_context_information(path.path().path().to_string())?;
        if repository_rule_source_uses_unresolved_dynamic_label(&source) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Clone, Debug, Eq, PartialEq, Allocative)]
enum RepositoryRuleLabelLiteral {
    Apparent(String),
    ApparentTemplate(String),
    Canonical(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Allocative)]
struct RepositoryRuleSourceLabelDepScan {
    labels: Vec<RepositoryRuleLabelLiteral>,
    dynamic_label_attr_names: Vec<String>,
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("repository rule source label dependency scan for {}", path)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct RepositoryRuleSourceLabelDepScanKey {
    path: CellPath,
}

#[async_trait::async_trait]
impl Key for RepositoryRuleSourceLabelDepScanKey {
    type Value = buck2_error::Result<Arc<RepositoryRuleSourceLabelDepScan>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let source = DiceFileComputations::read_file(ctx, self.path.as_ref())
            .await
            .with_package_context_information(self.path.path().to_string())?;
        Ok(Arc::new(repository_rule_source_label_dep_scan(&source)))
    }

    fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
        false
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn repository_rule_source_label_dep_scan_for_path(
    ctx: &mut DiceComputations<'_>,
    path: &ImportPath,
) -> buck2_error::Result<Arc<RepositoryRuleSourceLabelDepScan>> {
    ctx.compute(&RepositoryRuleSourceLabelDepScanKey {
        path: path.path().clone(),
    })
    .await?
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("repository rule loaded module load paths for {}", path)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct RepositoryRuleLoadedModuleLoadPathsKey {
    path: ImportPath,
}

#[async_trait::async_trait]
impl Key for RepositoryRuleLoadedModuleLoadPathsKey {
    type Value = buck2_error::Result<Arc<Vec<ImportPath>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let module = ctx
            .get_loaded_module(StarlarkModulePath::LoadFile(&self.path))
            .await?;
        let mut paths = Vec::new();
        collect_loaded_module_load_paths(&module, &mut BTreeSet::new(), &mut paths);
        Ok(Arc::new(paths))
    }

    fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
        false
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn repository_rule_loaded_module_load_paths(
    ctx: &mut DiceComputations<'_>,
    path: &ImportPath,
) -> buck2_error::Result<Arc<Vec<ImportPath>>> {
    ctx.compute(&RepositoryRuleLoadedModuleLoadPathsKey { path: path.clone() })
        .await?
}

async fn materialize_repository_rule_source_label_deps(
    ctx: &mut DiceComputations<'_>,
    rule_path: &ImportPath,
    invocation: &BazelRepositoryRuleInvocation,
) -> buck2_error::Result<()> {
    let scan = repository_rule_source_label_dep_scan_for_path(ctx, rule_path).await?;
    let cell_alias_resolver = ctx.get_cell_alias_resolver(rule_path.path().cell()).await?;
    let attrs = repository_rule_attr_expressions_by_name(&invocation.attrs);
    let mut label_deps = BTreeSet::new();
    collect_repository_rule_source_scan_label_deps(
        &scan,
        &attrs,
        &mut label_deps,
        &cell_alias_resolver,
    );
    let label_deps = label_deps.into_iter().collect::<Vec<_>>();
    materialize_repository_rule_label_deps(
        ctx,
        &label_deps,
        LabelDepMaterialization::NonGeneratedExternalOnly,
    )
    .await
}

async fn materialize_repository_rule_loaded_module_label_deps(
    ctx: &mut DiceComputations<'_>,
    rule_path: &ImportPath,
    invocation: &BazelRepositoryRuleInvocation,
) -> buck2_error::Result<()> {
    let loaded_paths = repository_rule_loaded_module_load_paths(ctx, rule_path).await?;
    let attrs = repository_rule_attr_expressions_by_name(&invocation.attrs);
    let mut label_deps = BTreeSet::new();
    for path in loaded_paths.iter() {
        let scan = repository_rule_source_label_dep_scan_for_path(ctx, path).await?;
        let cell_alias_resolver = ctx.get_cell_alias_resolver(path.path().cell()).await?;
        collect_repository_rule_source_scan_label_deps(
            &scan,
            &attrs,
            &mut label_deps,
            &cell_alias_resolver,
        );
    }
    let label_deps = label_deps.into_iter().collect::<Vec<_>>();
    materialize_repository_rule_label_deps(
        ctx,
        &label_deps,
        LabelDepMaterialization::NonGeneratedExternalOnly,
    )
    .await
}

fn collect_repository_rule_source_scan_label_deps(
    scan: &RepositoryRuleSourceLabelDepScan,
    attrs: &BTreeMap<&str, &str>,
    label_deps: &mut BTreeSet<String>,
    cell_alias_resolver: &CellAliasResolver,
) {
    collect_repository_rule_label_literal_deps(&scan.labels, label_deps, cell_alias_resolver);
    for attr_name in &scan.dynamic_label_attr_names {
        let Some(expression) = attrs.get(attr_name.as_str()) else {
            continue;
        };
        collect_repository_rule_string_label_deps_from_expression(
            expression,
            label_deps,
            cell_alias_resolver,
        );
    }
}

fn collect_repository_rule_label_literal_deps(
    labels: &[RepositoryRuleLabelLiteral],
    label_deps: &mut BTreeSet<String>,
    cell_alias_resolver: &CellAliasResolver,
) {
    for label in labels {
        let cell_name = match label {
            RepositoryRuleLabelLiteral::Apparent(repo_name) => {
                match cell_alias_resolver.resolve(repo_name) {
                    Ok(cell_name) => cell_name.to_string(),
                    Err(_) => continue,
                }
            }
            RepositoryRuleLabelLiteral::ApparentTemplate(repo_template) => {
                collect_repository_rule_repo_template_label_deps(
                    repo_template,
                    label_deps,
                    cell_alias_resolver,
                );
                continue;
            }
            RepositoryRuleLabelLiteral::Canonical(canonical_repo_name) => {
                bzlmod_cell_name(canonical_repo_name)
            }
        };
        label_deps.insert(cell_name);
    }
}

fn repository_rule_attr_expressions_by_name(attrs: &[(String, String)]) -> BTreeMap<&str, &str> {
    attrs
        .iter()
        .map(|(name, expression)| (name.as_str(), expression.as_str()))
        .collect()
}

fn repository_rule_source_label_dep_scan(source: &str) -> RepositoryRuleSourceLabelDepScan {
    let mut dynamic_label_attr_names = repository_rule_dynamic_label_attr_names(source);
    dynamic_label_attr_names.extend(repository_rule_dynamic_repo_name_attr_names(source));
    RepositoryRuleSourceLabelDepScan {
        labels: repository_rule_label_literals(source),
        dynamic_label_attr_names: dynamic_label_attr_names.into_iter().collect(),
    }
}

fn repository_rule_label_literals(source: &str) -> Vec<RepositoryRuleLabelLiteral> {
    let mut labels = Vec::new();
    let mut offset = 0usize;
    while let Some(label_offset) = source[offset..].find("Label(") {
        let mut index = offset + label_offset + "Label(".len();
        let bytes = source.as_bytes();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || (bytes[index] != b'"' && bytes[index] != b'\'') {
            offset = index;
            continue;
        }
        index += 1;
        if index >= bytes.len() || bytes[index] != b'@' {
            offset = index;
            continue;
        }
        index += 1;
        let canonical = if index < bytes.len() && bytes[index] == b'@' {
            index += 1;
            true
        } else {
            false
        };
        let repo_start = index;
        while index + 1 < bytes.len() && !(bytes[index] == b'/' && bytes[index + 1] == b'/') {
            index += 1;
        }
        if index + 1 >= bytes.len() || index == repo_start {
            offset = index;
            continue;
        }
        let repo_name = &source[repo_start..index];
        labels.push(if canonical {
            if repository_rule_repo_template(repo_name).is_some() {
                offset = index + 2;
                continue;
            }
            RepositoryRuleLabelLiteral::Canonical(repo_name.to_owned())
        } else if repository_rule_repo_template(repo_name).is_some() {
            RepositoryRuleLabelLiteral::ApparentTemplate(repo_name.to_owned())
        } else {
            RepositoryRuleLabelLiteral::Apparent(repo_name.to_owned())
        });
        offset = index + 2;
    }
    labels
}

#[derive(Clone, Copy)]
enum LabelDepMaterialization {
    AllExternal,
    // Static source scans are speculative. Generated repos are emitted by module
    // extensions and should materialize only from concrete attrs or runtime paths.
    NonGeneratedExternalOnly,
}

async fn materialize_repository_rule_label_deps(
    ctx: &mut DiceComputations<'_>,
    label_deps: &[String],
    materialization: LabelDepMaterialization,
) -> buck2_error::Result<()> {
    let label_deps = label_deps
        .iter()
        .map(|cell_name| RepositoryPathLabelDep::cell(cell_name.clone()))
        .collect::<Vec<_>>();
    materialize_repository_rule_path_label_deps(ctx, &label_deps, materialization).await
}

async fn materialize_repository_rule_path_label_deps(
    ctx: &mut DiceComputations<'_>,
    label_deps: &[RepositoryPathLabelDep],
    materialization: LabelDepMaterialization,
) -> buck2_error::Result<()> {
    let mut seen = BTreeSet::new();
    for dep in label_deps {
        if !seen.insert(dep) {
            continue;
        }
        let cell_name = CellName::unchecked_new(&dep.cell_name)?;
        let should_materialize = {
            let cell_resolver = ctx.get_cell_resolver().await?;
            match cell_resolver.get(cell_name) {
                Ok(cell) => match cell.external() {
                    Some(ExternalCellOrigin::BzlmodGenerated(_)) => {
                        matches!(materialization, LabelDepMaterialization::AllExternal)
                    }
                    Some(_) => true,
                    None => false,
                },
                Err(_) => false,
            }
        };
        if !should_materialize {
            continue;
        }
        match &dep.path {
            Some(path) if dep.recursive => {
                materialize_repository_rule_path_label_dep_tree(ctx, cell_name, path).await?;
            }
            Some(path) => {
                let cell_path =
                    CellPath::new(cell_name, CellRelativePathBuf::try_from(path.to_owned())?);
                DiceFileComputations::read_path_metadata_if_exists(ctx, cell_path.as_ref()).await?;
            }
            None if dep.recursive => {
                materialize_repository_rule_path_label_dep_tree(ctx, cell_name, "").await?;
            }
            None => {
                let cell_root =
                    CellPath::new(cell_name, CellRelativePathBuf::unchecked_new(String::new()));
                DiceFileComputations::read_dir(ctx, cell_root.as_ref()).await?;
            }
        }
    }
    Ok(())
}

async fn materialize_repository_rule_path_label_dep_tree(
    ctx: &mut DiceComputations<'_>,
    cell_name: CellName,
    path: &str,
) -> buck2_error::Result<()> {
    let root = CellPath::new(cell_name, CellRelativePathBuf::try_from(path.to_owned())?);
    let Some(metadata) =
        DiceFileComputations::read_path_metadata_if_exists(ctx, root.as_ref()).await?
    else {
        return Ok(());
    };
    if !matches!(metadata, RawPathMetadata::Directory) {
        return Ok(());
    }

    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        let entries = DiceFileComputations::read_dir(ctx, dir.as_ref()).await?;
        for entry in entries.included.iter() {
            let child = dir.join(&entry.file_name);
            if entry.file_type.is_dir() {
                dirs.push(child);
            } else {
                DiceFileComputations::read_path_metadata_if_exists(ctx, child.as_ref()).await?;
            }
        }
    }
    Ok(())
}

pub async fn evaluate_bzlmod_repository_rule_invocation(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodRepositoryRuleInvocationSetup,
    canonical_repo_name: &str,
    repository_ctx_working_dir: &str,
    cancellation: &CancellationContext,
) -> buck2_error::Result<Vec<BazelRepositoryGeneratedFile>> {
    let invocation = bzlmod_repository_rule_invocation_from_setup(setup, canonical_repo_name)?;
    evaluate_bzlmod_repository_rule(ctx, &invocation, repository_ctx_working_dir, cancellation)
        .await
}

pub fn bzlmod_repository_rule_invocation_from_setup(
    setup: &BzlmodRepositoryRuleInvocationSetup,
    canonical_repo_name: &str,
) -> buck2_error::Result<BazelRepositoryRuleInvocation> {
    let rule_cell = CellName::unchecked_new(&setup.rule_bzl_cell)?;
    let rule_path = CellPath::new(
        rule_cell,
        CellRelativePathBuf::try_from(setup.rule_bzl_path.to_string())?,
    );
    let build_file_cell =
        BuildFileCell::new(CellName::unchecked_new(&setup.rule_bzl_build_file_cell)?);
    let rule_path = ImportPath::new_with_build_file_cells(rule_path, build_file_cell)?;
    Ok(BazelRepositoryRuleInvocation {
        rule_id: StarlarkRuleType {
            path: BzlOrBxlPath::Bzl(rule_path),
            name: setup.rule_name.to_string(),
        },
        name: canonical_repo_name.to_owned(),
        original_name: setup.repo_name.to_string(),
        attrs: setup
            .attrs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        label_deps: setup
            .label_deps
            .iter()
            .map(|cell_name| cell_name.to_string())
            .collect(),
    })
}

pub(crate) fn module_extension_from_loaded_module(
    extension_module_path: &ImportPath,
    extension_name: &str,
    extension_value: starlark::values::OwnedFrozenValue,
) -> buck2_error::Result<starlark::values::OwnedFrozenValueTyped<FrozenStarlarkModuleExtension>> {
    extension_value.downcast_starlark().map_err(|err| {
        let got = err.to_string();
        BazelRepositoryError::ModuleExtensionSymbolWrongType {
            path: extension_module_path.to_string(),
            extension: extension_name.to_owned(),
            got,
        }
        .into()
    })
}

pub(crate) fn repository_rule_from_loaded_module(
    rule_module_path: &ImportPath,
    rule_name: &str,
    rule_value: starlark::values::OwnedFrozenValue,
) -> buck2_error::Result<starlark::values::OwnedFrozenValueTyped<FrozenStarlarkRepositoryRule>> {
    rule_value.downcast_starlark().map_err(|err| {
        let got = err.to_string();
        BazelRepositoryError::RepositoryRuleSymbolWrongType {
            path: rule_module_path.to_string(),
            rule: rule_name.to_owned(),
            got,
        }
        .into()
    })
}

fn eval_bzlmod_tag_expression<'v>(
    expression: &str,
    constants: &[(String, String)],
    value_name: &str,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let mut source = String::new();
    for (name, value) in constants {
        source.push_str(name);
        source.push_str(" = (");
        source.push_str(value);
        source.push_str(")\n");
    }
    source.push_str(&format!("{value_name} = ({expression})"));
    let filename = format!("<bzlmod module extension tag expression {value_name}>");
    let ast = AstModule::parse(&filename, source, &Dialect::AllOptionsInternal)?;
    eval.eval_module(ast, globals)?;
    eval.module()
        .get(value_name)
        .ok_or_else(|| {
            buck2_error::Error::from(BazelRepositoryError::MissingEvaluatedTagExpression(
                value_name.to_owned(),
            ))
        })
        .map_err(Into::into)
}

fn alloc_coerced_attr_value<'v>(
    value: &CoercedAttr,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    match value {
        CoercedAttr::Label(label)
        | CoercedAttr::SourceLabel(label)
        | CoercedAttr::Dep(label)
        | CoercedAttr::ConfigurationDep(label)
        | CoercedAttr::SplitTransitionDep(label) => {
            return Ok(eval
                .heap()
                .alloc(StarlarkProvidersLabel::new(label.clone())));
        }
        CoercedAttr::List(list) => {
            let values = list
                .iter()
                .map(|item| alloc_coerced_attr_value(item, eval))
                .collect::<starlark::Result<Vec<_>>>()?;
            return Ok(eval.heap().alloc(AllocList(values)));
        }
        CoercedAttr::Tuple(tuple) => {
            let values = tuple
                .iter()
                .map(|item| alloc_coerced_attr_value(item, eval))
                .collect::<starlark::Result<Vec<_>>>()?;
            return Ok(eval.heap().alloc(AllocList(values)));
        }
        CoercedAttr::Dict(dict) => {
            let values = dict
                .iter()
                .map(|(key, value)| {
                    Ok((
                        alloc_coerced_attr_value(key, eval)?,
                        alloc_coerced_attr_value(value, eval)?,
                    ))
                })
                .collect::<starlark::Result<Vec<_>>>()?;
            return Ok(eval.heap().alloc(AllocDict(values)));
        }
        CoercedAttr::OneOf(value, _) => return alloc_coerced_attr_value(value, eval),
        CoercedAttr::None => return Ok(Value::new_none()),
        _ => {}
    }
    let json = value
        .to_json(&AttrFmtContext::NO_CONTEXT)
        .map_err(starlark::Error::from)?;
    Ok(eval.heap().alloc(json))
}

fn alloc_attr_value<'v>(
    attr_name: &str,
    attr: &Attribute,
    attr_coercion_ctx: &BuildAttrCoercionContext,
    raw_value: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    match attr
        .coerce(
            attr_name,
            AttrIsConfigurable::No,
            attr_coercion_ctx,
            raw_value,
        )
        .map_err(starlark::Error::from)?
    {
        CoercedValue::Custom(value) => alloc_coerced_attr_value(&value, eval),
        CoercedValue::Default => {
            let default = attr.default().ok_or_else(|| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Tier0,
                    "attribute `{}` selected a default but has no default value",
                    attr_name
                )
            })?;
            alloc_coerced_attr_value(default, eval)
        }
    }
}

fn bzlmod_module_cell_name(
    canonical_repo_name: &str,
    is_root: bool,
    eval: &Evaluator<'_, '_, '_>,
) -> buck2_error::Result<CellName> {
    let cell_resolver = BuildContext::from_context(eval)?
        .cell_info()
        .cell_resolver();
    if is_root {
        return Ok(cell_resolver.root_cell());
    }
    if canonical_repo_name == "bazel_tools" {
        return CellName::unchecked_new("bazel_tools");
    }
    CellName::unchecked_new(&bzlmod_cell_name(canonical_repo_name))
}

fn bzlmod_module_attr_coercion_context(
    module_config: &BzlmodModuleExtensionModuleConfig,
    eval: &Evaluator<'_, '_, '_>,
) -> buck2_error::Result<BuildAttrCoercionContext> {
    let build_context = BuildContext::from_context(eval)?;
    let cell_resolver = build_context.cell_info().cell_resolver().dupe();
    let cell_name = bzlmod_module_cell_name(
        &module_config.canonical_repo_name,
        module_config.is_root,
        eval,
    )?;
    let cell_alias_resolver = if module_config.is_root {
        cell_resolver.root_cell_cell_alias_resolver().dupe()
    } else {
        CellAliasResolver::new_for_non_root_cell(
            cell_name,
            cell_resolver.root_cell_cell_alias_resolver(),
            std::iter::empty::<(NonEmptyCellAlias, NonEmptyCellAlias)>(),
        )?
    };
    Ok(BuildAttrCoercionContext::new_no_package(
        cell_resolver,
        cell_name,
        cell_alias_resolver,
        Arc::new(ConcurrentTargetLabelInterner::default()),
    ))
}

fn bzlmod_current_attr_coercion_context(
    eval: &Evaluator<'_, '_, '_>,
) -> buck2_error::Result<BuildAttrCoercionContext> {
    let build_context = BuildContext::from_context(eval)?;
    Ok(BuildAttrCoercionContext::new_no_package(
        build_context.cell_info().cell_resolver().dupe(),
        build_context.cell_info().name().name(),
        build_context.cell_info().cell_alias_resolver().dupe(),
        Arc::new(ConcurrentTargetLabelInterner::default()),
    ))
}

pub(crate) fn alloc_bzlmod_module_extension_context<'v>(
    extension: &FrozenStarlarkModuleExtension,
    extension_usages_json: &str,
    working_dir: &str,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let config: BzlmodModuleExtensionEvaluationConfig = serde_json::from_str(extension_usages_json)
        .map_err(|e| {
            buck2_error::Error::from(BazelRepositoryError::InvalidModuleExtensionUsageData)
                .context(format!("JSON parse error: {e}"))
        })?;
    let extension_id = extension.id()?;
    let tag_classes = extension.tag_classes();
    let tag_class_names = tag_classes.keys().cloned().collect::<Vec<_>>();

    let mut expression_index = 0usize;
    let mut sort_key = 0i32;
    let mut modules = Vec::new();
    for module_config in config.modules {
        let attr_coercion_ctx = bzlmod_module_attr_coercion_context(&module_config, eval)
            .map_err(starlark::Error::from)?;
        let mut tags = SmallMap::new();
        for tag_class_name in &tag_class_names {
            tags.insert(tag_class_name.clone(), Vec::new());
        }

        for tag_config in module_config.tags {
            let mut expression_bindings = module_config.constants.clone();
            expression_bindings.extend(tag_config.bindings.iter().cloned());
            let tag_class_value = tag_classes.get(&tag_config.tag_name).ok_or_else(|| {
                buck2_error::Error::from(BazelRepositoryError::UnknownModuleExtensionTag {
                    extension: extension_id.to_string(),
                    tag: tag_config.tag_name.clone(),
                })
            })?;
            let tag_class = tag_class_value
                .to_value()
                .downcast_ref::<FrozenStarlarkTagClass>()
                .ok_or_else(|| {
                    buck2_error::Error::from(BazelRepositoryError::InvalidFrozenTagClass {
                        tag: tag_config.tag_name.clone(),
                        got: tag_class_value.to_value().get_type().to_owned(),
                    })
                })?;
            let mut explicit_attrs = tag_config
                .kwargs
                .into_iter()
                .collect::<BTreeMap<String, String>>();
            let mut attrs = SmallMap::new();
            for (attr_name, attr) in tag_class.attributes() {
                let value = match explicit_attrs.remove(attr_name) {
                    Some(expression) => {
                        let value_name = format!("buck_bzlmod_tag_value_{expression_index}");
                        expression_index += 1;
                        let raw_value = eval_bzlmod_tag_expression(
                            &expression,
                            &expression_bindings,
                            &value_name,
                            globals,
                            eval,
                        )?;
                        alloc_attr_value(attr_name, attr, &attr_coercion_ctx, raw_value, eval)?
                    }
                    None => match attr.default() {
                        Some(default) => alloc_coerced_attr_value(default, eval)?,
                        None => {
                            return Err(buck2_error::Error::from(
                                BazelRepositoryError::MissingModuleExtensionTagAttribute {
                                    tag: tag_config.tag_name.clone(),
                                    attr: attr_name.clone(),
                                },
                            )
                            .into());
                        }
                    },
                };
                attrs.insert(attr_name.clone(), value);
            }
            if let Some((attr, _)) = explicit_attrs.into_iter().next() {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "module extension tag `{}` has unknown attribute `{}`",
                    tag_config.tag_name,
                    attr
                )
                .into());
            }

            let tag_value = eval.heap().alloc(StarlarkBazelModuleTag::new(
                tag_config.tag_name.clone(),
                tag_config.dev_dependency,
                sort_key,
                attrs,
            ));
            sort_key += 1;
            tags.entry(tag_config.tag_name)
                .or_insert_with(Vec::new)
                .push(tag_value);
        }

        let tags = tags
            .into_iter()
            .map(|(name, values)| (name, eval.heap().alloc(AllocList(values))))
            .collect();
        let tags_value = eval.heap().alloc(StarlarkBazelModuleTags::new(tags));
        let module_value = eval.heap().alloc(StarlarkBazelModule::new(
            module_config.name,
            module_config.version,
            tags_value,
            module_config.is_root,
        ));
        modules.push(module_value);
    }
    let modules = eval.heap().alloc(AllocList(modules));

    Ok(eval.heap().alloc(StarlarkModuleExtensionContext::new(
        modules,
        working_dir.to_owned(),
        config.root_module_has_non_dev_dependency,
    )))
}

pub(crate) fn alloc_bzlmod_repository_context<'v>(
    repository_rule: &FrozenStarlarkRepositoryRule,
    invocation: &BazelRepositoryRuleInvocation,
    working_dir: &str,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let mut expression_index = 0usize;
    let mut explicit_attrs = invocation
        .attrs
        .iter()
        .cloned()
        .collect::<BTreeMap<String, String>>();
    let attr_coercion_ctx =
        bzlmod_current_attr_coercion_context(eval).map_err(starlark::Error::from)?;
    let mut attrs = Vec::new();
    for (attr_name, attr) in repository_rule.attributes.attributes() {
        let value = match explicit_attrs.remove(attr_name) {
            Some(expression) => {
                let value_name = format!("buck_repository_rule_attr_{expression_index}");
                expression_index += 1;
                let raw_value =
                    eval_bzlmod_tag_expression(&expression, &[], &value_name, globals, eval)?;
                alloc_attr_value(attr_name, attr, &attr_coercion_ctx, raw_value, eval)?
            }
            None => match attr.default() {
                Some(default) => alloc_coerced_attr_value(default, eval)?,
                None => {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "repository_rule `{}` invocation `{}` is missing required attribute `{}`",
                        invocation.rule_id,
                        invocation.name,
                        attr_name
                    )
                    .into());
                }
            },
        };
        attrs.push((attr_name.as_str(), value));
    }
    if let Some((attr, _)) = explicit_attrs.into_iter().next() {
        return Err(buck2_error::Error::from(
            BazelRepositoryError::RepositoryRuleUnknownAttribute {
                rule: invocation.rule_id.to_string(),
                attr,
            },
        )
        .into());
    }
    attrs.push((
        NAME_ATTRIBUTE_FIELD,
        eval.heap().alloc_str(&invocation.name).to_value(),
    ));
    let attr = eval.heap().alloc(AllocStruct(attrs));
    Ok(eval.heap().alloc(StarlarkRepositoryContext::new(
        invocation.name.clone(),
        invocation.original_name.clone(),
        attr,
        working_dir.to_owned(),
        repository_ctx_workspace_root(working_dir),
    )))
}

fn repository_ctx_workspace_root(working_dir: &str) -> String {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut components = Path::new(working_dir).components();
    if let (Some(std::path::Component::Normal(first)), Some(std::path::Component::Normal(second))) =
        (components.next(), components.next())
    {
        let isolation_root = Path::new(first).join(second);
        if cwd.ends_with(&isolation_root)
            && let Some(root) = cwd.parent().and_then(|path| path.parent())
        {
            return root.to_string_lossy().into_owned();
        }
    }
    cwd.to_string_lossy().into_owned()
}

#[derive(Debug, Allocative)]
struct BazelAttributeSpec {
    attributes: SmallMap<String, Attribute>,
}

impl BazelAttributeSpec {
    fn from_entries<'v>(
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>>,
        allow_name: bool,
    ) -> buck2_error::Result<Self> {
        let attrs = attrs.unwrap_or_default();
        let attributes = attrs
            .entries
            .into_iter()
            .sorted_by(|(k1, _), (k2, _)| Ord::cmp(k1, k2))
            .map(|(name, value)| {
                if !allow_name && name == NAME_ATTRIBUTE_FIELD {
                    Err(BazelRepositoryError::InvalidRepositoryRuleAttributeName(
                        NAME_ATTRIBUTE_FIELD.to_owned(),
                    )
                    .into())
                } else {
                    Ok((name.to_owned(), value.clone_attribute()))
                }
            })
            .collect::<buck2_error::Result<SmallMap<_, _>>>()?;
        Ok(Self { attributes })
    }

    fn documentation(&self, name: &str, docs: Option<&str>, ret: Ty) -> DocItem {
        let parameters_spec = ParametersSpec::new_named_only(
            name,
            self.attributes.iter().map(|(name, attribute)| {
                (
                    name.as_str(),
                    match attribute.default() {
                        Some(_) => ParametersSpecParam::<FrozenValue>::Optional,
                        None => ParametersSpecParam::<FrozenValue>::Required,
                    },
                )
            }),
        );
        let params = parameters_spec.documentation_with_default_value_formatter(
            vec![Ty::any(); self.attributes.len()],
            HashMap::new(),
            |_v| "<default>".to_owned(),
        );

        DocItem::Member(DocMember::Function(DocFunction::from_docstring(
            DocStringKind::Starlark,
            params,
            ret,
            docs,
        )))
    }

    #[allow(dead_code)]
    pub(crate) fn attributes(&self) -> &SmallMap<String, Attribute> {
        &self.attributes
    }
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkRepositoryRule<'v> {
    rule_path: BzlOrBxlPath,
    id: RefCell<Option<StarlarkRuleType>>,
    implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
    #[trace(unsafe_ignore)]
    attributes: BazelAttributeSpec,
    local: bool,
    configure: bool,
    remotable: bool,
    environ: Vec<String>,
    docs: Option<String>,
    ty: Ty,
}

impl<'v> StarlarkRepositoryRule<'v> {
    fn new(
        implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>>,
        local: bool,
        configure: bool,
        remotable: bool,
        environ: UnpackListOrTuple<String>,
        doc: NoneOr<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<Self> {
        let attributes = BazelAttributeSpec::from_entries(attrs, false)?;
        let ty = Ty::function(ParamSpec::kwargs(Ty::any()), Ty::none());
        Ok(Self {
            rule_path: current_bzl_path(eval, "repository_rule")?,
            id: RefCell::new(None),
            implementation,
            attributes,
            local,
            configure,
            remotable,
            environ: environ.items,
            docs: doc_string(doc),
            ty,
        })
    }

    fn name_for_docs(&self) -> String {
        self.id
            .borrow()
            .as_ref()
            .map_or_else(|| "repository_rule".to_owned(), |id| id.name.clone())
    }
}

impl<'v> Display for StarlarkRepositoryRule<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.id.borrow() {
            Some(id) => write!(f, "<starlark repository rule {}>", id.name),
            None => write!(f, "<anonymous starlark repository rule>"),
        }
    }
}

impl<'v> AllocValue<'v> for StarlarkRepositoryRule<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "repository_rule")]
impl<'v> StarlarkValue<'v> for StarlarkRepositoryRule<'v> {
    fn export_as(
        &self,
        variable_name: &str,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<()> {
        *self.id.borrow_mut() = Some(StarlarkRuleType {
            path: self.rule_path.clone(),
            name: variable_name.to_owned(),
        });
        Ok(())
    }

    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let id = self.id.borrow();
        let Some(id) = id.as_ref() else {
            return Err(
                buck2_error::Error::from(BazelRepositoryError::RepositoryRuleNotExported).into(),
            );
        };
        record_repository_rule_invocation(id, args, eval)
    }

    fn documentation(&self) -> DocItem {
        self.attributes
            .documentation(&self.name_for_docs(), self.docs.as_deref(), Ty::none())
    }

    fn typechecker_ty(&self) -> Option<Ty> {
        Some(self.ty.clone())
    }

    fn get_type_starlark_repr() -> Ty {
        Ty::function(ParamSpec::kwargs(Ty::any()), Ty::none())
    }
}

impl<'v> Freeze for StarlarkRepositoryRule<'v> {
    type Frozen = FrozenStarlarkRepositoryRule;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkRepositoryRule {
            id: self.id.into_inner().map(Arc::new),
            implementation: self.implementation.0.freeze(freezer)?,
            attributes: self.attributes,
            local: self.local,
            configure: self.configure,
            remotable: self.remotable,
            environ: self.environ,
            docs: self.docs,
            ty: self.ty,
        })
    }
}

#[derive(Debug, Display, ProvidesStaticType, NoSerialize, Allocative)]
#[display("{}", self.display())]
pub(crate) struct FrozenStarlarkRepositoryRule {
    id: Option<Arc<StarlarkRuleType>>,
    #[allow(dead_code)]
    implementation: FrozenValue,
    attributes: BazelAttributeSpec,
    #[allow(dead_code)]
    local: bool,
    #[allow(dead_code)]
    configure: bool,
    #[allow(dead_code)]
    remotable: bool,
    #[allow(dead_code)]
    environ: Vec<String>,
    docs: Option<String>,
    ty: Ty,
}

impl FrozenStarlarkRepositoryRule {
    fn display(&self) -> String {
        match &self.id {
            Some(id) => format!("<starlark repository rule {}>", id.name),
            None => "<anonymous starlark repository rule>".to_owned(),
        }
    }

    fn name_for_docs(&self) -> String {
        self.id
            .as_ref()
            .map_or_else(|| "repository_rule".to_owned(), |id| id.name.clone())
    }

    pub(crate) fn invoke_implementation<'v>(
        &self,
        repository_ctx: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        eval.eval_function(self.implementation.to_value(), &[repository_ctx], &[])
    }
}

starlark_simple_value!(FrozenStarlarkRepositoryRule);

#[starlark_value(type = "repository_rule")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkRepositoryRule {
    type Canonical = StarlarkRepositoryRule<'v>;

    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let Some(id) = &self.id else {
            return Err(
                buck2_error::Error::from(BazelRepositoryError::RepositoryRuleNotExported).into(),
            );
        };
        record_repository_rule_invocation(id, args, eval)
    }

    fn documentation(&self) -> DocItem {
        self.attributes
            .documentation(&self.name_for_docs(), self.docs.as_deref(), Ty::none())
    }

    fn typechecker_ty(&self) -> Option<Ty> {
        Some(self.ty.clone())
    }

    fn get_type_starlark_repr() -> Ty {
        StarlarkRepositoryRule::get_type_starlark_repr()
    }
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkTagClass {
    #[trace(unsafe_ignore)]
    attributes: BazelAttributeSpec,
    docs: Option<String>,
}

impl<'v> StarlarkTagClass {
    fn new(
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>>,
        doc: NoneOr<&str>,
    ) -> buck2_error::Result<Self> {
        Ok(Self {
            attributes: BazelAttributeSpec::from_entries(attrs, true)?,
            docs: doc_string(doc),
        })
    }
}

impl Display for StarlarkTagClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<tag_class>")
    }
}

impl<'v> AllocValue<'v> for StarlarkTagClass {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "tag_class")]
impl<'v> StarlarkValue<'v> for StarlarkTagClass {
    fn documentation(&self) -> DocItem {
        self.attributes
            .documentation("tag_class", self.docs.as_deref(), Ty::any())
    }
}

impl Freeze for StarlarkTagClass {
    type Frozen = FrozenStarlarkTagClass;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkTagClass {
            attributes: self.attributes,
            docs: self.docs,
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkTagClass {
    #[allow(dead_code)]
    attributes: BazelAttributeSpec,
    #[allow(dead_code)]
    docs: Option<String>,
}

impl FrozenStarlarkTagClass {
    #[allow(dead_code)]
    pub(crate) fn attributes(&self) -> &SmallMap<String, Attribute> {
        self.attributes.attributes()
    }
}

impl Display for FrozenStarlarkTagClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<tag_class>")
    }
}

starlark_simple_value!(FrozenStarlarkTagClass);

#[starlark_value(type = "tag_class")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkTagClass {
    type Canonical = StarlarkTagClass;
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkModuleExtension<'v> {
    extension_path: BzlOrBxlPath,
    id: RefCell<Option<StarlarkRuleType>>,
    implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
    tag_classes: SmallMap<String, Value<'v>>,
    docs: Option<String>,
    environ: Vec<String>,
    os_dependent: bool,
    arch_dependent: bool,
}

impl<'v> StarlarkModuleExtension<'v> {
    fn new(
        implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
        tag_classes: SmallMap<String, Value<'v>>,
        doc: NoneOr<&str>,
        environ: UnpackListOrTuple<String>,
        os_dependent: bool,
        arch_dependent: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<Self> {
        for (name, value) in &tag_classes {
            if ValueTypedComplex::<StarlarkTagClass>::new(*value).is_none() {
                return Err(BazelRepositoryError::InvalidTagClass(
                    name.to_owned(),
                    value.get_type().to_owned(),
                )
                .into());
            }
        }
        Ok(Self {
            extension_path: current_bzl_path(eval, "module_extension")?,
            id: RefCell::new(None),
            implementation,
            tag_classes,
            docs: doc_string(doc),
            environ: environ.items,
            os_dependent,
            arch_dependent,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn invoke_implementation(
        &self,
        module_ctx: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let id = self.id.borrow();
        let Some(id) = id.as_ref() else {
            return Err(
                buck2_error::Error::from(BazelRepositoryError::ModuleExtensionNotExported).into(),
            );
        };
        let result = eval.eval_function(self.implementation.0, &[module_ctx], &[])?;
        validate_module_extension_return(id, result)
    }
}

impl<'v> Display for StarlarkModuleExtension<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.id.borrow() {
            Some(id) => write!(f, "<module_extension {}>", id.name),
            None => write!(f, "<anonymous module_extension>"),
        }
    }
}

impl<'v> AllocValue<'v> for StarlarkModuleExtension<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "module_extension")]
impl<'v> StarlarkValue<'v> for StarlarkModuleExtension<'v> {
    fn export_as(
        &self,
        variable_name: &str,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<()> {
        *self.id.borrow_mut() = Some(StarlarkRuleType {
            path: self.extension_path.clone(),
            name: variable_name.to_owned(),
        });
        Ok(())
    }
}

impl<'v> Freeze for StarlarkModuleExtension<'v> {
    type Frozen = FrozenStarlarkModuleExtension;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let tag_classes = self
            .tag_classes
            .into_iter()
            .map(|(name, value)| Ok((name, value.freeze(freezer)?)))
            .collect::<FreezeResult<SmallMap<String, FrozenValue>>>()?;
        Ok(FrozenStarlarkModuleExtension {
            id: self.id.into_inner().map(Arc::new),
            implementation: self.implementation.0.freeze(freezer)?,
            tag_classes,
            docs: self.docs,
            environ: self.environ,
            os_dependent: self.os_dependent,
            arch_dependent: self.arch_dependent,
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkModuleExtension {
    #[allow(dead_code)]
    id: Option<Arc<StarlarkRuleType>>,
    #[allow(dead_code)]
    implementation: FrozenValue,
    #[allow(dead_code)]
    tag_classes: SmallMap<String, FrozenValue>,
    #[allow(dead_code)]
    docs: Option<String>,
    #[allow(dead_code)]
    environ: Vec<String>,
    #[allow(dead_code)]
    os_dependent: bool,
    #[allow(dead_code)]
    arch_dependent: bool,
}

impl Display for FrozenStarlarkModuleExtension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.id {
            Some(id) => write!(f, "<module_extension {}>", id.name),
            None => write!(f, "<anonymous module_extension>"),
        }
    }
}

impl FrozenStarlarkModuleExtension {
    #[allow(dead_code)]
    pub(crate) fn id(&self) -> buck2_error::Result<&StarlarkRuleType> {
        self.id
            .as_deref()
            .ok_or_else(|| BazelRepositoryError::ModuleExtensionNotExported.into())
    }

    #[allow(dead_code)]
    pub(crate) fn tag_classes(&self) -> &SmallMap<String, FrozenValue> {
        &self.tag_classes
    }

    #[allow(dead_code)]
    pub(crate) fn invoke_implementation<'v>(
        &self,
        module_ctx: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let id = self.id()?;
        let result = eval.eval_function(self.implementation.to_value(), &[module_ctx], &[])?;
        validate_module_extension_return(id, result)
    }
}

starlark_simple_value!(FrozenStarlarkModuleExtension);

#[starlark_value(type = "module_extension")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkModuleExtension {
    type Canonical = StarlarkModuleExtension<'v>;
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkRepositoryOs;

impl Display for StarlarkRepositoryOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository_os>")
    }
}

impl<'v> AllocValue<'v> for StarlarkRepositoryOs {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "repository_os")]
impl<'v> StarlarkValue<'v> for StarlarkRepositoryOs {
    fn dir_attr(&self) -> Vec<String> {
        vec!["arch".to_owned(), "environ".to_owned(), "name".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "arch" => Some(heap.alloc(env::consts::ARCH)),
            "environ" => Some(host_environ(heap)),
            "name" => Some(heap.alloc(bazel_host_os_name())),
            _ => None,
        }
    }
}

impl Freeze for StarlarkRepositoryOs {
    type Frozen = FrozenStarlarkRepositoryOs;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkRepositoryOs)
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkRepositoryOs;

impl Display for FrozenStarlarkRepositoryOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository_os>")
    }
}

starlark_simple_value!(FrozenStarlarkRepositoryOs);

#[starlark_value(type = "repository_os")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkRepositoryOs {
    type Canonical = StarlarkRepositoryOs;

    fn dir_attr(&self) -> Vec<String> {
        vec!["arch".to_owned(), "environ".to_owned(), "name".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "arch" => Some(heap.alloc(env::consts::ARCH)),
            "environ" => Some(host_environ(heap)),
            "name" => Some(heap.alloc(bazel_host_os_name())),
            _ => None,
        }
    }
}

#[derive(
    Clone,
    Debug,
    ProvidesStaticType,
    Trace,
    Freeze,
    NoSerialize,
    Allocative
)]
pub(crate) struct StarlarkRepositoryPath {
    path: String,
    #[trace(unsafe_ignore)]
    dep: Option<RepositoryPathLabelDep>,
}

starlark_simple_value!(StarlarkRepositoryPath);

impl StarlarkRepositoryPath {
    fn new(path: String) -> Self {
        Self { path, dep: None }
    }

    fn new_with_dep(path: String, dep: Option<RepositoryPathLabelDep>) -> Self {
        Self { path, dep }
    }
}

impl Display for StarlarkRepositoryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.dep.is_some() {
            repository_path_for_read_abs(&self.path)
                .to_string_lossy()
                .fmt(f)
        } else {
            self.path.fmt(f)
        }
    }
}

#[starlark_value(type = "repository_path")]
impl<'v> StarlarkValue<'v> for StarlarkRepositoryPath {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(repository_path_methods)
    }
}

#[starlark_module]
fn repository_path_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn basename(this: &StarlarkRepositoryPath) -> starlark::Result<String> {
        Ok(Path::new(&this.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default())
    }

    #[starlark(attribute)]
    fn dirname(this: &StarlarkRepositoryPath) -> starlark::Result<StarlarkRepositoryPath> {
        let path = Path::new(&this.path)
            .parent()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dep = repository_ctx_external_input_tree_dep(Path::new(&path));
        Ok(StarlarkRepositoryPath::new_with_dep(path, dep))
    }

    fn get_child<'v>(
        this: &StarlarkRepositoryPath,
        args: &Arguments<'v, '_>,
        heap: Heap<'v>,
    ) -> starlark::Result<StarlarkRepositoryPath> {
        args.no_named_args()?;
        let mut path = PathBuf::from(&this.path);
        for child in args.positions(heap)? {
            let Some(child) = child.unpack_str() else {
                return Err(buck2_error::Error::from(
                    BazelRepositoryError::RepositoryPathGetChildNonString(
                        child.get_type().to_owned(),
                    ),
                )
                .into());
            };
            path.push(child);
        }
        Ok(StarlarkRepositoryPath::new(
            path.to_string_lossy().into_owned(),
        ))
    }

    #[starlark(attribute)]
    fn exists(this: &StarlarkRepositoryPath) -> starlark::Result<bool> {
        Ok(Path::new(&repository_path_for_read(&this.path)).exists())
    }

    #[starlark(attribute)]
    fn is_dir(this: &StarlarkRepositoryPath) -> starlark::Result<bool> {
        Ok(Path::new(&repository_path_for_read(&this.path)).is_dir())
    }

    #[starlark(attribute)]
    fn realpath(this: &StarlarkRepositoryPath) -> starlark::Result<StarlarkRepositoryPath> {
        let read_path = repository_path_for_read(&this.path);
        let path = fs::canonicalize(&read_path).map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryPathRealpath {
                path: this.path.clone(),
                error: error.to_string(),
            })
        })?;
        Ok(StarlarkRepositoryPath::new(
            path.to_string_lossy().into_owned(),
        ))
    }

    fn readdir(
        this: &StarlarkRepositoryPath,
        #[starlark(require = named, default = "auto")] watch: &str,
    ) -> starlark::Result<Vec<StarlarkRepositoryPath>> {
        let _unused = watch;
        let read_path = repository_path_for_read(&this.path);
        let entries = fs::read_dir(&read_path).map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryPathReaddir {
                path: this.path.clone(),
                error: error.to_string(),
            })
        })?;
        let mut paths = entries
            .map(|entry| {
                let entry = entry.map_err(|error| {
                    buck2_error::Error::from(BazelRepositoryError::RepositoryPathReaddir {
                        path: this.path.clone(),
                        error: error.to_string(),
                    })
                })?;
                let path = Path::new(&this.path).join(entry.file_name());
                Ok(StarlarkRepositoryPath::new(
                    path.to_string_lossy().into_owned(),
                ))
            })
            .collect::<starlark::Result<Vec<_>>>()?;
        paths.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(paths)
    }
}

fn repository_path_from_value_relative_to(
    value: Value<'_>,
    eval: &Evaluator<'_, '_, '_>,
    relative_root: Option<&str>,
) -> starlark::Result<String> {
    Ok(repository_path_and_dep_from_value_relative_to(value, eval, relative_root)?.0)
}

fn repository_path_and_dep_from_value_relative_to(
    value: Value<'_>,
    eval: &Evaluator<'_, '_, '_>,
    relative_root: Option<&str>,
) -> starlark::Result<(String, Option<RepositoryPathLabelDep>)> {
    if let Some(path) = value.downcast_ref::<StarlarkRepositoryPath>() {
        return Ok((path.path.clone(), path.dep.clone()));
    }
    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
        let target = label.label().target();
        let cell_path = target
            .pkg()
            .to_cell_path()
            .join_normalized(target.name().as_str())?;
        let project_path = BuildContext::from_context(eval)?
            .cell_resolver()
            .resolve_path(cell_path.as_ref())?;
        return Ok((
            project_path.as_str().to_owned(),
            Some(RepositoryPathLabelDep::cell_path(
                cell_path.cell().as_str().to_owned(),
                cell_path.path().as_str().to_owned(),
            )),
        ));
    }
    if let Some(path) = value.unpack_str() {
        if let Some(relative_root) = relative_root
            && !Path::new(path).is_absolute()
            && !path.starts_with("buck-out/")
        {
            return Ok((repository_join_normalized(relative_root, path), None));
        }
        return Ok((path.to_owned(), None));
    }
    Err(
        buck2_error::Error::from(BazelRepositoryError::ModuleCtxPathUnsupportedValue(
            value.get_type().to_owned(),
        ))
        .into(),
    )
}

fn repository_join_normalized(root: &str, path: &str) -> String {
    let mut joined = PathBuf::from(root);
    for component in Path::new(path).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                joined.pop();
            }
            std::path::Component::Normal(part) => joined.push(part),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                joined.push(component.as_os_str());
            }
        }
    }
    joined.to_string_lossy().into_owned()
}

fn repository_path_for_read(path: &str) -> String {
    if Path::new(path).exists() {
        return path.to_owned();
    }

    let Some(suffix) = path.strip_prefix("buck-out/v2/external_cells/") else {
        return repository_project_relative_path_for_read(path).unwrap_or_else(|| path.to_owned());
    };

    for root in repository_read_roots() {
        let candidate = root.join(path);
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }

        if let Some(candidate) = repository_path_for_extracted_external_cell(&root, suffix) {
            return candidate;
        }

        let Ok(entries) = fs::read_dir(root.join("buck-out")) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join("external_cells").join(suffix);
            if candidate.exists() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }

    path.to_owned()
}

fn repository_path_for_read_abs(path: &str) -> PathBuf {
    let path = repository_path_for_read(path);
    let path_buf = PathBuf::from(&path);
    if path_buf.is_absolute() {
        return path_buf;
    }
    repository_path_for_write(&path).unwrap_or(path_buf)
}

fn repository_path_for_read_abs_relative_to(path: &str, working_dir: &str) -> PathBuf {
    if let Some(suffix) = repository_external_cell_suffix(path)
        && let Some(candidate) =
            repository_external_cell_existing_path_relative_to(suffix, working_dir)
    {
        return candidate;
    }
    repository_path_for_read_abs(path)
}

fn repository_external_cell_suffix(path: &str) -> Option<&str> {
    let buck_out_relative = path
        .strip_prefix("buck-out/")
        .or_else(|| path.split_once("/buck-out/").map(|(_, suffix)| suffix))?;
    let (_, suffix) = buck_out_relative.split_once("/external_cells/")?;
    (!suffix.is_empty()).then_some(suffix)
}

fn repository_external_cell_path_relative_to(suffix: &str, working_dir: &str) -> Option<PathBuf> {
    let (buck_out_root, _) = working_dir.split_once("/external_cells/")?;
    let candidate = format!("{buck_out_root}/external_cells/{suffix}");
    Some(repository_path_for_write(&candidate).unwrap_or_else(|_| PathBuf::from(candidate)))
}

fn repository_external_cell_existing_path_relative_to(
    suffix: &str,
    working_dir: &str,
) -> Option<PathBuf> {
    let candidate = repository_external_cell_path_relative_to(suffix, working_dir)?;
    if candidate.exists() {
        return Some(candidate);
    }
    let candidate = repository_external_cell_repository_ctx_path_relative_to(suffix, working_dir)?;
    candidate.exists().then_some(candidate)
}

fn repository_external_cell_repository_ctx_path_relative_to(
    suffix: &str,
    working_dir: &str,
) -> Option<PathBuf> {
    let generated_suffix = suffix.strip_prefix("bzlmod_generated/")?;
    let (repo_name, repo_path) = generated_suffix
        .split_once('/')
        .unwrap_or((generated_suffix, ""));
    if repo_name.ends_with(".repository_ctx") {
        return None;
    }
    let source_suffix = if repo_path.is_empty() {
        format!("bzlmod_generated/{repo_name}.repository_ctx")
    } else {
        format!("bzlmod_generated/{repo_name}.repository_ctx/{repo_path}")
    };
    repository_external_cell_path_relative_to(&source_suffix, working_dir)
}

fn repository_project_relative_path_for_read(path: &str) -> Option<String> {
    for root in repository_read_roots() {
        let candidate = root.join(path);
        if candidate.exists() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn repository_path_for_extracted_external_cell(root: &Path, suffix: &str) -> Option<String> {
    let mut parts = suffix.splitn(3, '/');
    let cell_kind = parts.next()?;
    let cell_name = parts.next()?;
    let cell_path = parts.next()?;
    let candidate = root
        .join("buck-out/v2/external_cells")
        .join(cell_kind)
        .join(cell_name)
        .join("extract-tmp")
        .join(cell_path);
    if candidate.exists() {
        Some(candidate.to_string_lossy().into_owned())
    } else {
        None
    }
}

fn repository_read_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(pwd) = env::var_os("PWD") {
        push_repository_read_roots(&mut roots, PathBuf::from(pwd));
    }
    if let Ok(cwd) = env::current_dir() {
        push_repository_read_roots(&mut roots, cwd);
    }
    roots
}

fn push_repository_read_roots(roots: &mut Vec<PathBuf>, path: PathBuf) {
    for ancestor in path.ancestors() {
        if ancestor.join(".buckconfig").exists()
            || ancestor.join("MODULE.bazel").exists()
            || ancestor.join("WORKSPACE.bazel").exists()
            || ancestor.join("WORKSPACE").exists()
        {
            push_unique_repository_read_root(roots, ancestor.to_owned());
        }
    }
    push_unique_repository_read_root(roots, path);
}

fn push_unique_repository_read_root(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !roots.iter().any(|existing| existing == &root) {
        roots.push(root);
    }
}

fn repository_path_for_write(path: &str) -> buck2_error::Result<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    let root = match repository_read_roots().into_iter().next() {
        Some(root) => root,
        None => env::current_dir().map_err(|e| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "could not resolve repository write root: {}",
                e
            )
        })?,
    };
    Ok(root.join(path))
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkRepositoryContext<'v> {
    name: String,
    original_name: String,
    attr: Value<'v>,
    working_dir: String,
    workspace_root: String,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    files: Mutex<Vec<BazelRepositoryGeneratedFile>>,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    path_label_deps: Mutex<Vec<RepositoryPathLabelDep>>,
}

impl<'v> StarlarkRepositoryContext<'v> {
    fn new(
        name: String,
        original_name: String,
        attr: Value<'v>,
        working_dir: String,
        workspace_root: String,
    ) -> Self {
        Self {
            name,
            original_name,
            attr,
            working_dir,
            workspace_root,
            files: Mutex::new(Vec::new()),
            path_label_deps: Mutex::new(Vec::new()),
        }
    }

    fn take_files(&self) -> Vec<BazelRepositoryGeneratedFile> {
        std::mem::take(&mut *self.files.lock().expect("repository_ctx files poisoned"))
    }

    fn take_path_label_deps(&self) -> Vec<RepositoryPathLabelDep> {
        std::mem::take(
            &mut *self
                .path_label_deps
                .lock()
                .expect("repository_ctx path label deps poisoned"),
        )
    }
}

impl<'v> Display for StarlarkRepositoryContext<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository_ctx {}>", self.name)
    }
}

impl<'v> AllocValue<'v> for StarlarkRepositoryContext<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "repository_ctx")]
impl<'v> StarlarkValue<'v> for StarlarkRepositoryContext<'v> {
    fn dir_attr(&self) -> Vec<String> {
        vec![
            "attr".to_owned(),
            "name".to_owned(),
            "original_name".to_owned(),
            "os".to_owned(),
            "workspace_root".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "attr" => Some(self.attr),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "original_name" => Some(heap.alloc_str(&self.original_name).to_value()),
            "os" => Some(heap.alloc(StarlarkRepositoryOs)),
            "workspace_root" => {
                Some(heap.alloc(StarlarkRepositoryPath::new(self.workspace_root.clone())))
            }
            _ => None,
        }
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self>(repository_context_methods)
    }
}

impl<'v> Freeze for StarlarkRepositoryContext<'v> {
    type Frozen = FrozenStarlarkRepositoryContext;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkRepositoryContext {
            name: self.name,
            original_name: self.original_name,
            attr: self.attr.freeze(freezer)?,
            working_dir: self.working_dir,
            workspace_root: self.workspace_root,
            files: Mutex::new(
                self.files
                    .into_inner()
                    .expect("repository_ctx files poisoned"),
            ),
            path_label_deps: Mutex::new(
                self.path_label_deps
                    .into_inner()
                    .expect("repository_ctx path label deps poisoned"),
            ),
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkRepositoryContext {
    name: String,
    original_name: String,
    attr: FrozenValue,
    working_dir: String,
    workspace_root: String,
    #[allocative(skip)]
    files: Mutex<Vec<BazelRepositoryGeneratedFile>>,
    #[allocative(skip)]
    path_label_deps: Mutex<Vec<RepositoryPathLabelDep>>,
}

impl Display for FrozenStarlarkRepositoryContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository_ctx {}>", self.name)
    }
}

starlark_simple_value!(FrozenStarlarkRepositoryContext);

#[starlark_value(type = "repository_ctx")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkRepositoryContext {
    type Canonical = StarlarkRepositoryContext<'v>;

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "attr".to_owned(),
            "name".to_owned(),
            "original_name".to_owned(),
            "os".to_owned(),
            "workspace_root".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "attr" => Some(self.attr.to_value()),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "original_name" => Some(heap.alloc_str(&self.original_name).to_value()),
            "os" => Some(heap.alloc(FrozenStarlarkRepositoryOs)),
            "workspace_root" => {
                Some(heap.alloc(StarlarkRepositoryPath::new(self.workspace_root.clone())))
            }
            _ => None,
        }
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(repository_context_methods)
    }
}

fn repository_ctx_output_path_from_value(
    value: Value<'_>,
    working_dir: &str,
) -> starlark::Result<String> {
    if let Some(path) = value.downcast_ref::<StarlarkRepositoryPath>() {
        let prefix = format!("{working_dir}/");
        return Ok(path
            .path
            .strip_prefix(&prefix)
            .unwrap_or(&path.path)
            .to_owned());
    }
    if let Some(path) = value.unpack_str() {
        return Ok(path.to_owned());
    }
    Err(buck2_error::Error::from(
        BazelRepositoryError::RepositoryCtxOutputPathUnsupportedValue(value.get_type().to_owned()),
    )
    .into())
}

pub(crate) fn take_repository_ctx_files<'v>(
    repository_ctx: Value<'v>,
) -> starlark::Result<Vec<BazelRepositoryGeneratedFile>> {
    let repository_ctx = repository_ctx
        .downcast_ref::<StarlarkRepositoryContext<'v>>()
        .ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "expected repository_ctx, got `{}`",
                repository_ctx.get_type()
            )
        })?;
    repository_ctx_current_generated_files(&repository_ctx.working_dir, repository_ctx.take_files())
}

fn repository_ctx_current_generated_files(
    working_dir: &str,
    files: Vec<BazelRepositoryGeneratedFile>,
) -> starlark::Result<Vec<BazelRepositoryGeneratedFile>> {
    let mut seen = BTreeSet::new();
    let mut refreshed = Vec::new();
    for file in files.into_iter().rev() {
        if !seen.insert(file.path.clone()) {
            continue;
        }
        let path = Path::new(working_dir).join(&file.path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Tier0,
                    "repository_ctx could not stat generated file `{}`: {}",
                    path.to_string_lossy(),
                    error
                )
                .into());
            }
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let content = match fs::read(&path) {
            Ok(content) => content,
            Err(error) => {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Tier0,
                    "repository_ctx could not read generated file `{}`: {}",
                    path.to_string_lossy(),
                    error
                )
                .into());
            }
        };
        let Ok(content) = String::from_utf8(content) else {
            continue;
        };
        refreshed.push(BazelRepositoryGeneratedFile {
            path: file.path,
            content,
            executable: repository_path_is_executable(&path),
        });
    }
    refreshed.reverse();
    Ok(refreshed)
}

pub(crate) fn take_repository_ctx_path_label_deps<'v>(
    repository_ctx: Value<'v>,
) -> starlark::Result<Vec<RepositoryPathLabelDep>> {
    let repository_ctx = repository_ctx
        .downcast_ref::<StarlarkRepositoryContext>()
        .ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "expected repository_ctx, got `{}`",
                repository_ctx.get_type()
            )
        })?;
    Ok(repository_ctx.take_path_label_deps())
}

pub(crate) fn take_module_ctx_path_label_deps<'v>(
    module_ctx: Value<'v>,
) -> starlark::Result<Vec<RepositoryPathLabelDep>> {
    let module_ctx = module_ctx
        .downcast_ref::<StarlarkModuleExtensionContext>()
        .ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "expected module_ctx, got `{}`",
                module_ctx.get_type()
            )
        })?;
    Ok(module_ctx.take_path_label_deps())
}

fn repository_ctx_working_dir<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
) -> &'v str {
    match this.unpack() {
        either::Either::Left(ctx) => &ctx.working_dir,
        either::Either::Right(ctx) => &ctx.working_dir,
    }
}

fn repository_ctx_record_path_dep<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
    dep: RepositoryPathLabelDep,
) {
    match this.unpack() {
        either::Either::Left(ctx) => {
            let mut deps = ctx
                .path_label_deps
                .lock()
                .expect("repository_ctx path label deps poisoned");
            if !deps.iter().any(|existing| existing == &dep) {
                deps.push(dep);
            }
        }
        either::Either::Right(ctx) => {
            let mut deps = ctx
                .path_label_deps
                .lock()
                .expect("repository_ctx path label deps poisoned");
            if !deps.iter().any(|existing| existing == &dep) {
                deps.push(dep);
            }
        }
    }
}

fn repository_ctx_path_from_value_relative_to<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
    path: Value<'v>,
    eval: &Evaluator<'v, '_, '_>,
) -> starlark::Result<String> {
    let (path, dep) = repository_path_and_dep_from_value_relative_to(
        path,
        eval,
        Some(repository_ctx_working_dir(this)),
    )?;
    if let Some(dep) = dep {
        repository_ctx_record_path_dep(this, dep);
    }
    Ok(path)
}

fn module_ctx_record_path_dep<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
    dep: RepositoryPathLabelDep,
) {
    match this.unpack() {
        either::Either::Left(ctx) => {
            let mut deps = ctx
                .path_label_deps
                .lock()
                .expect("module_ctx path label deps poisoned");
            if !deps.iter().any(|existing| existing == &dep) {
                deps.push(dep);
            }
        }
        either::Either::Right(ctx) => {
            let mut deps = ctx
                .path_label_deps
                .lock()
                .expect("module_ctx path label deps poisoned");
            if !deps.iter().any(|existing| existing == &dep) {
                deps.push(dep);
            }
        }
    }
}

fn module_ctx_path_from_value_relative_to<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
    path: Value<'v>,
    eval: &Evaluator<'v, '_, '_>,
) -> starlark::Result<String> {
    let (path, dep) = repository_path_and_dep_from_value_relative_to(
        path,
        eval,
        Some(module_ctx_working_dir(this)),
    )?;
    if let Some(dep) = dep {
        module_ctx_record_path_dep(this, dep);
    }
    Ok(path)
}

fn repository_ctx_clean_working_dir(working_dir: &str) -> buck2_error::Result<()> {
    let working_dir = repository_path_for_write(working_dir)?;
    if working_dir.exists() {
        fs::remove_dir_all(&working_dir).map_err(|error| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "repository_ctx could not clean `{}` before retry: {}",
                working_dir.to_string_lossy(),
                error
            )
        })?;
    }
    fs::create_dir_all(&working_dir).map_err(|error| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Tier0,
            "repository_ctx could not create `{}` before retry: {}",
            working_dir.to_string_lossy(),
            error
        )
    })
}

fn repository_ctx_push_file<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
    file: BazelRepositoryGeneratedFile,
) {
    match this.unpack() {
        either::Either::Left(ctx) => ctx
            .files
            .lock()
            .expect("repository_ctx files poisoned")
            .push(file),
        either::Either::Right(ctx) => ctx
            .files
            .lock()
            .expect("repository_ctx files poisoned")
            .push(file),
    }
}

fn repository_ctx_write_bytes(path: &str, bytes: &[u8], executable: bool) -> starlark::Result<()> {
    let write_path = repository_path_for_write(path)?;
    if let Some(parent) = write_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryCtxWriteFile {
                path: write_path.to_string_lossy().into_owned(),
                error: e.to_string(),
            })
        })?;
    }
    fs::write(&write_path, bytes).map_err(|e| {
        buck2_error::Error::from(BazelRepositoryError::RepositoryCtxWriteFile {
            path: write_path.to_string_lossy().into_owned(),
            error: e.to_string(),
        })
    })?;
    if executable {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&write_path, fs::Permissions::from_mode(0o755)).map_err(|e| {
                buck2_error::Error::from(BazelRepositoryError::RepositoryCtxWriteFile {
                    path: write_path.to_string_lossy().into_owned(),
                    error: e.to_string(),
                })
            })?;
        }
    }
    Ok(())
}

fn repository_path_is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
fn repository_ctx_create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn repository_ctx_create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

fn repository_ctx_command_arg(
    value: Value<'_>,
    working_dir: &str,
    eval: &Evaluator<'_, '_, '_>,
) -> starlark::Result<String> {
    if let Some(path) = value.downcast_ref::<StarlarkRepositoryPath>() {
        return Ok(repository_ctx_command_path_object(&path.path, working_dir));
    }
    if StarlarkProvidersLabel::from_value(value).is_some() {
        let (path, _dep) =
            repository_path_and_dep_from_value_relative_to(value, eval, Some(working_dir))?;
        return Ok(repository_ctx_command_path_object(&path, working_dir));
    }
    if let Some(path) = value.unpack_str() {
        return Ok(repository_ctx_command_path(path, working_dir));
    }
    Ok(value.to_string())
}

fn repository_ctx_command_path_object(path: &str, working_dir: &str) -> String {
    if let Some(path) = repository_ctx_command_external_path(path, working_dir) {
        return path;
    }
    repository_path_for_read_abs_relative_to(path, working_dir)
        .to_string_lossy()
        .into_owned()
}

fn repository_ctx_command_env(value: &str, working_dir: &str) -> String {
    repository_ctx_command_path(value, working_dir)
}

fn repository_ctx_command_path(path: &str, working_dir: &str) -> String {
    if let Some(path) = repository_ctx_command_assignment_path(path, working_dir) {
        return path;
    }
    if let Some(path) = repository_ctx_command_external_path(path, working_dir) {
        return path;
    }
    path.to_owned()
}

fn repository_ctx_command_assignment_path(path: &str, working_dir: &str) -> Option<String> {
    if let Some(path) =
        repository_ctx_command_assignment_path_with_split(working_dir, path.split_once('='))
    {
        return Some(path);
    }
    repository_ctx_command_assignment_path_with_split(working_dir, path.rsplit_once('='))
}

fn repository_ctx_command_assignment_path_with_split(
    working_dir: &str,
    split: Option<(&str, &str)>,
) -> Option<String> {
    let (prefix, value) = split?;
    if prefix.is_empty() || prefix.contains('/') || prefix.contains('\\') {
        return None;
    }
    let value = repository_ctx_command_external_path(value, working_dir)?;
    Some(format!("{prefix}={value}"))
}

fn repository_ctx_command_external_path(path: &str, working_dir: &str) -> Option<String> {
    let suffix = repository_external_cell_suffix(path)?;
    let path = repository_external_cell_existing_path_relative_to(suffix, working_dir)
        .or_else(|| repository_external_cell_path_relative_to(suffix, working_dir))?;
    Some(path.to_string_lossy().into_owned())
}

fn repository_ctx_command_external_input_path(
    value: &str,
    repository_working_dir: &Path,
) -> Option<PathBuf> {
    if !Path::new(value).is_absolute() {
        return None;
    }
    if !value.contains("/external_cells/") {
        return None;
    }
    let path = PathBuf::from(value);
    if path == repository_working_dir || path.starts_with(repository_working_dir) {
        return None;
    }
    Some(path)
}

fn repository_ctx_validate_external_inputs_ready(
    values: impl IntoIterator<Item = String>,
    repository_working_dir: &Path,
    program: &str,
    mut record_dep: impl FnMut(RepositoryPathLabelDep),
) -> starlark::Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        let Some(path) = repository_ctx_command_external_input_path(&value, repository_working_dir)
        else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        if !repository_ctx_external_input_ready(&path) {
            if let Some(dep) = repository_ctx_external_input_dep(&path) {
                record_dep(dep);
            }
            return Err(buck2_error::Error::from(
                BazelRepositoryError::RepositoryCtxExecuteFailed {
                    program: program.to_owned(),
                    error: format!(
                        "external input `{}` was not materialized",
                        path.to_string_lossy()
                    ),
                },
            )
            .into());
        }
    }
    Ok(())
}

fn repository_ctx_external_input_dep(path: &Path) -> Option<RepositoryPathLabelDep> {
    repository_ctx_external_input_dep_impl(path, false)
}

fn repository_ctx_external_input_tree_dep(path: &Path) -> Option<RepositoryPathLabelDep> {
    repository_ctx_external_input_dep_impl(path, true)
}

fn repository_ctx_external_input_dep_impl(
    path: &Path,
    recursive: bool,
) -> Option<RepositoryPathLabelDep> {
    let path = path.to_string_lossy();
    let suffix = path
        .split_once("/external_cells/bzlmod_generated/")
        .map(|(_, suffix)| suffix)
        .or_else(|| {
            path.split_once("/external_cells/bzlmod/")
                .map(|(_, suffix)| suffix)
        })?;
    let (canonical_repo_name, repo_path) = suffix.split_once('/').unwrap_or((suffix, ""));
    if canonical_repo_name.ends_with(".repository_ctx") {
        return None;
    }
    let cell_name = bzlmod_cell_name(canonical_repo_name);
    if recursive {
        Some(RepositoryPathLabelDep::tree(
            cell_name,
            (!repo_path.is_empty()).then(|| repo_path.to_owned()),
        ))
    } else if repo_path.is_empty() {
        Some(RepositoryPathLabelDep::cell(cell_name))
    } else {
        Some(RepositoryPathLabelDep::cell_path(
            cell_name,
            repo_path.to_owned(),
        ))
    }
}

fn repository_ctx_external_input_ready(path: &Path) -> bool {
    path.exists()
}

fn repository_ctx_download_error<'v>(
    allow_fail: bool,
    error: buck2_error::Error,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    if allow_fail {
        Ok(module_ctx_download_result(
            false,
            None,
            None,
            Some(&error.to_string()),
            eval,
        ))
    } else {
        Err(error.into())
    }
}

fn repository_ctx_download_option_is_empty(value: Value<'_>) -> bool {
    if value.is_none() {
        return true;
    }
    if let Some(value) = value.unpack_str() {
        return value.is_empty();
    }
    if let Some(dict) = DictRef::from_value(value) {
        return dict.iter().next().is_none();
    }
    if let Some(list) = ListRef::from_value(value) {
        return list.iter().next().is_none();
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return tuple.iter().next().is_none();
    }
    false
}

fn repository_ctx_download_options_are_empty(
    entries: &UnpackDictEntries<Value<'_>, Value<'_>>,
) -> bool {
    entries
        .entries
        .iter()
        .all(|(_, value)| repository_ctx_download_option_is_empty(*value))
}

fn repository_ctx_download_to_path<'v>(
    urls: Vec<String>,
    output_path: String,
    sha256: &str,
    executable: bool,
    allow_fail: bool,
    integrity: &str,
    canonical_id: &str,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<(Value<'v>, bool)> {
    let expected_checksum = match module_ctx_expected_checksum(sha256, integrity) {
        Ok(expected_checksum) => expected_checksum,
        Err(error) => {
            return Ok((
                repository_ctx_download_error(allow_fail, error, eval)?,
                false,
            ));
        }
    };
    let write_path = match repository_path_for_write(&output_path) {
        Ok(path) => path,
        Err(error) => {
            return Ok((
                repository_ctx_download_error(allow_fail, error, eval)?,
                false,
            ));
        }
    };
    let (got_sha256, got_integrity) = match module_ctx_download_to_path_blocking(
        &urls,
        &write_path,
        expected_checksum.as_ref(),
        canonical_id,
        executable,
    ) {
        Ok(checksums) => checksums,
        Err(error) => {
            return Ok((
                repository_ctx_download_error(allow_fail, error, eval)?,
                false,
            ));
        }
    };
    Ok((
        module_ctx_download_result(
            true,
            got_sha256.as_deref(),
            Some(&got_integrity),
            None,
            eval,
        ),
        true,
    ))
}

fn repository_ctx_extract_archive(
    archive: &Path,
    output: &Path,
    strip_prefix: &str,
    strip_components: i32,
) -> buck2_error::Result<()> {
    fs::create_dir_all(output).map_err(|e| BazelRepositoryError::RepositoryCtxExtractArchive {
        archive: archive.to_string_lossy().into_owned(),
        error: e.to_string(),
    })?;
    let archive_str = archive.to_string_lossy();
    let mut command = if archive_str.ends_with(".zip") {
        let mut command = Command::new("unzip");
        command.arg("-q").arg(archive).arg("-d").arg(output);
        command
    } else {
        let mut command = Command::new("tar");
        command.arg("-xf").arg(archive).arg("-C").arg(output);
        let strip_components = if strip_components > 0 {
            strip_components
        } else if !strip_prefix.is_empty() {
            strip_prefix
                .split('/')
                .filter(|part| !part.is_empty())
                .count()
                .try_into()
                .unwrap_or(i32::MAX)
        } else {
            0
        };
        if strip_components > 0 {
            command.arg(format!("--strip-components={strip_components}"));
        }
        command
    };
    let output =
        command
            .output()
            .map_err(|e| BazelRepositoryError::RepositoryCtxExtractArchive {
                archive: archive.to_string_lossy().into_owned(),
                error: e.to_string(),
            })?;
    if !output.status.success() {
        return Err(BazelRepositoryError::RepositoryCtxExtractArchive {
            archive: archive.to_string_lossy().into_owned(),
            error: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
        .into());
    }
    Ok(())
}

#[starlark_module]
fn repository_context_methods(builder: &mut MethodsBuilder) {
    fn file<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(default = "")] content: &str,
        #[starlark(default = true)] executable: bool,
        #[starlark(require = named, default = false)] _legacy_utf8: bool,
    ) -> starlark::Result<NoneType> {
        let path = repository_ctx_output_path_from_value(path, repository_ctx_working_dir(this))?;
        let full_path = Path::new(repository_ctx_working_dir(this))
            .join(&path)
            .to_string_lossy()
            .into_owned();
        repository_ctx_write_bytes(&full_path, content.as_bytes(), executable)?;
        repository_ctx_push_file(
            this,
            BazelRepositoryGeneratedFile {
                path,
                content: content.to_owned(),
                executable,
            },
        );
        Ok(NoneType)
    }

    fn template<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = pos)] template: Value<'v>,
        #[starlark(default = UnpackDictEntries::default())] substitutions: UnpackDictEntries<
            &'v str,
            &'v str,
        >,
        #[starlark(default = true)] executable: bool,
        #[starlark(require = named, default = "auto")] _watch_template: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let working_dir = repository_ctx_working_dir(this);
        let path = repository_ctx_output_path_from_value(path, working_dir)?;
        let template_path = repository_ctx_path_from_value_relative_to(this, template, eval)?;
        let read_path = repository_path_for_read(&template_path);
        let mut content = fs::read_to_string(&read_path).map_err(|e| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryCtxTemplateReadFile {
                path: template_path.clone(),
                error: e.to_string(),
            })
        })?;
        for (key, value) in substitutions.entries {
            content = content.replace(key, value);
        }
        let full_path = Path::new(working_dir)
            .join(&path)
            .to_string_lossy()
            .into_owned();
        repository_ctx_write_bytes(&full_path, content.as_bytes(), executable)?;
        repository_ctx_push_file(
            this,
            BazelRepositoryGeneratedFile {
                path,
                content,
                executable,
            },
        );
        Ok(NoneType)
    }

    fn path<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkRepositoryPath> {
        let (path, dep) = repository_path_and_dep_from_value_relative_to(
            path,
            eval,
            Some(repository_ctx_working_dir(this)),
        )?;
        if let Some(dep) = dep.clone() {
            repository_ctx_record_path_dep(this, dep);
        }
        Ok(StarlarkRepositoryPath::new_with_dep(path, dep))
    }

    fn read<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = named, default = "auto")] _watch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let path = repository_ctx_path_from_value_relative_to(this, path, eval)?;
        let read_path = repository_path_for_read(&path);
        let bytes = fs::read(&read_path).map_err(|e| {
            buck2_error::Error::from(BazelRepositoryError::ModuleCtxReadFile {
                path: path.clone(),
                error: e.to_string(),
            })
        })?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn watch<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let _path = repository_ctx_path_from_value_relative_to(this, path, eval)?;
        Ok(NoneType)
    }

    fn watch_tree<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let path = repository_ctx_path_from_value_relative_to(this, path, eval)?;
        if let Some(dep) = repository_ctx_external_input_tree_dep(Path::new(&path)) {
            repository_ctx_record_path_dep(this, dep);
        }
        Ok(NoneType)
    }

    fn repo_metadata<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = named, default = false)] reproducible: bool,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        attrs_for_reproducibility: UnpackDictEntries<Value<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let _unused = this;
        if reproducible && !attrs_for_reproducibility.entries.is_empty() {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "attrs_for_reproducibility can only be specified if reproducible is False"
            )
            .into());
        }
        let attrs_for_reproducibility = eval
            .heap()
            .alloc(AllocDict(attrs_for_reproducibility.entries));
        Ok(eval.heap().alloc(AllocStruct([
            ("reproducible", eval.heap().alloc(reproducible)),
            ("attrs_for_reproducibility", attrs_for_reproducibility),
        ])))
    }

    fn report_progress<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] message: &str,
    ) -> starlark::Result<NoneType> {
        let _unused = (this, message);
        Ok(NoneType)
    }

    fn delete<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let path = repository_ctx_path_from_value_relative_to(this, path, eval)?;
        let write_path = repository_path_for_write(&path)?;
        if !write_path.exists() {
            return Ok(false);
        }
        let result = if write_path.is_dir() {
            fs::remove_dir_all(&write_path)
        } else {
            fs::remove_file(&write_path)
        };
        result.map_err(|e| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryCtxDeletePath {
                path: write_path.to_string_lossy().into_owned(),
                error: e.to_string(),
            })
        })?;
        Ok(true)
    }

    fn patch<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] patch_file: Value<'v>,
        #[starlark(default = 0)] strip: i32,
        #[starlark(require = named, default = "auto")] _watch_patch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let working_dir = repository_ctx_working_dir(this);
        let patch_path = repository_ctx_path_from_value_relative_to(this, patch_file, eval)?;
        let patch_path_abs = repository_path_for_read_abs_relative_to(&patch_path, working_dir);
        if patch_path_abs.is_dir() {
            return Err(
                buck2_error::Error::from(BazelRepositoryError::RepositoryCtxPatch {
                    patch: patch_path.clone(),
                    error: "attempting to use a directory as patch file".to_owned(),
                })
                .into(),
            );
        }
        let working_dir_abs = repository_path_for_write(working_dir)?;
        fs::create_dir_all(&working_dir_abs).map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryCtxPatch {
                patch: patch_path.clone(),
                error: error.to_string(),
            })
        })?;
        let output = Command::new("patch")
            .arg(format!("-p{strip}"))
            .arg("-i")
            .arg(&patch_path_abs)
            .arg("-d")
            .arg(&working_dir_abs)
            .output()
            .map_err(|error| {
                buck2_error::Error::from(BazelRepositoryError::RepositoryCtxPatch {
                    patch: patch_path.clone(),
                    error: error.to_string(),
                })
            })?;
        if !output.status.success() {
            return Err(
                buck2_error::Error::from(BazelRepositoryError::RepositoryCtxPatch {
                    patch: patch_path,
                    error: format!(
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ),
                })
                .into(),
            );
        }
        Ok(NoneType)
    }

    fn symlink<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] target: Value<'v>,
        #[starlark(require = pos)] link_name: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let working_dir = repository_ctx_working_dir(this);
        let target = repository_ctx_path_from_value_relative_to(this, target, eval)?;
        let link = repository_ctx_path_from_value_relative_to(this, link_name, eval)?;
        let target_path = repository_path_for_read_abs_relative_to(&target, working_dir);
        let link_path = repository_path_for_write(&link)?;
        if let Some(dep) = repository_ctx_external_input_dep(&target_path) {
            repository_ctx_record_path_dep(this, dep);
        }
        if repository_ctx_external_input_dep(&target_path).is_some()
            && !repository_ctx_external_input_ready(&target_path)
        {
            return Err(
                buck2_error::Error::from(BazelRepositoryError::RepositoryCtxSymlink {
                    target,
                    link,
                    error: "external symlink target is not materialized".to_owned(),
                })
                .into(),
            );
        }
        if target_path.is_dir()
            && let Some(dep) = repository_ctx_external_input_tree_dep(&target_path)
        {
            repository_ctx_record_path_dep(this, dep);
        }
        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                buck2_error::Error::from(BazelRepositoryError::RepositoryCtxSymlink {
                    target: target.clone(),
                    link: link.clone(),
                    error: error.to_string(),
                })
            })?;
        }
        repository_ctx_create_symlink(&target_path, &link_path).map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryCtxSymlink {
                target,
                link,
                error: error.to_string(),
            })
        })?;
        Ok(NoneType)
    }

    fn which<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] program: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let _unused = this;
        if program.is_empty() {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::RepositoryCtxWhichEmptyProgram,
            )
            .into());
        }
        if program.contains('/') || program.contains('\\') {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::RepositoryCtxWhichInvalidProgram(program.to_owned()),
            )
            .into());
        }
        let Some(path) = env::var_os("PATH") else {
            return Ok(Value::new_none());
        };
        for dir in env::split_paths(&path) {
            if !dir.is_absolute() {
                continue;
            }
            let candidate = dir.join(program);
            if repository_path_is_executable(&candidate) {
                return Ok(eval
                    .heap()
                    .alloc(StarlarkRepositoryPath::new(
                        candidate.to_string_lossy().into_owned(),
                    ))
                    .to_value());
            }
        }
        Ok(Value::new_none())
    }

    fn execute<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] arguments: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        environment: UnpackDictEntries<&'v str, &'v str>,
        #[starlark(require = named, default = 600)] timeout: i32,
        #[starlark(require = named, default = true)] quiet: bool,
        #[starlark(require = named)] working_directory: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let _unused = (timeout, quiet);
        let repository_working_dir = repository_ctx_working_dir(this).to_owned();
        let mut arguments = arguments
            .items
            .into_iter()
            .map(|arg| repository_ctx_command_arg(arg, &repository_working_dir, eval))
            .collect::<starlark::Result<Vec<_>>>()?;
        if arguments.is_empty() {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::RepositoryCtxExecuteEmptyArguments,
            )
            .into());
        }
        let program = arguments.remove(0);
        let repository_working_dir_abs = repository_path_for_write(&repository_working_dir)?;
        let environment = environment
            .entries
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    repository_ctx_command_env(value, &repository_working_dir),
                )
            })
            .collect::<Vec<_>>();
        repository_ctx_validate_external_inputs_ready(
            std::iter::once(program.clone()).chain(arguments.iter().cloned()),
            &repository_working_dir_abs,
            &program,
            |dep| repository_ctx_record_path_dep(this, dep),
        )?;
        let mut command = Command::new(&program);
        command.args(arguments);
        for (key, value) in environment {
            command.env(key, value);
        }
        let working_directory = match working_directory {
            Some(working_directory) => repository_path_from_value_relative_to(
                working_directory,
                eval,
                Some(&repository_working_dir),
            )?,
            None => repository_working_dir.clone(),
        };
        let working_directory = if working_directory == repository_working_dir {
            repository_working_dir_abs
        } else {
            repository_path_for_write(&working_directory)?
        };
        fs::create_dir_all(&working_directory).map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryCtxExecuteFailed {
                program: program.clone(),
                error: error.to_string(),
            })
        })?;
        command.current_dir(working_directory);
        let output = command.output().map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryCtxExecuteFailed {
                program: program.clone(),
                error: error.to_string(),
            })
        })?;
        Ok(eval.heap().alloc(AllocStruct([
            (
                "stdout",
                eval.heap()
                    .alloc(String::from_utf8_lossy(&output.stdout).into_owned()),
            ),
            (
                "stderr",
                eval.heap()
                    .alloc(String::from_utf8_lossy(&output.stderr).into_owned()),
            ),
            (
                "return_code",
                eval.heap().alloc(output.status.code().unwrap_or(1)),
            ),
        ])))
    }

    fn download<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        url: Value<'v>,
        output: Value<'v>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = false)] executable: bool,
        #[starlark(default = false)] allow_fail: bool,
        #[starlark(default = "")] canonical_id: &str,
        #[starlark(default = UnpackDictEntries::default())] auth: UnpackDictEntries<
            Value<'v>,
            Value<'v>,
        >,
        #[starlark(default = UnpackDictEntries::default())] headers: UnpackDictEntries<
            Value<'v>,
            Value<'v>,
        >,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if !repository_ctx_download_options_are_empty(&auth) {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "auth" },
            )
            .into());
        }
        if !repository_ctx_download_options_are_empty(&headers) {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "headers" },
            )
            .into());
        }

        let urls = module_ctx_urls_from_value(url, eval.heap())?;
        let output_path = repository_path_from_value_relative_to(
            output,
            eval,
            Some(repository_ctx_working_dir(this)),
        )?;
        let (result, _) = repository_ctx_download_to_path(
            urls,
            output_path,
            sha256,
            executable,
            allow_fail,
            integrity,
            canonical_id,
            eval,
        )?;
        Ok(module_ctx_pending_download(block, result, eval))
    }

    #[allow(non_snake_case)]
    fn download_and_extract<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        url: Value<'v>,
        #[starlark(default = "")] output: Value<'v>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = "")] r#type: &str,
        #[starlark(default = "")] strip_prefix: &str,
        #[starlark(default = false)] allow_fail: bool,
        #[starlark(default = "")] canonical_id: &str,
        #[starlark(default = UnpackDictEntries::default())] auth: UnpackDictEntries<
            Value<'v>,
            Value<'v>,
        >,
        #[starlark(default = UnpackDictEntries::default())] headers: UnpackDictEntries<
            Value<'v>,
            Value<'v>,
        >,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        rename_files: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = "")] stripPrefix: &str,
        #[starlark(require = named, default = 0)] strip_components: i32,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let working_dir = repository_ctx_working_dir(this);
        let archive_name = if r#type.is_empty() {
            ".buck2_download_and_extract.archive".to_owned()
        } else {
            format!(
                ".buck2_download_and_extract.archive.{}",
                r#type.trim_start_matches('.')
            )
        };
        let archive_path = Path::new(working_dir).join(archive_name);
        let archive_path_string = archive_path.to_string_lossy().into_owned();
        if !repository_ctx_download_options_are_empty(&auth) {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "auth" },
            )
            .into());
        }
        if !repository_ctx_download_options_are_empty(&headers) {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "headers" },
            )
            .into());
        }
        if !rename_files.entries.is_empty() {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField {
                    field: "rename_files",
                },
            )
            .into());
        }
        let urls = module_ctx_urls_from_value(url, eval.heap())?;
        let (result, success) = repository_ctx_download_to_path(
            urls,
            archive_path_string.clone(),
            sha256,
            false,
            allow_fail,
            integrity,
            canonical_id,
            eval,
        )?;
        if !success {
            return Ok(module_ctx_pending_download(block, result, eval));
        }
        let output_path = repository_path_from_value_relative_to(output, eval, Some(working_dir))?;
        let output_path = repository_path_for_write(&output_path)?;
        let archive_path = repository_path_for_write(&archive_path_string)?;
        let strip_prefix = if stripPrefix.is_empty() {
            strip_prefix
        } else {
            stripPrefix
        };
        let result = match repository_ctx_extract_archive(
            &archive_path,
            &output_path,
            strip_prefix,
            strip_components,
        ) {
            Ok(()) => result,
            Err(error) => repository_ctx_download_error(allow_fail, error, eval)?,
        };
        Ok(module_ctx_pending_download(block, result, eval))
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkModuleExtensionContext<'v> {
    modules: Value<'v>,
    working_dir: String,
    root_module_has_non_dev_dependency: bool,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    path_label_deps: Mutex<Vec<RepositoryPathLabelDep>>,
}

#[allow(dead_code)]
impl<'v> StarlarkModuleExtensionContext<'v> {
    pub(crate) fn new(
        modules: Value<'v>,
        working_dir: String,
        root_module_has_non_dev_dependency: bool,
    ) -> Self {
        Self {
            modules,
            working_dir,
            root_module_has_non_dev_dependency,
            path_label_deps: Mutex::new(Vec::new()),
        }
    }

    fn take_path_label_deps(&self) -> Vec<RepositoryPathLabelDep> {
        std::mem::take(
            &mut *self
                .path_label_deps
                .lock()
                .expect("module_ctx path label deps poisoned"),
        )
    }
}

impl<'v> Display for StarlarkModuleExtensionContext<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_ctx>")
    }
}

impl<'v> AllocValue<'v> for StarlarkModuleExtensionContext<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "module_ctx")]
impl<'v> StarlarkValue<'v> for StarlarkModuleExtensionContext<'v> {
    fn dir_attr(&self) -> Vec<String> {
        vec![
            "facts".to_owned(),
            "execute".to_owned(),
            "modules".to_owned(),
            "os".to_owned(),
            "report_progress".to_owned(),
            "root_module_has_non_dev_dependency".to_owned(),
            "watch".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "facts" => Some(empty_dict_value(heap)),
            "modules" => Some(self.modules),
            "os" => Some(heap.alloc(StarlarkRepositoryOs)),
            "root_module_has_non_dev_dependency" => {
                Some(Value::new_bool(self.root_module_has_non_dev_dependency))
            }
            _ => None,
        }
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(module_extension_context_methods)
    }
}

impl<'v> Freeze for StarlarkModuleExtensionContext<'v> {
    type Frozen = FrozenStarlarkModuleExtensionContext;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkModuleExtensionContext {
            modules: self.modules.freeze(freezer)?,
            working_dir: self.working_dir,
            root_module_has_non_dev_dependency: self.root_module_has_non_dev_dependency,
            path_label_deps: Mutex::new(
                self.path_label_deps
                    .into_inner()
                    .expect("module_ctx path label deps poisoned"),
            ),
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkModuleExtensionContext {
    modules: FrozenValue,
    working_dir: String,
    root_module_has_non_dev_dependency: bool,
    #[allocative(skip)]
    path_label_deps: Mutex<Vec<RepositoryPathLabelDep>>,
}

impl Display for FrozenStarlarkModuleExtensionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_ctx>")
    }
}

starlark_simple_value!(FrozenStarlarkModuleExtensionContext);

#[starlark_value(type = "module_ctx")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkModuleExtensionContext {
    type Canonical = StarlarkModuleExtensionContext<'v>;

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "facts".to_owned(),
            "execute".to_owned(),
            "modules".to_owned(),
            "os".to_owned(),
            "report_progress".to_owned(),
            "root_module_has_non_dev_dependency".to_owned(),
            "watch".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "facts" => Some(empty_dict_value(heap)),
            "modules" => Some(self.modules.to_value()),
            "os" => Some(heap.alloc(FrozenStarlarkRepositoryOs)),
            "root_module_has_non_dev_dependency" => {
                Some(Value::new_bool(self.root_module_has_non_dev_dependency))
            }
            _ => None,
        }
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(module_extension_context_methods)
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkBazelModule<'v> {
    name: String,
    version: String,
    tags: Value<'v>,
    is_root: bool,
}

#[allow(dead_code)]
impl<'v> StarlarkBazelModule<'v> {
    pub(crate) fn new(name: String, version: String, tags: Value<'v>, is_root: bool) -> Self {
        Self {
            name,
            version,
            tags,
            is_root,
        }
    }
}

impl<'v> Display for StarlarkBazelModule<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<bazel_module {}@{}>", self.name, self.version)
    }
}

impl<'v> AllocValue<'v> for StarlarkBazelModule<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "bazel_module")]
impl<'v> StarlarkValue<'v> for StarlarkBazelModule<'v> {
    fn dir_attr(&self) -> Vec<String> {
        vec![
            "is_root".to_owned(),
            "name".to_owned(),
            "tags".to_owned(),
            "version".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "is_root" => Some(Value::new_bool(self.is_root)),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "tags" => Some(self.tags),
            "version" => Some(heap.alloc_str(&self.version).to_value()),
            _ => None,
        }
    }
}

impl<'v> Freeze for StarlarkBazelModule<'v> {
    type Frozen = FrozenStarlarkBazelModule;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkBazelModule {
            name: self.name,
            version: self.version,
            tags: self.tags.freeze(freezer)?,
            is_root: self.is_root,
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkBazelModule {
    name: String,
    version: String,
    tags: FrozenValue,
    is_root: bool,
}

impl Display for FrozenStarlarkBazelModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<bazel_module {}@{}>", self.name, self.version)
    }
}

starlark_simple_value!(FrozenStarlarkBazelModule);

#[starlark_value(type = "bazel_module")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkBazelModule {
    type Canonical = StarlarkBazelModule<'v>;

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "is_root".to_owned(),
            "name".to_owned(),
            "tags".to_owned(),
            "version".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "is_root" => Some(Value::new_bool(self.is_root)),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "tags" => Some(self.tags.to_value()),
            "version" => Some(heap.alloc_str(&self.version).to_value()),
            _ => None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkBazelModuleTags<'v> {
    tags: SmallMap<String, Value<'v>>,
}

#[allow(dead_code)]
impl<'v> StarlarkBazelModuleTags<'v> {
    pub(crate) fn new(tags: SmallMap<String, Value<'v>>) -> Self {
        Self { tags }
    }
}

impl<'v> Display for StarlarkBazelModuleTags<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<bazel_module_tags>")
    }
}

impl<'v> AllocValue<'v> for StarlarkBazelModuleTags<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "bazel_module_tags")]
impl<'v> StarlarkValue<'v> for StarlarkBazelModuleTags<'v> {
    fn dir_attr(&self) -> Vec<String> {
        self.tags.keys().cloned().collect()
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let _unused = heap;
        self.tags.get(attribute).copied()
    }
}

impl<'v> Freeze for StarlarkBazelModuleTags<'v> {
    type Frozen = FrozenStarlarkBazelModuleTags;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let tags = self
            .tags
            .into_iter()
            .map(|(name, values)| Ok((name, values.freeze(freezer)?)))
            .collect::<FreezeResult<SmallMap<_, _>>>()?;
        Ok(FrozenStarlarkBazelModuleTags { tags })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkBazelModuleTags {
    tags: SmallMap<String, FrozenValue>,
}

impl Display for FrozenStarlarkBazelModuleTags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<bazel_module_tags>")
    }
}

starlark_simple_value!(FrozenStarlarkBazelModuleTags);

#[starlark_value(type = "bazel_module_tags")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkBazelModuleTags {
    type Canonical = StarlarkBazelModuleTags<'v>;

    fn dir_attr(&self) -> Vec<String> {
        self.tags.keys().cloned().collect()
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let _unused = heap;
        self.tags.get(attribute).map(|tags| tags.to_value())
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkBazelModuleTag<'v> {
    tag_name: String,
    dev_dependency: bool,
    sort_key: i32,
    attrs: SmallMap<String, Value<'v>>,
}

#[allow(dead_code)]
impl<'v> StarlarkBazelModuleTag<'v> {
    pub(crate) fn new(
        tag_name: String,
        dev_dependency: bool,
        sort_key: i32,
        attrs: SmallMap<String, Value<'v>>,
    ) -> Self {
        Self {
            tag_name,
            dev_dependency,
            sort_key,
            attrs,
        }
    }
}

impl<'v> Display for StarlarkBazelModuleTag<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_extension_tag {}>", self.tag_name)
    }
}

impl<'v> AllocValue<'v> for StarlarkBazelModuleTag<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "module_extension_tag")]
impl<'v> StarlarkValue<'v> for StarlarkBazelModuleTag<'v> {
    fn dir_attr(&self) -> Vec<String> {
        self.attrs.keys().cloned().collect()
    }

    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.attrs.get(attribute).copied()
    }
}

impl<'v> Freeze for StarlarkBazelModuleTag<'v> {
    type Frozen = FrozenStarlarkBazelModuleTag;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let attrs = self
            .attrs
            .into_iter()
            .map(|(name, value)| Ok((name, value.freeze(freezer)?)))
            .collect::<FreezeResult<SmallMap<_, _>>>()?;
        Ok(FrozenStarlarkBazelModuleTag {
            tag_name: self.tag_name,
            dev_dependency: self.dev_dependency,
            sort_key: self.sort_key,
            attrs,
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkBazelModuleTag {
    tag_name: String,
    dev_dependency: bool,
    sort_key: i32,
    attrs: SmallMap<String, FrozenValue>,
}

impl Display for FrozenStarlarkBazelModuleTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_extension_tag {}>", self.tag_name)
    }
}

starlark_simple_value!(FrozenStarlarkBazelModuleTag);

#[starlark_value(type = "module_extension_tag")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkBazelModuleTag {
    type Canonical = StarlarkBazelModuleTag<'v>;

    fn dir_attr(&self) -> Vec<String> {
        self.attrs.keys().cloned().collect()
    }

    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.attrs.get(attribute).map(|value| value.to_value())
    }
}

#[derive(Debug, Display, ProvidesStaticType, NoSerialize, Allocative)]
#[display("<extension_metadata>")]
pub(crate) struct StarlarkModuleExtensionMetadata {
    #[allow(dead_code)]
    reproducible: bool,
}

starlark_simple_value!(StarlarkModuleExtensionMetadata);

#[starlark_value(type = "extension_metadata")]
impl<'v> StarlarkValue<'v> for StarlarkModuleExtensionMetadata {}

fn bazel_module_tag_dev_dependency<'v>(tag: Value<'v>) -> starlark::Result<bool> {
    if let Some(tag) = tag.downcast_ref::<StarlarkBazelModuleTag>() {
        return Ok(tag.dev_dependency);
    }
    if let Some(tag) = tag.downcast_ref::<FrozenStarlarkBazelModuleTag>() {
        return Ok(tag.dev_dependency);
    }
    Err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::Input,
        "expected module extension tag, got `{}`",
        tag.get_type()
    )
    .into())
}

fn bazel_module_tag_sort_key<'v>(tag: Value<'v>) -> starlark::Result<i32> {
    if let Some(tag) = tag.downcast_ref::<StarlarkBazelModuleTag>() {
        return Ok(tag.sort_key);
    }
    if let Some(tag) = tag.downcast_ref::<FrozenStarlarkBazelModuleTag>() {
        return Ok(tag.sort_key);
    }
    Err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::Input,
        "expected module extension tag, got `{}`",
        tag.get_type()
    )
    .into())
}

fn module_ctx_urls_from_value<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<String>> {
    if let Some(url) = value.unpack_str() {
        return Ok(vec![url.to_owned()]);
    }

    let mut urls = Vec::new();
    for value in value.iterate(heap).map_err(|_| {
        buck2_error::Error::from(BazelRepositoryError::ModuleCtxDownloadUrlUnsupportedValue(
            value.get_type().to_owned(),
        ))
    })? {
        let Some(url) = value.unpack_str() else {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUrlUnsupportedValue(
                    value.get_type().to_owned(),
                ),
            )
            .into());
        };
        urls.push(url.to_owned());
    }
    if urls.is_empty() {
        return Err(buck2_error::Error::from(BazelRepositoryError::ModuleCtxDownloadNoUrls).into());
    }
    Ok(urls)
}

fn module_ctx_download_error_is_retryable(error: &buck2_http::HttpError) -> bool {
    match error {
        buck2_http::HttpError::Status { status, .. } => {
            let status = status.as_u16();
            matches!(status, 403 | 408 | 429)
                || (500..600).contains(&status) && status != 501 && status != 505
        }
        buck2_http::HttpError::SendRequest { .. } | buck2_http::HttpError::Timeout { .. } => true,
        _ => false,
    }
}

fn module_ctx_download_retry_delay(attempt: usize) -> Duration {
    const MIN_RETRY_DELAY_MS: u64 = 100;
    let shift = attempt.min(6) as u32;
    Duration::from_millis(MIN_RETRY_DELAY_MS.saturating_mul(1u64 << shift))
}

const MODULE_CTX_HTTP_MAX_PARALLEL_DOWNLOADS: usize = 8;

static MODULE_CTX_HTTP_DOWNLOAD_SEMAPHORE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

fn module_ctx_http_download_semaphore() -> &'static tokio::sync::Semaphore {
    MODULE_CTX_HTTP_DOWNLOAD_SEMAPHORE
        .get_or_init(|| {
            Arc::new(tokio::sync::Semaphore::new(
                MODULE_CTX_HTTP_MAX_PARALLEL_DOWNLOADS,
            ))
        })
        .as_ref()
}

async fn module_ctx_download_url_bytes(
    client: &buck2_http::HttpClient,
    url: &str,
) -> Result<Vec<u8>, String> {
    const MAX_ATTEMPTS: usize = 8;

    for attempt in 0..MAX_ATTEMPTS {
        let _permit = module_ctx_http_download_semaphore()
            .acquire()
            .await
            .map_err(|error| error.to_string())?;
        let result = match client.get(url).await {
            Ok(response) => match buck2_http::to_bytes(response.into_body()).await {
                Ok(body) => Ok(body.to_vec()),
                Err(error) => Err((error.to_string(), true)),
            },
            Err(error) => Err((
                error.to_string(),
                module_ctx_download_error_is_retryable(&error),
            )),
        };
        drop(_permit);

        match result {
            Ok(bytes) => return Ok(bytes),
            Err((message, retryable)) => {
                if attempt + 1 == MAX_ATTEMPTS || !retryable {
                    return Err(message);
                }
                tokio::time::sleep(module_ctx_download_retry_delay(attempt)).await;
            }
        }
    }

    unreachable!("module_ctx.download retry loop exits after success or final failure")
}

async fn module_ctx_download_bytes(urls: &[String]) -> buck2_error::Result<Vec<u8>> {
    let client = buck2_http::HttpClientBuilder::oss()
        .await?
        .with_max_redirects(10)
        .build();
    let mut last_error = None;
    for url in urls {
        match module_ctx_download_url_bytes(&client, url).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                last_error = Some(error);
            }
        }
    }
    Err(BazelRepositoryError::ModuleCtxDownloadFailed {
        urls: urls.to_owned(),
        error: last_error.unwrap_or_else(|| "no URL attempted".to_owned()),
    }
    .into())
}

fn module_ctx_download_bytes_uncached_blocking(urls: &[String]) -> buck2_error::Result<Vec<u8>> {
    let urls = urls.to_owned();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "could not create module_ctx.download runtime: {}",
                    e
                )
            })?
            .block_on(async move { module_ctx_download_bytes(&urls).await })
    })
    .join()
    .map_err(|_| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "module_ctx.download worker thread panicked"
        )
    })?
}

static MODULE_CTX_DOWNLOAD_CACHE_LOCKS: OnceLock<Mutex<BTreeMap<String, Arc<Mutex<()>>>>> =
    OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModuleCtxVerifiedDownloadCacheMetadata {
    len: u64,
    modified: Option<SystemTime>,
}

static MODULE_CTX_VERIFIED_DOWNLOAD_CACHE: OnceLock<
    Mutex<BTreeMap<String, ModuleCtxVerifiedDownloadCacheMetadata>>,
> = OnceLock::new();

fn module_ctx_download_cache_lock(key: &str) -> Arc<Mutex<()>> {
    let locks = MODULE_CTX_DOWNLOAD_CACHE_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks
        .lock()
        .expect("module ctx download cache lock map is poisoned");
    locks
        .entry(key.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn module_ctx_download_cache_verification_key(
    file: &Path,
    checksum: &ModuleCtxChecksum,
    canonical_id: &str,
) -> String {
    format!(
        "{}:{}:{}:{}",
        file.to_string_lossy(),
        checksum.kind.repository_cache_dir_name(),
        checksum.hex,
        canonical_id
    )
}

fn module_ctx_download_cache_file_metadata(
    file: &Path,
) -> buck2_error::Result<ModuleCtxVerifiedDownloadCacheMetadata> {
    let metadata = fs::metadata(file)
        .map_err(|error| module_ctx_download_cache_io_error("stat", file, error))?;
    Ok(ModuleCtxVerifiedDownloadCacheMetadata {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn module_ctx_download_cache_is_verified(
    key: &str,
    metadata: ModuleCtxVerifiedDownloadCacheMetadata,
) -> bool {
    let verified = MODULE_CTX_VERIFIED_DOWNLOAD_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    verified
        .lock()
        .expect("module ctx verified download cache is poisoned")
        .get(key)
        .copied()
        == Some(metadata)
}

fn module_ctx_download_cache_record_verified(
    key: String,
    metadata: ModuleCtxVerifiedDownloadCacheMetadata,
) {
    let verified = MODULE_CTX_VERIFIED_DOWNLOAD_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    verified
        .lock()
        .expect("module ctx verified download cache is poisoned")
        .insert(key, metadata);
}

fn module_ctx_repository_cache_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("BUCK2_BAZEL_REPOSITORY_CACHE") {
        if path.is_empty() {
            return None;
        }
        return Some(PathBuf::from(path));
    }
    Some(
        PathBuf::from(env::var_os("HOME")?)
            .join(".cache")
            .join("buck2")
            .join("cache")
            .join("repos")
            .join("v1"),
    )
}

fn module_ctx_repository_cache_entry_dir(checksum: &ModuleCtxChecksum) -> Option<PathBuf> {
    Some(
        module_ctx_repository_cache_path()?
            .join("content_addressable")
            .join(checksum.kind.repository_cache_dir_name())
            .join(&checksum.hex),
    )
}

fn module_ctx_repository_cache_id_path(
    entry: &Path,
    checksum: &ModuleCtxChecksum,
    canonical_id: &str,
) -> Option<PathBuf> {
    if canonical_id.is_empty() {
        return None;
    }
    Some(entry.join(format!(
        "id-{}",
        module_ctx_checksum_hex(checksum.kind, canonical_id.as_bytes())
    )))
}

fn module_ctx_download_cache_io_error(
    action: &str,
    path: &Path,
    error: std::io::Error,
) -> buck2_error::Error {
    buck2_error::buck2_error!(
        buck2_error::ErrorTag::Input,
        "failed to {} Bazel repository cache path `{}`: {}",
        action,
        path.display(),
        error
    )
}

fn module_ctx_download_write_error(path: &Path, error: std::io::Error) -> buck2_error::Error {
    BazelRepositoryError::ModuleCtxDownloadWriteFile {
        path: path.to_string_lossy().into_owned(),
        error: error.to_string(),
    }
    .into()
}

fn module_ctx_set_executable(path: &Path, executable: bool) -> buck2_error::Result<()> {
    if executable {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                .map_err(|error| module_ctx_download_write_error(path, error))?;
        }
    }
    Ok(())
}

fn module_ctx_write_download_bytes(
    path: &Path,
    bytes: &[u8],
    executable: bool,
) -> buck2_error::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| module_ctx_download_write_error(parent, error))?;
    }
    fs::write(path, bytes).map_err(|error| module_ctx_download_write_error(path, error))?;
    module_ctx_set_executable(path, executable)
}

fn module_ctx_copy_download_file(
    source: &Path,
    destination: &Path,
    executable: bool,
) -> buck2_error::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| module_ctx_download_write_error(parent, error))?;
    }
    fs::copy(source, destination)
        .map_err(|error| module_ctx_download_write_error(destination, error))?;
    module_ctx_set_executable(destination, executable)
}

fn module_ctx_download_cache_get_to_path(
    checksum: &ModuleCtxChecksum,
    canonical_id: &str,
    destination: &Path,
    executable: bool,
) -> buck2_error::Result<bool> {
    let Some(entry) = module_ctx_repository_cache_entry_dir(checksum) else {
        return Ok(false);
    };
    let file = entry.join("file");
    if !file.is_file() {
        return Ok(false);
    }
    if let Some(id_path) = module_ctx_repository_cache_id_path(&entry, checksum, canonical_id)
        && !id_path.exists()
    {
        return Ok(false);
    }
    let verification_key =
        module_ctx_download_cache_verification_key(&file, checksum, canonical_id);
    let metadata = module_ctx_download_cache_file_metadata(&file)?;
    if !module_ctx_download_cache_is_verified(&verification_key, metadata) {
        module_ctx_validate_download_file_checksum(&file, checksum)?;
        module_ctx_download_cache_record_verified(verification_key, metadata);
    }
    module_ctx_copy_download_file(&file, destination, executable)?;
    Ok(true)
}

fn module_ctx_download_cache_put_verified(
    checksum: &ModuleCtxChecksum,
    canonical_id: &str,
    bytes: &[u8],
) -> buck2_error::Result<()> {
    let Some(entry) = module_ctx_repository_cache_entry_dir(checksum) else {
        return Ok(());
    };
    fs::create_dir_all(&entry)
        .map_err(|error| module_ctx_download_cache_io_error("create", &entry, error))?;
    let file = entry.join("file");
    if !file.is_file() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let tmp = entry.join(format!("tmp-{}-{}", std::process::id(), nanos));
        fs::write(&tmp, bytes)
            .map_err(|error| module_ctx_download_cache_io_error("write", &tmp, error))?;
        if let Err(error) = fs::rename(&tmp, &file) {
            let _unused = fs::remove_file(&tmp);
            if !file.is_file() {
                return Err(module_ctx_download_cache_io_error("rename", &file, error));
            }
        }
    }
    if let Some(id_path) = module_ctx_repository_cache_id_path(&entry, checksum, canonical_id) {
        fs::write(&id_path, b"")
            .map_err(|error| module_ctx_download_cache_io_error("write", &id_path, error))?;
    }
    Ok(())
}

fn module_ctx_download_to_path_blocking(
    urls: &[String],
    destination: &Path,
    expected_checksum: Option<&ModuleCtxChecksum>,
    canonical_id: &str,
    executable: bool,
) -> buck2_error::Result<(Option<String>, String)> {
    if let Some(expected_checksum) = expected_checksum {
        if destination.is_file()
            && module_ctx_validate_download_file_checksum(destination, expected_checksum).is_ok()
        {
            module_ctx_set_executable(destination, executable)?;
            return module_ctx_download_result_checksums_verified(expected_checksum);
        }

        let lock_key = format!(
            "{}:{}:{}",
            expected_checksum.kind.repository_cache_dir_name(),
            expected_checksum.hex,
            canonical_id
        );
        let lock = module_ctx_download_cache_lock(&lock_key);
        let _guard = lock
            .lock()
            .expect("module ctx download cache entry lock is poisoned");
        if destination.is_file()
            && module_ctx_validate_download_file_checksum(destination, expected_checksum).is_ok()
        {
            module_ctx_set_executable(destination, executable)?;
            return module_ctx_download_result_checksums_verified(expected_checksum);
        }
        if module_ctx_download_cache_get_to_path(
            expected_checksum,
            canonical_id,
            destination,
            executable,
        )
        .unwrap_or(false)
        {
            return module_ctx_download_result_checksums_verified(expected_checksum);
        }

        let bytes = module_ctx_download_bytes_uncached_blocking(urls)?;
        module_ctx_validate_download_checksum(
            &destination.to_string_lossy(),
            &bytes,
            Some(expected_checksum),
        )?;
        module_ctx_write_download_bytes(destination, &bytes, executable)?;
        module_ctx_download_cache_put_verified(expected_checksum, canonical_id, &bytes)?;
        return module_ctx_download_result_checksums_verified(expected_checksum);
    }

    let bytes = module_ctx_download_bytes_uncached_blocking(urls)?;
    let checksums = module_ctx_download_result_checksums(&bytes, None)?;
    module_ctx_write_download_bytes(destination, &bytes, executable)?;
    if let Some(sha256) = &checksums.0 {
        module_ctx_download_cache_put_verified(
            &ModuleCtxChecksum {
                kind: ModuleCtxChecksumKind::Sha256,
                hex: sha256.clone(),
            },
            canonical_id,
            &bytes,
        )?;
    }
    Ok(checksums)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModuleCtxChecksumKind {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl ModuleCtxChecksumKind {
    fn integrity_prefix(&self) -> &'static str {
        match self {
            Self::Sha1 => "sha1-",
            Self::Sha256 => "sha256-",
            Self::Sha384 => "sha384-",
            Self::Sha512 => "sha512-",
        }
    }

    fn byte_len(&self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
        }
    }

    fn repository_cache_dir_name(&self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModuleCtxChecksum {
    kind: ModuleCtxChecksumKind,
    hex: String,
}

fn module_ctx_expected_checksum(
    sha256: &str,
    integrity: &str,
) -> buck2_error::Result<Option<ModuleCtxChecksum>> {
    if !sha256.is_empty() && !integrity.is_empty() {
        return Err(BazelRepositoryError::ModuleCtxDownloadConflictingChecksums.into());
    }
    if !sha256.is_empty() {
        return Ok(Some(ModuleCtxChecksum {
            kind: ModuleCtxChecksumKind::Sha256,
            hex: sha256.to_ascii_lowercase(),
        }));
    }
    module_ctx_checksum_from_integrity(integrity)
}

fn module_ctx_checksum_from_integrity(
    integrity: &str,
) -> buck2_error::Result<Option<ModuleCtxChecksum>> {
    if integrity.is_empty() {
        return Ok(None);
    }
    for kind in [
        ModuleCtxChecksumKind::Sha1,
        ModuleCtxChecksumKind::Sha256,
        ModuleCtxChecksumKind::Sha384,
        ModuleCtxChecksumKind::Sha512,
    ] {
        if let Some(encoded) = integrity.strip_prefix(kind.integrity_prefix()) {
            let bytes = BASE64_STANDARD.decode(encoded).map_err(|_| {
                BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(integrity.to_owned())
            })?;
            if bytes.len() != kind.byte_len() {
                return Err(BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(
                    integrity.to_owned(),
                )
                .into());
            }
            return Ok(Some(ModuleCtxChecksum {
                kind,
                hex: hex::encode(bytes),
            }));
        }
    }
    Err(BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(integrity.to_owned()).into())
}

fn module_ctx_checksum_hex(kind: ModuleCtxChecksumKind, bytes: &[u8]) -> String {
    match kind {
        ModuleCtxChecksumKind::Sha1 => hex::encode(Sha1::digest(bytes)),
        ModuleCtxChecksumKind::Sha256 => hex::encode(Sha256::digest(bytes)),
        ModuleCtxChecksumKind::Sha384 => hex::encode(Sha384::digest(bytes)),
        ModuleCtxChecksumKind::Sha512 => hex::encode(Sha512::digest(bytes)),
    }
}

fn module_ctx_checksum_hex_file(
    kind: ModuleCtxChecksumKind,
    path: &Path,
) -> buck2_error::Result<String> {
    fn read_chunks(path: &Path, mut update: impl FnMut(&[u8])) -> buck2_error::Result<()> {
        let mut file = fs::File::open(path).map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::ModuleCtxReadFile {
                path: path.to_string_lossy().into_owned(),
                error: error.to_string(),
            })
        })?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let bytes_read = file.read(&mut buffer).map_err(|error| {
                buck2_error::Error::from(BazelRepositoryError::ModuleCtxReadFile {
                    path: path.to_string_lossy().into_owned(),
                    error: error.to_string(),
                })
            })?;
            if bytes_read == 0 {
                return Ok(());
            }
            update(&buffer[..bytes_read]);
        }
    }

    match kind {
        ModuleCtxChecksumKind::Sha1 => {
            let mut hasher = Sha1::new();
            read_chunks(path, |bytes| hasher.update(bytes))?;
            Ok(hex::encode(hasher.finalize()))
        }
        ModuleCtxChecksumKind::Sha256 => {
            let mut hasher = Sha256::new();
            read_chunks(path, |bytes| hasher.update(bytes))?;
            Ok(hex::encode(hasher.finalize()))
        }
        ModuleCtxChecksumKind::Sha384 => {
            let mut hasher = Sha384::new();
            read_chunks(path, |bytes| hasher.update(bytes))?;
            Ok(hex::encode(hasher.finalize()))
        }
        ModuleCtxChecksumKind::Sha512 => {
            let mut hasher = Sha512::new();
            read_chunks(path, |bytes| hasher.update(bytes))?;
            Ok(hex::encode(hasher.finalize()))
        }
    }
}

fn module_ctx_integrity_from_checksum(checksum: &ModuleCtxChecksum) -> buck2_error::Result<String> {
    let bytes = hex::decode(&checksum.hex).map_err(|_| {
        BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(checksum.hex.clone())
    })?;
    Ok(format!(
        "{}{}",
        checksum.kind.integrity_prefix(),
        BASE64_STANDARD.encode(bytes)
    ))
}

fn module_ctx_validate_download_checksum(
    path: &str,
    bytes: &[u8],
    expected_checksum: Option<&ModuleCtxChecksum>,
) -> buck2_error::Result<()> {
    let Some(expected_checksum) = expected_checksum else {
        return Ok(());
    };
    let got = module_ctx_checksum_hex(expected_checksum.kind, bytes);
    if expected_checksum.hex == got {
        return Ok(());
    }
    Err(BazelRepositoryError::ModuleCtxDownloadChecksumMismatch {
        path: path.to_owned(),
        expected: expected_checksum.hex.clone(),
        got,
    }
    .into())
}

fn module_ctx_validate_download_file_checksum(
    path: &Path,
    expected_checksum: &ModuleCtxChecksum,
) -> buck2_error::Result<()> {
    let got = module_ctx_checksum_hex_file(expected_checksum.kind, path)?;
    if expected_checksum.hex == got {
        return Ok(());
    }
    Err(BazelRepositoryError::ModuleCtxDownloadChecksumMismatch {
        path: path.to_string_lossy().into_owned(),
        expected: expected_checksum.hex.clone(),
        got,
    }
    .into())
}

fn module_ctx_download_result_checksums_verified(
    expected_checksum: &ModuleCtxChecksum,
) -> buck2_error::Result<(Option<String>, String)> {
    let sha256 = (expected_checksum.kind == ModuleCtxChecksumKind::Sha256)
        .then(|| expected_checksum.hex.clone());
    let integrity = module_ctx_integrity_from_checksum(expected_checksum)?;
    Ok((sha256, integrity))
}

fn module_ctx_download_result_checksums(
    bytes: &[u8],
    expected_checksum: Option<&ModuleCtxChecksum>,
) -> buck2_error::Result<(Option<String>, String)> {
    let checksum = expected_checksum
        .cloned()
        .unwrap_or_else(|| ModuleCtxChecksum {
            kind: ModuleCtxChecksumKind::Sha256,
            hex: module_ctx_checksum_hex(ModuleCtxChecksumKind::Sha256, bytes),
        });
    let sha256 = (checksum.kind == ModuleCtxChecksumKind::Sha256).then(|| checksum.hex.clone());
    let integrity = module_ctx_integrity_from_checksum(&checksum)?;
    Ok((sha256, integrity))
}

fn module_ctx_download_result<'v>(
    success: bool,
    sha256: Option<&str>,
    integrity: Option<&str>,
    error: Option<&str>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    let success = eval.heap().alloc(success);
    let mut fields = Vec::new();
    fields.push(("success", success));
    if let Some(sha256) = sha256 {
        fields.push(("sha256", eval.heap().alloc_str(sha256).to_value()));
    }
    if let Some(integrity) = integrity {
        fields.push(("integrity", eval.heap().alloc_str(integrity).to_value()));
    }
    if let Some(error) = error {
        fields.push(("error", eval.heap().alloc_str(error).to_value()));
    }
    eval.heap().alloc(AllocStruct(fields))
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
struct StarlarkPendingDownload<'v> {
    result: Value<'v>,
}

impl<'v> AllocValue<'v> for StarlarkPendingDownload<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

impl<'v> Freeze for StarlarkPendingDownload<'v> {
    type Frozen = FrozenStarlarkPendingDownload;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkPendingDownload {
            result: self.result.freeze(freezer)?,
        })
    }
}

impl<'v> Display for StarlarkPendingDownload<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<pending download>")
    }
}

#[starlark_value(type = "pending_download")]
impl<'v> StarlarkValue<'v> for StarlarkPendingDownload<'v> {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(pending_download_methods)
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct FrozenStarlarkPendingDownload {
    result: FrozenValue,
}

impl Display for FrozenStarlarkPendingDownload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<pending download>")
    }
}

starlark_simple_value!(FrozenStarlarkPendingDownload);

#[starlark_value(type = "pending_download")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkPendingDownload {
    type Canonical = StarlarkPendingDownload<'v>;

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(pending_download_methods)
    }
}

#[starlark_module]
fn pending_download_methods(builder: &mut MethodsBuilder) {
    fn wait<'v>(
        this: ValueTypedComplex<'v, StarlarkPendingDownload<'v>>,
    ) -> starlark::Result<Value<'v>> {
        Ok(match this.unpack() {
            either::Either::Left(download) => download.result,
            either::Either::Right(download) => download.result.to_value(),
        })
    }
}

fn module_ctx_pending_download<'v>(
    block: bool,
    result: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    if block {
        result
    } else {
        eval.heap().alloc(StarlarkPendingDownload { result })
    }
}

fn module_ctx_download_error_with_block<'v>(
    block: bool,
    allow_fail: bool,
    error: buck2_error::Error,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let result = module_ctx_download_error(allow_fail, error, eval)?;
    Ok(module_ctx_pending_download(block, result, eval))
}

fn module_ctx_download_error<'v>(
    allow_fail: bool,
    error: buck2_error::Error,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    if allow_fail {
        Ok(module_ctx_download_result(
            false,
            None,
            None,
            Some(&error.to_string()),
            eval,
        ))
    } else {
        Err(error.into())
    }
}

fn module_ctx_working_dir<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
) -> &'v str {
    match this.unpack() {
        either::Either::Left(ctx) => &ctx.working_dir,
        either::Either::Right(ctx) => &ctx.working_dir,
    }
}

#[starlark_module]
fn module_extension_context_methods(builder: &mut MethodsBuilder) {
    fn is_dev_dependency<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        tag: Value<'v>,
    ) -> starlark::Result<bool> {
        let _unused = this;
        bazel_module_tag_dev_dependency(tag)
    }

    fn tag_sort_key<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        tag: Value<'v>,
    ) -> starlark::Result<i32> {
        let _unused = this;
        bazel_module_tag_sort_key(tag)
    }

    fn getenv<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] name: &str,
        #[starlark(require = pos, default = NoneOr::None)] default: NoneOr<StringValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneOr<StringValue<'v>>> {
        let _unused = this;
        match env::var(name) {
            Ok(value) => Ok(NoneOr::Other(eval.heap().alloc_str(&value))),
            Err(env::VarError::NotPresent) => Ok(default),
            Err(env::VarError::NotUnicode(value)) => Ok(NoneOr::Other(
                eval.heap().alloc_str(&value.to_string_lossy()),
            )),
        }
    }

    fn path<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkRepositoryPath> {
        let (path, dep) = repository_path_and_dep_from_value_relative_to(
            path,
            eval,
            Some(module_ctx_working_dir(this)),
        )?;
        if let Some(dep) = dep.clone() {
            module_ctx_record_path_dep(this, dep);
        }
        Ok(StarlarkRepositoryPath::new_with_dep(path, dep))
    }

    fn watch<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let _path = module_ctx_path_from_value_relative_to(this, path, eval)?;
        Ok(NoneType)
    }

    fn report_progress<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] message: &str,
    ) -> starlark::Result<NoneType> {
        let _unused = (this, message);
        Ok(NoneType)
    }

    fn execute<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] arguments: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        environment: UnpackDictEntries<&'v str, &'v str>,
        #[starlark(require = named, default = 600)] timeout: i32,
        #[starlark(require = named, default = true)] quiet: bool,
        #[starlark(require = named)] working_directory: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let _unused = (timeout, quiet);
        let repository_working_dir = module_ctx_working_dir(this).to_owned();
        let mut arguments = arguments
            .items
            .into_iter()
            .map(|arg| repository_ctx_command_arg(arg, &repository_working_dir, eval))
            .collect::<starlark::Result<Vec<_>>>()?;
        if arguments.is_empty() {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::RepositoryCtxExecuteEmptyArguments,
            )
            .into());
        }
        let program = arguments.remove(0);
        let repository_working_dir_abs = repository_path_for_write(&repository_working_dir)?;
        let environment = environment
            .entries
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    repository_ctx_command_env(value, &repository_working_dir),
                )
            })
            .collect::<Vec<_>>();
        repository_ctx_validate_external_inputs_ready(
            std::iter::once(program.clone()).chain(arguments.iter().cloned()),
            &repository_working_dir_abs,
            &program,
            |dep| module_ctx_record_path_dep(this, dep),
        )?;
        let mut command = Command::new(&program);
        command.args(arguments);
        for (key, value) in environment {
            command.env(key, value);
        }
        let working_directory = match working_directory {
            Some(working_directory) => {
                module_ctx_path_from_value_relative_to(this, working_directory, eval)?
            }
            None => repository_working_dir.clone(),
        };
        let working_directory = if working_directory == repository_working_dir {
            repository_working_dir_abs
        } else {
            repository_path_for_write(&working_directory)?
        };
        fs::create_dir_all(&working_directory).map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryCtxExecuteFailed {
                program: program.clone(),
                error: error.to_string(),
            })
        })?;
        command.current_dir(working_directory);
        let output = command.output().map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::RepositoryCtxExecuteFailed {
                program: program.clone(),
                error: error.to_string(),
            })
        })?;
        Ok(eval.heap().alloc(AllocStruct([
            (
                "stdout",
                eval.heap()
                    .alloc(String::from_utf8_lossy(&output.stdout).into_owned()),
            ),
            (
                "stderr",
                eval.heap()
                    .alloc(String::from_utf8_lossy(&output.stderr).into_owned()),
            ),
            (
                "return_code",
                eval.heap().alloc(output.status.code().unwrap_or(1)),
            ),
        ])))
    }

    fn read<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = named, default = "auto")] _watch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let path = module_ctx_path_from_value_relative_to(this, path, eval)?;
        let read_path = repository_path_for_read(&path);
        let bytes = fs::read(&read_path).map_err(|e| {
            buck2_error::Error::from(BazelRepositoryError::ModuleCtxReadFile {
                path: path.clone(),
                error: e.to_string(),
            })
        })?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn download<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = named)] url: Value<'v>,
        #[starlark(require = named)] output: Option<Value<'v>>,
        #[starlark(require = named, default = "")] sha256: &str,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named, default = false)] allow_fail: bool,
        #[starlark(require = named, default = "")] canonical_id: &str,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        auth: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        headers: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if !repository_ctx_download_options_are_empty(&auth) {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "auth" },
            )
            .into());
        }
        if !repository_ctx_download_options_are_empty(&headers) {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "headers" },
            )
            .into());
        }

        let urls = module_ctx_urls_from_value(url, eval.heap())?;
        let output = output.unwrap_or_else(|| eval.heap().alloc(""));
        let output_path = repository_path_from_value_relative_to(
            output,
            eval,
            Some(module_ctx_working_dir(this)),
        )?;
        let write_path = match repository_path_for_write(&output_path) {
            Ok(path) => path,
            Err(error) => {
                return module_ctx_download_error_with_block(block, allow_fail, error, eval);
            }
        };
        let expected_checksum = match module_ctx_expected_checksum(sha256, integrity) {
            Ok(expected_checksum) => expected_checksum,
            Err(error) => {
                return module_ctx_download_error_with_block(block, allow_fail, error, eval);
            }
        };

        let (got_sha256, got_integrity) = match module_ctx_download_to_path_blocking(
            &urls,
            &write_path,
            expected_checksum.as_ref(),
            canonical_id,
            executable,
        ) {
            Ok(checksums) => checksums,
            Err(error) => {
                return module_ctx_download_error_with_block(block, allow_fail, error, eval);
            }
        };

        let result = module_ctx_download_result(
            true,
            got_sha256.as_deref(),
            Some(&got_integrity),
            None,
            eval,
        );
        Ok(module_ctx_pending_download(block, result, eval))
    }

    fn extension_metadata<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = named, default = false)] reproducible: bool,
        #[starlark(require = named, default = NoneOr::None)] root_module_direct_deps: NoneOr<
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] root_module_direct_dev_deps: NoneOr<
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] facts: NoneOr<Value<'v>>,
    ) -> starlark::Result<StarlarkModuleExtensionMetadata> {
        let _unused = this;
        let _unused = root_module_direct_deps;
        let _unused = root_module_direct_dev_deps;
        let _unused = facts;
        Ok(StarlarkModuleExtensionMetadata { reproducible })
    }
}

#[starlark_module]
#[starlark_types(
    StarlarkRepositoryRule<'_> as RepositoryRule,
    StarlarkTagClass as TagClass,
    StarlarkModuleExtension<'_> as ModuleExtension
)]
pub(crate) fn register_bazel_repository_globals(builder: &mut GlobalsBuilder) {
    fn repository_rule<'v>(
        implementation: Option<StarlarkCallable<'v, (Value<'v>,), Value<'v>>>,
        #[starlark(require = named)] attrs: Option<
            UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        >,
        #[starlark(require = named, default = false)] local: bool,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        environ: UnpackListOrTuple<String>,
        #[starlark(require = named, default = false)] configure: bool,
        #[starlark(require = named, default = false)] remotable: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkRepositoryRule<'v>> {
        let implementation = implementation.ok_or_else(|| {
            buck2_error::Error::from(BazelRepositoryError::MissingRepositoryRuleImplementation)
        })?;
        Ok(StarlarkRepositoryRule::new(
            implementation,
            attrs,
            local,
            configure,
            remotable,
            environ,
            doc,
            eval,
        )?)
    }

    fn tag_class<'v>(
        #[starlark(require = named)] attrs: Option<
            UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        >,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
    ) -> starlark::Result<StarlarkTagClass> {
        Ok(StarlarkTagClass::new(attrs, doc)?)
    }

    fn module_extension<'v>(
        implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
        #[starlark(require = named, default = SmallMap::new())] tag_classes: SmallMap<
            String,
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        environ: UnpackListOrTuple<String>,
        #[starlark(require = named, default = false)] os_dependent: bool,
        #[starlark(require = named, default = false)] arch_dependent: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkModuleExtension<'v>> {
        Ok(StarlarkModuleExtension::new(
            implementation,
            tag_classes,
            doc,
            environ,
            os_dependent,
            arch_dependent,
            eval,
        )?)
    }
}
