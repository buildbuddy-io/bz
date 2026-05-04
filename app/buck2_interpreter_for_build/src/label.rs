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
use buck2_core::cells::name::CellName;
use buck2_core::configuration::data::ConfigurationData;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::package::PackageLabel;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern_type::ProvidersPatternExtra;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::target::label::label::TargetLabel;
use buck2_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use buck2_interpreter::types::target_label::StarlarkTargetLabel;
use dupe::Dupe;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;

use crate::interpreter::build_context::BuildContext;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum LabelCreatorError {
    #[error("Expected provider, found something else: `{0}`")]
    ExpectedProvider(String),
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
    let aliases = bzlmod_cell_aliases_for_cell(cell_name.as_str())
        .into_iter()
        .map(|(alias, destination)| {
            Ok((
                NonEmptyCellAlias::new(alias)?,
                NonEmptyCellAlias::new(destination)?,
            ))
        })
        .collect::<buck2_error::Result<Vec<_>>>()?;
    CellAliasResolver::new_for_non_root_cell(
        cell_name,
        c.cell_info()
            .cell_resolver()
            .root_cell_cell_alias_resolver(),
        aliases,
    )
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
    } else if let Some(root_label) = s.strip_prefix("@@root//") {
        format!("root//{root_label}")
    } else if let Some(canonical_label) = bazel_canonical_label_to_buck_label(s) {
        canonical_label
    } else {
        s.to_owned()
    };
    let target = match ParsedPattern::<ProvidersPatternExtra>::parse_precise(
        &label,
        label_context.cell_name,
        c.cell_info().cell_resolver(),
        &label_context.cell_alias_resolver,
    )? {
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

fn bazel_canonical_label_to_buck_label(label: &str) -> Option<String> {
    let label = label.strip_prefix("@@")?;
    let (repo_name, package_and_target) = label.split_once("//")?;
    let cell_name = if repo_name.is_empty() || repo_name == "root" {
        "root".to_owned()
    } else if repo_name == "bazel_tools" {
        "bazel_tools".to_owned()
    } else {
        bzlmod_cell_name(repo_name)
    };
    Some(format!("{cell_name}//{package_and_target}"))
}

#[starlark_module]
pub fn register_bazel_label(builder: &mut GlobalsBuilder) {
    #[allow(non_snake_case)]
    fn Label<'v>(
        s: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkProvidersLabel> {
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
