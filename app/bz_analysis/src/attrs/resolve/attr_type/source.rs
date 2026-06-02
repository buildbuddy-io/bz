/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_artifact::artifact::source_artifact::SourceArtifact;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use bz_core::package::source_path::SourcePath;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_node::attrs::attr_type::source::SourceAttrType;
use bz_node::attrs::coerced_path::CoercedPath;
use starlark::values::Value;

use crate::attrs::resolve::ctx::AttrResolutionContext;

#[derive(bz_error::Error, Debug)]
#[buck2(tag = Input)]
enum SourceLabelResolutionError {
    #[error("Expected a single artifact from {0}, but it returned {1} artifacts")]
    ExpectedSingleValue(String, usize),
}

pub(crate) trait SourceAttrTypeExt {
    fn resolve_single_file<'v>(
        ctx: &mut dyn AttrResolutionContext<'v>,
        path: SourcePath,
        source_is_directory: bool,
    ) -> Value<'v> {
        ctx.heap().alloc(StarlarkArtifact::new_source(
            SourceArtifact::new(path).into(),
            source_is_directory,
        ))
    }

    fn source_is_directory(path: &CoercedPath) -> bool {
        matches!(path, CoercedPath::Directory(_))
    }

    fn resolve_label<'v>(
        ctx: &mut dyn AttrResolutionContext<'v>,
        label: &ConfiguredProvidersLabel,
    ) -> bz_error::Result<Vec<Value<'v>>> {
        let dep = ctx.get_dep(label)?;
        dep.default_info()?.default_output_values()
    }

    fn resolve_single_label<'v>(
        ctx: &mut dyn AttrResolutionContext<'v>,
        value: &ConfiguredProvidersLabel,
    ) -> bz_error::Result<Value<'v>> {
        let mut resolved = Self::resolve_label(ctx, value)?;
        if resolved.len() == 1 {
            Ok(resolved.pop().unwrap())
        } else {
            Err(
                SourceLabelResolutionError::ExpectedSingleValue(value.to_string(), resolved.len())
                    .into(),
            )
        }
    }
}

impl SourceAttrTypeExt for SourceAttrType {}
