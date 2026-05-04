/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;

use allocative::Allocative;
use buck2_node::attrs::attr::Attribute;
use buck2_node::attrs::attr_type::AttrType;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::rule::BazelOutputAttrKind;
use dupe::Dupe;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::starlark_value;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum StarlarkAttributeError {
    #[error("`attrs.default_only()` cannot be used in nested attributes")]
    DefaultOnlyInNested,
}

#[derive(
    derive_more::Display,
    Debug,
    ProvidesStaticType,
    NoSerialize,
    Allocative
)]
#[display("<attr>")]
pub struct StarlarkAttribute {
    attr: Attribute,
    bazel_output_kind: Option<BazelOutputAttrKind>,
    is_bazel: bool,
}

starlark_simple_value!(StarlarkAttribute);

/// Type of the attribute object returned by methods under [`attrs`](../attrs) namespace, e. g. `attrs.string()`.
#[starlark_module]
fn starlark_attribute_methods(builder: &mut MethodsBuilder) {}

#[starlark_value(type = "Attr")]
impl<'v> StarlarkValue<'v> for StarlarkAttribute {
    // Used to add type documentation to the generated documentation
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(starlark_attribute_methods)
    }
}

impl StarlarkAttribute {
    pub fn new(attr: Attribute) -> Self {
        Self {
            attr,
            bazel_output_kind: None,
            is_bazel: false,
        }
    }

    pub fn new_bazel(attr: Attribute) -> Self {
        Self {
            attr,
            bazel_output_kind: None,
            is_bazel: true,
        }
    }

    pub fn new_bazel_output(attr: Attribute, kind: BazelOutputAttrKind) -> Self {
        Self {
            attr,
            bazel_output_kind: Some(kind),
            is_bazel: true,
        }
    }

    pub fn clone_attribute(&self) -> Attribute {
        self.attr.clone()
    }

    pub fn bazel_output_kind(&self) -> Option<BazelOutputAttrKind> {
        self.bazel_output_kind.dupe()
    }

    pub fn is_bazel(&self) -> bool {
        self.is_bazel
    }

    /// Coercer to put into higher lever coercer (e. g. for `attrs.list(xxx)`).
    pub fn coercer_for_inner(&self) -> buck2_error::Result<AttrType> {
        if self.attr.is_default_only() {
            return Err(StarlarkAttributeError::DefaultOnlyInNested.into());
        }
        Ok(self.attr.coercer().dupe())
    }

    pub fn coercer_for_default_only(&self) -> AttrType {
        self.attr.coercer().dupe()
    }

    pub fn default(&self) -> Option<&Arc<CoercedAttr>> {
        self.attr.default()
    }
}

#[starlark_module]
#[starlark_types(StarlarkAttribute as Attr)]
pub(crate) fn register_attr_type(globals: &mut GlobalsBuilder) {}
