/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::alias::NonEmptyCellAlias;
use buck2_core::cells::external::bzlmod_cell_aliases_for_cell;
use buck2_core::cells::external::bzlmod_cell_name;
use buck2_core::cells::external::register_bzlmod_cell_canonical_repo_name_for_cell;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePathBuf;
use buck2_core::configuration::data::ConfigurationData;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::package::PackageLabel;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern_type::ProvidersPatternExtra;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::provider::label::ProvidersName;
use buck2_core::target::label::label::TargetLabel;
use buck2_core::target::name::TargetName;
use buck2_core::target::name::TargetNameRef;
use buck2_hash::StdBuckHashMap;
use buck2_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use buck2_interpreter::types::target_label::StarlarkTargetLabel;
use dupe::Dupe;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;

use crate::bazel_label::bazel_absolute_label_parts;
use crate::bazel_label::parse_bazel_canonical_providers_label;
use crate::interpreter::build_context::BuildContext;

#[derive(Debug, buck2_error::Error)]
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
    cell_name: CellName,
    cell_alias_resolver: CellAliasResolver,
    package: Option<PackageLabel>,
}

fn is_bazel_compat_cell(cell_name: CellName) -> bool {
    let cell = cell_name.as_str();
    cell == "root" || cell == "bazel_tools" || cell.starts_with("bzlmod_")
}

fn default_label_parse_context(c: &BuildContext<'_>) -> LabelParseContext {
    LabelParseContext {
        cell_name: c.cell_info().name().name(),
        cell_alias_resolver: c.cell_info().cell_alias_resolver().dupe(),
        package: c.require_package().ok(),
    }
}

fn bzlmod_cell_alias_resolver(
    c: &BuildContext<'_>,
    cell_name: CellName,
) -> buck2_error::Result<CellAliasResolver> {
    let mut aliases = StdBuckHashMap::default();
    for alias in ["root", "prelude", "bazel_tools"] {
        let alias = NonEmptyCellAlias::new(alias.to_owned())?;
        let destination = CellName::unchecked_new(alias.as_str())?;
        c.cell_info().cell_resolver().get(destination)?;
        aliases.insert(alias, destination);
    }
    for (alias, destination) in bzlmod_cell_aliases_for_cell(cell_name.as_str()) {
        let destination = CellName::unchecked_new(&destination)?;
        aliases.insert(NonEmptyCellAlias::new(alias)?, destination);
    }
    CellAliasResolver::new(cell_name, aliases)
}

fn label_parse_context<'v>(
    c: &BuildContext<'_>,
    eval: &Evaluator<'v, '_, '_>,
) -> buck2_error::Result<LabelParseContext> {
    let default = || default_label_parse_context(c);
    let Some(location) = eval.call_stack_top_location() else {
        return Ok(default());
    };
    let Ok(project_relative_path) = ProjectRelativePath::new(location.filename()) else {
        return Ok(default());
    };
    let callsite_path = c
        .cell_info()
        .cell_resolver()
        .get_cell_path(project_relative_path);
    let cell_name = callsite_path.cell();
    let package = callsite_path
        .parent()
        .map(PackageLabel::from_cell_path)
        .transpose()?;

    let cell_alias_resolver = if cell_name == c.cell_info().name().name() {
        c.cell_info().cell_alias_resolver().dupe()
    } else if is_bazel_compat_cell(cell_name) {
        bzlmod_cell_alias_resolver(c, cell_name)?
    } else {
        c.cell_info().cell_alias_resolver().dupe()
    };

    Ok(LabelParseContext {
        cell_name,
        cell_alias_resolver,
        package,
    })
}

fn parse_providers_label<'v>(
    s: &str,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<StarlarkProvidersLabel> {
    let c = BuildContext::from_context(eval)?;
    let label_context = label_parse_context(c, eval)?;
    let label = if s.starts_with(':') {
        let package = match label_context.package {
            Some(package) => package,
            None => c.require_package()?,
        };
        format!("{}{}", package, s)
    } else if let Some(canonical_label) = parse_bazel_canonical_providers_label(s)? {
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
        c.cell_info().cell_resolver(),
        &label_context.cell_alias_resolver,
    ) {
        Ok(pattern) => pattern,
        Err(e) => {
            if let Some(label) = bazel_compat_label(s, &label_context)? {
                return Ok(StarlarkProvidersLabel::new(label));
            }
            if s != label
                && let Some(label) = bazel_compat_label(&label, &label_context)?
            {
                return Ok(StarlarkProvidersLabel::new(label));
            }
            if let Some(label) = bazel_non_visible_repo_label(&label, &label_context)? {
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
                buck2_error::Error::from(LabelCreatorError::ExpectedProvider(s.to_owned())).into(),
            );
        }
    };
    Ok(StarlarkProvidersLabel::new(target))
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
) -> buck2_error::Result<Option<ProvidersLabel>> {
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
            CellName::unchecked_new("root")?
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
                CellName::unchecked_new("root")
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
) -> buck2_error::Result<ProvidersLabel> {
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
) -> buck2_error::Result<ProvidersLabel> {
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
) -> buck2_error::Result<Option<ProvidersLabel>> {
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
                buck2_error::Error::from(LabelCreatorError::ExpectedStringOrLabel(
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
                    return Err(buck2_error::Error::from(LabelCreatorError::ExpectedTarget(
                        s.to_owned(),
                    ))
                    .into());
                }
            };
            Ok(StarlarkTargetLabel::new(target))
        }
    }
}
