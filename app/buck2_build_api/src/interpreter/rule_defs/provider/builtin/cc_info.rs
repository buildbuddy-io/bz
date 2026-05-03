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
use starlark::environment::GlobalsBuilder;
use starlark::values::Freeze;
use starlark::values::FrozenValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::none::NoneType;

use crate as buck2_build_api;

#[internal_provider(cc_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct CcInfoGen<V: ValueLifetimeless> {
    compilation_context: ValueOfUncheckedGeneric<V, FrozenValue>,
    linking_context: ValueOfUncheckedGeneric<V, FrozenValue>,
    debug_context: ValueOfUncheckedGeneric<V, FrozenValue>,
    cc_native_library_info: ValueOfUncheckedGeneric<V, FrozenValue>,
}

#[starlark_module]
fn cc_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenCcInfo)]
    fn CcInfo<'v>(
        #[starlark(require = named, default = NoneType)] compilation_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] linking_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] debug_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] cc_native_library_info: Value<'v>,
    ) -> starlark::Result<CcInfo<'v>> {
        Ok(CcInfo {
            compilation_context: ValueOfUnchecked::<FrozenValue>::new(compilation_context),
            linking_context: ValueOfUnchecked::<FrozenValue>::new(linking_context),
            debug_context: ValueOfUnchecked::<FrozenValue>::new(debug_context),
            cc_native_library_info: ValueOfUnchecked::<FrozenValue>::new(cc_native_library_info),
        })
    }
}
