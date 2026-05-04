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
use buck2_core::cells::external::BZLMOD_BAZEL_COMPAT_VERSION;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::dict::AllocDict;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::tuple::UnpackTuple;

use crate::interpreter::build_context::BuildContext;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelNativeError {
    #[error("`native.register_toolchains` expected a string target pattern, got `{0}`")]
    RegisterToolchainsNonString(String),
    #[error("Bazel label build setting requires the prelude `alias` rule to be loaded")]
    MissingAliasRule,
    #[error("Bazel native rule `{0}` requires a loaded Buck rule with the same name")]
    MissingNativeRule(&'static str),
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct NativeRuleCallable {
    name: &'static str,
}

impl fmt::Display for NativeRuleCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native.{}", self.name)
    }
}

starlark::starlark_simple_value!(NativeRuleCallable);

#[starlark_value(type = "native_rule_callable")]
impl<'v> StarlarkValue<'v> for NativeRuleCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let rule = eval.module().get(self.name).ok_or_else(|| {
            buck2_error::Error::from(BazelNativeError::MissingNativeRule(self.name))
        })?;
        ValueLike::invoke(rule, args, eval)
    }
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
        #[starlark(args)] toolchains: UnpackTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let build_context = BuildContext::from_context(eval)?;
        if let Some(recorder) = build_context.bazel_repository_rule_recorder {
            for toolchain in toolchains.items {
                let Some(pattern) = toolchain.unpack_str() else {
                    return Err(buck2_error::Error::from(
                        BazelNativeError::RegisterToolchainsNonString(
                            toolchain.get_type().to_owned(),
                        ),
                    )
                    .into());
                };
                recorder.record_registered_toolchain(pattern.to_owned());
            }
        }
        Ok(NoneType)
    }
}

fn label_build_setting<'v>(
    name: &str,
    build_setting_default: Value<'v>,
    visibility: Option<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let alias = eval
        .module()
        .get("alias")
        .ok_or_else(|| buck2_error::Error::from(BazelNativeError::MissingAliasRule))?;
    let name = eval.heap().alloc(name);
    let mut kwargs = vec![("name", name), ("actual", build_setting_default)];
    if let Some(visibility) = visibility {
        kwargs.push(("visibility", visibility));
    }
    eval.eval_function(alias, &[], &kwargs)
        .map_err(buck2_error::Error::from)?;
    Ok(NoneType)
}

#[starlark_module]
fn bazel_build_setting_rules(builder: &mut GlobalsBuilder) {
    fn label_flag<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] build_setting_default: Value<'v>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        label_build_setting(name, build_setting_default, visibility, eval)
    }

    fn label_setting<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] build_setting_default: Value<'v>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        label_build_setting(name, build_setting_default, visibility, eval)
    }
}

pub(crate) fn register_bazel_native(builder: &mut GlobalsBuilder) {
    builder.namespace("native", |globals| {
        globals.set("bazel_version", BZLMOD_BAZEL_COMPAT_VERSION);
        bazel_native_module(globals);
        for name in [
            "alias",
            "cc_binary",
            "cc_import",
            "cc_library",
            "cc_proto_library",
            "cc_shared_library",
            "cc_test",
            "cc_toolchain",
            "cc_toolchain_suite",
            "config_setting",
            "filegroup",
            "genrule",
            "java_binary",
            "java_import",
            "java_library",
            "java_lite_proto_library",
            "java_package_configuration",
            "java_plugin",
            "java_proto_library",
            "java_runtime",
            "java_test",
            "java_toolchain",
            "proto_library",
            "sh_binary",
            "sh_library",
            "sh_test",
            "test_suite",
            "toolchain",
            "toolchain_type",
        ] {
            globals.set(name, NativeRuleCallable { name });
        }
    });
    bazel_build_setting_rules(builder);
}
