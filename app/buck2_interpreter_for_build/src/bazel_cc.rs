/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::values::Value;
use starlark::values::none::NoneType;

#[starlark_module]
fn bazel_cc_common_module(builder: &mut GlobalsBuilder) {
    fn is_cc_toolchain_resolution_enabled_do_not_use<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
    ) -> starlark::Result<bool> {
        let _unused = ctx;
        Ok(true)
    }
}

pub(crate) fn register_bazel_cc_common(builder: &mut GlobalsBuilder) {
    builder.set("CcSharedLibraryInfo", NoneType);
    builder.set("CcSharedLibraryHintInfo", NoneType);
    builder.namespace("cc_common", |cc_common| {
        cc_common.set("CcToolchainInfo", "CcToolchainInfo");
        bazel_cc_common_module(cc_common);
    });
}
