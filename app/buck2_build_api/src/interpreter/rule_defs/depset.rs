/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;
use std::marker::PhantomData;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::values::Freeze;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelDepsetError {
    #[error("Invalid order: {0}")]
    InvalidOrder(String),
    #[error("Order mismatch: {transitive} != {parent}")]
    OrderMismatch {
        parent: &'static str,
        transitive: &'static str,
    },
    #[error("Expected depset in `transitive`, got `{0}`")]
    TransitiveNotDepset(String),
    #[error("depset elements must all have the same type: got `{actual}` after `{expected}`")]
    ElementTypeMismatch { expected: String, actual: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Allocative)]
pub(crate) enum BazelDepsetOrder {
    Default,
    Postorder,
    Topological,
    Preorder,
}

impl BazelDepsetOrder {
    fn parse(order: &str) -> buck2_error::Result<Self> {
        match order {
            "default" => Ok(Self::Default),
            "postorder" => Ok(Self::Postorder),
            "topological" => Ok(Self::Topological),
            "preorder" => Ok(Self::Preorder),
            _ => Err(BazelDepsetError::InvalidOrder(order.to_owned()).into()),
        }
    }

    fn is_compatible(self, other: Self) -> bool {
        self == other || self == Self::Default || other == Self::Default
    }

    fn starlark_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Postorder => "postorder",
            Self::Topological => "topological",
            Self::Preorder => "preorder",
        }
    }
}

#[derive(
    Debug,
    Clone,
    Coerce,
    Trace,
    Freeze,
    ProvidesStaticType,
    NoSerialize,
    Allocative
)]
#[repr(C)]
pub struct BazelDepsetGen<'v, V: ValueLike<'v>> {
    direct: Box<[V]>,
    transitive: Box<[V]>,
    #[freeze(identity)]
    order: BazelDepsetOrder,
    #[freeze(identity)]
    element_type: Option<String>,
    _marker: PhantomData<&'v ()>,
}

starlark_complex_value!(pub BazelDepset<'v>);

impl<'v, V: ValueLike<'v>> BazelDepsetGen<'v, V> {
    fn order(&self) -> BazelDepsetOrder {
        self.order
    }

    fn element_type(&self) -> Option<&str> {
        self.element_type.as_deref()
    }

    fn collect_to_list(&self, values: &mut Vec<Value<'v>>) -> starlark::Result<()> {
        match self.order {
            BazelDepsetOrder::Default | BazelDepsetOrder::Postorder => {
                self.collect_transitive(values)?;
                self.collect_direct(values)?;
            }
            BazelDepsetOrder::Preorder | BazelDepsetOrder::Topological => {
                self.collect_direct(values)?;
                self.collect_transitive(values)?;
            }
        }
        Ok(())
    }

    fn collect_transitive(&self, values: &mut Vec<Value<'v>>) -> starlark::Result<()> {
        for transitive in &self.transitive {
            depset_from_value(transitive.to_value())?.collect_to_list(values)?;
        }
        Ok(())
    }

    fn collect_direct(&self, values: &mut Vec<Value<'v>>) -> starlark::Result<()> {
        for value in &self.direct {
            push_unique(values, value.to_value())?;
        }
        Ok(())
    }
}

impl<'v, V: ValueLike<'v>> fmt::Display for BazelDepsetGen<'v, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("depset(")?;
        let mut first = true;
        for value in &self.direct {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            fmt::Display::fmt(value, f)?;
        }
        f.write_str(")")
    }
}

#[starlark_value(type = "depset")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for BazelDepsetGen<'v, V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_depset_methods)
    }
}

fn depset_from_value<'v>(value: Value<'v>) -> starlark::Result<&'v BazelDepset<'v>> {
    if let Some(depset) = BazelDepset::from_value(value) {
        return Ok(depset);
    }
    Err(
        buck2_error::Error::from(BazelDepsetError::TransitiveNotDepset(
            value.to_string_for_type_error(),
        ))
        .into(),
    )
}

fn push_unique<'v>(values: &mut Vec<Value<'v>>, value: Value<'v>) -> starlark::Result<()> {
    for existing in values.iter().copied() {
        if existing.equals(value)? {
            return Ok(());
        }
    }
    values.push(value);
    Ok(())
}

fn check_element_type(element_type: &mut Option<String>, value: Value) -> buck2_error::Result<()> {
    let actual = value.get_type();
    match element_type {
        Some(expected) if expected != actual => Err(BazelDepsetError::ElementTypeMismatch {
            expected: expected.clone(),
            actual: actual.to_owned(),
        }
        .into()),
        Some(_) => Ok(()),
        None => {
            *element_type = Some(actual.to_owned());
            Ok(())
        }
    }
}

#[starlark_module]
fn bazel_depset_methods(builder: &mut MethodsBuilder) {
    fn to_list<'v>(this: &BazelDepset<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let mut values = Vec::new();
        this.collect_to_list(&mut values)?;
        Ok(heap.alloc(values))
    }
}

#[starlark_module]
pub fn register_bazel_depset(builder: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenBazelDepset)]
    fn depset<'v>(
        #[starlark(default = NoneOr::None)] direct: NoneOr<UnpackListOrTuple<Value<'v>>>,
        #[starlark(default = "default")] order: &str,
        #[starlark(require = named, default = NoneOr::None)] transitive: NoneOr<
            UnpackListOrTuple<Value<'v>>,
        >,
    ) -> starlark::Result<BazelDepset<'v>> {
        let order = BazelDepsetOrder::parse(order)?;
        let direct = direct.into_option().unwrap_or_default().items;
        let transitive = transitive.into_option().unwrap_or_default().items;
        let mut element_type = None;

        for value in &direct {
            check_element_type(&mut element_type, *value)?;
        }

        for value in &transitive {
            let transitive_depset = depset_from_value(*value)?;
            let transitive_order = transitive_depset.order();
            if !order.is_compatible(transitive_order) {
                return Err(buck2_error::Error::from(BazelDepsetError::OrderMismatch {
                    parent: order.starlark_name(),
                    transitive: transitive_order.starlark_name(),
                })
                .into());
            }
            match (element_type.as_deref(), transitive_depset.element_type()) {
                (Some(expected), Some(actual)) if expected != actual => {
                    return Err(
                        buck2_error::Error::from(BazelDepsetError::ElementTypeMismatch {
                            expected: expected.to_owned(),
                            actual: actual.to_owned(),
                        })
                        .into(),
                    );
                }
                (None, Some(actual)) => element_type = Some(actual.to_owned()),
                _ => {}
            }
        }

        Ok(BazelDepset {
            direct: direct.into_boxed_slice(),
            transitive: transitive.into_boxed_slice(),
            order,
            element_type,
            _marker: PhantomData,
        })
    }
}
