/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;

use std::fmt;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelConfigError {
    #[error("'repeatable' can only be set for a setting with 'flag = True'")]
    RepeatableRequiresFlag,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct BazelExecTransition {
    exec_group: Option<String>,
}

impl fmt::Display for BazelExecTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.exec_group {
            Some(exec_group) => write!(f, "<exec transition for {exec_group}>"),
            None => f.write_str("<exec transition>"),
        }
    }
}

starlark_simple_value!(BazelExecTransition);

#[starlark_value(type = "ExecTransitionFactory")]
impl<'v> StarlarkValue<'v> for BazelExecTransition {}

pub(crate) fn bazel_exec_transition_from_value(value: Value<'_>) -> Option<&BazelExecTransition> {
    value.downcast_ref::<BazelExecTransition>()
}

fn build_setting<'v>(
    kind: &'static str,
    flag: bool,
    allow_multiple: bool,
    repeatable: bool,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    let kind = eval.heap().alloc(kind);
    let flag = eval.heap().alloc(flag);
    let allow_multiple = eval.heap().alloc(allow_multiple);
    let repeatable = eval.heap().alloc(repeatable);
    eval.heap().alloc(AllocStruct([
        ("type", kind),
        ("flag", flag),
        ("allow_multiple", allow_multiple),
        ("repeatable", repeatable),
    ]))
}

#[starlark_module]
fn bazel_config_module(builder: &mut GlobalsBuilder) {
    fn int<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("int", flag, false, false, eval))
    }

    fn bool<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("bool", flag, false, false, eval))
    }

    fn string<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        #[starlark(require = named, default = false)] allow_multiple: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("string", flag, allow_multiple, false, eval))
    }

    fn string_list<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        #[starlark(require = named, default = false)] repeatable: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if repeatable && !flag {
            return Err(buck2_error::Error::from(BazelConfigError::RepeatableRequiresFlag).into());
        }
        Ok(build_setting("string_list", flag, false, repeatable, eval))
    }

    fn exec(
        #[starlark(require = named, default = NoneOr::None)] exec_group: NoneOr<&str>,
    ) -> starlark::Result<BazelExecTransition> {
        Ok(BazelExecTransition {
            exec_group: exec_group.into_option().map(str::to_owned),
        })
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
