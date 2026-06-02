use super::*;

pub(super) fn write_repository_rule_repo(
    project_fs: &ProjectRoot,
    dest: &AbsNormPath,
    canonical_repo_name: &str,
    setup: &BzlmodRepositoryRuleSetup,
) -> bz_error::Result<()> {
    if let Some(source_dir) = &setup.source_dir {
        let source_dir = ProjectRelativePath::new(source_dir.as_ref())?;
        let source = project_fs.resolve(source_dir);
        copy_dir_contents(&source, dest)?;
    }
    write_generated_module_file(dest, canonical_repo_name)?;
    for file in setup.files.iter() {
        let rel_path = ForwardRelativePath::new(file.path.as_ref())?;
        let path = dest.join(rel_path);
        if let Some(parent) = path.parent() {
            fs_util::create_dir_all(parent)?;
        }
        fs_util::remove_all(&path).categorize_internal()?;
        fs_util::write(&path, file.content.as_bytes()).categorize_internal()?;
        if file.executable {
            fs_util::set_executable(&path, true).categorize_internal()?;
        }
    }
    let build_bazel = dest.join(ForwardRelativePath::new("BUILD.bazel")?);
    if fs_util::symlink_metadata_if_exists(&build_bazel)?.is_none() {
        let build = dest.join(ForwardRelativePath::new("BUILD")?);
        match fs_util::metadata(&build) {
            Ok(metadata) if metadata.is_file() => {
                if let Some(build_content) = fs_util::read_to_string_if_exists(&build)? {
                    fs_util::write(build_bazel, build_content).categorize_internal()?;
                }
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.io_error_kind(),
                    Some(ErrorKind::NotFound | ErrorKind::NotADirectory)
                ) => {}
            Err(error) => return Err(error.categorize_internal()),
        }
    }
    Ok(())
}

pub(super) fn write_cc_autoconf_toolchains_repo(
    project_fs: &ProjectRoot,
    dest_rel: &ProjectRelativePath,
    dest: &AbsNormPath,
    setup: &BzlmodCcAutoconfToolchainsSetup,
) -> bz_error::Result<()> {
    write_generated_module_file(dest, "local_config_cc_toolchains")?;
    let template =
        cc_toolchains_build_template(project_fs, dest_rel, &setup.parent_canonical_repo_name)?;
    let build = local_config_cc_toolchains_build_file(&template);
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    Ok(())
}

fn actual_host_constraints_bzl() -> String {
    let mut constraints = Vec::new();
    if let Some(cpu) = host_platform_cpu_constraint() {
        constraints.push(format!("    \"@platforms//cpu:{cpu}\","));
    }
    if let Some(os) = host_platform_os_constraint() {
        constraints.push(format!("    \"@platforms//os:{os}\","));
    }
    format!("HOST_CONSTRAINTS = [\n{}\n]\n", constraints.join("\n"))
}

pub(super) fn local_config_cc_toolchains_build_file(template: &str) -> String {
    template
        .replacen(
            "load(\"@platforms//host:constraints.bzl\", \"HOST_CONSTRAINTS\")\n",
            &actual_host_constraints_bzl(),
            1,
        )
        .replace("%{name}", host_cc_cpu_value())
}

pub(super) fn write_cc_autoconf_repo(
    dest: &AbsNormPath,
    _setup: &BzlmodCcAutoconfSetup,
) -> bz_error::Result<()> {
    write_generated_module_file(dest, "local_config_cc")?;
    write_cc_autoconf_support_files(dest)?;
    let build = local_config_cc_build_file();
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    Ok(())
}

pub(super) fn write_xcode_config_repo(
    dest: &AbsNormPath,
    _setup: &BzlmodXcodeConfigSetup,
) -> bz_error::Result<()> {
    write_generated_module_file(dest, "local_config_xcode")?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("xcode_config.bzl")?),
        local_config_xcode_bzl_file(),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("BUILD.bazel")?),
        local_config_xcode_build_file(),
    )
    .categorize_internal()?;
    Ok(())
}

fn local_config_xcode_build_file() -> String {
    r#"
load(":xcode_config.bzl", "xcode_config")

package(default_visibility = ["//visibility:public"])

xcode_config(
    name = "host_xcodes",
)
"#
    .to_owned()
}

fn local_config_xcode_bzl_file() -> String {
    let macos_sdk_version = starlark_string_literal(&host_macos_sdk_version());
    format!(
        r#"
def _version_or_default(value, default):
    if value:
        return str(value)
    return default

def _xcode_config_impl(ctx):
    apple_fragment = ctx.fragments.apple
    macos_sdk_version = {macos_sdk_version}
    macos_minimum_os = _version_or_default(apple_fragment.macos_minimum_os_flag, macos_sdk_version)

    return [apple_common.XcodeVersionConfig(
        ios_sdk_version = "0.0",
        ios_minimum_os_version = "0.0",
        visionos_sdk_version = "0.0",
        visionos_minimum_os_version = "0.0",
        watchos_sdk_version = "0.0",
        watchos_minimum_os_version = "0.0",
        tvos_sdk_version = "0.0",
        tvos_minimum_os_version = "0.0",
        macos_sdk_version = macos_sdk_version,
        macos_minimum_os_version = macos_minimum_os,
        xcode_version = None,
        availability = "UNKNOWN",
        xcode_version_flag = None,
        include_xcode_execution_info = False,
    )]

xcode_config = rule(
    implementation = _xcode_config_impl,
    fragments = ["apple"],
)
"#
    )
}

pub(super) fn host_macos_sdk_version() -> String {
    static HOST_MACOS_SDK_VERSION: OnceLock<String> = OnceLock::new();
    HOST_MACOS_SDK_VERSION
        .get_or_init(|| detect_host_macos_sdk_version().unwrap_or_else(|| "0.0".to_owned()))
        .clone()
}

fn detect_host_macos_sdk_version() -> Option<String> {
    if std::env::consts::OS != "macos" {
        return None;
    }
    let output = std::process::Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-version"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8(output.stdout).ok()?;
    let version = version.trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_owned())
    }
}

fn write_cc_autoconf_support_files(dest: &AbsNormPath) -> bz_error::Result<()> {
    let cc = host_tool_path("CC", "cc");
    let cxx = host_tool_path("CXX", "c++");
    let ar = if std::env::consts::OS == "macos" {
        host_tool_path("LIBTOOL", "libtool")
    } else {
        host_tool_path("AR", "ar")
    };

    fs_util::write(
        dest.join(ForwardRelativePath::new("cc_toolchain_config.bzl")?),
        LOCAL_CONFIG_CC_TOOLCHAIN_CONFIG,
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("armeabi_cc_toolchain_config.bzl")?),
        LOCAL_CONFIG_CC_ARMEABI_TOOLCHAIN_CONFIG,
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("cc_wrapper.sh")?),
        format!(
            r#"#!/usr/bin/env bash
has_c_source=0
has_cxx_source=0
previous_arg=
for arg in "$@"; do
  if [[ "$previous_arg" == "-x" ]]; then
    case "$arg" in
      c|objective-c)
        has_c_source=1
        ;;
      c++|objective-c++|c++-*|objective-c++-*)
        has_cxx_source=1
        ;;
    esac
  fi
  case "$arg" in
    -xc|-xobjective-c)
      has_c_source=1
      ;;
    -xc++|-xobjective-c++|-xc++-*|-xobjective-c++-*)
      has_cxx_source=1
      ;;
    *.c|*.m)
      has_c_source=1
      ;;
    *.cc|*.cpp|*.cxx|*.c++|*.C|*.mm)
      has_cxx_source=1
      ;;
  esac
  previous_arg="$arg"
done

if [[ "$has_c_source" == "1" && "$has_cxx_source" == "0" ]]; then
  exec {} "$@"
fi

exec {} "$@"
"#,
            shell_quote(&cc),
            shell_quote(&cxx),
        ),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("ar_wrapper.sh")?),
        format!("#!/usr/bin/env bash\nexec {} \"$@\"\n", shell_quote(&ar)),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("dwp_wrapper.sh")?),
        host_tool_wrapper("DWP", "dwp"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("gcov_wrapper.sh")?),
        host_tool_wrapper("GCOV", "gcov"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("llvm_profdata_wrapper.sh")?),
        host_tool_wrapper("LLVM_PROFDATA", "llvm-profdata"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("nm_wrapper.sh")?),
        host_tool_wrapper("NM", "nm"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("objcopy_wrapper.sh")?),
        host_tool_wrapper("OBJCOPY", "objcopy"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("objdump_wrapper.sh")?),
        host_tool_wrapper("OBJDUMP", "objdump"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("strip_wrapper.sh")?),
        host_tool_wrapper("STRIP", "strip"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("deps_scanner_wrapper.sh")?),
        format!("#!/usr/bin/env bash\nexec {} \"$@\"\n", shell_quote(&cc)),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("validate_static_library.sh")?),
        "#!/usr/bin/env bash\nexit 0\n",
    )
    .categorize_internal()?;
    for wrapper in [
        "cc_wrapper.sh",
        "ar_wrapper.sh",
        "dwp_wrapper.sh",
        "gcov_wrapper.sh",
        "llvm_profdata_wrapper.sh",
        "nm_wrapper.sh",
        "objcopy_wrapper.sh",
        "objdump_wrapper.sh",
        "strip_wrapper.sh",
        "deps_scanner_wrapper.sh",
        "validate_static_library.sh",
    ] {
        fs_util::set_executable(&dest.join(ForwardRelativePath::new(wrapper)?), true)
            .categorize_internal()?;
    }
    fs_util::create_dir_all(dest.join(ForwardRelativePath::new("tools/cpp")?))?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("tools/cpp/empty.cc")?),
        "int main() { return 0; }\n",
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("builtin_include_directory_paths")?),
        "",
    )
    .categorize_internal()?;
    Ok(())
}

fn host_tool_wrapper(env_var: &str, fallback: &str) -> String {
    match find_host_tool_path(env_var, fallback) {
        Some(path) => format!("#!/usr/bin/env bash\nexec {} \"$@\"\n", shell_quote(&path)),
        None => missing_tool_wrapper_content(fallback),
    }
}

fn missing_tool_wrapper_content(description: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
echo \"Buck2 generated local_config_cc cannot execute {description}.\" >&2\n\
exit 1\n",
    )
}

fn host_tool_path(env_var: &str, fallback: &str) -> String {
    find_host_tool_path(env_var, fallback).unwrap_or_else(|| fallback.to_owned())
}

fn find_host_tool_path(env_var: &str, fallback: &str) -> Option<String> {
    if let Ok(value) = std::env::var(env_var) {
        if !value.trim().is_empty() {
            return Some(value);
        }
    }
    find_executable_on_path(fallback)
}

fn find_executable_on_path(name: &str) -> Option<String> {
    if name.contains('/') || name.contains('\\') {
        let path = std::path::Path::new(name);
        return path.exists().then(|| path.to_string_lossy().into_owned());
    }

    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn local_config_cc_build_file() -> String {
    let host_toolchain_identifier = host_cc_toolchain_identifier();
    LOCAL_CONFIG_CC_BUILD
        .replace(
            "%HOST_CPU_LITERAL%",
            &starlark_string_literal(host_cc_cpu_value()),
        )
        .replace("%HOST_CPU%", host_cc_cpu_value())
        .replace(
            "%HOST_TOOLCHAIN_IDENTIFIER_LITERAL%",
            &starlark_string_literal(&host_toolchain_identifier),
        )
        .replace("%HOST_TOOLCHAIN_IDENTIFIER%", &host_toolchain_identifier)
        .replace(
            "%HOST_TARGET_LIBC%",
            &starlark_string_literal(host_cc_target_libc()),
        )
}

fn starlark_string_literal(value: &str) -> String {
    format!("{value:?}")
}

fn host_cc_toolchain_identifier() -> String {
    std::env::var("CC_TOOLCHAIN_NAME").unwrap_or_else(|_| "local".to_owned())
}

fn host_cc_target_libc() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macosx",
        "android" => "android",
        _ => "local",
    }
}

const LOCAL_CONFIG_CC_BUILD: &str = r#"load(":cc_toolchain_config.bzl", "cc_toolchain_config")
load(":armeabi_cc_toolchain_config.bzl", "armeabi_cc_toolchain_config")
load("@rules_cc//cc/toolchains:cc_toolchain.bzl", "cc_toolchain")
load("@rules_cc//cc/toolchains:cc_toolchain_suite.bzl", "cc_toolchain_suite")

package(default_visibility = ["//visibility:public"])

licenses(["notice"])

cc_library(name = "empty_lib")

label_flag(
    name = "link_extra_libs",
    build_setting_default = ":empty_lib",
)

cc_library(
    name = "link_extra_lib",
    deps = [
        ":link_extra_libs",
    ],
)

cc_library(name = "malloc")

filegroup(
    name = "empty",
    srcs = [],
)

filegroup(
    name = "builtin_include_directory_paths",
    srcs = ["builtin_include_directory_paths"],
)

filegroup(
    name = "cc_wrapper",
    srcs = ["cc_wrapper.sh"],
)

filegroup(
    name = "ar_wrapper",
    srcs = ["ar_wrapper.sh"],
)

filegroup(
    name = "dwp_wrapper",
    srcs = ["dwp_wrapper.sh"],
)

filegroup(
    name = "gcov_wrapper",
    srcs = ["gcov_wrapper.sh"],
)

filegroup(
    name = "llvm_profdata_wrapper",
    srcs = ["llvm_profdata_wrapper.sh"],
)

filegroup(
    name = "nm_wrapper",
    srcs = ["nm_wrapper.sh"],
)

filegroup(
    name = "objcopy_wrapper",
    srcs = ["objcopy_wrapper.sh"],
)

filegroup(
    name = "objdump_wrapper",
    srcs = ["objdump_wrapper.sh"],
)

filegroup(
    name = "strip_wrapper",
    srcs = ["strip_wrapper.sh"],
)

filegroup(
    name = "deps_scanner_wrapper",
    srcs = ["deps_scanner_wrapper.sh"],
)

filegroup(
    name = "validate_static_library",
    srcs = ["validate_static_library.sh"],
)

filegroup(
    name = "compiler_deps",
    srcs = [
        "builtin_include_directory_paths",
        "cc_wrapper.sh",
        "deps_scanner_wrapper.sh",
    ],
)

filegroup(
    name = "ar_files",
    srcs = [
        "ar_wrapper.sh",
        "builtin_include_directory_paths",
        "cc_wrapper.sh",
        "deps_scanner_wrapper.sh",
        "validate_static_library.sh",
    ],
)

filegroup(
    name = "dwp_files",
    srcs = ["dwp_wrapper.sh"],
)

filegroup(
    name = "objcopy_files",
    srcs = ["objcopy_wrapper.sh"],
)

filegroup(
    name = "strip_files",
    srcs = ["strip_wrapper.sh"],
)

cc_toolchain_suite(
    name = "toolchain",
    toolchains = {
        "%HOST_CPU%|compiler": ":cc-compiler-%HOST_CPU%",
        "%HOST_CPU%": ":cc-compiler-%HOST_CPU%",
        "armeabi-v7a|compiler": ":cc-compiler-armeabi-v7a",
        "armeabi-v7a": ":cc-compiler-armeabi-v7a",
    },
)

cc_toolchain(
    name = "cc-compiler-%HOST_CPU%",
    toolchain_identifier = %HOST_TOOLCHAIN_IDENTIFIER_LITERAL%,
    toolchain_config = ":%HOST_TOOLCHAIN_IDENTIFIER%",
    all_files = ":compiler_deps",
    ar_files = ":ar_files",
    as_files = ":compiler_deps",
    compiler_files = ":compiler_deps",
    dwp_files = ":dwp_files",
    linker_files = ":compiler_deps",
    objcopy_files = ":objcopy_files",
    strip_files = ":strip_files",
    supports_header_parsing = True,
    supports_param_files = True,
)

cc_toolchain_config(
    name = %HOST_TOOLCHAIN_IDENTIFIER_LITERAL%,
    cpu = %HOST_CPU_LITERAL%,
    compiler = "compiler",
    toolchain_identifier = %HOST_TOOLCHAIN_IDENTIFIER_LITERAL%,
    host_system_name = "local",
    target_system_name = "local",
    target_libc = %HOST_TARGET_LIBC%,
    abi_version = "local",
    abi_libc_version = "local",
    tool_paths = {
        "ar": "ar_wrapper.sh",
        "cpp": "cc_wrapper.sh",
        "cpp-module-deps-scanner": "deps_scanner_wrapper.sh",
        "dwp": "dwp_wrapper.sh",
        "gcc": "cc_wrapper.sh",
        "gcov": "gcov_wrapper.sh",
        "ld": "cc_wrapper.sh",
        "llvm-profdata": "llvm_profdata_wrapper.sh",
        "nm": "nm_wrapper.sh",
        "objcopy": "objcopy_wrapper.sh",
        "objdump": "objdump_wrapper.sh",
        "parse_headers": "cc_wrapper.sh",
        "strip": "strip_wrapper.sh",
        "validate_static_library": "validate_static_library.sh",
    },
)

cc_toolchain(
    name = "cc-compiler-armeabi-v7a",
    toolchain_identifier = "stub_armeabi-v7a",
    toolchain_config = ":stub_armeabi-v7a",
    all_files = ":empty",
    ar_files = ":empty",
    as_files = ":empty",
    compiler_files = ":empty",
    dwp_files = ":empty",
    linker_files = ":empty",
    objcopy_files = ":empty",
    strip_files = ":empty",
    supports_param_files = 1,
)

armeabi_cc_toolchain_config(name = "stub_armeabi-v7a")
"#;

const LOCAL_CONFIG_CC_TOOLCHAIN_CONFIG: &str = r#"load("@rules_cc//cc/toolchains:cc_toolchain_config_info.bzl", "CcToolchainConfigInfo")

def _tool_path(name, path):
    return struct(name = name, path = path)

def _impl(ctx):
    tool_paths = [
        _tool_path(name, path)
        for name, path in ctx.attr.tool_paths.items()
    ]
    return [cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        features = [],
        action_configs = [],
        artifact_name_patterns = [],
        cxx_builtin_include_directories = ctx.attr.cxx_builtin_include_directories,
        toolchain_identifier = ctx.attr.toolchain_identifier,
        host_system_name = ctx.attr.host_system_name,
        target_system_name = ctx.attr.target_system_name,
        target_cpu = ctx.attr.cpu,
        target_libc = ctx.attr.target_libc,
        compiler = ctx.attr.compiler,
        abi_version = ctx.attr.abi_version,
        abi_libc_version = ctx.attr.abi_libc_version,
        tool_paths = tool_paths,
        builtin_sysroot = ctx.attr.builtin_sysroot,
        cc_target_os = None,
    )]

cc_toolchain_config = rule(
    implementation = _impl,
    attrs = {
        "abi_libc_version": attr.string(mandatory = True),
        "abi_version": attr.string(mandatory = True),
        "builtin_sysroot": attr.string(),
        "compiler": attr.string(mandatory = True),
        "cpu": attr.string(mandatory = True),
        "cxx_builtin_include_directories": attr.string_list(),
        "host_system_name": attr.string(mandatory = True),
        "target_libc": attr.string(mandatory = True),
        "target_system_name": attr.string(mandatory = True),
        "tool_paths": attr.string_dict(),
        "toolchain_identifier": attr.string(mandatory = True),
    },
    provides = [CcToolchainConfigInfo],
)
"#;

const LOCAL_CONFIG_CC_ARMEABI_TOOLCHAIN_CONFIG: &str = r#"load(
    "@rules_cc//cc:cc_toolchain_config_lib.bzl",
    "feature",
    "tool_path",
)
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/toolchains:cc_toolchain_config_info.bzl", "CcToolchainConfigInfo")

def _impl(ctx):
    toolchain_identifier = "stub_armeabi-v7a"
    host_system_name = "armeabi-v7a"
    target_system_name = "armeabi-v7a"
    target_cpu = "armeabi-v7a"
    target_libc = "armeabi-v7a"
    compiler = "compiler"
    abi_version = "armeabi-v7a"
    abi_libc_version = "armeabi-v7a"
    cc_target_os = None
    builtin_sysroot = None
    action_configs = []

    supports_pic_feature = feature(name = "supports_pic", enabled = True)
    supports_dynamic_linker_feature = feature(name = "supports_dynamic_linker", enabled = True)
    features = [supports_dynamic_linker_feature, supports_pic_feature]

    cxx_builtin_include_directories = []
    artifact_name_patterns = []
    make_variables = []

    tool_paths = [
        tool_path(name = "ar", path = "/bin/false"),
        tool_path(name = "compat-ld", path = "/bin/false"),
        tool_path(name = "cpp", path = "/bin/false"),
        tool_path(name = "dwp", path = "/bin/false"),
        tool_path(name = "gcc", path = "/bin/false"),
        tool_path(name = "gcov", path = "/bin/false"),
        tool_path(name = "ld", path = "/bin/false"),
        tool_path(name = "llvm-profdata", path = "/bin/false"),
        tool_path(name = "nm", path = "/bin/false"),
        tool_path(name = "objcopy", path = "/bin/false"),
        tool_path(name = "objdump", path = "/bin/false"),
        tool_path(name = "strip", path = "/bin/false"),
    ]
    return cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        features = features,
        action_configs = action_configs,
        artifact_name_patterns = artifact_name_patterns,
        cxx_builtin_include_directories = cxx_builtin_include_directories,
        toolchain_identifier = toolchain_identifier,
        host_system_name = host_system_name,
        target_system_name = target_system_name,
        target_cpu = target_cpu,
        target_libc = target_libc,
        compiler = compiler,
        abi_version = abi_version,
        abi_libc_version = abi_libc_version,
        tool_paths = tool_paths,
        make_variables = make_variables,
        builtin_sysroot = builtin_sysroot,
        cc_target_os = cc_target_os,
    )

armeabi_cc_toolchain_config = rule(
    implementation = _impl,
    attrs = {},
    provides = [CcToolchainConfigInfo],
)
"#;

pub(super) fn write_shell_config_repo(
    dest: &AbsNormPath,
    _setup: &BzlmodShellConfigSetup,
) -> bz_error::Result<()> {
    write_generated_module_file(dest, "local_config_shell")?;
    let mut toolchains = Vec::new();
    for (os, default_shell_path) in [
        ("windows", "c:/msys64/usr/bin/bash.exe"),
        ("linux", "/bin/bash"),
        ("osx", "/bin/bash"),
        ("freebsd", "/usr/local/bin/bash"),
        ("openbsd", "/usr/local/bin/bash"),
    ] {
        let is_host = host_platform_os_constraint() == Some(os);
        let sh_path = if is_host {
            std::env::var("BAZEL_SH")
                .ok()
                .filter(|path| !path.trim().is_empty())
                .or_else(|| find_executable_on_path("bash"))
                .unwrap_or_else(|| default_shell_path.to_owned())
        } else {
            default_shell_path.to_owned()
        };
        if os == "windows" {
            toolchains.push(format!(
                r#"sh_toolchain(
    name = "{os}_sh",
    path = {sh_path:?},
    launcher = "@bazel_tools//tools/launcher",
    launcher_maker = "@bazel_tools//tools/launcher:launcher_maker",
)"#
            ));
        } else {
            toolchains.push(format!(
                r#"sh_toolchain(
    name = "{os}_sh",
    path = {sh_path:?},
)"#
            ));
        }
        toolchains.push(format!(
            r#"toolchain(
    name = "{os}_sh_toolchain",
    toolchain = ":{os}_sh",
    toolchain_type = "@rules_shell//shell:toolchain_type",
    target_compatible_with = [
        "@platforms//os:{os}",
    ],
)"#
        ));
    }
    let build = format!(
        "load(\"@rules_shell//shell/toolchains:sh_toolchain.bzl\", \"sh_toolchain\")\n\n{}\n",
        toolchains.join("\n\n")
    );
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    Ok(())
}

pub(super) fn write_python_hub_repo(
    dest: &AbsNormPath,
    _setup: &BzlmodPythonHubSetup,
) -> bz_error::Result<()> {
    write_generated_module_file(dest, "pythons_hub")?;
    let build = r#"package(default_visibility = ["//visibility:public"])

exports_files([
    "interpreters.bzl",
    "versions.bzl",
])
"#;
    let interpreters = r#"# Generated by Buck2 for an unpopulated rules_python hub.

INTERPRETER_LABELS = {}
"#;
    let versions = r#"# Generated by Buck2 for an unpopulated rules_python hub.

DEFAULT_PYTHON_VERSION = ""
MINOR_MAPPING = {}
PYTHON_VERSIONS = []
"#;
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("interpreters.bzl")?),
        interpreters,
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("versions.bzl")?),
        versions,
    )
    .categorize_internal()?;
    Ok(())
}

fn cc_toolchains_build_template(
    project_fs: &ProjectRoot,
    dest_rel: &ProjectRelativePath,
    parent_canonical_repo_name: &str,
) -> bz_error::Result<String> {
    let Some((external_cells_root, _)) = dest_rel
        .as_str()
        .split_once(BZLMOD_GENERATED_EXTERNAL_CELL_PATH_MARKER)
    else {
        return Err(BzlmodError::InvalidGeneratedRepoPath(dest_rel.to_string()).into());
    };
    read_bzlmod_module_file_text(
        project_fs,
        external_cells_root,
        parent_canonical_repo_name,
        "cc/private/toolchain/BUILD.toolchains.tpl",
    )
    .with_buck_error_context(|| {
        format!("Error reading rules_cc toolchains template from `{parent_canonical_repo_name}`")
    })
}

fn read_bzlmod_module_file_text(
    project_fs: &ProjectRoot,
    external_cells_root: &str,
    canonical_repo_name: &str,
    path: &str,
) -> bz_error::Result<String> {
    let materialized_path = ProjectRelativePathBuf::unchecked_new(format!(
        "{external_cells_root}/{BZLMOD_EXTERNAL_CELL_KIND}/{canonical_repo_name}/{path}",
    ));
    match fs_util::read_to_string(project_fs.resolve(&materialized_path)) {
        Ok(contents) => return Ok(contents),
        Err(error)
            if matches!(
                error.io_error_kind(),
                Some(ErrorKind::NotFound | ErrorKind::NotADirectory)
            ) => {}
        Err(error) => return Err(error.categorize_internal()),
    }

    let alias_path = bzlmod_repo_contents_cache_alias_path(canonical_repo_name);
    let cache_repo = fs_util::read_to_string(project_fs.resolve(&alias_path))
        .categorize_internal()
        .with_buck_error_context(|| {
            format!("Error reading bzlmod repo contents cache alias `{alias_path}`")
        })?;
    let cache_path = ProjectRelativePathBuf::unchecked_new(format!("{cache_repo}/{path}"));
    fs_util::read_to_string(project_fs.resolve(&cache_path))
        .categorize_internal()
        .with_buck_error_context(|| {
            format!("Error reading bzlmod cached module file `{cache_path}`")
        })
}

pub(super) fn host_cc_cpu_value() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin_arm64",
        ("macos", _) => "darwin_x86_64",
        ("freebsd", _) => "freebsd",
        ("openbsd", _) => "openbsd",
        ("windows", "aarch64") => "arm64_windows",
        ("windows", _) => "x64_windows",
        (_, "power" | "powerpc" | "powerpc64" | "powerpc64le") => "ppc",
        (_, "s390x") => "s390x",
        (_, "mips64") => "mips64",
        (_, "riscv64") => "riscv64",
        (_, "arm" | "armv7" | "armv7l") => "arm",
        (_, "aarch64") => "aarch64",
        (_, "x86_64") => "k8",
        (_, "x86" | "i386" | "i486" | "i586" | "i686" | "i786") => "piii",
        _ => "k8",
    }
}

pub(super) fn write_bazel_features_version_repo(
    dest: &AbsNormPath,
    setup: &BzlmodBazelFeaturesVersionSetup,
) -> bz_error::Result<()> {
    write_generated_module_file(dest, "bazel_features_version")?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("BUILD.bazel")?),
        "load(\"@bazel_skylib//:bzl_library.bzl\", \"bzl_library\")\n\nexports_files([\"version.bzl\"])\n\nbzl_library(\n    name = \"version\",\n    srcs = [\"version.bzl\"],\n    visibility = [\"//visibility:public\"],\n)\n",
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("version.bzl")?),
        format!("version = '{}'", setup.bazel_version.as_ref()),
    )
    .categorize_internal()?;
    Ok(())
}

pub(super) fn write_host_platform_repo(
    dest: &AbsNormPath,
    setup: &BzlmodHostPlatformSetup,
) -> bz_error::Result<()> {
    write_generated_module_file(dest, "host_platform")?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("BUILD.bazel")?),
        "# DO NOT EDIT: automatically generated BUILD file\nexports_files([\"constraints.bzl\"])\n",
    )
    .categorize_internal()?;

    let mut constraints = Vec::new();
    let cpu_constraint = match setup.cpu_constraint.as_deref() {
        Some(cpu_constraint) => Some(cpu_constraint),
        None => host_platform_cpu_constraint(),
    };
    if let Some(cpu) = cpu_constraint {
        constraints.push(format!("    '@platforms//cpu:{cpu}',"));
    }
    let os_constraint = match setup.os_constraint.as_deref() {
        Some(os_constraint) => Some(os_constraint),
        None => host_platform_os_constraint(),
    };
    if let Some(os) = os_constraint {
        constraints.push(format!("    '@platforms//os:{os}',"));
    }
    let constraints = if constraints.is_empty() {
        String::new()
    } else {
        format!("\n{}\n", constraints.join("\n"))
    };
    fs_util::write(
        dest.join(ForwardRelativePath::new("constraints.bzl")?),
        format!(
            "# DO NOT EDIT: automatically generated constraints list\nHOST_CONSTRAINTS = [{}]\n",
            constraints
        ),
    )
    .categorize_internal()?;
    Ok(())
}

pub(super) fn host_platform_cpu_constraint() -> Option<&'static str> {
    translate_host_platform_cpu_constraint(std::env::consts::ARCH)
}

pub(super) fn translate_host_platform_cpu_constraint(arch: &str) -> Option<&'static str> {
    match arch {
        "i386" | "i486" | "i586" | "i686" | "i786" | "x86" => Some("x86_32"),
        "amd64" | "x86_64" | "x64" => Some("x86_64"),
        "ppc" | "powerpc" | "powerpc64" => Some("ppc"),
        "ppc64le" | "powerpc64le" => Some("ppc64le"),
        "arm" | "armv7" | "armv7l" => Some("arm"),
        "aarch64" => Some("aarch64"),
        "s390x" | "s390" => Some("s390x"),
        "mips64el" | "mips64" => Some("mips64"),
        "riscv64" => Some("riscv64"),
        _ => None,
    }
}

pub(super) fn host_platform_os_constraint() -> Option<&'static str> {
    translate_host_platform_os_constraint(std::env::consts::OS)
}

pub(super) fn translate_host_platform_os_constraint(os: &str) -> Option<&'static str> {
    let os = os.to_ascii_lowercase();
    if os.starts_with("mac os") || matches!(os.as_str(), "macos" | "osx" | "darwin") {
        return Some("osx");
    }
    if os.starts_with("freebsd") {
        return Some("freebsd");
    }
    if os.starts_with("openbsd") {
        return Some("openbsd");
    }
    if os.starts_with("linux") {
        return Some("linux");
    }
    if os.starts_with("windows") {
        return Some("windows");
    }
    None
}

pub(super) fn write_bazel_features_globals_repo(
    project_fs: &ProjectRoot,
    dest_rel: &ProjectRelativePath,
    dest: &AbsNormPath,
    setup: &BzlmodBazelFeaturesGlobalsSetup,
) -> bz_error::Result<()> {
    let Some((external_cells_root, _)) = dest_rel
        .as_str()
        .split_once(BZLMOD_GENERATED_EXTERNAL_CELL_PATH_MARKER)
    else {
        return Err(BzlmodError::InvalidGeneratedRepoPath(dest_rel.to_string()).into());
    };
    let globals_path = ProjectRelativePathBuf::unchecked_new(format!(
        "{external_cells_root}/{BZLMOD_EXTERNAL_CELL_KIND}/{}/private/globals.bzl",
        setup.parent_canonical_repo_name
    ));
    let globals_text = read_bzlmod_module_file_text(
        project_fs,
        external_cells_root,
        &setup.parent_canonical_repo_name,
        "private/globals.bzl",
    )
    .with_buck_error_context(|| {
        format!("Error reading bazel_features globals `{}`", globals_path)
    })?;
    let globals = parse_bazel_features_globals_dict(&globals_text, &globals_path)?;

    write_generated_module_file(dest, "bazel_features_globals")?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("BUILD.bazel")?),
        "load(\"@bazel_skylib//:bzl_library.bzl\", \"bzl_library\")\n\nexports_files([\"globals.bzl\"])\n\nbzl_library(\n    name = \"globals\",\n    srcs = [\"globals.bzl\"],\n    visibility = [\"//visibility:public\"],\n)\n",
    )
    .categorize_internal()?;

    let mut globals_bzl = String::from("globals = struct(\n");
    for (global, versions) in globals {
        let value = if bazel_feature_global_is_available(
            &setup.bazel_version,
            &versions.min_version,
            &versions.max_version,
        ) {
            global.as_str()
        } else {
            "None"
        };
        globals_bzl.push_str(&format!(
            "    {global} = getattr(getattr(native, 'legacy_globals', None), '{global}', {value}),\n"
        ));
    }
    globals_bzl.push(')');
    fs_util::write(
        dest.join(ForwardRelativePath::new("globals.bzl")?),
        globals_bzl,
    )
    .categorize_internal()?;
    Ok(())
}

struct BazelFeatureGlobalVersions {
    min_version: String,
    max_version: String,
}

fn parse_bazel_features_globals_dict(
    text: &str,
    path: &ProjectRelativePath,
) -> bz_error::Result<Vec<(String, BazelFeatureGlobalVersions)>> {
    let mut values = Vec::new();
    let mut in_dict = false;

    for line in text.lines() {
        let line = strip_starlark_line_comment(line).trim();
        if !in_dict {
            if line == "GLOBALS = {" {
                in_dict = true;
            }
            continue;
        }
        if line.starts_with('}') {
            return Ok(values);
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(key) = parse_simple_bzl_string(key.trim()) else {
            continue;
        };
        let value = value.trim().trim_end_matches(',');
        let Some((min_version, max_version)) = parse_bazel_features_version_pair(value) else {
            continue;
        };
        values.push((
            key,
            BazelFeatureGlobalVersions {
                min_version,
                max_version,
            },
        ));
    }

    Err(BzlmodError::MissingBazelFeaturesGlobalsDict {
        path: path.to_string(),
        dict: "GLOBALS",
    }
    .into())
}

fn strip_starlark_line_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;

    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            continue;
        }
        if ch == '#' {
            return &line[..idx];
        }
    }

    line
}

fn parse_simple_bzl_string(value: &str) -> Option<String> {
    let quote = value.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = value.strip_prefix(quote)?.strip_suffix(quote)?;
    Some(value.to_owned())
}

pub(super) fn parse_bazel_features_version_pair(value: &str) -> Option<(String, String)> {
    if let Some(min_version) = parse_simple_bzl_string(value) {
        return Some((min_version, String::new()));
    }

    let value = value
        .strip_prefix('(')
        .and_then(|value| value.strip_suffix(')'))
        .or_else(|| {
            value
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
        })?;
    let mut parts = value.split(',');
    let min_version = parse_simple_bzl_string(parts.next()?.trim())?;
    let max_version = parse_simple_bzl_string(parts.next()?.trim())?;
    if parts.next().is_some() {
        return None;
    }
    Some((min_version, max_version))
}
