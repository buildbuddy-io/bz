/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;
use std::fmt::Debug;
use std::sync::Arc;

use allocative::Allocative;
use buck2_build_api_derive::internal_provider;
use buck2_core::provider::id::ProviderId;
use buck2_interpreter::types::provider::callable::ProviderCallableLike;
use dupe::Dupe;
use serde::Serializer;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::values::Demand;
use starlark::values::Freeze;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark_map::StarlarkHasher;

use crate as buck2_build_api;
use crate::interpreter::rule_defs::provider::ProviderLike;
use crate::interpreter::rule_defs::provider::callable::provider_callable_equals;
use crate::interpreter::rule_defs::provider::callable::provider_callable_write_hash;

const DEBUG_PACKAGE_INFO: &str = "DebugPackageInfo";
const CC_TOOLCHAIN_CONFIG_INFO: &str = "CcToolchainConfigInfo";
const CC_SHARED_LIBRARY_INFO: &str = "CcSharedLibraryInfo";
const CC_SHARED_LIBRARY_HINT_INFO: &str = "CcSharedLibraryHintInfo";
const CC_TOOLCHAIN_INFO: &str = "CcToolchainInfo";

#[internal_provider(cc_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct CcInfoGen<V: ValueLifetimeless> {
    compilation_context: ValueOfUncheckedGeneric<V, FrozenValue>,
    linking_context: ValueOfUncheckedGeneric<V, FrozenValue>,
    debug_context: ValueOfUncheckedGeneric<V, FrozenValue>,
    cc_native_library_info: ValueOfUncheckedGeneric<V, FrozenValue>,
}

#[starlark_module]
fn cc_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenCcInfo)]
    fn CcInfo<'v>(
        #[starlark(require = named, default = NoneType)] compilation_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] linking_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] debug_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] cc_native_library_info: Value<'v>,
    ) -> starlark::Result<CcInfo<'v>> {
        Ok(CcInfo {
            compilation_context: ValueOfUnchecked::<FrozenValue>::new(compilation_context),
            linking_context: ValueOfUnchecked::<FrozenValue>::new(linking_context),
            debug_context: ValueOfUnchecked::<FrozenValue>::new(debug_context),
            cc_native_library_info: ValueOfUnchecked::<FrozenValue>::new(cc_native_library_info),
        })
    }
}

#[derive(Clone, Debug, Freeze, ProvidesStaticType, Trace, Allocative)]
#[repr(C)]
pub struct CcNativeProviderGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[trace(unsafe_ignore)]
    id: Arc<ProviderId>,
    #[freeze(identity)]
    name: &'static str,
    values: Box<[(String, V)]>,
}

starlark::starlark_complex_value!(pub CcNativeProvider);

unsafe impl<FromV, ToV> Coerce<CcNativeProviderGen<ToV>> for CcNativeProviderGen<FromV>
where
    FromV: ValueLifetimeless + Coerce<ToV>,
    ToV: ValueLifetimeless,
{
}

impl<V: ValueLifetimeless> fmt::Display for CcNativeProviderGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(<{} field(s)>)", self.name, self.values.len())
    }
}

impl<'v, V: ValueLike<'v>> serde::Serialize for CcNativeProviderGen<V> {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.collect_map(
            self.values
                .iter()
                .map(|(name, value)| (name, value.to_value())),
        )
    }
}

#[starlark_value(type = "CcNativeProvider")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for CcNativeProviderGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn dir_attr(&self) -> Vec<String> {
        self.values.iter().map(|(name, _)| name.clone()).collect()
    }

    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.values
            .iter()
            .find_map(|(name, value)| (name == attribute).then(|| value.to_value()))
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderLike>(self);
    }
}

impl<'v, V: ValueLike<'v>> ProviderLike<'v> for CcNativeProviderGen<V> {
    fn id(&self) -> &Arc<ProviderId> {
        &self.id
    }

    fn items(&self) -> Vec<(&str, Value<'v>)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_value()))
            .collect()
    }
}

#[derive(Debug, Clone, Dupe, ProvidesStaticType, NoSerialize, Allocative)]
struct CcNativeProviderCallable {
    name: &'static str,
    id: Arc<ProviderId>,
}

impl CcNativeProviderCallable {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            id: Arc::new(ProviderId {
                path: None,
                name: name.to_owned(),
            }),
        }
    }
}

impl fmt::Display for CcNativeProviderCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

starlark::starlark_simple_value!(CcNativeProviderCallable);

#[starlark_value(type = "cc_native_provider_callable")]
impl<'v> StarlarkValue<'v> for CcNativeProviderCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        Ok(eval
            .heap()
            .alloc(make_cc_native_provider_from_kwargs(
                self.name,
                self.id.dupe(),
                args.names_map()?,
            ))
            .to_value())
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderCallableLike>(self);
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        provider_callable_equals(self, other)
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        provider_callable_write_hash(self, hasher)
    }
}

impl ProviderCallableLike for CcNativeProviderCallable {
    fn id(&self) -> buck2_error::Result<&Arc<ProviderId>> {
        Ok(&self.id)
    }
}

fn make_cc_native_provider_from_kwargs<'v>(
    name: &'static str,
    id: Arc<ProviderId>,
    kwargs: SmallMap<starlark::values::StringValue<'v>, Value<'v>>,
) -> CcNativeProvider<'v> {
    make_cc_native_provider(
        name,
        id,
        kwargs
            .into_iter()
            .map(|(name, value)| (name.as_str().to_owned(), value)),
    )
}

fn make_cc_native_provider<'v>(
    name: &'static str,
    id: Arc<ProviderId>,
    values: impl IntoIterator<Item = (String, Value<'v>)>,
) -> CcNativeProvider<'v> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_by(|(left, _), (right, _)| left.cmp(right));
    CcNativeProvider {
        id,
        name,
        values: values.into_boxed_slice(),
    }
}

#[starlark_module]
fn bazel_cc_common_module(builder: &mut GlobalsBuilder) {
    fn is_cc_toolchain_resolution_enabled_do_not_use<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
    ) -> starlark::Result<bool> {
        let _unused = ctx;
        Ok(true)
    }

    fn create_cc_toolchain_config_info<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<CcNativeProvider<'v>> {
        Ok(make_cc_native_provider(
            CC_TOOLCHAIN_CONFIG_INFO,
            CcNativeProviderCallable::new(CC_TOOLCHAIN_CONFIG_INFO).id,
            kwargs.into_iter().filter(|(name, _)| name != "ctx"),
        ))
    }
}

pub(crate) fn register_cc_common(globals: &mut GlobalsBuilder) {
    globals.set(
        DEBUG_PACKAGE_INFO,
        CcNativeProviderCallable::new(DEBUG_PACKAGE_INFO),
    );
    globals.set(
        CC_TOOLCHAIN_CONFIG_INFO,
        CcNativeProviderCallable::new(CC_TOOLCHAIN_CONFIG_INFO),
    );
    globals.set(
        CC_SHARED_LIBRARY_INFO,
        CcNativeProviderCallable::new(CC_SHARED_LIBRARY_INFO),
    );
    globals.set(
        CC_SHARED_LIBRARY_HINT_INFO,
        CcNativeProviderCallable::new(CC_SHARED_LIBRARY_HINT_INFO),
    );
    globals.namespace("cc_common", |cc_common| {
        cc_common.set(
            CC_TOOLCHAIN_INFO,
            CcNativeProviderCallable::new(CC_TOOLCHAIN_INFO),
        );
        bazel_cc_common_module(cc_common);
    });
}
