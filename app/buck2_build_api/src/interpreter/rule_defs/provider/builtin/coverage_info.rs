/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory or the Apache License, Version 2.0
 * found in the LICENSE-APACHE file in the root directory. You may select, at
 * your option, one of the above-listed licenses.
 */

use std::fmt::Debug;

use allocative::Allocative;
use buck2_build_api_derive::internal_provider;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Freeze;
use starlark::values::FrozenValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;

use crate as buck2_build_api;
use crate::interpreter::rule_defs::depset::bazel_depset_from_direct;

#[internal_provider(instrumented_files_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct InstrumentedFilesInfoGen<V: ValueLifetimeless> {
    instrumented_files: ValueOfUncheckedGeneric<V, FrozenValue>,
    metadata_files: ValueOfUncheckedGeneric<V, FrozenValue>,
}

#[starlark_module]
fn instrumented_files_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenInstrumentedFilesInfo)]
    fn InstrumentedFilesInfo<'v>(
        #[starlark(require = named, default = NoneOr::None)] instrumented_files: NoneOr<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] metadata_files: NoneOr<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<InstrumentedFilesInfo<'v>> {
        let empty = eval.heap().alloc(bazel_depset_from_direct(Vec::new())?);
        Ok(InstrumentedFilesInfo {
            instrumented_files: ValueOfUnchecked::<FrozenValue>::new(
                instrumented_files.into_option().unwrap_or(empty),
            ),
            metadata_files: ValueOfUnchecked::<FrozenValue>::new(
                metadata_files.into_option().unwrap_or(empty),
            ),
        })
    }
}

#[starlark_module]
fn coverage_common(globals: &mut GlobalsBuilder) {
    fn instrumented_files_info<'v>(
        ctx: Value<'v>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        source_attributes: UnpackListOrTuple<String>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        dependency_attributes: UnpackListOrTuple<String>,
        #[starlark(require = named, default = AllocList::EMPTY)] coverage_support_files: Value<'v>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        coverage_environment: UnpackDictEntries<String, String>,
        #[starlark(require = named, default = NoneOr::None)] extensions: NoneOr<
            UnpackListOrTuple<String>,
        >,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        metadata_files: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] reported_to_actual_sources: NoneOr<
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] baseline_coverage_files: NoneOr<
            UnpackListOrTuple<Value<'v>>,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<InstrumentedFilesInfo<'v>> {
        let instrumented_files = eval.heap().alloc(bazel_depset_from_direct(Vec::new())?);
        let metadata_files = eval
            .heap()
            .alloc(bazel_depset_from_direct(metadata_files.items)?);
        let _ = (
            ctx,
            source_attributes,
            dependency_attributes,
            coverage_support_files,
            coverage_environment,
            extensions,
            reported_to_actual_sources,
            baseline_coverage_files,
        );

        Ok(InstrumentedFilesInfo {
            instrumented_files: ValueOfUnchecked::<FrozenValue>::new(instrumented_files),
            metadata_files: ValueOfUnchecked::<FrozenValue>::new(metadata_files),
        })
    }
}

pub(crate) fn register_coverage_common(globals: &mut GlobalsBuilder) {
    globals.namespace("coverage_common", coverage_common);
}
