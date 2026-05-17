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
use buck2_error::internal_error;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::values::Freeze;
use starlark::values::FrozenValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictType;
use starlark::values::dict::FrozenDictRef;

use crate as buck2_build_api;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueIsInputArtifactAnnotation;

/// Internal provider connecting Bazel output-file targets to their generating rule.
#[internal_provider(bazel_output_file_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct BazelOutputFileInfoGen<V: ValueLifetimeless> {
    outputs: ValueOfUncheckedGeneric<V, DictType<String, ValueIsInputArtifactAnnotation>>,
}

impl FrozenBazelOutputFileInfo {
    pub fn output(&self, name: &str) -> buck2_error::Result<Option<FrozenValue>> {
        Ok(FrozenDictRef::from_frozen_value(self.outputs.get())
            .ok_or_else(|| internal_error!("BazelOutputFileInfo.outputs should be a dict"))?
            .get_str(name))
    }
}

pub fn new_bazel_output_file_info<'v>(
    outputs: impl IntoIterator<Item = (String, Value<'v>)>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> BazelOutputFileInfo<'v> {
    BazelOutputFileInfo {
        outputs: ValueOfUnchecked::new(eval.heap().alloc(AllocDict(outputs))),
    }
}

#[starlark_module]
fn bazel_output_file_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenBazelOutputFileInfo)]
    fn BazelOutputFileInfo<'v>(
        #[starlark(require = named, default = AllocDict::EMPTY)] outputs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<BazelOutputFileInfo<'v>> {
        let _ = eval;
        Ok(BazelOutputFileInfo {
            outputs: ValueOfUnchecked::new(outputs),
        })
    }
}
