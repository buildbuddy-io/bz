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
use buck2_core::cells::CellResolver;
use buck2_core::cells::name::CellName;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::visibility::VisibilityPattern;
use buck2_node::visibility::VisibilitySpecification;
use buck2_node::visibility::VisibilityWithinViewBuilder;
use buck2_node::visibility::WithinViewSpecification;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneType;

use crate::bazel_visibility::NormalizedVisibilityPattern;
use crate::bazel_visibility::add_visibility_pattern;
use crate::bazel_visibility::normalize_visibility_pattern;
use crate::interpreter::build_context::BuildContext;
use crate::interpreter::build_context::PerFileTypeContext;
use crate::interpreter::module_internals::ModuleInternals;
use crate::super_package::eval_ctx::PackageFileVisibilityFields;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum PackageFileError {
    #[error("`package()` function can be used at most once per `PACKAGE` file")]
    AtMostOnce,
    #[error("`package()` argument `{0}` is only supported in BUILD files")]
    BuildFileOnlyArg(&'static str),
    #[error("`package()` argument `{0}` is only supported in PACKAGE files")]
    PackageFileOnlyArg(&'static str),
    #[error("at least one argument must be given to the 'package' function")]
    NoArguments,
}

fn parse_visibility(
    patterns: &[String],
    cell_name: CellName,
    cell_resolver: &CellResolver,
    cell_alias_resolver: &CellAliasResolver,
) -> buck2_error::Result<VisibilitySpecification> {
    let mut builder = VisibilityWithinViewBuilder::with_capacity(patterns.len());
    for pattern in patterns {
        match normalize_visibility_pattern(pattern) {
            NormalizedVisibilityPattern::Public => builder.add_public(),
            NormalizedVisibilityPattern::Private => {}
            NormalizedVisibilityPattern::Pattern(pattern) => {
                builder.add(VisibilityPattern(ParsedPattern::parse_precise(
                    &pattern,
                    cell_name,
                    cell_resolver,
                    cell_alias_resolver,
                )?));
            }
        }
    }
    Ok(builder.build_visibility())
}

fn parse_within_view(
    patterns: &[String],
    cell_name: CellName,
    cell_resolver: &CellResolver,
    cell_alias_resolver: &CellAliasResolver,
) -> buck2_error::Result<WithinViewSpecification> {
    let mut builder = VisibilityWithinViewBuilder::with_capacity(patterns.len());
    for pattern in patterns {
        match normalize_visibility_pattern(pattern) {
            NormalizedVisibilityPattern::Public => builder.add_public(),
            NormalizedVisibilityPattern::Private => {}
            NormalizedVisibilityPattern::Pattern(pattern) => {
                builder.add(VisibilityPattern(ParsedPattern::parse_precise(
                    &pattern,
                    cell_name,
                    cell_resolver,
                    cell_alias_resolver,
                )?));
            }
        }
    }
    Ok(builder.build_within_view())
}

fn parse_build_default_visibility(
    patterns: &[String],
    internals: &ModuleInternals,
) -> buck2_error::Result<VisibilitySpecification> {
    let mut builder = VisibilityWithinViewBuilder::with_capacity(patterns.len());
    for pattern in patterns {
        add_visibility_pattern(
            &mut builder,
            internals.attr_coercion_context() as &dyn AttrCoercionContext,
            pattern,
        )?;
    }
    Ok(builder.build_visibility())
}

/// Globals for `PACKAGE` files and `bzl` files included from `PACKAGE` files.
#[starlark_module]
pub(crate) fn register_package_function(globals: &mut GlobalsBuilder) {
    /// DO NOT USE THIS FUNCTION!
    ///
    /// It controls which test config to use in downstream systems. Mostly likely you don't want to specify it by yourself.
    fn test_config_unification_rollout(
        enabled: bool,
        eval: &mut Evaluator,
    ) -> starlark::Result<NoneType> {
        let build_context = BuildContext::from_context(eval)?;
        let package_file_eval_ctx = build_context.additional.require_package_file("package")?;
        *package_file_eval_ctx
            .test_config_unification_rollout
            .borrow_mut() = Some(enabled);
        Ok(NoneType)
    }

    fn package(
        #[starlark(require=named)] inherit: Option<bool>,
        #[starlark(require=named)] visibility: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] within_view: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] default_visibility: Option<UnpackListOrTuple<String>>,
        eval: &mut Evaluator,
    ) -> starlark::Result<NoneType> {
        let build_context = BuildContext::from_context(eval)?;
        match &build_context.additional {
            PerFileTypeContext::Package(package_file_eval_ctx) => {
                if default_visibility.is_some() {
                    return Err(buck2_error::Error::from(PackageFileError::BuildFileOnlyArg(
                        "default_visibility",
                    ))
                    .into());
                }

                let visibility = visibility.unwrap_or_default();
                let within_view = within_view.unwrap_or_default();
                let inherit = inherit.unwrap_or(false);
                let visibility = parse_visibility(
                    &visibility.items,
                    build_context.cell_info().name().name(),
                    build_context.cell_info().cell_resolver(),
                    build_context.cell_info().cell_alias_resolver(),
                )?;
                let within_view = parse_within_view(
                    &within_view.items,
                    build_context.cell_info().name().name(),
                    build_context.cell_info().cell_resolver(),
                    build_context.cell_info().cell_alias_resolver(),
                )?;

                match &mut *package_file_eval_ctx.visibility.borrow_mut() {
                    Some(_) => {
                        return Err(buck2_error::Error::from(PackageFileError::AtMostOnce).into());
                    }
                    x => {
                        *x = Some(PackageFileVisibilityFields {
                            visibility,
                            within_view,
                            inherit,
                        })
                    }
                };
            }
            PerFileTypeContext::Build(internals) => {
                if inherit.is_some() {
                    return Err(
                        buck2_error::Error::from(PackageFileError::PackageFileOnlyArg("inherit"))
                            .into(),
                    );
                }
                if visibility.is_some() {
                    return Err(
                        buck2_error::Error::from(PackageFileError::PackageFileOnlyArg(
                            "visibility",
                        ))
                        .into(),
                    );
                }
                if within_view.is_some() {
                    return Err(
                        buck2_error::Error::from(PackageFileError::PackageFileOnlyArg(
                            "within_view",
                        ))
                        .into(),
                    );
                }

                let Some(default_visibility) = default_visibility else {
                    return Err(buck2_error::Error::from(PackageFileError::NoArguments).into());
                };
                let default_visibility =
                    parse_build_default_visibility(&default_visibility.items, internals)?;
                internals.set_bazel_package_default_visibility(default_visibility)?;
            }
            _ => {
                build_context.additional.require_build("package")?;
            }
        }

        Ok(NoneType)
    }
}
