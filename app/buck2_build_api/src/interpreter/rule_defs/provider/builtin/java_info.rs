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
use buck2_core::fs::buck_out_path::BazelOutputPathKind;
use buck2_core::provider::id::ProviderId;
use buck2_interpreter::types::provider::callable::ProviderCallableLike;
use buck2_util::late_binding::LateBinding;
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
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueTyped;
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::none::NoneOr;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::structs::StructRef;
use starlark::values::tuple::AllocTuple;
use starlark::values::tuple::TupleRef;
use starlark_map::StarlarkHasher;

use crate::interpreter::rule_defs::artifact::starlark_artifact_like::StarlarkArtifactLike;
use crate::interpreter::rule_defs::cmd_args::StarlarkCmdArgs;
use crate::interpreter::rule_defs::context::AnalysisActions;
use crate::interpreter::rule_defs::context::bazel_analysis_context_declare_file_with_path_kind;
use crate::interpreter::rule_defs::context::bazel_shell_tokenize;
use crate::interpreter::rule_defs::depset::BazelDepset;
use crate::interpreter::rule_defs::depset::BazelDepsetOrder;
use crate::interpreter::rule_defs::depset::bazel_depset_from_direct_and_transitive_with_order;
use crate::interpreter::rule_defs::depset::bazel_depset_from_transitive;
use crate::interpreter::rule_defs::depset::bazel_depset_from_values;
use crate::interpreter::rule_defs::depset::bazel_depset_is_empty;
use crate::interpreter::rule_defs::depset::bazel_depset_to_list;
use crate::interpreter::rule_defs::provider::ProviderLike;
use crate::interpreter::rule_defs::provider::builtin::worker_info::WorkerInfo;
use crate::interpreter::rule_defs::provider::builtin::worker_info::synthetic_bazel_local_worker_info;
use crate::interpreter::rule_defs::provider::builtin::worker_run_info::synthetic_worker_run_info;
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

pub struct BazelJavaRunAction<'v> {
    pub actions: ValueTyped<'v, AnalysisActions<'v>>,
    pub executable: Value<'v>,
    pub arguments: Vec<Value<'v>>,
    pub inputs: Vec<Value<'v>>,
    pub outputs: Vec<Value<'v>>,
    pub mnemonic: StringValue<'v>,
    pub worker_executable: Option<Value<'v>>,
}

pub static BAZEL_JAVA_RUN_ACTION: LateBinding<
    for<'v, 'a, 'b, 'c> fn(
        BazelJavaRunAction<'v>,
        &'a mut Evaluator<'v, 'b, 'c>,
    ) -> starlark::Result<NoneType>,
> = LateBinding::new("BAZEL_JAVA_RUN_ACTION");

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
            .binary_search_by(|(name, _)| name.as_str().cmp(attribute))
            .ok()
            .map(|index| self.values[index].1.to_value())
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

fn java_replace_extension(basename: &str, replacement: &str) -> String {
    let extension_start = basename
        .as_bytes()
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, byte)| {
            if *byte == b'.' && index != 0 {
                Some(index)
            } else {
                None
            }
        });
    match extension_start {
        Some(index) => format!("{}{}", &basename[..index], replacement),
        None => format!("{basename}{replacement}"),
    }
}

fn java_bazel_output_dir_relative_path(exec_path: &str) -> &str {
    let Some(path) = exec_path
        .strip_prefix("buck-out/bin/")
        .or_else(|| exec_path.strip_prefix("buck-out/genfiles/"))
    else {
        return exec_path;
    };

    path.split_once('/').map_or(exec_path, |(_, path)| path)
}

fn java_derive_output_file<'v>(
    ctx: Value<'v>,
    base_file: Value<'v>,
    name_suffix: &str,
    extension: NoneOr<StringValue<'v>>,
    extension_suffix: &str,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let extension = extension.into_option();
    if name_suffix.is_empty() && extension_suffix.is_empty() && extension.is_none() {
        return Err(java_common_error(
            "At least one of name_suffix, extension or extension_suffix is required",
        )
        .into());
    }
    if extension.is_some() && !extension_suffix.is_empty() {
        return Err(java_common_error(
            "only one of extension or extension_suffix can be specified",
        )
        .into());
    }

    let base_file = <&dyn StarlarkArtifactLike<'v>>::unpack_value(base_file)?
        .ok_or_else(|| java_common_error("derive_output_file expected base_file to be a File"))?;
    let basename = base_file.with_filename(&|filename| eval.heap().alloc_str(filename.as_str()))?;
    let default_extension = base_file
        .with_filename(&|filename| eval.heap().alloc_str(filename.extension().unwrap_or("")))?;
    let extension = extension
        .map(|extension| extension.as_str())
        .unwrap_or_else(|| default_extension.as_str());
    let replacement = format!("{name_suffix}.{extension}{extension_suffix}");
    let new_basename = java_replace_extension(basename.as_str(), &replacement);
    let sibling_path = base_file.with_bazel_path(&|path| eval.heap().alloc_str(path))?;
    let sibling_path = java_bazel_output_dir_relative_path(sibling_path.as_str());
    let output_path = match sibling_path.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("{parent}/{new_basename}"),
        _ => new_basename,
    };

    bazel_analysis_context_declare_file_with_path_kind(
        ctx,
        &output_path,
        BazelOutputPathKind::OutputDirRelative,
        eval.heap(),
    )
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
        match bazel_shell_tokenize(value) {
            Ok(parts) => {
                for token in parts {
                    tokens.push(heap.alloc_str(&token).to_value());
                }
            }
            Err(_) => tokens.push(heap.alloc_str(value).to_value()),
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

fn java_add_flag_collection<'v>(
    argv: &mut Vec<Value<'v>>,
    heap: Heap<'v>,
    flag: &str,
    value: Value<'v>,
) -> starlark::Result<()> {
    if value.is_none() {
        return Ok(());
    }
    if BazelDepset::from_value(value).is_some() {
        if !bazel_depset_is_empty(value)? {
            java_add_flag(argv, heap, flag);
            argv.push(value);
        }
        return Ok(());
    }
    java_add_flag_values(argv, heap, flag, java_collection_values(value, heap)?);
    Ok(())
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
    worker_executable: Option<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let heap = eval.heap();
    let actions = java_attr(ctx, "actions", heap)?;
    let _unused = execution_requirements;
    let arguments = if worker_executable.is_some() {
        java_param_file_arguments(argv[param_file_start..].to_vec(), 0, heap)?
    } else {
        java_param_file_arguments(argv, param_file_start, heap)?
    };
    (BAZEL_JAVA_RUN_ACTION.get()?)(
        BazelJavaRunAction {
            actions: ValueTyped::new_err(actions)?,
            executable,
            arguments,
            inputs,
            outputs,
            mnemonic: heap.alloc_str(mnemonic),
            worker_executable,
        },
        eval,
    )?;
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

fn java_compile_worker_executable<'v>(
    java_toolchain: Value<'v>,
    executable: Value<'v>,
    argv: &[Value<'v>],
    param_file_start: usize,
    heap: Heap<'v>,
) -> starlark::Result<Option<Value<'v>>> {
    if !java_bool_attr(java_toolchain, "_javac_supports_workers", heap)? {
        return Ok(None);
    }

    let fixed_args = argv[..param_file_start].iter().copied();
    let fallback_exe = StarlarkCmdArgs::from_values(std::iter::once(executable).chain(fixed_args))?;

    let fixed_args = argv[..param_file_start].iter().copied();
    let worker_exe = StarlarkCmdArgs::from_values(std::iter::once(executable).chain(fixed_args))?;

    let concurrency = if java_bool_attr(java_toolchain, "_javac_supports_multiplex_workers", heap)?
    {
        Some(8)
    } else {
        Some(1)
    };
    let worker_info = synthetic_bazel_local_worker_info(
        worker_exe,
        concurrency,
        false, // Bazel only requires worker sandboxing for stripped output path mapping.
        heap,
    );
    let worker = ValueTypedComplex::<WorkerInfo>::new_err(heap.alloc(worker_info))?;
    let worker_run_info = synthetic_worker_run_info(worker, fallback_exe, heap);
    Ok(Some(heap.alloc(worker_run_info).to_value()))
}

fn java_push_output<'v>(outputs: &mut Vec<Value<'v>>, output: Value<'v>) {
    if !output.is_none() {
        outputs.push(output);
    }
}

fn java_depset_preorder<'v>(
    heap: Heap<'v>,
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
) -> starlark::Result<Value<'v>> {
    bazel_depset_from_direct_and_transitive_with_order(
        heap,
        direct,
        transitive,
        BazelDepsetOrder::Preorder,
    )
}

fn java_depset_topological<'v>(
    heap: Heap<'v>,
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
) -> starlark::Result<Value<'v>> {
    bazel_depset_from_direct_and_transitive_with_order(
        heap,
        direct,
        transitive,
        BazelDepsetOrder::Topological,
    )
}

fn java_attr_values<'v>(
    values: &[Value<'v>],
    attr: &str,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Value<'v>>> {
    values
        .iter()
        .map(|value| java_attr(*value, attr, heap))
        .collect()
}

fn java_annotation_processing_values<'v>(
    values: &[Value<'v>],
    attr: &str,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Value<'v>>> {
    let mut result = Vec::new();
    for value in values {
        let annotation_processing = java_attr(*value, "annotation_processing", heap)?;
        if annotation_processing.to_bool() {
            result.push(java_attr(annotation_processing, attr, heap)?);
        }
    }
    Ok(result)
}

fn java_has_plugin_data_value<'v>(
    plugin_data: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<bool> {
    if !plugin_data.to_bool() {
        return Ok(false);
    }
    Ok(java_attr(plugin_data, "processor_classes", heap)?.to_bool()
        || java_attr(plugin_data, "processor_jars", heap)?.to_bool()
        || java_attr(plugin_data, "processor_data", heap)?.to_bool())
}

fn java_merge_plugin_data<'v>(
    plugin_data_provider: Value<'v>,
    empty_plugin_data: Value<'v>,
    datas: Vec<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    let mut processor_classes = Vec::with_capacity(datas.len());
    let mut processor_jars = Vec::with_capacity(datas.len());
    let mut processor_data = Vec::with_capacity(datas.len());
    for data in datas {
        processor_classes.push(java_attr(data, "processor_classes", heap)?);
        processor_jars.push(java_attr(data, "processor_jars", heap)?);
        processor_data.push(java_attr(data, "processor_data", heap)?);
    }

    let processor_classes = bazel_depset_from_transitive(heap, processor_classes)?;
    let processor_jars = bazel_depset_from_transitive(heap, processor_jars)?;
    let processor_data = bazel_depset_from_transitive(heap, processor_data)?;
    if !processor_classes.to_bool() && !processor_jars.to_bool() && !processor_data.to_bool() {
        return Ok(empty_plugin_data);
    }

    eval.eval_function(
        plugin_data_provider,
        &[],
        &[
            ("processor_classes", processor_classes),
            ("processor_jars", processor_jars),
            ("processor_data", processor_data),
        ],
    )
}

#[allow(clippy::too_many_arguments)]
fn java_javainfo_init_base<'v>(
    java_output_info_provider: Value<'v>,
    java_rule_output_jars_info_provider: Value<'v>,
    java_gen_jars_info_provider: Value<'v>,
    java_plugin_data_provider: Value<'v>,
    empty_plugin_data: Value<'v>,
    output_jar: Value<'v>,
    compile_jar: Value<'v>,
    source_jar: Value<'v>,
    deps: Value<'v>,
    runtime_deps: Value<'v>,
    exports: Value<'v>,
    exported_plugins: Value<'v>,
    jdeps: Value<'v>,
    compile_jdeps: Value<'v>,
    native_headers_jar: Value<'v>,
    manifest_proto: Value<'v>,
    generated_class_jar: Value<'v>,
    generated_source_jar: Value<'v>,
    native_libraries: Value<'v>,
    neverlink: Value<'v>,
    header_compilation_jar: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    let deps = java_collection_values(deps, heap)?;
    let runtime_deps = java_collection_values(runtime_deps, heap)?;
    let exports = java_collection_values(exports, heap)?;
    let exported_plugins = java_collection_values(exported_plugins, heap)?;
    let native_libraries = java_collection_values(native_libraries, heap)?;

    let header_compilation_jar = if compile_jar.to_bool() && !header_compilation_jar.to_bool() {
        compile_jar
    } else {
        header_compilation_jar
    };

    let mut deps_exports = Vec::with_capacity(deps.len() + exports.len());
    deps_exports.extend(deps.iter().copied());
    deps_exports.extend(exports.iter().copied());

    let mut exports_deps = Vec::with_capacity(exports.len() + deps.len());
    exports_deps.extend(exports.iter().copied());
    exports_deps.extend(deps.iter().copied());

    let mut runtimedeps_exports_deps = Vec::with_capacity(runtime_deps.len() + exports_deps.len());
    runtimedeps_exports_deps.extend(runtime_deps.iter().copied());
    runtimedeps_exports_deps.extend(exports_deps.iter().copied());

    let source_jars = if source_jar.to_bool() {
        vec![source_jar]
    } else {
        Vec::new()
    };

    let mut plugin_infos = Vec::with_capacity(exported_plugins.len() + exports.len());
    plugin_infos.extend(exported_plugins.iter().copied());
    plugin_infos.extend(exports.iter().copied());
    let mut plugins = Vec::new();
    let mut api_generating_plugins = Vec::new();
    for info in plugin_infos {
        let plugin_data = java_attr(info, "plugins", heap)?;
        if java_has_plugin_data_value(plugin_data, heap)? {
            plugins.push(plugin_data);
        }
        let plugin_data = java_attr(info, "api_generating_plugins", heap)?;
        if java_has_plugin_data_value(plugin_data, heap)? {
            api_generating_plugins.push(plugin_data);
        }
    }
    let plugins =
        java_merge_plugin_data(java_plugin_data_provider, empty_plugin_data, plugins, eval)?;
    let api_generating_plugins = java_merge_plugin_data(
        java_plugin_data_provider,
        empty_plugin_data,
        api_generating_plugins,
        eval,
    )?;

    let transitive_compile_time_jars = java_depset_preorder(
        heap,
        if compile_jar.to_bool() {
            vec![compile_jar]
        } else {
            Vec::new()
        },
        java_attr_values(&exports_deps, "transitive_compile_time_jars", heap)?,
    )?;

    let java_output = eval.eval_function(
        java_output_info_provider,
        &[],
        &[
            ("class_jar", output_jar),
            ("compile_jar", compile_jar),
            ("header_compilation_jar", header_compilation_jar),
            ("ijar", compile_jar),
            ("compile_jdeps", compile_jdeps),
            ("generated_class_jar", generated_class_jar),
            ("generated_source_jar", generated_source_jar),
            ("native_headers_jar", native_headers_jar),
            ("manifest_proto", manifest_proto),
            ("jdeps", jdeps),
            (
                "source_jars",
                bazel_depset_from_values(heap, source_jars.clone())?,
            ),
            ("source_jar", source_jar),
        ],
    )?;
    let java_outputs = heap.alloc(AllocList(vec![java_output])).to_value();

    let outputs = eval.eval_function(
        java_rule_output_jars_info_provider,
        &[],
        &[
            ("jars", java_outputs),
            ("jdeps", jdeps),
            ("native_headers", native_headers_jar),
        ],
    )?;

    let annotation_processing = eval.eval_function(
        java_gen_jars_info_provider,
        &[],
        &[
            ("enabled", Value::new_bool(false)),
            ("class_jar", generated_class_jar),
            ("source_jar", generated_source_jar),
            (
                "transitive_class_jars",
                java_depset_preorder(
                    heap,
                    if generated_class_jar.to_bool() {
                        vec![generated_class_jar]
                    } else {
                        Vec::new()
                    },
                    java_annotation_processing_values(
                        &deps_exports,
                        "transitive_class_jars",
                        heap,
                    )?,
                )?,
            ),
            (
                "transitive_source_jars",
                java_depset_preorder(
                    heap,
                    if generated_source_jar.to_bool() {
                        vec![generated_source_jar]
                    } else {
                        Vec::new()
                    },
                    java_annotation_processing_values(
                        &deps_exports,
                        "transitive_source_jars",
                        heap,
                    )?,
                )?,
            ),
            (
                "processor_classnames",
                heap.alloc(AllocList(Vec::<Value<'v>>::new())).to_value(),
            ),
            (
                "processor_classpath",
                bazel_depset_from_values(heap, Vec::new())?,
            ),
        ],
    )?;

    let mut compile_time_java_dependencies =
        java_attr_values(&exports, "_compile_time_java_dependencies", heap)?;
    if compile_jdeps.to_bool() {
        compile_time_java_dependencies.push(bazel_depset_from_values(heap, vec![compile_jdeps])?);
    }

    let mut native_library_depsets = java_attr_values(
        &runtimedeps_exports_deps,
        "transitive_native_libraries",
        heap,
    )?;
    for native_library in native_libraries {
        if let Some(libraries_to_link) =
            native_library.get_attr("_legacy_transitive_native_libraries", heap)?
        {
            native_library_depsets.push(libraries_to_link);
        }
    }

    let result = heap
        .alloc(AllocDict([
            ("transitive_compile_time_jars", transitive_compile_time_jars),
            (
                "compile_jars",
                java_depset_preorder(
                    heap,
                    if compile_jar.to_bool() {
                        vec![compile_jar]
                    } else {
                        Vec::new()
                    },
                    java_attr_values(&exports, "compile_jars", heap)?,
                )?,
            ),
            (
                "header_compilation_direct_deps",
                java_depset_preorder(
                    heap,
                    if header_compilation_jar.to_bool() {
                        vec![header_compilation_jar]
                    } else {
                        Vec::new()
                    },
                    java_attr_values(&exports, "header_compilation_direct_deps", heap)?,
                )?,
            ),
            (
                "full_compile_jars",
                java_depset_preorder(
                    heap,
                    vec![output_jar],
                    java_attr_values(&exports, "full_compile_jars", heap)?,
                )?,
            ),
            ("source_jars", heap.alloc(AllocList(source_jars)).to_value()),
            (
                "runtime_output_jars",
                heap.alloc(AllocList(vec![output_jar])).to_value(),
            ),
            ("plugins", plugins),
            ("api_generating_plugins", api_generating_plugins),
            ("java_outputs", java_outputs),
            ("outputs", outputs),
            ("annotation_processing", annotation_processing),
            (
                "_transitive_full_compile_time_jars",
                java_depset_preorder(
                    heap,
                    vec![output_jar],
                    java_attr_values(&exports_deps, "_transitive_full_compile_time_jars", heap)?,
                )?,
            ),
            (
                "_compile_time_java_dependencies",
                java_depset_preorder(heap, Vec::new(), compile_time_java_dependencies)?,
            ),
            ("_neverlink", Value::new_bool(neverlink.to_bool())),
            ("compilation_info", Value::new_none()),
            (
                "_constraints",
                heap.alloc(AllocList(Vec::<Value<'v>>::new())).to_value(),
            ),
            (
                "transitive_native_libraries",
                java_depset_topological(heap, Vec::new(), native_library_depsets)?,
            ),
        ]))
        .to_value();

    let concatenated_deps = heap
        .alloc(AllocStruct([
            (
                "deps_exports",
                heap.alloc(AllocList(deps_exports)).to_value(),
            ),
            (
                "exports_deps",
                heap.alloc(AllocList(exports_deps)).to_value(),
            ),
            (
                "runtimedeps_exports_deps",
                heap.alloc(AllocList(runtimedeps_exports_deps)).to_value(),
            ),
        ]))
        .to_value();

    Ok(heap
        .alloc(AllocTuple([result, concatenated_deps]))
        .to_value())
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
    java_add_flag_collection(&mut argv, heap, "--bootclasspath", bootclasspath)?;
    java_add_flag_collection(&mut argv, heap, "--sources", source_files)?;
    java_add_flag_collection(&mut argv, heap, "--source_jars", source_jars)?;
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
    java_add_flag_collection(&mut argv, heap, "--processors", processor_classes)?;
    if !use_header_compiler_direct {
        java_add_flag_collection(&mut argv, heap, "--processorpath", processor_jars)?;
    }
    java_add_flag_collection(&mut argv, heap, "--classpath", compilation_classpath)?;
    if strict_deps_mode
        .unpack_str()
        .is_some_and(|mode| mode != "OFF")
    {
        java_add_flag_collection(&mut argv, heap, "--direct_dependencies", direct_jars)?;
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
        None,
        eval,
    )
}

fn java_register_compilation_action<'v>(
    ctx: Value<'v>,
    java_toolchain: Value<'v>,
    output: Value<'v>,
    manifest_proto: Value<'v>,
    plugin_info: Value<'v>,
    compilation_classpath: Value<'v>,
    direct_jars: Value<'v>,
    bootclasspath: Value<'v>,
    javabuilder_jvm_flags: Value<'v>,
    compile_time_java_deps: Value<'v>,
    javac_opts: Value<'v>,
    strict_deps_mode: Value<'v>,
    target_label: Value<'v>,
    deps_proto: Value<'v>,
    gen_class: Value<'v>,
    gen_source: Value<'v>,
    native_header_jar: Value<'v>,
    sources: Value<'v>,
    source_jars: Value<'v>,
    resources: Value<'v>,
    resource_jars: Value<'v>,
    classpath_resources: Value<'v>,
    sourcepath: Value<'v>,
    injecting_rule_kind: Value<'v>,
    _enable_jspecify: Value<'v>,
    _enable_direct_classpath: Value<'v>,
    additional_inputs: Value<'v>,
    additional_outputs: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let heap = eval.heap();
    let (executable, mut argv, mut inputs, param_file_start) =
        java_tool_command(java_toolchain, "_javabuilder", javabuilder_jvm_flags, heap)?;

    java_add_flag_value(&mut argv, heap, "--output", output);
    java_add_flag_value(&mut argv, heap, "--native_header_output", native_header_jar);
    java_add_flag_value(&mut argv, heap, "--generated_sources_output", gen_source);
    java_add_flag_value(&mut argv, heap, "--output_manifest_proto", manifest_proto);
    java_add_flag(&mut argv, heap, "--compress_jar");
    java_add_flag_value(&mut argv, heap, "--output_deps_proto", deps_proto);
    let bootclasspath = java_bootclasspath(java_toolchain, bootclasspath, heap)?;
    java_add_flag_collection(&mut argv, heap, "--bootclasspath", bootclasspath)?;
    java_add_flag_collection(&mut argv, heap, "--sourcepath", sourcepath)?;
    let processor_classes =
        java_plugin_data_field(plugin_info, "plugins", "processor_classes", heap)?;
    let processor_jars = java_plugin_data_field(plugin_info, "plugins", "processor_jars", heap)?;
    let processor_data = java_plugin_data_field(plugin_info, "plugins", "processor_data", heap)?;
    java_add_flag_collection(&mut argv, heap, "--processorpath", processor_jars)?;
    java_add_flag_collection(&mut argv, heap, "--processors", processor_classes)?;
    java_add_flag_collection(&mut argv, heap, "--source_jars", source_jars)?;
    java_add_flag_collection(&mut argv, heap, "--sources", sources)?;
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
        java_add_flag_collection(&mut argv, heap, "--direct_dependencies", direct_jars)?;
    }
    java_add_flag_value(
        &mut argv,
        heap,
        "--experimental_fix_deps_tool",
        heap.alloc_str("add_dep").to_value(),
    );
    java_add_flag_collection(&mut argv, heap, "--classpath", compilation_classpath)?;
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

    let worker_executable =
        java_compile_worker_executable(java_toolchain, executable, &argv, param_file_start, heap)?;
    java_call_run_action(
        ctx,
        executable,
        argv,
        inputs,
        outputs,
        "Javac",
        param_file_start,
        None,
        worker_executable,
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

    fn derive_output_file<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(require = pos)] ctx: Value<'v>,
        #[starlark(require = pos)] base_file: Value<'v>,
        #[starlark(require = named, default = "")] name_suffix: &str,
        #[starlark(require = named, default = NoneOr::None)] extension: NoneOr<StringValue<'v>>,
        #[starlark(require = named, default = "")] extension_suffix: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        java_derive_output_file(
            ctx,
            base_file,
            name_suffix,
            extension,
            extension_suffix,
            eval,
        )
    }

    fn has_plugin_data<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(require = pos)] plugin_data: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        java_has_plugin_data_value(plugin_data, eval.heap())
    }

    fn merge_plugin_data<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(require = pos)] plugin_data_provider: Value<'v>,
        #[starlark(require = pos)] empty_plugin_data: Value<'v>,
        #[starlark(require = pos)] datas: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let datas = java_collection_values(datas, eval.heap())?;
        java_merge_plugin_data(plugin_data_provider, empty_plugin_data, datas, eval)
    }

    fn javainfo_init_base<'v>(
        #[starlark(this)] _this: &JavaCommonInternal,
        #[starlark(require = pos)] java_output_info_provider: Value<'v>,
        #[starlark(require = pos)] java_rule_output_jars_info_provider: Value<'v>,
        #[starlark(require = pos)] java_gen_jars_info_provider: Value<'v>,
        #[starlark(require = pos)] java_plugin_data_provider: Value<'v>,
        #[starlark(require = pos)] empty_plugin_data: Value<'v>,
        #[starlark(require = pos)] output_jar: Value<'v>,
        #[starlark(require = pos)] compile_jar: Value<'v>,
        #[starlark(require = pos)] source_jar: Value<'v>,
        #[starlark(require = pos)] deps: Value<'v>,
        #[starlark(require = pos)] runtime_deps: Value<'v>,
        #[starlark(require = pos)] exports: Value<'v>,
        #[starlark(require = pos)] exported_plugins: Value<'v>,
        #[starlark(require = pos)] jdeps: Value<'v>,
        #[starlark(require = pos)] compile_jdeps: Value<'v>,
        #[starlark(require = pos)] native_headers_jar: Value<'v>,
        #[starlark(require = pos)] manifest_proto: Value<'v>,
        #[starlark(require = pos)] generated_class_jar: Value<'v>,
        #[starlark(require = pos)] generated_source_jar: Value<'v>,
        #[starlark(require = pos)] native_libraries: Value<'v>,
        #[starlark(require = pos)] neverlink: Value<'v>,
        #[starlark(require = pos)] header_compilation_jar: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        java_javainfo_init_base(
            java_output_info_provider,
            java_rule_output_jars_info_provider,
            java_gen_jars_info_provider,
            java_plugin_data_provider,
            empty_plugin_data,
            output_jar,
            compile_jar,
            source_jar,
            deps,
            runtime_deps,
            exports,
            exported_plugins,
            jdeps,
            compile_jdeps,
            native_headers_jar,
            manifest_proto,
            generated_class_jar,
            generated_source_jar,
            native_libraries,
            neverlink,
            header_compilation_jar,
            eval,
        )
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
        #[starlark(require = pos)] ctx: Value<'v>,
        #[starlark(require = pos)] java_toolchain: Value<'v>,
        #[starlark(require = pos)] output: Value<'v>,
        #[starlark(require = pos)] manifest_proto: Value<'v>,
        #[starlark(require = pos)] plugin_info: Value<'v>,
        #[starlark(require = pos)] compilation_classpath: Value<'v>,
        #[starlark(require = pos)] direct_jars: Value<'v>,
        #[starlark(require = pos)] bootclasspath: Value<'v>,
        #[starlark(require = pos)] javabuilder_jvm_flags: Value<'v>,
        #[starlark(require = pos)] compile_time_java_deps: Value<'v>,
        #[starlark(require = pos)] javac_opts: Value<'v>,
        #[starlark(require = pos)] strict_deps_mode: Value<'v>,
        #[starlark(require = pos)] target_label: Value<'v>,
        #[starlark(require = pos)] deps_proto: Value<'v>,
        #[starlark(require = pos)] gen_class: Value<'v>,
        #[starlark(require = pos)] gen_source: Value<'v>,
        #[starlark(require = pos)] native_header_jar: Value<'v>,
        #[starlark(require = pos)] sources: Value<'v>,
        #[starlark(require = pos)] source_jars: Value<'v>,
        #[starlark(require = pos)] resources: Value<'v>,
        #[starlark(require = pos)] resource_jars: Value<'v>,
        #[starlark(require = pos)] classpath_resources: Value<'v>,
        #[starlark(require = pos)] sourcepath: Value<'v>,
        #[starlark(require = pos)] injecting_rule_kind: Value<'v>,
        #[starlark(require = pos)] enable_jspecify: Value<'v>,
        #[starlark(require = pos)] enable_direct_classpath: Value<'v>,
        #[starlark(require = pos)] additional_inputs: Value<'v>,
        #[starlark(require = pos)] additional_outputs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        java_register_compilation_action(
            ctx,
            java_toolchain,
            output,
            manifest_proto,
            plugin_info,
            compilation_classpath,
            direct_jars,
            bootclasspath,
            javabuilder_jvm_flags,
            compile_time_java_deps,
            javac_opts,
            strict_deps_mode,
            target_label,
            deps_proto,
            gen_class,
            gen_source,
            native_header_jar,
            sources,
            source_jars,
            resources,
            resource_jars,
            classpath_resources,
            sourcepath,
            injecting_rule_kind,
            enable_jspecify,
            enable_direct_classpath,
            additional_inputs,
            additional_outputs,
            eval,
        )
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
