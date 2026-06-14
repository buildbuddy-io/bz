/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::future::Future;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use bz_cli_proto::BepTerminalOutputResponse;
use bz_cli_proto::BesOptions;
use bz_cli_proto::ClientContext;
use bz_cli_proto::ClientEnvironmentVariable;
use bz_cli_proto::client_context::ExitWhen as GrpcExitWhen;
use bz_cli_proto::client_context::HostArchOverride as GrpcHostArchOverride;
use bz_cli_proto::client_context::HostPlatformOverride as GrpcHostPlatformOverride;
use bz_cli_proto::client_context::PreemptibleWhen as GrpcPreemptibleWhen;
use bz_common::argv::Argv;
use bz_common::bazel::bzlmod::BZLMOD_ALLOWED_YANKED_VERSIONS_ENV;
use bz_common::init::BUILDBUDDY_API_KEY_HEADER;
use bz_common::init::DaemonStartupConfig;
use bz_common::init::LogDownloadMethod;
use bz_common::init::RemoteDownloadOutputsMode;
use bz_common::init::RemoteExecutionStartupConfig;
use bz_common::invocation_paths::InvocationPaths;
use bz_common::invocation_paths_result::InvocationPathsResult;
use bz_core::error::bz_hard_error_env;
use bz_core::error::bz_show_soft_errors_env;
use bz_error::BuckErrorContext;
use bz_event_observer::verbosity::Verbosity;
use bz_fs::paths::file_name::FileNameBuf;
use bz_fs::working_dir::AbsWorkingDir;
use bz_wrapper_common::invocation_id::TraceId;
use dupe::Dupe;
use superconsole::Stdin;
use tokio::runtime::Runtime;

use crate::agent_context::AgentContextEntry;
use crate::client_metadata::ClientMetadata;
use crate::common::BuckArgMatches;
use crate::common::CommonEventLogOptions;
use crate::common::ExitWhen;
use crate::common::HostArchOverride;
use crate::common::HostPlatformOverride;
use crate::common::PreemptibleWhen;
use crate::common::ui::CommonConsoleOptions;
use crate::console_interaction_stream::ConsoleInteractionStream;
use crate::daemon_constraints::get_possibly_nested_invocation_daemon_uuid;
use crate::events_ctx::EventsCtx;
use crate::exit_result::ExitResult;
use crate::immediate_config::ImmediateConfigContext;
use crate::restarter::Restarter;
use crate::stdio::OutputEvent;
use crate::stdio::OutputTapGuard;
use crate::stdio::install_output_tap;
use crate::streaming::StreamingCommand;

pub struct ClientCommandContext<'a> {
    init: fbinit::FacebookInit,
    pub immediate_config: &'a ImmediateConfigContext<'a>,
    paths: InvocationPathsResult,
    pub working_dir: AbsWorkingDir,
    pub verbosity: Verbosity,
    pub start_time: SystemTime,
    /// When set, this function is called to launch in process daemon.
    /// The function returns `Ok` when daemon successfully started
    /// and ready to accept connections.
    pub(crate) start_in_process_daemon:
        Option<Box<dyn FnOnce() -> bz_error::Result<()> + Send + Sync>>,
    pub(crate) argv: Argv,
    pub trace_id: TraceId,
    stdin: &'a mut Stdin,
    pub(crate) restarter: &'a mut Restarter,
    runtime: &'a Runtime,
    oncall: Option<String>,
    pub(crate) client_metadata: Vec<ClientMetadata>,
    pub(crate) isolation: FileNameBuf,
    pub(crate) agent_context: Vec<AgentContextEntry>,
    pub(crate) watchfs_override: Option<bool>,
    pub(crate) remote_download_outputs_override: Option<RemoteDownloadOutputsMode>,
    pub(crate) remote_execution_startup_config: RemoteExecutionStartupConfig,
    pub(crate) buildbuddy_bes: bool,
    rbe_implies_remote_only: bool,
    bep_output_rx: Option<tokio::sync::mpsc::UnboundedReceiver<OutputEvent>>,
    bep_output_tap_guard: Option<OutputTapGuard>,
    bep_output_forwarder:
        Option<tokio::task::JoinHandle<bz_error::Result<BepTerminalOutputResponse>>>,
}

impl<'a> ClientCommandContext<'a> {
    pub fn new(
        init: fbinit::FacebookInit,
        immediate_config: &'a ImmediateConfigContext<'a>,
        paths: InvocationPathsResult,
        working_dir: AbsWorkingDir,
        verbosity: Verbosity,
        start_time: SystemTime,
        start_in_process_daemon: Option<Box<dyn FnOnce() -> bz_error::Result<()> + Send + Sync>>,
        argv: Argv,
        trace_id: TraceId,
        stdin: &'a mut Stdin,
        restarter: &'a mut Restarter,
        runtime: &'a Runtime,
        oncall: Option<String>,
        client_metadata: Vec<ClientMetadata>,
        isolation: FileNameBuf,
        agent_context: Vec<AgentContextEntry>,
        watchfs_override: Option<bool>,
        remote_execution_startup_config: RemoteExecutionStartupConfig,
        remote_download_outputs_override: Option<RemoteDownloadOutputsMode>,
        buildbuddy_bes: bool,
        rbe_implies_remote_only: bool,
    ) -> Self {
        ClientCommandContext {
            init,
            immediate_config,
            paths,
            working_dir,
            verbosity,
            start_time,
            start_in_process_daemon,
            argv,
            trace_id,
            stdin,
            restarter,
            runtime,
            oncall,
            client_metadata,
            isolation,
            agent_context,
            watchfs_override,
            remote_download_outputs_override,
            remote_execution_startup_config,
            buildbuddy_bes,
            rbe_implies_remote_only,
            bep_output_rx: None,
            bep_output_tap_guard: None,
            bep_output_forwarder: None,
        }
    }

    pub fn rbe_implies_remote_only(&self) -> bool {
        self.rbe_implies_remote_only
    }

    /// Check whether the expanded argv (after flagfile expansion) contains a `--` separator.
    pub fn expanded_argv_has_separator(&self) -> bool {
        self.argv.expanded_argv.args().any(|arg| arg == "--")
    }

    pub fn fbinit(&self) -> fbinit::FacebookInit {
        self.init
    }

    pub fn paths(&self) -> bz_error::Result<&InvocationPaths> {
        match &self.paths {
            InvocationPathsResult::Paths(p) => Ok(p),
            InvocationPathsResult::OutsideOfRepo(e) | InvocationPathsResult::OtherError(e) => {
                Err(e.dupe())
            }
        }
    }

    pub fn maybe_paths(&self) -> bz_error::Result<Option<&InvocationPaths>> {
        match &self.paths {
            InvocationPathsResult::Paths(p) => Ok(Some(p)),
            InvocationPathsResult::OutsideOfRepo(_) => Ok(None), // commands like log don't need a root but still need to create an invocation record
            InvocationPathsResult::OtherError(e) => Err(e.dupe()),
        }
    }

    pub fn with_runtime<Fut, F>(self, func: F) -> <Fut as Future>::Output
    where
        Fut: Future + 'a,
        F: FnOnce(ClientCommandContext<'a>) -> Fut,
    {
        self.runtime.block_on(func(self))
    }

    pub fn exec<T: BuckSubcommand>(
        self,
        cmd: T,
        matches: BuckArgMatches<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        self.with_runtime(|ctx| ctx.exec_async(cmd, matches, events_ctx))
    }

    // Handles setting up subscribers, executing a command and finalizing logging.
    pub async fn exec_async<T: BuckSubcommand>(
        mut self,
        cmd: T,
        matches: BuckArgMatches<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        self.install_bep_output_tap_if_needed(&cmd);
        if let Err(error) = cmd.update_events_ctx(matches, &self, events_ctx) {
            return ExitResult::err(error);
        }
        events_ctx.buck_log_dir = self.paths().map(|paths| paths.log_dir()).ok();
        events_ctx.command_report_path = cmd
            .event_log_opts()
            .command_report_path
            .as_ref()
            .map(|path| path.resolve(&self.working_dir));
        cmd.exec_impl(matches, self, events_ctx).await
    }

    fn install_bep_output_tap_if_needed<T: BuckSubcommand>(&mut self, cmd: &T) {
        if cmd
            .event_log_opts()
            .bes_backend_with_buildbuddy_default(self.buildbuddy_bes())
            .is_none()
        {
            return;
        }

        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.bep_output_tap_guard = Some(install_output_tap(sender));
        self.bep_output_rx = Some(receiver);
    }

    pub(crate) fn take_bep_output_rx(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<OutputEvent>> {
        self.bep_output_rx.take()
    }

    pub(crate) fn set_bep_output_forwarder(
        &mut self,
        forwarder: tokio::task::JoinHandle<bz_error::Result<BepTerminalOutputResponse>>,
    ) {
        self.bep_output_forwarder = Some(forwarder);
    }

    pub(crate) async fn finish_bep_output_forwarder(&mut self) {
        self.bep_output_tap_guard.take();

        let Some(forwarder) = self.bep_output_forwarder.take() else {
            return;
        };

        match forwarder.await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!("BEP terminal output forwarding failed: {error:#}");
            }
            Err(error) => {
                tracing::warn!("BEP terminal output forwarding task failed: {error:#}");
            }
        }
    }

    pub fn stdin(&mut self) -> &mut Stdin {
        self.stdin
    }

    pub fn console_interaction_stream(
        &mut self,
        opts: &CommonConsoleOptions,
    ) -> Option<ConsoleInteractionStream<'_>> {
        if opts.no_interactive_console {
            tracing::debug!("Disabling console interaction: no_interactive_console is set");
            return None;
        }

        ConsoleInteractionStream::new(self.stdin)
    }

    pub fn client_context<T: StreamingCommand>(
        &self,
        arg_matches: BuckArgMatches<'_>,
        cmd: &T,
    ) -> bz_error::Result<ClientContext> {
        // TODO(cjhopman): Support non unicode paths?
        let config_opts = cmd.build_config_opts();
        let starlark_opts = cmd.starlark_opts();

        Ok(ClientContext {
            config_overrides: config_opts.config_overrides(
                arg_matches,
                self.immediate_config,
                &self.working_dir,
            )?,
            host_platform: match config_opts.host_platform_override() {
                HostPlatformOverride::Default => GrpcHostPlatformOverride::DefaultPlatform,
                HostPlatformOverride::Linux => GrpcHostPlatformOverride::Linux,
                HostPlatformOverride::MacOs => GrpcHostPlatformOverride::MacOs,
                HostPlatformOverride::Windows => GrpcHostPlatformOverride::Windows,
            }
            .into(),
            host_arch: match config_opts.host_arch_override() {
                HostArchOverride::Default => GrpcHostArchOverride::DefaultArch,
                HostArchOverride::X86_64 => GrpcHostArchOverride::X8664,
                HostArchOverride::AArch64 => GrpcHostArchOverride::AArch64,
            }
            .into(),
            host_xcode_version: config_opts.host_xcode_version_override(),
            disable_starlark_types: starlark_opts.disable_starlark_types,
            unstable_typecheck: starlark_opts.unstable_typecheck,
            skip_targets_with_duplicate_names: starlark_opts.skip_targets_with_duplicate_names,
            reuse_current_config: config_opts.reuse_current_config,
            sanitized_argv: cmd
                .sanitize_argv(self.argv.clone())
                .redacted_arg_values(&["--api-key"])
                .argv,
            preemptible: match config_opts.preemptible {
                None => GrpcPreemptibleWhen::Never,
                Some(PreemptibleWhen::Never) => GrpcPreemptibleWhen::Never,
                Some(PreemptibleWhen::Always) => GrpcPreemptibleWhen::Always,
                Some(PreemptibleWhen::OnDifferentState) => GrpcPreemptibleWhen::OnDifferentState,
            }
            .into(),
            argfiles: self
                .immediate_config
                .trace()
                .iter()
                .map(|path| path.to_string())
                .collect(),
            target_call_stacks: starlark_opts.target_call_stacks,
            representative_config_flags: arg_matches.get_representative_config_flags_by_source(),
            exit_when: match config_opts.exit_when {
                None => GrpcExitWhen::ExitNever,
                Some(ExitWhen::Never) => GrpcExitWhen::ExitNever,
                Some(ExitWhen::DifferentState) => GrpcExitWhen::ExitDifferentState,
                Some(ExitWhen::NotIdle) => GrpcExitWhen::ExitNotIdle,
            }
            .into(),
            profile_pattern_opts: starlark_opts.profile_pattern_opts(&self.working_dir),
            bes_options: self.bes_options(cmd)?,
            ..self.empty_client_context(cmd.logging_name())?
        })
    }

    /// A client context for commands where CommonConfigOptions are not provided.
    pub fn empty_client_context(&self, command_name: &str) -> bz_error::Result<ClientContext> {
        #[derive(Debug, bz_error::Error)]
        #[error("Current directory is not UTF-8")]
        #[buck2(tag = Input)]
        struct CurrentDirIsNotUtf8;

        Ok(ClientContext {
            working_dir: self
                .working_dir
                .path()
                .to_str()
                .buck_error_context(CurrentDirIsNotUtf8.to_string())?
                .to_owned(),
            config_overrides: Default::default(),
            host_platform: Default::default(),
            host_arch: Default::default(),
            host_xcode_version: Default::default(),
            oncall: self.oncall.clone().unwrap_or_default(), // TODO: Why do we not make this optional?
            disable_starlark_types: false,
            unstable_typecheck: false,
            target_call_stacks: false,
            skip_targets_with_duplicate_names: false,
            trace_id: format!("{}", self.trace_id),
            reuse_current_config: false,
            daemon_uuid: get_possibly_nested_invocation_daemon_uuid(),
            sanitized_argv: Vec::new(),
            argfiles: Vec::new(),
            bz_hard_error: bz_hard_error_env()?.unwrap_or_default().to_owned(),
            bz_show_soft_errors: bz_show_soft_errors_env()?.unwrap_or_default().to_owned(),
            command_name: command_name.to_owned(),
            client_metadata: self
                .client_metadata
                .iter()
                .map(ClientMetadata::to_proto)
                .collect(),
            preemptible: Default::default(),
            representative_config_flags: Vec::new(),
            exit_when: Default::default(),
            profile_pattern_opts: None,
            agent_context: self
                .agent_context
                .iter()
                .map(|e| bz_data::AgentContextEntry {
                    key: e.key.clone(),
                    value: e.value.clone(),
                })
                .collect(),
            client_environment: vec![ClientEnvironmentVariable {
                name: BZLMOD_ALLOWED_YANKED_VERSIONS_ENV.to_owned(),
                value: std::env::var(BZLMOD_ALLOWED_YANKED_VERSIONS_ENV).ok(),
            }],
            repo_environment: client_repository_environment(),
            bes_options: None,
        })
    }

    fn bes_options<T: StreamingCommand>(&self, cmd: &T) -> bz_error::Result<Option<BesOptions>> {
        let event_log_opts = cmd.event_log_opts();
        let Some(backend) = event_log_opts
            .bes_backend_with_buildbuddy_default(self.buildbuddy_bes())
            .map(ToOwned::to_owned)
        else {
            return Ok(None);
        };

        let headers = headers_with_buildbuddy_api_key(
            event_log_opts.bes_header.clone(),
            self.remote_execution_startup_config
                .buildbuddy_api_key
                .as_deref(),
            &backend,
            self.remote_execution_startup_config.remote_cache.as_deref(),
        );

        Ok(Some(BesOptions {
            backend,
            headers,
            instance_name: event_log_opts.bes_instance_name.clone().unwrap_or_default(),
            keywords: bes_keywords(T::COMMAND_NAME, &event_log_opts.bes_keywords),
            timeout: event_log_opts
                .bes_timeout_duration()?
                .map(duration_to_proto),
            results_url: event_log_opts
                .bes_results_url_with_buildbuddy_default(self.buildbuddy_bes())
                .map(ToOwned::to_owned),
            target_patterns: cmd.build_event_protocol_target_patterns(),
            sync: event_log_opts.bes_sync,
            start_time: Some(system_time_to_proto(self.start_time)),
            mirror_terminal_output: true,
        }))
    }

    pub fn client_id(&self) -> Option<&str> {
        self.client_metadata
            .iter()
            .find(|m| m.key == "id")
            .map(|m| m.value.as_str())
    }

    pub fn log_download_method(&self) -> bz_error::Result<LogDownloadMethod> {
        Ok(self.daemon_startup_config()?.log_download_method)
    }

    pub fn buildbuddy_bes(&self) -> bool {
        self.buildbuddy_bes
    }

    pub fn daemon_startup_config(&self) -> bz_error::Result<DaemonStartupConfig> {
        let mut daemon_startup_config = self.immediate_config.daemon_startup_config()?.clone();
        if let Some(watchfs) = self.watchfs_override {
            daemon_startup_config.watchfs = watchfs;
        }
        if let Some(remote_download_outputs) = self.remote_download_outputs_override {
            daemon_startup_config.remote_download_outputs = remote_download_outputs;
        }
        daemon_startup_config
            .remote_execution
            .apply_overrides(&self.remote_execution_startup_config);
        Ok(daemon_startup_config)
    }
}

fn client_repository_environment() -> Vec<ClientEnvironmentVariable> {
    std::env::vars_os()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .map(|(name, value)| ClientEnvironmentVariable {
            name,
            value: Some(value),
        })
        .collect()
}

fn bes_keywords(command_name: &str, user_keywords: &[String]) -> Vec<String> {
    let mut out = vec![
        format!("command_name={command_name}"),
        "protocol_name=BEP".to_owned(),
        "tool=buck2".to_owned(),
    ];
    for value in user_keywords {
        for keyword in value.split(',') {
            let keyword = keyword.trim();
            if !keyword.is_empty() {
                out.push(format!("user_keyword={keyword}"));
            }
        }
    }
    out
}

fn headers_with_buildbuddy_api_key(
    mut headers: Vec<String>,
    api_key: Option<&str>,
    bes_backend: &str,
    remote_cache: Option<&str>,
) -> Vec<String> {
    if let Some(api_key) = api_key
        && remote_cache.is_some_and(|remote_cache| endpoints_match(bes_backend, remote_cache))
    {
        headers.retain(|header| match header.split_once('=') {
            Some((name, _)) => !name.trim().eq_ignore_ascii_case(BUILDBUDDY_API_KEY_HEADER),
            None => true,
        });
        if !api_key.trim().is_empty() {
            headers.push(format!("{BUILDBUDDY_API_KEY_HEADER}={api_key}"));
        }
    }
    headers
}

fn endpoints_match(left: &str, right: &str) -> bool {
    normalize_endpoint(left).eq_ignore_ascii_case(normalize_endpoint(right))
}

fn normalize_endpoint(endpoint: &str) -> &str {
    let endpoint = endpoint.trim();
    let endpoint = endpoint
        .strip_prefix("grpcs://")
        .or_else(|| endpoint.strip_prefix("grpc://"))
        .or_else(|| endpoint.strip_prefix("https://"))
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    endpoint.trim_end_matches('/')
}

fn duration_to_proto(duration: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: duration.as_secs() as i64,
        nanos: duration.subsec_nanos() as i32,
    }
}

fn system_time_to_proto(time: SystemTime) -> prost_types::Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => prost_types::Timestamp {
            seconds: duration.as_secs() as i64,
            nanos: duration.subsec_nanos() as i32,
        },
        Err(error) => {
            let duration = error.duration();
            prost_types::Timestamp {
                seconds: -(duration.as_secs() as i64),
                nanos: -(duration.subsec_nanos() as i32),
            }
        }
    }
}

/// Provides a common interface for buck subcommands that use event subscribers for logging.
/// Executed by a ClientCommandContext.
#[allow(async_fn_in_trait)]
pub trait BuckSubcommand {
    /// Give the command a name for printing, debugging, etc.
    const COMMAND_NAME: &'static str;

    async fn exec_impl(
        self,
        matches: BuckArgMatches<'_>,
        ctx: ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult;

    fn logging_name(&self) -> &'static str {
        Self::COMMAND_NAME
    }

    // Don't return an error, all logging will break if this fails.
    fn update_events_ctx(
        &self,
        _matches: BuckArgMatches<'_>,
        ctx: &ClientCommandContext,
        events_ctx: &mut EventsCtx,
    ) -> bz_error::Result<()> {
        let paths = ctx.paths().ok();
        if let Some(recorder) = events_ctx.recorder.as_mut() {
            recorder.update_for_command(
                ctx,
                self.event_log_opts(),
                ctx.argv.argv.clone(),
                None,
                Vec::new(),
                None,
                None,
                paths,
            );
        }
        Ok(())
    }

    fn event_log_opts(&self) -> &CommonEventLogOptions {
        CommonEventLogOptions::no_event_log_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_bes_user_keywords_like_bazel() {
        assert_eq!(
            bes_keywords(
                "build",
                &["ci, pull-request".to_owned(), "linux".to_owned()]
            ),
            vec![
                "command_name=build",
                "protocol_name=BEP",
                "tool=buck2",
                "user_keyword=ci",
                "user_keyword=pull-request",
                "user_keyword=linux"
            ]
        );
    }

    #[test]
    fn api_key_adds_buildbuddy_header_to_bes_headers() {
        assert_eq!(
            headers_with_buildbuddy_api_key(
                vec!["x-other=value".to_owned()],
                Some("secret"),
                "remote.buildbuddy.dev",
                Some("remote.buildbuddy.dev"),
            ),
            vec!["x-other=value", "x-buildbuddy-api-key=secret"]
        );
    }

    #[test]
    fn api_key_allows_equivalent_buildbuddy_endpoint_spellings() {
        assert_eq!(
            headers_with_buildbuddy_api_key(
                vec!["x-other=value".to_owned()],
                Some("secret"),
                "grpc://remote.buildbuddy.dev",
                Some("https://remote.buildbuddy.dev/"),
            ),
            vec!["x-other=value", "x-buildbuddy-api-key=secret"]
        );
    }

    #[test]
    fn api_key_replaces_existing_buildbuddy_bes_header() {
        assert_eq!(
            headers_with_buildbuddy_api_key(
                vec![
                    "X-BuildBuddy-Api-Key=old".to_owned(),
                    "x-other=value".to_owned()
                ],
                Some("new"),
                "remote.buildbuddy.dev",
                Some("remote.buildbuddy.dev"),
            ),
            vec!["x-other=value", "x-buildbuddy-api-key=new"]
        );
    }

    #[test]
    fn empty_api_key_clears_existing_buildbuddy_bes_header() {
        assert_eq!(
            headers_with_buildbuddy_api_key(
                vec![
                    "x-buildbuddy-api-key=old".to_owned(),
                    "x-other=value".to_owned()
                ],
                Some(""),
                "remote.buildbuddy.dev",
                Some("remote.buildbuddy.dev"),
            ),
            vec!["x-other=value"]
        );
    }

    #[test]
    fn api_key_is_not_added_when_bes_backend_and_remote_cache_do_not_match() {
        assert_eq!(
            headers_with_buildbuddy_api_key(
                vec!["x-other=value".to_owned()],
                Some("secret"),
                "remote.buildbuddy.dev",
                Some("example.com"),
            ),
            vec!["x-other=value"]
        );
    }

    #[test]
    fn explicit_bes_header_is_preserved_when_remote_cache_does_not_match() {
        assert_eq!(
            headers_with_buildbuddy_api_key(
                vec![
                    "x-buildbuddy-api-key=manual".to_owned(),
                    "x-other=value".to_owned()
                ],
                Some("secret"),
                "remote.buildbuddy.dev",
                None,
            ),
            vec!["x-buildbuddy-api-key=manual", "x-other=value"]
        );
    }
}
