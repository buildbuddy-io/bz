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
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;
use starlark_map::StarlarkHasher;

use crate::interpreter::rule_defs::cmd_args::StarlarkCmdArgs;
use crate::interpreter::rule_defs::context::bazel_shell_tokenize;
use crate::interpreter::rule_defs::depset::BazelDepset;
use crate::interpreter::rule_defs::depset::bazel_depset_from_values;
use crate::interpreter::rule_defs::depset::bazel_depset_to_list;
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

fn java_common_error(message: impl std::fmt::Display) -> buck2_error::Error {
    buck2_error::buck2_error!(buck2_error::ErrorTag::Input, "{}", message)
}

fn java_attr<'v>(value: Value<'v>, attr: &str, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
    value.get_attr(attr, heap)?.ok_or_else(|| {
        java_common_error(format!(
            "Object of type `{}` has no attribute `{}`",
            value.get_type(),
            attr
        ))
        .into()
    })
}

fn java_plugin_data_field<'v>(
    plugin_info: Value<'v>,
    plugin_data: &str,
    field: &str,
    heap: Heap<'v>,
) -> starlark::Result<Value<'v>> {
    if plugin_info.is_none() {
        return Ok(Value::new_none());
    }
    java_attr(java_attr(plugin_info, plugin_data, heap)?, field, heap)
}

fn java_arg<'v>(
    args: &starlark::values::tuple::UnpackTuple<Value<'v>>,
    kwargs: &SmallMap<String, Value<'v>>,
    index: usize,
    name: &str,
) -> Value<'v> {
    args.items
        .get(index)
        .copied()
        .or_else(|| kwargs.get(name).copied())
        .unwrap_or_else(Value::new_none)
}

fn java_files_to_run_executable<'v>(files_to_run: Value<'v>) -> starlark::Result<Value<'v>> {
    let Some(files_to_run) = StructRef::from_value(files_to_run) else {
        return Err(java_common_error(format!(
            "expected FilesToRunProvider struct, got `{}`",
            files_to_run.get_type()
        ))
        .into());
    };
    files_to_run
        .iter()
        .find_map(|(name, value)| {
            (name.as_str() == "executable" && !value.is_none()).then_some(value)
        })
        .ok_or_else(|| {
            java_common_error("FilesToRunProvider does not contain an executable").into()
        })
}

fn java_collection_values<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Value<'v>>> {
    if value.is_none() {
        return Ok(Vec::new());
    }
    if BazelDepset::from_value(value).is_some() {
        return bazel_depset_to_list(value);
    }
    if let Some(list) = ListRef::from_value(value) {
        return Ok(list.iter().collect());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return Ok(tuple.iter().collect());
    }
    Ok(value
        .iterate(heap)
        .map_err(|_| {
            java_common_error(format!(
                "expected value of type `sequence or depset`, got `{}`",
                value.get_type()
            ))
        })?
        .collect())
}

fn java_string_values<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<Vec<Value<'v>>> {
    java_collection_values(value, heap)?
        .into_iter()
        .map(|value| {
            if value.unpack_str().is_some() {
                Ok(value)
            } else {
                Err(java_common_error(format!(
                    "expected string in Java option depset, got `{}`",
                    value.get_type()
                ))
                .into())
            }
        })
        .collect()
}

fn java_string_list<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<Vec<String>> {
    java_string_values(value, heap)?
        .into_iter()
        .map(|value| {
            Ok(value
                .unpack_str()
                .expect("java_string_values only returns strings")
                .to_owned())
        })
        .collect()
}

fn java_tokenized_option_values<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Value<'v>>> {
    if value.is_none() {
        return Ok(Vec::new());
    }
    let mut values = if BazelDepset::from_value(value).is_some() {
        let mut values = bazel_depset_to_list(value)?;
        values.reverse();
        values
    } else {
        java_collection_values(value, heap)?
    };

    let mut tokens = Vec::new();
    for value in values.drain(..) {
        let Some(value) = value.unpack_str() else {
            return Err(java_common_error(format!(
                "expected string in javac options, got `{}`",
                value.get_type()
            ))
            .into());
        };
        for token in bazel_shell_tokenize(value)? {
            tokens.push(heap.alloc_str(&token).to_value());
        }
    }
    Ok(tokens)
}

fn java_add_flag<'v>(argv: &mut Vec<Value<'v>>, heap: Heap<'v>, flag: &str) {
    argv.push(heap.alloc_str(flag).to_value());
}

fn java_add_flag_value<'v>(
    argv: &mut Vec<Value<'v>>,
    heap: Heap<'v>,
    flag: &str,
    value: Value<'v>,
) {
    if !value.is_none() {
        java_add_flag(argv, heap, flag);
        argv.push(value);
    }
}

fn java_add_flag_values<'v>(
    argv: &mut Vec<Value<'v>>,
    heap: Heap<'v>,
    flag: &str,
    values: Vec<Value<'v>>,
) {
    if !values.is_empty() {
        java_add_flag(argv, heap, flag);
        argv.extend(values);
    }
}

fn java_bootclasspath<'v>(
    java_toolchain: Value<'v>,
    bootclasspath: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Value<'v>> {
    if !bootclasspath.is_none()
        && let Some(value) = bootclasspath.get_attr("bootclasspath", heap)?
    {
        return Ok(value);
    }
    java_attr(java_toolchain, "bootclasspath", heap)
}

fn java_tool_command<'v>(
    java_toolchain: Value<'v>,
    tool_attr: &str,
    extra_jvm_flags: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<(Value<'v>, Vec<Value<'v>>, Vec<Value<'v>>, usize)> {
    let tool = java_attr(java_toolchain, tool_attr, heap)?;
    let files_to_run = java_attr(tool, "tool", heap)?;
    let tool_executable = java_files_to_run_executable(files_to_run)?;
    let tool_data = java_attr(tool, "data", heap)?;
    let tool_jvm_opts = java_attr(tool, "jvm_opts", heap)?;
    let tool_executable_extension = java_attr(tool_executable, "extension", heap)?;
    let tool_executable_extension = tool_executable_extension.unpack_str().unwrap_or("");

    let mut inputs = vec![
        files_to_run,
        tool_data,
        java_attr(java_toolchain, "tools", heap)?,
    ];
    let mut argv = Vec::new();

    if tool_executable_extension == "jar" {
        let java_runtime = java_attr(java_toolchain, "java_runtime", heap)?;
        let java_executable = java_attr(java_runtime, "java_executable_exec_path", heap)?;
        inputs.push(java_attr(java_runtime, "files", heap)?);
        argv.extend(java_string_values(
            java_attr(java_toolchain, "jvm_opt", heap)?,
            heap,
        )?);
        argv.extend(java_string_values(tool_jvm_opts, heap)?);
        argv.extend(java_string_values(extra_jvm_flags, heap)?);
        java_add_flag(&mut argv, heap, "-jar");
        argv.push(tool_executable);
        let param_file_start = argv.len();
        Ok((java_executable, argv, inputs, param_file_start))
    } else {
        argv.extend(java_string_values(tool_jvm_opts, heap)?);
        argv.extend(java_string_values(extra_jvm_flags, heap)?);
        let param_file_start = argv.len();
        Ok((tool_executable, argv, inputs, param_file_start))
    }
}

fn java_call_run_action<'v>(
    ctx: Value<'v>,
    executable: Value<'v>,
    argv: Vec<Value<'v>>,
    inputs: Vec<Value<'v>>,
    outputs: Vec<Value<'v>>,
    mnemonic: &str,
    param_file_start: usize,
    execution_requirements: Option<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let heap = eval.heap();
    let actions = java_attr(ctx, "actions", heap)?;
    let run = actions
        .get_attr("run", heap)?
        .ok_or_else(|| java_common_error("ctx.actions has no `run` method"))?;
    let arguments = heap.alloc(AllocList(java_param_file_arguments(
        argv,
        param_file_start,
        heap,
    )?));
    let mut kwargs = vec![
        ("executable", executable),
        ("arguments", arguments),
        ("inputs", heap.alloc(AllocList(inputs))),
        ("outputs", heap.alloc(AllocList(outputs))),
        ("mnemonic", heap.alloc_str(mnemonic).to_value()),
        ("use_default_shell_env", Value::new_bool(true)),
    ];
    if let Some(execution_requirements) = execution_requirements {
        kwargs.push(("execution_requirements", execution_requirements));
    }
    eval.eval_function(run, &[], &kwargs)?;
    Ok(NoneType)
}

fn java_param_file_arguments<'v>(
    argv: Vec<Value<'v>>,
    param_file_start: usize,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Value<'v>>> {
    if param_file_start >= argv.len() {
        return Ok(argv);
    }

    let mut argv = argv;
    let param_file_values = argv.split_off(param_file_start);
    let param_file = StarlarkCmdArgs::from_values_with_bazel_param_file(
        param_file_values,
        heap.alloc_str("@{}"),
        "UNQUOTED",
    )?;
    argv.push(heap.alloc(param_file));
    Ok(argv)
}

fn java_bool_attr<'v>(value: Value<'v>, attr: &str, heap: Heap<'v>) -> starlark::Result<bool> {
    java_attr(value, attr, heap)?.unpack_bool().ok_or_else(|| {
        java_common_error(format!("Java toolchain attribute `{attr}` is not a bool")).into()
    })
}

fn java_compile_execution_requirements<'v>(
    java_toolchain: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Value<'v>> {
    let mut requirements = vec![("supports-path-mapping", "1")];
    if java_bool_attr(java_toolchain, "_javac_supports_workers", heap)? {
        requirements.push(("supports-workers", "1"));
    }
    if java_bool_attr(java_toolchain, "_javac_supports_multiplex_workers", heap)? {
        requirements.push(("supports-multiplex-workers", "1"));
    }
    if java_bool_attr(java_toolchain, "_javac_supports_worker_cancellation", heap)? {
        requirements.push(("supports-worker-cancellation", "1"));
    }
    if java_bool_attr(
        java_toolchain,
        "_javac_supports_worker_multiplex_sandboxing",
        heap,
    )? {
        requirements.push(("supports-multiplex-sandboxing", "1"));
    }
    Ok(heap.alloc(AllocDict(requirements)))
}

fn java_push_output<'v>(outputs: &mut Vec<Value<'v>>, output: Value<'v>) {
    if !output.is_none() {
        outputs.push(output);
    }
}

fn java_register_gen_class_action<'v>(
    ctx: Value<'v>,
    java_toolchain: Value<'v>,
    class_jar: Value<'v>,
    manifest_proto: Value<'v>,
    gen_class_jar: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    if gen_class_jar.is_none() {
        return Ok(NoneType);
    }
    let heap = eval.heap();
    let java_runtime = java_attr(java_toolchain, "java_runtime", heap)?;
    let java_executable = java_attr(java_runtime, "java_executable_exec_path", heap)?;
    let gen_class_tool = java_attr(java_toolchain, "_gen_class", heap)?;
    if gen_class_tool.is_none() {
        return Err(java_common_error("Java toolchain does not provide `_gen_class`").into());
    }

    let mut argv = Vec::new();
    argv.extend(java_string_values(
        java_attr(java_toolchain, "jvm_opt", heap)?,
        heap,
    )?);
    java_add_flag(&mut argv, heap, "-jar");
    argv.push(gen_class_tool);
    let param_file_start = argv.len();
    java_add_flag_value(&mut argv, heap, "--manifest_proto", manifest_proto);
    java_add_flag_value(&mut argv, heap, "--class_jar", class_jar);
    java_add_flag_value(&mut argv, heap, "--output_jar", gen_class_jar);

    let inputs = vec![
        manifest_proto,
        class_jar,
        gen_class_tool,
        java_attr(java_runtime, "files", heap)?,
    ];
    java_call_run_action(
        ctx,
        java_executable,
        argv,
        inputs,
        vec![gen_class_jar],
        "JavaSourceJar",
        param_file_start,
        None,
        eval,
    )
}

fn java_register_header_compilation_action<'v>(
    args: &starlark::values::tuple::UnpackTuple<Value<'v>>,
    kwargs: &SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let heap = eval.heap();
    let ctx = java_arg(args, kwargs, 0, "ctx");
    let java_toolchain = java_arg(args, kwargs, 1, "java_toolchain");
    let compile_jar = java_arg(args, kwargs, 2, "compile_jar");
    let compile_deps_proto = java_arg(args, kwargs, 3, "compile_deps_proto");
    let plugin_info = java_arg(args, kwargs, 4, "plugin_info");
    let source_files = java_arg(args, kwargs, 5, "source_files");
    let source_jars = java_arg(args, kwargs, 6, "source_jars");
    let compilation_classpath = java_arg(args, kwargs, 7, "compilation_classpath");
    let direct_jars = java_arg(args, kwargs, 8, "direct_jars");
    let bootclasspath = java_arg(args, kwargs, 9, "bootclasspath");
    let compile_time_java_deps = java_arg(args, kwargs, 10, "compile_time_java_deps");
    let javac_opts = java_arg(args, kwargs, 11, "javac_opts");
    let strict_deps_mode = java_arg(args, kwargs, 12, "strict_deps_mode");
    let target_label = java_arg(args, kwargs, 13, "target_label");
    let injecting_rule_kind = java_arg(args, kwargs, 14, "injecting_rule_kind");
    let additional_inputs = java_arg(args, kwargs, 16, "additional_inputs");
    let header_compilation_jar = java_arg(args, kwargs, 17, "header_compilation_jar");
    let header_compilation_direct_deps =
        java_arg(args, kwargs, 18, "header_compilation_direct_deps");

    let processor_classes = java_plugin_data_field(
        plugin_info,
        "api_generating_plugins",
        "processor_classes",
        heap,
    )?;
    let processor_class_names = java_string_list(processor_classes, heap)?;
    let builtin_processor_names = java_string_list(
        java_attr(java_toolchain, "_header_compiler_builtin_processors", heap)?,
        heap,
    )?;
    let use_header_compiler_direct = java_attr(java_toolchain, "_header_compiler_direct", heap)
        .is_ok_and(|tool| !tool.is_none())
        && processor_class_names
            .iter()
            .all(|processor| builtin_processor_names.contains(processor));

    let tool_attr = if use_header_compiler_direct {
        "_header_compiler_direct"
    } else {
        "_header_compiler"
    };
    let (executable, mut argv, mut inputs, param_file_start) =
        java_tool_command(java_toolchain, tool_attr, Value::new_none(), heap)?;

    java_add_flag_value(&mut argv, heap, "--output", compile_jar);
    java_add_flag_value(
        &mut argv,
        heap,
        "--header_compilation_output",
        header_compilation_jar,
    );
    java_add_flag_value(&mut argv, heap, "--output_deps", compile_deps_proto);
    let bootclasspath = java_bootclasspath(java_toolchain, bootclasspath, heap)?;
    java_add_flag_values(
        &mut argv,
        heap,
        "--bootclasspath",
        java_collection_values(bootclasspath, heap)?,
    );
    java_add_flag_values(
        &mut argv,
        heap,
        "--sources",
        java_collection_values(source_files, heap)?,
    );
    java_add_flag_values(
        &mut argv,
        heap,
        "--source_jars",
        java_collection_values(source_jars, heap)?,
    );
    java_add_flag(&mut argv, heap, "--javacopts");
    argv.extend(java_tokenized_option_values(javac_opts, heap)?);
    java_add_flag(&mut argv, heap, "-Aexperimental_turbine_hjar");
    java_add_flag(&mut argv, heap, "--");
    java_add_flag_value(&mut argv, heap, "--target_label", target_label);
    java_add_flag_value(
        &mut argv,
        heap,
        "--injecting_rule_kind",
        injecting_rule_kind,
    );
    let processor_jars = java_plugin_data_field(
        plugin_info,
        "api_generating_plugins",
        "processor_jars",
        heap,
    )?;
    let processor_data = java_plugin_data_field(
        plugin_info,
        "api_generating_plugins",
        "processor_data",
        heap,
    )?;
    let builtin_processors = java_collection_values(processor_classes, heap)?
        .into_iter()
        .filter(|processor| {
            processor
                .unpack_str()
                .is_some_and(|processor| builtin_processor_names.iter().any(|p| p == processor))
        })
        .collect();
    java_add_flag_values(&mut argv, heap, "--builtin_processors", builtin_processors);
    java_add_flag_values(
        &mut argv,
        heap,
        "--processors",
        java_collection_values(processor_classes, heap)?,
    );
    if !use_header_compiler_direct {
        java_add_flag_values(
            &mut argv,
            heap,
            "--processorpath",
            java_collection_values(processor_jars, heap)?,
        );
    }
    java_add_flag_values(
        &mut argv,
        heap,
        "--classpath",
        java_collection_values(compilation_classpath, heap)?,
    );
    if strict_deps_mode
        .unpack_str()
        .is_some_and(|mode| mode != "OFF")
    {
        java_add_flag_values(
            &mut argv,
            heap,
            "--direct_dependencies",
            java_collection_values(direct_jars, heap)?,
        );
    }

    inputs.extend([
        bootclasspath,
        source_files,
        source_jars,
        compilation_classpath,
        direct_jars,
        compile_time_java_deps,
        additional_inputs,
        header_compilation_direct_deps,
    ]);
    if !use_header_compiler_direct {
        inputs.extend([processor_jars, processor_data]);
    }
    let mut outputs = Vec::new();
    java_push_output(&mut outputs, compile_jar);
    java_push_output(&mut outputs, compile_deps_proto);
    java_push_output(&mut outputs, header_compilation_jar);

    java_call_run_action(
        ctx,
        executable,
        argv,
        inputs,
        outputs,
        "JavacTurbine",
        param_file_start,
        None,
        eval,
    )
}

fn java_register_compilation_action<'v>(
    args: &starlark::values::tuple::UnpackTuple<Value<'v>>,
    kwargs: &SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let heap = eval.heap();
    let ctx = java_arg(args, kwargs, 0, "ctx");
    let java_toolchain = java_arg(args, kwargs, 1, "java_toolchain");
    let output = java_arg(args, kwargs, 2, "output");
    let manifest_proto = java_arg(args, kwargs, 3, "manifest_proto");
    let plugin_info = java_arg(args, kwargs, 4, "plugin_info");
    let compilation_classpath = java_arg(args, kwargs, 5, "compilation_classpath");
    let direct_jars = java_arg(args, kwargs, 6, "direct_jars");
    let bootclasspath = java_arg(args, kwargs, 7, "bootclasspath");
    let javabuilder_jvm_flags = java_arg(args, kwargs, 8, "javabuilder_jvm_flags");
    let compile_time_java_deps = java_arg(args, kwargs, 9, "compile_time_java_deps");
    let javac_opts = java_arg(args, kwargs, 10, "javac_opts");
    let strict_deps_mode = java_arg(args, kwargs, 11, "strict_deps_mode");
    let target_label = java_arg(args, kwargs, 12, "target_label");
    let deps_proto = java_arg(args, kwargs, 13, "deps_proto");
    let gen_class = java_arg(args, kwargs, 14, "gen_class");
    let gen_source = java_arg(args, kwargs, 15, "gen_source");
    let native_header_jar = java_arg(args, kwargs, 16, "native_header_jar");
    let sources = java_arg(args, kwargs, 17, "sources");
    let source_jars = java_arg(args, kwargs, 18, "source_jars");
    let resources = java_arg(args, kwargs, 19, "resources");
    let resource_jars = java_arg(args, kwargs, 20, "resource_jars");
    let classpath_resources = java_arg(args, kwargs, 21, "classpath_resources");
    let sourcepath = java_arg(args, kwargs, 22, "sourcepath");
    let injecting_rule_kind = java_arg(args, kwargs, 23, "injecting_rule_kind");
    let additional_inputs = java_arg(args, kwargs, 26, "additional_inputs");
    let additional_outputs = java_arg(args, kwargs, 27, "additional_outputs");

    let (executable, mut argv, mut inputs, param_file_start) =
        java_tool_command(java_toolchain, "_javabuilder", javabuilder_jvm_flags, heap)?;

    java_add_flag_value(&mut argv, heap, "--output", output);
    java_add_flag_value(&mut argv, heap, "--native_header_output", native_header_jar);
    java_add_flag_value(&mut argv, heap, "--generated_sources_output", gen_source);
    java_add_flag_value(&mut argv, heap, "--output_manifest_proto", manifest_proto);
    java_add_flag(&mut argv, heap, "--compress_jar");
    java_add_flag_value(&mut argv, heap, "--output_deps_proto", deps_proto);
    let bootclasspath = java_bootclasspath(java_toolchain, bootclasspath, heap)?;
    java_add_flag_values(
        &mut argv,
        heap,
        "--bootclasspath",
        java_collection_values(bootclasspath, heap)?,
    );
    java_add_flag_values(
        &mut argv,
        heap,
        "--sourcepath",
        java_collection_values(sourcepath, heap)?,
    );
    let processor_classes =
        java_plugin_data_field(plugin_info, "plugins", "processor_classes", heap)?;
    let processor_jars = java_plugin_data_field(plugin_info, "plugins", "processor_jars", heap)?;
    let processor_data = java_plugin_data_field(plugin_info, "plugins", "processor_data", heap)?;
    java_add_flag_values(
        &mut argv,
        heap,
        "--processorpath",
        java_collection_values(processor_jars, heap)?,
    );
    java_add_flag_values(
        &mut argv,
        heap,
        "--processors",
        java_collection_values(processor_classes, heap)?,
    );
    java_add_flag_values(
        &mut argv,
        heap,
        "--source_jars",
        java_collection_values(source_jars, heap)?,
    );
    java_add_flag_values(
        &mut argv,
        heap,
        "--sources",
        java_collection_values(sources, heap)?,
    );
    let javac_opts = java_tokenized_option_values(javac_opts, heap)?;
    if !javac_opts.is_empty() {
        java_add_flag(&mut argv, heap, "--javacopts");
        argv.extend(javac_opts);
        java_add_flag(&mut argv, heap, "--");
    }
    java_add_flag_value(&mut argv, heap, "--target_label", target_label);
    java_add_flag_value(
        &mut argv,
        heap,
        "--injecting_rule_kind",
        injecting_rule_kind,
    );
    if strict_deps_mode
        .unpack_str()
        .is_some_and(|mode| mode != "OFF")
    {
        java_add_flag_value(&mut argv, heap, "--strict_java_deps", strict_deps_mode);
        java_add_flag_values(
            &mut argv,
            heap,
            "--direct_dependencies",
            java_collection_values(direct_jars, heap)?,
        );
    }
    java_add_flag_value(
        &mut argv,
        heap,
        "--experimental_fix_deps_tool",
        heap.alloc_str("add_dep").to_value(),
    );
    java_add_flag_values(
        &mut argv,
        heap,
        "--classpath",
        java_collection_values(compilation_classpath, heap)?,
    );
    java_add_flag_value(
        &mut argv,
        heap,
        "--reduce_classpath_mode",
        heap.alloc_str("NONE").to_value(),
    );

    inputs.extend([
        bootclasspath,
        compilation_classpath,
        direct_jars,
        compile_time_java_deps,
        sources,
        source_jars,
        resources,
        resource_jars,
        classpath_resources,
        sourcepath,
        additional_inputs,
        processor_jars,
        processor_data,
    ]);
    let mut outputs = Vec::new();
    java_push_output(&mut outputs, output);
    java_push_output(&mut outputs, manifest_proto);
    java_push_output(&mut outputs, deps_proto);
    java_push_output(&mut outputs, gen_source);
    java_push_output(&mut outputs, native_header_jar);
    outputs.extend(java_collection_values(additional_outputs, heap)?);

    let execution_requirements = java_compile_execution_requirements(java_toolchain, heap)?;
    java_call_run_action(
        ctx,
        executable,
        argv,
        inputs,
        outputs,
        "Javac",
        param_file_start,
        Some(execution_requirements),
        eval,
    )?;
    java_register_gen_class_action(ctx, java_toolchain, output, manifest_proto, gen_class, eval)
}

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
    fn check_provider_instances<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(require = pos)] providers: Value<'v>,
        #[starlark(require = pos)] what: &str,
        #[starlark(require = pos)] provider_type: Value<'v>,
    ) -> starlark::Result<NoneType> {
        let Some(provider_callable) = provider_type.request_value::<&dyn ProviderCallableLike>()
        else {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "wanted Provider, got {}",
                provider_type.get_type()
            )
            .into());
        };
        let provider_id = provider_callable.id()?;
        let values = if let Some(list) = ListRef::from_value(providers) {
            list.iter().collect::<Vec<_>>()
        } else if let Some(tuple) = TupleRef::from_value(providers) {
            tuple.iter().collect::<Vec<_>>()
        } else {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "expected list or tuple for {}, got {}",
                what,
                providers.get_type()
            )
            .into());
        };
        for (index, value) in values.into_iter().enumerate() {
            let Some(provider) = value.request_value::<&dyn ProviderLike>() else {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "at index {} of {}, got element of type {}, want {}",
                    index,
                    what,
                    value.get_type(),
                    provider_type.to_repr()
                )
                .into());
            };
            if provider.id() != provider_id {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "at index {} of {}, got element of type {}, want {}",
                    index,
                    what,
                    value.get_type(),
                    provider_type.to_repr()
                )
                .into());
            }
        }
        Ok(NoneType)
    }

    fn expand_java_opts<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(args)] args: starlark::values::tuple::UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        let ctx = java_arg(&args, &kwargs, 0, "ctx");
        let attr_name = java_arg(&args, &kwargs, 1, "attr")
            .unpack_str()
            .ok_or_else(|| java_common_error("expand_java_opts expected `attr` to be a string"))?;
        let tokenize = java_arg(&args, &kwargs, 2, "tokenize")
            .unpack_bool()
            .unwrap_or(false);
        let exec_paths = java_arg(&args, &kwargs, 3, "exec_paths")
            .unpack_bool()
            .unwrap_or(false);

        let attrs = java_attr(ctx, "attr", heap)?;
        let opts = java_collection_values(java_attr(attrs, attr_name, heap)?, heap)?;
        let expand_location = ctx
            .get_attr("expand_location", heap)?
            .ok_or_else(|| java_common_error("ctx has no `expand_location` method"))?;
        let mut expanded_opts = Vec::new();
        for opt in opts {
            let Some(opt) = opt.unpack_str() else {
                return Err(java_common_error(format!(
                    "expected string in Java option `{attr_name}`, got `{}`",
                    opt.get_type()
                ))
                .into());
            };
            let expanded = eval.eval_function(
                expand_location,
                &[heap.alloc_str(opt).to_value()],
                &[("short_paths", Value::new_bool(!exec_paths))],
            )?;
            let Some(expanded) = expanded.unpack_str() else {
                return Err(java_common_error(format!(
                    "ctx.expand_location returned `{}`, want string",
                    expanded.get_type()
                ))
                .into());
            };
            if tokenize {
                for token in bazel_shell_tokenize(expanded)? {
                    expanded_opts.push(heap.alloc_str(&token).to_value());
                }
            } else {
                expanded_opts.push(heap.alloc_str(expanded).to_value());
            }
        }
        Ok(heap.alloc(AllocList(expanded_opts)).to_value())
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

    fn create_header_compilation_action<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(args)] args: starlark::values::tuple::UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        java_register_header_compilation_action(&args, &kwargs, eval)
    }

    fn create_compilation_action<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(args)] args: starlark::values::tuple::UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        java_register_compilation_action(&args, &kwargs, eval)
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
        #[starlark(args)] args: starlark::values::tuple::UnpackTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let runtime_classpath = args.items.first().copied().unwrap_or_else(Value::new_none);
        let excluded_artifacts = args.items.get(1).copied().unwrap_or_else(Value::new_none);
        if excluded_artifacts.is_none()
            || java_collection_values(excluded_artifacts, eval.heap())?.is_empty()
        {
            return Ok(runtime_classpath);
        }
        let excluded = java_collection_values(excluded_artifacts, eval.heap())?;
        let mut filtered = Vec::new();
        'candidate: for candidate in java_collection_values(runtime_classpath, eval.heap())? {
            for excluded in &excluded {
                if candidate.equals(*excluded)? {
                    continue 'candidate;
                }
            }
            filtered.push(candidate);
        }
        bazel_depset_from_values(eval.heap(), filtered)
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

    fn incompatible_java_info_merge_runtime_module_flags(
        #[starlark(this)] _this: &JavaCommonInternal,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn _incompatible_java_info_merge_runtime_module_flags(
        #[starlark(this)] _this: &JavaCommonInternal,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn google_legacy_api_enabled(
        #[starlark(this)] _this: &JavaCommonInternal,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn _google_legacy_api_enabled(
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
