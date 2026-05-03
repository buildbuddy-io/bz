/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;
use starlark::values::dict::AllocDict;
use starlark::values::none::NoneType;
use starlark::values::tuple::UnpackTuple;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelNativeError {
    #[error("`native.register_toolchains` is not implemented yet")]
    RegisterToolchainsNotImplemented,
}

#[starlark_module]
fn bazel_native_module(builder: &mut GlobalsBuilder) {
    fn existing_rule<'v>(
        _name: &str,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn existing_rules<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocDict::EMPTY))
    }

    fn register_toolchains<'v>(
        #[starlark(args)] _toolchains: UnpackTuple<Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Err(buck2_error::Error::from(BazelNativeError::RegisterToolchainsNotImplemented).into())
    }
}

pub(crate) fn register_bazel_native(builder: &mut GlobalsBuilder) {
    builder.namespace("native", |globals| {
        globals.set("bazel_version", "9.1.0");
        bazel_native_module(globals);
    });
}
