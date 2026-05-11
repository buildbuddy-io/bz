/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::Hashed;
use starlark::collections::SmallSet;
use starlark::collections::StarlarkHashValue;
use starlark::collections::StarlarkHasher;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::values::Demand;
use starlark::values::Freeze;
use starlark::values::FrozenHeap;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;
use starlark::values::type_repr::StarlarkTypeRepr;

use crate::interpreter::rule_defs::cmd_args::ArtifactPathMapper;
use crate::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use crate::interpreter::rule_defs::cmd_args::CommandLineArtifactVisitor;
use crate::interpreter::rule_defs::cmd_args::CommandLineBuilder;
use crate::interpreter::rule_defs::cmd_args::CommandLineContext;
use crate::interpreter::rule_defs::cmd_args::WriteToFileMacroVisitor;
use crate::interpreter::rule_defs::cmd_args::command_line_arg_like_type::command_line_arg_like_impl;
use crate::interpreter::rule_defs::cmd_args::value_as::ValueAsCommandLineLike;
use crate::interpreter::rule_defs::provider::ValueAsProviderLike;

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

fn command_line_depset_error(error: starlark::Error) -> buck2_error::Error {
    buck2_error::buck2_error!(buck2_error::ErrorTag::Input, "{}", error)
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

    fn collect_to_list(
        &self,
        values: &mut Vec<Value<'v>>,
        seen_values: &mut SmallSet<Value<'v>>,
        seen_depsets: &mut SmallSet<Value<'v>>,
    ) -> starlark::Result<()> {
        match self.order {
            BazelDepsetOrder::Default | BazelDepsetOrder::Postorder => {
                self.collect_transitive(values, seen_values, seen_depsets)?;
                self.collect_direct(values, seen_values)?;
            }
            BazelDepsetOrder::Preorder | BazelDepsetOrder::Topological => {
                self.collect_direct(values, seen_values)?;
                self.collect_transitive(values, seen_values, seen_depsets)?;
            }
        }
        Ok(())
    }

    fn collect_transitive(
        &self,
        values: &mut Vec<Value<'v>>,
        seen_values: &mut SmallSet<Value<'v>>,
        seen_depsets: &mut SmallSet<Value<'v>>,
    ) -> starlark::Result<()> {
        for transitive in &self.transitive {
            let transitive = transitive.to_value();
            if seen_depsets.insert_hashed(bazel_depset_identity_hash(transitive)) {
                depset_from_value(transitive)?.collect_to_list(
                    values,
                    seen_values,
                    seen_depsets,
                )?;
            }
        }
        Ok(())
    }

    fn collect_direct(
        &self,
        values: &mut Vec<Value<'v>>,
        seen: &mut SmallSet<Value<'v>>,
    ) -> starlark::Result<()> {
        for value in &self.direct {
            let value = value.to_value();
            if seen.insert_hashed(Hashed::new_unchecked(bazel_depset_hash(value)?, value)) {
                values.push(value);
            }
        }
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.direct.is_empty()
            && self.transitive.iter().all(|transitive| {
                depset_from_value(transitive.to_value()).map_or(false, BazelDepsetGen::is_empty)
            })
    }

    fn count_unique_values_up_to(
        &self,
        count: &mut usize,
        seen_values: &mut SmallSet<Value<'v>>,
        seen_depsets: &mut SmallSet<Value<'v>>,
        limit: usize,
    ) -> starlark::Result<()> {
        for value in &self.direct {
            let value = value.to_value();
            if seen_values.insert_hashed(Hashed::new_unchecked(bazel_depset_hash(value)?, value)) {
                *count += 1;
                if *count > limit {
                    return Ok(());
                }
            }
        }

        for transitive in &self.transitive {
            let transitive = transitive.to_value();
            if seen_depsets.insert_hashed(bazel_depset_identity_hash(transitive)) {
                depset_from_value(transitive)?.count_unique_values_up_to(
                    count,
                    seen_values,
                    seen_depsets,
                    limit,
                )?;
                if *count > limit {
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    fn for_each_command_line_value(
        &self,
        values: &mut SmallSet<Value<'v>>,
        depsets: &mut SmallSet<Value<'v>>,
        visitor: &mut dyn FnMut(Value<'v>) -> buck2_error::Result<()>,
    ) -> buck2_error::Result<()> {
        match self.order {
            BazelDepsetOrder::Default | BazelDepsetOrder::Postorder => {
                self.for_each_transitive_command_line_value(values, depsets, visitor)?;
                self.for_each_direct_command_line_value(values, visitor)?;
            }
            BazelDepsetOrder::Preorder | BazelDepsetOrder::Topological => {
                self.for_each_direct_command_line_value(values, visitor)?;
                self.for_each_transitive_command_line_value(values, depsets, visitor)?;
            }
        }
        Ok(())
    }

    fn for_each_transitive_command_line_value(
        &self,
        values: &mut SmallSet<Value<'v>>,
        depsets: &mut SmallSet<Value<'v>>,
        visitor: &mut dyn FnMut(Value<'v>) -> buck2_error::Result<()>,
    ) -> buck2_error::Result<()> {
        for transitive in &self.transitive {
            let transitive = transitive.to_value();
            if depsets.insert_hashed(bazel_depset_identity_hash(transitive)) {
                depset_from_value(transitive)
                    .map_err(command_line_depset_error)?
                    .for_each_command_line_value(values, depsets, visitor)?;
            }
        }
        Ok(())
    }

    fn for_each_direct_command_line_value(
        &self,
        seen: &mut SmallSet<Value<'v>>,
        visitor: &mut dyn FnMut(Value<'v>) -> buck2_error::Result<()>,
    ) -> buck2_error::Result<()> {
        for value in &self.direct {
            let value = value.to_value();
            let hash = bazel_depset_hash(value).map_err(command_line_depset_error)?;
            if seen.insert_hashed(Hashed::new_unchecked(hash, value)) {
                visitor(value)?;
            }
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

impl<'v, V: ValueLike<'v>> CommandLineArgLike<'v> for BazelDepsetGen<'v, V> {
    fn register_me(&self) {
        command_line_arg_like_impl!(BazelDepset::starlark_type_repr());
    }

    fn add_to_command_line(
        &self,
        cli: &mut dyn CommandLineBuilder,
        context: &mut dyn CommandLineContext,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        let mut seen_values = SmallSet::new();
        let mut seen_depsets = SmallSet::new();
        self.for_each_command_line_value(&mut seen_values, &mut seen_depsets, &mut |value| {
            ValueAsCommandLineLike::unpack_value_err(value)?
                .0
                .add_to_command_line(cli, context, artifact_path_mapping)
        })
    }

    fn add_to_command_line_expanding_directories(
        &self,
        cli: &mut dyn CommandLineBuilder,
        context: &mut dyn CommandLineContext,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        let mut seen_values = SmallSet::new();
        let mut seen_depsets = SmallSet::new();
        self.for_each_command_line_value(&mut seen_values, &mut seen_depsets, &mut |value| {
            ValueAsCommandLineLike::unpack_value_err(value)?
                .0
                .add_to_command_line_expanding_directories(cli, context, artifact_path_mapping)
        })
    }

    fn visit_artifacts(
        &self,
        visitor: &mut dyn CommandLineArtifactVisitor<'v>,
    ) -> buck2_error::Result<()> {
        let mut seen_values = SmallSet::new();
        let mut seen_depsets = SmallSet::new();
        self.for_each_command_line_value(&mut seen_values, &mut seen_depsets, &mut |value| {
            ValueAsCommandLineLike::unpack_value_err(value)?
                .0
                .visit_artifacts(visitor)
        })
    }

    fn contains_arg_attr(&self) -> bool {
        let mut seen_values = SmallSet::new();
        let mut seen_depsets = SmallSet::new();
        let mut contains = false;
        let _ignored =
            self.for_each_command_line_value(&mut seen_values, &mut seen_depsets, &mut |value| {
                if ValueAsCommandLineLike::unpack_value_err(value)?
                    .0
                    .contains_arg_attr()
                {
                    contains = true;
                }
                Ok(())
            });
        contains
    }

    fn visit_write_to_file_macros(
        &self,
        visitor: &mut dyn WriteToFileMacroVisitor,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        let mut seen_values = SmallSet::new();
        let mut seen_depsets = SmallSet::new();
        self.for_each_command_line_value(&mut seen_values, &mut seen_depsets, &mut |value| {
            ValueAsCommandLineLike::unpack_value_err(value)?
                .0
                .visit_write_to_file_macros(visitor, artifact_path_mapping)
        })
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

    fn to_bool(&self) -> bool {
        !self.is_empty()
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn CommandLineArgLike<'v>>(self);
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

fn bazel_depset_identity_hash<'v>(value: Value<'v>) -> Hashed<Value<'v>> {
    Hashed::new_unchecked(StarlarkHashValue::new(&value.identity()), value)
}

fn bazel_depset_hash<'v>(value: Value<'v>) -> starlark::Result<StarlarkHashValue> {
    if let Ok(hash) = value.get_hashed() {
        return Ok(hash.hash());
    }

    let mut hasher = StarlarkHasher::new();
    bazel_depset_write_hash(value, &mut hasher)?;
    Ok(hasher.finish_small())
}

fn bazel_depset_write_hash<'v>(
    value: Value<'v>,
    hasher: &mut StarlarkHasher,
) -> starlark::Result<()> {
    if let Some(list) = ListRef::from_value(value) {
        "bazel_depset_list".hash(hasher);
        list.content().len().hash(hasher);
        for item in list.iter() {
            bazel_depset_hash(item)?.get().hash(hasher);
        }
        return Ok(());
    }

    if let Some(tuple) = TupleRef::from_value(value) {
        "bazel_depset_tuple".hash(hasher);
        tuple.content().len().hash(hasher);
        for item in tuple.iter() {
            bazel_depset_hash(item)?.get().hash(hasher);
        }
        return Ok(());
    }

    if let Some(dict) = DictRef::from_value(value) {
        "bazel_depset_dict".hash(hasher);
        let mut entries = Vec::new();
        for (key, value) in dict.iter() {
            entries.push((
                bazel_depset_hash(key)?.get(),
                bazel_depset_hash(value)?.get(),
            ));
        }
        entries.sort_unstable();
        entries.len().hash(hasher);
        entries.hash(hasher);
        return Ok(());
    }

    if let Some(st) = StructRef::from_value(value) {
        "bazel_depset_struct".hash(hasher);
        let mut entries = Vec::new();
        for (name, value) in st.iter() {
            entries.push((name.as_str(), bazel_depset_hash(value)?.get()));
        }
        entries.sort_unstable();
        entries.len().hash(hasher);
        entries.hash(hasher);
        return Ok(());
    }

    if let Some(provider) = ValueAsProviderLike::unpack(value) {
        "bazel_depset_provider".hash(hasher);
        provider.0.id().hash(hasher);
        let mut entries = Vec::new();
        for (name, value) in provider.0.items() {
            entries.push((name, bazel_depset_hash(value)?.get()));
        }
        entries.sort_unstable();
        entries.len().hash(hasher);
        entries.hash(hasher);
        return Ok(());
    }

    "bazel_depset_identity".hash(hasher);
    value.identity().hash(hasher);
    Ok(())
}

pub fn bazel_depset_to_list<'v>(value: Value<'v>) -> starlark::Result<Vec<Value<'v>>> {
    let mut values = Vec::new();
    let mut seen_values = SmallSet::new();
    let mut seen_depsets = SmallSet::new();
    seen_depsets.insert_hashed(bazel_depset_identity_hash(value));
    depset_from_value(value)?.collect_to_list(&mut values, &mut seen_values, &mut seen_depsets)?;
    Ok(values)
}

pub fn bazel_depset_is_singleton<'v>(value: Value<'v>) -> starlark::Result<bool> {
    let mut count = 0;
    let mut seen_values = SmallSet::new();
    let mut seen_depsets = SmallSet::new();
    seen_depsets.insert_hashed(bazel_depset_identity_hash(value));
    depset_from_value(value)?.count_unique_values_up_to(
        &mut count,
        &mut seen_values,
        &mut seen_depsets,
        1,
    )?;
    Ok(count == 1)
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

pub(crate) fn bazel_depset_from_direct<'v>(
    direct: Vec<Value<'v>>,
) -> starlark::Result<BazelDepset<'v>> {
    let mut element_type = None;

    for value in &direct {
        check_element_type(&mut element_type, *value)?;
    }

    Ok(BazelDepset {
        direct: direct.into_boxed_slice(),
        transitive: Vec::new().into_boxed_slice(),
        order: BazelDepsetOrder::Default,
        element_type,
        _marker: PhantomData,
    })
}

pub(crate) fn bazel_depset_from_values<'v>(
    heap: Heap<'v>,
    direct: Vec<Value<'v>>,
) -> starlark::Result<Value<'v>> {
    Ok(heap.alloc(bazel_depset_from_direct(direct)?))
}

pub(crate) fn bazel_depset_empty<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(bazel_depset_from_direct(Vec::new()).unwrap())
}

pub(crate) fn bazel_depset_empty_frozen(heap: &FrozenHeap) -> FrozenValue {
    heap.alloc(FrozenBazelDepset {
        direct: Vec::new().into_boxed_slice(),
        transitive: Vec::new().into_boxed_slice(),
        order: BazelDepsetOrder::Default,
        element_type: None,
        _marker: PhantomData,
    })
}

#[starlark_module]
fn bazel_depset_methods(builder: &mut MethodsBuilder) {
    fn to_list<'v>(this: &BazelDepset<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let mut values = Vec::new();
        let mut seen_values = SmallSet::new();
        let mut seen_depsets = SmallSet::new();
        this.collect_to_list(&mut values, &mut seen_values, &mut seen_depsets)?;
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
        let mut depset = bazel_depset_from_direct(direct)?;
        let mut element_type = depset.element_type.clone();

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

        depset.transitive = transitive.into_boxed_slice();
        depset.order = order;
        depset.element_type = element_type;

        Ok(depset)
    }
}
