use std::fmt::Debug;

use allocative::Allocative;
use buck2_build_api_derive::internal_provider;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::values::Freeze;
use starlark::values::Trace;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueOf;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;

use crate as buck2_build_api;
use crate::interpreter::rule_defs::depset::BazelDepset;
use crate::interpreter::rule_defs::depset::FrozenBazelDepset;

/// Provider propagated by Bazel when analysis failures are represented as provider data.
#[internal_provider(analysis_failure_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct AnalysisFailureInfoGen<V: ValueLifetimeless> {
    causes: ValueOfUncheckedGeneric<V, FrozenBazelDepset>,
}

#[starlark_module]
fn analysis_failure_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenAnalysisFailureInfo)]
    fn AnalysisFailureInfo<'v>(
        #[starlark(require = named)] causes: ValueOf<'v, &'v BazelDepset<'v>>,
    ) -> starlark::Result<AnalysisFailureInfo<'v>> {
        Ok(AnalysisFailureInfo {
            causes: ValueOfUnchecked::new(causes.value),
        })
    }
}
