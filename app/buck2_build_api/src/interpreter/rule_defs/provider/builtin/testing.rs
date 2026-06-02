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
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictType;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list_or_tuple::UnpackListOrTuple;

use crate as buck2_build_api;
use crate::interpreter::rule_defs::provider::builtin::run_environment_info::RunEnvironmentInfo;
use crate::interpreter::rule_defs::provider::builtin::run_environment_info::make_run_environment_info;

/// Bazel provider for special execution requirements on tests.
#[internal_provider(execution_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct ExecutionInfoGen<V: ValueLifetimeless> {
    requirements: ValueOfUncheckedGeneric<V, DictType<String, String>>,
    exec_group: ValueOfUncheckedGeneric<V, String>,
}

#[starlark_module]
fn execution_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenExecutionInfo)]
    fn ExecutionInfo<'v>(
        #[starlark(default = UnpackDictEntries::default())] requirements: UnpackDictEntries<
            &'v str,
            &'v str,
        >,
        #[starlark(default = "test")] exec_group: &'v str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ExecutionInfo<'v>> {
        let heap = eval.heap();
        let requirements = heap.alloc(AllocDict(
            requirements
                .entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned())),
        ));
        let exec_group = heap.alloc(exec_group.to_owned());
        Ok(ExecutionInfo {
            requirements: ValueOfUnchecked::new(requirements),
            exec_group: ValueOfUnchecked::new(exec_group),
        })
    }
}

#[starlark_module]
fn bazel_testing_module(globals: &mut GlobalsBuilder) {
    fn TestEnvironment<'v>(
        #[starlark(default = UnpackDictEntries::default())] environment: UnpackDictEntries<
            &'v str,
            &'v str,
        >,
        #[starlark(default = UnpackListOrTuple::default())]
        inherited_environment: UnpackListOrTuple<&'v str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<RunEnvironmentInfo<'v>> {
        Ok(make_run_environment_info(
            environment,
            inherited_environment,
            eval,
        ))
    }
}

pub(crate) fn register_bazel_testing(globals: &mut GlobalsBuilder) {
    globals.namespace("testing", |globals| {
        globals.set("ExecutionInfo", ExecutionInfoCallable::new());
        bazel_testing_module(globals);
    });
}
