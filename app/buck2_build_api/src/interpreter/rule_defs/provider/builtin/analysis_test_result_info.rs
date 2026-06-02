use std::fmt::Debug;

use allocative::Allocative;
use buck2_build_api_derive::internal_provider;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::values::Freeze;
use starlark::values::Trace;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;

use crate as buck2_build_api;

/// Provider returned by Bazel analysis-test rules to report analysis-time test status.
#[internal_provider(analysis_test_result_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct AnalysisTestResultInfoGen<V: ValueLifetimeless> {
    success: ValueOfUncheckedGeneric<V, bool>,
    message: ValueOfUncheckedGeneric<V, String>,
}

#[starlark_module]
fn analysis_test_result_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenAnalysisTestResultInfo)]
    fn AnalysisTestResultInfo<'v>(
        #[starlark(require = named)] success: bool,
        #[starlark(require = named)] message: &'v str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<AnalysisTestResultInfo<'v>> {
        let heap = eval.heap();
        Ok(AnalysisTestResultInfo {
            success: ValueOfUnchecked::new(heap.alloc(success)),
            message: ValueOfUnchecked::new(heap.alloc(message)),
        })
    }
}
