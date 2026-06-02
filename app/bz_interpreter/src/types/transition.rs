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

use bz_core::configuration::transition::id::TransitionId;
use starlark::any::ProvidesStaticType;
use starlark::eval::Evaluator;
use starlark::values::Value;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum TransitionError {
    #[error("cfg parameter is not a transition object: {}", _0)]
    WrongType(String),
}

/// Implemented by starlark transition objects.
pub trait TransitionValue {
    fn transition_id(&self) -> bz_error::Result<Arc<TransitionId>>;

    fn transition_id_for_bazel_attr<'v>(
        &self,
        _value: Value<'v>,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> bz_error::Result<Arc<TransitionId>> {
        self.transition_id()
    }
}

unsafe impl<'v> ProvidesStaticType<'v> for &'v dyn TransitionValue {
    type StaticType = &'static dyn TransitionValue;
}

pub fn transition_id_from_value(value: Value) -> bz_error::Result<Arc<TransitionId>> {
    match value.request_value::<&dyn TransitionValue>() {
        Some(has) => has.transition_id(),
        None => Err(TransitionError::WrongType(value.to_repr()).into()),
    }
}

pub fn transition_id_from_value_for_bazel_attr<'v>(
    value: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> bz_error::Result<Arc<TransitionId>> {
    match value.request_value::<&dyn TransitionValue>() {
        Some(has) => has.transition_id_for_bazel_attr(value, eval),
        None => Err(TransitionError::WrongType(value.to_repr()).into()),
    }
}
