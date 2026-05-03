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
use starlark::values::structs::AllocStruct;

fn build_setting<'v>(
    kind: &'static str,
    flag: bool,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    let kind = eval.heap().alloc(kind);
    let flag = eval.heap().alloc(flag);
    eval.heap()
        .alloc(AllocStruct([("type", kind), ("flag", flag)]))
}

#[starlark_module]
fn bazel_config_module(builder: &mut GlobalsBuilder) {
    fn int<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("int", flag, eval))
    }

    fn bool<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("bool", flag, eval))
    }

    fn string<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("string", flag, eval))
    }

    fn string_list<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("string_list", flag, eval))
    }
}

#[starlark_module]
fn bazel_config_common_module(builder: &mut GlobalsBuilder) {
    fn toolchain_type<'v>(
        toolchain_type: Value<'v>,
        #[starlark(require = named, default = false)] mandatory: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let mandatory = eval.heap().alloc(mandatory);
        Ok(eval.heap().alloc(AllocStruct([
            ("toolchain_type", toolchain_type),
            ("mandatory", mandatory),
        ])))
    }
}

pub(crate) fn register_bazel_config(builder: &mut GlobalsBuilder) {
    builder.namespace("config", bazel_config_module);
    builder.namespace("config_common", bazel_config_common_module);
}
