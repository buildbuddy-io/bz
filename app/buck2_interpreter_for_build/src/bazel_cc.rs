/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::UnpackTuple;

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCcInternal;

impl fmt::Display for BazelCcInternal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cc_internal")
    }
}

starlark::starlark_simple_value!(BazelCcInternal);

#[starlark_value(type = "cc_internal")]
impl<'v> StarlarkValue<'v> for BazelCcInternal {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cc_internal_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "check_private_api".to_owned(),
            "create_header_info".to_owned(),
            "create_header_info_with_deps".to_owned(),
            "freeze".to_owned(),
        ]
    }
}

fn kw_value<'v>(kwargs: &SmallMap<String, Value<'v>>, name: &str, default: Value<'v>) -> Value<'v> {
    kwargs.get(name).copied().unwrap_or(default)
}

fn header_info_attr<'v>(
    header_info: Value<'v>,
    name: &str,
    default: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    if header_info.is_none() {
        return Ok(default);
    }
    Ok(header_info.get_attr(name, eval.heap())?.unwrap_or(default))
}

fn alloc_header_info<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    let none = Value::new_none();
    let empty_list = eval.heap().alloc(AllocList::EMPTY);
    eval.heap().alloc(AllocStruct([
        ("header_module", kw_value(kwargs, "header_module", none)),
        (
            "pic_header_module",
            kw_value(kwargs, "pic_header_module", none),
        ),
        (
            "modular_public_headers",
            kw_value(kwargs, "modular_public_headers", empty_list),
        ),
        (
            "modular_private_headers",
            kw_value(kwargs, "modular_private_headers", empty_list),
        ),
        (
            "textual_headers",
            kw_value(kwargs, "textual_headers", empty_list),
        ),
        (
            "separate_module_headers",
            kw_value(kwargs, "separate_module_headers", empty_list),
        ),
        ("separate_module", kw_value(kwargs, "separate_module", none)),
        (
            "separate_pic_module",
            kw_value(kwargs, "separate_pic_module", none),
        ),
        ("deps", kw_value(kwargs, "deps", empty_list)),
        ("merged_deps", kw_value(kwargs, "merged_deps", empty_list)),
    ]))
}

fn alloc_header_info_with_deps<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let none = Value::new_none();
    let empty_list = eval.heap().alloc(AllocList::EMPTY);
    let header_info = kw_value(kwargs, "header_info", none);
    Ok(eval.heap().alloc(AllocStruct([
        (
            "header_module",
            header_info_attr(header_info, "header_module", none, eval)?,
        ),
        (
            "pic_header_module",
            header_info_attr(header_info, "pic_header_module", none, eval)?,
        ),
        (
            "modular_public_headers",
            header_info_attr(header_info, "modular_public_headers", empty_list, eval)?,
        ),
        (
            "modular_private_headers",
            header_info_attr(header_info, "modular_private_headers", empty_list, eval)?,
        ),
        (
            "textual_headers",
            header_info_attr(header_info, "textual_headers", empty_list, eval)?,
        ),
        (
            "separate_module_headers",
            header_info_attr(header_info, "separate_module_headers", empty_list, eval)?,
        ),
        (
            "separate_module",
            header_info_attr(header_info, "separate_module", none, eval)?,
        ),
        (
            "separate_pic_module",
            header_info_attr(header_info, "separate_pic_module", none, eval)?,
        ),
        ("deps", kw_value(kwargs, "deps", empty_list)),
        ("merged_deps", kw_value(kwargs, "merged_deps", empty_list)),
    ])))
}

#[starlark_module]
fn bazel_cc_internal_methods(builder: &mut MethodsBuilder) {
    fn create_header_info<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(alloc_header_info(&kwargs, eval))
    }

    fn create_header_info_with_deps<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        alloc_header_info_with_deps(&kwargs, eval)
    }

    fn freeze<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        value: Value<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(value)
    }

    fn check_private_api<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(args)] _args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }
}

#[starlark_module]
fn bazel_cc_common_module(builder: &mut GlobalsBuilder) {
    fn internal_DO_NOT_USE() -> starlark::Result<BazelCcInternal> {
        Ok(BazelCcInternal)
    }

    fn get_tool_for_action<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }

    fn get_execution_requirements<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocDict::EMPTY))
    }

    fn action_is_enabled<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn get_memory_inefficient_command_line<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }

    fn get_environment_variables<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocDict::EMPTY))
    }

    fn empty_variables<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Ok(eval
            .heap()
            .alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new())))
    }

    fn legacy_cc_flags_make_variable_do_not_use<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }

    fn incompatible_disable_objc_library_transition() -> starlark::Result<bool> {
        Ok(false)
    }

    fn add_go_exec_groups_to_binary_rules() -> starlark::Result<bool> {
        Ok(false)
    }

    fn check_experimental_cc_shared_library() -> starlark::Result<bool> {
        Ok(false)
    }

    fn get_tool_requirement_for_action<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }

    fn implementation_deps_allowed_by_allowlist<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(true)
    }
}

pub(crate) fn register_bazel_cc_common(builder: &mut GlobalsBuilder) {
    builder.namespace("cc_common", |cc_common| {
        cc_common.set("do_not_use_tools_cpp_compiler_present", NoneType);
        bazel_cc_common_module(cc_common);
    });
}
