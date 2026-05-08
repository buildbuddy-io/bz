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
