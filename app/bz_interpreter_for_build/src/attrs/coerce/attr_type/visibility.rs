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
use bz_node::attrs::attr_type::AttrType;
use bz_node::attrs::attr_type::visibility::VisibilityAttrType;
use bz_node::attrs::coerced_attr::CoercedAttr;
use bz_node::attrs::coercion_context::AttrCoercionContext;
use bz_node::attrs::configurable::AttrIsConfigurable;
use bz_node::visibility::VisibilityWithinViewBuilder;
use starlark::values::Value;

use crate::attrs::coerce::AttrTypeCoerce;
use crate::attrs::coerce::attr_type::AttrTypeExt;
use crate::attrs::coerce::attr_type::list::coerce_list;
use crate::attrs::coerce::attr_type::ty_maybe_select::TyMaybeSelect;
use crate::bazel::visibility::add_visibility_pattern;
use crate::interpreter::selector::StarlarkSelector;

#[derive(Debug, bz_error::Error)]
enum VisibilityAttrTypeCoerceError {
    #[error("Visibility attribute is not configurable (internal error)")]
    #[buck2(tag = Tier0)]
    AttrTypeNotConfigurable,
    #[error("Visibility must be a list of string, got `{0}`")]
    #[buck2(tag = Input)]
    WrongType(String),
    #[error("Visibility attribute is not configurable (i.e. cannot use `select()`): `{0}`")]
    #[buck2(tag = Input)]
    NotConfigurable(String),
}

impl AttrTypeCoerce for VisibilityAttrType {
    fn coerce_item(
        &self,
        configurable: AttrIsConfigurable,
        ctx: &dyn AttrCoercionContext,
        value: Value,
    ) -> bz_error::Result<CoercedAttr> {
        if configurable == AttrIsConfigurable::Yes {
            return Err(VisibilityAttrTypeCoerceError::AttrTypeNotConfigurable.into());
        }
        Ok(CoercedAttr::Visibility(
            parse_visibility_with_view(ctx, value)?.build_visibility(),
        ))
    }

    fn starlark_type(&self) -> TyMaybeSelect {
        AttrType::list(AttrType::string()).starlark_type()
    }
}

pub(crate) fn parse_visibility_with_view(
    ctx: &dyn AttrCoercionContext,
    attr: Value,
) -> bz_error::Result<VisibilityWithinViewBuilder> {
    let list = match coerce_list(attr) {
        Ok(list) => list,
        Err(e) => {
            if StarlarkSelector::from_value(attr).is_some() {
                return Err(VisibilityAttrTypeCoerceError::NotConfigurable(attr.to_repr()).into());
            }
            return Err(e);
        }
    };

    let mut builder = VisibilityWithinViewBuilder::with_capacity(list.len());
    for item in list {
        let item_as_label;
        let item = if let Some(item) = item.unpack_str() {
            item
        } else if let Some(label) = StarlarkProvidersLabel::from_value(*item) {
            item_as_label = label.label().to_string();
            &item_as_label
        } else if let Some(label) = StarlarkTargetLabel::from_value(*item) {
            item_as_label = label.label().to_string();
            &item_as_label
        } else {
            if StarlarkSelector::from_value(*item).is_some() {
                return Err(VisibilityAttrTypeCoerceError::NotConfigurable(attr.to_repr()).into());
            }
            return Err(VisibilityAttrTypeCoerceError::WrongType(attr.to_repr()).into());
        };

        add_visibility_pattern(&mut builder, ctx, item)?;
    }
    Ok(builder)
}
