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
use starlark::starlark_module;
use starlark::values::Freeze;
use starlark::values::FrozenValue;
use starlark::values::Trace;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueOf;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::dict::DictType;
use starlark::values::dict::UnpackDictEntries;

use crate as buck2_build_api;
use crate::interpreter::rule_defs::provider::builtin::constraint_value_info::ConstraintValueInfoCallable;
use crate::interpreter::rule_defs::provider::builtin::toolchain_info::register_toolchain_info;

#[internal_provider(template_variable_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct TemplateVariableInfoGen<V: ValueLifetimeless> {
    variables: ValueOfUncheckedGeneric<V, DictType<String, String>>,
}

impl FrozenTemplateVariableInfo {
    pub fn variables_raw(&self) -> FrozenValue {
        self.variables.get()
    }
}

#[starlark_module]
fn template_variable_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenTemplateVariableInfo)]
    fn TemplateVariableInfo<'v>(
        vars: ValueOf<'v, UnpackDictEntries<&'v str, &'v str>>,
    ) -> starlark::Result<TemplateVariableInfo<'v>> {
        Ok(TemplateVariableInfo {
            variables: ValueOfUnchecked::new(vars.value),
        })
    }
}

pub(crate) fn register_platform_common(globals: &mut GlobalsBuilder) {
    globals.namespace("platform_common", |globals| {
        globals.set("ConstraintValueInfo", ConstraintValueInfoCallable::new());
        globals.set("TemplateVariableInfo", TemplateVariableInfoCallable::new());
        register_toolchain_info(globals);
    });
}
