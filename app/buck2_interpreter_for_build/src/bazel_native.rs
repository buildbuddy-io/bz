/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use buck2_core::cells::external::BZLMOD_BAZEL_COMPAT_VERSION;
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
    #[error("Bazel label build setting requires the prelude `alias` rule to be loaded")]
    MissingAliasRule,
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
    });
    bazel_build_setting_rules(builder);
}
