/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! This modules contains common options that are shared between different commands.
//! They are shared by composition together with flattening of the options.
//!
//! For example, to adopt config options, add the following field to the
//! command definition:
//!
//! ```ignore
//! #[derive(Debug, clap::Parser)]
//! struct MyCommand {
//!    #[clap(flatten)]
//!    config_opts: CommonConfigOptions,
//!    ...
//! }
//! ```

pub mod build;
pub mod profiling;
pub mod target_cfg;
pub mod timeout;
pub mod ui;

use std::path::Path;

use bz_cli_proto::ConfigOverride;
use bz_cli_proto::RepresentativeConfigFlag;
use bz_cli_proto::config_override::ConfigType;
use bz_cli_proto::representative_config_flag::Source as RepresentativeConfigFlagSource;
use bz_common::argv::ExpandedArgSource;
use bz_common::argv::ExpandedArgv;
use bz_fs::paths::abs_path::AbsPath;
use bz_fs::working_dir::AbsWorkingDir;
use dupe::Dupe;
use gazebo::prelude::*;

use crate::common::profiling::BuckProfileMode;
use crate::common::ui::CommonConsoleOptions;
use crate::immediate_config::ImmediateConfigContext;
use crate::path_arg::PathArg;

pub const EVENT_LOG: &str = "event-log";
pub const NO_EVENT_LOG: &str = "no-event-log";
const BUILDBUDDY_BES_BACKEND: &str = "remote.buildbuddy.io";
const BUILDBUDDY_BES_RESULTS_URL: &str = "https://app.buildbuddy.io/invocation/";
const BUILDBUDDY_BES_BACKEND_DEV: &str = "remote.buildbuddy.dev";
const BUILDBUDDY_BES_RESULTS_URL_DEV: &str = "https://app.buildbuddy.dev/invocation/";
const BAZEL_JAVA_LANGUAGE_VERSION: &str = "//command_line_option:java_language_version";
const BAZEL_JAVA_RUNTIME_VERSION: &str = "//command_line_option:java_runtime_version";
const BAZEL_TOOL_JAVA_LANGUAGE_VERSION: &str = "//command_line_option:tool_java_language_version";
const BAZEL_TOOL_JAVA_RUNTIME_VERSION: &str = "//command_line_option:tool_java_runtime_version";
const BAZEL_ACTION_ENV: &str = "//command_line_option:action_env";
const BAZEL_HOST_ACTION_ENV: &str = "//command_line_option:host_action_env";
const BAZEL_EXTRA_BZLMOD_DEPS: &str = "bazel.extra_bzlmod_deps";
const LLVM_BZLMOD_DEP: &str = "llvm@0.8.5";
const LLVM_BAZEL_FEATURES_BZLMOD_DEP: &str = "bazel_features@1.47.0";
const LLVM_TOOLCHAIN_PATTERN: &str = "@llvm//toolchain:all";
const LLVM_LINUX_X86_64_PLATFORM: &str = "@llvm//platforms:linux_x86_64";
const LLVM_MACOS_AARCH64_PLATFORM: &str = "@llvm//platforms:macos_aarch64";
const LLVM_STUB_LIBGCC_S_SETTING: &str = "@llvm//config:experimental_stub_libgcc_s";
const LLVM_EMPTY_SYSROOT_SETTING: &str = "@llvm//config:empty_sysroot";
const RULES_CC_USE_LIBTOOL_ON_MACOS_SETTING: &str =
    "@@rules_cc+//cc/toolchains/args/archiver_flags:use_libtool_on_macos";
const RULES_RUST_SH_BOOTSTRAP_PROCESS_WRAPPER_SETTING: &str =
    "@@rules_rust+//rust/settings:experimental_use_sh_toolchain_for_bootstrap_process_wrapper";
const LLVM_LINUX_LINKOPT: &str = "-no-pie";
const LLVM_CARGO_CC_RS_LINKER_FLAG: &str = "-fuse-ld=lld";
const LLVM_MACOS_AARCH64_CFLAGS_ENV: &str = "CFLAGS_aarch64_apple_darwin";
const LLVM_MACOS_AARCH64_CXXFLAGS_ENV: &str = "CXXFLAGS_aarch64_apple_darwin";
const LLVM_MACOS_FRAMEWORKS: &str = "CoreFoundation,Foundation,Kernel,OSLog,Security,SystemConfiguration,IOKit,CoreServices,DiskArbitration,CFNetwork";

#[derive(Debug, bz_error::Error)]
#[error("indices len is not equal to collection len for flag `{flag_name}`")]
#[buck2(tag = bz_error::ErrorTag::InternalError)]
struct IndicesLengthMismatchError {
    flag_name: String,
}

#[derive(
    Debug,
    serde::Serialize,
    serde::Deserialize,
    Clone,
    Dupe,
    Copy,
    PartialEq,
    Eq,
    clap::ValueEnum
)]
#[clap(rename_all = "lower")]
pub enum HostPlatformOverride {
    Default,
    Linux,
    MacOs,
    Windows,
}

#[derive(
    Debug,
    serde::Serialize,
    serde::Deserialize,
    Clone,
    Dupe,
    Copy,
    clap::ValueEnum,
    Default
)]
#[clap(rename_all = "lower")]
pub enum PreemptibleWhen {
    /// (default) When another command starts that cannot run in parallel with this one, block that command.
    #[default]
    Never, // Read; "If I am Never, then never preempt me" (the default)
    /// When another command starts, interrupt this command, *even if they could run in
    /// parallel*. There is no good reason to use this other than that it provides slightly nicer
    /// superconsole output.
    Always,
    /// When another command starts that cannot run in parallel with this one,
    /// interrupt this command.
    OnDifferentState, // Read; "if a command comes in, preempt me on different state"
}

#[derive(
    Debug,
    serde::Serialize,
    serde::Deserialize,
    Clone,
    Dupe,
    Copy,
    clap::ValueEnum,
    Default
)]
#[clap(rename_all = "lower")]
pub enum ExitWhen {
    /// (default) Execute this command normally.
    #[default]
    Never,
    /// Fail this command if another command is already running with a different state.
    DifferentState,
    /// Fail this command if another command is already running (regardless of daemon state).
    NotIdle,
}

#[derive(
    Debug,
    serde::Serialize,
    serde::Deserialize,
    Clone,
    Dupe,
    Copy,
    PartialEq,
    Eq,
    clap::ValueEnum
)]
#[clap(rename_all = "lower")]
pub enum HostArchOverride {
    Default,
    AArch64,
    X86_64,
}

#[derive(Debug, Clone, Dupe, Copy)]
struct PlatformSpec {
    platform: HostPlatformOverride,
    arch: HostArchOverride,
}

impl PlatformSpec {
    fn new(platform: HostPlatformOverride, arch: HostArchOverride) -> Self {
        Self { platform, arch }
    }
}

/// Defines options related to commands that involves a streaming daemon command.
#[derive(Debug, clap::Parser, serde::Serialize, serde::Deserialize, Default)]
#[clap(next_help_heading = "Event Log Options")]
pub struct CommonEventLogOptions {
    /// Write events to this log file
    #[clap(value_name = "PATH", long = EVENT_LOG)]
    pub event_log: Option<PathArg>,

    /// Do not write any event logs. Overrides --event-log. Used from `replay` to avoid recursive logging
    #[clap(long = NO_EVENT_LOG, hide = true)]
    pub no_event_log: bool,

    /// Write command invocation id into this file.
    #[clap(long, value_name = "PATH")]
    pub(crate) write_build_id: Option<PathArg>,

    /// Write the invocation record (as JSON) to this path. No guarantees whatsoever are made
    /// regarding the stability of the format.
    #[clap(long, value_name = "PATH")]
    pub(crate) unstable_write_invocation_record: Option<PathArg>,

    /// Write the command report to this path. A command report is always
    /// written to `buck-out/v2/<uuid>/command_report` even without this flag.
    #[clap(long, value_name = "PATH")]
    pub(crate) command_report_path: Option<PathArg>,

    /// Upload Build Event Protocol events to BuildBuddy.
    #[serde(default)]
    #[clap(long = "bep", alias = "bes", hide = true)]
    pub(crate) bep: bool,

    /// Bazel-compatible Build Event Service endpoint, e.g. `grpc://localhost:1985`
    /// or `grpcs://remote.buildbuddy.io`.
    #[serde(default)]
    #[clap(
        long = "bes_backend",
        alias = "bes-backend",
        value_name = "ENDPOINT",
        hide = true
    )]
    pub(crate) bes_backend: Option<String>,

    /// Header in `NAME=VALUE` form to include in BES requests. May be repeated.
    #[serde(default)]
    #[clap(
        long = "bes_header",
        alias = "bes-header",
        value_name = "NAME=VALUE",
        hide = true
    )]
    pub(crate) bes_header: Vec<String>,

    /// Build Event Service instance/project name.
    #[serde(default)]
    #[clap(
        long = "bes_instance_name",
        alias = "bes-instance-name",
        value_name = "INSTANCE",
        hide = true
    )]
    pub(crate) bes_instance_name: Option<String>,

    /// Additional BES notification keywords. Comma-separated values are accepted.
    #[serde(default)]
    #[clap(
        long = "bes_keywords",
        alias = "bes-keywords",
        value_name = "KEYWORDS",
        hide = true
    )]
    pub(crate) bes_keywords: Vec<String>,

    /// How long to wait for BES upload completion during finalization.
    #[serde(default)]
    #[clap(
        long = "bes_timeout",
        alias = "bes-timeout",
        value_name = "DURATION",
        hide = true
    )]
    pub(crate) bes_timeout: Option<String>,

    /// Base URL for viewing BES results; the invocation id is appended.
    #[serde(default)]
    #[clap(
        long = "bes_results_url",
        alias = "bes-results-url",
        value_name = "URL",
        hide = true
    )]
    pub(crate) bes_results_url: Option<String>,

    /// Wait for daemon-side BES upload completion before returning the command result.
    #[serde(default)]
    #[clap(long = "bes_sync", alias = "bes-sync", hide = true)]
    pub(crate) bes_sync: bool,
}

impl CommonEventLogOptions {
    pub fn bes_backend(&self) -> Option<&str> {
        self.bes_backend_with_buildbuddy_default(false, false)
    }

    pub fn bes_backend_with_buildbuddy_default(&self, buildbuddy: bool, dev: bool) -> Option<&str> {
        self.bes_backend.as_deref().or_else(|| {
            (self.bep || buildbuddy).then_some(if dev {
                BUILDBUDDY_BES_BACKEND_DEV
            } else {
                BUILDBUDDY_BES_BACKEND
            })
        })
    }

    pub fn bes_results_url(&self) -> Option<&str> {
        self.bes_results_url_with_buildbuddy_default(false, false)
    }

    pub fn bes_results_url_with_buildbuddy_default(
        &self,
        buildbuddy: bool,
        dev: bool,
    ) -> Option<&str> {
        self.bes_results_url.as_deref().or_else(|| {
            (self.bep || buildbuddy).then_some(if dev {
                BUILDBUDDY_BES_RESULTS_URL_DEV
            } else {
                BUILDBUDDY_BES_RESULTS_URL
            })
        })
    }

    pub(crate) fn bes_timeout_duration(&self) -> bz_error::Result<Option<std::time::Duration>> {
        self.bes_timeout
            .as_deref()
            .map(|timeout| {
                humantime::parse_duration(timeout).map_err(|error| {
                    bz_error::bz_error!(
                        bz_error::ErrorTag::Input,
                        "invalid --bes_timeout `{}`: {}",
                        timeout,
                        error
                    )
                })
            })
            .transpose()
    }

    pub fn default_ref() -> &'static Self {
        static DEFAULT: CommonEventLogOptions = CommonEventLogOptions {
            event_log: None,
            no_event_log: false,
            write_build_id: None,
            command_report_path: None,
            unstable_write_invocation_record: None,
            bep: false,
            bes_backend: None,
            bes_header: Vec::new(),
            bes_instance_name: None,
            bes_keywords: Vec::new(),
            bes_timeout: None,
            bes_results_url: None,
            bes_sync: false,
        };
        &DEFAULT
    }

    pub fn no_event_log_ref() -> &'static Self {
        static NO_EVENT_LOG: CommonEventLogOptions = CommonEventLogOptions {
            event_log: None,
            no_event_log: true,
            write_build_id: None,
            command_report_path: None,
            unstable_write_invocation_record: None,
            bep: false,
            bes_backend: None,
            bes_header: Vec::new(),
            bes_instance_name: None,
            bes_keywords: Vec::new(),
            bes_timeout: None,
            bes_results_url: None,
            bes_sync: false,
        };
        &NO_EVENT_LOG
    }
}

/// Defines options for config and configuration related things. Any command that involves the build
/// graph should include these options.
#[derive(Debug, clap::Parser, serde::Serialize, serde::Deserialize, Default)]
#[clap(next_help_heading = "Buckconfig Options")]
pub struct CommonBuildConfigurationOptions {
    #[clap(
        value_name = "SECTION.OPTION=VALUE",
        long = "config",
        short = 'c',
        help = "List of config options, or Bazel compilation mode values: fastbuild, dbg, opt",
        // Needs to be explicitly set, otherwise will treat `-c a b c` -> [a, b, c]
        // rather than [a] and other positional arguments `b c`.
        num_args = 1
    )]
    pub config_values: Vec<String>,

    #[clap(
        value_name = "PATH",
        long = "config-file",
        help = "List of config file paths",
        num_args = 1
    )]
    pub config_files: Vec<String>,

    #[clap(
        long,
        alias = "fake-os",
        alias = "os",
        ignore_case = true,
        value_name = "HOST",
        value_enum
    )]
    pub fake_host: Option<HostPlatformOverride>,

    #[clap(
        long,
        alias = "arch",
        ignore_case = true,
        value_name = "ARCH",
        value_enum
    )]
    pub fake_arch: Option<HostArchOverride>,

    /// Alias for `--os=linux --arch=x8664`.
    #[clap(
        long,
        conflicts_with_all = ["linux_arm", "mac", "mac_intel"]
    )]
    pub linux: bool,

    /// Alias for `--os=linux --arch=aarch64`.
    #[clap(
        long = "linux_arm",
        alias = "linux-arm",
        conflicts_with_all = ["linux", "mac", "mac_intel"]
    )]
    pub linux_arm: bool,

    /// Alias for `--os=macos --arch=aarch64`.
    #[clap(
        long,
        conflicts_with_all = ["linux", "linux_arm", "mac_intel"]
    )]
    pub mac: bool,

    /// Alias for `--os=macos --arch=x8664`.
    #[clap(
        long = "mac_intel",
        alias = "mac-intel",
        conflicts_with_all = ["linux", "linux_arm", "mac"]
    )]
    pub mac_intel: bool,

    /// Use hermeticbuild/hermetic-llvm's Bazel C/C++ toolchain.
    #[clap(long)]
    pub llvm: bool,

    /// Alias for Linux x86_64 host/exec plus macOS aarch64 target using hermetic LLVM.
    #[clap(
        long,
        alias = "linux-to-mac",
        conflicts_with_all = ["linux", "linux_arm", "mac", "mac_intel", "mac2linux"]
    )]
    pub linux2mac: bool,

    /// Alias for macOS aarch64 host/exec plus Linux x86_64 target using hermetic LLVM.
    #[clap(
        long,
        alias = "mac-to-linux",
        conflicts_with_all = ["linux", "linux_arm", "mac", "mac_intel", "linux2mac"]
    )]
    pub mac2linux: bool,

    /// Bazel-compatible target CPU setting. This populates
    /// `//command_line_option:cpu` for Bazel rules.
    #[clap(long = "cpu", value_name = "CPU", num_args = 1)]
    pub bazel_cpu: Vec<String>,

    /// Bazel-compatible host CPU setting. This populates
    /// `//command_line_option:host_cpu` for Bazel rules.
    #[clap(
        long = "host_cpu",
        alias = "host-cpu",
        value_name = "CPU",
        num_args = 1
    )]
    pub bazel_host_cpu: Vec<String>,

    /// Bazel-compatible target platform setting. This accepts Bazel's
    /// comma-separated label list and populates `//command_line_option:platforms`.
    #[clap(long = "platforms", value_name = "PLATFORMS", num_args = 1)]
    pub bazel_platforms: Vec<String>,

    /// Bazel-compatible extra execution platform setting. This accepts Bazel's
    /// comma-separated label list and populates
    /// `//command_line_option:extra_execution_platforms`.
    #[clap(
        long = "extra_execution_platforms",
        alias = "extra-execution-platforms",
        value_name = "PLATFORMS",
        num_args = 1
    )]
    pub bazel_extra_execution_platforms: Vec<String>,

    /// Bazel-compatible extra toolchain setting. This accepts Bazel's
    /// comma-separated target pattern list and populates
    /// `//command_line_option:extra_toolchains`.
    #[clap(
        long = "extra_toolchains",
        alias = "extra-toolchains",
        value_name = "TOOLCHAINS",
        num_args = 1
    )]
    pub bazel_extra_toolchains: Vec<String>,

    /// Bazel-compatible compilation mode setting. This populates
    /// `//command_line_option:compilation_mode`.
    #[clap(
        long = "compilation_mode",
        alias = "compilation-mode",
        value_name = "MODE",
        num_args = 1
    )]
    pub bazel_compilation_mode: Vec<String>,

    /// Bazel-compatible C/C++ link option. This populates
    /// `//command_line_option:linkopt`.
    #[clap(
        long = "linkopt",
        value_name = "LINKOPT",
        num_args = 1,
        allow_hyphen_values = true
    )]
    pub bazel_linkopt: Vec<String>,

    /// Bazel-compatible host C/C++ link option. This populates
    /// `//command_line_option:host_linkopt`.
    #[clap(
        long = "host_linkopt",
        alias = "host-linkopt",
        value_name = "LINKOPT",
        num_args = 1,
        allow_hyphen_values = true
    )]
    pub bazel_host_linkopt: Vec<String>,

    /// Bazel-compatible action environment setting. Values are formatted as
    /// `NAME`, `NAME=VALUE`, or `=NAME`.
    #[clap(
        long = "action_env",
        alias = "action-env",
        value_name = "ENV",
        num_args = 1
    )]
    pub bazel_action_env: Vec<String>,

    /// Bazel-compatible host/exec action environment setting. Values are
    /// formatted as `NAME`, `NAME=VALUE`, or `=NAME`.
    #[clap(
        long = "host_action_env",
        alias = "host-action-env",
        value_name = "ENV",
        num_args = 1
    )]
    pub bazel_host_action_env: Vec<String>,

    /// Bazel-compatible Java language setting. This populates
    /// `//command_line_option:java_language_version`.
    #[clap(
        long = "java_language_version",
        alias = "java-language-version",
        value_name = "VERSION",
        num_args = 1
    )]
    pub bazel_java_language_version: Vec<String>,

    /// Bazel-compatible Java runtime setting. This populates
    /// `//command_line_option:java_runtime_version`.
    #[clap(
        long = "java_runtime_version",
        alias = "java-runtime-version",
        value_name = "VERSION",
        num_args = 1
    )]
    pub bazel_java_runtime_version: Vec<String>,

    /// Bazel-compatible tool Java language setting. This populates
    /// `//command_line_option:tool_java_language_version`.
    #[clap(
        long = "tool_java_language_version",
        alias = "tool-java-language-version",
        value_name = "VERSION",
        num_args = 1
    )]
    pub bazel_tool_java_language_version: Vec<String>,

    /// Bazel-compatible tool Java runtime setting. This populates
    /// `//command_line_option:tool_java_runtime_version`.
    #[clap(
        long = "tool_java_runtime_version",
        alias = "tool-java-runtime-version",
        value_name = "VERSION",
        num_args = 1
    )]
    pub bazel_tool_java_runtime_version: Vec<String>,

    /// Value must be formatted as: version-build (e.g., 14.3.0-14C18 or 14.1-14B47b)
    #[clap(long, value_name = "VERSION-BUILD")]
    pub fake_xcode_version: Option<String>,

    /// Re-uses any `--config` values (inline or via modefiles) if there's
    /// a previous command, otherwise the flag is ignored.
    ///
    /// If there is a previous command and `--reuse-current-config` is set,
    /// then the old config is used, ignoring any overrides.
    ///
    /// If there is no previous command but the flag was set, then the flag is ignored,
    /// the command behaves as if the flag was not set at all.
    #[clap(long)]
    pub reuse_current_config: bool,

    /// Used to configure when this command could be preempted by another command for the same isolation dir.
    ///
    /// Normally, when you run two commands - from different terminals, say - bz will attempt
    /// to run them in parallel. However, if the two commands are based on different state, that
    /// is they either have different configs or different filesystem states, bz cannot run them
    /// in parallel. The default behavior in this case is to block the second command until the
    /// first completes.
    #[clap(long, ignore_case = true, value_enum)]
    pub preemptible: Option<PreemptibleWhen>,
    /// Whether to proceed with or fail this invocation based on the daemon state.
    #[clap(long, ignore_case = true, value_enum)]
    pub exit_when: Option<ExitWhen>,
}

impl CommonBuildConfigurationOptions {
    fn bazel_command_line_build_setting_entry(kind: &str, key: &str, value: &str) -> String {
        format!("{kind}\t{key}\t{value}")
    }

    fn bazel_command_line_build_settings_override(settings: Vec<String>) -> ConfigOverride {
        ConfigOverride {
            cell: None,
            config_override: format!("bazel.command_line_build_settings={}", settings.join("\n")),
            config_type: ConfigType::Value as i32,
        }
    }

    fn bazel_host_platform_constraints_override(
        platform: HostPlatformOverride,
        arch: HostArchOverride,
    ) -> ConfigOverride {
        let os_constraint = match platform {
            HostPlatformOverride::Default => match std::env::consts::OS {
                "macos" => "osx",
                "linux" => "linux",
                "windows" => "windows",
                os => os,
            },
            HostPlatformOverride::Linux => "linux",
            HostPlatformOverride::MacOs => "osx",
            HostPlatformOverride::Windows => "windows",
        };
        let cpu_constraint = match arch {
            HostArchOverride::Default => std::env::consts::ARCH,
            HostArchOverride::AArch64 => "aarch64",
            HostArchOverride::X86_64 => "x86_64",
        };
        ConfigOverride {
            cell: None,
            config_override: format!(
                "bazel.host_platform_constraints=@platforms//cpu:{cpu_constraint}\n@platforms//os:{os_constraint}"
            ),
            config_type: ConfigType::Value as i32,
        }
    }

    fn bazel_command_line_string_build_setting(key: &str, value: &str) -> String {
        Self::bazel_command_line_build_setting_entry("string", key, value)
    }

    fn bazel_command_line_bool_build_setting(key: &str, value: bool) -> String {
        Self::bazel_command_line_build_setting_entry(
            "bool",
            key,
            if value { "true" } else { "false" },
        )
    }

    fn bazel_command_line_list_build_setting(key: &str, value: &str) -> Vec<String> {
        value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| Self::bazel_command_line_build_setting_entry("list", key, value))
            .collect()
    }

    fn bazel_command_line_single_list_build_setting(key: &str, value: &str) -> String {
        Self::bazel_command_line_build_setting_entry("list", key, value)
    }

    fn bazel_compilation_mode(value: &str) -> Option<&str> {
        match value {
            "fastbuild" | "dbg" | "opt" => Some(value),
            _ => None,
        }
    }

    fn bazel_cpu_from_platform(
        platform: HostPlatformOverride,
        arch: HostArchOverride,
    ) -> &'static str {
        let os = match platform {
            HostPlatformOverride::Default => std::env::consts::OS,
            HostPlatformOverride::Linux => "linux",
            HostPlatformOverride::MacOs => "macos",
            HostPlatformOverride::Windows => "windows",
        };
        let arch = match arch {
            HostArchOverride::Default => std::env::consts::ARCH,
            HostArchOverride::AArch64 => "aarch64",
            HostArchOverride::X86_64 => "x86_64",
        };

        // Match Bazel's AutoCpuConverter legacy names. Bazel intentionally
        // auto-detects only for the local machine; these aliases are explicit
        // cross-machine settings, so they must populate cpu/host_cpu.
        match (os, arch) {
            ("macos", "x86_64") => "darwin_x86_64",
            ("macos", "aarch64") => "darwin_arm64",
            ("freebsd", _) => "freebsd",
            ("openbsd", _) => "openbsd",
            ("windows", "x86_64") => "x64_windows",
            ("windows", "aarch64") => "arm64_windows",
            ("linux", "x86" | "i386" | "i486" | "i586" | "i686" | "i786") => "piii",
            ("linux", "x86_64") => "k8",
            ("linux", "power" | "powerpc" | "powerpc64" | "powerpc64le") => "ppc",
            ("linux", "arm" | "armv7" | "armv7l") => "arm",
            ("linux", "aarch64") => "aarch64",
            ("linux", "s390x") => "s390x",
            ("linux", "mips64") => "mips64",
            ("linux", "riscv64") => "riscv64",
            _ => "unknown",
        }
    }

    fn flag_last_index(matches: BuckArgMatches<'_>, name: &str) -> Option<usize> {
        if !matches.inner.ids().any(|id| id.as_str() == name) {
            return None;
        }
        matches
            .inner
            .indices_of(name)
            .and_then(|indices| indices.into_iter().last())
    }

    fn bazel_cpu_override_index(&self, matches: BuckArgMatches<'_>) -> Option<usize> {
        let names = [
            (self.fake_host.is_some(), "fake_host"),
            (self.fake_arch.is_some(), "fake_arch"),
            (self.linux, "linux"),
            (self.linux_arm, "linux_arm"),
            (self.mac, "mac"),
            (self.mac_intel, "mac_intel"),
            (self.linux2mac, "linux2mac"),
            (self.mac2linux, "mac2linux"),
        ];
        names
            .iter()
            .filter_map(|(enabled, name)| enabled.then(|| Self::flag_last_index(matches, name))?)
            .max()
    }

    fn bazel_cpu_override_settings(
        &self,
        matches: BuckArgMatches<'_>,
    ) -> Option<(usize, Vec<String>)> {
        let index = self.bazel_cpu_override_index(matches)?;
        let host = self.host_platform_spec();
        let target = self.target_platform_spec();
        let cpu = Self::bazel_cpu_from_platform(target.platform, target.arch);
        let host_cpu = Self::bazel_cpu_from_platform(host.platform, host.arch);
        Some((
            index,
            vec![
                Self::bazel_command_line_string_build_setting("//command_line_option:cpu", cpu),
                Self::bazel_command_line_string_build_setting(
                    "//command_line_option:host_cpu",
                    host_cpu,
                ),
            ],
        ))
    }

    fn bazel_host_platform_constraints(
        &self,
        matches: BuckArgMatches<'_>,
    ) -> Option<(usize, ConfigOverride)> {
        let index = self.bazel_cpu_override_index(matches)?;
        Some((
            index,
            Self::bazel_host_platform_constraints_override(
                self.host_platform_spec().platform,
                self.host_platform_spec().arch,
            ),
        ))
    }

    fn llvm_toolchain_override_settings(
        &self,
        matches: BuckArgMatches<'_>,
        rules_rust_available: bool,
    ) -> Option<(usize, Vec<String>)> {
        if !(self.llvm || self.linux2mac || self.mac2linux) {
            return None;
        }

        let index = ["llvm", "linux2mac", "mac2linux"]
            .iter()
            .filter_map(|name| Self::flag_last_index(matches, name))
            .max()?;

        let mut settings = vec![
            Self::bazel_command_line_single_list_build_setting(
                "//command_line_option:extra_toolchains",
                LLVM_TOOLCHAIN_PATTERN,
            ),
            Self::bazel_command_line_bool_build_setting(LLVM_STUB_LIBGCC_S_SETTING, true),
            // Hermetic LLVM documents this setting for Rust interop where
            // unmanaged build scripts link against host system libraries.
            Self::bazel_command_line_bool_build_setting(LLVM_EMPTY_SYSROOT_SETTING, false),
            Self::bazel_command_line_bool_build_setting(
                RULES_CC_USE_LIBTOOL_ON_MACOS_SETTING,
                false,
            ),
        ];
        if rules_rust_available {
            settings.push(Self::bazel_command_line_bool_build_setting(
                RULES_RUST_SH_BOOTSTRAP_PROCESS_WRAPPER_SETTING,
                true,
            ));
        }

        Some((index, settings))
    }

    fn llvm_bzlmod_dep_override(
        &self,
        matches: BuckArgMatches<'_>,
    ) -> Option<(usize, ConfigOverride)> {
        if !(self.llvm || self.linux2mac || self.mac2linux) {
            return None;
        }

        let index = ["llvm", "linux2mac", "mac2linux"]
            .iter()
            .filter_map(|name| Self::flag_last_index(matches, name))
            .max()?;

        Some((
            index,
            ConfigOverride {
                cell: None,
                config_override: format!(
                    "{BAZEL_EXTRA_BZLMOD_DEPS}={LLVM_BZLMOD_DEP},{LLVM_BAZEL_FEATURES_BZLMOD_DEP}"
                ),
                config_type: ConfigType::Value as i32,
            },
        ))
    }

    fn llvm_macos_repo_env_override(
        &self,
        matches: BuckArgMatches<'_>,
    ) -> Option<(usize, ConfigOverride)> {
        if !self.linux2mac {
            return None;
        }

        let index = Self::flag_last_index(matches, "linux2mac")?;

        Some((
            index,
            ConfigOverride {
                cell: None,
                config_override: format!(
                    "bazel.repo_env=BAZEL_MACOS_FRAMEWORKS={LLVM_MACOS_FRAMEWORKS}"
                ),
                config_type: ConfigType::Value as i32,
            },
        ))
    }

    fn llvm_cargo_cc_rs_action_env_settings(
        &self,
        matches: BuckArgMatches<'_>,
    ) -> Option<(usize, Vec<String>)> {
        if !self.linux2mac {
            return None;
        }

        let index = Self::flag_last_index(matches, "linux2mac")?;

        Some((
            index,
            vec![
                Self::bazel_command_line_single_list_build_setting(
                    BAZEL_ACTION_ENV,
                    &format!("{LLVM_MACOS_AARCH64_CFLAGS_ENV}={LLVM_CARGO_CC_RS_LINKER_FLAG}"),
                ),
                Self::bazel_command_line_single_list_build_setting(
                    BAZEL_ACTION_ENV,
                    &format!("{LLVM_MACOS_AARCH64_CXXFLAGS_ENV}={LLVM_CARGO_CC_RS_LINKER_FLAG}"),
                ),
            ],
        ))
    }

    fn target_platform_is_linux(&self) -> bool {
        match self.target_platform_spec().platform {
            HostPlatformOverride::Linux => true,
            HostPlatformOverride::Default => std::env::consts::OS == "linux",
            HostPlatformOverride::MacOs | HostPlatformOverride::Windows => false,
        }
    }

    fn llvm_linux_linkopt_settings(
        &self,
        matches: BuckArgMatches<'_>,
    ) -> Option<(usize, Vec<String>)> {
        if !(self.llvm || self.linux2mac || self.mac2linux) || !self.target_platform_is_linux() {
            return None;
        }

        let index = ["llvm", "linux2mac", "mac2linux"]
            .iter()
            .filter_map(|name| Self::flag_last_index(matches, name))
            .max()?;

        Some((
            index,
            vec![Self::bazel_command_line_single_list_build_setting(
                "//command_line_option:linkopt",
                LLVM_LINUX_LINKOPT,
            )],
        ))
    }

    fn llvm_cross_platform_settings(
        &self,
        matches: BuckArgMatches<'_>,
    ) -> Option<(usize, Vec<String>)> {
        let (index, target_platform, exec_platform) = if self.linux2mac {
            (
                Self::flag_last_index(matches, "linux2mac")?,
                LLVM_MACOS_AARCH64_PLATFORM,
                LLVM_LINUX_X86_64_PLATFORM,
            )
        } else if self.mac2linux {
            (
                Self::flag_last_index(matches, "mac2linux")?,
                LLVM_LINUX_X86_64_PLATFORM,
                LLVM_MACOS_AARCH64_PLATFORM,
            )
        } else {
            return None;
        };

        Some((
            index,
            vec![
                Self::bazel_command_line_single_list_build_setting(
                    "//command_line_option:platforms",
                    target_platform,
                ),
                Self::bazel_command_line_single_list_build_setting(
                    "//command_line_option:extra_execution_platforms",
                    exec_platform,
                ),
            ],
        ))
    }

    fn parse_bazel_command_line_build_settings_override(raw_arg: &str) -> Option<Vec<String>> {
        let value = raw_arg.strip_prefix("bazel.command_line_build_settings=")?;
        Some(
            value
                .split('\n')
                .filter(|value| !value.is_empty())
                .map(|value| value.to_owned())
                .collect(),
        )
    }

    fn cwd_alias_available(immediate_ctx: &ImmediateConfigContext<'_>, alias: &str) -> bool {
        immediate_ctx.resolve_alias_to_path_in_cwd(alias).is_ok()
    }

    /// Produces a single, ordered list of config overrides. A `ConfigOverride`
    /// represents either a file, passed via `--config-file`, or a config value,
    /// passed via `-c`/`--config`. The relative order of those are important,
    /// hence they're merged into a single list.
    pub fn config_overrides(
        &self,
        matches: BuckArgMatches<'_>,
        immediate_ctx: &ImmediateConfigContext<'_>,
        cwd: &AbsWorkingDir,
    ) -> bz_error::Result<Vec<ConfigOverride>> {
        fn with_indices<'a, T>(
            collection: &'a [T],
            name: &str,
            matches: BuckArgMatches<'a>,
        ) -> bz_error::Result<impl Iterator<Item = (usize, &'a T)> + use<'a, T>> {
            let indices: Vec<usize> = if collection.is_empty() {
                Vec::new()
            } else {
                let indices = matches.inner.indices_of(name);
                let indices = indices.unwrap_or_default();
                indices.into_iter().collect()
            };
            if indices.len() != collection.len() {
                return Err(bz_error::Error::from(IndicesLengthMismatchError {
                    flag_name: name.to_owned(),
                }));
            }
            Ok(indices.into_iter().zip(collection))
        }

        let mut bazel_command_line_build_setting_args = Vec::new();
        let mut config_values_args = Vec::new();
        for (index, config_value) in with_indices(&self.config_values, "config_values", matches)? {
            let (cell, raw_arg) = match config_value.split_once("//") {
                Some((cell, val)) if !cell.contains('=') => {
                    let cell = immediate_ctx
                        .resolve_alias_to_path_in_cwd(cell)?
                        .to_string();
                    (Some(cell), val)
                }
                _ => (None, config_value.as_str()),
            };

            if cell.is_none()
                && let Some(settings) =
                    Self::parse_bazel_command_line_build_settings_override(raw_arg)
            {
                bazel_command_line_build_setting_args.push((index, settings));
            } else if cell.is_none()
                && let Some(compilation_mode) = Self::bazel_compilation_mode(raw_arg)
            {
                bazel_command_line_build_setting_args.push((
                    index,
                    vec![Self::bazel_command_line_string_build_setting(
                        "//command_line_option:compilation_mode",
                        compilation_mode,
                    )],
                ));
            } else {
                config_values_args.push((
                    index,
                    ConfigOverride {
                        cell,
                        config_override: raw_arg.to_owned(),
                        config_type: ConfigType::Value as i32,
                    },
                ));
            }
        }

        let config_file_args = with_indices(&self.config_files, "config_files", matches)?
            .map(|(index, file)| {
                let (cell, path) = match file.split_once("//") {
                    Some((cell, val)) => {
                        // This should also reject =?
                        let cell = immediate_ctx
                            .resolve_alias_to_path_in_cwd(cell)?
                            .to_string();
                        (Some(cell), val.to_owned())
                    }
                    None => {
                        let abs_path = match AbsPath::new(file) {
                            Ok(p) => p.to_owned(),
                            Err(_) => cwd.resolve(Path::new(file)),
                        };
                        (None, abs_path.to_string())
                    }
                };
                Ok((
                    index,
                    ConfigOverride {
                        cell,
                        config_override: path,
                        config_type: ConfigType::File as i32,
                    },
                ))
            })
            .collect::<bz_error::Result<Vec<_>>>()?;

        bazel_command_line_build_setting_args.extend(
            with_indices(&self.bazel_cpu, "bazel_cpu", matches)?.map(|(index, cpu)| {
                (
                    index,
                    vec![Self::bazel_command_line_string_build_setting(
                        "//command_line_option:cpu",
                        cpu,
                    )],
                )
            }),
        );

        if let Some(settings) = self.bazel_cpu_override_settings(matches) {
            bazel_command_line_build_setting_args.push(settings);
        }

        bazel_command_line_build_setting_args.extend(
            with_indices(&self.bazel_host_cpu, "bazel_host_cpu", matches)?.map(|(index, cpu)| {
                (
                    index,
                    vec![Self::bazel_command_line_string_build_setting(
                        "//command_line_option:host_cpu",
                        cpu,
                    )],
                )
            }),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(&self.bazel_platforms, "bazel_platforms", matches)?.filter_map(
                |(index, platforms)| {
                    let settings = Self::bazel_command_line_list_build_setting(
                        "//command_line_option:platforms",
                        platforms,
                    );
                    if settings.is_empty() {
                        None
                    } else {
                        Some((index, settings))
                    }
                },
            ),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(
                &self.bazel_extra_execution_platforms,
                "bazel_extra_execution_platforms",
                matches,
            )?
            .filter_map(|(index, platforms)| {
                let settings = Self::bazel_command_line_list_build_setting(
                    "//command_line_option:extra_execution_platforms",
                    platforms,
                );
                if settings.is_empty() {
                    None
                } else {
                    Some((index, settings))
                }
            }),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(
                &self.bazel_extra_toolchains,
                "bazel_extra_toolchains",
                matches,
            )?
            .filter_map(|(index, toolchains)| {
                let settings = Self::bazel_command_line_list_build_setting(
                    "//command_line_option:extra_toolchains",
                    toolchains,
                );
                if settings.is_empty() {
                    None
                } else {
                    Some((index, settings))
                }
            }),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(
                &self.bazel_compilation_mode,
                "bazel_compilation_mode",
                matches,
            )?
            .map(|(index, compilation_mode)| {
                (
                    index,
                    vec![Self::bazel_command_line_string_build_setting(
                        "//command_line_option:compilation_mode",
                        compilation_mode,
                    )],
                )
            }),
        );

        let rules_rust_available = if self.llvm || self.linux2mac || self.mac2linux {
            Self::cwd_alias_available(immediate_ctx, "rules_rust")
        } else {
            false
        };

        if let Some(settings) = self.llvm_toolchain_override_settings(matches, rules_rust_available)
        {
            bazel_command_line_build_setting_args.push(settings);
        }

        if let Some(settings) = self.llvm_linux_linkopt_settings(matches) {
            bazel_command_line_build_setting_args.push(settings);
        }

        if let Some(settings) = self.llvm_cross_platform_settings(matches) {
            bazel_command_line_build_setting_args.push(settings);
        }

        if let Some(settings) = self.llvm_cargo_cc_rs_action_env_settings(matches) {
            bazel_command_line_build_setting_args.push(settings);
        }

        bazel_command_line_build_setting_args.extend(
            with_indices(&self.bazel_linkopt, "bazel_linkopt", matches)?.map(|(index, linkopt)| {
                (
                    index,
                    vec![Self::bazel_command_line_single_list_build_setting(
                        "//command_line_option:linkopt",
                        linkopt,
                    )],
                )
            }),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(&self.bazel_host_linkopt, "bazel_host_linkopt", matches)?.map(
                |(index, linkopt)| {
                    (
                        index,
                        vec![Self::bazel_command_line_single_list_build_setting(
                            "//command_line_option:host_linkopt",
                            linkopt,
                        )],
                    )
                },
            ),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(&self.bazel_action_env, "bazel_action_env", matches)?.map(
                |(index, env)| {
                    (
                        index,
                        vec![Self::bazel_command_line_single_list_build_setting(
                            BAZEL_ACTION_ENV,
                            env,
                        )],
                    )
                },
            ),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(
                &self.bazel_host_action_env,
                "bazel_host_action_env",
                matches,
            )?
            .map(|(index, env)| {
                (
                    index,
                    vec![Self::bazel_command_line_single_list_build_setting(
                        BAZEL_HOST_ACTION_ENV,
                        env,
                    )],
                )
            }),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(
                &self.bazel_java_language_version,
                "bazel_java_language_version",
                matches,
            )?
            .map(|(index, java_language_version)| {
                (
                    index,
                    vec![Self::bazel_command_line_string_build_setting(
                        BAZEL_JAVA_LANGUAGE_VERSION,
                        java_language_version,
                    )],
                )
            }),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(
                &self.bazel_java_runtime_version,
                "bazel_java_runtime_version",
                matches,
            )?
            .map(|(index, java_runtime_version)| {
                (
                    index,
                    vec![Self::bazel_command_line_string_build_setting(
                        BAZEL_JAVA_RUNTIME_VERSION,
                        java_runtime_version,
                    )],
                )
            }),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(
                &self.bazel_tool_java_language_version,
                "bazel_tool_java_language_version",
                matches,
            )?
            .map(|(index, tool_java_language_version)| {
                (
                    index,
                    vec![Self::bazel_command_line_string_build_setting(
                        BAZEL_TOOL_JAVA_LANGUAGE_VERSION,
                        tool_java_language_version,
                    )],
                )
            }),
        );

        bazel_command_line_build_setting_args.extend(
            with_indices(
                &self.bazel_tool_java_runtime_version,
                "bazel_tool_java_runtime_version",
                matches,
            )?
            .map(|(index, tool_java_runtime_version)| {
                (
                    index,
                    vec![Self::bazel_command_line_string_build_setting(
                        BAZEL_TOOL_JAVA_RUNTIME_VERSION,
                        tool_java_runtime_version,
                    )],
                )
            }),
        );

        let bazel_command_line_build_setting_arg =
            if bazel_command_line_build_setting_args.is_empty() {
                None
            } else {
                bazel_command_line_build_setting_args
                    .sort_by(|(lhs_index, _), (rhs_index, _)| lhs_index.cmp(rhs_index));
                let index = bazel_command_line_build_setting_args
                    .last()
                    .map(|(index, _)| *index)
                    .unwrap();
                let settings = bazel_command_line_build_setting_args
                    .into_iter()
                    .flat_map(|(_, settings)| settings)
                    .collect();
                Some((
                    index,
                    Self::bazel_command_line_build_settings_override(settings),
                ))
            };

        let mut ordered_merged_configs: Vec<(usize, ConfigOverride)> = config_file_args;
        ordered_merged_configs.extend(config_values_args);
        ordered_merged_configs.extend(bazel_command_line_build_setting_arg);
        if let Some(llvm_bzlmod_dep) = self.llvm_bzlmod_dep_override(matches) {
            ordered_merged_configs.push(llvm_bzlmod_dep);
        }
        if let Some(llvm_macos_repo_env) = self.llvm_macos_repo_env_override(matches) {
            ordered_merged_configs.push(llvm_macos_repo_env);
        }
        if let Some(host_platform_constraints) = self.bazel_host_platform_constraints(matches) {
            ordered_merged_configs.push(host_platform_constraints);
        }
        ordered_merged_configs.sort_by(|(lhs_index, _), (rhs_index, _)| lhs_index.cmp(rhs_index));

        Ok(ordered_merged_configs.into_map(|(_, config_arg)| config_arg))
    }

    pub fn host_platform_override(&self) -> HostPlatformOverride {
        self.host_platform_spec().platform
    }

    pub fn implied_target_platform(&self) -> Option<&'static str> {
        if self.linux2mac {
            Some(LLVM_MACOS_AARCH64_PLATFORM)
        } else if self.mac2linux {
            Some(LLVM_LINUX_X86_64_PLATFORM)
        } else {
            None
        }
    }

    fn host_platform_spec(&self) -> PlatformSpec {
        match self.fake_host {
            Some(platform) => PlatformSpec::new(platform, self.host_arch_spec()),
            None if self.linux || self.linux_arm || self.linux2mac => {
                PlatformSpec::new(HostPlatformOverride::Linux, self.host_arch_spec())
            }
            None if self.mac || self.mac_intel || self.mac2linux => {
                PlatformSpec::new(HostPlatformOverride::MacOs, self.host_arch_spec())
            }
            None => PlatformSpec::new(HostPlatformOverride::Default, self.host_arch_spec()),
        }
    }

    fn target_platform_spec(&self) -> PlatformSpec {
        if self.linux2mac {
            PlatformSpec::new(HostPlatformOverride::MacOs, HostArchOverride::AArch64)
        } else if self.mac2linux {
            PlatformSpec::new(HostPlatformOverride::Linux, HostArchOverride::X86_64)
        } else {
            self.host_platform_spec()
        }
    }

    fn host_arch_spec(&self) -> HostArchOverride {
        match self.fake_arch {
            Some(arch) => arch,
            None if self.linux || self.linux2mac => HostArchOverride::X86_64,
            None if self.linux_arm => HostArchOverride::AArch64,
            None if self.mac || self.mac2linux => HostArchOverride::AArch64,
            None if self.mac_intel => HostArchOverride::X86_64,
            None => HostArchOverride::Default,
        }
    }

    pub fn host_arch_override(&self) -> HostArchOverride {
        self.host_arch_spec()
    }
    pub fn host_xcode_version_override(&self) -> Option<String> {
        self.fake_xcode_version.to_owned()
    }

    pub fn default_ref() -> &'static Self {
        static DEFAULT: CommonBuildConfigurationOptions = CommonBuildConfigurationOptions {
            config_values: vec![],
            config_files: vec![],
            fake_host: None,
            fake_arch: None,
            linux: false,
            linux_arm: false,
            mac: false,
            mac_intel: false,
            llvm: false,
            linux2mac: false,
            mac2linux: false,
            bazel_cpu: vec![],
            bazel_host_cpu: vec![],
            bazel_platforms: vec![],
            bazel_extra_execution_platforms: vec![],
            bazel_extra_toolchains: vec![],
            bazel_compilation_mode: vec![],
            bazel_linkopt: vec![],
            bazel_host_linkopt: vec![],
            bazel_action_env: vec![],
            bazel_host_action_env: vec![],
            bazel_java_language_version: vec![],
            bazel_java_runtime_version: vec![],
            bazel_tool_java_language_version: vec![],
            bazel_tool_java_runtime_version: vec![],
            fake_xcode_version: None,
            reuse_current_config: false,
            preemptible: Some(PreemptibleWhen::Never),
            exit_when: None,
        };
        &DEFAULT
    }

    pub fn reuse_current_config_and_preemptible_ref() -> &'static Self {
        static OPTS: CommonBuildConfigurationOptions = CommonBuildConfigurationOptions {
            config_values: vec![],
            config_files: vec![],
            fake_host: None,
            fake_arch: None,
            linux: false,
            linux_arm: false,
            mac: false,
            mac_intel: false,
            llvm: false,
            linux2mac: false,
            mac2linux: false,
            bazel_cpu: vec![],
            bazel_host_cpu: vec![],
            bazel_platforms: vec![],
            bazel_extra_execution_platforms: vec![],
            bazel_extra_toolchains: vec![],
            bazel_compilation_mode: vec![],
            bazel_linkopt: vec![],
            bazel_host_linkopt: vec![],
            bazel_action_env: vec![],
            bazel_host_action_env: vec![],
            bazel_java_language_version: vec![],
            bazel_java_runtime_version: vec![],
            bazel_tool_java_language_version: vec![],
            bazel_tool_java_runtime_version: vec![],
            fake_xcode_version: None,
            reuse_current_config: true,
            preemptible: Some(PreemptibleWhen::OnDifferentState),
            exit_when: None,
        };
        &OPTS
    }
}

#[derive(Debug, clap::Parser, serde::Serialize, serde::Deserialize, Default)]
#[clap(next_help_heading = "Starlark Options")]
pub struct CommonStarlarkOptions {
    /// Disable runtime type checking in Starlark interpreter.
    ///
    /// This option is not stable, and can be used only locally
    /// to diagnose evaluation performance problems.
    #[clap(long)]
    pub disable_starlark_types: bool,

    /// Typecheck bzl and bxl files during evaluation.
    #[clap(long, hide = true)]
    pub unstable_typecheck: bool,

    /// Record or show target call stacks.
    ///
    /// Starlark call stacks will be included in duplicate targets error.
    ///
    /// If a command outputs targets (like `targets` command),
    /// starlark call stacks will be printed after the targets.
    #[clap(long = "stack")]
    pub target_call_stacks: bool,

    /// If there are targets with duplicate names in `BUCK` file,
    /// skip all the duplicates but the first one.
    /// This is a hack for TD. Do not use this option.
    #[clap(long, hide = true)]
    pub(crate) skip_targets_with_duplicate_names: bool,

    /// Enables profiling for all evaluations whose evaluation identifier matches one of the provided patterns.
    ///
    /// Some examples identifiers:
    ///    analysis/cell//bz/app/bz_action_impl:bz_action_impl (cfg:linux-x86_64#27ac5723e0c99706)
    ///    load/cell//build_defs/json.bzl
    ///    load/prelude//playground/test.bxl
    ///    load/cell//build_defs/json.bzl@other_cell
    ///    load_buildfile/root//third-party-buck/platform010/build/ncurses
    ///    load_packagefile/root//cli/rust/cli_delegate
    ///    anon_analysis/anon//:_anon_link_rule (anon: 766183dc9b6f680a) (//platform/execution:linux-x86_64#08961b14cfb182aa)
    ///    bxl/prelude//playground/test.bxl:playground
    ///
    /// You can pass `--profile-patterns=.*` to enable no-op profiling for everything (additionally pass `--profile-patterns-mode=none` to
    /// use no-op profiling to just get a list of all the identifiers).
    ///
    /// The profile results will be written to individual .profile files in `<ROOT_OUTPUT>/<data+time>-<uuid>/` where ROOT_OUTPUT comes from
    /// the --profile-patterns-output flag. In that directory there will also be a file listing all the identifiers that were profiled.
    ///
    /// Enabling/disabling profiling of an evaluation will invalidate the results of that evaluation and it will be recomputed. In some
    /// cases, this will cause other work to also need to be redone (for example, invalidating the result of loading PACKAGE files
    /// causes all consumers to be recomputed). But if you keep profiling options consistent between commands, only the work that is
    /// otherwise invalidated will be redone (and only for those would profiling results be created).
    ///
    /// You must also pass --profile-patterns-mode and --profile-patterns-output.
    #[clap(
        long,
        requires = "profile_patterns_output",
        requires = "profile_patterns_mode"
    )]
    pub(crate) profile_patterns: Option<Vec<String>>,

    #[clap(long, value_name = "PATH")]
    profile_patterns_output: Option<PathArg>,

    /// Profile mode.
    ///
    /// Memory profiling modes have suffixes either `-allocated` or `-retained`.
    ///
    /// `-retained` means memory kept in frozen starlark heaps after analysis completes.
    /// `-retained` does not work when profiling loading,
    /// because no memory is retained after loading and frozen heap is not even created.
    /// This is probably what you want when profiling analysis.
    ///
    /// `-allocated` means allocated memory, including memory which is later garbage collected.
    #[clap(long, value_enum)]
    profile_patterns_mode: Option<BuckProfileMode>,
}

impl CommonStarlarkOptions {
    pub fn default_ref() -> &'static Self {
        static DEFAULT: CommonStarlarkOptions = CommonStarlarkOptions {
            disable_starlark_types: false,
            unstable_typecheck: false,
            target_call_stacks: false,
            skip_targets_with_duplicate_names: false,
            profile_patterns: None,
            profile_patterns_output: None,
            profile_patterns_mode: None,
        };
        &DEFAULT
    }

    pub(crate) fn profile_pattern_opts(
        &self,
        working_dir: &AbsWorkingDir,
    ) -> Option<bz_cli_proto::client_context::ProfilePatternOptions> {
        self.profile_patterns.as_ref().map(|v| {
            bz_cli_proto::client_context::ProfilePatternOptions {
                profile_patterns: v.clone(),
                profile_mode: self.profile_patterns_mode.as_ref().unwrap().to_proto() as i32,
                profile_output: self
                    .profile_patterns_output
                    .as_ref()
                    .unwrap()
                    .resolve(working_dir)
                    .to_string(),
            }
        })
    }
}

/// Common options for commands like `build` or `query`.
/// Not all the commands have all the options.
#[derive(Debug, clap::Parser, serde::Serialize, serde::Deserialize, Default)]
pub struct CommonCommandOptions {
    /// Buckconfig and similar options.
    #[clap(flatten)]
    pub config_opts: CommonBuildConfigurationOptions,

    /// Starlark options.
    #[clap(flatten)]
    pub starlark_opts: CommonStarlarkOptions,

    /// UI options.
    #[clap(flatten)]
    pub console_opts: CommonConsoleOptions,

    /// Event-log options.
    #[clap(flatten)]
    pub event_log_opts: CommonEventLogOptions,
}

#[derive(Debug, PartialEq)]
pub enum PrintOutputsFormat {
    Plain,
    Simple,
    Json,
}

#[derive(Clone, Copy)]
pub struct BuckArgMatches<'a> {
    inner: &'a clap::ArgMatches,
    expanded_argv: &'a ExpandedArgv,
}

impl<'a> BuckArgMatches<'a> {
    pub fn from_clap(inner: &'a clap::ArgMatches, expanded_argv: &'a ExpandedArgv) -> Self {
        Self {
            inner,
            expanded_argv,
        }
    }

    pub fn unwrap_subcommand(&self) -> Self {
        match self.inner.subcommand().map(|s| s.1) {
            Some(submatches) => Self {
                inner: submatches,
                expanded_argv: self.expanded_argv,
            },
            None => panic!("Parsed a subcommand but couldn't extract subcommand argument matches"),
        }
    }

    pub fn get_representative_config_flags(&self) -> Vec<String> {
        bz_common::argv::get_representative_config_flags(self.expanded_argv)
    }

    pub fn get_representative_config_flags_by_source(&self) -> Vec<RepresentativeConfigFlag> {
        use bz_common::argv::ConfigFlagValue;
        use bz_common::argv::get_flagfile_for_logging;
        use bz_common::argv::parse_config_flags;

        let mut args: Vec<RepresentativeConfigFlag> = Vec::new();
        let mut last_flagfile = None;

        for (flag_value, source) in parse_config_flags(self.expanded_argv) {
            let flagfile = match source {
                ExpandedArgSource::Inline => None,
                ExpandedArgSource::Flagfile(file) => get_flagfile_for_logging(file),
            };

            match flagfile {
                Some(flagfile) => {
                    if Some(flagfile) != last_flagfile {
                        args.push(RepresentativeConfigFlag {
                            source: Some(RepresentativeConfigFlagSource::ModeFile(
                                flagfile.kind.to_string(),
                            )),
                        });
                    }
                }
                None => {
                    let source = match flag_value {
                        ConfigFlagValue::ConfigFlag(v) => {
                            RepresentativeConfigFlagSource::ConfigFlag(v)
                        }
                        ConfigFlagValue::ConfigFile(v) => {
                            RepresentativeConfigFlagSource::ConfigFile(v)
                        }
                        ConfigFlagValue::Modifier(v) => RepresentativeConfigFlagSource::Modifier(v),
                        ConfigFlagValue::TargetPlatforms(v) => {
                            RepresentativeConfigFlagSource::TargetPlatforms(v)
                        }
                        ConfigFlagValue::TargetUniverse(v) => {
                            RepresentativeConfigFlagSource::TargetUniverse(v)
                        }
                    };
                    args.push(RepresentativeConfigFlag {
                        source: Some(source),
                    });
                }
            }
            last_flagfile = flagfile;
        }

        args
    }
}

#[cfg(test)]
mod tests {
    use bz_cli_proto::RepresentativeConfigFlag;
    use bz_cli_proto::representative_config_flag::Source as RepresentativeConfigFlagSource;
    use bz_common::argv::ArgFileKind;
    use bz_common::argv::ArgFilePath;
    use bz_common::argv::ExpandedArgvBuilder;
    use bz_core::cells::cell_path::CellPath;
    use bz_core::fs::project::ProjectRootTemp;
    use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
    use clap::CommandFactory;
    use clap::FromArgMatches;

    use super::*;

    fn source(flag: RepresentativeConfigFlagSource) -> RepresentativeConfigFlag {
        RepresentativeConfigFlag { source: Some(flag) }
    }

    #[derive(Debug, clap::Parser)]
    struct TestConfigOpts {
        #[clap(flatten)]
        config: CommonBuildConfigurationOptions,
    }

    fn test_cwd() -> bz_fs::working_dir::AbsWorkingDir {
        use bz_fs::paths::abs_norm_path::AbsNormPathBuf;
        use bz_fs::working_dir::AbsWorkingDir;

        if cfg!(windows) {
            AbsWorkingDir::unchecked_new(AbsNormPathBuf::new("C:\\tmp".into()).unwrap())
        } else {
            AbsWorkingDir::unchecked_new(AbsNormPathBuf::new("/tmp".into()).unwrap())
        }
    }

    #[test]
    fn test_bes_timeout_duration_rejects_invalid_value() {
        let opts = CommonEventLogOptions {
            bes_timeout: Some("not-a-duration".to_owned()),
            ..Default::default()
        };
        let error = opts.bes_timeout_duration().unwrap_err().to_string();
        assert!(error.contains("invalid --bes_timeout"));
        assert!(error.contains("not-a-duration"));
    }

    #[test]
    fn test_bes_timeout_duration_parses_valid_value() {
        let opts = CommonEventLogOptions {
            bes_timeout: Some("5s".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            opts.bes_timeout_duration().unwrap(),
            Some(std::time::Duration::from_secs(5))
        );
    }

    #[test]
    fn test_bep_sets_buildbuddy_bes_defaults() {
        let opts = CommonEventLogOptions {
            bep: true,
            ..Default::default()
        };

        assert_eq!(opts.bes_backend(), Some(BUILDBUDDY_BES_BACKEND));
        assert_eq!(opts.bes_results_url(), Some(BUILDBUDDY_BES_RESULTS_URL));
    }

    #[test]
    fn test_bep_allows_explicit_bes_overrides() {
        let opts = CommonEventLogOptions {
            bep: true,
            bes_backend: Some("grpc://example.com".to_owned()),
            bes_results_url: Some("https://example.com/invocation/".to_owned()),
            ..Default::default()
        };

        assert_eq!(opts.bes_backend(), Some("grpc://example.com"));
        assert_eq!(
            opts.bes_results_url(),
            Some("https://example.com/invocation/")
        );
    }

    #[test]
    fn test_buildbuddy_default_sets_bes_defaults() {
        let opts = CommonEventLogOptions::default();

        assert_eq!(
            opts.bes_backend_with_buildbuddy_default(true, false),
            Some(BUILDBUDDY_BES_BACKEND)
        );
        assert_eq!(
            opts.bes_results_url_with_buildbuddy_default(true, false),
            Some(BUILDBUDDY_BES_RESULTS_URL)
        );
    }

    #[test]
    fn test_dev_uses_buildbuddy_dev_bes_defaults() {
        let opts = CommonEventLogOptions::default();

        assert_eq!(
            opts.bes_backend_with_buildbuddy_default(true, true),
            Some(BUILDBUDDY_BES_BACKEND_DEV)
        );
        assert_eq!(
            opts.bes_results_url_with_buildbuddy_default(true, true),
            Some(BUILDBUDDY_BES_RESULTS_URL_DEV)
        );
    }

    #[test]
    fn test_os_and_arch_alias_fake_host_and_arch() {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--os=linux", "--arch=x8664"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();

        assert_eq!(
            opts.config.host_platform_override(),
            HostPlatformOverride::Linux
        );
        assert_eq!(opts.config.host_arch_override(), HostArchOverride::X86_64);
    }

    #[test]
    fn test_linux_alias_sets_linux_x8664() {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--linux"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();

        assert_eq!(
            opts.config.host_platform_override(),
            HostPlatformOverride::Linux
        );
        assert_eq!(opts.config.host_arch_override(), HostArchOverride::X86_64);
    }

    #[test]
    fn test_linux_alias_does_not_override_java_runtime() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--linux"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        let override_values = overrides
            .iter()
            .map(|override_| override_.config_override.as_str())
            .collect::<Vec<_>>();
        assert_eq!(override_values.len(), 2);
        assert!(override_values.contains(&"bazel.command_line_build_settings=string\t//command_line_option:cpu\tk8\nstring\t//command_line_option:host_cpu\tk8"));
        assert!(override_values.contains(
            &"bazel.host_platform_constraints=@platforms//cpu:x86_64\n@platforms//os:linux"
        ));
        Ok(())
    }

    #[test]
    fn test_linux_arm_alias_sets_linux_aarch64() {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--linux_arm"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();

        assert_eq!(
            opts.config.host_platform_override(),
            HostPlatformOverride::Linux
        );
        assert_eq!(opts.config.host_arch_override(), HostArchOverride::AArch64);
    }

    #[test]
    fn test_java_runtime_flags_become_build_settings() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from([
                "test",
                "--java_language_version=17",
                "--java_runtime_version=remotejdk_17",
                "--tool_java_language_version=21",
                "--tool_java_runtime_version=remotejdk_21",
            ])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        assert_eq!(overrides.len(), 1);
        assert_eq!(
            overrides[0].config_override,
            "bazel.command_line_build_settings=string\t//command_line_option:java_language_version\t17\nstring\t//command_line_option:java_runtime_version\tremotejdk_17\nstring\t//command_line_option:tool_java_language_version\t21\nstring\t//command_line_option:tool_java_runtime_version\tremotejdk_21"
        );
        Ok(())
    }

    #[test]
    fn test_java_runtime_flag_with_linux_alias_becomes_build_setting() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--linux", "--java_runtime_version=local_jdk"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        let override_values = overrides
            .iter()
            .map(|override_| override_.config_override.as_str())
            .collect::<Vec<_>>();
        assert_eq!(override_values.len(), 2);
        assert!(override_values.contains(&"bazel.command_line_build_settings=string\t//command_line_option:cpu\tk8\nstring\t//command_line_option:host_cpu\tk8\nstring\t//command_line_option:java_runtime_version\tlocal_jdk"));
        assert!(override_values.contains(
            &"bazel.host_platform_constraints=@platforms//cpu:x86_64\n@platforms//os:linux"
        ));
        Ok(())
    }

    #[test]
    fn test_os_and_arch_aliases_set_bazel_cpu_defaults() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--os=linux", "--arch=aarch64"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        let override_values = overrides
            .iter()
            .map(|override_| override_.config_override.as_str())
            .collect::<Vec<_>>();
        assert_eq!(override_values.len(), 2);
        assert!(override_values.contains(&"bazel.command_line_build_settings=string\t//command_line_option:cpu\taarch64\nstring\t//command_line_option:host_cpu\taarch64"));
        assert!(override_values.contains(
            &"bazel.host_platform_constraints=@platforms//cpu:aarch64\n@platforms//os:linux"
        ));
        Ok(())
    }

    #[test]
    fn test_explicit_cpu_after_linux_alias_wins() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--linux", "--cpu=custom"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        let override_values = overrides
            .iter()
            .map(|override_| override_.config_override.as_str())
            .collect::<Vec<_>>();
        assert_eq!(override_values.len(), 2);
        assert!(override_values.contains(&"bazel.command_line_build_settings=string\t//command_line_option:cpu\tk8\nstring\t//command_line_option:host_cpu\tk8\nstring\t//command_line_option:cpu\tcustom"));
        assert!(override_values.contains(
            &"bazel.host_platform_constraints=@platforms//cpu:x86_64\n@platforms//os:linux"
        ));
        Ok(())
    }

    #[test]
    fn test_mac_alias_sets_macos_aarch64() {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--mac"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();

        assert_eq!(
            opts.config.host_platform_override(),
            HostPlatformOverride::MacOs
        );
        assert_eq!(opts.config.host_arch_override(), HostArchOverride::AArch64);
    }

    #[test]
    fn test_mac_intel_alias_sets_macos_x8664() {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--mac_intel"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();

        assert_eq!(
            opts.config.host_platform_override(),
            HostPlatformOverride::MacOs
        );
        assert_eq!(opts.config.host_arch_override(), HostArchOverride::X86_64);
    }

    #[test]
    fn test_llvm_flag_adds_hermetic_llvm_toolchain() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--llvm"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        let override_values = overrides
            .iter()
            .map(|override_| override_.config_override.as_str())
            .collect::<Vec<_>>();
        assert_eq!(override_values.len(), 2);
        assert!(
            override_values.contains(&"bazel.extra_bzlmod_deps=llvm@0.8.5,bazel_features@1.47.0")
        );
        let build_settings = override_values
            .iter()
            .find(|value| value.starts_with("bazel.command_line_build_settings="))
            .unwrap();
        assert!(
            build_settings
                .contains("list\t//command_line_option:extra_toolchains\t@llvm//toolchain:all")
        );
        assert!(build_settings.contains("bool\t@llvm//config:experimental_stub_libgcc_s\ttrue"));
        assert!(build_settings.contains("bool\t@llvm//config:empty_sysroot\tfalse"));
        assert!(build_settings.contains(
            "bool\t@@rules_cc+//cc/toolchains/args/archiver_flags:use_libtool_on_macos\tfalse"
        ));
        assert!(!build_settings.contains("@@rules_rust+//rust/settings"));
        if cfg!(target_os = "linux") {
            assert!(build_settings.contains("list\t//command_line_option:linkopt\t-no-pie"));
        } else {
            assert!(!build_settings.contains("//command_line_option:linkopt"));
        }
        Ok(())
    }

    #[test]
    fn test_llvm_flag_adds_rules_rust_setting_when_rules_rust_is_available() {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--llvm"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);

        let build_settings = opts
            .config
            .llvm_toolchain_override_settings(matches, true)
            .unwrap()
            .1
            .join("\n");
        assert!(build_settings.contains(
            "bool\t@@rules_rust+//rust/settings:experimental_use_sh_toolchain_for_bootstrap_process_wrapper\ttrue"
        ));
    }

    #[test]
    fn test_linux_llvm_adds_no_pie_linkopt() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--linux", "--llvm"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        let override_values = overrides
            .iter()
            .map(|override_| override_.config_override.as_str())
            .collect::<Vec<_>>();
        assert_eq!(override_values.len(), 4);
        assert!(
            override_values.contains(&"bazel.extra_bzlmod_deps=llvm@0.8.5,bazel_features@1.47.0")
        );
        let macos_frameworks_repo_env =
            format!("bazel.repo_env=BAZEL_MACOS_FRAMEWORKS={LLVM_MACOS_FRAMEWORKS}");
        assert!(override_values.contains(&macos_frameworks_repo_env.as_str()));

        let build_settings = override_values
            .iter()
            .find(|value| value.starts_with("bazel.command_line_build_settings="))
            .unwrap();
        assert!(build_settings.contains("string\t//command_line_option:cpu\tk8"));
        assert!(
            build_settings
                .contains("list\t//command_line_option:extra_toolchains\t@llvm//toolchain:all")
        );
        assert!(build_settings.contains("bool\t@llvm//config:experimental_stub_libgcc_s\ttrue"));
        assert!(build_settings.contains("bool\t@llvm//config:empty_sysroot\tfalse"));
        assert!(build_settings.contains(
            "bool\t@@rules_cc+//cc/toolchains/args/archiver_flags:use_libtool_on_macos\tfalse"
        ));
        assert!(build_settings.contains("list\t//command_line_option:linkopt\t-no-pie"));
        assert!(override_values.contains(
            &"bazel.host_platform_constraints=@platforms//cpu:x86_64\n@platforms//os:linux"
        ));
        Ok(())
    }

    #[test]
    fn test_linux2mac_alias_sets_linux_host_and_macos_llvm_target() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--linux2mac"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();

        assert_eq!(
            opts.config.host_platform_override(),
            HostPlatformOverride::Linux
        );
        assert_eq!(opts.config.host_arch_override(), HostArchOverride::X86_64);
        assert_eq!(
            opts.config.implied_target_platform(),
            Some(LLVM_MACOS_AARCH64_PLATFORM)
        );

        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        let override_values = overrides
            .iter()
            .map(|override_| override_.config_override.as_str())
            .collect::<Vec<_>>();
        assert_eq!(override_values.len(), 3);
        assert!(
            override_values.contains(&"bazel.extra_bzlmod_deps=llvm@0.8.5,bazel_features@1.47.0")
        );

        let build_settings = override_values
            .iter()
            .find(|value| value.starts_with("bazel.command_line_build_settings="))
            .unwrap();
        assert!(build_settings.contains("string\t//command_line_option:cpu\tdarwin_arm64"));
        assert!(build_settings.contains("string\t//command_line_option:host_cpu\tk8"));
        assert!(
            build_settings
                .contains("list\t//command_line_option:extra_toolchains\t@llvm//toolchain:all")
        );
        assert!(build_settings.contains("bool\t@llvm//config:experimental_stub_libgcc_s\ttrue"));
        assert!(build_settings.contains("bool\t@llvm//config:empty_sysroot\tfalse"));
        assert!(build_settings.contains(
            "bool\t@@rules_cc+//cc/toolchains/args/archiver_flags:use_libtool_on_macos\tfalse"
        ));
        assert!(
            build_settings
                .contains("list\t//command_line_option:platforms\t@llvm//platforms:macos_aarch64")
        );
        assert!(build_settings.contains(
            "list\t//command_line_option:extra_execution_platforms\t@llvm//platforms:linux_x86_64"
        ));
        assert!(build_settings.contains(
            "list\t//command_line_option:action_env\tCFLAGS_aarch64_apple_darwin=-fuse-ld=lld"
        ));
        assert!(build_settings.contains(
            "list\t//command_line_option:action_env\tCXXFLAGS_aarch64_apple_darwin=-fuse-ld=lld"
        ));
        assert!(override_values.contains(
            &"bazel.host_platform_constraints=@platforms//cpu:x86_64\n@platforms//os:linux"
        ));
        Ok(())
    }

    #[test]
    fn test_mac2linux_alias_sets_macos_host_and_linux_llvm_target() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "--mac2linux"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();

        assert_eq!(
            opts.config.host_platform_override(),
            HostPlatformOverride::MacOs
        );
        assert_eq!(opts.config.host_arch_override(), HostArchOverride::AArch64);
        assert_eq!(
            opts.config.implied_target_platform(),
            Some(LLVM_LINUX_X86_64_PLATFORM)
        );

        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        let override_values = overrides
            .iter()
            .map(|override_| override_.config_override.as_str())
            .collect::<Vec<_>>();
        assert_eq!(override_values.len(), 3);
        assert!(
            override_values.contains(&"bazel.extra_bzlmod_deps=llvm@0.8.5,bazel_features@1.47.0")
        );

        let build_settings = override_values
            .iter()
            .find(|value| value.starts_with("bazel.command_line_build_settings="))
            .unwrap();
        assert!(build_settings.contains("string\t//command_line_option:cpu\tk8"));
        assert!(build_settings.contains("string\t//command_line_option:host_cpu\tdarwin_arm64"));
        assert!(
            build_settings
                .contains("list\t//command_line_option:extra_toolchains\t@llvm//toolchain:all")
        );
        assert!(build_settings.contains("bool\t@llvm//config:experimental_stub_libgcc_s\ttrue"));
        assert!(build_settings.contains("bool\t@llvm//config:empty_sysroot\tfalse"));
        assert!(build_settings.contains(
            "bool\t@@rules_cc+//cc/toolchains/args/archiver_flags:use_libtool_on_macos\tfalse"
        ));
        assert!(
            build_settings
                .contains("list\t//command_line_option:platforms\t@llvm//platforms:linux_x86_64")
        );
        assert!(build_settings.contains(
            "list\t//command_line_option:extra_execution_platforms\t@llvm//platforms:macos_aarch64"
        ));
        assert!(override_values.contains(
            &"bazel.host_platform_constraints=@platforms//cpu:aarch64\n@platforms//os:osx"
        ));
        Ok(())
    }

    #[test]
    fn test_by_source_inline_flags() -> bz_error::Result<()> {
        let mut argv = ExpandedArgvBuilder::new();
        argv.push("-c".to_owned());
        argv.push("section.key=val".to_owned());
        argv.push("-m".to_owned());
        argv.push("//mod:bar".to_owned());
        argv.push("--config-file".to_owned());
        argv.push("//cfg.bcfg".to_owned());
        argv.push("--target-platforms=ovr//p:linux".to_owned());
        argv.push("--target-universe".to_owned());
        argv.push("//uni:target".to_owned());

        let argv = argv.build();
        let clap = clap::ArgMatches::default();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let flags = matches.get_representative_config_flags_by_source();

        assert_eq!(
            flags,
            vec![
                source(RepresentativeConfigFlagSource::ConfigFlag(
                    "section.key=val".to_owned()
                )),
                source(RepresentativeConfigFlagSource::Modifier(
                    "//mod:bar".to_owned()
                )),
                source(RepresentativeConfigFlagSource::ConfigFile(
                    "//cfg.bcfg".to_owned()
                )),
                source(RepresentativeConfigFlagSource::TargetPlatforms(
                    "ovr//p:linux".to_owned()
                )),
                source(RepresentativeConfigFlagSource::TargetUniverse(
                    "//uni:target".to_owned()
                )),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_by_source_argfile_collapsing() -> bz_error::Result<()> {
        let project_argfile = |path: &str| ArgFilePath::Project(CellPath::testing_new(path));
        let external_root = ProjectRootTemp::new().unwrap();
        let external_root = external_root.path();
        let external_argfile = |path: &str| {
            ArgFilePath::External(
                external_root
                    .root()
                    .join(ForwardRelativePathBuf::new(path.to_owned()).unwrap()),
            )
        };

        let mut argv = ExpandedArgvBuilder::new();
        argv.push("-c".to_owned());
        argv.push("inline.key=val".to_owned());

        argv.argfile_scope(
            ArgFileKind::Path(project_argfile("root//mode/dev")),
            |argv| {
                argv.push("-c=from.mode=1".to_owned());
                argv.push("-m".to_owned());
                argv.push("//mod:x".to_owned());
            },
        );

        argv.argfile_scope(ArgFileKind::Path(external_argfile("ext/mode")), |argv| {
            argv.push("-c=external.key=val".to_owned());
        });

        let argv = argv.build();
        let clap = clap::ArgMatches::default();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        let flags = matches.get_representative_config_flags_by_source();

        assert_eq!(
            flags,
            vec![
                source(RepresentativeConfigFlagSource::ConfigFlag(
                    "inline.key=val".to_owned()
                )),
                source(RepresentativeConfigFlagSource::ModeFile(
                    "@root//mode/dev".to_owned()
                )),
                source(RepresentativeConfigFlagSource::ConfigFlag(
                    "external.key=val".to_owned()
                )),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_config_overrides_with_default_opts_and_unregistered_args() -> bz_error::Result<()> {
        use crate::immediate_config::ImmediateConfigContext;

        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let opts = CommonBuildConfigurationOptions::default();
        let argv = ExpandedArgvBuilder::new().build();
        let clap = clap::ArgMatches::default();
        let matches = BuckArgMatches::from_clap(&clap, &argv);

        let overrides = opts.config_overrides(matches, &immediate_ctx, &cwd)?;
        assert!(overrides.is_empty());
        Ok(())
    }

    #[test]
    fn test_bazel_native_flags_become_build_settings() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from([
                "test",
                "--cpu=k8",
                "-c",
                "foo.bar=baz",
                "--platforms=//platforms:linux,@platforms//cpu:x86_64",
                "--extra_execution_platforms=@toolchains//platforms:linux_x86_64",
                "--extra_toolchains=@toolchains//cc:linux_x86_64",
                "--linkopt=-Wl,-z,now",
                "--host_linkopt",
                "-no-pie",
                "--host_cpu",
                "k8",
            ])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        use crate::immediate_config::ImmediateConfigContext;

        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        assert_eq!(overrides.len(), 2);
        assert_eq!(overrides[0].config_override, "foo.bar=baz");
        assert_eq!(
            overrides[1].config_override,
            "bazel.command_line_build_settings=string\t//command_line_option:cpu\tk8\nlist\t//command_line_option:platforms\t//platforms:linux\nlist\t//command_line_option:platforms\t@platforms//cpu:x86_64\nlist\t//command_line_option:extra_execution_platforms\t@toolchains//platforms:linux_x86_64\nlist\t//command_line_option:extra_toolchains\t@toolchains//cc:linux_x86_64\nlist\t//command_line_option:linkopt\t-Wl,-z,now\nlist\t//command_line_option:host_linkopt\t-no-pie\nstring\t//command_line_option:host_cpu\tk8"
        );
        Ok(())
    }

    #[test]
    fn test_bazel_compilation_mode_becomes_build_setting() -> bz_error::Result<()> {
        let clap = TestConfigOpts::command()
            .try_get_matches_from(["test", "-c", "opt", "--compilation_mode=dbg"])
            .unwrap();
        let opts = TestConfigOpts::from_arg_matches(&clap).unwrap();
        let argv = ExpandedArgvBuilder::new().build();
        let matches = BuckArgMatches::from_clap(&clap, &argv);
        use crate::immediate_config::ImmediateConfigContext;

        let cwd = test_cwd();
        let immediate_ctx = ImmediateConfigContext::new(&cwd);

        let overrides = opts
            .config
            .config_overrides(matches, &immediate_ctx, &cwd)?;

        assert_eq!(overrides.len(), 1);
        assert_eq!(
            overrides[0].config_override,
            "bazel.command_line_build_settings=string\t//command_line_option:compilation_mode\topt\nstring\t//command_line_option:compilation_mode\tdbg"
        );
        Ok(())
    }
}
