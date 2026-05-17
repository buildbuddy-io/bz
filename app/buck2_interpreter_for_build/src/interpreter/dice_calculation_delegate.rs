/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::dice::cycles::CycleGuard;
use buck2_common::file_ops::dice::DiceFileComputations;
use buck2_common::file_ops::error::FileReadErrorContext;
use buck2_common::legacy_configs::dice::HasLegacyConfigs;
use buck2_common::legacy_configs::dice::OpaqueLegacyBuckConfigOnDice;
use buck2_common::package_boundary::HasPackageBoundaryExceptions;
use buck2_common::package_listing::PackageListingStrategy;
use buck2_common::package_listing::bazel_compat_package_listing_enabled;
use buck2_common::package_listing::dice::DicePackageListingResolver;
use buck2_common::package_listing::listing::PackageListing;
use buck2_core::build_file_path::BuildFilePath;
use buck2_core::bzl::ImportPath;
use buck2_core::cells::build_file_cell::BuildFileCell;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::package::PackageLabel;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use buck2_events::dispatch::span;
use buck2_events::dispatch::span_async_simple;
use buck2_interpreter::allow_relative_paths::HasAllowRelativePaths;
use buck2_interpreter::dice::starlark_provider::StarlarkEvalKind;
use buck2_interpreter::factory::StarlarkEvaluatorProvider;
use buck2_interpreter::file_loader::LoadedModule;
use buck2_interpreter::file_loader::ModuleDeps;
use buck2_interpreter::from_freeze::from_freeze_error;
use buck2_interpreter::import_paths::HasImportPaths;
use buck2_interpreter::load_module::InterpreterCalculation;
use buck2_interpreter::paths::module::OwnedStarlarkModulePath;
use buck2_interpreter::paths::module::StarlarkModulePath;
use buck2_interpreter::paths::package::PackageFilePath;
use buck2_interpreter::paths::path::OwnedStarlarkPath;
use buck2_interpreter::paths::path::StarlarkPath;
use buck2_node::nodes::eval_result::EvaluationResult;
use buck2_node::super_package::SuperPackage;
use buck2_util::time_span::TimeSpan;
use derive_more::Display;
use dice::DiceComputations;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::FutureExt;
use pagable::Pagable;
use pagable::pagable_typetag;
use sha2::Digest;
use sha2::Sha256;
use starlark::codemap::FileSpan;
use starlark::environment::Module;
use starlark::syntax::AstModule;
use starlark::values::FrozenHeapName;

use crate::bazel_repository::BazelRepositoryRuleEvaluation;
use crate::bazel_skylib_paths::BazelRulesCcIsPathAbsolute;
use crate::bazel_skylib_paths::BazelSkylibPaths;
use crate::interpreter::buckconfig::ConfigsOnDiceViewForStarlark;
use crate::interpreter::build_context::BazelRepositoryRuleInvocation;
use crate::interpreter::cell_info::InterpreterCellInfo;
use crate::interpreter::check_starlark_stack_size::check_starlark_stack_size;
use crate::interpreter::cycles::LoadCycleDescriptor;
use crate::interpreter::global_interpreter_state::HasGlobalInterpreterState;
use crate::interpreter::interpreter_for_dir::BuildFileEvalResult;
use crate::interpreter::interpreter_for_dir::InterpreterForDir;
use crate::interpreter::interpreter_for_dir::ParseData;
use crate::interpreter::interpreter_for_dir::ParseResult;
use crate::super_package::package_value::SuperPackageValuesImpl;

fn toml_value_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::Number(i.into()),
        toml::Value::Float(f) => match serde_json::Number::from_f64(f) {
            Some(n) => serde_json::Value::Number(n),
            None => serde_json::Value::Null,
        },
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .into_iter()
                .map(|(k, v)| (k, toml_value_to_json(v)))
                .collect(),
        ),
    }
}

const BAZEL_SKYLIB_PATHS_BZL_SHA256: &str =
    "96cce43871d8228126a12ceff771351f9030b1e9d029f2185853aa6541766a83";
const BAZEL_RULES_CC_PATHS_BZL_SHA256: &str =
    "c982ac685f0bfbd32602d82d1c37f3bf50a2714ca6a13bfd3c08d4e5cc8b8872";
const BAZEL_RULES_CC_CC_INFO_BZL_SHA256: &str =
    "4424bb876c3f8234d7cfce20652e7ab1a7b2fc34cc2c637b1cb4313590d9f1bc";
const BAZEL_RULES_CC_CC_HELPER_BZL_SHA256: &str =
    "22b11a7833958f11fb32ddf6406195e02ccef9dc635369a556cbafa4e933fdbe";
const BAZEL_RULES_CC_CONFIGURE_FEATURES_BZL_SHA256: &str =
    "d950aa9acda68b999c452178f8ccf49860eac910a8c28551c547c3725198b977";
const BAZEL_RULES_JAVA_INFO_BZL_SHA256: &str =
    "02438c92066a825629a47f6dd01d9ea2200dc90a666b68fb4ee1ebf09e6a3026";
const BAZEL_RULES_JAVA_COMMON_INTERNAL_BZL_SHA256: &str =
    "68776f7ef30ca86f97edb7359812e1c3ff12cf348c812cfa86292d1d41a825a3";
const BAZEL_RULES_JAVA_HELPER_BZL_SHA256: &str =
    "f2cec4a3f582799329e1aca55b81dbd3591dac00f88d951b68730e861110faa8";

fn is_bazel_skylib_paths_module(starlark_file: StarlarkModulePath<'_>) -> bool {
    match starlark_file {
        StarlarkModulePath::LoadFile(_) => {
            let path = starlark_file.path();
            path.cell().as_str().contains("bazel_skylib") && path.path().as_str() == "lib/paths.bzl"
        }
        StarlarkModulePath::BxlFile(_)
        | StarlarkModulePath::JsonFile(_)
        | StarlarkModulePath::TomlFile(_) => false,
    }
}

fn is_bazel_rules_cc_paths_module(starlark_file: StarlarkModulePath<'_>) -> bool {
    match starlark_file {
        StarlarkModulePath::LoadFile(_) => {
            let path = starlark_file.path();
            path.cell().as_str().contains("rules_cc")
                && path.path().as_str() == "cc/private/paths.bzl"
        }
        StarlarkModulePath::BxlFile(_)
        | StarlarkModulePath::JsonFile(_)
        | StarlarkModulePath::TomlFile(_) => false,
    }
}

fn is_bazel_rules_cc_cc_info_path(starlark_file: StarlarkPath<'_>) -> bool {
    match starlark_file {
        StarlarkPath::LoadFile(path) => {
            path.cell().as_str().contains("rules_cc")
                && path.path().path().as_str() == "cc/private/cc_info.bzl"
        }
        StarlarkPath::BuildFile(_)
        | StarlarkPath::PackageFile(_)
        | StarlarkPath::BxlFile(_)
        | StarlarkPath::JsonFile(_)
        | StarlarkPath::TomlFile(_) => false,
    }
}

fn is_bazel_rules_cc_cc_helper_path(starlark_file: StarlarkPath<'_>) -> bool {
    match starlark_file {
        StarlarkPath::LoadFile(path) => {
            path.cell().as_str().contains("rules_cc")
                && path.path().path().as_str() == "cc/common/cc_helper.bzl"
        }
        StarlarkPath::BuildFile(_)
        | StarlarkPath::PackageFile(_)
        | StarlarkPath::BxlFile(_)
        | StarlarkPath::JsonFile(_)
        | StarlarkPath::TomlFile(_) => false,
    }
}

fn is_bazel_rules_cc_configure_features_path(starlark_file: StarlarkPath<'_>) -> bool {
    match starlark_file {
        StarlarkPath::LoadFile(path) => {
            path.cell().as_str().contains("rules_cc")
                && path.path().path().as_str()
                    == "cc/private/toolchain_config/configure_features.bzl"
        }
        StarlarkPath::BuildFile(_)
        | StarlarkPath::PackageFile(_)
        | StarlarkPath::BxlFile(_)
        | StarlarkPath::JsonFile(_)
        | StarlarkPath::TomlFile(_) => false,
    }
}

fn is_bazel_rules_java_common_internal_path(starlark_file: StarlarkPath<'_>) -> bool {
    match starlark_file {
        StarlarkPath::LoadFile(path) => {
            path.cell().as_str().contains("rules_java")
                && path.path().path().as_str() == "java/private/java_common_internal.bzl"
        }
        StarlarkPath::BuildFile(_)
        | StarlarkPath::PackageFile(_)
        | StarlarkPath::BxlFile(_)
        | StarlarkPath::JsonFile(_)
        | StarlarkPath::TomlFile(_) => false,
    }
}

fn is_bazel_rules_java_info_path(starlark_file: StarlarkPath<'_>) -> bool {
    match starlark_file {
        StarlarkPath::LoadFile(path) => {
            path.cell().as_str().contains("rules_java")
                && path.path().path().as_str() == "java/private/java_info.bzl"
        }
        StarlarkPath::BuildFile(_)
        | StarlarkPath::PackageFile(_)
        | StarlarkPath::BxlFile(_)
        | StarlarkPath::JsonFile(_)
        | StarlarkPath::TomlFile(_) => false,
    }
}

fn is_bazel_rules_java_helper_path(starlark_file: StarlarkPath<'_>) -> bool {
    match starlark_file {
        StarlarkPath::LoadFile(path) => {
            path.cell().as_str().contains("rules_java")
                && path.path().path().as_str() == "java/common/rules/java_helper.bzl"
        }
        StarlarkPath::BuildFile(_)
        | StarlarkPath::PackageFile(_)
        | StarlarkPath::BxlFile(_)
        | StarlarkPath::JsonFile(_)
        | StarlarkPath::TomlFile(_) => false,
    }
}

fn rewrite_bazel_rules_cc_cc_info(
    starlark_file: StarlarkPath<'_>,
    contents: String,
) -> buck2_error::Result<String> {
    if !is_bazel_rules_cc_cc_info_path(starlark_file) {
        return Ok(contents);
    }
    if hex::encode(Sha256::digest(contents.as_bytes())) != BAZEL_RULES_CC_CC_INFO_BZL_SHA256 {
        return Ok(contents);
    }

    const FLAT_DEPSET_STUB: &str = r#"def _flat_depset(*, transitive = []):
    largest_depset = depset()
    largest_depset_list = []
    for t in transitive:
        t_list = t.to_list()
        if len(t_list) > len(largest_depset_list):
            largest_depset_list = t_list
            largest_depset = t

    all = depset(transitive = transitive)
    if all.to_list() == largest_depset_list:
        return largest_depset
    return all
"#;
    const NATIVE_FLAT_DEPSET_STUB: &str = r#"def _flat_depset(*, transitive = []):
    return __buck2_bazel_flat_depset(transitive = transitive)
"#;
    const MERGE_COMPILATION_CONTEXTS_STUB: &str = r#"def _merge_compilation_contexts(*, compilation_context = EMPTY_COMPILATION_CONTEXT, exported_deps = [], deps = []):
    exporting_module_maps = depset(
        direct = [dep._module_map for dep in exported_deps if dep._module_map],
        transitive = [dep._exporting_module_maps for dep in exported_deps],
    )
    exporting_module_map_files = depset(
        direct = [dep._module_map.file for dep in exported_deps if dep._module_map],
        transitive = [dep._exporting_module_map_files for dep in exported_deps],
    )
    all_deps = exported_deps + deps
    direct_module_maps = depset(
        direct = [dep._module_map.file for dep in all_deps if dep._module_map],
        transitive = [dep._exporting_module_map_files for dep in all_deps],
    )

    dep_header_infos = [dep._header_info for dep in all_deps]
    merged_header_infos = [dep._header_info for dep in exported_deps]

    compilation_context_header_info = compilation_context._header_info
    header_info = _cc_internal.create_header_info_with_deps(
        header_info = compilation_context_header_info,
        deps = dep_header_infos,
        merged_deps = merged_header_infos,
    )

    transitive_modules_artifacts = []
    transitive_pic_modules_artifacts = []
    for dep in all_deps:
        dep_header_info = dep._header_info
        if dep_header_info.header_module:
            transitive_modules_artifacts.append(dep_header_info.header_module)
        if dep_header_info.separate_module:
            transitive_modules_artifacts.append(dep_header_info.separate_module)
        if dep_header_info.pic_header_module:
            transitive_pic_modules_artifacts.append(dep_header_info.pic_header_module)
        if dep_header_info.separate_pic_module:
            transitive_pic_modules_artifacts.append(dep_header_info.separate_pic_module)

    return CcCompilationContextInfo(
        includes = _flat_depset(
            transitive = [compilation_context.includes] + [dep.includes for dep in all_deps],
        ),
        quote_includes = _flat_depset(
            transitive = [compilation_context.quote_includes] + [dep.quote_includes for dep in all_deps],
        ),
        system_includes = _flat_depset(
            transitive = [compilation_context.system_includes] + [dep.system_includes for dep in all_deps],
        ),
        framework_includes = _flat_depset(
            transitive = [compilation_context.framework_includes] + [dep.framework_includes for dep in all_deps],
        ),
        external_includes = _flat_depset(
            transitive = [compilation_context.external_includes] + [dep.external_includes for dep in all_deps],
        ),
        defines = _flat_depset(
            transitive = [dep.defines for dep in all_deps] + [compilation_context.defines],
        ),
        local_defines = compilation_context.local_defines,
        headers = depset(
            direct = compilation_context.headers.to_list(),
            transitive = [dep.headers for dep in all_deps],
        ),
        # Duplication with HeaderInfo data:
        direct_headers = _cc_internal.freeze(header_info.modular_public_headers + header_info.modular_private_headers + header_info.separate_module_headers),
        direct_public_headers = header_info.modular_public_headers,
        direct_private_headers = header_info.modular_private_headers,
        direct_textual_headers = header_info.textual_headers,
        _direct_module_maps = direct_module_maps,
        _module_map = compilation_context._module_map,
        _exporting_module_maps = exporting_module_maps,
        _exporting_module_map_files = exporting_module_map_files,
        _non_code_inputs = depset(
            direct = compilation_context._non_code_inputs.to_list(),
            transitive = [dep._non_code_inputs for dep in all_deps],
        ),
        _virtual_to_original_headers = depset(
            transitive = [compilation_context._virtual_to_original_headers] + [dep._virtual_to_original_headers for dep in all_deps],
        ),
        validation_artifacts = depset(
            transitive = [compilation_context.validation_artifacts] + [dep.validation_artifacts for dep in all_deps],
        ),
        _header_info = header_info,
        _transitive_modules = depset(
            transitive_modules_artifacts,
            transitive = [dep._transitive_modules for dep in all_deps],
        ),
        _transitive_pic_modules = depset(
            transitive_pic_modules_artifacts,
            transitive = [dep._transitive_pic_modules for dep in all_deps],
        ),
        _modules_info_files = depset(
            transitive = [compilation_context._modules_info_files] + [dep._modules_info_files for dep in all_deps],
        ),
        _pic_modules_info_files = depset(
            transitive = [compilation_context._pic_modules_info_files] + [dep._pic_modules_info_files for dep in all_deps],
        ),
        _module_files = depset(
            transitive = [compilation_context._module_files] + [dep._module_files for dep in all_deps],
        ),
        _pic_module_files = depset(
            transitive = [compilation_context._pic_module_files] + [dep._pic_module_files for dep in all_deps],
        ),
    )
"#;
    const NATIVE_MERGE_COMPILATION_CONTEXTS_STUB: &str = r#"def _merge_compilation_contexts(*, compilation_context = EMPTY_COMPILATION_CONTEXT, exported_deps = [], deps = []):
    return __buck2_bazel_merge_compilation_contexts(
        CcCompilationContextInfo,
        compilation_context,
        exported_deps,
        deps,
    )
"#;

    let rewritten = contents.replacen(FLAT_DEPSET_STUB, NATIVE_FLAT_DEPSET_STUB, 1);
    if rewritten == contents {
        return Err(internal_error!(
            "rules_cc cc_info.bzl hash matched, but _flat_depset stub did not"
        ));
    }
    let rewritten_merge = rewritten.replacen(
        MERGE_COMPILATION_CONTEXTS_STUB,
        NATIVE_MERGE_COMPILATION_CONTEXTS_STUB,
        1,
    );
    if rewritten_merge == rewritten {
        return Err(internal_error!(
            "rules_cc cc_info.bzl hash matched, but _merge_compilation_contexts stub did not"
        ));
    }
    Ok(rewritten_merge)
}

fn rewrite_bazel_rules_cc_configure_features(
    starlark_file: StarlarkPath<'_>,
    contents: String,
) -> buck2_error::Result<String> {
    if !is_bazel_rules_cc_configure_features_path(starlark_file) {
        return Ok(contents);
    }
    if hex::encode(Sha256::digest(contents.as_bytes()))
        != BAZEL_RULES_CC_CONFIGURE_FEATURES_BZL_SHA256
    {
        return Ok(contents);
    }

    const CONFIGURE_FEATURES_START: &str = "def configure_features(\n";
    const NATIVE_CONFIGURE_FEATURES_STUB: &str = r#"def configure_features(
        *,
        ctx,
        cc_toolchain,
        language = "c++",
        requested_features = [],
        unsupported_features = []):
    return cc_common.internal_DO_NOT_USE().configure_features(
        ctx = ctx,
        cc_toolchain = cc_toolchain,
        language = language,
        requested_features = requested_features,
        unsupported_features = unsupported_features,
    )
"#;

    let Some(start) = contents.find(CONFIGURE_FEATURES_START) else {
        return Err(internal_error!(
            "rules_cc configure_features.bzl hash matched, but configure_features stub did not"
        ));
    };

    let mut rewritten = contents[..start].to_owned();
    rewritten.push_str(NATIVE_CONFIGURE_FEATURES_STUB);
    Ok(rewritten)
}

fn rewrite_bazel_rules_cc_cc_helper(
    starlark_file: StarlarkPath<'_>,
    contents: String,
) -> buck2_error::Result<String> {
    if !is_bazel_rules_cc_cc_helper_path(starlark_file) {
        return Ok(contents);
    }
    if hex::encode(Sha256::digest(contents.as_bytes())) != BAZEL_RULES_CC_CC_HELPER_BZL_SHA256 {
        return Ok(contents);
    }

    const DYNAMIC_LIBRARIES_FOR_RUNTIME_STUB: &str = r#"def _get_dynamic_libraries_for_runtime(cc_linking_context, linking_statically):
    libraries = []
    for linker_input in cc_linking_context.linker_inputs.to_list():
        libraries.extend(linker_input.libraries)

    dynamic_libraries_for_runtime = []
    for library in libraries:
        artifact = _get_dynamic_library_for_runtime_or_none(library, linking_statically)
        if artifact != None:
            dynamic_libraries_for_runtime.append(artifact)

    return dynamic_libraries_for_runtime
"#;
    const NATIVE_DYNAMIC_LIBRARIES_FOR_RUNTIME_STUB: &str = r#"def _get_dynamic_libraries_for_runtime(cc_linking_context, linking_statically):
    return __buck2_bazel_get_dynamic_libraries_for_runtime(cc_linking_context, linking_statically)
"#;
    const COLLECT_LIBRARY_HIDDEN_TOP_LEVEL_ARTIFACTS_STUB: &str = r#"def _collect_library_hidden_top_level_artifacts(
        ctx,
        files_to_compile):
    artifacts_to_force_builder = [files_to_compile]
    if hasattr(ctx.attr, "deps"):
        for dep in ctx.attr.deps:
            if OutputGroupInfo in dep:
                if "_hidden_top_level_INTERNAL_" in dep[OutputGroupInfo]:
                    artifacts_to_force_builder.append(dep[OutputGroupInfo]["_hidden_top_level_INTERNAL_"])

    return depset(transitive = artifacts_to_force_builder)
"#;
    const NATIVE_COLLECT_LIBRARY_HIDDEN_TOP_LEVEL_ARTIFACTS_STUB: &str = r#"def _collect_library_hidden_top_level_artifacts(
        ctx,
        files_to_compile):
    return __buck2_bazel_collect_library_hidden_top_level_artifacts(
        OutputGroupInfo,
        files_to_compile,
        ctx.attr.deps if hasattr(ctx.attr, "deps") else [],
    )
"#;
    const CHECK_FILE_EXTENSION_STUB: &str = r#"def _check_file_extension(file, allowed_extensions, allow_versioned_shared_libraries):
    extension = "." + file.extension
    if _matches_extension(extension, allowed_extensions) or (allow_versioned_shared_libraries and is_versioned_shared_library_extension_valid(file.path)):
        return True
    return False
"#;
    const NATIVE_CHECK_FILE_EXTENSION_STUB: &str = r#"def _check_file_extension(file, allowed_extensions, allow_versioned_shared_libraries):
    return __buck2_bazel_check_file_extension(file, allowed_extensions, allow_versioned_shared_libraries)
"#;

    let rewritten = contents.replacen(
        DYNAMIC_LIBRARIES_FOR_RUNTIME_STUB,
        NATIVE_DYNAMIC_LIBRARIES_FOR_RUNTIME_STUB,
        1,
    );
    if rewritten == contents {
        return Err(internal_error!(
            "rules_cc cc_helper.bzl hash matched, but _get_dynamic_libraries_for_runtime stub did not"
        ));
    }
    let rewritten_hidden = rewritten.replacen(
        COLLECT_LIBRARY_HIDDEN_TOP_LEVEL_ARTIFACTS_STUB,
        NATIVE_COLLECT_LIBRARY_HIDDEN_TOP_LEVEL_ARTIFACTS_STUB,
        1,
    );
    if rewritten_hidden == rewritten {
        return Err(internal_error!(
            "rules_cc cc_helper.bzl hash matched, but _collect_library_hidden_top_level_artifacts stub did not"
        ));
    }
    let rewritten_check_extension = rewritten_hidden.replacen(
        CHECK_FILE_EXTENSION_STUB,
        NATIVE_CHECK_FILE_EXTENSION_STUB,
        1,
    );
    if rewritten_check_extension == rewritten_hidden {
        return Err(internal_error!(
            "rules_cc cc_helper.bzl hash matched, but _check_file_extension stub did not"
        ));
    }
    Ok(rewritten_check_extension)
}

fn rewrite_bazel_rules_java_info(
    starlark_file: StarlarkPath<'_>,
    contents: String,
) -> buck2_error::Result<String> {
    if !is_bazel_rules_java_info_path(starlark_file) {
        return Ok(contents);
    }
    if hex::encode(Sha256::digest(contents.as_bytes())) != BAZEL_RULES_JAVA_INFO_BZL_SHA256 {
        return Ok(contents);
    }

    const HAS_PLUGIN_DATA_STUB: &str = r#"def _has_plugin_data(plugin_data):
    return plugin_data and (
        plugin_data.processor_classes or
        plugin_data.processor_jars or
        plugin_data.processor_data
    )
"#;
    const NATIVE_HAS_PLUGIN_DATA_STUB: &str = r#"def _has_plugin_data(plugin_data):
    return get_internal_java_common().has_plugin_data(plugin_data)
"#;
    const MERGE_PLUGIN_DATA_STUB: &str = r#"def _merge_plugin_data(datas):
    return _create_plugin_data_info(
        processor_classes = depset(transitive = [p.processor_classes for p in datas]),
        processor_jars = depset(transitive = [p.processor_jars for p in datas]),
        processor_data = depset(transitive = [p.processor_data for p in datas]),
    )
"#;
    const NATIVE_MERGE_PLUGIN_DATA_STUB: &str = r#"def _merge_plugin_data(datas):
    return get_internal_java_common().merge_plugin_data(
        JavaPluginDataInfo,
        _EMPTY_PLUGIN_DATA,
        datas,
    )
"#;
    const JAVAINFO_INIT_BASE_VALIDATION_STUB: &str = r#"    _validate_provider_list(deps, "deps", JavaInfo)
    _validate_provider_list(runtime_deps, "runtime_deps", JavaInfo)
    _validate_provider_list(exports, "exports", JavaInfo)
    _validate_provider_list(native_libraries, "native_libraries", CcInfo)

"#;
    const NATIVE_JAVAINFO_INIT_BASE_VALIDATION_STUB: &str = r#"    _validate_provider_list(deps, "deps", JavaInfo)
    _validate_provider_list(runtime_deps, "runtime_deps", JavaInfo)
    _validate_provider_list(exports, "exports", JavaInfo)
    _validate_provider_list(native_libraries, "native_libraries", CcInfo)

    if not get_internal_java_common().google_legacy_api_enabled():
        return get_internal_java_common().javainfo_init_base(
            _JavaOutputInfo,
            _JavaRuleOutputJarsInfo,
            _JavaGenJarsInfo,
            JavaPluginDataInfo,
            _EMPTY_PLUGIN_DATA,
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
        )

"#;

    let rewritten = contents.replacen(HAS_PLUGIN_DATA_STUB, NATIVE_HAS_PLUGIN_DATA_STUB, 1);
    if rewritten == contents {
        return Err(internal_error!(
            "rules_java java_info.bzl hash matched, but _has_plugin_data stub did not"
        ));
    }
    let rewritten_merge =
        rewritten.replacen(MERGE_PLUGIN_DATA_STUB, NATIVE_MERGE_PLUGIN_DATA_STUB, 1);
    if rewritten_merge == rewritten {
        return Err(internal_error!(
            "rules_java java_info.bzl hash matched, but _merge_plugin_data stub did not"
        ));
    }
    let rewritten_javainfo_base = rewritten_merge.replacen(
        JAVAINFO_INIT_BASE_VALIDATION_STUB,
        NATIVE_JAVAINFO_INIT_BASE_VALIDATION_STUB,
        1,
    );
    if rewritten_javainfo_base == rewritten_merge {
        return Err(internal_error!(
            "rules_java java_info.bzl hash matched, but _javainfo_init_base validation stub did not"
        ));
    }
    Ok(rewritten_javainfo_base)
}

fn rewrite_bazel_rules_java_common_internal(
    starlark_file: StarlarkPath<'_>,
    contents: String,
) -> buck2_error::Result<String> {
    if !is_bazel_rules_java_common_internal_path(starlark_file) {
        return Ok(contents);
    }
    if hex::encode(Sha256::digest(contents.as_bytes()))
        != BAZEL_RULES_JAVA_COMMON_INTERNAL_BZL_SHA256
    {
        return Ok(contents);
    }

    const DERIVE_OUTPUT_FILE_STUB: &str = r#"def _derive_output_file(ctx, base_file, *, name_suffix = "", extension = None, extension_suffix = ""):
    """Declares a new file whose name is derived from the given file

    This method allows appending a suffix to the name (before extension), changing
    the extension or appending a suffix after the extension. The new file is declared
    as a sibling of the given base file. At least one of the three options must be
    specified. It is an error to specify both `extension` and `extension_suffix`.

    Args:
        ctx: (RuleContext) the rule context.
        base_file: (File) the file from which to derive the resultant file.
        name_suffix: (str) Optional. The suffix to append to the name before the
        extension.
        extension: (str) Optional. The new extension to use (without '.'). By default,
        the base_file's extension is used.
        extension_suffix: (str) Optional. The suffix to append to the base_file's extension

    Returns:
        (File) the derived file
    """
    if not name_suffix and not extension_suffix and not extension:
        fail("At least one of name_suffix, extension or extension_suffix is required")
    if extension and extension_suffix:
        fail("only one of extension or extension_suffix can be specified")
    if extension == None:
        extension = base_file.extension
    new_basename = paths.replace_extension(base_file.basename, name_suffix + "." + extension + extension_suffix)
    return ctx.actions.declare_file(new_basename, sibling = base_file)
"#;
    const NATIVE_DERIVE_OUTPUT_FILE_STUB: &str = r#"def _derive_output_file(ctx, base_file, *, name_suffix = "", extension = None, extension_suffix = ""):
    return get_internal_java_common().derive_output_file(
        ctx,
        base_file,
        name_suffix = name_suffix,
        extension = extension,
        extension_suffix = extension_suffix,
    )
"#;

    let rewritten = contents.replacen(DERIVE_OUTPUT_FILE_STUB, NATIVE_DERIVE_OUTPUT_FILE_STUB, 1);
    if rewritten == contents {
        return Err(internal_error!(
            "rules_java java_common_internal.bzl hash matched, but _derive_output_file stub did not"
        ));
    }
    Ok(rewritten)
}

fn rewrite_bazel_rules_java_helper(
    starlark_file: StarlarkPath<'_>,
    contents: String,
) -> buck2_error::Result<String> {
    if !is_bazel_rules_java_helper_path(starlark_file) {
        return Ok(contents);
    }
    if hex::encode(Sha256::digest(contents.as_bytes())) != BAZEL_RULES_JAVA_HELPER_BZL_SHA256 {
        return Ok(contents);
    }

    const RESOURCE_MAPPER_STUB: &str = r#"def _resource_mapper(file):
    root_relative_path = paths.relativize(
        path = file.path,
        start = paths.join(file.root.path, file.owner.workspace_root),
    )
    return "%s:%s" % (
        file.path,
        semantics.get_default_resource_path(root_relative_path, segment_extractor = _java_segments),
    )
"#;
    const NATIVE_RESOURCE_MAPPER_STUB: &str = r#"def _resource_mapper(file):
    return java_common.internal_DO_NOT_USE().resource_mapper(file)
"#;

    let rewritten = contents.replacen(RESOURCE_MAPPER_STUB, NATIVE_RESOURCE_MAPPER_STUB, 1);
    if rewritten == contents {
        return Err(internal_error!(
            "rules_java java_helper.bzl hash matched, but _resource_mapper stub did not"
        ));
    }
    Ok(rewritten)
}

#[async_trait]
pub trait HasCalculationDelegate<'c, 'd> {
    /// Get calculator for a file evaluation.
    ///
    /// This function only accepts cell names, but it is created
    /// per evaluated file (build file or `.bzl`).
    async fn get_interpreter_calculator(
        &'c mut self,
        path: OwnedStarlarkPath,
    ) -> buck2_error::Result<DiceCalculationDelegate<'c, 'd>>;
}

#[async_trait]
impl<'c, 'd> HasCalculationDelegate<'c, 'd> for DiceComputations<'d> {
    async fn get_interpreter_calculator(
        &'c mut self,
        path: OwnedStarlarkPath,
    ) -> buck2_error::Result<DiceCalculationDelegate<'c, 'd>> {
        #[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
        #[display("{}@{}", _0, _1)]
        #[pagable_typetag(dice::DiceKeyDyn)]
        struct InterpreterConfigForDirKey(CellPath, BuildFileCell);

        #[async_trait]
        impl Key for InterpreterConfigForDirKey {
            type Value = buck2_error::Result<Arc<InterpreterForDir>>;
            async fn compute(
                &self,
                ctx: &mut DiceComputations,
                _cancellation: &CancellationContext,
            ) -> Self::Value {
                let global_state = ctx.get_global_interpreter_state().await?;

                let cell_alias_resolver = ctx.get_cell_alias_resolver(self.0.cell()).await?;

                let implicit_import_paths = ctx.import_paths_for_cell(self.1).await?;

                let dirs_allowing_relative_paths =
                    ctx.dirs_allowing_relative_paths(self.0.clone()).await?;

                let cell_info = InterpreterCellInfo::new(
                    self.1,
                    ctx.get_cell_resolver().await?,
                    cell_alias_resolver,
                )?;

                Ok(Arc::new(InterpreterForDir::new(
                    cell_info,
                    global_state.dupe(),
                    implicit_import_paths,
                    dirs_allowing_relative_paths,
                )?))
            }

            fn equality(x: &Self::Value, y: &Self::Value) -> bool {
                match (x, y) {
                    (Ok(x), Ok(y)) => x.equivalent(y),
                    _ => false,
                }
            }

            fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
                OkPagableValueSerialize::<Self::Value>::new()
            }
        }

        let build_file_cell = path.borrow().build_file_cell();
        let path_ref = path.borrow();
        let import_dir = match path_ref {
            StarlarkPath::LoadFile(path)
            | StarlarkPath::JsonFile(path)
            | StarlarkPath::TomlFile(path) => path.package_root().cloned(),
            StarlarkPath::BuildFile(_)
            | StarlarkPath::PackageFile(_)
            | StarlarkPath::BxlFile(_) => None,
        }
        .unwrap_or_else(|| {
            path_ref
                .path()
                .parent()
                .expect("starlark path to have parent")
                .to_owned()
        });
        let configs = self
            .compute(&InterpreterConfigForDirKey(import_dir, build_file_cell))
            .await??;

        Ok(DiceCalculationDelegate {
            build_file_cell,
            ctx: self,
            configs,
        })
    }
}

pub struct DiceCalculationDelegate<'c, 'd> {
    build_file_cell: BuildFileCell,
    ctx: &'c mut DiceComputations<'d>,
    configs: Arc<InterpreterForDir>,
}

impl<'c, 'd: 'c> DiceCalculationDelegate<'c, 'd> {
    async fn get_legacy_buck_config_for_starlark(
        &mut self,
    ) -> buck2_error::Result<OpaqueLegacyBuckConfigOnDice> {
        self.ctx
            .get_legacy_config_on_dice(self.build_file_cell.name())
            .await
    }

    async fn parse_file(
        &mut self,
        starlark_path: StarlarkPath<'_>,
    ) -> buck2_error::Result<ParseResult> {
        let result =
            DiceFileComputations::read_file(self.ctx, starlark_path.path().as_ref().as_ref()).await;
        let content = match starlark_path {
            StarlarkPath::BuildFile(file) => {
                result.with_package_context_information(file.path().path().to_string())
            }
            // Should potentially add support for other file types as well
            _ => result.without_package_context_information(),
        }?;
        let content = rewrite_bazel_rules_cc_cc_info(starlark_path, content)?;
        let content = rewrite_bazel_rules_cc_cc_helper(starlark_path, content)?;
        let content = rewrite_bazel_rules_cc_configure_features(starlark_path, content)?;
        let content = rewrite_bazel_rules_java_info(starlark_path, content)?;
        let content = rewrite_bazel_rules_java_common_internal(starlark_path, content)?;
        let content = rewrite_bazel_rules_java_helper(starlark_path, content)?;

        self.configs.parse(starlark_path, content)
    }

    async fn eval_deps(
        ctx: &mut DiceComputations<'_>,
        modules: &[(Option<FileSpan>, OwnedStarlarkModulePath)],
    ) -> buck2_error::Result<ModuleDeps> {
        Ok(ModuleDeps(
            ctx.try_compute_join(modules, |ctx, (span, import)| {
                async move {
                    ctx.get_loaded_module(import.borrow())
                        .await
                        .with_buck_error_context(|| {
                            format!(
                                "From load at {}",
                                span.as_ref()
                                    .map_or("implicit location".to_owned(), |file_span| file_span
                                        .resolve()
                                        .begin_file_line()
                                        .to_string())
                            )
                        })
                }
                .boxed()
            })
            .await?,
        ))
    }

    async fn eval_deps_with_cycle_guard(
        ctx: &mut DiceComputations<'_>,
        modules: &[(Option<FileSpan>, OwnedStarlarkModulePath)],
    ) -> buck2_error::Result<ModuleDeps> {
        let deps = CycleGuard::<LoadCycleDescriptor>::new(ctx)?
            .guard_this(Self::eval_deps(ctx, modules))
            .await
            .into_result(ctx)
            .await?;
        deps.map_err(buck2_error::Error::from)?
    }

    pub async fn prepare_eval(
        &mut self,
        starlark_file: StarlarkPath<'_>,
    ) -> buck2_error::Result<(AstModule, ModuleDeps)> {
        let (parse_data, deps) = self.prepare_eval_with_parse_data(starlark_file).await?;
        Ok((parse_data.ast, deps))
    }

    async fn prepare_eval_with_parse_data(
        &mut self,
        starlark_file: StarlarkPath<'_>,
    ) -> buck2_error::Result<(ParseData, ModuleDeps)> {
        let parse_data = self.parse_file(starlark_file).await??;
        let deps = Self::eval_deps_with_cycle_guard(self.ctx, &parse_data.imports).await?;
        Ok((parse_data, deps))
    }

    pub fn prepare_eval_with_content(
        &self,
        starlark_file: StarlarkPath<'_>,
        content: String,
    ) -> buck2_error::Result<ParseResult> {
        self.configs.parse(starlark_file, content)
    }

    pub async fn resolve_load(
        &self,
        starlark_file: StarlarkPath<'_>,
        load_string: &str,
    ) -> buck2_error::Result<OwnedStarlarkModulePath> {
        self.configs.resolve_path(starlark_file, load_string)
    }

    pub async fn eval_module_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<LoadedModule> {
        match starlark_file {
            StarlarkModulePath::JsonFile(_) => self.eval_json_module_uncached(starlark_file).await,
            StarlarkModulePath::TomlFile(_) => self.eval_toml_file_uncached(starlark_file).await,
            _ => {
                if let Some(module) = self
                    .eval_bazel_skylib_paths_module_uncached(starlark_file)
                    .await?
                {
                    return Ok(module);
                }
                if let Some(module) = self
                    .eval_bazel_rules_cc_paths_module_uncached(starlark_file)
                    .await?
                {
                    return Ok(module);
                }
                self.eval_starlark_module_uncached(starlark_file, cancellation)
                    .await
            }
        }
    }

    async fn eval_bazel_skylib_paths_module_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
    ) -> buck2_error::Result<Option<LoadedModule>> {
        if !is_bazel_skylib_paths_module(starlark_file) {
            return Ok(None);
        }

        let path = starlark_file.path();
        let contents = DiceFileComputations::read_file(self.ctx, path.as_ref())
            .await
            .without_package_context_information()?;
        if hex::encode(Sha256::digest(contents.as_bytes())) != BAZEL_SKYLIB_PATHS_BZL_SHA256 {
            return Ok(None);
        }

        let frozen = Module::with_temp_heap(|module| {
            module.set("paths", module.heap().alloc(BazelSkylibPaths));
            module
                .freeze_named(FrozenHeapName::User(Box::new(StarlarkEvalKind::Load(
                    Arc::new(OwnedStarlarkModulePath::new(starlark_file)),
                ))))
                .map_err(from_freeze_error)
        })?;

        Ok(Some(LoadedModule::new(
            OwnedStarlarkModulePath::new(starlark_file),
            Default::default(),
            frozen,
        )))
    }

    async fn eval_bazel_rules_cc_paths_module_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
    ) -> buck2_error::Result<Option<LoadedModule>> {
        if !is_bazel_rules_cc_paths_module(starlark_file) {
            return Ok(None);
        }

        let path = starlark_file.path();
        let contents = DiceFileComputations::read_file(self.ctx, path.as_ref())
            .await
            .without_package_context_information()?;
        if hex::encode(Sha256::digest(contents.as_bytes())) != BAZEL_RULES_CC_PATHS_BZL_SHA256 {
            return Ok(None);
        }

        let frozen = Module::with_temp_heap(|module| {
            module.set(
                "is_path_absolute",
                module.heap().alloc(BazelRulesCcIsPathAbsolute),
            );
            module
                .freeze_named(FrozenHeapName::User(Box::new(StarlarkEvalKind::Load(
                    Arc::new(OwnedStarlarkModulePath::new(starlark_file)),
                ))))
                .map_err(from_freeze_error)
        })?;

        Ok(Some(LoadedModule::new(
            OwnedStarlarkModulePath::new(starlark_file),
            Default::default(),
            frozen,
        )))
    }

    async fn eval_json_module_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
    ) -> buck2_error::Result<LoadedModule> {
        let path = starlark_file.path();
        let contents = DiceFileComputations::read_file(self.ctx, path.as_ref())
            .await
            .with_package_context_information(path.path().to_string())?;

        let value: serde_json::Value = serde_json::from_str(&contents)
            .with_buck_error_context(|| format!("Parsing {path}"))?;

        // patternlint-disable-next-line buck2-no-starlark-module: We expect these to be small + simple
        let frozen = Module::with_temp_heap(|module| {
            module.set("value", module.heap().alloc(value));
            module
                .freeze_named(FrozenHeapName::User(Box::new(StarlarkEvalKind::Load(
                    Arc::new(OwnedStarlarkModulePath::new(starlark_file)),
                ))))
                .map_err(from_freeze_error)
        })?;
        Ok(LoadedModule::new(
            OwnedStarlarkModulePath::new(starlark_file),
            Default::default(),
            frozen,
        ))
    }

    async fn eval_toml_file_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
    ) -> buck2_error::Result<LoadedModule> {
        let path = starlark_file.path();
        let contents = DiceFileComputations::read_file(self.ctx, path.as_ref())
            .await
            .with_package_context_information(path.path().to_string())?;

        let value: toml::Value =
            toml::from_str(&contents).with_buck_error_context(|| format!("Parsing {path}"))?;
        let json_value = toml_value_to_json(value);

        // patternlint-disable-next-line buck2-no-starlark-module: We expect these to be small + simple
        let frozen = Module::with_temp_heap(|module| {
            module.set("value", module.heap().alloc(json_value));
            module
                .freeze_named(FrozenHeapName::User(Box::new(StarlarkEvalKind::Load(
                    Arc::new(OwnedStarlarkModulePath::new(starlark_file)),
                ))))
                .map_err(from_freeze_error)
        })?;
        Ok(LoadedModule::new(
            OwnedStarlarkModulePath::new(starlark_file),
            Default::default(),
            frozen,
        ))
    }

    async fn eval_starlark_module_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<LoadedModule> {
        let (ast, deps) = self.prepare_eval(starlark_file.into()).await?;
        let loaded_modules = deps.get_loaded_modules();
        let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
        let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;

        let configs = &self.configs;
        let ctx = &mut *self.ctx;

        let eval_kind = StarlarkEvalKind::Load(Arc::new(starlark_file.to_owned()));
        let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;

        let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(ctx, buckconfig, root_buckconfig);
        let evaluation = configs
            .eval_module(
                starlark_file,
                &mut buckconfigs,
                ast,
                loaded_modules.clone(),
                provider,
                cancellation,
            )
            .with_buck_error_context(|| format!("Error evaluating module: `{}`", starlark_file))?;

        Ok(LoadedModule::new(
            OwnedStarlarkModulePath::new(starlark_file),
            loaded_modules,
            evaluation,
        ))
    }

    pub async fn eval_bzlmod_module_extension(
        &mut self,
        extension_path: &ImportPath,
        extension_module: &LoadedModule,
        extension_name: &str,
        extension_usages_json: &str,
        module_ctx_working_dir: &str,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<crate::bazel_repository::BazelModuleExtensionEvaluation> {
        let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
        let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;

        let configs = &self.configs;
        let ctx = &mut *self.ctx;
        let eval_kind = StarlarkEvalKind::Unknown(
            format!("bzlmod_module_extension/{extension_path}%{extension_name}").into(),
        );
        let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;
        let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(ctx, buckconfig, root_buckconfig);
        configs.eval_bzlmod_module_extension(
            extension_path,
            extension_module.env(),
            extension_name,
            extension_usages_json,
            module_ctx_working_dir,
            &mut buckconfigs,
            provider,
            cancellation,
        )
    }

    pub(crate) async fn eval_bzlmod_repository_rule(
        &mut self,
        rule_path: &ImportPath,
        rule_module: &LoadedModule,
        invocation: &BazelRepositoryRuleInvocation,
        repository_ctx_working_dir: &str,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<BazelRepositoryRuleEvaluation> {
        let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
        let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;

        let configs = &self.configs;
        let ctx = &mut *self.ctx;
        let eval_kind = StarlarkEvalKind::Unknown(
            format!("bzlmod_repository_rule/{}", invocation.rule_id).into(),
        );
        let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;
        let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(ctx, buckconfig, root_buckconfig);
        configs.eval_bzlmod_repository_rule(
            rule_path,
            rule_module.env(),
            invocation,
            repository_ctx_working_dir,
            &mut buckconfigs,
            provider,
            cancellation,
        )
    }

    /// Eval parent `PACKAGE` file for given package file.
    async fn eval_parent_package_file(
        &mut self,
        file: PackageLabel,
    ) -> buck2_error::Result<SuperPackage> {
        let cell_resolver = self.ctx.get_cell_resolver().await?;
        let proj_rel_path = cell_resolver.resolve_path(file.as_cell_path())?;
        match proj_rel_path.parent() {
            None => {
                // We are in the project root, there's no parent.
                Ok(SuperPackage::empty::<SuperPackageValuesImpl>()?)
            }
            Some(parent) => {
                let parent_cell = cell_resolver.get_cell_path(parent);
                self.eval_package_file(PackageLabel::from_cell_path(parent_cell.as_ref())?)
                    .await
            }
        }
    }

    /// Return `None` if there's no `PACKAGE` file in the directory.
    pub async fn prepare_package_file_eval(
        &mut self,
        package: PackageLabel,
    ) -> buck2_error::Result<Option<(PackageFilePath, AstModule, ModuleDeps)>> {
        // Note:
        /// To avoid paying the cost of read_dir when computing if any specific file has changed (e.g. PACKAGE),
        /// we depend on directory_sublisting_matching_any_case_key to invalidate all files that match (regardless of case).
        /// We need to do this to make sure to work with case-sensitive file paths.
        //   * `read_path_metadata` would not tell us if the file name is `PACKAGE`
        //     and not `package` on case-insensitive filesystems.
        //     We do case-sensitive comparison for `BUCK` files, so we do the same here.
        //   * we fail here if `PACKAGE` (but not `package`) exists, and it is not a file.

        // package file results capture starlark values and so cannot be checked for equality. This means we
        // can't get early cutoff for the consumers, and so we need to be careful to ensure our deps are precise.
        // Otherwise noop package value recomputations can lead to large recompute costs.
        //
        // Here we put the package file check behind an additional dice key so that we don't recompute on irrelevant
        // changes to the directory contents.
        #[derive(Debug, Display, Clone, Allocative, Eq, PartialEq, Hash, Pagable)]
        #[pagable_typetag(dice::DiceKeyDyn)]
        struct PackageFileLookupKey(PackageLabel);

        #[async_trait]
        impl Key for PackageFileLookupKey {
            type Value = buck2_error::Result<Option<Arc<PackageFilePath>>>;

            async fn compute(
                &self,
                ctx: &mut DiceComputations,
                _cancellation: &CancellationContext,
            ) -> Self::Value {
                for package_file_path in PackageFilePath::for_dir(self.0.as_cell_path()) {
                    if DiceFileComputations::exists_matching_exact_case(
                        ctx,
                        package_file_path.path().as_ref(),
                    )
                    .await?
                    {
                        return Ok(Some(Arc::new(package_file_path)));
                    }
                }
                Ok(None)
            }

            fn equality(x: &Self::Value, y: &Self::Value) -> bool {
                match (x, y) {
                    (Ok(x), Ok(y)) => x == y,
                    _ => false,
                }
            }

            fn validity(x: &Self::Value) -> bool {
                x.is_ok()
            }

            fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
                OkPagableValueSerialize::<Self::Value>::new()
            }
        }

        match self
            .ctx
            .compute(&PackageFileLookupKey(package.dupe()))
            .await??
        {
            Some(package_file_path) => {
                let (module, deps) = self
                    .prepare_eval(StarlarkPath::PackageFile(&package_file_path))
                    .await?;
                Ok(Some(((*package_file_path).clone(), module, deps)))
            }
            None => Ok(None),
        }
    }

    async fn eval_package_file_uncached(
        &mut self,
        path: PackageLabel,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<SuperPackage> {
        let parent = self.eval_parent_package_file(path.dupe()).await?;
        let ast_deps = self.prepare_package_file_eval(path.dupe()).await?;

        let (package_file_path, ast, deps) = match ast_deps {
            Some(x) => x,
            None => {
                // If there's no `PACKAGE` file, return parent.
                return Ok(parent);
            }
        };

        let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
        let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;

        let configs = &self.configs;
        let ctx = &mut *self.ctx;

        let eval_kind = StarlarkEvalKind::LoadPackageFile(path.dupe());
        let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;

        let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(ctx, buckconfig, root_buckconfig);

        configs
            .eval_package_file(
                &package_file_path,
                ast,
                parent,
                &mut buckconfigs,
                deps.get_loaded_modules(),
                provider,
                cancellation,
            )
            .with_buck_error_context(|| format!("evaluating Starlark PACKAGE file `{path}`"))
    }

    pub(crate) async fn eval_package_file(
        &mut self,
        path: PackageLabel,
    ) -> buck2_error::Result<SuperPackage> {
        #[derive(Debug, Display, Clone, Allocative, Eq, PartialEq, Hash, Pagable)]
        #[pagable_typetag(dice::DiceKeyDyn)]
        struct PackageFileKey(PackageLabel);

        #[async_trait]
        impl Key for PackageFileKey {
            type Value = buck2_error::Result<SuperPackage>;

            async fn compute(
                &self,
                ctx: &mut DiceComputations,
                cancellation: &CancellationContext,
            ) -> Self::Value {
                let mut interpreter = ctx
                    .get_interpreter_calculator(OwnedStarlarkPath::PackageFile(
                        PackageFilePath::package_file_for_dir(self.0.as_cell_path()),
                    ))
                    .await?;
                interpreter
                    .eval_package_file_uncached(self.0.dupe(), cancellation)
                    .await
            }

            fn equality(x: &Self::Value, y: &Self::Value) -> bool {
                match (x, y) {
                    (Ok(x), Ok(y)) => x == y,
                    _ => false,
                }
            }

            fn validity(x: &Self::Value) -> bool {
                x.is_ok()
            }

            fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
                OkPagableValueSerialize::<Self::Value>::new()
            }
        }

        self.ctx.compute(&PackageFileKey(path)).await?
    }

    /// Most directories do not contain a `PACKAGE` file, this function
    /// optimizes `eval_package_file` for this case by avoiding creation of DICE key.
    pub(crate) async fn eval_package_file_for_build_file(
        &mut self,
        package: PackageLabel,
        package_listing: &PackageListing,
    ) -> buck2_error::Result<SuperPackage> {
        for package_file_name in PackageFilePath::package_file_names() {
            if package_listing
                .get_file(package_file_name.as_ref())
                .is_some()
            {
                return self.eval_package_file(package).await;
            }
        }

        // Without this optimization, `cquery <that android target>` has 6% time regression.
        // With this optimization, check for `PACKAGE` files adds 2% to time.
        self.eval_parent_package_file(package).await
    }

    async fn resolve_package_listing(
        ctx: &mut DiceComputations<'_>,
        package: PackageLabel,
    ) -> buck2_error::Result<PackageListing> {
        span_async_simple(
            buck2_data::LoadPackageStart {
                path: package.as_cell_path().to_string(),
            },
            DicePackageListingResolver(ctx).resolve_package_listing(package.dupe()),
            buck2_data::LoadPackageEnd {
                path: package.as_cell_path().to_string(),
            },
        )
        .await
    }

    async fn resolve_package_listing_with_strategy(
        ctx: &mut DiceComputations<'_>,
        package: PackageLabel,
        strategy: PackageListingStrategy,
    ) -> buck2_error::Result<PackageListing> {
        span_async_simple(
            buck2_data::LoadPackageStart {
                path: package.as_cell_path().to_string(),
            },
            DicePackageListingResolver(ctx)
                .resolve_package_listing_with_strategy(package.dupe(), strategy),
            buck2_data::LoadPackageEnd {
                path: package.as_cell_path().to_string(),
            },
        )
        .await
    }

    pub async fn eval_build_file(
        &mut self,
        package: PackageLabel,
        cancellation: &CancellationContext,
    ) -> (TimeSpan, buck2_error::Result<Arc<EvaluationResult>>) {
        let mut now = None;
        let eval_kind = StarlarkEvalKind::LoadBuildFile(package.dupe());
        let eval_result: buck2_error::Result<_> = try {
            let package_cell = package.cell_name();
            let ((), bazel_compat_listing) = self
                .ctx
                .try_compute2(
                    |ctx| check_starlark_stack_size(ctx).boxed(),
                    |ctx| {
                        async move { bazel_compat_package_listing_enabled(ctx, package_cell).await }
                            .boxed()
                    },
                )
                .await?;

            let (mut listing, build_file_path, ast, deps, mut listing_strategy) =
                if bazel_compat_listing {
                    let shallow_listing = Self::resolve_package_listing_with_strategy(
                        self.ctx,
                        package.dupe(),
                        PackageListingStrategy::Shallow,
                    )
                    .await?;
                    let build_file_path =
                        BuildFilePath::new(package.dupe(), shallow_listing.buildfile().to_owned());
                    let parse_data = self
                        .parse_file(StarlarkPath::BuildFile(&build_file_path))
                        .await??;
                    let strategy = parse_data
                        .bazel_package_listing_strategy
                        .clone()
                        .unwrap_or(PackageListingStrategy::Recursive);
                    let (listing, deps) = if strategy == PackageListingStrategy::Shallow {
                        let deps =
                            Self::eval_deps_with_cycle_guard(self.ctx, &parse_data.imports).await?;
                        (shallow_listing, deps)
                    } else {
                        let imports = parse_data.imports.clone();
                        self.ctx
                            .try_compute2(
                                {
                                    let package = package.dupe();
                                    let strategy = strategy.clone();
                                    move |ctx| {
                                        async move {
                                            Self::resolve_package_listing_with_strategy(
                                                ctx,
                                                package.dupe(),
                                                strategy.clone(),
                                            )
                                            .await
                                        }
                                        .boxed()
                                    }
                                },
                                move |ctx| {
                                    async move {
                                        Self::eval_deps_with_cycle_guard(ctx, &imports).await
                                    }
                                    .boxed()
                                },
                            )
                            .await?
                    };
                    (listing, build_file_path, parse_data.ast, deps, strategy)
                } else {
                    let listing = Self::resolve_package_listing(self.ctx, package.dupe()).await?;
                    let build_file_path =
                        BuildFilePath::new(package.dupe(), listing.buildfile().to_owned());
                    let (ast, deps) = self
                        .prepare_eval(StarlarkPath::BuildFile(&build_file_path))
                        .await?;
                    (
                        listing,
                        build_file_path,
                        ast,
                        deps,
                        PackageListingStrategy::Recursive,
                    )
                };
            let super_package = self
                .eval_package_file_for_build_file(package.dupe(), &listing)
                .await?;

            let package_boundary_exception = self
                .ctx
                .get_package_boundary_exception(package.as_cell_path())
                .await?
                .is_some();
            let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
            let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;
            let module_id = build_file_path.to_string();
            let cell_str = build_file_path.cell().as_str().to_owned();

            let configs = &self.configs;

            now = Some(TimeSpan::start_now());
            let (profile_data, eval_result) = loop {
                let ctx = &mut *self.ctx;
                let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind.dupe()).await?;
                let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(
                    ctx,
                    buckconfig.dupe(),
                    root_buckconfig.dupe(),
                );
                let start_event = buck2_data::LoadBuildFileStart {
                    cell: cell_str.clone(),
                    module_id: module_id.clone(),
                };
                let span_module_id = module_id.clone();
                let span_cell_str = cell_str.clone();

                let eval_attempt = span(start_event, || {
                    let result_with_stats = configs
                        .eval_build_file(
                            &build_file_path,
                            &mut buckconfigs,
                            listing.dupe(),
                            listing_strategy.clone(),
                            super_package.dupe(),
                            package_boundary_exception,
                            ast.clone(),
                            deps.get_loaded_modules(),
                            provider,
                            false,
                            cancellation,
                        )
                        .with_buck_error_context(|| {
                            format!("Error evaluating build file: `{}`", build_file_path)
                        });
                    let error = result_with_stats.as_ref().err().map(|e| format!("{e:#}"));
                    let (
                        starlark_peak_allocated_bytes,
                        cpu_instruction_count,
                        starlark_tick_count,
                        target_count,
                    ) = match &result_with_stats {
                        Ok(BuildFileEvalResult::Complete(_, rs)) => (
                            Some(rs.starlark_peak_allocated_bytes),
                            rs.cpu_instruction_count,
                            Some(rs.starlark_tick_count),
                            Some(rs.result.targets().len() as u64),
                        ),
                        Ok(BuildFileEvalResult::NeedsPackageListing(_)) => (None, None, None, None),
                        Err(_) => (None, None, None, None),
                    };

                    (
                        result_with_stats,
                        buck2_data::LoadBuildFileEnd {
                            module_id: span_module_id,
                            cell: span_cell_str,
                            target_count,
                            starlark_peak_allocated_bytes,
                            cpu_instruction_count,
                            error,
                            starlark_tick_count,
                        },
                    )
                })?;
                drop(buckconfigs);

                match eval_attempt {
                    BuildFileEvalResult::Complete(profile_data, eval_result) => {
                        break (profile_data, eval_result);
                    }
                    BuildFileEvalResult::NeedsPackageListing(next_strategy) => {
                        if listing_strategy.covers(&next_strategy) {
                            return (
                                now.unwrap().end_now(),
                                Err(internal_error!(
                                    "package listing restart did not expand strategy from `{:?}` to `{:?}`",
                                    listing_strategy,
                                    next_strategy
                                )),
                            );
                        }
                        listing_strategy = next_strategy;
                        listing = Self::resolve_package_listing_with_strategy(
                            self.ctx,
                            package.dupe(),
                            listing_strategy.clone(),
                        )
                        .await?;
                    }
                }
            };

            let mut eval_result = eval_result.result;

            if eval_result.starlark_profile.is_some() {
                return (
                    now.unwrap().end_now(),
                    Err(internal_error!(
                        "starlark_profile field must not be set yet"
                    )),
                );
            }
            eval_result.starlark_profile = profile_data.map(|d| d as _);
            eval_result
        };

        (
            now.map_or(TimeSpan::empty_now(), |v| v.end_now()),
            eval_result.map(Arc::new),
        )
    }
}

mod keys {
    use allocative::Allocative;
    use buck2_interpreter::paths::module::OwnedStarlarkModulePath;
    use derive_more::Display;
    use pagable::Pagable;
    use pagable::pagable_typetag;

    #[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    pub struct EvalImportKey(pub OwnedStarlarkModulePath);
}

pub mod testing {
    // re-exports for testing
    pub use super::keys::EvalImportKey;
}
