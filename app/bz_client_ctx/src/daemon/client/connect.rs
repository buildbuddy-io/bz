/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::env;
use std::ffi::OsStr;
use std::fs::File;
use std::io::BufReader;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use bz_cli_proto::DaemonProcessInfo;
use bz_cli_proto::daemon_api_client::DaemonApiClient;
use bz_common::buckd_connection::BUCK_AUTH_TOKEN_HEADER;
use bz_common::buckd_connection::ConnectionType;
use bz_common::client_utils::RetryError;
use bz_common::client_utils::get_channel_tcp;
use bz_common::client_utils::get_channel_uds;
use bz_common::client_utils::retrying;
use bz_common::daemon_dir::DaemonDir;
use bz_common::init::DaemonStartupConfig;
use bz_common::invocation_paths::InvocationPaths;
use bz_core::bz_env;
use bz_data::DaemonWasStartedReason;
use bz_error::BuckErrorContext;
use bz_error::ErrorTag;
use bz_error::bz_error;
use bz_error::conversion::from_any_with_tag;
use bz_error::internal_error;
use bz_event_observer::fmt_duration;
use bz_events::daemon_id::DaemonId;
use bz_fs::fs_util;
use bz_fs::paths::abs_norm_path::AbsNormPathBuf;
use bz_resource_control::spawn_daemon::create_daemon_spawn_command;
use bz_util::truncate::truncate;
use bz_wrapper_common::kill::process_exists;
use bz_wrapper_common::pid::Pid;
use dupe::Dupe;
use futures::FutureExt;
use futures::future::try_join3;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tonic::Request;
use tonic::Status;
use tonic::codegen::InterceptedService;
use tonic::metadata::AsciiMetadataValue;
use tonic::service::Interceptor;
use tonic::transport::Channel;

use crate::command_outcome::CommandOutcome;
use crate::daemon::client::BuckdClient;
use crate::daemon::client::BuckdClientConnector;
use crate::daemon::client::BuckdLifecycleLock;
use crate::daemon::client::kill;
use crate::daemon::client::kill::hard_kill_until;
use crate::daemon::daemon_windows::spawn_background_process_on_windows;
use crate::daemon_constraints;
use crate::daemon_constraints::get_possibly_nested_invocation_daemon_uuid;
use crate::events_ctx::DaemonEventsCtx;
use crate::events_ctx::EventsCtx;
use crate::immediate_config::ImmediateConfigContext;
use crate::startup_deadline::StartupDeadline;
use crate::subscribers::classify_server_stderr::classify_server_stderr;
use crate::subscribers::stdout_stderr_forwarder::StdoutStderrForwarder;

const DEFAULT_BUCKD_STARTUP_TIMEOUT_SECS: u64 = 120;

/// The client side matcher for DaemonConstraints.
#[derive(Clone, Debug)]
pub struct DaemonConstraintsRequest {
    /// The version of bz.
    version: String,
    /// Sandcastle id.
    user_version: Option<String>,
    desired_trace_io_state: DesiredTraceIoState,
    nested_invocation_daemon_uuid: Option<String>,
    pub reject_daemon: Option<String>,
    pub reject_materializer_state: Option<String>,
    pub daemon_startup_config: DaemonStartupConfig,
}

#[derive(Debug, Clone, Copy, derive_more::Display)]
pub(crate) enum ConstraintUnsatisfiedReason {
    #[display("Version mismatch")]
    Version,
    #[display("User version mismatch")]
    UserVersion,
    #[display("Startup config mismatch")]
    StartupConfig,
    #[display("Reject daemon id")]
    RejectDaemonId,
    #[display("Trace IO mismatch")]
    TraceIo,
    #[display("Sqlite identity mismatch")]
    SqliteIdentity,
}

impl ConstraintUnsatisfiedReason {
    pub(crate) fn to_daemon_was_started_reason(&self) -> bz_data::DaemonWasStartedReason {
        match self {
            ConstraintUnsatisfiedReason::Version => {
                bz_data::DaemonWasStartedReason::ConstraintMismatchVersion
            }
            ConstraintUnsatisfiedReason::UserVersion => {
                bz_data::DaemonWasStartedReason::ConstraintMismatchUserVersion
            }
            ConstraintUnsatisfiedReason::StartupConfig => {
                bz_data::DaemonWasStartedReason::ConstraintMismatchStartupConfig
            }
            ConstraintUnsatisfiedReason::RejectDaemonId => {
                bz_data::DaemonWasStartedReason::ConstraintRejectDaemonId
            }
            ConstraintUnsatisfiedReason::TraceIo => {
                bz_data::DaemonWasStartedReason::ConstraintMismatchTraceIo
            }
            ConstraintUnsatisfiedReason::SqliteIdentity => {
                bz_data::DaemonWasStartedReason::ConstraintMismatchSqliteIdentity
            }
        }
    }
}

impl DaemonConstraintsRequest {
    pub fn new(
        immediate_config: &ImmediateConfigContext<'_>,
        desired_trace_io_state: DesiredTraceIoState,
    ) -> bz_error::Result<Self> {
        Self::new_with_startup_config(
            immediate_config.daemon_startup_config()?.clone(),
            desired_trace_io_state,
        )
    }

    pub fn new_with_startup_config(
        daemon_startup_config: DaemonStartupConfig,
        desired_trace_io_state: DesiredTraceIoState,
    ) -> bz_error::Result<Self> {
        Ok(Self {
            version: daemon_constraints::version()?,
            user_version: daemon_constraints::user_version()?,
            desired_trace_io_state,
            nested_invocation_daemon_uuid: get_possibly_nested_invocation_daemon_uuid(),
            reject_daemon: None,
            reject_materializer_state: None,
            daemon_startup_config,
        })
    }

    fn is_trace_io_requested(&self) -> bool {
        matches!(self.desired_trace_io_state, DesiredTraceIoState::Enabled)
    }

    fn satisfied(
        &self,
        daemon: &bz_cli_proto::DaemonConstraints,
    ) -> Result<(), ConstraintUnsatisfiedReason> {
        if self.version != daemon.version {
            return Err(ConstraintUnsatisfiedReason::Version);
        }

        if !is_nested_invocation(self.nested_invocation_daemon_uuid.as_ref(), daemon)
            && self.user_version != daemon.user_version
        {
            return Err(ConstraintUnsatisfiedReason::UserVersion);
        }

        let server_daemon_startup_config = daemon.daemon_startup_config.as_ref().and_then(|c| {
            let server = DaemonStartupConfig::deserialize(c);
            if let Err(e) = server.as_ref() {
                tracing::warn!("Daemon returned invalid DaemonStartupConfig: {:#}", e);
            }
            server.ok()
        });

        if Some(&self.daemon_startup_config) != server_daemon_startup_config.as_ref() {
            return Err(ConstraintUnsatisfiedReason::StartupConfig);
        }

        if let Some(r) = &self.reject_daemon {
            if *r == daemon.daemon_id {
                return Err(ConstraintUnsatisfiedReason::RejectDaemonId);
            }
        }

        // At this point, if ExtraDaemonConstraints is missing, we'll reuse the daemon (as that
        // means it failed to start), if not we proceed to check further constraints.

        let extra = match &daemon.extra {
            Some(e) => e,
            None => return Ok(()),
        };

        match (self.desired_trace_io_state, extra.trace_io_enabled) {
            (DesiredTraceIoState::Enabled, false) => {
                return Err(ConstraintUnsatisfiedReason::TraceIo);
            }
            (DesiredTraceIoState::Disabled, true) => {
                return Err(ConstraintUnsatisfiedReason::TraceIo);
            }
            _ => {}
        }

        if let Some(r) = &self.reject_materializer_state {
            if extra.materializer_state_identity.as_ref() == Some(r) {
                return Err(ConstraintUnsatisfiedReason::SqliteIdentity);
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Dupe)]
pub enum DesiredTraceIoState {
    Enabled,
    Disabled,
    Existing,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum BuckdConnectConstraints {
    ExistingOnly,
    Constraints(DaemonConstraintsRequest),
}

async fn get_channel(
    endpoint: ConnectionType,
    change_to_parent_dir: bool,
) -> bz_error::Result<Channel> {
    match endpoint {
        ConnectionType::Uds { unix_socket } => {
            Ok(get_channel_uds(&unix_socket, change_to_parent_dir).await?)
        }
        ConnectionType::Tcp { port } => Ok(get_channel_tcp(Ipv4Addr::LOCALHOST, port).await?),
    }
}

#[derive(Clone)]
pub struct BuckAddAuthTokenInterceptor {
    auth_token: AsciiMetadataValue,
}

impl Interceptor for BuckAddAuthTokenInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .append(BUCK_AUTH_TOKEN_HEADER, self.auth_token.clone());
        Ok(request)
    }
}

pub async fn new_daemon_api_client(
    endpoint: ConnectionType,
    auth_token: String,
) -> bz_error::Result<DaemonApiClient<InterceptedService<Channel, BuckAddAuthTokenInterceptor>>>
{
    let channel = get_channel(endpoint, true).await?;
    Ok(DaemonApiClient::with_interceptor(
        channel,
        BuckAddAuthTokenInterceptor {
            auth_token: AsciiMetadataValue::try_from(auth_token)
                .map_err(|e| from_any_with_tag(e, ErrorTag::InvalidAuthToken))?,
        },
    )
    .max_encoding_message_size(usize::MAX)
    .max_decoding_message_size(usize::MAX))
}

pub fn buckd_startup_timeout() -> bz_error::Result<Duration> {
    Ok(Duration::from_secs(buckd_startup_timeout_secs()?))
}

pub fn buckd_startup_init_timeout() -> bz_error::Result<Duration> {
    Ok(buckd_startup_init_timeout_from_values(
        buckd_startup_timeout_secs()?,
        bz_env!("BUCKD_STARTUP_INIT_TIMEOUT", type=u64)?,
    ))
}

fn buckd_startup_timeout_secs() -> bz_error::Result<u64> {
    let timeout_secs =
        bz_env!("BUCKD_STARTUP_TIMEOUT", type=u64)?.unwrap_or(DEFAULT_BUCKD_STARTUP_TIMEOUT_SECS);
    Ok(timeout_secs)
}

fn buckd_startup_init_timeout_from_values(
    startup_timeout_secs: u64,
    startup_init_timeout_secs: Option<u64>,
) -> Duration {
    Duration::from_secs(startup_init_timeout_secs.unwrap_or(startup_timeout_secs))
}

/// Responsible for starting the daemon when no daemon is running.
/// This struct holds a lock such that only one daemon is ever started per daemon directory.
struct BuckdLifecycle<'a> {
    paths: &'a InvocationPaths,
    lock: BuckdLifecycleLock,
    constraints: &'a DaemonConstraintsRequest,
}

impl<'a> BuckdLifecycle<'a> {
    async fn lock_with_timeout(
        paths: &'a InvocationPaths,
        deadline: StartupDeadline,
        constraints: &'a DaemonConstraintsRequest,
    ) -> bz_error::Result<BuckdLifecycle<'a>> {
        Ok(BuckdLifecycle::<'a> {
            paths,
            lock: BuckdLifecycleLock::lock_with_timeout(paths.daemon_dir()?, deadline).await?,
            constraints,
        })
    }

    fn clean_daemon_dir(&self) -> bz_error::Result<()> {
        self.lock
            .clean_daemon_dir(true)
            .buck_error_context("Cleaning daemon dir")
            .tag(ErrorTag::DaemonDirCleanupFailed)
    }

    async fn start_server(&self) -> bz_error::Result<()> {
        let mut args = vec!["--isolation-dir", self.paths.isolation.as_str(), "daemon"];

        let daemon_id = DaemonId::new();
        let daemon_id_s = daemon_id.to_string();

        args.push("--daemon-id");
        args.push(&daemon_id_s);

        if self.constraints.is_trace_io_requested() {
            args.push("--enable-trace-io");
        }

        if let Some(r) = &self.constraints.reject_materializer_state {
            args.push("--reject-materializer-state");
            args.push(r);
        }

        let mut daemon_env_vars = Vec::new();

        daemon_env_vars.push((OsStr::new("RUST_BACKTRACE"), OsStr::new("1")));
        daemon_env_vars.push((OsStr::new("BZ_SUPPRESS_UNOPTIMIZED_WARNING"), OsStr::new("1")));

        // TODO(nga): We create too many backtraces during `attrs.source()` coercion. Can be
        //   reproduced with this command:
        //   ```
        //   bz --isolation-dir=xx audit providers fbcode//bz:bz --quiet
        //   ```
        //   Which regresses from 15s to 80s when `RUST_LIB_BACKTRACE` is set. So we disable
        //   backtraces in the daemon unless the user has explicitly asked for them. We
        //   intentionally avoid considering the `RUST_BACKTRACE` variables that buck was invoked
        //   with, because a lot of Rust tooling sets those without meaning to influence this
        //   behavior.
        daemon_env_vars.push((
            OsStr::new("RUST_LIB_BACKTRACE"),
            OsStr::new(bz_env!("BUCK2_LIB_BACKTRACE")?.unwrap_or("0")),
        ));

        if env::var_os("FORCE_WANT_RESTART").is_some() {
            // Disable restarter for the actual daemon command, even if it was forced, otherwise we
            // restart the daemon when it exits.
            daemon_env_vars.push((OsStr::new("FORCE_WANT_RESTART"), OsStr::new("false")));
        }

        if cfg!(unix) {
            // On Unix we spawn a process which forks and exits,
            // and here we wait for that spawned process to terminate.
            self.start_server_unix(
                args,
                &daemon_env_vars,
                &self.constraints.daemon_startup_config,
                &daemon_id,
            )
            .await
        } else {
            self.start_server_windows(
                args,
                &daemon_env_vars,
                &self.constraints.daemon_startup_config,
            )
        }
    }

    fn start_server_windows(
        &self,
        mut args: Vec<&str>,
        daemon_env_vars: &[(&OsStr, &OsStr)],
        daemon_startup_config: &DaemonStartupConfig,
    ) -> bz_error::Result<()> {
        let daemon_startup_config = daemon_startup_config.serialize()?;
        args.extend(["--dont-daemonize"]);
        spawn_background_process_on_windows(
            self.paths.project_root().root(),
            &get_daemon_exe()?,
            args.into_iter()
                .chain(std::iter::once(daemon_startup_config.as_str())),
            daemon_env_vars,
        )
    }

    async fn start_server_unix(
        &self,
        args: Vec<&str>,
        daemon_env_vars: &[(&OsStr, &OsStr)],
        daemon_startup_config: &DaemonStartupConfig,
        daemon_id: &DaemonId,
    ) -> bz_error::Result<()> {
        let project_dir = self.paths.project_root();
        let timeout_secs = buckd_startup_timeout()?;

        let daemon_exe = get_daemon_exe()?;

        // Create a unique name that we know won't overlap with other bz daemons and has enough
        // information to understand at least a little bit about which daemon it is
        let repo_name = project_dir
            .root()
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let unit_name = format!(
            "buck2-daemon.{}.{}.{}",
            &repo_name,
            self.paths.isolation.as_str(),
            &daemon_id,
        );

        let (cmd, resource_control_args) = create_daemon_spawn_command(
            &daemon_startup_config.resource_control,
            daemon_exe,
            unit_name,
            project_dir.root(),
        )
        .await?;
        let mut cmd: tokio::process::Command = cmd.into();

        cmd.current_dir(project_dir.root())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .args(args);

        cmd.arg(daemon_startup_config.serialize()?);

        cmd.args(&resource_control_args);

        if bz_env!("BUCK_DAEMON_LOG_TO_FILE", type=u8)? == Some(1) {
            cmd.env("BUCK_LOG_TO_FILE_PATH", self.paths.log_dir().as_os_str());
        }

        for (key, val) in daemon_env_vars {
            cmd.env(key, val);
        }

        // For Unix, set a daemon process title
        #[cfg(unix)]
        {
            use std::ffi::OsString;

            let mut title = OsString::new();
            title.push("buck2d");
            if let Some(dir) = project_dir.root().file_name() {
                title.push("[");
                title.push(dir);
                title.push("]");
            }
            cmd.arg0(title);
        }

        let mut child = cmd.spawn()?;
        let mut stdout_taken = child
            .stdout
            .take()
            .ok_or_else(|| internal_error!("Child should have its stdout piped"))?;
        let mut stderr_taken = child
            .stderr
            .take()
            .ok_or_else(|| internal_error!("Child should have its stderr piped"))?;

        let status_fut = async {
            let result = timeout(timeout_secs, child.wait()).await;
            match result {
                Err(_elapsed) => {
                    // The command has timed out, kill the process and wait
                    child.kill().await.buck_error_context(
                        "Error killing process after bz daemon launch timing out",
                    )?;
                    // This should return immediately as kill() waits for the process to end. We wait here again to fetch the ExitStatus
                    // Signal termination is not considered a success, so wait() results in an appropriate ExitStatus
                    bz_error::Ok(child.wait().await?)
                }
                Ok(result) => result.map_err(bz_error::Error::from),
            }
        };
        let stdout_fut = async {
            let mut buf = Vec::new();
            stdout_taken
                .read_to_end(&mut buf)
                .await
                .buck_error_context("Error reading stdout of child")?;
            Ok(buf)
        };
        let stderr_fut = async {
            let mut buf = Vec::new();
            stderr_taken
                .read_to_end(&mut buf)
                .await
                .buck_error_context("Error reading stderr of child")?;
            Ok(buf)
        };

        // `bz daemon` will either:
        // * fork and kill parent (daemonize) on Unix
        // * or spawn another process and exit on Windows
        // so we wait for termination of the child process.
        let joined = try_join3(status_fut, stdout_fut, stderr_fut).await;
        match joined {
            Err(error) => Err(BuckdConnectError::BuckDaemonLaunchFailed { error }.into()),
            Ok((status, stdout, stderr)) => {
                if !status.success() {
                    let exit_status_error = bz_error::Error::from(status);
                    Err(BuckdConnectError::BuckDaemonStartupFailed {
                        stdout: String::from_utf8_lossy(&stdout).to_string(),
                        stderr: String::from_utf8_lossy(&stderr).to_string(),
                        exit_status_error,
                    }
                    .into())
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// Represents an established connection to the daemon. We then upgrade it into a
/// BootstrapBuckdClient by querying constraints. This is a separate step so that we retry
/// establishing the channel but not querying constraints.
pub struct BuckdChannel {
    info: DaemonProcessInfo,
    daemon_dir: DaemonDir,
    client: DaemonApiClient<InterceptedService<Channel, BuckAddAuthTokenInterceptor>>,
}

impl BuckdChannel {
    /// Upgrade this BuckdChannel to a BootstrapBuckdClient.
    pub async fn upgrade(self) -> bz_error::Result<BootstrapBuckdClient> {
        let Self {
            info,
            daemon_dir,
            mut client,
        } = self;

        let constraints = get_constraints(&mut client)
            .await
            .buck_error_context("Error obtaining daemon constraints")?;

        Ok(BootstrapBuckdClient {
            info,
            daemon_dir,
            client,
            constraints,
        })
    }
}

/// Client used for connection setup. Can be used to create BuckdClientConnector instances later.
#[derive(Clone)]
pub struct BootstrapBuckdClient {
    info: DaemonProcessInfo,
    daemon_dir: DaemonDir,
    client: DaemonApiClient<InterceptedService<Channel, BuckAddAuthTokenInterceptor>>,
    /// The constraints for the daemon we're connected to.
    constraints: bz_cli_proto::DaemonConstraints,
}

impl BootstrapBuckdClient {
    pub async fn connect(
        paths: &InvocationPaths,
        constraints: BuckdConnectConstraints,
        events_ctx: &mut EventsCtx,
    ) -> bz_error::Result<Self> {
        let daemon_dir = paths.daemon_dir()?;

        fs_util::create_dir_all(&daemon_dir.path)
            .with_buck_error_context(|| format!("Error creating daemon dir: {daemon_dir}"))?;

        let res = match constraints {
            BuckdConnectConstraints::ExistingOnly => {
                establish_connection_existing(&daemon_dir).await
            }
            BuckdConnectConstraints::Constraints(constraints) => {
                establish_connection(paths, constraints, events_ctx).await
            }
        };

        if let Err(e) = res {
            Err(daemon_connect_error(e, paths)
                .await
                .tag([ErrorTag::DaemonConnect]))
        } else {
            res
        }
    }

    pub fn to_connector(self) -> BuckdClientConnector {
        let cgroup_path_of_bz_daemon = {
            #[cfg(target_os = "linux")]
            {
                bz_resource_control::buck_cgroup_tree::read_cgroup_path_of_bz_daemon(
                    self.info.pid,
                )
                .ok()
                .flatten()
            }
            #[cfg(not(target_os = "linux"))]
            {
                None
            }
        };
        BuckdClientConnector {
            client: BuckdClient {
                daemon_dir: self.daemon_dir,
                client: self.client,
                constraints: self.constraints,
            },
            cgroup_path_of_bz_daemon,
        }
    }

    pub(crate) async fn kill(&mut self, reason: &str) -> bz_error::Result<Pid> {
        kill::kill(&mut self.client, &self.info, reason).await?;
        Pid::from_i64(self.info.pid)
    }

    async fn kill_for_constraints_mismatch(&mut self) -> bz_error::Result<Pid> {
        kill::kill_for_constraints_mismatch(
            &mut self.client,
            &self.info,
            "client expected different buckd constraints",
        )
        .await?;
        Pid::from_i64(self.info.pid)
    }

    pub fn pid(&self) -> i64 {
        self.info.pid
    }
}

/// Attempt to connect to a daemon that can satisfy specified constraints.
/// If the daemon does not match constraints (different version or does not enable I/O tracing),
/// it will kill it and restart it with the correct constraints.
/// This behavior can be overridden by passing `BuckdConnectConstraints::ExistingOnly`.
/// In that case, then any existing buck daemon (regardless of constraint) is accepted.
pub async fn connect_buckd(
    constraints: BuckdConnectConstraints,
    events_ctx: &mut EventsCtx,
    paths: &InvocationPaths,
) -> bz_error::Result<BuckdClientConnector> {
    match BootstrapBuckdClient::connect(paths, constraints, events_ctx).await {
        Ok(client) => Ok(client.to_connector()),
        Err(e) => {
            events_ctx.handle_daemon_connection_failure();
            Err(e)
        }
    }
}

pub async fn establish_connection_existing(
    daemon_dir: &DaemonDir,
) -> bz_error::Result<BootstrapBuckdClient> {
    let deadline = StartupDeadline::duration_from_now(buckd_startup_timeout()?)?;
    deadline
        .run(
            "establishing connection to existing Buck daemon",
            async move {
                BuckdProcessInfo::load(daemon_dir)?
                    .create_channel()
                    .await?
                    .upgrade()
                    .await
            },
        )
        .await
}

async fn establish_connection(
    paths: &InvocationPaths,
    constraints: DaemonConstraintsRequest,
    events_ctx: &mut EventsCtx,
) -> bz_error::Result<BootstrapBuckdClient> {
    // There are many places where `establish_connection_inner` may hang.
    // If it does, better print something to the user instead of hanging quietly forever.
    let timeout = buckd_startup_init_timeout()?;
    let deadline = StartupDeadline::duration_from_now(timeout)?;
    deadline
        .down(
            "establishing connection to Buck daemon or start a daemon",
            |timeout| establish_connection_inner(paths, constraints, timeout, events_ctx),
        )
        .await
}

fn explain_failed_to_connect_reason(reason: bz_data::DaemonWasStartedReason) -> &'static str {
    match reason {
        DaemonWasStartedReason::UnknownReason => "Unknown reason",
        DaemonWasStartedReason::ConstraintMismatchVersion => "Version mismatch",
        DaemonWasStartedReason::ConstraintMismatchUserVersion => "User version mismatch",
        DaemonWasStartedReason::ConstraintMismatchStartupConfig => "Startup config mismatch",
        DaemonWasStartedReason::ConstraintRejectDaemonId => "Reject daemon id",
        DaemonWasStartedReason::ConstraintMismatchTraceIo => "Trace IO mismatch",
        DaemonWasStartedReason::ConstraintMismatchSqliteIdentity => "Sqlite identity mismatch",
        DaemonWasStartedReason::CouldNotConnectToDaemon => {
            // TODO(nga): get rid of this variant.
            "Could not connect to daemon"
        }
        DaemonWasStartedReason::TimedOutConnectingToDaemon => "Timed out connecting to daemon",
        DaemonWasStartedReason::TimeoutCalculationError => "Timeout calculation error",
        DaemonWasStartedReason::NoBuckdInfo => "No buckd.info",
        DaemonWasStartedReason::CouldNotLoadBuckdInfo => "Could not load buckd.info",
        DaemonWasStartedReason::NoDaemonProcess => "bz daemon is not running",
    }
}

fn format_daemon_elapsed(elapsed: Duration) -> String {
    fmt_duration::fmt_duration(elapsed)
}

#[allow(clippy::collapsible_match)]
async fn establish_connection_inner(
    paths: &InvocationPaths,
    constraints: DaemonConstraintsRequest,
    deadline: StartupDeadline,
    events_ctx: &mut EventsCtx,
) -> bz_error::Result<BootstrapBuckdClient> {
    let daemon_dir = paths.daemon_dir()?;

    let connect_before_restart_start = Instant::now();
    let res = deadline
        .half()?
        .run("connecting to existing buck daemon", {
            try_connect_existing_before_acquiring_lifecycle_lock(&daemon_dir, &constraints).map(Ok)
        })
        .await;
    if let Ok(connect_before_restart) = res {
        match connect_before_restart {
            ConnectBeforeRestart::Accepted(client) => return Ok(client),
            ConnectBeforeRestart::Rejected => {}
            ConnectBeforeRestart::ConstraintMismatch(reason) => {
                events_ctx
                    .eprintln(&format!(
                        "bz daemon constraint mismatch: {reason}; waiting for daemon lifecycle lock... (detected in {})",
                        format_daemon_elapsed(connect_before_restart_start.elapsed()),
                    ))
                    .await?;
            }
        };
    }

    // At this point, we've either failed to connect to buckd or buckd had the wrong constraints.
    // Get the lifecycle lock to ensure we don't have races with other processes as we check and change things.
    let lifecycle_lock_wait_start = Instant::now();
    let lifecycle_lock = deadline
        .down("acquire lifecycle lock", |deadline| {
            BuckdLifecycle::lock_with_timeout(paths, deadline, &constraints)
        })
        .await?;
    let lifecycle_lock_wait_elapsed = lifecycle_lock_wait_start.elapsed();

    // Even if we didn't connect before, it's possible that we just raced with another invocation
    // starting the server, so we try to connect again while holding the lock.
    let daemon_was_started_reason = {
        match BuckdProcessInfo::load_if_exists(&daemon_dir) {
            Ok(Some(buckd_info)) => {
                match try_connect_existing(&buckd_info, &deadline, &lifecycle_lock).await {
                    Ok(channel) => {
                        let mut client = channel.upgrade().await?;

                        let reason = match constraints.satisfied(&client.constraints) {
                            Ok(()) => return Ok(client),
                            Err(reason) => reason,
                        };

                        if is_nested_invocation(
                            get_possibly_nested_invocation_daemon_uuid().as_ref(),
                            &client.constraints,
                        ) {
                            match reason {
                                ConstraintUnsatisfiedReason::TraceIo
                                | ConstraintUnsatisfiedReason::StartupConfig => {
                                    return Err(BuckdConnectError::NestedConstraintMismatch {
                                        reason,
                                    }
                                    .into());
                                }
                                _ => (),
                            }
                        }

                        events_ctx
                            .eprintln(&format!(
                                "bz daemon constraint mismatch: {reason}; killing daemon... (lifecycle lock acquired in {})",
                                format_daemon_elapsed(lifecycle_lock_wait_elapsed),
                            ))
                            .await?;

                        let kill_start = Instant::now();
                        deadline
                            .run(
                                "sending kill command to the Buck daemon",
                                client.kill_for_constraints_mismatch(),
                            )
                            .await?;

                        events_ctx
                            .eprintln(&format!(
                                "Starting new bz daemon... (previous daemon killed in {})",
                                format_daemon_elapsed(kill_start.elapsed()),
                            ))
                            .await?;

                        reason.to_daemon_was_started_reason()
                    }
                    Err(reason) => {
                        if reason == bz_data::DaemonWasStartedReason::NoDaemonProcess {
                            events_ctx
                                .eprintln(
                                    "Stale bz daemon info found (daemon is not running); starting new bz daemon...",
                                )
                                .await?;
                        } else {
                            events_ctx
                                .eprintln(&format!(
                                    "Could not connect to bz daemon ({}), killing daemon...",
                                    explain_failed_to_connect_reason(reason)
                                ))
                                .await?;

                            hard_kill_until(&buckd_info.info, &deadline)
                                .await
                                .map_err(|error| BuckdConnectError::DaemonKillFailed { error })?;
                        }

                        reason
                    }
                }
            }
            Ok(None) => {
                events_ctx.eprintln("Starting new bz daemon...").await?;

                bz_data::DaemonWasStartedReason::NoBuckdInfo
            }
            Err(e) => {
                events_ctx
                    .eprintln(&format!(
                        "Could not load buckd.info: {e}, starting new bz daemon..."
                    ))
                    .await?;

                bz_data::DaemonWasStartedReason::CouldNotLoadBuckdInfo
            }
        }
    };

    deadline
        .down(
            &format!(
                "starting new bz daemon for reason: {}",
                explain_failed_to_connect_reason(daemon_was_started_reason)
            ),
            |deadline| {
                start_new_buckd_and_connect(
                    deadline,
                    &lifecycle_lock,
                    paths,
                    &constraints,
                    events_ctx,
                    daemon_was_started_reason,
                )
            },
        )
        .await
}

async fn start_new_buckd_and_connect(
    deadline: StartupDeadline,
    lifecycle_lock: &BuckdLifecycle<'_>,
    paths: &InvocationPaths,
    constraints: &DaemonConstraintsRequest,
    events_ctx: &mut EventsCtx,
    daemon_was_started_reason: bz_data::DaemonWasStartedReason,
) -> bz_error::Result<BootstrapBuckdClient> {
    let start_and_connect_start = Instant::now();

    // Daemon dir may be corrupted. Safer to delete it.
    lifecycle_lock.clean_daemon_dir()?;

    // Now there's definitely no server that can be connected to
    let start_server_start = Instant::now();
    lifecycle_lock
        .start_server()
        .await
        .buck_error_context("Error starting bz daemon")?;
    let start_server_elapsed = start_server_start.elapsed();
    // It might take a little bit for the daemon server to start up. We could wait for the buckd.info
    // file to appear, but it's just as easy to just retry the connection itself.

    let connect_start = Instant::now();
    let channel = deadline
        .retrying(
            "connect to buckd after server start",
            Duration::from_millis(5),
            Duration::from_millis(100),
            || async { BuckdProcessInfo::load_and_create_channel(&paths.daemon_dir()?).await },
        )
        .await?;
    let connect_elapsed = connect_start.elapsed();

    let client = channel.upgrade().await?;

    if let Err(reason) = constraints.satisfied(&client.constraints) {
        return Err(BuckdConnectError::BuckDaemonConstraintWrongAfterStart {
            reason,
            expected: constraints.clone(),
            actual: client.constraints,
        }
        .into());
    }

    events_ctx.handle_daemon_started(daemon_was_started_reason);

    events_ctx
        .eprintln(&format!(
            "Connected to new bz daemon in {} (start: {}, connect: {}).",
            format_daemon_elapsed(start_and_connect_start.elapsed()),
            format_daemon_elapsed(start_server_elapsed),
            format_daemon_elapsed(connect_elapsed),
        ))
        .await?;

    Ok(client)
}

#[allow(clippy::large_enum_variant)]
enum ConnectBeforeRestart {
    Accepted(BootstrapBuckdClient),
    Rejected,
    ConstraintMismatch(ConstraintUnsatisfiedReason),
}

/// Connect to buckd before attempt to restart the server.
///
/// # Returns
///
/// * `Ok(Some(client))` if we connected to an existing buckd
/// * `Ok(None)` if we failed to connect and should restart buckd
/// * `Err` if we failed to connect and should abandon startup
async fn try_connect_existing_before_acquiring_lifecycle_lock(
    daemon_dir: &DaemonDir,
    constraints: &DaemonConstraintsRequest,
) -> ConnectBeforeRestart {
    match BuckdProcessInfo::load_and_create_channel(daemon_dir).await {
        Ok(channel) => {
            let Ok(client) = channel.upgrade().await else {
                return ConnectBeforeRestart::Rejected;
            };
            match constraints.satisfied(&client.constraints) {
                Ok(()) => ConnectBeforeRestart::Accepted(client),
                Err(reason) => ConnectBeforeRestart::ConstraintMismatch(reason),
            }
        }
        Err(e) => {
            tracing::debug!("Connect failed: {:#}", e);
            ConnectBeforeRestart::Rejected
        }
    }
}

async fn try_connect_existing(
    buckd_info: &BuckdProcessInfo<'_>,
    timeout: &StartupDeadline,
    _lock: &BuckdLifecycle<'_>,
) -> Result<BuckdChannel, bz_data::DaemonWasStartedReason> {
    let timeout: bz_error::Result<_> = try { timeout.min(buckd_startup_timeout()?)? };
    let Ok(timeout) = timeout else {
        return Err(bz_data::DaemonWasStartedReason::TimeoutCalculationError);
    };
    let Ok(rem_duration) = timeout.rem_duration("connect existing buckd") else {
        return Err(bz_data::DaemonWasStartedReason::TimedOutConnectingToDaemon);
    };
    match tokio::time::timeout(rem_duration, buckd_info.create_channel()).await {
        Ok(Ok(channel)) => Ok(channel),
        Ok(Err(_)) => {
            let Ok(pid) = buckd_info.pid() else {
                return Err(bz_data::DaemonWasStartedReason::CouldNotLoadBuckdInfo);
            };
            let buckd_process_exists = process_exists(pid).unwrap_or(true);
            if !buckd_process_exists {
                // We don't delete the `buckd.info` file, and if we failed to connect,
                // the most likely reason is that the daemon process doesn't exist.
                Err(bz_data::DaemonWasStartedReason::NoDaemonProcess)
            } else {
                Err(bz_data::DaemonWasStartedReason::CouldNotConnectToDaemon)
            }
        }
        Err(e) => {
            let _assert_type: tokio::time::error::Elapsed = e;
            Err(bz_data::DaemonWasStartedReason::TimedOutConnectingToDaemon)
        }
    }
}

pub struct BuckdProcessInfo<'a> {
    pub(crate) info: DaemonProcessInfo,
    daemon_dir: &'a DaemonDir,
}

impl<'a> BuckdProcessInfo<'a> {
    /// Utility method for places that want to match on the overall result of those two operations.
    async fn load_and_create_channel(
        daemon_dir: &'a DaemonDir,
    ) -> bz_error::Result<BuckdChannel> {
        Self::load(daemon_dir)?.create_channel().await
    }

    pub fn load(daemon_dir: &'a DaemonDir) -> bz_error::Result<Self> {
        match Self::load_if_exists(daemon_dir) {
            Ok(Some(info)) => Ok(info),
            Ok(None) => Err(BuckdConnectError::BuckdInfoMissing {
                path: daemon_dir.buckd_info(),
            }
            .into()),
            Err(e) => Err(e),
        }
    }

    pub fn load_if_exists(daemon_dir: &'a DaemonDir) -> bz_error::Result<Option<Self>> {
        let location = daemon_dir.buckd_info();
        let file = match File::open(&location) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_buck_error_context(|| {
                    format!("Trying to open buckd info, `{}`", location.display())
                });
            }
        };
        let reader = BufReader::new(file);
        let info = serde_json::from_reader(reader)
            .map_err(|error| BuckdConnectError::BuckdInfoParseError { location, error })?;

        Ok(Some(BuckdProcessInfo { info, daemon_dir }))
    }

    pub async fn create_channel(&self) -> bz_error::Result<BuckdChannel> {
        tracing::debug!("Creating channel to: {}", self.info.endpoint);
        let connection_type = ConnectionType::parse(&self.info.endpoint)?;

        let client = new_daemon_api_client(connection_type, self.info.auth_token.clone())
            .await
            .buck_error_context("Error connecting")?;

        Ok(BuckdChannel {
            info: self.info.clone(),
            daemon_dir: self.daemon_dir.clone(),
            client,
        })
    }

    pub async fn hard_kill(&self) -> bz_error::Result<()> {
        kill::hard_kill(&self.info).await
    }

    pub fn pid(&self) -> bz_error::Result<Pid> {
        Pid::from_i64(self.info.pid)
    }
}

async fn get_constraints(
    client: &mut DaemonApiClient<InterceptedService<Channel, BuckAddAuthTokenInterceptor>>,
) -> bz_error::Result<bz_cli_proto::DaemonConstraints> {
    // NOTE: No tailers in bootstrap client, we capture logs if we fail to connect, but
    // otherwise we leave them alone.
    let mut events_ctx = EventsCtx::new(None, vec![Box::new(StdoutStderrForwarder)]);
    let status = DaemonEventsCtx::without_tailers(&mut events_ctx)
        .unpack_oneshot({
            client.status(tonic::Request::new(bz_cli_proto::StatusRequest {
                snapshot: false,
                include_tokio_runtime_metrics: false,
            }))
        })
        .await?;

    let status: bz_cli_proto::StatusResponse = match status {
        CommandOutcome::Success(r) => Ok(r),
        CommandOutcome::Failure(_) => Err(bz_error!(
            ErrorTag::DaemonStatus,
            "Unexpected failure message in status()"
        )),
    }?;

    Ok(status.daemon_constraints.unwrap_or_default())
}

pub fn get_daemon_exe() -> bz_error::Result<PathBuf> {
    let exe = env::current_exe().buck_error_context("Failed to get current exe")?;
    if bz_core::client_only::is_client_only()? {
        let ext = if cfg!(windows) { ".exe" } else { "" };
        Ok(exe
            .parent()
            .ok_or_else(|| internal_error!("Expected current exe to be in a directory"))?
            .join(format!("buck2-daemon{ext}")))
    } else {
        Ok(exe)
    }
}

#[derive(Debug, bz_error::Error)]
#[allow(clippy::large_enum_variant)]
#[buck2(tag = DaemonConnect)]
enum BuckdConnectError {
    #[error("buck daemon startup failed\nstdout:\n{stdout}\nstderr:\n{stderr}")]
    #[buck2(tag = DaemonStartupFailed)]
    BuckDaemonStartupFailed {
        stdout: String,
        stderr: String,
        #[source]
        exit_status_error: bz_error::Error,
    },
    #[error("Failed to launch bz daemon: {error:#}")]
    #[buck2(tag = DaemonLaunchFailed)]
    BuckDaemonLaunchFailed {
        #[source]
        error: bz_error::Error,
    },
    #[error(
        "during buck daemon startup, the started process did not match constraints ({reason}).\nexpected: {expected:?}\nactual: {actual:?}"
    )]
    #[buck2(tag = DaemonConstraintsWrongAfterStart)]
    BuckDaemonConstraintWrongAfterStart {
        reason: ConstraintUnsatisfiedReason,
        expected: DaemonConstraintsRequest,
        actual: bz_cli_proto::DaemonConstraints,
    },
    #[error("bz daemon constraint mismatch during nested invocation: {reason}")]
    #[buck2(tag = DaemonNestedConstraintsMismatch)]
    NestedConstraintMismatch { reason: ConstraintUnsatisfiedReason },
    #[error("buckd info {path} does not exist")]
    #[buck2(tag = BuckdInfoMissing)]
    BuckdInfoMissing { path: AbsNormPathBuf },
    #[error("Error parsing daemon info in `{}`. \
                Try deleting that file and running `bz killall` before running your command again",
                location.display())]
    #[buck2(tag = BuckdInfoParseError)]
    BuckdInfoParseError {
        location: AbsNormPathBuf,
        #[source]
        error: serde_json::Error,
    },
    #[error("Failed to kill buckd: {error:#}")]
    #[buck2(tag = DaemonKillFailed)]
    DaemonKillFailed {
        #[source]
        error: bz_error::Error,
    },
}

async fn daemon_connect_error(
    error: bz_error::Error,
    paths: &InvocationPaths,
) -> bz_error::Error {
    let error_report: Result<bz_data::ErrorReport, RetryError<bz_error::Error>> = retrying(
        Duration::from_millis(50),
        Duration::from_millis(100),
        Duration::from_millis(500),
        || async {
            let daemon_dir = paths.daemon_dir()?;
            let error_log = std::fs::read(daemon_dir.buckd_error_log())?;

            let error_report = bz_data::ErrorReport::deserialize(
                &mut serde_json::Deserializer::from_slice(&error_log),
            )?;
            Ok(error_report)
        },
    )
    .await;

    let error = if let Ok(error_report) = error_report {
        // Daemon wrote an error and most likely quit.
        let daemon_error: bz_error::Error = error_report.into();
        if daemon_error.has_tag(ErrorTag::DaemonStateInitFailed) {
            // If error is in this stage of daemon init, exclude connection error details/workaround message.
            // TODO(ctolliday) always hide connection error details/workaround message if there is a structured error from daemon.
            return daemon_error;
        }
        daemon_error
    } else {
        // Daemon crashed or panicked, or is still running but can't be connected to.
        let stderr = paths
            .daemon_dir()
            .and_then(|dir| {
                let stderr = std::fs::read(dir.buckd_stderr())?;
                Ok(String::from_utf8_lossy(&stderr).into_owned())
            })
            .unwrap_or_else(|_| "<none>".to_owned());

        let stderr = truncate(&stderr, 64000);
        let error = error
            .context(format!(
                "Error connecting to the daemon, daemon stderr follows:\n{stderr}"
            ))
            .tag([ErrorTag::DaemonConnect]);

        classify_server_stderr(error, &stderr)
    };
    let delete_commad = if cfg!(windows) {
        "rmdir /s /q %USERPROFILE%\\.buck\\buckd"
    } else {
        "rm -rf ~/.buck/buckd"
    };
    // FIXME(ctolliday) we should check if the pid at daemon_dir.buckd_pid is still running before suggesting bz kill.
    let error_message = format!(
        "Failed to connect to buck daemon.
    Try running `bz kill` and your command afterwards.
    Alternatively, try running `{delete_commad}` and your command afterwards"
    );
    error.context(error_message)
}

fn is_nested_invocation(
    bz_daemon_uuid: Option<&String>,
    daemon: &bz_cli_proto::DaemonConstraints,
) -> bool {
    bz_daemon_uuid == Some(&daemon.daemon_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constraints(trace_io_enabled: bool) -> bz_cli_proto::DaemonConstraints {
        bz_cli_proto::DaemonConstraints {
            version: "version".to_owned(),
            user_version: Some("test".to_owned()),
            daemon_id: "foo".to_owned(),
            extra: Some(bz_cli_proto::ExtraDaemonConstraints {
                trace_io_enabled,
                materializer_state_identity: None,
            }),
            daemon_startup_config: Some(
                serde_json::to_string(&DaemonStartupConfig::testing_empty()).unwrap(),
            ),
        }
    }

    fn request(desired_trace_io_state: DesiredTraceIoState) -> DaemonConstraintsRequest {
        DaemonConstraintsRequest {
            version: "version".to_owned(),
            user_version: Some("test".to_owned()),
            desired_trace_io_state,
            nested_invocation_daemon_uuid: None,
            reject_daemon: None,
            reject_materializer_state: None,
            daemon_startup_config: DaemonStartupConfig::testing_empty(),
        }
    }

    #[test]
    fn test_startup_init_timeout_defaults_to_startup_timeout() {
        assert_eq!(
            buckd_startup_init_timeout_from_values(42, None),
            Duration::from_secs(42)
        );
        assert_eq!(
            buckd_startup_init_timeout_from_values(42, Some(5)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn test_constraints_equal_for_same_constraints() {
        let req = request(DesiredTraceIoState::Enabled);
        let daemon = constraints(true);
        assert!(req.satisfied(&daemon).is_ok());
    }

    #[test]
    fn test_constraints_equal_for_trace_io_existing() {
        let req = request(DesiredTraceIoState::Existing);
        let daemon = constraints(true);
        assert!(req.satisfied(&daemon).is_ok());
    }

    #[test]
    fn test_constraints_unequal_for_trace_io() {
        let req = request(DesiredTraceIoState::Disabled);
        let daemon = constraints(true);
        assert!(req.satisfied(&daemon).is_err());
    }

    #[test]
    fn test_trace_io_is_enabled() {
        let c = request(DesiredTraceIoState::Enabled);
        assert!(c.is_trace_io_requested());

        let c = request(DesiredTraceIoState::Disabled);
        assert!(!c.is_trace_io_requested());
    }

    #[test]
    fn test_reject_daemon() {
        let mut req = DaemonConstraintsRequest {
            version: "foo".to_owned(),
            user_version: None,
            desired_trace_io_state: DesiredTraceIoState::Existing,
            reject_daemon: None,
            nested_invocation_daemon_uuid: None,
            reject_materializer_state: None,
            daemon_startup_config: DaemonStartupConfig::testing_empty(),
        };

        let daemon = bz_cli_proto::DaemonConstraints {
            version: "foo".to_owned(),
            user_version: None,
            daemon_id: "ddd".to_owned(),
            extra: None,
            daemon_startup_config: Some(
                serde_json::to_string(&DaemonStartupConfig::testing_empty()).unwrap(),
            ),
        };

        assert!(req.satisfied(&daemon).is_ok());
        req.reject_daemon = Some("zzz".to_owned());
        assert!(req.satisfied(&daemon).is_ok());
        req.reject_daemon = Some("ddd".to_owned());
        assert!(req.satisfied(&daemon).is_err());
    }

    #[test]
    fn test_reject_materializer_state() {
        let mut req = DaemonConstraintsRequest {
            version: "foo".to_owned(),
            user_version: None,
            desired_trace_io_state: DesiredTraceIoState::Existing,
            reject_daemon: None,
            nested_invocation_daemon_uuid: None,
            reject_materializer_state: None,
            daemon_startup_config: DaemonStartupConfig::testing_empty(),
        };

        let daemon = bz_cli_proto::DaemonConstraints {
            version: "foo".to_owned(),
            user_version: None,
            daemon_id: "ddd".to_owned(),
            extra: Some(bz_cli_proto::ExtraDaemonConstraints {
                trace_io_enabled: false,
                materializer_state_identity: Some("mmm".to_owned()),
            }),
            daemon_startup_config: Some(
                serde_json::to_string(&DaemonStartupConfig::testing_empty()).unwrap(),
            ),
        };

        assert!(req.satisfied(&daemon).is_ok());
        req.reject_materializer_state = Some("zzz".to_owned());
        assert!(req.satisfied(&daemon).is_ok());
        req.reject_materializer_state = Some("mmm".to_owned());
        assert!(req.satisfied(&daemon).is_err());
    }

    #[test]
    fn test_daemon_buster() {
        let mut req = DaemonConstraintsRequest {
            version: "foo".to_owned(),
            user_version: None,
            desired_trace_io_state: DesiredTraceIoState::Existing,
            reject_daemon: None,
            nested_invocation_daemon_uuid: None,
            reject_materializer_state: None,
            daemon_startup_config: DaemonStartupConfig::testing_empty(),
        };

        let daemon = bz_cli_proto::DaemonConstraints {
            version: "foo".to_owned(),
            user_version: None,
            daemon_id: "ddd".to_owned(),
            extra: Some(bz_cli_proto::ExtraDaemonConstraints {
                trace_io_enabled: false,
                materializer_state_identity: Some("mmm".to_owned()),
            }),
            daemon_startup_config: Some(
                serde_json::to_string(&DaemonStartupConfig::testing_empty()).unwrap(),
            ),
        };

        assert!(req.satisfied(&daemon).is_ok());
        req.daemon_startup_config.daemon_buster = Some("1".to_owned());
        assert!(req.satisfied(&daemon).is_err());
    }

    #[test]
    fn test_constraints_nested_invocation_diff_user_version() {
        let mut req = DaemonConstraintsRequest {
            version: "foo".to_owned(),
            user_version: Some("fake_version_1".to_owned()),
            desired_trace_io_state: DesiredTraceIoState::Existing,
            nested_invocation_daemon_uuid: None,
            reject_daemon: None,
            reject_materializer_state: None,
            daemon_startup_config: DaemonStartupConfig::testing_empty(),
        };

        let daemon = bz_cli_proto::DaemonConstraints {
            version: "foo".to_owned(),
            user_version: Some("fake_version_2".to_owned()),
            daemon_id: "ddd".to_owned(),
            extra: Some(bz_cli_proto::ExtraDaemonConstraints {
                trace_io_enabled: false,
                materializer_state_identity: Some("mmm".to_owned()),
            }),
            daemon_startup_config: Some(
                serde_json::to_string(&DaemonStartupConfig::testing_empty()).unwrap(),
            ),
        };

        assert!(req.satisfied(&daemon).is_err());

        req.nested_invocation_daemon_uuid = Some("ddd".to_owned());
        assert!(req.satisfied(&daemon).is_ok());
    }
}
