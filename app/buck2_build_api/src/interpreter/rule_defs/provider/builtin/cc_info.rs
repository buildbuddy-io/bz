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
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
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
use starlark::values::list::AllocList;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::UnpackTuple;
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

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCcInternal;

impl fmt::Display for BazelCcInternal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cc_internal")
    }
}

starlark::starlark_simple_value!(BazelCcInternal);

#[starlark_value(type = "cc_internal")]
impl<'v> StarlarkValue<'v> for BazelCcInternal {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cc_internal_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "check_private_api".to_owned(),
            "create_header_info".to_owned(),
            "create_header_info_with_deps".to_owned(),
            "freeze".to_owned(),
        ]
    }
}

fn cc_internal_kw_value<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    name: &str,
    default: Value<'v>,
) -> Value<'v> {
    kwargs.get(name).copied().unwrap_or(default)
}

fn cc_internal_header_info_attr<'v>(
    header_info: Value<'v>,
    name: &str,
    default: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    if header_info.is_none() {
        return Ok(default);
    }
    Ok(header_info.get_attr(name, eval.heap())?.unwrap_or(default))
}

fn cc_internal_alloc_header_info<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    let none = Value::new_none();
    let empty_list = eval.heap().alloc(AllocList::EMPTY);
    eval.heap().alloc(AllocStruct([
        (
            "header_module",
            cc_internal_kw_value(kwargs, "header_module", none),
        ),
        (
            "pic_header_module",
            cc_internal_kw_value(kwargs, "pic_header_module", none),
        ),
        (
            "modular_public_headers",
            cc_internal_kw_value(kwargs, "modular_public_headers", empty_list),
        ),
        (
            "modular_private_headers",
            cc_internal_kw_value(kwargs, "modular_private_headers", empty_list),
        ),
        (
            "textual_headers",
            cc_internal_kw_value(kwargs, "textual_headers", empty_list),
        ),
        (
            "separate_module_headers",
            cc_internal_kw_value(kwargs, "separate_module_headers", empty_list),
        ),
        (
            "separate_module",
            cc_internal_kw_value(kwargs, "separate_module", none),
        ),
        (
            "separate_pic_module",
            cc_internal_kw_value(kwargs, "separate_pic_module", none),
        ),
        ("deps", cc_internal_kw_value(kwargs, "deps", empty_list)),
        (
            "merged_deps",
            cc_internal_kw_value(kwargs, "merged_deps", empty_list),
        ),
    ]))
}

fn cc_internal_alloc_header_info_with_deps<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let none = Value::new_none();
    let empty_list = eval.heap().alloc(AllocList::EMPTY);
    let header_info = cc_internal_kw_value(kwargs, "header_info", none);
    Ok(eval.heap().alloc(AllocStruct([
        (
            "header_module",
            cc_internal_header_info_attr(header_info, "header_module", none, eval)?,
        ),
        (
            "pic_header_module",
            cc_internal_header_info_attr(header_info, "pic_header_module", none, eval)?,
        ),
        (
            "modular_public_headers",
            cc_internal_header_info_attr(header_info, "modular_public_headers", empty_list, eval)?,
        ),
        (
            "modular_private_headers",
            cc_internal_header_info_attr(header_info, "modular_private_headers", empty_list, eval)?,
        ),
        (
            "textual_headers",
            cc_internal_header_info_attr(header_info, "textual_headers", empty_list, eval)?,
        ),
        (
            "separate_module_headers",
            cc_internal_header_info_attr(header_info, "separate_module_headers", empty_list, eval)?,
        ),
        (
            "separate_module",
            cc_internal_header_info_attr(header_info, "separate_module", none, eval)?,
        ),
        (
            "separate_pic_module",
            cc_internal_header_info_attr(header_info, "separate_pic_module", none, eval)?,
        ),
        ("deps", cc_internal_kw_value(kwargs, "deps", empty_list)),
        (
            "merged_deps",
            cc_internal_kw_value(kwargs, "merged_deps", empty_list),
        ),
    ])))
}

#[starlark_module]
fn bazel_cc_internal_methods(builder: &mut MethodsBuilder) {
    fn create_header_info<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(cc_internal_alloc_header_info(&kwargs, eval))
    }

    fn create_header_info_with_deps<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        cc_internal_alloc_header_info_with_deps(&kwargs, eval)
    }

    fn freeze<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        value: Value<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(value)
    }

    fn check_private_api<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(args)] _args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }
}

#[starlark_module]
fn bazel_cc_common_module(builder: &mut GlobalsBuilder) {
    fn internal_DO_NOT_USE() -> starlark::Result<BazelCcInternal> {
        Ok(BazelCcInternal)
    }

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
        cc_common.set("do_not_use_tools_cpp_compiler_present", NoneType);
        bazel_cc_common_module(cc_common);
    });
}
