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
use starlark::values::list::AllocList;
use starlark::values::list::ListType;

use crate as buck2_build_api;

/// Provider used by Bazel package specification targets.
#[internal_provider(package_specification_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct PackageSpecificationInfoGen<V: ValueLifetimeless> {
    packages: ValueOfUncheckedGeneric<V, ListType<String>>,
}

#[starlark_module]
fn package_specification_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenPackageSpecificationInfo)]
    fn PackageSpecificationInfo<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<PackageSpecificationInfo<'v>> {
        Ok(PackageSpecificationInfo {
            packages: ValueOfUnchecked::new(eval.heap().alloc(AllocList::EMPTY)),
        })
    }
}
