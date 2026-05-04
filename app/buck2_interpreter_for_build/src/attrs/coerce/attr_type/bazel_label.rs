/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use buck2_node::attrs::attr_type::bazel_label::BazelLabelAttrType;
use buck2_node::attrs::attr_type::source::SourceAttrType;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::attrs::configurable::AttrIsConfigurable;
use starlark::typing::Ty;
use starlark::values::Value;

use crate::attrs::coerce::AttrTypeCoerce;
use crate::attrs::coerce::attr_type::AttrTypeExt;
use crate::attrs::coerce::attr_type::ty_maybe_select::TyMaybeSelect;

fn looks_like_label(value: &str) -> bool {
    value.contains(':') || value.starts_with('@') || value.starts_with("//")
}

impl AttrTypeCoerce for BazelLabelAttrType {
    fn coerce_item(
        &self,
        configurable: AttrIsConfigurable,
        ctx: &dyn AttrCoercionContext,
        value: Value,
    ) -> buck2_error::Result<CoercedAttr> {
        if let Some(value_str) = value.unpack_str() {
            if !looks_like_label(value_str) {
                let source = SourceAttrType {
                    allow_directory: false,
                };
                if let Ok(value) = source.coerce_item(configurable, ctx, value) {
                    return Ok(value);
                }
            }
        }

        match self.dep.coerce_item(configurable, ctx, value) {
            Ok(value) => Ok(value),
            Err(dep_error) => {
                let source = SourceAttrType {
                    allow_directory: false,
                };
                source
                    .coerce_item(configurable, ctx, value)
                    .map_err(|source_error| {
                        buck2_error::buck2_error!(
                            buck2_error::ErrorTag::Input,
                            "could not coerce Bazel label as dependency ({:#}) or source ({:#})",
                            dep_error,
                            source_error
                        )
                    })
            }
        }
    }

    fn starlark_type(&self) -> TyMaybeSelect {
        TyMaybeSelect::Basic(Ty::string())
    }
}
