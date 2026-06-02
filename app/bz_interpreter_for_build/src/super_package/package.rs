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
use bz_core::cells::name::CellName;
use bz_core::pattern::pattern::ParsedPattern;
use bz_node::attrs::coercion_context::AttrCoercionContext;
use bz_node::visibility::VisibilityPattern;
use bz_node::visibility::VisibilitySpecification;
use bz_node::visibility::VisibilityWithinViewBuilder;
use bz_node::visibility::WithinViewSpecification;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneType;

use crate::bazel::visibility::NormalizedVisibilityPattern;
use crate::bazel::visibility::add_visibility_pattern;
use crate::bazel::visibility::normalize_visibility_pattern;
use crate::interpreter::build_context::BuildContext;
use crate::interpreter::build_context::PerFileTypeContext;
use crate::interpreter::module_internals::ModuleInternals;
use crate::super_package::eval_ctx::PackageFileVisibilityFields;

#[derive(Debug, bz_error::Error)]
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
    #[error("expected one of [False, True, 0, 1] for package() argument `{0}`, got `{1}`")]
    InvalidBool(&'static str, String),
}

fn parse_visibility(
    patterns: &[String],
    cell_name: CellName,
    cell_resolver: &CellResolver,
    cell_alias_resolver: &CellAliasResolver,
) -> bz_error::Result<VisibilitySpecification> {
    let mut builder = VisibilityWithinViewBuilder::with_capacity(patterns.len());
    for pattern in patterns {
        match normalize_visibility_pattern(pattern, None) {
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
) -> bz_error::Result<WithinViewSpecification> {
    let mut builder = VisibilityWithinViewBuilder::with_capacity(patterns.len());
    for pattern in patterns {
        match normalize_visibility_pattern(pattern, None) {
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
) -> bz_error::Result<VisibilitySpecification> {
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

fn parse_bazel_bool_arg(name: &'static str, value: Value<'_>) -> bz_error::Result<bool> {
    if let Some(value) = value.unpack_bool() {
        return Ok(value);
    }
    if let Some(value) = value.unpack_i32() {
        return match value {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PackageFileError::InvalidBool(name, value.to_string()).into()),
        };
    }
    Err(PackageFileError::InvalidBool(name, value.to_repr()).into())
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

    fn package<'v>(
        #[starlark(require=named)] inherit: Option<bool>,
        #[starlark(require=named)] visibility: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] within_view: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] default_visibility: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] default_testonly: Option<Value<'v>>,
        #[starlark(require=named)] default_deprecation: Option<String>,
        #[starlark(require=named)] features: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] licenses: Option<Value<'v>>,
        #[starlark(require=named)] default_compatible_with: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] default_restricted_to: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] default_applicable_licenses: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] default_package_metadata: Option<UnpackListOrTuple<String>>,
        #[starlark(require=named)] default_hdrs_check: Option<String>,
        #[starlark(require=named)] transitive_visibility: Option<String>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let build_context = BuildContext::from_context(eval)?;
        let has_bazel_build_arg = default_visibility.is_some()
            || default_testonly.is_some()
            || default_deprecation.is_some()
            || features.is_some()
            || licenses.is_some()
            || default_compatible_with.is_some()
            || default_restricted_to.is_some()
            || default_applicable_licenses.is_some()
            || default_package_metadata.is_some()
            || default_hdrs_check.is_some()
            || transitive_visibility.is_some();
        match &build_context.additional {
            PerFileTypeContext::Package(package_file_eval_ctx) => {
                if default_visibility.is_some() {
                    return Err(bz_error::Error::from(PackageFileError::BuildFileOnlyArg(
                        "default_visibility",
                    ))
                    .into());
                }
                for name in [
                    ("default_testonly", default_testonly.is_some()),
                    ("default_deprecation", default_deprecation.is_some()),
                    ("features", features.is_some()),
                    ("licenses", licenses.is_some()),
                    ("default_compatible_with", default_compatible_with.is_some()),
                    ("default_restricted_to", default_restricted_to.is_some()),
                    (
                        "default_applicable_licenses",
                        default_applicable_licenses.is_some(),
                    ),
                    (
                        "default_package_metadata",
                        default_package_metadata.is_some(),
                    ),
                    ("default_hdrs_check", default_hdrs_check.is_some()),
                    ("transitive_visibility", transitive_visibility.is_some()),
                ] {
                    if name.1 {
                        return Err(bz_error::Error::from(PackageFileError::BuildFileOnlyArg(
                            name.0,
                        ))
                        .into());
                    }
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
                        return Err(bz_error::Error::from(PackageFileError::AtMostOnce).into());
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
                        bz_error::Error::from(PackageFileError::PackageFileOnlyArg("inherit"))
                            .into(),
                    );
                }
                if visibility.is_some() {
                    return Err(
                        bz_error::Error::from(PackageFileError::PackageFileOnlyArg(
                            "visibility",
                        ))
                        .into(),
                    );
                }
                if within_view.is_some() {
                    return Err(
                        bz_error::Error::from(PackageFileError::PackageFileOnlyArg(
                            "within_view",
                        ))
                        .into(),
                    );
                }

                if !has_bazel_build_arg {
                    return Err(bz_error::Error::from(PackageFileError::NoArguments).into());
                };
                if let Some(default_visibility) = default_visibility {
                    let default_visibility =
                        parse_build_default_visibility(&default_visibility.items, internals)?;
                    let default_testonly = default_testonly
                        .map(|value| parse_bazel_bool_arg("default_testonly", value))
                        .transpose()?;
                    internals
                        .set_bazel_package_defaults(Some(default_visibility), default_testonly)?;
                } else if let Some(default_testonly) = default_testonly {
                    let default_testonly =
                        parse_bazel_bool_arg("default_testonly", default_testonly)?;
                    internals.set_bazel_package_defaults(None, Some(default_testonly))?;
                }
            }
            _ => {
                build_context.additional.require_build("package")?;
            }
        }

        Ok(NoneType)
    }
}
