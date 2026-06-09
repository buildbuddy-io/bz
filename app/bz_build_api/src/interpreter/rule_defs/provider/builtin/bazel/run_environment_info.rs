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
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::dict::DictType;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list::ListType;
use starlark::values::list_or_tuple::UnpackListOrTuple;

use crate as bz_build_api;

/// Provider containing environment variables for executable and test targets.
#[internal_provider(run_environment_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct RunEnvironmentInfoGen<V: ValueLifetimeless> {
    /// Fixed environment variables to make available when the target is executed.
    environment: ValueOfUncheckedGeneric<V, DictType<String, String>>,
    /// Names of environment variables to inherit from the shell environment.
    inherited_environment: ValueOfUncheckedGeneric<V, ListType<String>>,
}

pub(crate) fn make_run_environment_info<'v>(
    environment: UnpackDictEntries<&'v str, &'v str>,
    inherited_environment: UnpackListOrTuple<&'v str>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> RunEnvironmentInfo<'v> {
    let heap = eval.heap();
    let environment = heap.alloc(AllocDict(
        environment
            .entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned())),
    ));
    let inherited_environment = heap.alloc(AllocList(
        inherited_environment
            .items
            .into_iter()
            .map(|name| name.to_owned()),
    ));
    RunEnvironmentInfo {
        environment: ValueOfUnchecked::new(environment),
        inherited_environment: ValueOfUnchecked::new(inherited_environment),
    }
}

impl FrozenRunEnvironmentInfo {
    pub fn environment(&self) -> bz_error::Result<Vec<(String, String)>> {
        let environment =
            DictRef::from_value(self.environment.get().to_value()).ok_or_else(|| {
                bz_error::internal_error!("RunEnvironmentInfo.environment should be a dict")
            })?;
        environment
            .iter()
            .map(|(name, value)| {
                let name = name.unpack_str().ok_or_else(|| {
                    bz_error::internal_error!(
                        "RunEnvironmentInfo.environment keys should be strings"
                    )
                })?;
                let value = value.unpack_str().ok_or_else(|| {
                    bz_error::internal_error!(
                        "RunEnvironmentInfo.environment values should be strings"
                    )
                })?;
                Ok((name.to_owned(), value.to_owned()))
            })
            .collect()
    }

    pub fn inherited_environment(&self) -> bz_error::Result<Vec<String>> {
        let inherited_environment =
            ListRef::from_value(self.inherited_environment.get().to_value()).ok_or_else(|| {
                bz_error::internal_error!(
                    "RunEnvironmentInfo.inherited_environment should be a list"
                )
            })?;
        inherited_environment
            .iter()
            .map(|name| {
                let name = name.unpack_str().ok_or_else(|| {
                    bz_error::internal_error!(
                        "RunEnvironmentInfo.inherited_environment values should be strings"
                    )
                })?;
                Ok(name.to_owned())
            })
            .collect()
    }
}

#[starlark_module]
fn run_environment_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenRunEnvironmentInfo)]
    fn RunEnvironmentInfo<'v>(
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
