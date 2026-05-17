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
use std::sync::OnceLock;

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
use starlark::values::FreezeResult;
use starlark::values::Freezer;
use starlark::values::FrozenHeap;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Tracer;
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

use crate::artifact_groups::ArtifactGroup;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use crate::interpreter::rule_defs::cmd_args::ArtifactPathMapper;
use crate::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use crate::interpreter::rule_defs::cmd_args::CommandLineArtifactVisitor;
use crate::interpreter::rule_defs::cmd_args::CommandLineBuilder;
use crate::interpreter::rule_defs::cmd_args::CommandLineContext;
use crate::interpreter::rule_defs::cmd_args::SimpleCommandLineArtifactVisitor;
use crate::interpreter::rule_defs::cmd_args::WriteToFileMacroVisitor;
use crate::interpreter::rule_defs::cmd_args::command_line_arg_like_type::command_line_arg_like_impl;
use crate::interpreter::rule_defs::cmd_args::value_as::ValueAsCommandLineLike;
use crate::interpreter::rule_defs::provider::ValueAsProviderLike;

#[derive(Debug, Clone, Copy)]
struct BazelDepsetCachedValue {
    hash: StarlarkHashValue,
    value: FrozenValue,
}

#[derive(Debug, Clone, Copy)]
struct BazelDepsetCollectedValue<'v> {
    hash: StarlarkHashValue,
    value: Value<'v>,
}

#[derive(Debug, Allocative)]
struct BazelDepsetToListCache {
    #[allocative(skip)]
    values: OnceLock<Box<[BazelDepsetCachedValue]>>,
}

impl Default for BazelDepsetToListCache {
    fn default() -> Self {
        Self {
            values: OnceLock::new(),
        }
    }
}

impl Clone for BazelDepsetToListCache {
    fn clone(&self) -> Self {
        // The cached flattening is a performance hint, not part of depset identity.
        Self::default()
    }
}

unsafe impl<'v> Trace<'v> for BazelDepsetToListCache {
    fn trace(&mut self, _tracer: &Tracer<'v>) {}
}

impl Freeze for BazelDepsetToListCache {
    type Frozen = BazelDepsetToListCache;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        // Values in the cache are redundant with `direct`/`transitive`. Dropping
        // the cache avoids freezing a second copy of a potentially large list.
        Ok(BazelDepsetToListCache::default())
    }
}

#[derive(Debug, Allocative)]
struct BazelDepsetArtifactInputsCache {
    #[allocative(skip)]
    values: OnceLock<Option<Box<[ArtifactGroup]>>>,
}

impl Default for BazelDepsetArtifactInputsCache {
    fn default() -> Self {
        Self {
            values: OnceLock::new(),
        }
    }
}

impl Clone for BazelDepsetArtifactInputsCache {
    fn clone(&self) -> Self {
        // The cached action-input projection is a performance hint, not part of depset identity.
        Self::default()
    }
}

unsafe impl<'v> Trace<'v> for BazelDepsetArtifactInputsCache {
    fn trace(&mut self, _tracer: &Tracer<'v>) {}
}

impl Freeze for BazelDepsetArtifactInputsCache {
    type Frozen = BazelDepsetArtifactInputsCache;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        // Inputs in the cache are redundant with `direct`/`transitive`. Dropping
        // the cache avoids freezing a second copy of a potentially large list.
        Ok(BazelDepsetArtifactInputsCache::default())
    }
}

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
    to_list_cache: BazelDepsetToListCache,
    artifact_inputs_cache: BazelDepsetArtifactInputsCache,
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
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
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
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen_values: &mut SmallSet<Value<'v>>,
        seen_depsets: &mut SmallSet<Value<'v>>,
    ) -> starlark::Result<()> {
        for transitive in &self.transitive {
            let transitive = transitive.to_value();
            if seen_depsets.insert_hashed(bazel_depset_identity_hash(transitive)) {
                let transitive = depset_from_value(transitive)?;
                if let Some(cached) = transitive.to_list_cache.values.get() {
                    Self::collect_hashed_values(
                        values,
                        seen_values,
                        cached.iter().map(|value| BazelDepsetCollectedValue {
                            hash: value.hash,
                            value: value.value.to_value(),
                        }),
                    )?;
                } else {
                    transitive.collect_to_list(values, seen_values, seen_depsets)?;
                }
            }
        }
        Ok(())
    }

    fn collect_direct(
        &self,
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen: &mut SmallSet<Value<'v>>,
    ) -> starlark::Result<()> {
        Self::collect_values(
            values,
            seen,
            self.direct.iter().map(|value| value.to_value()),
        )
    }

    fn collect_values(
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen: &mut SmallSet<Value<'v>>,
        iter: impl IntoIterator<Item = Value<'v>>,
    ) -> starlark::Result<()> {
        for value in iter {
            let hash = bazel_depset_hash(value)?;
            if seen.insert_hashed(Hashed::new_unchecked(hash, value)) {
                values.push(BazelDepsetCollectedValue { hash, value });
            }
        }
        Ok(())
    }

    fn collect_hashed_values(
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen: &mut SmallSet<Value<'v>>,
        iter: impl IntoIterator<Item = BazelDepsetCollectedValue<'v>>,
    ) -> starlark::Result<()> {
        for value in iter {
            if seen.insert_hashed(Hashed::new_unchecked(value.hash, value.value)) {
                values.push(value);
            }
        }
        Ok(())
    }

    fn to_list_cached(&self) -> starlark::Result<Vec<Value<'v>>> {
        if let Some(values) = self.to_list_cache.values.get() {
            return Ok(values.iter().map(|value| value.value.to_value()).collect());
        }

        let mut values = Vec::new();
        let mut seen_values = SmallSet::new();
        let mut seen_depsets = SmallSet::new();
        self.collect_to_list(&mut values, &mut seen_values, &mut seen_depsets)?;
        if let Some(frozen_values) = values
            .iter()
            .map(|value| {
                Some(BazelDepsetCachedValue {
                    hash: value.hash,
                    value: value.value.unpack_frozen()?,
                })
            })
            .collect::<Option<Vec<_>>>()
        {
            let _ignored = self
                .to_list_cache
                .values
                .set(frozen_values.into_boxed_slice());
        }
        Ok(values.into_iter().map(|value| value.value).collect())
    }

    fn collect_artifact_inputs_for_cache(
        &self,
    ) -> buck2_error::Result<Option<Box<[ArtifactGroup]>>> {
        let values = self.to_list_cached().map_err(command_line_depset_error)?;
        let mut inputs = Vec::with_capacity(values.len());

        for value in values {
            let Some(input) = ValueAsInputArtifactLike::unpack_value(value)? else {
                return Ok(None);
            };

            let mut visitor = SimpleCommandLineArtifactVisitor::new();
            input
                .0
                .as_command_line_like()
                .visit_artifacts(&mut visitor)?;
            if !visitor.declared_outputs.is_empty() || !visitor.frozen_outputs.is_empty() {
                return Ok(None);
            }
            inputs.extend(visitor.inputs.into_iter());
        }

        Ok(Some(inputs.into_boxed_slice()))
    }

    fn artifact_inputs_cached(&self) -> buck2_error::Result<Option<&[ArtifactGroup]>> {
        if self.artifact_inputs_cache.values.get().is_none() {
            let inputs = self.collect_artifact_inputs_for_cache()?;
            let _ignored = self.artifact_inputs_cache.values.set(inputs);
        }

        Ok(self
            .artifact_inputs_cache
            .values
            .get()
            .and_then(|values| values.as_deref()))
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
        for value in self.to_list_cached().map_err(command_line_depset_error)? {
            ValueAsCommandLineLike::unpack_value_err(value)?
                .0
                .add_to_command_line(cli, context, artifact_path_mapping)?;
        }
        Ok(())
    }

    fn add_to_command_line_expanding_directories(
        &self,
        cli: &mut dyn CommandLineBuilder,
        context: &mut dyn CommandLineContext,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        for value in self.to_list_cached().map_err(command_line_depset_error)? {
            ValueAsCommandLineLike::unpack_value_err(value)?
                .0
                .add_to_command_line_expanding_directories(cli, context, artifact_path_mapping)?;
        }
        Ok(())
    }

    fn visit_artifacts(
        &self,
        visitor: &mut dyn CommandLineArtifactVisitor<'v>,
    ) -> buck2_error::Result<()> {
        if let Some(inputs) = self.artifact_inputs_cached()? {
            for input in inputs {
                visitor.visit_input(input.clone(), Vec::new());
            }
            return Ok(());
        }

        for value in self.to_list_cached().map_err(command_line_depset_error)? {
            ValueAsCommandLineLike::unpack_value_err(value)?
                .0
                .visit_artifacts(visitor)?;
        }
        Ok(())
    }

    fn contains_arg_attr(&self) -> bool {
        let mut contains = false;
        let _ignored = self
            .to_list_cached()
            .map_err(command_line_depset_error)
            .and_then(|values| {
                for value in values {
                    if ValueAsCommandLineLike::unpack_value_err(value)?
                        .0
                        .contains_arg_attr()
                    {
                        contains = true;
                    }
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
        for value in self.to_list_cached().map_err(command_line_depset_error)? {
            ValueAsCommandLineLike::unpack_value_err(value)?
                .0
                .visit_write_to_file_macros(visitor, artifact_path_mapping)?;
        }
        Ok(())
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
    depset_from_value(value)?.to_list_cached()
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

pub fn bazel_depset_is_empty<'v>(value: Value<'v>) -> starlark::Result<bool> {
    Ok(depset_from_value(value)?.is_empty())
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
        to_list_cache: BazelDepsetToListCache::default(),
        artifact_inputs_cache: BazelDepsetArtifactInputsCache::default(),
        _marker: PhantomData,
    })
}

fn bazel_depset_from_direct_and_transitive_values<'v>(
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
    order: BazelDepsetOrder,
) -> starlark::Result<BazelDepset<'v>> {
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

pub(crate) fn bazel_depset_from_values<'v>(
    heap: Heap<'v>,
    direct: Vec<Value<'v>>,
) -> starlark::Result<Value<'v>> {
    Ok(heap.alloc(bazel_depset_from_direct(direct)?))
}

pub(crate) fn bazel_depset_from_direct_and_transitive<'v>(
    heap: Heap<'v>,
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
) -> starlark::Result<Value<'v>> {
    Ok(heap.alloc(bazel_depset_from_direct_and_transitive_values(
        direct,
        transitive,
        BazelDepsetOrder::Default,
    )?))
}

pub(crate) fn bazel_depset_from_direct_and_transitive_with_order<'v>(
    heap: Heap<'v>,
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
    order: BazelDepsetOrder,
) -> starlark::Result<Value<'v>> {
    Ok(heap.alloc(bazel_depset_from_direct_and_transitive_values(
        direct, transitive, order,
    )?))
}

pub(crate) fn bazel_depset_from_transitive<'v>(
    heap: Heap<'v>,
    transitive: Vec<Value<'v>>,
) -> starlark::Result<Value<'v>> {
    bazel_depset_from_direct_and_transitive(heap, Vec::new(), transitive)
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
        to_list_cache: BazelDepsetToListCache::default(),
        artifact_inputs_cache: BazelDepsetArtifactInputsCache::default(),
        _marker: PhantomData,
    })
}

#[starlark_module]
fn bazel_depset_methods(builder: &mut MethodsBuilder) {
    fn to_list<'v>(this: &BazelDepset<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(heap.alloc(this.to_list_cached()?))
    }
}

pub(crate) fn bazel_flat_depset_impl<'v>(
    heap: Heap<'v>,
    transitive: Vec<Value<'v>>,
) -> starlark::Result<Value<'v>> {
    let mut non_empty = Vec::with_capacity(transitive.len());
    for depset in transitive {
        let depset_ref = depset_from_value(depset)?;
        if !depset_ref.is_empty() {
            non_empty.push(depset);
        }
    }

    match non_empty.as_slice() {
        [] => return Ok(bazel_depset_empty(heap)),
        [depset] => {
            return Ok(*depset);
        }
        _ => {}
    }

    let first = non_empty[0];
    if non_empty
        .iter()
        .all(|depset| depset.identity() == first.identity())
    {
        return Ok(first);
    }

    let mut largest_depset = None;
    let mut largest_depset_list = Vec::new();

    for depset in &non_empty {
        let values = bazel_depset_to_list(*depset)?;
        if values.len() > largest_depset_list.len() {
            largest_depset_list = values;
            largest_depset = Some(*depset);
        }
    }

    let all = bazel_depset_from_transitive(heap, non_empty)?;
    if bazel_depset_to_list(all)? == largest_depset_list {
        Ok(largest_depset.unwrap_or_else(|| bazel_depset_empty(heap)))
    } else {
        Ok(all)
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
        bazel_depset_from_direct_and_transitive_values(direct, transitive, order)
    }

    fn __buck2_bazel_flat_depset<'v>(
        #[starlark(require = named, default = NoneOr::None)] transitive: NoneOr<
            UnpackListOrTuple<Value<'v>>,
        >,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        bazel_flat_depset_impl(heap, transitive.into_option().unwrap_or_default().items)
    }
}
