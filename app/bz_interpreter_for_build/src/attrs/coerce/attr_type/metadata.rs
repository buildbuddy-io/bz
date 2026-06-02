/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_error::BuckErrorContext;
use bz_error::conversion::from_any_with_tag;
use bz_error::internal_error;
use bz_interpreter::types::opaque_metadata::OpaqueMetadata;
use bz_node::attrs::attr_type::metadata::MetadataAttrType;
use bz_node::attrs::coerced_attr::CoercedAttr;
use bz_node::attrs::coercion_context::AttrCoercionContext;
use bz_node::attrs::configurable::AttrIsConfigurable;
use bz_node::metadata::key::MetadataKeyRef;
use bz_node::metadata::map::MetadataMap;
use bz_node::metadata::value::MetadataValue;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::dict::DictRef;
use starlark::values::type_repr::StarlarkTypeRepr;
use starlark_map::small_map::SmallMap;

use crate::attrs::coerce::AttrTypeCoerce;
use crate::attrs::coerce::attr_type::ty_maybe_select::TyMaybeSelect;

impl AttrTypeCoerce for MetadataAttrType {
    fn coerce_item(
        &self,
        configurable: AttrIsConfigurable,
        _ctx: &dyn AttrCoercionContext,
        value: Value,
    ) -> bz_error::Result<CoercedAttr> {
        if configurable == AttrIsConfigurable::Yes {
            return Err(internal_error!("Metadata attribute is not configurable"));
        }

        let dict = DictRef::unpack_value_err(value)?;

        let mut map = SmallMap::with_capacity(dict.len());
        for (key, value) in dict.iter() {
            let key = MetadataKeyRef::new(key.unpack_str_err()?)?;

            let value = value
                .to_json_value()
                .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Tier0))
                .with_buck_error_context(|| {
                    format!(
                        "Metadata attribute with key {} is not convertible to JSON: {}",
                        key.to_owned(),
                        value.to_repr(),
                    )
                })?;

            map.insert(key.to_owned(), MetadataValue::new(value));
        }

        Ok(CoercedAttr::Metadata(MetadataMap::new(map)))
    }

    fn starlark_type(&self) -> TyMaybeSelect {
        TyMaybeSelect::Basic(OpaqueMetadata::starlark_type_repr())
    }
}
