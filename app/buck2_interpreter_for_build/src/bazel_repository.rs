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
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;

use allocative::Allocative;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use buck2_common::dice::cells::HasCellResolver;
use buck2_core::bzl::ImportPath;
use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::alias::NonEmptyCellAlias;
use buck2_core::cells::build_file_cell::BuildFileCell;
use buck2_core::cells::external::BzlmodModuleExtensionRepoSetup;
use buck2_core::cells::external::bzlmod_cell_name;
use buck2_core::cells::name::CellName;
use buck2_core::target::label::interner::ConcurrentTargetLabelInterner;
use buck2_interpreter::load_module::InterpreterCalculation;
use buck2_interpreter::parse_import::RelativeImports;
use buck2_interpreter::parse_import::parse_import;
use buck2_interpreter::paths::module::StarlarkModulePath;
use buck2_interpreter::paths::path::OwnedStarlarkPath;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use buck2_node::attrs::attr::Attribute;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::configurable::AttrIsConfigurable;
use buck2_node::attrs::fmt_context::AttrFmtContext;
use buck2_node::bzl_or_bxl_path::BzlOrBxlPath;
use buck2_node::rule_type::StarlarkRuleType;
use derive_more::Display;
use dice::CancellationContext;
use dice::DiceComputations;
use dupe::Dupe;
use itertools::Itertools;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
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
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::typing::StarlarkCallable;
use starlark_map::small_map::SmallMap;

use crate::attrs::coerce::attr_type::AttrTypeExt;
use crate::attrs::coerce::ctx::BuildAttrCoercionContext;
use crate::attrs::starlark_attribute::StarlarkAttribute;
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
    #[error("repository_ctx.download_and_extract could not extract `{archive}`: {error}")]
    RepositoryCtxExtractArchive { archive: String, error: String },
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
    #[error("module_ctx.download(block = False) is not implemented")]
    ModuleCtxDownloadAsyncUnsupported,
    #[error("module_ctx.download `{field}` is not implemented")]
    ModuleCtxDownloadUnsupportedField { field: &'static str },
    #[error("module_ctx.download failed for {urls:?}: {error}")]
    ModuleCtxDownloadFailed { urls: Vec<String>, error: String },
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
            attrs.push((attr_name.to_owned(), attr_value.to_repr()));
        }
    }
    let name = name
        .ok_or_else(|| buck2_error::Error::from(BazelRepositoryError::RepositoryRuleMissingName))?;
    attrs.sort_by(|(left, _), (right, _)| left.cmp(right));

    recorder.record(BazelRepositoryRuleInvocation {
        rule_id: rule_id.clone(),
        name,
        attrs,
    });

    Ok(Value::new_none())
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
    modules: Vec<BzlmodModuleExtensionModuleConfig>,
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionModuleConfig {
    name: String,
    version: String,
    #[allow(dead_code)]
    canonical_repo_name: String,
    is_root: bool,
    tags: Vec<BzlmodModuleExtensionTagConfig>,
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionTagConfig {
    tag_name: String,
    dev_dependency: bool,
    kwargs: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BazelRepositoryGeneratedFile {
    pub path: String,
    pub content: String,
    pub executable: bool,
}

pub async fn evaluate_bzlmod_module_extension_repo(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
    module_ctx_working_dir: &str,
    cancellation: &CancellationContext,
) -> buck2_error::Result<Vec<BazelRepositoryRuleInvocation>> {
    let parent_cell =
        CellName::unchecked_new(&bzlmod_cell_name(&setup.parent_canonical_repo_name))?;
    let parent_alias_resolver = ctx.get_cell_alias_resolver(parent_cell).await?;
    let extension_cell_path = parse_import(
        &parent_alias_resolver,
        RelativeImports::Disallow,
        &setup.extension_bzl_file,
    )?;
    let extension_path = ImportPath::new_with_build_file_cells(
        extension_cell_path,
        BuildFileCell::new(parent_cell),
    )?;
    let extension_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(&extension_path))
        .await?;
    let mut interpreter = ctx
        .get_interpreter_calculator(OwnedStarlarkPath::LoadFile(extension_path.clone()))
        .await?;
    interpreter
        .eval_bzlmod_module_extension(
            &extension_path,
            &extension_module,
            &setup.extension_name,
            &setup.extension_usages_json,
            module_ctx_working_dir,
            cancellation,
        )
        .await
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
    let rule_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(rule_path))
        .await?;
    let mut interpreter = ctx
        .get_interpreter_calculator(OwnedStarlarkPath::LoadFile(rule_path.clone()))
        .await?;
    interpreter
        .eval_bzlmod_repository_rule(
            rule_path,
            &rule_module,
            invocation,
            repository_ctx_working_dir,
            cancellation,
        )
        .await
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
    value_name: &str,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let source = format!("{value_name} = ({expression})");
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
                        let raw_value =
                            eval_bzlmod_tag_expression(&expression, &value_name, globals, eval)?;
                        let coerced_value = attr
                            .coercer()
                            .coerce(AttrIsConfigurable::No, &attr_coercion_ctx, raw_value)
                            .map_err(starlark::Error::from)?;
                        alloc_coerced_attr_value(&coerced_value, eval)?
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

        let tags_value = eval.heap().alloc(StarlarkBazelModuleTags::new(tags));
        let module_value = eval.heap().alloc(StarlarkBazelModule::new(
            module_config.name,
            module_config.version,
            tags_value,
            module_config.is_root,
        ));
        modules.push(module_value);
    }

    Ok(eval.heap().alloc(StarlarkModuleExtensionContext::new(
        modules,
        working_dir.to_owned(),
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
    let mut attrs = Vec::new();
    for (attr_name, attr) in repository_rule.attributes.attributes() {
        let value = match explicit_attrs.remove(attr_name) {
            Some(expression) => {
                let value_name = format!("buck_repository_rule_attr_{expression_index}");
                expression_index += 1;
                eval_bzlmod_tag_expression(&expression, &value_name, globals, eval)?
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
    let attr = eval.heap().alloc(AllocStruct(attrs));
    Ok(eval.heap().alloc(StarlarkRepositoryContext::new(
        invocation.name.clone(),
        attr,
        working_dir.to_owned(),
    )))
}

#[derive(Debug, Allocative)]
struct BazelAttributeSpec {
    attributes: SmallMap<String, Attribute>,
}

impl BazelAttributeSpec {
    fn from_entries<'v>(
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute>>,
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
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute>>,
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
        let positional = [repository_ctx];
        let args = Arguments::new_positional(&positional);
        self.implementation.to_value().invoke(&args, eval)
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
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute>>,
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
        let positional = [module_ctx];
        let args = Arguments::new_positional(&positional);
        let result = self.implementation.0.invoke(&args, eval)?;
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
        let positional = [module_ctx];
        let args = Arguments::new_positional(&positional);
        let result = self.implementation.to_value().invoke(&args, eval)?;
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
    Display,
    ProvidesStaticType,
    Trace,
    Freeze,
    NoSerialize,
    Allocative
)]
#[display("{}", path)]
pub(crate) struct StarlarkRepositoryPath {
    path: String,
}

starlark_simple_value!(StarlarkRepositoryPath);

impl StarlarkRepositoryPath {
    fn new(path: String) -> Self {
        Self { path }
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
        Ok(StarlarkRepositoryPath::new(
            Path::new(&this.path)
                .parent()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ))
    }

    #[starlark(attribute)]
    fn exists(this: &StarlarkRepositoryPath) -> starlark::Result<bool> {
        Ok(Path::new(&repository_path_for_read(&this.path)).exists())
    }
}

fn repository_path_from_value_relative_to(
    value: Value<'_>,
    eval: &Evaluator<'_, '_, '_>,
    relative_root: Option<&str>,
) -> starlark::Result<String> {
    if let Some(path) = value.downcast_ref::<StarlarkRepositoryPath>() {
        return Ok(path.path.clone());
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
        return Ok(project_path.as_str().to_owned());
    }
    if let Some(path) = value.unpack_str() {
        if let Some(relative_root) = relative_root
            && !Path::new(path).is_absolute()
        {
            return Ok(Path::new(relative_root)
                .join(path)
                .to_string_lossy()
                .into_owned());
        }
        return Ok(path.to_owned());
    }
    Err(
        buck2_error::Error::from(BazelRepositoryError::ModuleCtxPathUnsupportedValue(
            value.get_type().to_owned(),
        ))
        .into(),
    )
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
    attr: Value<'v>,
    working_dir: String,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    files: Mutex<Vec<BazelRepositoryGeneratedFile>>,
}

impl<'v> StarlarkRepositoryContext<'v> {
    fn new(name: String, attr: Value<'v>, working_dir: String) -> Self {
        Self {
            name,
            attr,
            working_dir,
            files: Mutex::new(Vec::new()),
        }
    }

    fn take_files(&self) -> Vec<BazelRepositoryGeneratedFile> {
        std::mem::take(&mut *self.files.lock().expect("repository_ctx files poisoned"))
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
        vec!["attr".to_owned(), "name".to_owned(), "os".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "attr" => Some(self.attr),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "os" => Some(heap.alloc(StarlarkRepositoryOs)),
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
            attr: self.attr.freeze(freezer)?,
            working_dir: self.working_dir,
            files: Mutex::new(
                self.files
                    .into_inner()
                    .expect("repository_ctx files poisoned"),
            ),
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkRepositoryContext {
    name: String,
    attr: FrozenValue,
    working_dir: String,
    #[allocative(skip)]
    files: Mutex<Vec<BazelRepositoryGeneratedFile>>,
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
        vec!["attr".to_owned(), "name".to_owned(), "os".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "attr" => Some(self.attr.to_value()),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "os" => Some(heap.alloc(FrozenStarlarkRepositoryOs)),
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
    Ok(repository_ctx.take_files())
}

fn repository_ctx_working_dir<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
) -> &'v str {
    match this.unpack() {
        either::Either::Left(ctx) => &ctx.working_dir,
        either::Either::Right(ctx) => &ctx.working_dir,
    }
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

fn repository_ctx_download_to_path<'v>(
    urls: Vec<String>,
    output_path: String,
    sha256: &str,
    executable: bool,
    allow_fail: bool,
    integrity: &str,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<(Value<'v>, bool)> {
    let expected_sha256 = match module_ctx_sha256_from_integrity(integrity) {
        Ok(Some(integrity_sha256)) => Some(integrity_sha256),
        Ok(None) if !sha256.is_empty() => Some(sha256.to_ascii_lowercase()),
        Ok(None) => None,
        Err(error) => {
            return Ok((
                repository_ctx_download_error(allow_fail, error, eval)?,
                false,
            ));
        }
    };
    let bytes = match module_ctx_download_bytes_blocking(&urls) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok((
                repository_ctx_download_error(allow_fail, error, eval)?,
                false,
            ));
        }
    };
    let got_sha256 = module_ctx_sha256_hex(&bytes);
    if let Some(expected_sha256) = &expected_sha256
        && *expected_sha256 != got_sha256
    {
        return Ok((
            repository_ctx_download_error(
                allow_fail,
                BazelRepositoryError::ModuleCtxDownloadChecksumMismatch {
                    path: output_path.clone(),
                    expected: expected_sha256.clone(),
                    got: got_sha256,
                }
                .into(),
                eval,
            )?,
            false,
        ));
    }
    let got_integrity = match module_ctx_integrity_from_sha256_hex(&got_sha256) {
        Ok(integrity) => integrity,
        Err(error) => {
            return Ok((
                repository_ctx_download_error(allow_fail, error, eval)?,
                false,
            ));
        }
    };
    if let Err(error) = repository_ctx_write_bytes(&output_path, &bytes, executable) {
        return Ok((
            repository_ctx_download_error(allow_fail, error.into(), eval)?,
            false,
        ));
    }
    Ok((
        module_ctx_download_result(true, Some(&got_sha256), Some(&got_integrity), None, eval),
        true,
    ))
}

fn repository_ctx_extract_archive(
    archive: &Path,
    output: &Path,
    strip_prefix: &str,
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
        if !strip_prefix.is_empty() {
            command.arg(format!(
                "--strip-components={}",
                strip_prefix
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .count()
            ));
        }
        command
    };
    command.env("LC_ALL", "C").env("LANG", "C");
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
        #[starlark(require = named, default = true)] executable: bool,
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
        #[starlark(require = named, default = UnpackDictEntries::default())]
        substitutions: UnpackDictEntries<&'v str, &'v str>,
        #[starlark(require = named, default = true)] executable: bool,
        #[starlark(require = named, default = "auto")] _watch_template: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let working_dir = repository_ctx_working_dir(this);
        let path = repository_ctx_output_path_from_value(path, working_dir)?;
        let template_path =
            repository_path_from_value_relative_to(template, eval, Some(working_dir))?;
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
        Ok(StarlarkRepositoryPath::new(
            repository_path_from_value_relative_to(
                path,
                eval,
                Some(repository_ctx_working_dir(this)),
            )?,
        ))
    }

    fn read<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = named, default = "auto")] _watch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let path = repository_path_from_value_relative_to(
            path,
            eval,
            Some(repository_ctx_working_dir(this)),
        )?;
        let read_path = repository_path_for_read(&path);
        let bytes = fs::read(&read_path).map_err(|e| {
            buck2_error::Error::from(BazelRepositoryError::ModuleCtxReadFile {
                path: path.clone(),
                error: e.to_string(),
            })
        })?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
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
        let path = repository_path_from_value_relative_to(
            path,
            eval,
            Some(repository_ctx_working_dir(this)),
        )?;
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

    fn download<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = named)] url: Value<'v>,
        #[starlark(require = named)] output: Value<'v>,
        #[starlark(require = named, default = "")] sha256: &str,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named, default = false)] allow_fail: bool,
        #[starlark(require = named, default = "")] _canonical_id: &str,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        auth: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        headers: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if !block {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadAsyncUnsupported,
            )
            .into());
        }
        if !auth.entries.is_empty() {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "auth" },
            )
            .into());
        }
        if !headers.entries.is_empty() {
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
            eval,
        )?;
        Ok(result)
    }

    #[allow(non_snake_case)]
    fn download_and_extract<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = named)] url: Value<'v>,
        #[starlark(require = named, default = "")] output: Value<'v>,
        #[starlark(require = named, default = "")] sha256: &str,
        #[starlark(require = named, default = false)] allow_fail: bool,
        #[starlark(require = named, default = "")] _canonical_id: &str,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        auth: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        headers: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = true)] block: bool,
        #[starlark(require = named, default = "")] stripPrefix: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let working_dir = repository_ctx_working_dir(this);
        let archive_path = Path::new(working_dir).join(".buck2_download_and_extract.archive");
        let archive_path_string = archive_path.to_string_lossy().into_owned();
        if !block {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadAsyncUnsupported,
            )
            .into());
        }
        if !auth.entries.is_empty() {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "auth" },
            )
            .into());
        }
        if !headers.entries.is_empty() {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "headers" },
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
            eval,
        )?;
        if !success {
            return Ok(result);
        }
        let output_path = repository_path_from_value_relative_to(output, eval, Some(working_dir))?;
        let output_path = repository_path_for_write(&output_path)?;
        let archive_path = repository_path_for_write(&archive_path_string)?;
        match repository_ctx_extract_archive(&archive_path, &output_path, stripPrefix) {
            Ok(()) => Ok(result),
            Err(error) => repository_ctx_download_error(allow_fail, error, eval),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkModuleExtensionContext<'v> {
    modules: Vec<Value<'v>>,
    working_dir: String,
}

#[allow(dead_code)]
impl<'v> StarlarkModuleExtensionContext<'v> {
    pub(crate) fn new(modules: Vec<Value<'v>>, working_dir: String) -> Self {
        Self {
            modules,
            working_dir,
        }
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
        vec!["facts".to_owned(), "modules".to_owned(), "os".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "facts" => Some(empty_dict_value(heap)),
            "modules" => Some(heap.alloc(AllocList(self.modules.iter().copied()))),
            "os" => Some(heap.alloc(StarlarkRepositoryOs)),
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
        let modules = self
            .modules
            .into_iter()
            .map(|module| module.freeze(freezer))
            .collect::<FreezeResult<Vec<_>>>()?;
        Ok(FrozenStarlarkModuleExtensionContext {
            modules,
            working_dir: self.working_dir,
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkModuleExtensionContext {
    modules: Vec<FrozenValue>,
    working_dir: String,
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
        vec!["facts".to_owned(), "modules".to_owned(), "os".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "facts" => Some(empty_dict_value(heap)),
            "modules" => Some(heap.alloc(AllocList(
                self.modules.iter().map(|module| module.to_value()),
            ))),
            "os" => Some(heap.alloc(FrozenStarlarkRepositoryOs)),
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
    tags: SmallMap<String, Vec<Value<'v>>>,
}

#[allow(dead_code)]
impl<'v> StarlarkBazelModuleTags<'v> {
    pub(crate) fn new(tags: SmallMap<String, Vec<Value<'v>>>) -> Self {
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
        self.tags
            .get(attribute)
            .map(|tags| heap.alloc(AllocList(tags.iter().copied())))
    }
}

impl<'v> Freeze for StarlarkBazelModuleTags<'v> {
    type Frozen = FrozenStarlarkBazelModuleTags;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let tags = self
            .tags
            .into_iter()
            .map(|(name, values)| {
                let values = values
                    .into_iter()
                    .map(|value| value.freeze(freezer))
                    .collect::<FreezeResult<Vec<_>>>()?;
                Ok((name, values))
            })
            .collect::<FreezeResult<SmallMap<_, _>>>()?;
        Ok(FrozenStarlarkBazelModuleTags { tags })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkBazelModuleTags {
    tags: SmallMap<String, Vec<FrozenValue>>,
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
        self.tags
            .get(attribute)
            .map(|tags| heap.alloc(AllocList(tags.iter().map(|tag| tag.to_value()))))
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

async fn module_ctx_download_bytes(urls: &[String]) -> buck2_error::Result<Vec<u8>> {
    let client = buck2_http::HttpClientBuilder::oss().await?.build();
    let mut last_error = None;
    for url in urls {
        match client.get(url).await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    last_error = Some(format!("HTTP status {status}"));
                    continue;
                }
                let body = buck2_http::to_bytes(response.into_body()).await?;
                return Ok(body.to_vec());
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
    }
    Err(BazelRepositoryError::ModuleCtxDownloadFailed {
        urls: urls.to_owned(),
        error: last_error.unwrap_or_else(|| "no URL attempted".to_owned()),
    }
    .into())
}

fn module_ctx_download_bytes_blocking(urls: &[String]) -> buck2_error::Result<Vec<u8>> {
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

fn module_ctx_sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn module_ctx_integrity_from_sha256_hex(sha256: &str) -> buck2_error::Result<String> {
    let bytes = hex::decode(sha256).map_err(|_| {
        BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(sha256.to_owned())
    })?;
    Ok(format!("sha256-{}", BASE64_STANDARD.encode(bytes)))
}

fn module_ctx_sha256_from_integrity(integrity: &str) -> buck2_error::Result<Option<String>> {
    if integrity.is_empty() {
        return Ok(None);
    }
    let Some(encoded) = integrity.strip_prefix("sha256-") else {
        return Err(BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(
            integrity.to_owned(),
        )
        .into());
    };
    let bytes = BASE64_STANDARD.decode(encoded).map_err(|_| {
        BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(integrity.to_owned())
    })?;
    Ok(Some(hex::encode(bytes)))
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
        Ok(StarlarkRepositoryPath::new(
            repository_path_from_value_relative_to(path, eval, Some(module_ctx_working_dir(this)))?,
        ))
    }

    fn read<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = named, default = "auto")] _watch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let path =
            repository_path_from_value_relative_to(path, eval, Some(module_ctx_working_dir(this)))?;
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
        #[starlark(require = named, default = "")] _canonical_id: &str,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        auth: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        headers: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if !block {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadAsyncUnsupported,
            )
            .into());
        }
        if !auth.entries.is_empty() {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUnsupportedField { field: "auth" },
            )
            .into());
        }
        if !headers.entries.is_empty() {
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
        let expected_sha256 = match module_ctx_sha256_from_integrity(integrity) {
            Ok(Some(integrity_sha256)) => Some(integrity_sha256),
            Ok(None) if !sha256.is_empty() => Some(sha256.to_ascii_lowercase()),
            Ok(None) => None,
            Err(error) => return module_ctx_download_error(allow_fail, error, eval),
        };

        let bytes = match module_ctx_download_bytes_blocking(&urls) {
            Ok(bytes) => bytes,
            Err(error) => return module_ctx_download_error(allow_fail, error, eval),
        };
        let got_sha256 = module_ctx_sha256_hex(&bytes);
        if let Some(expected_sha256) = &expected_sha256
            && *expected_sha256 != got_sha256
        {
            return module_ctx_download_error(
                allow_fail,
                BazelRepositoryError::ModuleCtxDownloadChecksumMismatch {
                    path: output_path.clone(),
                    expected: expected_sha256.clone(),
                    got: got_sha256,
                }
                .into(),
                eval,
            );
        }
        let got_integrity = match module_ctx_integrity_from_sha256_hex(&got_sha256) {
            Ok(integrity) => integrity,
            Err(error) => return module_ctx_download_error(allow_fail, error, eval),
        };

        let write_path = match repository_path_for_write(&output_path) {
            Ok(path) => path,
            Err(error) => return module_ctx_download_error(allow_fail, error, eval),
        };
        if let Some(parent) = write_path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            return module_ctx_download_error(
                allow_fail,
                BazelRepositoryError::ModuleCtxDownloadWriteFile {
                    path: write_path.to_string_lossy().into_owned(),
                    error: error.to_string(),
                }
                .into(),
                eval,
            );
        }
        if let Err(error) = fs::write(&write_path, &bytes) {
            return module_ctx_download_error(
                allow_fail,
                BazelRepositoryError::ModuleCtxDownloadWriteFile {
                    path: write_path.to_string_lossy().into_owned(),
                    error: error.to_string(),
                }
                .into(),
                eval,
            );
        }
        if executable {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let executable_mode = 0o755;
                if let Err(error) =
                    fs::set_permissions(&write_path, fs::Permissions::from_mode(executable_mode))
                {
                    return module_ctx_download_error(
                        allow_fail,
                        BazelRepositoryError::ModuleCtxDownloadWriteFile {
                            path: write_path.to_string_lossy().into_owned(),
                            error: error.to_string(),
                        }
                        .into(),
                        eval,
                    );
                }
            }
        }

        Ok(module_ctx_download_result(
            true,
            Some(&got_sha256),
            Some(&got_integrity),
            None,
            eval,
        ))
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
            UnpackDictEntries<&'v str, &'v StarlarkAttribute>,
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
            UnpackDictEntries<&'v str, &'v StarlarkAttribute>,
        >,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
    ) -> starlark::Result<StarlarkTagClass> {
        Ok(StarlarkTagClass::new(attrs, doc)?)
    }

    fn module_extension<'v>(
        #[starlark(require = named)] implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
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
