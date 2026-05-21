/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt::Debug;

use allocative::Allocative;
use buck2_build_api_derive::internal_provider;
use buck2_error::internal_error;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Freeze;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::Trace;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueError;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::dict::DictType;

use crate as buck2_build_api;
use crate::artifact_groups::ArtifactGroup;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use crate::interpreter::rule_defs::depset::bazel_depset_from_transitive;
use crate::interpreter::rule_defs::depset::bazel_depset_to_list;

/// Bazel's default top-level output groups are `default`,
/// `temp_files_INTERNAL_`, and `_hidden_top_level_INTERNAL_`.
///
/// The hidden top-level group is where Bazel puts executable runfiles trees so a
/// `bazel build //:bin` validates runfiles/data dependencies without printing
/// those artifacts as primary outputs.
pub const BAZEL_HIDDEN_TOP_LEVEL_OUTPUT_GROUP: &str = "_hidden_top_level_INTERNAL_";
pub const BAZEL_TEMP_FILES_OUTPUT_GROUP: &str = "temp_files_INTERNAL_";

#[internal_provider(
    output_group_info_creator,
    at = output_group_info_at,
    is_in = output_group_info_is_in,
    get_attr = output_group_info_get_attr
)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct OutputGroupInfoGen<V: ValueLifetimeless> {
    groups: ValueOfUncheckedGeneric<V, DictType<String, FrozenValue>>,
}

fn output_group_info_groups<'v, V: ValueLike<'v>>(this: &OutputGroupInfoGen<V>) -> DictRef<'v> {
    DictRef::from_value(this.groups.get().to_value())
        .expect("OutputGroupInfo groups are checked on construction")
}

fn output_group_info_at<'v, V: ValueLike<'v>>(
    this: &OutputGroupInfoGen<V>,
    index: Value<'v>,
    _heap: Heap<'v>,
) -> starlark::Result<Value<'v>> {
    let groups = output_group_info_groups(this);
    match groups.get(index)? {
        Some(value) => Ok(value),
        None => Err(starlark::Error::new_other(ValueError::KeyNotFound(
            index.to_repr(),
        ))),
    }
}

fn output_group_info_is_in<'v, V: ValueLike<'v>>(
    this: &OutputGroupInfoGen<V>,
    other: Value<'v>,
) -> starlark::Result<bool> {
    let groups = output_group_info_groups(this);
    Ok(groups.get(other)?.is_some())
}

fn output_group_info_get_attr<'v, V: ValueLike<'v>>(
    this: &OutputGroupInfoGen<V>,
    attribute: &str,
    _heap: Heap<'v>,
) -> Option<Value<'v>> {
    output_group_info_groups(this).get_str(attribute)
}

fn output_group_info_groups_from_value<'v>(value: Value<'v>) -> buck2_error::Result<DictRef<'v>> {
    if let Some(info) = value.downcast_ref::<OutputGroupInfo<'v>>() {
        return Ok(output_group_info_groups(info));
    }
    if let Some(info) = value
        .unpack_frozen()
        .and_then(|value| value.downcast_ref::<FrozenOutputGroupInfo>())
    {
        return Ok(output_group_info_groups(info));
    }
    Err(internal_error!(
        "OutputGroupInfo provider should have the expected provider type"
    ))
}

pub(crate) fn merge_output_group_info_values<'v>(
    heap: Heap<'v>,
    left: Value<'v>,
    right: Value<'v>,
) -> buck2_error::Result<Value<'v>> {
    let mut groups: SmallMap<String, Vec<Value<'v>>> = SmallMap::new();
    for provider in [left, right] {
        for (name, value) in output_group_info_groups_from_value(provider)?.iter() {
            let name = name
                .unpack_str()
                .ok_or_else(|| internal_error!("OutputGroupInfo group names should be strings"))?;
            groups.entry(name.to_owned()).or_default().push(value);
        }
    }

    let mut merged = SmallMap::with_capacity(groups.len());
    for (name, values) in groups {
        let value = match values.as_slice() {
            [value] => *value,
            _ => bazel_depset_from_transitive(heap, values)?,
        };
        merged.insert(name, value);
    }

    Ok(heap.alloc(OutputGroupInfo {
        groups: ValueOfUnchecked::new(heap.alloc(AllocDict(merged))),
    }))
}

impl FrozenOutputGroupInfo {
    pub fn for_each_output_group(
        &self,
        group: &str,
        processor: &mut dyn FnMut(ArtifactGroup),
    ) -> buck2_error::Result<()> {
        let Some(value) = output_group_info_groups(self).get_str(group) else {
            return Ok(());
        };

        for value in bazel_depset_to_list(value)? {
            let artifact = ValueAsInputArtifactLike::unpack_value_err(value)?
                .0
                .get_bound_artifact()?;
            processor(ArtifactGroup::Artifact(artifact));
        }

        Ok(())
    }
}

#[starlark_module]
fn output_group_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenOutputGroupInfo)]
    fn OutputGroupInfo<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<OutputGroupInfo<'v>> {
        Ok(OutputGroupInfo {
            groups: ValueOfUnchecked::new(eval.heap().alloc(AllocDict(kwargs))),
        })
    }
}
