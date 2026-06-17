/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use bz_interpreter::types::target_label::StarlarkTargetLabel;
use bz_node::attrs::attr_type::string::StringAttrType;
use bz_node::attrs::attr_type::string::StringLiteral;
use bz_node::attrs::coerced_attr::CoercedAttr;
use bz_node::attrs::coercion_context::AttrCoercionContext;
use bz_node::attrs::configurable::AttrIsConfigurable;
use starlark::typing::Ty;
use starlark::values::Value;

use crate::attrs::coerce::AttrTypeCoerce;
use crate::attrs::coerce::attr_type::ty_maybe_select::TyMaybeSelect;

impl AttrTypeCoerce for StringAttrType {
    fn coerce_item(
        &self,
        _configurable: AttrIsConfigurable,
        ctx: &dyn AttrCoercionContext,
        value: Value,
    ) -> bz_error::Result<CoercedAttr> {
        // Bazel models label-shaped-but-no-dependency attributes (NODEP_LABEL, e.g.
        // the `toolchain` rule's `toolchain` attr) as strings, and accepts a `Label`
        // object there in addition to a string. Mirror that: if the value is a Label,
        // store its canonical string form (which round-trips for bz's own resolution).
        let label_string;
        let s = if let Some(s) = value.unpack_str() {
            s
        } else if let Some(label) = StarlarkProvidersLabel::from_value(value) {
            label_string = label.label().to_string();
            &label_string
        } else if let Some(label) = StarlarkTargetLabel::from_value(value) {
            label_string = label.label().to_string();
            &label_string
        } else {
            value.unpack_str_err()?
        };
        Ok(CoercedAttr::String(StringLiteral(ctx.intern_str(s))))
    }

    fn starlark_type(&self) -> TyMaybeSelect {
        TyMaybeSelect::Basic(Ty::string())
    }
}
