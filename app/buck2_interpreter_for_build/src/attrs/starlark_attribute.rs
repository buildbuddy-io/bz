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
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::starlark_value;
use starlark::values::typing::StarlarkCallable;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum StarlarkAttributeError {
    #[error("`attrs.default_only()` cannot be used in nested attributes")]
    DefaultOnlyInNested,
    #[error("Bazel computed default must be a Starlark function with known parameters, got `{0}`")]
    UnsupportedBazelComputedDefault(String),
}

#[derive(Debug, Allocative, Trace, Clone)]
#[allocative(bound = "")]
#[repr(C)]
pub struct BazelComputedDefaultGen<'v, V: ValueLike<'v>> {
    callback: V,
    dependencies: Vec<String>,
    _marker: std::marker::PhantomData<&'v ()>,
}

pub type BazelComputedDefault<'v> = BazelComputedDefaultGen<'v, Value<'v>>;
pub type FrozenBazelComputedDefault =
    BazelComputedDefaultGen<'static, starlark::values::FrozenValue>;

unsafe impl<'v, FromV, ToV> Coerce<BazelComputedDefaultGen<'v, ToV>>
    for BazelComputedDefaultGen<'v, FromV>
where
    FromV: ValueLifetimeless + ValueLike<'v> + Coerce<ToV>,
    ToV: ValueLifetimeless + ValueLike<'v>,
{
}

impl<'v> Freeze for BazelComputedDefault<'v> {
    type Frozen = FrozenBazelComputedDefault;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenBazelComputedDefault {
            callback: self.callback.freeze(freezer)?,
            dependencies: self.dependencies,
            _marker: std::marker::PhantomData,
        })
    }
}

impl<'v> BazelComputedDefault<'v> {
    pub(crate) fn from_value(value: Value<'v>) -> buck2_error::Result<Option<Self>> {
        if <StarlarkCallable<'v> as starlark::values::UnpackValue<'v>>::unpack_value_opt(value)
            .is_none()
        {
            return Ok(None);
        }
        let Some(parameters) = value.parameters_spec() else {
            return Err(
                StarlarkAttributeError::UnsupportedBazelComputedDefault(value.to_repr()).into(),
            );
        };
        Ok(Some(BazelComputedDefault {
            callback: value,
            dependencies: parameters
                .parameter_names()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            _marker: std::marker::PhantomData,
        }))
    }
}

impl<'v, V: ValueLike<'v>> BazelComputedDefaultGen<'v, V> {
    pub(crate) fn callback(&self) -> V
    where
        V: Copy,
    {
        self.callback
    }

    pub(crate) fn dependencies(&self) -> &[String] {
        &self.dependencies
    }
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
    bazel_computed_default: Vec<BazelComputedDefaultGen<'v, V>>,
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
            bazel_computed_default: self
                .bazel_computed_default
                .into_iter()
                .map(|computed_default| computed_default.freeze(freezer))
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
            bazel_computed_default: Vec::new(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn new_bazel(attr: Attribute) -> Self {
        Self::new_bazel_with_aspects(attr, Vec::new())
    }

    pub fn new_bazel_with_aspects(attr: Attribute, bazel_aspects: Vec<Value<'v>>) -> Self {
        Self::new_bazel_with_aspects_and_computed(attr, bazel_aspects, None)
    }

    pub fn new_bazel_with_aspects_and_computed(
        attr: Attribute,
        bazel_aspects: Vec<Value<'v>>,
        bazel_computed_default: Option<BazelComputedDefault<'v>>,
    ) -> Self {
        Self {
            attr,
            bazel_output_kind: None,
            is_bazel: true,
            bazel_aspects,
            bazel_computed_default: bazel_computed_default.into_iter().collect(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn new_bazel_output(attr: Attribute, kind: BazelOutputAttrKind) -> Self {
        Self {
            attr,
            bazel_output_kind: Some(kind),
            is_bazel: true,
            bazel_aspects: Vec::new(),
            bazel_computed_default: Vec::new(),
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

    pub fn bazel_computed_default(&self) -> Option<&BazelComputedDefaultGen<'v, V>> {
        self.bazel_computed_default.first()
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
