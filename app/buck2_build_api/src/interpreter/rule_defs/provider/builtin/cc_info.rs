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
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::external::bzlmod_cell_name;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePathBuf;
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
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::list::UnpackList;
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

fn rules_cc_provider_path(path: &str) -> CellPath {
    CellPath::new(
        CellName::unchecked_new(&bzlmod_cell_name("rules_cc+"))
            .expect("rules_cc bzlmod cell name should be valid"),
        CellRelativePathBuf::unchecked_new(path.to_owned()),
    )
}

fn cc_native_provider_path(name: &str) -> Option<CellPath> {
    match name {
        // Bazel's C++ builtins wrap the bzlmod rules_cc providers for these
        // symbols, so their provider identity is the @rules_cc+ .bzl file.
        DEBUG_PACKAGE_INFO => Some(rules_cc_provider_path("cc/private/debug_package_info.bzl")),
        CC_TOOLCHAIN_CONFIG_INFO => Some(rules_cc_provider_path(
            "cc/private/toolchain_config/cc_toolchain_config_info.bzl",
        )),
        CC_TOOLCHAIN_INFO => Some(rules_cc_provider_path(
            "cc/private/rules_impl/cc_toolchain_info.bzl",
        )),
        _ => None,
    }
}

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
                path: cc_native_provider_path(name),
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

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCcToolchainFeatures;

impl fmt::Display for BazelCcToolchainFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<CcToolchainFeatures>")
    }
}

starlark::starlark_simple_value!(BazelCcToolchainFeatures);

#[starlark_value(type = "CcToolchainFeatures")]
impl<'v> StarlarkValue<'v> for BazelCcToolchainFeatures {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cc_toolchain_features_methods)
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelFeatureConfiguration {
    requested_features: Vec<String>,
}

impl fmt::Display for BazelFeatureConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<FeatureConfiguration({})>",
            self.requested_features.join(", ")
        )
    }
}

starlark::starlark_simple_value!(BazelFeatureConfiguration);

#[starlark_value(type = "FeatureConfiguration")]
impl<'v> StarlarkValue<'v> for BazelFeatureConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_feature_configuration_methods)
    }
}

#[starlark_value(type = "cc_internal")]
impl<'v> StarlarkValue<'v> for BazelCcInternal {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cc_internal_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "check_private_api".to_owned(),
            "cc_toolchain_features".to_owned(),
            "cc_toolchain_variables".to_owned(),
            "combine_cc_toolchain_variables".to_owned(),
            "create_header_info".to_owned(),
            "create_header_info_with_deps".to_owned(),
            "exec_os".to_owned(),
            "freeze".to_owned(),
        ]
    }
}

fn bazel_cc_exec_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "DARWIN"
    } else if cfg!(target_os = "linux") {
        "LINUX"
    } else if cfg!(target_os = "windows") {
        "WINDOWS"
    } else if cfg!(target_os = "freebsd") {
        "FREEBSD"
    } else if cfg!(target_os = "openbsd") {
        "OPENBSD"
    } else {
        "UNKNOWN"
    }
}

fn cc_internal_kw_value<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    name: &str,
    default: Value<'v>,
) -> Value<'v> {
    kwargs.get(name).copied().unwrap_or(default)
}

fn cc_internal_kw_value_or_default<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    name: &str,
    default: Value<'v>,
) -> Value<'v> {
    let value = cc_internal_kw_value(kwargs, name, default);
    if value.is_none() { default } else { value }
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
fn bazel_cc_toolchain_features_methods(builder: &mut MethodsBuilder) {
    fn default_features_and_action_configs<'v>(
        #[starlark(this)] _this: &BazelCcToolchainFeatures,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }

    fn configure_features(
        #[starlark(this)] _this: &BazelCcToolchainFeatures,
        #[starlark(require = named, default = UnpackList::default())]
        requested_features: UnpackList<String>,
    ) -> starlark::Result<BazelFeatureConfiguration> {
        Ok(BazelFeatureConfiguration {
            requested_features: requested_features.into_iter().collect(),
        })
    }
}

#[starlark_module]
fn bazel_feature_configuration_methods(builder: &mut MethodsBuilder) {
    fn is_enabled(
        #[starlark(this)] this: &BazelFeatureConfiguration,
        feature: &str,
    ) -> starlark::Result<bool> {
        Ok(this
            .requested_features
            .iter()
            .any(|requested| requested == feature))
    }

    fn is_requested(
        #[starlark(this)] this: &BazelFeatureConfiguration,
        feature: &str,
    ) -> starlark::Result<bool> {
        Ok(this
            .requested_features
            .iter()
            .any(|requested| requested == feature))
    }
}

#[starlark_module]
fn bazel_cc_internal_methods(builder: &mut MethodsBuilder) {
    fn cc_toolchain_features<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<BazelCcToolchainFeatures> {
        Ok(BazelCcToolchainFeatures)
    }

    fn cc_toolchain_variables<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] vars: Value<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(vars)
    }

    fn combine_cc_toolchain_variables<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        parent: Value<'v>,
        #[starlark(args)] _variables: UnpackTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if parent.is_none() {
            Ok(eval.heap().alloc(AllocDict::EMPTY))
        } else {
            Ok(parent)
        }
    }

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

    fn exec_os<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] ctx: Value<'v>,
    ) -> starlark::Result<&'static str> {
        let _unused = ctx;
        Ok(bazel_cc_exec_os())
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

    fn get_tool_for_action<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }

    fn get_execution_requirements<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocDict::EMPTY))
    }

    fn action_is_enabled<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn get_memory_inefficient_command_line<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }

    fn get_environment_variables<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocDict::EMPTY))
    }

    fn empty_variables<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Ok(eval
            .heap()
            .alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new())))
    }

    fn legacy_cc_flags_make_variable_do_not_use<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }

    fn incompatible_disable_objc_library_transition() -> starlark::Result<bool> {
        Ok(false)
    }

    fn add_go_exec_groups_to_binary_rules() -> starlark::Result<bool> {
        Ok(false)
    }

    fn check_experimental_cc_shared_library() -> starlark::Result<bool> {
        Ok(false)
    }

    fn get_tool_requirement_for_action<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }

    fn implementation_deps_allowed_by_allowlist<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn create_cc_toolchain_config_info<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<CcNativeProvider<'v>> {
        let empty_list = eval.heap().alloc(AllocList::EMPTY);
        let empty_string = eval.heap().alloc("");
        Ok(make_cc_native_provider(
            CC_TOOLCHAIN_CONFIG_INFO,
            CcNativeProviderCallable::new(CC_TOOLCHAIN_CONFIG_INFO).id,
            [
                (
                    "_action_configs_DO_NOT_USE".to_owned(),
                    cc_internal_kw_value(&kwargs, "action_configs", empty_list),
                ),
                (
                    "_artifact_name_patterns_DO_NOT_USE".to_owned(),
                    cc_internal_kw_value(&kwargs, "artifact_name_patterns", empty_list),
                ),
                (
                    "_exec_os_DO_NOT_USE".to_owned(),
                    eval.heap().alloc(bazel_cc_exec_os()),
                ),
                (
                    "_features_DO_NOT_USE".to_owned(),
                    cc_internal_kw_value(&kwargs, "features", empty_list),
                ),
                (
                    "abi_libc_version".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "abi_libc_version", empty_string),
                ),
                (
                    "abi_version".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "abi_version", empty_string),
                ),
                (
                    "builtin_sysroot".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "builtin_sysroot", empty_string),
                ),
                (
                    "compiler".to_owned(),
                    cc_internal_kw_value(&kwargs, "compiler", empty_string),
                ),
                (
                    "cxx_builtin_include_directories".to_owned(),
                    cc_internal_kw_value(&kwargs, "cxx_builtin_include_directories", empty_list),
                ),
                (
                    "make_variables".to_owned(),
                    cc_internal_kw_value(&kwargs, "make_variables", empty_list),
                ),
                (
                    "target_cpu".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "target_cpu", empty_string),
                ),
                (
                    "target_libc".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "target_libc", empty_string),
                ),
                (
                    "target_system_name".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "target_system_name", empty_string),
                ),
                (
                    "tool_paths".to_owned(),
                    cc_internal_kw_value(&kwargs, "tool_paths", empty_list),
                ),
                (
                    "toolchain_id".to_owned(),
                    cc_internal_kw_value(&kwargs, "toolchain_identifier", empty_string),
                ),
            ],
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
