use std::fmt::Debug;

use allocative::Allocative;
use bz_build_api_derive::internal_provider;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::values::Freeze;
use starlark::values::FrozenHeap;
use starlark::values::FrozenValueOfUnchecked;
use starlark::values::FrozenValueTyped;
use starlark::values::Heap;
use starlark::values::Trace;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;

use crate as bz_build_api;

/// Internal marker for synthetic Bazel input-file and output-file targets.
#[internal_provider(bazel_file_target_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct BazelFileTargetInfoGen<V: ValueLifetimeless> {
    marker: ValueOfUncheckedGeneric<V, bool>,
}

pub fn new_bazel_file_target_info<'v>(heap: Heap<'v>) -> BazelFileTargetInfo<'v> {
    BazelFileTargetInfo {
        marker: ValueOfUnchecked::new(heap.alloc(true)),
    }
}

pub fn new_frozen_bazel_file_target_info(
    heap: &FrozenHeap,
) -> FrozenValueTyped<'static, FrozenBazelFileTargetInfo> {
    FrozenValueTyped::new_err(heap.alloc(FrozenBazelFileTargetInfo {
        marker: FrozenValueOfUnchecked::new(heap.alloc(true)),
    }))
    .unwrap()
}

#[starlark_module]
fn bazel_file_target_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenBazelFileTargetInfo)]
    fn BazelFileTargetInfo<'v>(
        #[starlark(require = named, default = true)] marker: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<BazelFileTargetInfo<'v>> {
        Ok(BazelFileTargetInfo {
            marker: ValueOfUnchecked::new(eval.heap().alloc(marker)),
        })
    }
}
