/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_core::cells::CellAliasResolver;
use bz_core::cells::CellResolver;
use bz_core::cells::alias::NonEmptyCellAlias;
use bz_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use bz_core::cells::external::bzlmod_cell_aliases_for_cell;
use bz_core::cells::external::bzlmod_cell_name;
use bz_core::cells::external::is_bzlmod_cell_name;
use bz_core::cells::external::register_bzlmod_cell_canonical_repo_name_for_cell;
use bz_core::cells::name::CellName;
use bz_core::cells::paths::CellRelativePathBuf;
use bz_core::configuration::data::ConfigurationData;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::package::PackageLabel;
use bz_core::pattern::pattern::ParsedPattern;
use bz_core::pattern::pattern_type::ProvidersPatternExtra;
use bz_core::pattern::pattern_type::TargetPatternExtra;
use bz_core::provider::label::ProvidersLabel;
use bz_core::provider::label::ProvidersName;
use bz_core::target::label::label::TargetLabel;
use bz_core::target::name::TargetName;
use bz_core::target::name::TargetNameRef;
use bz_hash::StdBuckHashMap;
use bz_interpreter::types::bazel::label_context::StarlarkLabelResolutionContext;
use bz_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use bz_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use bz_interpreter::types::target_label::StarlarkTargetLabel;
use dupe::Dupe;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;

use crate::bazel::label::bazel_absolute_label_parts;
use crate::bazel::label::parse_bazel_canonical_providers_label;
use crate::interpreter::build_context::BazelRepositoryRecordedInput;
use crate::interpreter::build_context::BuildContext;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum LabelCreatorError {
    #[error("Expected provider, found something else: `{0}`")]
    ExpectedProvider(String),
    #[error("Expected string or label, found `{0}`")]
    ExpectedStringOrLabel(String),
    #[error("Expected target, found something else: `{0}`")]
    ExpectedTarget(String),
}

struct LabelParseContext {
    root_cell: CellName,
    cell_name: CellName,
    cell_alias_resolver: CellAliasResolver,
    package: Option<PackageLabel>,
}

struct LabelParseContextData<'a, 'e> {
    label_context: LabelParseContext,
    cell_resolver: &'a CellResolver,
    build_context: Option<&'a BuildContext<'e>>,
}

fn is_bazel_compat_cell(cell_name: CellName) -> bool {
    let cell = cell_name.as_str();
    cell == "root" || cell == "bazel_tools" || is_bzlmod_cell_name(cell)
}

fn default_label_parse_context(c: &BuildContext<'_>) -> LabelParseContext {
    LabelParseContext {
        root_cell: c.cell_info().cell_resolver().root_cell(),
        cell_name: c.cell_info().name().name(),
        cell_alias_resolver: c.cell_info().cell_alias_resolver().dupe(),
        package: c.require_package().ok(),
    }
}

fn bzlmod_cell_alias_resolver(
    cell_resolver: &CellResolver,
    cell_name: CellName,
) -> bz_error::Result<CellAliasResolver> {
    let mut aliases = StdBuckHashMap::default();
    for alias in ["root", "prelude", "bazel_tools"] {
        let alias = NonEmptyCellAlias::new(alias.to_owned())?;
        let destination = if alias.as_str() == "root" {
            cell_resolver.root_cell()
        } else {
            CellName::unchecked_new(alias.as_str())?
        };
        if cell_resolver.get(destination).is_err() {
            continue;
        }
        aliases.insert(alias, destination);
    }
    for (alias, destination) in bzlmod_cell_aliases_for_cell(cell_name.as_str()) {
        let destination = CellName::unchecked_new(&destination)?;
        aliases.insert(NonEmptyCellAlias::new(alias)?, destination);
    }
    CellAliasResolver::new(cell_name, aliases)
}

fn label_parse_context_from_callsite<'v>(
    default: LabelParseContext,
    cell_resolver: &CellResolver,
    eval: &Evaluator<'v, '_, '_>,
) -> bz_error::Result<LabelParseContext> {
    let Some(location) = eval.call_stack_top_location() else {
        return Ok(default);
    };
    let Ok(project_relative_path) = ProjectRelativePath::new(location.filename()) else {
        return Ok(default);
    };
    let callsite_path = cell_resolver.get_cell_path(project_relative_path);
    let cell_name = callsite_path.cell();
    let package = callsite_path
        .parent()
        .map(PackageLabel::from_cell_path)
        .transpose()?;

    let cell_alias_resolver = if cell_name == default.cell_name {
        default.cell_alias_resolver.dupe()
    } else if is_bazel_compat_cell(cell_name) {
        bzlmod_cell_alias_resolver(cell_resolver, cell_name)?
    } else {
        default.cell_alias_resolver.dupe()
    };

    Ok(LabelParseContext {
        root_cell: default.root_cell,
        cell_name,
        cell_alias_resolver,
        package,
    })
}

fn label_parse_context<'v>(
    c: &BuildContext<'_>,
    eval: &Evaluator<'v, '_, '_>,
) -> bz_error::Result<LabelParseContext> {
    label_parse_context_from_callsite(
        default_label_parse_context(c),
        c.cell_info().cell_resolver(),
        eval,
    )
}

fn analysis_label_parse_context<'v>(
    c: &StarlarkLabelResolutionContext,
    eval: &Evaluator<'v, '_, '_>,
) -> bz_error::Result<LabelParseContext> {
    label_parse_context_from_callsite(
        LabelParseContext {
            root_cell: c.cell_resolver.root_cell(),
            cell_name: c.cell_name,
            cell_alias_resolver: c.cell_alias_resolver.dupe(),
            package: c.package.dupe(),
        },
        &c.cell_resolver,
        eval,
    )
}

fn label_parse_context_data<'v, 'a, 'e>(
    eval: &Evaluator<'v, 'a, 'e>,
) -> bz_error::Result<LabelParseContextData<'a, 'e>> {
    if let Ok(c) = BuildContext::from_context(eval) {
        return Ok(LabelParseContextData {
            label_context: label_parse_context(c, eval)?,
            cell_resolver: c.cell_info().cell_resolver(),
            build_context: Some(c),
        });
    }

    if let Some(c) = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<StarlarkLabelResolutionContext>())
    {
        return Ok(LabelParseContextData {
            label_context: analysis_label_parse_context(c, eval)?,
            cell_resolver: &c.cell_resolver,
            build_context: None,
        });
    }

    let _ = BuildContext::from_context(eval)?;
    unreachable!("BuildContext::from_context returned Ok after an earlier failed lookup")
}

fn parse_providers_label<'v>(
    s: &str,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<StarlarkProvidersLabel> {
    let LabelParseContextData {
        label_context,
        cell_resolver,
        build_context,
    } = label_parse_context_data(eval)?;
    let apparent_repo = bazel_apparent_repo_from_label(s);
    let label = if s.starts_with(':') {
        let package = match label_context.package {
            Some(package) => package,
            None => match build_context {
                Some(c) => c.require_package()?,
                None => {
                    return Err(bz_error::Error::from(LabelCreatorError::ExpectedProvider(
                        s.to_owned(),
                    ))
                    .into());
                }
            },
        };
        format!("{}{}", package, s)
    } else if is_bare_relative_label(s) {
        // A bare relative label such as `Label("foo.bzl")`: no repo (`@`), no
        // package root (`//`), and no target separator (`:`). Bazel resolves these
        // as a target in the calling file's current package, i.e. equivalent to
        // `Label(":foo.bzl")`.
        let package = match label_context.package {
            Some(package) => package,
            None => match build_context {
                Some(c) => c.require_package()?,
                None => {
                    return Err(bz_error::Error::from(LabelCreatorError::ExpectedProvider(
                        s.to_owned(),
                    ))
                    .into());
                }
            },
        };
        format!("{}:{}", package, s)
    } else if let Some(canonical_label) =
        parse_bazel_canonical_providers_label(s, label_context.root_cell)?
    {
        return Ok(StarlarkProvidersLabel::new(canonical_label));
    } else if let Some(root_label) = s.strip_prefix("@@root//") {
        format!("root//{root_label}")
    } else if let Some(repo_label) = bazel_repo_only_label(s) {
        repo_label
    } else {
        s.to_owned()
    };
    let target = match ParsedPattern::<ProvidersPatternExtra>::parse_precise(
        &label,
        label_context.cell_name,
        cell_resolver,
        &label_context.cell_alias_resolver,
    ) {
        Ok(pattern) => pattern,
        Err(e) => {
            if let Some(label) = bazel_compat_label(s, &label_context)? {
                record_bazel_repository_repo_mapping(
                    eval,
                    &label_context,
                    apparent_repo.as_deref(),
                );
                return Ok(StarlarkProvidersLabel::new(label));
            }
            if s != label
                && let Some(label) = bazel_compat_label(&label, &label_context)?
            {
                record_bazel_repository_repo_mapping(
                    eval,
                    &label_context,
                    apparent_repo.as_deref(),
                );
                return Ok(StarlarkProvidersLabel::new(label));
            }
            if let Some(label) = bazel_non_visible_repo_label(&label, &label_context)? {
                record_bazel_repository_repo_mapping(
                    eval,
                    &label_context,
                    apparent_repo.as_deref(),
                );
                return Ok(StarlarkProvidersLabel::new(label));
            }
            return Err(e.into());
        }
    };
    let target = match target {
        ParsedPattern::Target(package, target_name, providers) => {
            providers.into_providers_label(package, target_name.as_ref())
        }
        _ => {
            return Err(
                bz_error::Error::from(LabelCreatorError::ExpectedProvider(s.to_owned())).into(),
            );
        }
    };
    record_bazel_repository_repo_mapping(eval, &label_context, apparent_repo.as_deref());
    Ok(StarlarkProvidersLabel::new(target))
}

/// Whether `s` is a bare relative label like `Label("foo.bzl")`: no repo (`@`),
/// no package root (`//`), and no target separator (`:`). Such labels resolve to
/// a target in the current package, the same as `Label(":foo.bzl")`.
fn is_bare_relative_label(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('@') && !s.contains("//") && !s.contains(':')
}

fn bazel_apparent_repo_from_label(value: &str) -> Option<String> {
    let value = value.strip_prefix('@')?;
    if value.starts_with('@') {
        return None;
    }
    let (repo, _) = value.split_once("//")?;
    Some(repo.to_owned())
}

fn bazel_repo_name_for_cell(cell_name: CellName) -> String {
    if cell_name.as_str() == "root" {
        return String::new();
    }
    bzlmod_canonical_repo_name_for_cell(cell_name.as_str())
        .unwrap_or_else(|| cell_name.as_str().to_owned())
}

fn record_bazel_repository_repo_mapping(
    eval: &Evaluator<'_, '_, '_>,
    label_context: &LabelParseContext,
    apparent_repo: Option<&str>,
) {
    let Some(apparent_name) = apparent_repo else {
        return;
    };
    let Ok(build_context) = BuildContext::from_context(eval) else {
        return;
    };
    let Some(repository_context) = &build_context.bazel_repository_context else {
        return;
    };
    let canonical_name = if apparent_name.is_empty() {
        Some(String::new())
    } else {
        label_context
            .cell_alias_resolver
            .resolve(apparent_name)
            .ok()
            .map(bazel_repo_name_for_cell)
    };
    let input = BazelRepositoryRecordedInput::RepoMapping {
        source_repo: bazel_repo_name_for_cell(label_context.cell_name),
        source_cell_name: label_context.cell_name.as_str().to_owned(),
        apparent_name: apparent_name.to_owned(),
        canonical_name,
    };
    repository_context
        .recorded_inputs
        .lock()
        .expect("repository recorded inputs poisoned")
        .insert(input);
}

fn bazel_repo_only_label(value: &str) -> Option<String> {
    if !value.starts_with('@') || value.contains("//") {
        return None;
    }
    let repo = value
        .strip_prefix("@@")
        .or_else(|| value.strip_prefix('@'))?;
    if repo.is_empty() {
        return None;
    }
    Some(format!("{value}//:__repo__"))
}

fn bazel_compat_label(
    value: &str,
    label_context: &LabelParseContext,
) -> bz_error::Result<Option<ProvidersLabel>> {
    if !is_bazel_compat_cell(label_context.cell_name) {
        return Ok(None);
    }

    if let Some(target) = value.strip_prefix(':') {
        return bazel_compat_package_label(target, value, label_context).map(Some);
    }

    if let Some(package_and_target) = value.strip_prefix("//") {
        return bazel_compat_absolute_label(
            label_context.cell_name,
            package_and_target,
            label_context,
        )
        .map(Some);
    }

    if let Some(value) = value.strip_prefix('@') {
        if value.starts_with('@') {
            return Ok(None);
        }
        let Some((repo, package_and_target)) = value.split_once("//") else {
            return Ok(None);
        };
        let cell_name = if repo.is_empty() {
            label_context.root_cell
        } else {
            match label_context.cell_alias_resolver.resolve(repo) {
                Ok(cell_name) => cell_name,
                Err(_) => return Ok(None),
            }
        };
        return bazel_compat_absolute_label(cell_name, package_and_target, label_context).map(Some);
    }

    if let Some((cell, package_and_target)) = value.split_once("//") {
        if !cell.is_empty()
            && !cell.contains(['@', '/', ':', '[', ']'])
            && let Ok(cell_name) = if cell == "root" {
                Ok(label_context.root_cell)
            } else if cell == "bazel_tools" {
                CellName::unchecked_new("bazel_tools")
            } else {
                label_context.cell_alias_resolver.resolve(cell)
            }
        {
            return bazel_compat_absolute_label(cell_name, package_and_target, label_context)
                .map(Some);
        }
    }

    Ok(None)
}

fn bazel_compat_package_label(
    target: &str,
    original: &str,
    label_context: &LabelParseContext,
) -> bz_error::Result<ProvidersLabel> {
    let package = label_context
        .package
        .dupe()
        .ok_or_else(|| LabelCreatorError::ExpectedProvider(original.to_owned()))?;
    let target = TargetName::new_bazel(target)?;
    Ok(ProvidersLabel::new(
        TargetLabel::new(package, target.as_ref()),
        ProvidersName::Default,
    ))
}

fn bazel_compat_absolute_label(
    cell_name: CellName,
    package_and_target: &str,
    _label_context: &LabelParseContext,
) -> bz_error::Result<ProvidersLabel> {
    let Some((package, target)) = bazel_absolute_label_parts(package_and_target) else {
        return Err(LabelCreatorError::ExpectedProvider(package_and_target.to_owned()).into());
    };
    let package = PackageLabel::new(cell_name, CellRelativePathBuf::try_from(package)?.as_ref())?;
    let target = TargetName::new_bazel(&target)?;
    Ok(ProvidersLabel::new(
        TargetLabel::new(package, target.as_ref()),
        ProvidersName::Default,
    ))
}

fn bazel_non_visible_repo_label(
    value: &str,
    label_context: &LabelParseContext,
) -> bz_error::Result<Option<ProvidersLabel>> {
    if !is_bazel_compat_cell(label_context.cell_name) {
        return Ok(None);
    }

    let Some(value) = value.strip_prefix('@') else {
        return Ok(None);
    };
    if value.starts_with('@') {
        return Ok(None);
    }

    let Some((repo, label)) = value.split_once("//") else {
        return Ok(None);
    };
    if repo.is_empty() || label_context.cell_alias_resolver.resolve(repo).is_ok() {
        return Ok(None);
    }

    let Some((package, target)) = bazel_absolute_label_parts(label) else {
        return Ok(None);
    };

    let canonical_repo_name = format!("unknown+{}+{}", label_context.cell_name.as_str(), repo);
    let cell_name_string = bzlmod_cell_name(&canonical_repo_name);
    register_bzlmod_cell_canonical_repo_name_for_cell(&cell_name_string, &canonical_repo_name);
    let cell_name = CellName::unchecked_new(&cell_name_string)?;
    let package = PackageLabel::new(cell_name, CellRelativePathBuf::try_from(package)?.as_ref())?;
    let target = TargetNameRef::new_bazel(&target)?;
    Ok(Some(ProvidersLabel::new(
        TargetLabel::new(package, target),
        ProvidersName::Default,
    )))
}

#[starlark_module]
pub fn register_bazel_label(builder: &mut GlobalsBuilder) {
    #[allow(non_snake_case)]
    fn Label<'v>(
        s: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkProvidersLabel> {
        if let Some(label) = StarlarkProvidersLabel::from_value(s) {
            return Ok(StarlarkProvidersLabel::new(label.label().dupe()));
        }
        let Some(s) = s.unpack_str() else {
            return Err(
                bz_error::Error::from(LabelCreatorError::ExpectedStringOrLabel(
                    s.get_type().to_owned(),
                ))
                .into(),
            );
        };
        parse_providers_label(s, eval)
    }
}

pub mod testing {
    use super::*;

    #[starlark_module]
    pub fn label_creator(builder: &mut GlobalsBuilder) {
        fn label<'v>(
            s: &str,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<StarlarkConfiguredProvidersLabel> {
            let target = parse_providers_label(s, eval)?;
            Ok(StarlarkConfiguredProvidersLabel::new(
                target.label().configure(ConfigurationData::testing_new()),
            ))
        }

        fn target_label<'v>(
            s: &str,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<StarlarkTargetLabel> {
            let c = BuildContext::from_context(eval)?;
            let target = match ParsedPattern::<TargetPatternExtra>::parse_precise(
                s,
                c.cell_info().name().name(),
                c.cell_info().cell_resolver(),
                c.cell_info().cell_alias_resolver(),
            )? {
                ParsedPattern::Target(package, target_name, TargetPatternExtra) => {
                    TargetLabel::new(package, target_name.as_ref())
                }
                _ => {
                    return Err(bz_error::Error::from(LabelCreatorError::ExpectedTarget(
                        s.to_owned(),
                    ))
                    .into());
                }
            };
            Ok(StarlarkTargetLabel::new(target))
        }
    }
}
