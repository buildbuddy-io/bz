use std::fmt::Debug;

use allocative::Allocative;
use bz_build_api_derive::internal_provider;
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
use starlark::values::list::ListRef;
use starlark::values::list::ListType;
use starlark::values::list_or_tuple::UnpackListOrTuple;

use crate as bz_build_api;

/// Internal provider produced by Bazel's native `toolchain()` rule.
#[internal_provider(declared_toolchain_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct DeclaredToolchainInfoGen<V: ValueLifetimeless> {
    toolchain_type: ValueOfUncheckedGeneric<V, String>,
    toolchain: ValueOfUncheckedGeneric<V, String>,
    exec_compatible_with: ValueOfUncheckedGeneric<V, ListType<String>>,
    target_compatible_with: ValueOfUncheckedGeneric<V, ListType<String>>,
    target_settings: ValueOfUncheckedGeneric<V, ListType<String>>,
    use_target_platform_constraints: ValueOfUncheckedGeneric<V, bool>,
}

impl FrozenDeclaredToolchainInfo {
    pub fn toolchain_type(&self) -> &str {
        self.toolchain_type
            .get()
            .to_value()
            .unpack_str()
            .expect("validated at construction")
    }

    pub fn toolchain(&self) -> &str {
        self.toolchain
            .get()
            .to_value()
            .unpack_str()
            .expect("validated at construction")
    }

    pub fn exec_compatible_with(&self) -> Vec<&str> {
        collect_str_list(self.exec_compatible_with.get())
    }

    pub fn target_compatible_with(&self) -> Vec<&str> {
        collect_str_list(self.target_compatible_with.get())
    }

    pub fn target_settings(&self) -> Vec<&str> {
        collect_str_list(self.target_settings.get())
    }

    pub fn use_target_platform_constraints(&self) -> bool {
        self.use_target_platform_constraints
            .get()
            .to_value()
            .unpack_bool()
            .expect("validated at construction")
    }
}

fn collect_str_list(value: starlark::values::FrozenValue) -> Vec<&'static str> {
    ListRef::from_frozen_value(value)
        .expect("validated at construction")
        .iter()
        .map(|value| value.unpack_str().expect("validated at construction"))
        .collect()
}

#[starlark_module]
fn declared_toolchain_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenDeclaredToolchainInfo)]
    fn DeclaredToolchainInfo<'v>(
        #[starlark(require = named)] toolchain_type: &'v str,
        #[starlark(require = named)] toolchain: &'v str,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        exec_compatible_with: UnpackListOrTuple<&'v str>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        target_compatible_with: UnpackListOrTuple<&'v str>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        target_settings: UnpackListOrTuple<&'v str>,
        #[starlark(require = named, default = false)] use_target_platform_constraints: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<DeclaredToolchainInfo<'v>> {
        let heap = eval.heap();
        Ok(DeclaredToolchainInfo {
            toolchain_type: ValueOfUnchecked::new(heap.alloc(toolchain_type)),
            toolchain: ValueOfUnchecked::new(heap.alloc(toolchain)),
            exec_compatible_with: ValueOfUnchecked::new(
                heap.alloc(AllocList(
                    exec_compatible_with
                        .items
                        .into_iter()
                        .map(|label| label.to_owned()),
                )),
            ),
            target_compatible_with: ValueOfUnchecked::new(
                heap.alloc(AllocList(
                    target_compatible_with
                        .items
                        .into_iter()
                        .map(|label| label.to_owned()),
                )),
            ),
            target_settings: ValueOfUnchecked::new(
                heap.alloc(AllocList(
                    target_settings
                        .items
                        .into_iter()
                        .map(|label| label.to_owned()),
                )),
            ),
            use_target_platform_constraints: ValueOfUnchecked::new(
                heap.alloc(use_target_platform_constraints),
            ),
        })
    }
}
