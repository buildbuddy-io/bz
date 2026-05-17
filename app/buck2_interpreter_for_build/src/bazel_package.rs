/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::tuple::UnpackTuple;

use crate::interpreter::build_context::BuildContext;
use crate::interpreter::build_context::PerFileTypeContext;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelPackageError {
    #[error("visibility() can only be used during .bzl initialization")]
    VisibilityOutsideBzl,
    #[error("Invalid visibility: got `{0}`, want string or list of strings")]
    InvalidVisibilityType(String),
    #[error("Invalid visibility list item: got `{0}`, want string")]
    InvalidVisibilityListItem(String),
    #[error("Invalid visibility specification `{0}`: the `@` repository syntax is not allowed")]
    InvalidVisibilityRepositorySyntax(String),
}

fn parse_bzl_visibility(value: Value<'_>) -> buck2_error::Result<Vec<String>> {
    fn validate(spec: &str) -> buck2_error::Result<String> {
        if spec.starts_with('@') {
            return Err(
                BazelPackageError::InvalidVisibilityRepositorySyntax(spec.to_owned()).into(),
            );
        }
        Ok(spec.to_owned())
    }

    if let Some(spec) = value.unpack_str() {
        return Ok(vec![validate(spec)?]);
    }

    let Some(specs) = ListRef::from_value(value) else {
        return Err(BazelPackageError::InvalidVisibilityType(value.get_type().to_owned()).into());
    };

    specs
        .iter()
        .map(|spec| {
            let Some(spec) = spec.unpack_str() else {
                return Err(BazelPackageError::InvalidVisibilityListItem(
                    spec.get_type().to_owned(),
                )
                .into());
            };
            validate(spec)
        })
        .collect()
}

#[starlark_module]
pub(crate) fn register_bazel_package_globals(builder: &mut GlobalsBuilder) {
    fn licenses<'v>(
        #[starlark(args)] _args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }

    fn visibility<'v>(
        #[starlark(require = pos)] value: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let build_context = BuildContext::from_context(eval)?;
        let PerFileTypeContext::Bzl(bzl) = &build_context.additional else {
            return Err(buck2_error::Error::from(BazelPackageError::VisibilityOutsideBzl).into());
        };
        bzl.set_bzl_visibility(parse_bzl_visibility(value)?)?;
        Ok(NoneType)
    }
}
