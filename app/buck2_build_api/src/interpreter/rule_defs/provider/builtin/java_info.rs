/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;
use std::sync::Arc;

use allocative::Allocative;
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
use starlark::starlark_module;
use starlark::values::Demand;
use starlark::values::Freeze;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark_map::StarlarkHasher;

use crate::interpreter::rule_defs::provider::ProviderLike;
use crate::interpreter::rule_defs::provider::callable::provider_callable_equals;
use crate::interpreter::rule_defs::provider::callable::provider_callable_write_hash;

const JAVA_INFO: &str = "JavaInfo";
const JAVA_PLUGIN_INFO: &str = "JavaPluginInfo";
const JAVA_RUNTIME_INFO: &str = "JavaRuntimeInfo";
const JAVA_TOOLCHAIN_INFO: &str = "JavaToolchainInfo";
const BOOT_CLASS_PATH_INFO: &str = "BootClassPathInfo";
const JAVA_RUNTIME_CLASSPATH_INFO: &str = "JavaRuntimeClasspathInfo";
const JAVA_PLUGIN_DATA_INFO: &str = "JavaPluginDataInfo";
const JAVA_COMPILATION_INFO: &str = "JavaCompilationInfo";

#[derive(Clone, Debug, Freeze, ProvidesStaticType, Trace, Allocative)]
#[repr(C)]
pub struct JavaProviderGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[trace(unsafe_ignore)]
    id: Arc<ProviderId>,
    #[freeze(identity)]
    name: &'static str,
    values: Box<[(String, V)]>,
}

starlark::starlark_complex_value!(pub JavaProvider);

unsafe impl<FromV, ToV> Coerce<JavaProviderGen<ToV>> for JavaProviderGen<FromV>
where
    FromV: ValueLifetimeless + Coerce<ToV>,
    ToV: ValueLifetimeless,
{
}

impl<V: ValueLifetimeless> fmt::Display for JavaProviderGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(<{} field(s)>)", self.name, self.values.len())
    }
}

impl<'v, V: ValueLike<'v>> serde::Serialize for JavaProviderGen<V> {
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

#[starlark_value(type = "JavaProvider")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for JavaProviderGen<V>
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

impl<'v, V: ValueLike<'v>> ProviderLike<'v> for JavaProviderGen<V> {
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
struct JavaProviderCallable {
    name: &'static str,
    id: Arc<ProviderId>,
}

impl JavaProviderCallable {
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

impl fmt::Display for JavaProviderCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

starlark::starlark_simple_value!(JavaProviderCallable);

#[starlark_value(type = "java_provider_callable")]
impl<'v> StarlarkValue<'v> for JavaProviderCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        Ok(eval
            .heap()
            .alloc(make_java_provider_from_kwargs(
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

impl ProviderCallableLike for JavaProviderCallable {
    fn id(&self) -> buck2_error::Result<&Arc<ProviderId>> {
        Ok(&self.id)
    }
}

fn make_java_provider_from_kwargs<'v>(
    name: &'static str,
    id: Arc<ProviderId>,
    kwargs: SmallMap<StringValue<'v>, Value<'v>>,
) -> JavaProvider<'v> {
    let mut values = kwargs
        .into_iter()
        .map(|(name, value)| (name.as_str().to_owned(), value))
        .collect::<Vec<_>>();
    values.sort_by(|(left, _), (right, _)| left.cmp(right));
    JavaProvider {
        id,
        name,
        values: values.into_boxed_slice(),
    }
}

fn empty_java_provider<'v>(name: &'static str) -> JavaProvider<'v> {
    let callable = JavaProviderCallable::new(name);
    JavaProvider {
        id: callable.id,
        name,
        values: Box::new([]),
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct JavaCommon;

impl fmt::Display for JavaCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("java_common")
    }
}

starlark::starlark_simple_value!(JavaCommon);

#[starlark_value(type = "java_common")]
impl<'v> StarlarkValue<'v> for JavaCommon {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let provider = match attribute {
            JAVA_RUNTIME_INFO => JAVA_RUNTIME_INFO,
            JAVA_TOOLCHAIN_INFO => JAVA_TOOLCHAIN_INFO,
            BOOT_CLASS_PATH_INFO => BOOT_CLASS_PATH_INFO,
            JAVA_RUNTIME_CLASSPATH_INFO => JAVA_RUNTIME_CLASSPATH_INFO,
            _ => return None,
        };
        Some(heap.alloc(JavaProviderCallable::new(provider)).to_value())
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "BootClassPathInfo".to_owned(),
            "JavaRuntimeClasspathInfo".to_owned(),
            "JavaRuntimeInfo".to_owned(),
            "JavaToolchainInfo".to_owned(),
        ]
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(java_common_methods)
    }
}

#[starlark_module]
fn java_common_methods(builder: &mut MethodsBuilder) {
    fn merge<'v>(
        #[starlark(this)] _this: &JavaCommon,
        #[starlark(require = pos)] _providers: Value<'v>,
    ) -> starlark::Result<JavaProvider<'v>> {
        Ok(empty_java_provider(JAVA_INFO))
    }

    fn compile<'v>(
        #[starlark(this)] _this: &JavaCommon,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        Ok(eval.heap().alloc(empty_java_provider(JAVA_INFO)).to_value())
    }

    fn default_javac_opts<'v>(
        #[starlark(this)] _this: &JavaCommon,
        #[starlark(args)] _args: starlark::values::tuple::UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(Vec::<String>::new()).to_value())
    }

    fn internal_DO_NOT_USE(
        #[starlark(this)] _this: &JavaCommon,
    ) -> starlark::Result<JavaCommonInternal> {
        Ok(JavaCommonInternal)
    }

    fn incompatible_disable_non_executable_java_binary(
        #[starlark(this)] _this: &JavaCommon,
    ) -> starlark::Result<bool> {
        Ok(false)
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct JavaCommonInternal;

impl fmt::Display for JavaCommonInternal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("java_common.internal")
    }
}

starlark::starlark_simple_value!(JavaCommonInternal);

#[starlark_value(type = "java_common_internal")]
impl<'v> StarlarkValue<'v> for JavaCommonInternal {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let provider = match attribute {
            JAVA_PLUGIN_DATA_INFO => JAVA_PLUGIN_DATA_INFO,
            JAVA_COMPILATION_INFO => JAVA_COMPILATION_INFO,
            _ => return None,
        };
        Some(heap.alloc(JavaProviderCallable::new(provider)).to_value())
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "JavaCompilationInfo".to_owned(),
            "JavaPluginDataInfo".to_owned(),
        ]
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(java_common_internal_methods)
    }
}

#[starlark_module]
fn java_common_internal_methods(builder: &mut MethodsBuilder) {
    fn expand_java_opts<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(args)] _args: starlark::values::tuple::UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(Vec::<String>::new()).to_value())
    }

    fn target_kind<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(args)] _args: starlark::values::tuple::UnpackTuple<Value<'v>>,
    ) -> starlark::Result<&'static str> {
        Ok("")
    }

    fn run_ijar_private_for_builtins<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(args)] _args: starlark::values::tuple::UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }

    fn compile<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        Ok(eval.heap().alloc(empty_java_provider(JAVA_INFO)).to_value())
    }

    fn collect_native_deps_dirs<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(args)] _args: starlark::values::tuple::UnpackTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(Vec::<String>::new()).to_value())
    }

    fn get_runtime_classpath_for_archive<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(args)] _args: starlark::values::tuple::UnpackTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(Vec::<String>::new()).to_value())
    }

    fn to_java_binary_info<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(args)] _args: starlark::values::tuple::UnpackTuple<Value<'v>>,
    ) -> starlark::Result<JavaProvider<'v>> {
        Ok(empty_java_provider(JAVA_INFO))
    }

    fn incompatible_disable_non_executable_java_binary(
        #[starlark(this)] _this: &JavaCommonInternal,
    ) -> starlark::Result<bool> {
        Ok(false)
    }
}

pub(crate) fn register_java_common(globals: &mut GlobalsBuilder) {
    globals.set(JAVA_INFO, JavaProviderCallable::new(JAVA_INFO));
    globals.set(
        JAVA_PLUGIN_INFO,
        JavaProviderCallable::new(JAVA_PLUGIN_INFO),
    );
    globals.set("java_common", JavaCommon);
}
