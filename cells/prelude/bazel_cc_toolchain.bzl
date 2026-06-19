def _files(dep):
    if dep != None and DefaultInfo in dep:
        return dep[DefaultInfo].files
    return depset()

def _is_versioned_shared_library_name(name):
    for extension in [".so.", ".dylib."]:
        separator = name.find(extension)
        if separator == -1:
            continue
        if separator == 0:
            return False
        for part in name[separator + len(extension):].split("."):
            if not part:
                return False
            if part[0] < "0" or part[0] > "9":
                return False
            for char in part[1:].elems():
                if not (char.isalnum() or char == "_"):
                    return False
        return True
    return False

def _is_valid_shared_library(artifact):
    basename = artifact.basename
    for extension in [".so", ".dll", ".dylib", ".pyd", ".wasm"]:
        if basename.endswith(extension):
            return True
    return _is_versioned_shared_library_name(basename)

_PATH_ESCAPE_REPLACEMENTS = {
    "_": "_U",
    "/": "_S",
    "\\": "_B",
    ":": "_C",
    "@": "_A",
}

def _escape_label(label):
    path = label.package + ":" + label.name
    if label.repo_name:
        path = label.repo_name + "@" + path
    return "".join([
        _PATH_ESCAPE_REPLACEMENTS.get(char, char)
        for char in path.elems()
    ])

def _dynamic_runtime_lib(ctx, cc_internal, dynamic_runtime_lib, runtime_solib_dir_base):
    if dynamic_runtime_lib == None:
        return None

    symlinks = [
        cc_internal.solib_symlink_action(
            ctx = ctx,
            artifact = artifact,
            solib_directory = "_solib_" + ctx.attrs.toolchain_config[CcToolchainConfigInfo].target_cpu,
            runtime_solib_dir_base = runtime_solib_dir_base,
        )
        for artifact in dynamic_runtime_lib[DefaultInfo].files.to_list()
        if _is_valid_shared_library(artifact)
    ]
    return depset(symlinks)

def _toolchain_config(ctx):
    if ctx.attrs.toolchain_config != None and CcToolchainConfigInfo in ctx.attrs.toolchain_config:
        return ctx.attrs.toolchain_config[CcToolchainConfigInfo]
    return None

def _tools_directory(ctx):
    workspace_root = ctx.label.workspace_root
    package = ctx.label.package
    if workspace_root and package:
        return workspace_root + "/" + package
    if workspace_root:
        return workspace_root
    return package

def _exec_path(tools_directory, path):
    if path.startswith("/") or not tools_directory:
        return path
    return tools_directory + "/" + path

def _tool_paths(toolchain_config_info, tools_directory):
    if toolchain_config_info == None:
        return {}
    return {
        tool.name: _exec_path(tools_directory, tool.path)
        for tool in toolchain_config_info.tool_paths
    }

def _tool_path(tool_paths, name, default = ""):
    value = tool_paths.get(name)
    return value if value != None else default

def _empty_compilation_context(cc_internal):
    header_info = cc_internal.create_header_info(
        modular_private_headers = [],
        modular_public_headers = [],
        separate_module_headers = [],
        textual_headers = [],
    )
    return struct(
        defines = depset(),
        direct_headers = [],
        direct_private_headers = [],
        direct_public_headers = [],
        direct_textual_headers = [],
        external_includes = depset(),
        framework_includes = depset(),
        headers = depset(),
        includes = depset(),
        local_defines = depset(),
        quote_includes = depset(),
        system_includes = depset(),
        validation_artifacts = depset(),
        _direct_module_maps = depset(),
        _exporting_module_map_files = depset(),
        _exporting_module_maps = depset(),
        _header_info = header_info,
        _module_files = depset(),
        _module_map = None,
        _modules_info_files = depset(),
        _non_code_inputs = depset(),
        _pic_module_files = depset(),
        _pic_modules_info_files = depset(),
        _transitive_modules = depset(),
        _transitive_pic_modules = depset(),
        _virtual_to_original_headers = depset(),
    )

def cc_toolchain_impl(ctx):
    toolchain_config_info = _toolchain_config(ctx)
    cc_internal = cc_common.internal_DO_NOT_USE()
    tools_directory = _tools_directory(ctx)
    tool_paths = _tool_paths(toolchain_config_info, tools_directory)
    compiler = toolchain_config_info.compiler if toolchain_config_info != None else (ctx.attrs.compiler or "")
    libc = toolchain_config_info.target_libc if toolchain_config_info != None else ""
    cpu = toolchain_config_info.target_cpu if toolchain_config_info != None else ""
    target_system_name = toolchain_config_info.target_system_name if toolchain_config_info != None else ""
    toolchain_id = toolchain_config_info.toolchain_id if toolchain_config_info != None else (ctx.attrs.toolchain_identifier or "")
    abi = toolchain_config_info.abi_version if toolchain_config_info != None else None
    abi_glibc = toolchain_config_info.abi_libc_version if toolchain_config_info != None else None
    sysroot = toolchain_config_info.builtin_sysroot if toolchain_config_info != None and toolchain_config_info.builtin_sysroot != "" else None
    all_files = _files(ctx.attrs.all_files)
    runtime_solib_dir_base = "_solib__" + _escape_label(ctx.label)
    dynamic_runtime_lib = _dynamic_runtime_lib(ctx, cc_internal, ctx.attrs.dynamic_runtime_lib, runtime_solib_dir_base)
    static_runtime_lib = _files(ctx.attrs.static_runtime_lib)
    build_variables = cc_internal.cc_toolchain_variables(vars = {})
    toolchain_features = cc_internal.cc_toolchain_features(
        toolchain_config_info = toolchain_config_info,
        tools_directory = tools_directory,
    )
    cc_info = CcInfo(compilation_context = _empty_compilation_context(cc_internal))
    cc_toolchain_info = cc_common.CcToolchainInfo(
        _abi = abi,
        _abi_glibc_version = abi_glibc,
        _additional_make_variables = {},
        _aggregate_ddi = None,
        _all_files_including_libc = all_files,
        _ar_files = _files(ctx.attrs.ar_files),
        _as_files = _files(ctx.attrs.as_files),
        _build_info_files = None,
        _build_variables = build_variables,
        _build_variables_dict = {},
        _builtin_include_files = [],
        _cc_info = cc_info,
        _compiler_files = _files(ctx.attrs.compiler_files),
        _compiler_files_without_includes = depset(),
        _coverage_files = _files(ctx.attrs.coverage_files),
        _crosstool_top_path = tools_directory,
        _cpp_configuration = ctx.fragments.cpp,
        _dwp_files = _files(ctx.attrs.dwp_files),
        _dynamic_runtime_lib_depset = dynamic_runtime_lib,
        _fdo_context = None,
        _generate_modmap = None,
        _grep_includes = None,
        _if_so_builder = None,
        _is_sibling_repository_layout = False,
        _is_tool_configuration = False,
        _legacy_cc_flags_make_variable = "",
        _link_dynamic_library_tool = None,
        _linker_files = _files(ctx.attrs.linker_files),
        _objcopy_files = _files(ctx.attrs.objcopy_files),
        _solib_dir = "",
        _stamp_binaries = False,
        _static_runtime_lib_depset = static_runtime_lib,
        _strip_files = _files(ctx.attrs.strip_files),
        _supports_header_parsing = ctx.attrs.supports_header_parsing,
        _supports_param_files = ctx.attrs.supports_param_files,
        _tool_paths = tool_paths,
        _toolchain_features = toolchain_features,
        _toolchain_label = ctx.label,
        all_files = all_files,
        ar_executable = _tool_path(tool_paths, "ar", "ar"),
        built_in_include_directories = toolchain_config_info.cxx_builtin_include_directories if toolchain_config_info != None else [],
        compiler = compiler,
        compiler_executable = _tool_path(tool_paths, "gcc", compiler),
        cpu = cpu,
        dynamic_runtime_lib = lambda *, feature_configuration: dynamic_runtime_lib if feature_configuration.is_enabled("static_link_cpp_runtimes") else depset(),
        dynamic_runtime_solib_dir = runtime_solib_dir_base,
        gcov_executable = _tool_path(tool_paths, "gcov"),
        generate_modmap = None,
        ld_executable = _tool_path(tool_paths, "ld", "ld"),
        libc = libc,
        needs_pic_for_dynamic_libraries = lambda *, feature_configuration: True,
        nm_executable = _tool_path(tool_paths, "nm", "nm"),
        objcopy_executable = _tool_path(tool_paths, "objcopy"),
        objdump_executable = _tool_path(tool_paths, "objdump"),
        preprocessor_executable = _tool_path(tool_paths, "cpp"),
        static_runtime_lib = lambda *, feature_configuration: static_runtime_lib if feature_configuration.is_enabled("static_link_cpp_runtimes") else depset(),
        strip_executable = _tool_path(tool_paths, "strip", "strip"),
        sysroot = sysroot,
        target_gnu_system_name = target_system_name,
        toolchain_id = toolchain_id,
        toolchain_identifier = toolchain_id,
    )
    return [
        DefaultInfo(),
        cc_toolchain_info,
        platform_common.ToolchainInfo(
            cc = cc_toolchain_info,
            cc_provider_in_toolchain = True,
        ),
    ]

def cc_toolchain_suite_impl(ctx):
    _ = ctx.attrs.toolchains
    return [DefaultInfo()]
