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
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::Hashed;
use starlark::collections::SmallMap;
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
use starlark::values::ValueIdentity;
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

#[derive(Default)]
struct BazelDepsetHashCache<'v> {
    hashes: SmallMap<ValueIdentity<'v>, Option<StarlarkHashValue>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Allocative)]
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
    #[freeze(identity)]
    hash: StarlarkHashValue,
    to_list_cache: BazelDepsetToListCache,
    artifact_inputs_cache: BazelDepsetArtifactInputsCache,
    _marker: PhantomData<&'v ()>,
}

starlark_complex_value!(pub BazelDepset<'v>);

static NEXT_BAZEL_DEPSET_HASH: AtomicU32 = AtomicU32::new(1);

fn next_bazel_depset_hash() -> StarlarkHashValue {
    let id = NEXT_BAZEL_DEPSET_HASH.fetch_add(1, Ordering::Relaxed);
    StarlarkHashValue::new(&id)
}

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
        hash_cache: &mut BazelDepsetHashCache<'v>,
    ) -> starlark::Result<()> {
        match self.order {
            BazelDepsetOrder::Default | BazelDepsetOrder::Postorder => {
                self.collect_transitive(values, seen_values, seen_depsets, hash_cache)?;
                self.collect_direct(values, seen_values, hash_cache)?;
            }
            BazelDepsetOrder::Preorder => {
                self.collect_direct(values, seen_values, hash_cache)?;
                self.collect_transitive(values, seen_values, seen_depsets, hash_cache)?;
            }
            BazelDepsetOrder::Topological => {
                self.collect_topological(values, seen_values, seen_depsets, hash_cache)?;
            }
        }
        Ok(())
    }

    fn collect_topological(
        &self,
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen_values: &mut SmallSet<Value<'v>>,
        seen_depsets: &mut SmallSet<Value<'v>>,
        hash_cache: &mut BazelDepsetHashCache<'v>,
    ) -> starlark::Result<()> {
        let start = values.len();
        self.collect_topological_internal(values, seen_values, seen_depsets, hash_cache)?;
        values[start..].reverse();
        Ok(())
    }

    fn collect_topological_internal(
        &self,
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen_values: &mut SmallSet<Value<'v>>,
        seen_depsets: &mut SmallSet<Value<'v>>,
        hash_cache: &mut BazelDepsetHashCache<'v>,
    ) -> starlark::Result<()> {
        for transitive in self.transitive.iter().rev() {
            let transitive = transitive.to_value();
            if seen_depsets.insert_hashed(bazel_depset_identity_hash(transitive)) {
                let transitive = depset_from_value(transitive)?;
                if transitive.order() == BazelDepsetOrder::Topological {
                    transitive.collect_topological_internal(
                        values,
                        seen_values,
                        seen_depsets,
                        hash_cache,
                    )?;
                } else if let Some(cached) = transitive.to_list_cache.values.get() {
                    Self::collect_hashed_values(
                        values,
                        seen_values,
                        cached.iter().map(|value| BazelDepsetCollectedValue {
                            hash: value.hash,
                            value: value.value.to_value(),
                        }),
                    )?;
                } else {
                    transitive.collect_to_list(values, seen_values, seen_depsets, hash_cache)?;
                }
            }
        }

        self.collect_direct_reversed(values, seen_values, hash_cache)
    }

    fn collect_transitive(
        &self,
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen_values: &mut SmallSet<Value<'v>>,
        seen_depsets: &mut SmallSet<Value<'v>>,
        hash_cache: &mut BazelDepsetHashCache<'v>,
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
                    transitive.collect_to_list(values, seen_values, seen_depsets, hash_cache)?;
                }
            }
        }
        Ok(())
    }

    fn collect_direct(
        &self,
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen: &mut SmallSet<Value<'v>>,
        hash_cache: &mut BazelDepsetHashCache<'v>,
    ) -> starlark::Result<()> {
        Self::collect_values(
            values,
            seen,
            self.direct.iter().map(|value| value.to_value()),
            hash_cache,
        )
    }

    fn collect_direct_reversed(
        &self,
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen: &mut SmallSet<Value<'v>>,
        hash_cache: &mut BazelDepsetHashCache<'v>,
    ) -> starlark::Result<()> {
        Self::collect_values(
            values,
            seen,
            self.direct.iter().rev().map(|value| value.to_value()),
            hash_cache,
        )
    }

    fn collect_values(
        values: &mut Vec<BazelDepsetCollectedValue<'v>>,
        seen: &mut SmallSet<Value<'v>>,
        iter: impl IntoIterator<Item = Value<'v>>,
        hash_cache: &mut BazelDepsetHashCache<'v>,
    ) -> starlark::Result<()> {
        for value in iter {
            let hash = bazel_depset_hash(value, hash_cache)?;
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

        if self.transitive.is_empty() {
            return Ok(self.direct.iter().map(|value| value.to_value()).collect());
        }

        if self.direct.is_empty() && self.transitive.len() == 1 {
            return depset_from_value(self.transitive[0].to_value())?.to_list_cached();
        }

        let mut values = Vec::new();
        let mut seen_values = SmallSet::new();
        let mut seen_depsets = SmallSet::new();
        let mut hash_cache = BazelDepsetHashCache::default();
        self.collect_to_list(
            &mut values,
            &mut seen_values,
            &mut seen_depsets,
            &mut hash_cache,
        )?;
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
        self.direct.is_empty() && self.transitive.is_empty()
    }

    fn count_unique_values_up_to(
        &self,
        count: &mut usize,
        seen_values: &mut SmallSet<Value<'v>>,
        seen_depsets: &mut SmallSet<Value<'v>>,
        hash_cache: &mut BazelDepsetHashCache<'v>,
        limit: usize,
    ) -> starlark::Result<()> {
        for value in &self.direct {
            let value = value.to_value();
            if seen_values.insert_hashed(Hashed::new_unchecked(
                bazel_depset_hash(value, hash_cache)?,
                value,
            )) {
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
                    hash_cache,
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

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.hash.get().hash(hasher);
        Ok(())
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

fn bazel_depset_hash<'v>(
    value: Value<'v>,
    cache: &mut BazelDepsetHashCache<'v>,
) -> starlark::Result<StarlarkHashValue> {
    let identity = value.identity();
    if let Some(hash) = cache.hashes.get(&identity) {
        return Ok(hash.unwrap_or_else(|| bazel_depset_identity_hash_value(value)));
    }

    cache.hashes.insert(identity, None);
    let hash = bazel_depset_hash_uncached(value, cache)?;
    cache.hashes.insert(identity, Some(hash));
    Ok(hash)
}

fn bazel_depset_hash_uncached<'v>(
    value: Value<'v>,
    cache: &mut BazelDepsetHashCache<'v>,
) -> starlark::Result<StarlarkHashValue> {
    if BazelDepset::from_value(value).is_some() {
        return Ok(bazel_depset_identity_hash_value(value));
    }

    let mut hasher = StarlarkHasher::new();
    bazel_depset_write_hash(value, &mut hasher, cache)?;
    Ok(hasher.finish_small())
}

fn bazel_depset_write_hash<'v>(
    value: Value<'v>,
    hasher: &mut StarlarkHasher,
    cache: &mut BazelDepsetHashCache<'v>,
) -> starlark::Result<()> {
    if let Some(list) = ListRef::from_value(value) {
        "bazel_depset_list".hash(hasher);
        list.content().len().hash(hasher);
        for item in list.iter() {
            bazel_depset_hash(item, cache)?.get().hash(hasher);
        }
        return Ok(());
    }

    if let Some(tuple) = TupleRef::from_value(value) {
        "bazel_depset_tuple".hash(hasher);
        tuple.content().len().hash(hasher);
        for item in tuple.iter() {
            bazel_depset_hash(item, cache)?.get().hash(hasher);
        }
        return Ok(());
    }

    if let Some(dict) = DictRef::from_value(value) {
        "bazel_depset_dict".hash(hasher);
        let mut entries = Vec::new();
        for (key, value) in dict.iter() {
            entries.push((
                bazel_depset_hash(key, cache)?.get(),
                bazel_depset_hash(value, cache)?.get(),
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
            entries.push((name.as_str(), bazel_depset_hash(value, cache)?.get()));
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
            entries.push((name, bazel_depset_hash(value, cache)?.get()));
        }
        entries.sort_unstable();
        entries.len().hash(hasher);
        entries.hash(hasher);
        return Ok(());
    }

    if let Ok(hash) = value.get_hashed() {
        hash.hash().get().hash(hasher);
        return Ok(());
    }

    "bazel_depset_identity".hash(hasher);
    bazel_depset_identity_hash_value(value).get().hash(hasher);
    Ok(())
}

fn bazel_depset_identity_hash_value<'v>(value: Value<'v>) -> StarlarkHashValue {
    if let Some(depset) = BazelDepset::from_value(value) {
        return depset.hash;
    }
    StarlarkHashValue::new(&value.identity())
}

pub fn bazel_depset_to_list<'v>(value: Value<'v>) -> starlark::Result<Vec<Value<'v>>> {
    depset_from_value(value)?.to_list_cached()
}

pub fn bazel_depset_is_singleton<'v>(value: Value<'v>) -> starlark::Result<bool> {
    let mut count = 0;
    let mut seen_values = SmallSet::new();
    let mut seen_depsets = SmallSet::new();
    seen_depsets.insert_hashed(bazel_depset_identity_hash(value));
    let mut hash_cache = BazelDepsetHashCache::default();
    depset_from_value(value)?.count_unique_values_up_to(
        &mut count,
        &mut seen_values,
        &mut seen_depsets,
        &mut hash_cache,
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
    let mut deduped = Vec::with_capacity(direct.len());
    let mut seen = SmallSet::new();
    let mut hash_cache = BazelDepsetHashCache::default();

    for value in direct {
        check_element_type(&mut element_type, value)?;
        let hash = bazel_depset_hash(value, &mut hash_cache)?;
        if seen.insert_hashed(Hashed::new_unchecked(hash, value)) {
            deduped.push(value);
        }
    }

    Ok(BazelDepset {
        direct: deduped.into_boxed_slice(),
        transitive: Vec::new().into_boxed_slice(),
        order: BazelDepsetOrder::Default,
        element_type,
        hash: next_bazel_depset_hash(),
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
    let mut non_empty_transitive = Vec::with_capacity(transitive.len());

    for value in transitive {
        let transitive_depset = depset_from_value(value)?;
        if transitive_depset.is_empty() {
            continue;
        }
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
        non_empty_transitive.push(value);
    }

    depset.transitive = non_empty_transitive.into_boxed_slice();
    depset.order = order;
    depset.element_type = element_type;

    Ok(depset)
}

fn bazel_depset_from_direct_and_transitive_value<'v>(
    heap: Heap<'v>,
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
    order: BazelDepsetOrder,
) -> starlark::Result<Value<'v>> {
    let depset = bazel_depset_from_direct_and_transitive_values(direct, transitive, order)?;

    if depset.direct.is_empty()
        && depset.transitive.len() == 1
        && depset_from_value(depset.transitive[0].to_value())?.order() == order
    {
        return Ok(depset.transitive[0].to_value());
    }

    Ok(heap.alloc(depset))
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
    bazel_depset_from_direct_and_transitive_value(
        heap,
        direct,
        transitive,
        BazelDepsetOrder::Default,
    )
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
        hash: next_bazel_depset_hash(),
        to_list_cache: BazelDepsetToListCache::default(),
        artifact_inputs_cache: BazelDepsetArtifactInputsCache::default(),
        _marker: PhantomData,
    })
}

pub(crate) fn bazel_depset_from_frozen_values(
    heap: &FrozenHeap,
    direct: Vec<FrozenValue>,
) -> FrozenValue {
    heap.alloc(FrozenBazelDepset {
        direct: direct.into_boxed_slice(),
        transitive: Vec::new().into_boxed_slice(),
        order: BazelDepsetOrder::Default,
        element_type: None,
        hash: next_bazel_depset_hash(),
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

#[starlark_module]
pub fn register_bazel_depset(builder: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenBazelDepset)]
    fn depset<'v>(
        #[starlark(default = NoneOr::None)] direct: NoneOr<UnpackListOrTuple<Value<'v>>>,
        #[starlark(default = "default")] order: &str,
        #[starlark(require = named, default = NoneOr::None)] transitive: NoneOr<
            UnpackListOrTuple<Value<'v>>,
        >,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let order = BazelDepsetOrder::parse(order)?;
        let direct = direct.into_option().unwrap_or_default().items;
        let transitive = transitive.into_option().unwrap_or_default().items;
        bazel_depset_from_direct_and_transitive_value(heap, direct, transitive, order)
    }
}

#[cfg(test)]
mod tests {
    use starlark::assert::Assert;

    use crate::interpreter::rule_defs::depset::register_bazel_depset;

    fn depset_assert() -> Assert<'static> {
        let mut a = Assert::new();
        a.globals_add(register_bazel_depset);
        a
    }

    #[test]
    fn topological_order_matches_bazel_for_default_child() {
        let mut a = depset_assert();
        a.pass(
            r#"
s = depset([3, 4, 5], transitive = [depset([2, 4, 6])], order = "topological")
assert_eq(s.to_list(), [3, 5, 6, 4, 2])
            "#,
        );
    }

    #[test]
    fn topological_order_matches_bazel_for_diamond() {
        let mut a = depset_assert();
        a.pass(
            r#"
d = depset(["d"], order = "topological")
c = depset(["c"], transitive = [d], order = "topological")
b = depset(["b"], transitive = [d], order = "topological")
a = depset(["a"], transitive = [b, c], order = "topological")
assert_eq(a.to_list(), ["a", "b", "c", "d"])
            "#,
        );
    }

    #[test]
    fn topological_order_matches_bazel_for_ijar_link_shape() {
        let mut a = depset_assert();
        a.pass(
            r#"
strings = depset(["strings"], order = "topological")
port = depset(["port"], order = "topological")
logging = depset(["logging"], transitive = [strings], order = "topological")
errors = depset(["errors"], transitive = [logging, port, strings], order = "topological")
filesystem = depset(["filesystem"], transitive = [errors, logging, strings], order = "topological")
platform_utils = depset(["platform_utils"], transitive = [errors, filesystem, logging], order = "topological")
zlib = depset(["zlib"], order = "topological")
zlib_client = depset(["zlib_client"], transitive = [zlib], order = "topological")
zip = depset(["zip"], transitive = [platform_utils, zlib_client], order = "topological")
zipper = depset(["zipper"], transitive = [zip], order = "topological")
assert_eq(
    zipper.to_list(),
    [
        "zipper",
        "zip",
        "platform_utils",
        "filesystem",
        "errors",
        "port",
        "logging",
        "strings",
        "zlib_client",
        "zlib",
    ],
)
            "#,
        );
    }
}
