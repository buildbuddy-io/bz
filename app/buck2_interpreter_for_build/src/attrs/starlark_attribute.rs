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
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::Freeze;
use starlark::values::FreezeResult;
use starlark::values::Freezer;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLike;
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
    Allocative,
    Trace,
    Coerce
)]
#[display("<attr>")]
#[repr(C)]
pub struct StarlarkAttributeGen<'v, V: ValueLike<'v>> {
    attr: Attribute,
    bazel_output_kind: Option<BazelOutputAttrKind>,
    is_bazel: bool,
    bazel_aspects: Vec<V>,
    _marker: std::marker::PhantomData<&'v ()>,
}

starlark_complex_value!(pub StarlarkAttribute<'v>);

impl<'v> Freeze for StarlarkAttribute<'v> {
    type Frozen = FrozenStarlarkAttribute;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkAttribute {
            attr: self.attr,
            bazel_output_kind: self.bazel_output_kind,
            is_bazel: self.is_bazel,
            bazel_aspects: self
                .bazel_aspects
                .into_iter()
                .map(|aspect| aspect.freeze(freezer))
                .collect::<FreezeResult<Vec<_>>>()?,
            _marker: std::marker::PhantomData,
        })
    }
}

/// Type of the attribute object returned by methods under [`attrs`](../attrs) namespace, e. g. `attrs.string()`.
#[starlark_module]
fn starlark_attribute_methods(builder: &mut MethodsBuilder) {}

#[starlark_value(type = "Attr")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkAttributeGen<'v, V>
where
    Self: ProvidesStaticType<'v>,
{
    // Used to add type documentation to the generated documentation
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(starlark_attribute_methods)
    }
}

impl<'v> StarlarkAttribute<'v> {
    pub fn new(attr: Attribute) -> Self {
        Self {
            attr,
            bazel_output_kind: None,
            is_bazel: false,
            bazel_aspects: Vec::new(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn new_bazel(attr: Attribute) -> Self {
        Self::new_bazel_with_aspects(attr, Vec::new())
    }

    pub fn new_bazel_with_aspects(attr: Attribute, bazel_aspects: Vec<Value<'v>>) -> Self {
        Self {
            attr,
            bazel_output_kind: None,
            is_bazel: true,
            bazel_aspects,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn new_bazel_output(attr: Attribute, kind: BazelOutputAttrKind) -> Self {
        Self {
            attr,
            bazel_output_kind: Some(kind),
            is_bazel: true,
            bazel_aspects: Vec::new(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'v, V: ValueLike<'v>> StarlarkAttributeGen<'v, V> {
    pub fn clone_attribute(&self) -> Attribute {
        self.attr.clone()
    }

    pub fn bazel_output_kind(&self) -> Option<BazelOutputAttrKind> {
        self.bazel_output_kind.dupe()
    }

    pub fn is_bazel(&self) -> bool {
        self.is_bazel
    }

    pub fn bazel_aspects(&self) -> &[V] {
        &self.bazel_aspects
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
#[starlark_types(StarlarkAttribute<'_> as Attr)]
pub(crate) fn register_attr_type(globals: &mut GlobalsBuilder) {}
