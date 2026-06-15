/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![feature(error_generic_member_access)]
#![feature(used_with_arg)]

use std::thread;

use bz_client::commands::build::BuildCommand;
use bz_client::commands::bxl::BxlCommand;
use bz_client::commands::clean::CleanCommand;
use bz_client::commands::ctargets::ConfiguredTargetsCommand;
use bz_client::commands::expand_external_cell::ExpandExternalCellsCommand;
use bz_client::commands::explain::ExplainCommand;
use bz_client::commands::help_env::HelpEnvCommand;
use bz_client::commands::init::InitCommand;
use bz_client::commands::install::InstallCommand;
use bz_client::commands::kill::KillCommand;
use bz_client::commands::killall::KillallCommand;
use bz_client::commands::lsp::LspCommand;
use bz_client::commands::profile::ProfileCommand;
use bz_client::commands::query::aquery::AqueryCommand;
use bz_client::commands::query::cquery::CqueryCommand;
use bz_client::commands::query::uquery::UqueryCommand;
use bz_client::commands::root::RootCommand;
use bz_client::commands::run::RunCommand;
use bz_client::commands::server::ServerCommand;
use bz_client::commands::status::StatusCommand;
use bz_client::commands::subscribe::SubscribeCommand;
use bz_client::commands::targets::TargetsCommand;
use bz_client::commands::test::TestCommand;
use bz_client_ctx::agent_context::AgentContextEntry;
use bz_client_ctx::agent_context::parse_agent_context;
use bz_client_ctx::argfiles::expand_argv;
use bz_client_ctx::client_ctx::BuckSubcommand;
use bz_client_ctx::client_ctx::ClientCommandContext;
use bz_client_ctx::client_metadata::ClientMetadata;
use bz_client_ctx::client_metadata::parse_client_metadata;
use bz_client_ctx::common::BuckArgMatches;
use bz_client_ctx::exit_result::ExitResult;
use bz_client_ctx::immediate_config::ImmediateConfigContext;
use bz_client_ctx::version::BuckVersion;
use bz_cmd_audit_client::AuditCommand;
use bz_cmd_debug_client::DebugCommand;
use bz_cmd_log_client::LogCommand;
use bz_cmd_rage_client::rage::RageCommand;
use bz_cmd_starlark_client::StarlarkCommand;
use bz_common::argv::Argv;
use bz_common::init::DaemonStartupConfig;
use bz_common::init::RemoteDefaultExecProperty;
use bz_common::init::RemoteDownloadOutputsMode;
use bz_common::init::RemoteExecutionStartupConfig;
use bz_common::invocation_paths_result::InvocationPathsResult;
use bz_common::invocation_roots::get_invocation_paths_result;
use bz_core::bz_env;
use bz_core::bz_env_name;
use bz_data::ErrorReport;
use bz_error::BuckErrorContext;
use bz_error::ErrorTag;
use bz_error::ExitCode;
use bz_error::bz_error;
use bz_error::conversion::clap::buck_error_clap_parser;
use bz_event_observer::verbosity::Verbosity;
use bz_fs::paths::file_name::FileNameBuf;
use bz_util::threads::thread_spawn_scoped;
use clap::CommandFactory;
use clap::FromArgMatches;
use dupe::Dupe;

use crate::check_user_allowed::check_user_allowed;
use crate::process_context::ProcessContext;

mod check_user_allowed;
mod cli_style;
pub(crate) mod commands;
pub mod panic;
pub mod process_context;

const BUILDBUDDY_REMOTE_ENDPOINT: &str = "remote.buildbuddy.io";
const BUILDBUDDY_REMOTE_ENDPOINT_DEV: &str = "remote.buildbuddy.dev";
const BUILDBUDDY_DEFAULT_RBE_CONTAINER_IMAGE: &str = "docker://gcr.io/flame-public/rbe-ubuntu24-04@sha256:f7db0d4791247f032fdb4451b7c3ba90e567923a341cc6dc43abfc283436791a";
const BUILDBUDDY_API_KEY_ENV_VAR: &str = "BUILDBUDDY_API_KEY";
const BZ_BUILDBUDDY_API_KEY_ENV_VAR: &str = "BZ_BUILDBUDDY_API_KEY";
const BUILDBUDDY_REMOTE_TIMEOUT_SECS: u64 = 60;

fn non_empty_buildbuddy_api_key(api_key: String) -> Option<String> {
    (!api_key.trim().is_empty()).then_some(api_key)
}

fn buildbuddy_api_key_from_env_vars(
    mut get_env: impl FnMut(&'static str) -> Option<String>,
) -> Option<String> {
    [BUILDBUDDY_API_KEY_ENV_VAR, BZ_BUILDBUDDY_API_KEY_ENV_VAR]
        .into_iter()
        .find_map(|env_var| get_env(env_var).and_then(non_empty_buildbuddy_api_key))
}

fn buildbuddy_api_key_from_env() -> Option<String> {
    buildbuddy_api_key_from_env_vars(|env_var| std::env::var(env_var).ok())
}

fn parse_remote_default_exec_property(value: &str) -> bz_error::Result<RemoteDefaultExecProperty> {
    let (name, value) = value.split_once('=').ok_or_else(|| {
        bz_error!(
            ErrorTag::Input,
            "Expected remote exec property in NAME=VALUE form"
        )
    })?;
    if name.is_empty() {
        return Err(bz_error!(
            ErrorTag::Input,
            "Expected remote exec property name to be non-empty"
        ));
    }
    Ok(RemoteDefaultExecProperty {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

fn parse_remote_download_outputs(value: &str) -> bz_error::Result<RemoteDownloadOutputsMode> {
    value.parse()
}

fn parse_isolation_dir(s: &str) -> bz_error::Result<FileNameBuf> {
    FileNameBuf::try_from(s.to_owned()).buck_error_context("isolation dir must be a directory name")
}

/// Options of `bz` command, before subcommand.
#[derive(Clone, Debug, clap::Parser)]
#[clap(next_help_heading = "Universal Options")]
struct BeforeSubcommandOptions {
    /// The name of the directory that bz creates within buck-out for writing outputs and daemon
    /// information. If one is not provided, bz creates a directory with the default name.
    ///
    /// Instances of bz share a daemon if and only if their isolation directory is identical.
    /// The isolation directory also influences the output paths provided by bz,
    /// and as a result using a non-default isolation dir will cause cache misses (and slower builds).
    #[clap(
        value_parser = buck_error_clap_parser(parse_isolation_dir),
        env("BUCK_ISOLATION_DIR"),
        long,
        global = true,
        default_value="v2"
    )]
    isolation_dir: FileNameBuf,

    /// How verbose buck should be while logging.
    ///
    /// Values:
    /// 0 = Quiet, errors only;
    /// 1 = Show status. Default;
    /// 2 = more info about errors;
    /// 3 = more info about everything;
    /// 4 = more info about everything + stderr;
    ///
    /// It can be combined with specific log items (stderr, full_failed_command, commands, actions,
    /// status, stats, success) to fine-tune the verbosity of the log. Example usage "-v=1,stderr"
    #[clap(
        short = 'v',
        long = "verbose",
        default_value = "1",
        global = true,
        env = bz_env_name!("BUCK_VERBOSE"),
        value_parser = buck_error_clap_parser(Verbosity::try_from_cli)
    )]
    verbosity: Verbosity,

    /// The oncall executing this command
    #[clap(long, global = true)]
    oncall: Option<String>,

    /// Metadata key-value pairs to inject into bz's logging. Client metadata must be of the
    /// form `key=value`, where `key` is a snake_case identifier, and will be sent to backend
    /// datasets.
    #[clap(long, global = true, value_parser = buck_error_clap_parser(parse_client_metadata))]
    client_metadata: Vec<ClientMetadata>,

    /// Agent context key=value pairs for telemetry.
    /// Used by AI agents to pass structured metadata. Schema is defined via buckconfig.
    /// Entries can be comma-separated or passed as separate flags.
    /// Examples:
    ///   --agent-context intent=fix,attempt=2,prior_error=missing_target
    ///   --agent-context intent=build --agent-context attempt=1
    #[clap(long, global = true, value_delimiter = ',', value_parser = buck_error_clap_parser(parse_agent_context))]
    agent_context: Vec<AgentContextEntry>,

    /// Do not launch a daemon process, run buck server in client process.
    ///
    /// Note even when running in no-buckd mode, it still writes state files.
    /// In particular, this command effectively kills buckd process
    /// running with the same isolation directory.
    ///
    /// This is an unsupported option used only for development work.
    #[clap(env("BUCK2_NO_BUCKD"), long, global(true), hide(true))]
    // Env var is BUCK2_NO_BUCKD instead of NO_BUCKD env var from buck1 because no buckd
    // is not supported for production work for buck2 and lots of places already set
    // NO_BUCKD=1 for buck1.
    no_buckd: bool,

    /// Enable filesystem watching for incremental invalidation.
    ///
    /// When disabled, Buck avoids recursive watcher setup during daemon startup.
    #[clap(long, global = true, conflicts_with = "no_watchfs")]
    watchfs: bool,

    /// Disable filesystem watching, overriding `[buck2] watchfs = true`.
    #[clap(long = "no-watchfs", global = true, hide = true)]
    no_watchfs: bool,

    /// URI of a remote cache endpoint. Bazel-compatible spelling.
    #[clap(
        long = "remote_cache",
        alias = "remote-cache",
        value_name = "ENDPOINT",
        global = true
    )]
    remote_cache: Option<String>,

    /// URI of a remote execution endpoint. Bazel-compatible spelling.
    #[clap(
        long = "remote_executor",
        alias = "remote-executor",
        value_name = "ENDPOINT",
        global = true
    )]
    remote_executor: Option<String>,

    /// URI of a Remote Asset API endpoint for repository downloads.
    #[clap(
        long = "experimental_remote_downloader",
        alias = "remote_downloader",
        alias = "remote-downloader",
        value_name = "ENDPOINT",
        global = true
    )]
    remote_downloader: Option<String>,

    /// Store and load reproducible repository contents through the remote cache.
    #[clap(
        long = "experimental_remote_repo_contents_cache",
        alias = "experimental-remote-repo-contents-cache",
        global = true,
        conflicts_with = "no_experimental_remote_repo_contents_cache"
    )]
    experimental_remote_repo_contents_cache: bool,

    /// Disable remote repo contents cache usage.
    #[clap(
        long = "noexperimental_remote_repo_contents_cache",
        global = true,
        hide = true
    )]
    no_experimental_remote_repo_contents_cache: bool,

    /// Use BuildBuddy as the remote execution and remote cache endpoint.
    #[clap(long = "rbe", global = true)]
    rbe: bool,

    /// Use BuildBuddy as the remote cache endpoint.
    #[clap(long = "cache", global = true)]
    cache: bool,

    /// Use BuildBuddy for remote execution/cache and build event upload.
    #[clap(long = "bb", alias = "buildbuddy", global = true)]
    buildbuddy: bool,

    /// Point the default BuildBuddy endpoints at the dev environment
    /// (`*.buildbuddy.dev`) instead of production (`*.buildbuddy.io`).
    ///
    /// Only affects the defaults applied by `--rbe`/`--cache`/`--bb`/`--bep`;
    /// explicit `--remote_cache`/`--bes_backend`/etc. still take precedence.
    #[clap(long = "dev", global = true, hide = true)]
    dev: bool,

    /// BuildBuddy API key to send to BuildBuddy gRPC endpoints.
    ///
    /// Can also be set with the `BUILDBUDDY_API_KEY` or `BZ_BUILDBUDDY_API_KEY`
    /// environment variable.
    #[clap(long = "api-key", value_name = "KEY", global = true)]
    api_key: Option<String>,

    /// Limit the maximum number of concurrent remote cache/executor connections.
    #[clap(
        long = "remote_max_connections",
        alias = "remote-max-connections",
        value_name = "N",
        global = true
    )]
    remote_max_connections: Option<usize>,

    /// Limit the maximum number of concurrent requests per remote gRPC connection.
    #[clap(
        long = "remote_max_concurrency_per_connection",
        alias = "remote-max-concurrency-per-connection",
        value_name = "N",
        global = true
    )]
    remote_max_concurrency_per_connection: Option<usize>,

    /// Bazel-compatible default remote execution platform property.
    #[clap(
        long = "remote_default_exec_properties",
        alias = "remote-default-exec-properties",
        value_name = "NAME=VALUE",
        global = true,
        value_parser = buck_error_clap_parser(parse_remote_default_exec_property)
    )]
    remote_default_exec_properties: Vec<RemoteDefaultExecProperty>,

    /// Bazel-compatible remote output download mode: minimal, toplevel, or all.
    #[clap(
        long = "remote_download_outputs",
        alias = "remote-download-outputs",
        value_name = "MODE",
        global = true,
        value_parser = buck_error_clap_parser(parse_remote_download_outputs),
        conflicts_with_all = ["remote_download_minimal", "remote_download_toplevel", "remote_download_all"]
    )]
    remote_download_outputs: Option<RemoteDownloadOutputsMode>,

    /// Download only remote outputs required as local action inputs.
    #[clap(
        long = "remote_download_minimal",
        alias = "remote-download-minimal",
        global = true,
        conflicts_with_all = ["remote_download_outputs", "remote_download_toplevel", "remote_download_all"]
    )]
    remote_download_minimal: bool,

    /// Download requested top-level outputs.
    #[clap(
        long = "remote_download_toplevel",
        alias = "remote-download-toplevel",
        global = true,
        conflicts_with_all = ["remote_download_outputs", "remote_download_minimal", "remote_download_all"]
    )]
    remote_download_toplevel: bool,

    /// Download all remote outputs.
    #[clap(
        long = "remote_download_all",
        alias = "remote-download-all",
        global = true,
        conflicts_with_all = ["remote_download_outputs", "remote_download_minimal", "remote_download_toplevel"]
    )]
    remote_download_all: bool,

    /// Print buck wrapper help.
    #[clap(skip)]
    help_wrapper: bool,
}

impl BeforeSubcommandOptions {
    fn watchfs_override(&self) -> Option<bool> {
        if self.watchfs {
            Some(true)
        } else if self.no_watchfs {
            Some(false)
        } else {
            None
        }
    }

    /// The default BuildBuddy remote endpoint, selecting the dev environment
    /// when `--dev` is passed.
    fn buildbuddy_remote_endpoint(&self) -> &'static str {
        if self.dev {
            BUILDBUDDY_REMOTE_ENDPOINT_DEV
        } else {
            BUILDBUDDY_REMOTE_ENDPOINT
        }
    }

    fn remote_execution_startup_config(&self) -> RemoteExecutionStartupConfig {
        self.remote_execution_startup_config_with_buildbuddy_api_key_env(
            buildbuddy_api_key_from_env(),
        )
    }

    fn remote_execution_startup_config_with_buildbuddy_api_key_env(
        &self,
        buildbuddy_api_key_env: Option<String>,
    ) -> RemoteExecutionStartupConfig {
        let remote_default_exec_properties = if !self.remote_default_exec_properties.is_empty() {
            Some(self.remote_default_exec_properties.clone())
        } else if self.rbe || self.buildbuddy {
            Some(vec![
                RemoteDefaultExecProperty {
                    name: "OSFamily".to_owned(),
                    value: "Linux".to_owned(),
                },
                RemoteDefaultExecProperty {
                    name: "container-image".to_owned(),
                    value: BUILDBUDDY_DEFAULT_RBE_CONTAINER_IMAGE.to_owned(),
                },
            ])
        } else {
            None
        };

        RemoteExecutionStartupConfig {
            remote_cache: self.remote_cache.clone().or_else(|| {
                (self.rbe || self.cache || self.buildbuddy)
                    .then(|| self.buildbuddy_remote_endpoint().to_owned())
            }),
            remote_executor: self.remote_executor.clone().or_else(|| {
                (self.rbe || self.buildbuddy).then(|| self.buildbuddy_remote_endpoint().to_owned())
            }),
            remote_downloader: self.remote_downloader.clone().or_else(|| {
                (self.rbe || self.buildbuddy).then(|| self.buildbuddy_remote_endpoint().to_owned())
            }),
            experimental_remote_repo_contents_cache: self.experimental_remote_repo_contents_cache
                && !self.no_experimental_remote_repo_contents_cache,
            buildbuddy_api_key: self
                .api_key
                .clone()
                .or_else(|| buildbuddy_api_key_env.and_then(non_empty_buildbuddy_api_key)),
            remote_default_exec_properties,
            remote_max_connections: self.remote_max_connections,
            remote_max_concurrency_per_connection: self.remote_max_concurrency_per_connection,
            remote_timeout_secs: (self.rbe || self.buildbuddy)
                .then_some(BUILDBUDDY_REMOTE_TIMEOUT_SECS),
        }
    }

    fn buildbuddy_bes(&self) -> bool {
        self.buildbuddy
    }

    fn remote_download_outputs_override(&self) -> Option<RemoteDownloadOutputsMode> {
        if self.remote_download_minimal {
            Some(RemoteDownloadOutputsMode::Minimal)
        } else if self.remote_download_toplevel {
            Some(RemoteDownloadOutputsMode::Toplevel)
        } else if self.remote_download_all {
            Some(RemoteDownloadOutputsMode::All)
        } else {
            self.remote_download_outputs
        }
    }
}

fn apply_daemon_startup_config_overrides(
    mut daemon_startup_config: DaemonStartupConfig,
    watchfs_override: Option<bool>,
    remote_execution_startup_config: &RemoteExecutionStartupConfig,
    remote_download_outputs_override: Option<RemoteDownloadOutputsMode>,
) -> DaemonStartupConfig {
    if let Some(watchfs) = watchfs_override {
        daemon_startup_config.watchfs = watchfs;
    }
    if let Some(remote_download_outputs) = remote_download_outputs_override {
        daemon_startup_config.remote_download_outputs = remote_download_outputs;
    }
    daemon_startup_config
        .remote_execution
        .apply_overrides(remote_execution_startup_config);
    daemon_startup_config
}

#[rustfmt::skip] // Formatting in internal and in OSS versions disagree after oss markers applied.
fn help() -> &'static str {
    concat!(
        "A build system\n",
        "\n",
        "Documentation: https://bzcli.com/docs/\n",
    )
}

#[derive(Debug, clap::Parser)]
#[clap(
    name = "bz",
    about(Some(help())),
    version(BuckVersion::get_version_for_clap()),
    styles = cli_style::get_styles(),
)]
pub(crate) struct Opt {
    #[clap(subcommand)]
    cmd: CommandKind,
    #[clap(flatten)]
    common_opts: BeforeSubcommandOptions,
}

impl Opt {
    pub(crate) fn exec(
        self,
        process: ProcessContext<'_>,
        immediate_config: &ImmediateConfigContext,
        matches: BuckArgMatches<'_>,
        argv: Argv,
    ) -> ExitResult {
        let subcommand_matches = matches.unwrap_subcommand();

        self.cmd.exec(
            process,
            immediate_config,
            subcommand_matches,
            argv,
            self.common_opts,
        )
    }
}

pub fn exec(process: ProcessContext<'_>) -> ExitResult {
    let cwd = process.shared.working_dir.clone();
    let mut immediate_config = ImmediateConfigContext::new(&cwd);
    let arg0_override = bz_env!("BUCK2_ARG0")?;
    let expanded_args = expand_argv(
        arg0_override,
        process.shared.args.to_vec(),
        &mut immediate_config,
        &cwd,
    )
    .buck_error_context("Error expanding argsfiles")?;

    let argv = Argv {
        argv: process.shared.args.to_vec(),
        expanded_argv: expanded_args,
    };

    let clap = Opt::command();
    let matches = match clap.try_get_matches_from(argv.expanded_argv.args()) {
        Ok(matches) => matches,
        Err(e) => {
            // Print colorized output, ExitResult::report will not colorize
            e.print()?;
            return if e.exit_code() == 0 {
                ExitResult::success()
            } else {
                let e = bz_error::Error::from(e).tag([ErrorTag::ClapMatch]);
                ExitResult::status_with_emitted_errors(
                    ExitCode::UserError,
                    vec![ErrorReport::from(&e)],
                )
            };
        }
    };
    let mut opt = ParsedArgv::parse(argv, matches)?;

    let client_metadata = ClientMetadata::from_env()?;
    if !client_metadata.is_empty() {
        // insert the `client_metadata` at the beginning of the list, so that the client id metadata from the env var could be overridden by the cli arg
        opt.opt
            .common_opts
            .client_metadata
            .splice(0..0, client_metadata);
    }

    // If --client-metadata=? was not set and from_env did not find "id", then
    // if we are running in a terminal, we add id=terminal-fallback to
    // opt.opt.common_opts.client_metadata to indicate that the client is an end user.
    let has_client_id = opt
        .opt
        .common_opts
        .client_metadata
        .iter()
        .any(|m| m.key == "id");

    if !has_client_id {
        use std::io::IsTerminal;
        let client_id = if std::io::stdin().is_terminal() {
            Some("terminal-fallback")
        } else {
            // Check if running from VSCode
            let is_vscode = std::env::var("VSCODE_PID")
                .ok()
                .is_some_and(|v| !v.is_empty())
                || std::env::var("TERM_PROGRAM").ok().as_deref() == Some("vscode");
            if is_vscode {
                Some("vscode-fallback")
            } else {
                None
            }
        };

        if let Some(val) = client_id {
            opt.opt.common_opts.client_metadata.push(ClientMetadata {
                key: "id".to_owned(),
                value: val.to_owned(),
            });
        }
    }
    opt.exec(process, &immediate_config)
}

struct ParsedArgv {
    opt: Opt,
    argv: Argv,
    matches: clap::ArgMatches,
}

impl ParsedArgv {
    fn parse(argv: Argv, matches: clap::ArgMatches) -> bz_error::Result<Self> {
        let opt: Opt = Opt::from_arg_matches(&matches)?;

        if opt.common_opts.help_wrapper {
            return Err(bz_error!(
                bz_error::ErrorTag::Tier0,
                "`--help-wrapper` should have been handled by the wrapper"
            ));
        }

        match &opt.cmd {
            #[cfg(not(client_only))]
            CommandKind::Daemon(..) | CommandKind::Forkserver(..) => {}
            CommandKind::Clean(..) => {}
            _ => {
                check_user_allowed()?;
            }
        }

        Ok(ParsedArgv { opt, argv, matches })
    }

    fn exec(
        self,
        process: ProcessContext<'_>,
        immediate_config: &ImmediateConfigContext,
    ) -> ExitResult {
        let expanded_args = self.argv.expanded_argv.clone();
        self.opt.exec(
            process,
            immediate_config,
            BuckArgMatches::from_clap(&self.matches, &expanded_args),
            self.argv,
        )
    }
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum CommandKind {
    #[cfg(not(client_only))]
    #[clap(hide = true)]
    Daemon(bz_daemon::daemon::DaemonCommand),
    #[cfg(not(client_only))]
    #[clap(hide = true)]
    Forkserver(crate::commands::forkserver::ForkserverCommand),
    #[cfg(not(client_only))]
    #[clap(hide = true)]
    InternalTestRunner(crate::commands::internal_test_runner::InternalTestRunnerCommand),
    #[clap(subcommand)]
    Audit(AuditCommand),
    Aquery(AqueryCommand),
    Build(BuildCommand),
    Bxl(BxlCommand),
    // TODO(nga): implement `bz help-buckconfig` too.
    HelpEnv(HelpEnvCommand),
    Test(TestCommand),
    Cquery(CqueryCommand),
    Init(InitCommand),
    Explain(ExplainCommand),
    ExpandExternalCell(ExpandExternalCellsCommand),
    Install(InstallCommand),
    Kill(KillCommand),
    Killall(KillallCommand),
    Root(RootCommand),
    /// Alias for `uquery`.
    Query(UqueryCommand),
    Run(RunCommand),
    Server(ServerCommand),
    Status(StatusCommand),
    #[clap(subcommand)]
    Starlark(StarlarkCommand),
    /// Alias for `utargets`.
    Targets(TargetsCommand),
    Utargets(TargetsCommand),
    Ctargets(ConfiguredTargetsCommand),
    Uquery(UqueryCommand),
    #[clap(subcommand, hide = true)]
    Debug(DebugCommand),
    #[clap(hide = true)]
    Complete(bz_cmd_completion_client::complete::CompleteCommand),
    Completion(bz_cmd_completion_client::completion::CompletionCommand),
    Docs(bz_cmd_docs_client::DocsCommand),
    #[clap(subcommand)]
    Profile(ProfileCommand),
    #[clap(hide(true))]
    Rage(RageCommand),
    Clean(CleanCommand),
    #[clap(subcommand)]
    Log(LogCommand),
    Lsp(LspCommand),
    Subscribe(SubscribeCommand),
}

impl CommandKind {
    pub(crate) fn exec(
        self,
        process: ProcessContext<'_>,
        immediate_config: &ImmediateConfigContext,
        matches: BuckArgMatches<'_>,
        argv: Argv,
        common_opts: BeforeSubcommandOptions,
    ) -> ExitResult {
        let paths_result = get_invocation_paths_result(
            &process.shared.working_dir,
            common_opts.isolation_dir.clone(),
        );

        // Handle the daemon command earlier: it wants to fork, but the things we do below might
        // want to create threads.
        #[cfg(not(client_only))]
        if let CommandKind::Daemon(cmd) = self {
            process.events_ctx.log_invocation_record = false;
            return cmd
                .exec(
                    process.shared.log_reload_handle.dupe(),
                    paths_result.get_result()?,
                    false,
                    || {},
                )
                .into();
        }
        thread::scope(|scope| {
            // Spawn a thread to have stack size independent on linker/environment.
            match thread_spawn_scoped("buck2-main", scope, move || {
                self.exec_no_daemon(
                    common_opts,
                    process,
                    immediate_config,
                    matches,
                    argv,
                    paths_result,
                )
            }) {
                Ok(t) => match t.join() {
                    Ok(res) => res,
                    Err(_) => ExitResult::bail("Main thread panicked"),
                },
                Err(e) => ExitResult::bail(format_args!("Failed to start main thread: {e}")),
            }
        })
    }

    fn exec_no_daemon(
        self,
        common_opts: BeforeSubcommandOptions,
        process: ProcessContext<'_>,
        immediate_config: &ImmediateConfigContext,
        matches: BuckArgMatches<'_>,
        argv: Argv,
        paths: InvocationPathsResult,
    ) -> ExitResult {
        if common_opts.no_buckd {
            // `no_buckd` can't work in a client-only binary
            if let Some(res) = ExitResult::retry_command_with_full_binary()? {
                return res;
            }
        }

        let fb = bz_common::fbinit::get_or_init_build_globals();

        let ProcessContext {
            trace_id,
            events_ctx,
            shared,
            runtime,
            start_time,
        } = process;

        let runtime = runtime.get_or_init()?;
        let watchfs_override = common_opts.watchfs_override();
        let remote_execution_startup_config = common_opts.remote_execution_startup_config();
        let remote_download_outputs_override = common_opts.remote_download_outputs_override();
        let buildbuddy_bes = common_opts.buildbuddy_bes();
        let dev = common_opts.dev;
        let rbe_implies_remote_only = common_opts.rbe || common_opts.buildbuddy;

        let start_in_process_daemon = if common_opts.no_buckd {
            #[cfg(not(client_only))]
            let daemon_startup_config = apply_daemon_startup_config_overrides(
                immediate_config.daemon_startup_config()?.clone(),
                watchfs_override,
                &remote_execution_startup_config,
                remote_download_outputs_override,
            );
            #[cfg(not(client_only))]
            let v = bz_daemon::no_buckd::start_in_process_daemon(
                &daemon_startup_config,
                paths.clone().get_result()?,
                runtime,
            )?;
            #[cfg(client_only)]
            let v = unreachable!(); // case covered above
            #[allow(dead_code)]
            v
        } else {
            None
        };

        let command_ctx = ClientCommandContext::new(
            fb,
            immediate_config,
            paths,
            shared.working_dir.clone(),
            common_opts.verbosity,
            start_time,
            start_in_process_daemon,
            argv,
            trace_id.dupe(),
            &mut shared.stdin,
            &mut shared.restarter,
            runtime,
            common_opts.oncall,
            common_opts.client_metadata,
            common_opts.isolation_dir,
            common_opts.agent_context,
            watchfs_override,
            remote_execution_startup_config,
            remote_download_outputs_override,
            buildbuddy_bes,
            dev,
            rbe_implies_remote_only,
        );
        if let Some(recorder) = events_ctx.recorder.as_mut() {
            recorder.update_for_client_ctx(&command_ctx, self.command_name());
        }

        match self {
            #[cfg(not(client_only))]
            CommandKind::Daemon(..) => unreachable!("Checked earlier"),
            #[cfg(not(client_only))]
            CommandKind::Forkserver(cmd) => cmd.exec(
                matches,
                command_ctx,
                events_ctx,
                shared.log_reload_handle.dupe(),
            ),
            #[cfg(not(client_only))]
            CommandKind::InternalTestRunner(cmd) => cmd.exec(matches, command_ctx, events_ctx),
            CommandKind::Aquery(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Build(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Bxl(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Test(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Cquery(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::HelpEnv(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Kill(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Killall(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Clean(cmd) => cmd.exec(matches, command_ctx, events_ctx),
            CommandKind::Root(cmd) => cmd.exec(matches, command_ctx).into(),
            CommandKind::Query(cmd) => {
                bz_client_ctx::eprintln!(
                    "WARNING: \"bz query\" is an alias for \"bz uquery\". Consider using \"bz cquery\" or \"bz uquery\" explicitly."
                )?;
                command_ctx.exec(cmd, matches, events_ctx)
            }
            CommandKind::Server(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Status(cmd) => cmd.exec(matches, command_ctx).into(),
            CommandKind::Targets(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Utargets(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Ctargets(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Audit(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Starlark(cmd) => cmd.exec(matches, command_ctx, events_ctx),
            CommandKind::Run(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Uquery(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Debug(cmd) => cmd.exec(matches, command_ctx, events_ctx),
            CommandKind::Complete(cmd) => cmd.exec(matches, command_ctx, events_ctx),
            CommandKind::Completion(cmd) => cmd.exec(Opt::command(), matches, command_ctx),
            CommandKind::Docs(cmd) => cmd.exec(Opt::command(), matches, command_ctx, events_ctx),
            CommandKind::Profile(cmd) => cmd.exec(matches, command_ctx, events_ctx),
            CommandKind::Rage(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Init(cmd) => cmd.exec(matches, command_ctx),
            CommandKind::Explain(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Install(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Log(cmd) => cmd.exec(matches, command_ctx, events_ctx),
            CommandKind::Lsp(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::Subscribe(cmd) => command_ctx.exec(cmd, matches, events_ctx),
            CommandKind::ExpandExternalCell(cmd) => command_ctx.exec(cmd, matches, events_ctx),
        }
    }

    fn command_name(&self) -> &'static str {
        match self {
            #[cfg(not(client_only))]
            CommandKind::Daemon(_) => "daemon",
            #[cfg(not(client_only))]
            CommandKind::Forkserver(_) => "forkserver",
            #[cfg(not(client_only))]
            CommandKind::InternalTestRunner(_) => "internal-test-runner",
            CommandKind::Aquery(cmd) => cmd.logging_name(),
            CommandKind::Build(cmd) => cmd.logging_name(),
            CommandKind::Bxl(cmd) => cmd.logging_name(),
            CommandKind::Test(cmd) => cmd.logging_name(),
            CommandKind::Cquery(cmd) => cmd.logging_name(),
            CommandKind::HelpEnv(_) => "help-env",
            CommandKind::Kill(cmd) => cmd.logging_name(),
            CommandKind::Killall(cmd) => cmd.logging_name(),
            CommandKind::Clean(cmd) => cmd.command_name(),
            CommandKind::Root(_) => "root",
            CommandKind::Query(cmd) => cmd.logging_name(),
            CommandKind::Server(cmd) => cmd.logging_name(),
            CommandKind::Status(_) => "status",
            CommandKind::Targets(cmd) => cmd.logging_name(),
            CommandKind::Utargets(cmd) => cmd.logging_name(),
            CommandKind::Ctargets(cmd) => cmd.logging_name(),
            CommandKind::Audit(cmd) => cmd.logging_name(),
            CommandKind::Starlark(cmd) => cmd.command_name(),
            CommandKind::Run(cmd) => cmd.logging_name(),
            CommandKind::Uquery(cmd) => cmd.logging_name(),
            CommandKind::Debug(_) => "debug",
            CommandKind::Complete(_) => "complete",
            CommandKind::Completion(_) => "completion",
            CommandKind::Docs(_) => "docs",
            CommandKind::Profile(_) => "profile",
            CommandKind::Rage(_) => "rage",
            CommandKind::Init(_) => "init",
            CommandKind::Explain(cmd) => cmd.logging_name(),
            CommandKind::Install(cmd) => cmd.logging_name(),
            CommandKind::Log(cmd) => cmd.command_name(),
            CommandKind::Lsp(cmd) => cmd.logging_name(),
            CommandKind::Subscribe(cmd) => cmd.logging_name(),
            CommandKind::ExpandExternalCell(cmd) => cmd.logging_name(),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_bazel_remote_flags_after_subcommand() {
        let opts = Opt::try_parse_from([
            "buck2",
            "build",
            "--remote_cache=grpc://cache.example.com",
            "--remote_executor=grpcs://executor.example.com",
            "//:target",
        ])
        .unwrap();

        assert_eq!(
            opts.common_opts.remote_cache.as_deref(),
            Some("grpc://cache.example.com")
        );
        assert_eq!(
            opts.common_opts.remote_executor.as_deref(),
            Some("grpcs://executor.example.com")
        );
    }

    #[test]
    fn remote_flags_are_daemon_startup_overrides() {
        let opts = Opt::try_parse_from([
            "buck2",
            "--remote_cache=cache.example.com",
            "build",
            "--remote_executor=executor.example.com",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some("cache.example.com")
        );
        assert_eq!(
            remote_execution.remote_executor.as_deref(),
            Some("executor.example.com")
        );
    }

    #[test]
    fn rbe_sets_buildbuddy_remote_endpoints() {
        let opts = Opt::try_parse_from(["buck2", "build", "--rbe", "//:target"]).unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
        assert_eq!(
            remote_execution.remote_executor.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
        assert_eq!(
            remote_execution.remote_downloader.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
        assert_eq!(
            remote_execution.remote_default_exec_properties.as_deref(),
            Some(
                [
                    RemoteDefaultExecProperty {
                        name: "OSFamily".to_owned(),
                        value: "Linux".to_owned(),
                    },
                    RemoteDefaultExecProperty {
                        name: "container-image".to_owned(),
                        value: BUILDBUDDY_DEFAULT_RBE_CONTAINER_IMAGE.to_owned(),
                    },
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn rbe_allows_explicit_remote_overrides() {
        let opts = Opt::try_parse_from([
            "buck2",
            "build",
            "--rbe",
            "--remote_cache=grpc://cache.example.com",
            "--remote_executor=grpc://executor.example.com",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some("grpc://cache.example.com")
        );
        assert_eq!(
            remote_execution.remote_executor.as_deref(),
            Some("grpc://executor.example.com")
        );
        assert_eq!(
            remote_execution.remote_downloader.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
    }

    #[test]
    fn cache_sets_buildbuddy_remote_cache_only() {
        let opts = Opt::try_parse_from(["buck2", "build", "--cache", "//:target"]).unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
        assert_eq!(remote_execution.remote_executor, None);
        assert_eq!(remote_execution.remote_default_exec_properties, None);
    }

    #[test]
    fn parses_remote_default_exec_properties() {
        let opts = Opt::try_parse_from([
            "buck2",
            "build",
            "--remote_default_exec_properties=OSFamily=Linux",
            "--remote_default_exec_properties=container-image=docker://example/image",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_default_exec_properties.as_deref(),
            Some(
                [
                    RemoteDefaultExecProperty {
                        name: "OSFamily".to_owned(),
                        value: "Linux".to_owned(),
                    },
                    RemoteDefaultExecProperty {
                        name: "container-image".to_owned(),
                        value: "docker://example/image".to_owned(),
                    },
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn explicit_remote_default_exec_properties_replace_rbe_defaults() {
        let opts = Opt::try_parse_from([
            "buck2",
            "build",
            "--rbe",
            "--remote_default_exec_properties=container-image=docker://custom",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_default_exec_properties.as_deref(),
            Some(
                [RemoteDefaultExecProperty {
                    name: "container-image".to_owned(),
                    value: "docker://custom".to_owned(),
                }]
                .as_slice()
            )
        );
    }

    #[test]
    fn cache_allows_explicit_remote_cache_override() {
        let opts = Opt::try_parse_from([
            "buck2",
            "--cache",
            "build",
            "--remote_cache=grpc://cache.example.com",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some("grpc://cache.example.com")
        );
        assert_eq!(remote_execution.remote_executor, None);
    }

    #[test]
    fn bb_sets_buildbuddy_bes_and_remote_endpoints() {
        let opts = Opt::try_parse_from(["buck2", "build", "--bb", "//:target"]).unwrap();

        assert!(opts.common_opts.buildbuddy_bes());
        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
        assert_eq!(
            remote_execution.remote_executor.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
        assert_eq!(
            remote_execution.remote_downloader.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
        assert!(!remote_execution.experimental_remote_repo_contents_cache);
    }

    #[test]
    fn buildbuddy_sets_buildbuddy_bes_and_remote_endpoints() {
        let opts = Opt::try_parse_from(["buck2", "--buildbuddy", "build", "//:target"]).unwrap();

        assert!(opts.common_opts.buildbuddy_bes());
        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
        assert_eq!(
            remote_execution.remote_executor.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
        assert_eq!(
            remote_execution.remote_downloader.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
    }

    #[test]
    fn buildbuddy_remote_endpoints_default_to_production() {
        let opts = Opt::try_parse_from(["buck2", "build", "--bb", "//:target"]).unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some("remote.buildbuddy.io")
        );
        assert_eq!(
            remote_execution.remote_executor.as_deref(),
            Some("remote.buildbuddy.io")
        );
    }

    #[test]
    fn dev_flag_points_buildbuddy_remote_endpoints_at_dev() {
        let opts = Opt::try_parse_from(["buck2", "build", "--bb", "--dev", "//:target"]).unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some("remote.buildbuddy.dev")
        );
        assert_eq!(
            remote_execution.remote_executor.as_deref(),
            Some("remote.buildbuddy.dev")
        );
        assert_eq!(
            remote_execution.remote_downloader.as_deref(),
            Some("remote.buildbuddy.dev")
        );
    }

    #[test]
    fn experimental_remote_repo_contents_cache_is_global_startup_override() {
        let opts = Opt::try_parse_from([
            "buck2",
            "--experimental_remote_repo_contents_cache",
            "build",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert!(remote_execution.experimental_remote_repo_contents_cache);
    }

    #[test]
    fn noexperimental_remote_repo_contents_cache_is_bazel_compatible() {
        let opts = Opt::try_parse_from([
            "buck2",
            "--noexperimental_remote_repo_contents_cache",
            "build",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert!(!remote_execution.experimental_remote_repo_contents_cache);
    }

    #[test]
    fn buildbuddy_allows_explicit_remote_overrides() {
        let opts = Opt::try_parse_from([
            "buck2",
            "build",
            "--bb",
            "--remote_cache=grpc://cache.example.com",
            "--remote_executor=grpc://executor.example.com",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_cache.as_deref(),
            Some("grpc://cache.example.com")
        );
        assert_eq!(
            remote_execution.remote_executor.as_deref(),
            Some("grpc://executor.example.com")
        );
        assert_eq!(
            remote_execution.remote_downloader.as_deref(),
            Some(BUILDBUDDY_REMOTE_ENDPOINT)
        );
    }

    #[test]
    fn remote_downloader_flag_is_global_startup_override() {
        let opts = Opt::try_parse_from([
            "buck2",
            "--experimental_remote_downloader=grpc://downloader.example.com",
            "build",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_downloader.as_deref(),
            Some("grpc://downloader.example.com")
        );
    }

    #[test]
    fn api_key_sets_buildbuddy_api_key_startup_override() {
        let opts =
            Opt::try_parse_from(["buck2", "build", "--api-key=secret", "//:target"]).unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.buildbuddy_api_key.as_deref(),
            Some("secret")
        );
        assert_eq!(remote_execution.remote_cache, None);
        assert_eq!(remote_execution.remote_executor, None);
    }

    #[test]
    fn api_key_is_global() {
        let opts =
            Opt::try_parse_from(["buck2", "--api-key", "secret", "build", "//:target"]).unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.buildbuddy_api_key.as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn api_key_can_come_from_env() {
        let opts = Opt::try_parse_from(["buck2", "build", "//:target"]).unwrap();

        let remote_execution = opts
            .common_opts
            .remote_execution_startup_config_with_buildbuddy_api_key_env(Some(
                "from-env".to_owned(),
            ));
        assert_eq!(
            remote_execution.buildbuddy_api_key.as_deref(),
            Some("from-env")
        );
    }

    #[test]
    fn api_key_can_come_from_bz_env_name() {
        assert_eq!(
            buildbuddy_api_key_from_env_vars(|env_var| {
                (env_var == BZ_BUILDBUDDY_API_KEY_ENV_VAR).then(|| "from-bz-env".to_owned())
            })
            .as_deref(),
            Some("from-bz-env")
        );
    }

    #[test]
    fn buildbuddy_api_key_env_takes_precedence_over_bz_env_name() {
        assert_eq!(
            buildbuddy_api_key_from_env_vars(|env_var| match env_var {
                BUILDBUDDY_API_KEY_ENV_VAR => Some("from-buildbuddy-env".to_owned()),
                BZ_BUILDBUDDY_API_KEY_ENV_VAR => Some("from-bz-env".to_owned()),
                _ => None,
            })
            .as_deref(),
            Some("from-buildbuddy-env")
        );
    }

    #[test]
    fn api_key_flag_overrides_api_key_env() {
        let opts =
            Opt::try_parse_from(["buck2", "build", "--api-key=from-cli", "//:target"]).unwrap();

        let remote_execution = opts
            .common_opts
            .remote_execution_startup_config_with_buildbuddy_api_key_env(Some(
                "from-env".to_owned(),
            ));
        assert_eq!(
            remote_execution.buildbuddy_api_key.as_deref(),
            Some("from-cli")
        );
    }

    #[test]
    fn empty_api_key_env_is_ignored() {
        let opts = Opt::try_parse_from(["buck2", "build", "//:target"]).unwrap();

        let remote_execution = opts
            .common_opts
            .remote_execution_startup_config_with_buildbuddy_api_key_env(Some(" \t".to_owned()));
        assert_eq!(remote_execution.buildbuddy_api_key, None);
    }

    #[test]
    fn remote_connection_limits_are_global_startup_overrides() {
        let opts = Opt::try_parse_from([
            "buck2",
            "--remote_max_connections=12",
            "build",
            "--remote_max_concurrency_per_connection=34",
            "//:target",
        ])
        .unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(remote_execution.remote_max_connections, Some(12));
        assert_eq!(
            remote_execution.remote_max_concurrency_per_connection,
            Some(34)
        );
    }

    #[test]
    fn buildbuddy_sets_bazel_aligned_remote_timeout() {
        let opts = Opt::try_parse_from(["buck2", "--bb", "build", "//:target"]).unwrap();

        let remote_execution = opts.common_opts.remote_execution_startup_config();
        assert_eq!(
            remote_execution.remote_timeout_secs,
            Some(BUILDBUDDY_REMOTE_TIMEOUT_SECS)
        );
    }

    #[test]
    fn remote_download_outputs_is_global_startup_override() {
        let opts = Opt::try_parse_from([
            "buck2",
            "build",
            "--remote_download_outputs=minimal",
            "//:target",
        ])
        .unwrap();

        assert_eq!(
            opts.common_opts.remote_download_outputs_override(),
            Some(RemoteDownloadOutputsMode::Minimal)
        );
    }

    #[test]
    fn remote_download_aliases_set_startup_override() {
        let opts =
            Opt::try_parse_from(["buck2", "--remote_download_all", "build", "//:target"]).unwrap();

        assert_eq!(
            opts.common_opts.remote_download_outputs_override(),
            Some(RemoteDownloadOutputsMode::All)
        );
    }
}
