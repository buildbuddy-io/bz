/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::Hash;
use std::hash::Hasher;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bz_common::client_utils::get_channel_uds;
use bz_common::client_utils::retrying;
use bz_common::liveliness_observer::LivelinessGuard;
use bz_common::liveliness_observer::LivelinessObserver;
use bz_error::ErrorTag;
use bz_error::bz_error;
use bz_events::dispatch::EventDispatcher;
use bz_execute::execute::kind::CommandExecutionKind;
use bz_execute::execute::manager::CommandExecutionManagerExt;
use bz_execute::execute::manager::CommandExecutionManagerWithClaim;
use bz_execute::execute::output::CommandStdStreams;
use bz_execute::execute::request::CommandExecutionRequest;
use bz_execute::execute::request::WorkerId;
use bz_execute::execute::request::WorkerProtocol;
use bz_execute::execute::request::WorkerSpec;
use bz_execute::execute::result::CommandExecutionMetadata;
use bz_execute::execute::result::CommandExecutionResult;
use bz_execute_local::CommandResult;
use bz_execute_local::GatherOutputStatus;
use bz_execute_local::StdRedirectPaths;
use bz_fs::error::IoResultExt;
use bz_fs::fs_util;
use bz_fs::paths::abs_norm_path::AbsNormPath;
use bz_fs::paths::abs_norm_path::AbsNormPathBuf;
use bz_fs::paths::file_name::FileName;
use bz_hash::BuckDashMap;
use bz_hash::BuckIndexMap;
use bz_hash::StdBuckHashMap;
use bz_util::time_span::TimeSpan;
use bz_worker_proto::ExecuteCommand;
use bz_worker_proto::ExecuteCommandStream;
use bz_worker_proto::ExecuteResponse;
use bz_worker_proto::ExecuteResponseStream;
use bz_worker_proto::execute_command::EnvironmentEntry;
use bz_worker_proto::worker_client;
use bz_worker_proto::worker_streaming_client;
use dupe::Dupe;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use host_sharing::HostSharingBroker;
use host_sharing::HostSharingStrategy;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::Status;
use tonic::transport::Channel;

use crate::executors::local::ForkserverAccess;

// Request to the worker can contain a command to run the tests, and that command
// might explicitly spell out the lists of tests to run. In case of a large target
// and/or very lengthy test names, this part of a request can be very large.
// Response of the worker can contain a stderr of the command => also can be very large.
// To avoid unnecessarily limiting the usecases, let's use a maximum allowed value
// for the request/response.
const MAX_MESSAGE_SIZE_BYTES: usize = usize::MAX;

#[derive(Clone, PartialEq, prost::Message)]
struct BazelWorkRequest {
    #[prost(string, repeated, tag = "1")]
    arguments: Vec<String>,
    #[prost(int32, tag = "3")]
    request_id: i32,
    #[prost(bool, tag = "4")]
    cancel: bool,
    #[prost(int32, tag = "5")]
    verbosity: i32,
    #[prost(string, tag = "6")]
    sandbox_dir: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct BazelWorkResponse {
    #[prost(int32, tag = "1")]
    exit_code: i32,
    #[prost(string, tag = "2")]
    output: String,
    #[prost(int32, tag = "3")]
    request_id: i32,
    #[prost(bool, tag = "4")]
    was_cancelled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct WorkerCacheKey {
    id: WorkerId,
    protocol: WorkerProtocol,
    root: String,
    instance: u64,
    multiplex: bool,
    sandboxed: bool,
    exe: Vec<String>,
    env: Vec<(String, String)>,
}

impl WorkerCacheKey {
    fn new(worker_spec: &WorkerSpec, root: &AbsNormPath, instance: u64) -> Self {
        Self {
            id: worker_spec.id,
            protocol: worker_spec.protocol,
            root: root.to_string(),
            instance,
            multiplex: worker_spec.protocol == WorkerProtocol::Bazel
                && worker_spec.concurrency.unwrap_or(1) > 1,
            sandboxed: worker_spec.bazel_worker_sandboxing,
            exe: worker_spec.exe.clone(),
            env: worker_spec
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

#[derive(bz_error::Error, Debug)]
#[buck2(tag = WorkerInit)]
pub enum WorkerInitError {
    #[error("Worker failed to spawn: {0}")]
    SpawnFailed(String),
    #[error("Worker exited before connecting")]
    EarlyExit {
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    #[error("Worker failed to connect within `{0:.2}` seconds: {1}")]
    ConnectionTimeout(f64, String),
    /// Any error not related to worker behavior
    #[error("Error initializing worker `{0}`")]
    InternalError(bz_error::Error),
}

#[cfg_attr(windows, allow(dead_code))]
impl WorkerInitError {
    pub(crate) fn to_command_execution_result(
        &self,
        request: &CommandExecutionRequest,
        manager: CommandExecutionManagerWithClaim,
    ) -> CommandExecutionResult {
        let worker_spec = request.worker().as_ref().unwrap();
        let execution_kind = CommandExecutionKind::LocalWorkerInit {
            command: worker_spec.exe.clone(),
            env: request.env().clone(),
        };
        let manager = manager.with_execution_kind(execution_kind.clone());

        match self {
            WorkerInitError::EarlyExit {
                exit_code,
                stdout,
                stderr,
            } => {
                let std_streams = CommandStdStreams::Local {
                    stdout: stdout.to_owned().into(),
                    stderr: stderr.to_owned().into(),
                };
                // TODO(ctolliday) this should be a new failure type (worker_init_failure), not conflated with a "command failure" which
                // implies that it is the primary command and that exit code != 0
                manager.failure(
                    execution_kind,
                    BuckIndexMap::default(),
                    std_streams,
                    *exit_code,
                    CommandExecutionMetadata::empty(TimeSpan::empty_now()),
                    None,
                )
            }
            // TODO(ctolliday) as above, use a new failure type (worker_init_failure) that indicates this is a worker initialization error.
            WorkerInitError::ConnectionTimeout(..) | WorkerInitError::SpawnFailed(..) => manager
                .failure(
                    execution_kind,
                    BuckIndexMap::default(),
                    CommandStdStreams::Local {
                        stdout: Default::default(),
                        stderr: format!("Error initializing worker: {self}").into_bytes(),
                    },
                    None,
                    CommandExecutionMetadata::empty(TimeSpan::empty_now()),
                    None,
                ),
            WorkerInitError::InternalError(error) => {
                manager.error("get_worker_failed", error.clone())
            }
        }
    }
}

#[cfg(unix)]
fn spawn_via_forkserver(
    forkserver: ForkserverAccess,
    exe: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    working_directory: AbsNormPathBuf,
    liveliness_observer: impl LivelinessObserver + 'static,
    std_redirects: &StdRedirectPaths,
    socket_path: &AbsNormPathBuf,
    graceful_shutdown_timeout_s: Option<u32>,
) -> JoinHandle<bz_error::Result<GatherOutputStatus>> {
    use std::os::unix::ffi::OsStrExt;

    use crate::executors::local::apply_local_execution_environment;

    let ForkserverAccess::Client(forkserver) = forkserver else {
        unreachable!("Worker should not be spawned without a forkserver")
    };

    let std_redirects = std_redirects.clone();

    let socket_path = socket_path.clone();
    tokio::spawn(async move {
        let mut req = bz_forkserver_proto::CommandRequest {
            exe: exe.as_bytes().into(),
            argv: args.into_iter().map(|s| s.as_bytes().into()).collect(),
            cwd: Some(bz_forkserver_proto::WorkingDirectory {
                path: working_directory.as_path().as_os_str().as_bytes().into(),
            }),
            env: vec![],
            timeout: None,
            enable_miniperf: false,
            std_redirects: Some(bz_forkserver_proto::command_request::StdRedirectPaths {
                stdout: std_redirects.stdout.to_string(),
                stderr: std_redirects.stderr.to_string(),
            }),
            graceful_shutdown_timeout_s,
            command_cgroup: None,
            network_access: None,
        };
        apply_local_execution_environment(&mut req, &working_directory, env, None);
        let res = forkserver
            .execute(
                req,
                async move { liveliness_observer.while_alive().await },
                futures::stream::pending(),
            )
            .await
            .map(|CommandResult { status, .. }| status);

        // Socket is created by worker so won't exist if initialization fails.
        if fs_util::try_exists(&socket_path)? {
            // TODO(ctolliday) delete directory (after logs are moved to buck-out)
            fs_util::remove_file(&socket_path).categorize_internal()?;
        }
        res
    })
}

#[cfg(not(unix))]
fn spawn_via_forkserver(
    _forkserver: ForkserverAccess,
    _exe: OsString,
    _args: Vec<OsString>,
    _env: Vec<(OsString, OsString)>,
    _working_directory: AbsNormPathBuf,
    _liveliness_observer: impl LivelinessObserver + 'static,
    _std_redirects: &StdRedirectPaths,
    _socket_path: &AbsNormPathBuf,
    _graceful_shutdown_timeout_s: Option<u32>,
) -> JoinHandle<bz_error::Result<GatherOutputStatus>> {
    unreachable!("workers should not be initialized off unix")
}

fn encode_varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

async fn read_varint(reader: &mut ChildStdout) -> bz_error::Result<Option<usize>> {
    let mut value = 0usize;
    let mut shift = 0usize;
    for index in 0..10 {
        let byte = match reader.read_u8().await {
            Ok(byte) => byte,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && index == 0 => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        value |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            return Ok(Some(value));
        }
        shift += 7;
    }
    Err(bz_error!(
        ErrorTag::Input,
        "Invalid Bazel worker response length varint"
    ))
}

async fn read_bazel_work_response(
    reader: &mut ChildStdout,
) -> bz_error::Result<Option<BazelWorkResponse>> {
    let Some(len) = read_varint(reader).await? else {
        return Ok(None);
    };
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(prost::Message::decode(&*buf)?))
}

async fn write_bazel_work_request(
    writer: &mut ChildStdin,
    request: BazelWorkRequest,
) -> bz_error::Result<()> {
    let mut body = Vec::new();
    prost::Message::encode(&request, &mut body)?;
    let mut frame = Vec::with_capacity(body.len() + 10);
    encode_varint(body.len(), &mut frame);
    frame.extend(body);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

async fn spawn_bazel_worker(
    worker_id: WorkerId,
    worker_key_hash: u64,
    mut args: Vec<String>,
    env: impl IntoIterator<Item = (OsString, OsString)>,
    multiplex: bool,
    root: &AbsNormPath,
    dispatcher: EventDispatcher,
) -> Result<WorkerHandle, WorkerInitError> {
    let dir_name = format!(
        "{}-{}-{:016x}",
        dispatcher.trace_id(),
        worker_id,
        worker_key_hash
    );
    let worker_dir = AbsNormPathBuf::from("/tmp/bz_worker".to_owned())
        .map_err(WorkerInitError::InternalError)?
        .join(FileName::unchecked_new(&dir_name));
    if fs_util::try_exists(&worker_dir).map_err(|e| WorkerInitError::InternalError(e.into()))? {
        return Err(WorkerInitError::InternalError(bz_error!(
            bz_error::ErrorTag::WorkerDirectoryExists,
            "Directory for worker already exists: {:?}",
            worker_dir
        )));
    }
    let std_redirects = StdRedirectPaths {
        stdout: worker_dir.join(FileName::unchecked_new("stdout")),
        stderr: worker_dir.join(FileName::unchecked_new("stderr")),
    };
    fs_util::create_dir_all(&worker_dir).map_err(|e| WorkerInitError::InternalError(e.into()))?;

    args.push("--persistent_worker".to_owned());
    tracing::info!(
        "Starting Bazel protocol worker with logs at {}:\n$ {}\n",
        worker_dir,
        args.join(" ")
    );

    let stderr = std::fs::File::create(std_redirects.stderr.as_path())
        .map_err(|e| WorkerInitError::InternalError(e.into()))?;
    let mut command = tokio::process::Command::new(&args[0]);
    command
        .args(&args[1..])
        .current_dir(root.as_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true)
        .env("PWD", root.as_path());
    for (key, value) in env {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .map_err(|e| WorkerInitError::SpawnFailed(e.to_string()))?;
    let stdin = child.stdin.take().ok_or_else(|| {
        WorkerInitError::InternalError(bz_error!(
            ErrorTag::Tier0,
            "Bazel protocol worker stdin was not piped"
        ))
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        WorkerInitError::InternalError(bz_error!(
            ErrorTag::Tier0,
            "Bazel protocol worker stdout was not piped"
        ))
    })?;

    let (liveliness_observer, liveliness_guard) = LivelinessGuard::create();
    let (child_exited_observer, child_exited_guard) = LivelinessGuard::create();
    tokio::spawn(async move {
        tokio::select! {
            _ = child.wait() => {}
            _ = liveliness_observer.while_alive() => {
                let _ignored = child.kill().await;
                let _ignored = child.wait().await;
            }
        }
        drop(child_exited_guard);
    });

    let waiters: Arc<BuckDashMap<i32, tokio::sync::oneshot::Sender<BazelWorkResponse>>> =
        Default::default();
    let (stdout_closed_observer, stdout_closed_guard) = LivelinessGuard::create();
    {
        let waiters = waiters.dupe();
        tokio::spawn(async move {
            loop {
                match read_bazel_work_response(&mut stdout).await {
                    Ok(Some(response)) => match waiters.remove(&response.request_id) {
                        Some(waiter) => {
                            let _ignored = waiter.1.send(response);
                        }
                        None => {
                            tracing::warn!(
                                request_id = response.request_id,
                                "Missing waiter for Bazel worker response"
                            );
                        }
                    },
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(
                            error = e.to_string(),
                            "Error reading Bazel worker response"
                        );
                        break;
                    }
                }
            }
            drop(stdout_closed_guard);
        });
    }

    Ok(WorkerHandle::new(
        WorkerClient::Bazel(BazelWorkerClient {
            ids: Default::default(),
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            singleplex_lock: Arc::new(tokio::sync::Mutex::new(())),
            waiters,
            stdout_closed_observer,
            multiplex,
        }),
        child_exited_observer,
        std_redirects,
        liveliness_guard,
    ))
}

async fn spawn_worker(
    worker_id: WorkerId,
    worker_key_hash: u64,
    protocol: WorkerProtocol,
    args: Vec<String>,
    env: impl IntoIterator<Item = (OsString, OsString)>,
    streaming: bool,
    root: &AbsNormPath,
    forkserver: ForkserverAccess,
    dispatcher: EventDispatcher,
    graceful_shutdown_timeout_s: Option<u32>,
) -> Result<WorkerHandle, WorkerInitError> {
    if protocol == WorkerProtocol::Bazel {
        return spawn_bazel_worker(
            worker_id,
            worker_key_hash,
            args,
            env,
            streaming,
            root,
            dispatcher,
        )
        .await;
    }

    // Use fixed length path at /tmp to avoid 108 character limit for unix domain sockets
    let dir_name = format!(
        "{}-{}-{:016x}",
        dispatcher.trace_id(),
        worker_id,
        worker_key_hash
    );
    let worker_dir = AbsNormPathBuf::from("/tmp/bz_worker".to_owned())
        .map_err(WorkerInitError::InternalError)?
        .join(FileName::unchecked_new(&dir_name));
    let socket_path = worker_dir.join(FileName::unchecked_new("socket"));
    if fs_util::try_exists(&worker_dir).map_err(|e| WorkerInitError::InternalError(e.into()))? {
        return Err(WorkerInitError::InternalError(bz_error!(
            bz_error::ErrorTag::WorkerDirectoryExists,
            "Directory for worker already exists: {:?}",
            worker_dir
        )));
    }
    // TODO(ctolliday) put these in buck-out/<iso>/workers and only use /tmp dir for sockets
    let std_redirects = StdRedirectPaths {
        stdout: worker_dir.join(FileName::unchecked_new("stdout")),
        stderr: worker_dir.join(FileName::unchecked_new("stderr")),
    };
    fs_util::create_dir_all(&worker_dir).map_err(|e| WorkerInitError::InternalError(e.into()))?;

    tracing::info!(
        "Starting worker with logs at {}:\n$ {}\n",
        worker_dir,
        args.join(" ")
    );

    let worker_env = vec![("WORKER_SOCKET", socket_path.as_os_str())]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
    let env: Vec<(OsString, OsString)> = env.into_iter().chain(worker_env).collect();

    let (liveliness_observer, liveliness_guard) = LivelinessGuard::create();

    let spawn_fut = spawn_via_forkserver(
        forkserver,
        OsString::from(args[0].clone()),
        args[1..].iter().map(OsString::from).collect(),
        env.clone(),
        root.to_buf(),
        liveliness_observer,
        &std_redirects,
        &socket_path,
        graceful_shutdown_timeout_s,
    );

    let initial_delay = Duration::from_millis(50);
    let max_delay = Duration::from_millis(500);
    // Might want to make this configurable, and/or measure impact of worker initialization on critical path
    let timeout = Duration::from_mins(1);
    let (channel, check_exit) = {
        let socket_path = &socket_path;

        let connect = retrying(initial_delay, max_delay, timeout, move || {
            // TODO(ctolliday) T153604304
            // add handshake over grpc before returning a handle, to make sure the worker is responding
            get_channel_uds(socket_path, false)
        });

        let check_exit = async move {
            spawn_fut
                .await
                .map_err(|e| WorkerInitError::InternalError(e.into()))?
        }
        .boxed();
        futures::pin_mut!(connect);

        match futures::future::select(connect, check_exit).await {
            futures::future::Either::Left((connection_result, check_exit)) => {
                match connection_result {
                    Ok(channel) => Ok((channel, check_exit)),
                    Err(e) => Err(WorkerInitError::ConnectionTimeout(
                        timeout.as_secs_f64(),
                        e.to_string(),
                    )),
                }
            }
            futures::future::Either::Right((command_result, _)) => Err(match command_result {
                Ok(GatherOutputStatus::SpawnFailed(e)) => WorkerInitError::SpawnFailed(e),
                Ok(GatherOutputStatus::Finished { exit_code, .. }) => {
                    let stdout = fs_util::read_to_string(&std_redirects.stdout)
                        .categorize_internal()
                        .map_err(|e| WorkerInitError::InternalError(e.into()))?;
                    let stderr = fs_util::read_to_string(&std_redirects.stderr)
                        .categorize_internal()
                        .map_err(|e| WorkerInitError::InternalError(e.into()))?;
                    WorkerInitError::EarlyExit {
                        exit_code: Some(exit_code),
                        stdout,
                        stderr,
                    }
                }
                Ok(GatherOutputStatus::Cancelled | GatherOutputStatus::TimedOut(_)) => {
                    WorkerInitError::InternalError(bz_error!(
                        bz_error::ErrorTag::WorkerCancelled,
                        "Worker cancelled by buck"
                    ))
                }
                Err(e) => WorkerInitError::InternalError(e),
            }),
        }?
    };

    let (child_exited_observer, child_exited_guard) = LivelinessGuard::create();
    tokio::spawn(async move {
        drop(check_exit.await);
        drop(child_exited_guard);
    });

    tracing::info!("Connected to socket for spawned worker: {}", socket_path);
    let client = if streaming {
        WorkerClient::stream(channel)
            .await
            .map_err(|e| WorkerInitError::SpawnFailed(e.to_string()))?
    } else {
        WorkerClient::single(channel)
    };

    Ok(WorkerHandle::new(
        client,
        child_exited_observer,
        std_redirects,
        liveliness_guard,
    ))
}

type WorkerFuture = Shared<BoxFuture<'static, Result<Arc<WorkerHandle>, Arc<WorkerInitError>>>>;

pub struct WorkerPool {
    workers: Arc<parking_lot::Mutex<StdBuckHashMap<WorkerCacheKey, WorkerFuture>>>,
    brokers: Arc<parking_lot::Mutex<StdBuckHashMap<WorkerCacheKey, Arc<HostSharingBroker>>>>,
    next_instances: Arc<parking_lot::Mutex<StdBuckHashMap<WorkerCacheKey, Arc<AtomicU64>>>>,
    graceful_shutdown_timeout_s: Option<u32>,
}

impl WorkerPool {
    const DEFAULT_MAX_SINGLEPLEX_BAZEL_WORKERS: usize = 4;

    pub fn new(graceful_shutdown_timeout_s: Option<u32>) -> WorkerPool {
        tracing::info!("Creating new WorkerPool");
        WorkerPool {
            workers: Arc::new(parking_lot::Mutex::new(StdBuckHashMap::default())),
            brokers: Arc::new(parking_lot::Mutex::new(StdBuckHashMap::default())),
            next_instances: Arc::new(parking_lot::Mutex::new(StdBuckHashMap::default())),
            graceful_shutdown_timeout_s,
        }
    }

    fn worker_key(worker_spec: &WorkerSpec, root: &AbsNormPath, instance: u64) -> WorkerCacheKey {
        WorkerCacheKey::new(worker_spec, root, instance)
    }

    fn base_worker_key(worker_spec: &WorkerSpec, root: &AbsNormPath) -> WorkerCacheKey {
        Self::worker_key(worker_spec, root, 0)
    }

    fn max_instances(worker_spec: &WorkerSpec) -> usize {
        if worker_spec.protocol == WorkerProtocol::Bazel
            && worker_spec.concurrency.unwrap_or(1) == 1
        {
            Self::DEFAULT_MAX_SINGLEPLEX_BAZEL_WORKERS
        } else {
            1
        }
    }

    pub fn get_worker_broker(
        &self,
        worker_spec: &WorkerSpec,
        root: &AbsNormPath,
    ) -> Option<Arc<HostSharingBroker>> {
        let mut brokers = self.brokers.lock();
        let worker_key = Self::base_worker_key(worker_spec, root);
        worker_spec.concurrency.map(|concurrency| {
            let concurrency = if Self::max_instances(worker_spec) > 1 {
                Self::max_instances(worker_spec)
            } else {
                concurrency
            };
            brokers
                .entry(worker_key)
                .or_insert_with(|| {
                    Arc::new(HostSharingBroker::new(
                        HostSharingStrategy::Fifo,
                        concurrency,
                    ))
                })
                .clone()
        })
    }

    pub fn get_or_create_worker(
        &self,
        worker_spec: &WorkerSpec,
        env: impl IntoIterator<Item = (OsString, OsString)>,
        root: &AbsNormPath,
        forkserver: ForkserverAccess,
        dispatcher: EventDispatcher,
    ) -> (bool, WorkerFuture) {
        let mut workers = self.workers.lock();
        let base_worker_key = Self::base_worker_key(worker_spec, root);
        let max_instances = Self::max_instances(worker_spec);
        let instance = if max_instances > 1 {
            let next_instance = self
                .next_instances
                .lock()
                .entry(base_worker_key)
                .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                .clone();
            next_instance.fetch_add(1, Ordering::Relaxed) % max_instances as u64
        } else {
            0
        };
        let worker_key = Self::worker_key(worker_spec, root, instance);
        if let Some(worker_fut) = workers.get(&worker_key) {
            (false, worker_fut.clone())
        } else {
            let worker_id = worker_spec.id;
            let mut hasher = DefaultHasher::new();
            worker_key.hash(&mut hasher);
            let worker_key_hash = hasher.finish();
            let protocol = worker_spec.protocol;
            let args = worker_spec.exe.to_vec();
            let streaming = if worker_spec.protocol == WorkerProtocol::Bazel {
                worker_spec.concurrency.unwrap_or(1) > 1
            } else {
                worker_spec.streaming
            };
            let root = root.to_buf();
            let env: Vec<(OsString, OsString)> = env.into_iter().collect();
            let graceful_shutdown_timeout_s = self.graceful_shutdown_timeout_s;
            let fut = async move {
                match spawn_worker(
                    worker_id,
                    worker_key_hash,
                    protocol,
                    args,
                    env,
                    streaming,
                    &root,
                    forkserver,
                    dispatcher,
                    graceful_shutdown_timeout_s,
                )
                .await
                {
                    Ok(worker) => Ok(Arc::new(worker)),
                    Err(e) => Err(Arc::new(e)),
                }
            }
            .boxed()
            .shared();

            workers.insert(worker_key, fut.clone());
            (true, fut)
        }
    }
}

#[derive(Clone)]
enum WorkerClient {
    Single(worker_client::WorkerClient<Channel>),
    Stream {
        ids: Arc<AtomicU64>,
        stream: UnboundedSender<ExecuteCommandStream>,
        stream_closed_observer: Arc<dyn LivelinessObserver>,
        waiters: Arc<BuckDashMap<u64, tokio::sync::oneshot::Sender<ExecuteResponseStream>>>,
    },
    Bazel(BazelWorkerClient),
}

#[derive(Clone)]
struct BazelWorkerClient {
    ids: Arc<AtomicU64>,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    singleplex_lock: Arc<tokio::sync::Mutex<()>>,
    waiters: Arc<BuckDashMap<i32, tokio::sync::oneshot::Sender<BazelWorkResponse>>>,
    stdout_closed_observer: Arc<dyn LivelinessObserver>,
    multiplex: bool,
}

impl WorkerClient {
    fn single(channel: Channel) -> Self {
        Self::Single(
            worker_client::WorkerClient::new(channel)
                .max_encoding_message_size(MAX_MESSAGE_SIZE_BYTES)
                .max_decoding_message_size(MAX_MESSAGE_SIZE_BYTES),
        )
    }

    async fn stream(channel: Channel) -> Result<Self, Status> {
        let mut client = worker_streaming_client::WorkerStreamingClient::new(channel)
            .max_encoding_message_size(MAX_MESSAGE_SIZE_BYTES)
            .max_decoding_message_size(MAX_MESSAGE_SIZE_BYTES);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stream = client
            .execute_stream(tonic::Request::new(UnboundedReceiverStream::new(rx)))
            .await?;
        let waiters: Arc<BuckDashMap<u64, tokio::sync::oneshot::Sender<ExecuteResponseStream>>> =
            Default::default();
        let (stream_closed_observer, stream_closed_guard) = LivelinessGuard::create();
        {
            let waiters = waiters.dupe();
            tokio::spawn(async move {
                use futures::StreamExt;

                let mut stream = stream.into_inner();
                while let Some(response) = stream.next().await {
                    match response {
                        Ok(response) => {
                            match waiters.remove(&response.id) {
                                Some(waiter) => {
                                    let id = response.id;
                                    if waiter.1.send(response).is_err() {
                                        tracing::warn!(
                                            id = id,
                                            "Error passing streaming worker response to waiter"
                                        );
                                    }
                                }
                                None => {
                                    tracing::warn!(
                                        id = response.id,
                                        "Missing waiter for streaming worker response",
                                    );
                                }
                            };
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = e.to_string(),
                                "Response error in worker stream"
                            );
                        }
                    };
                }
                drop(stream_closed_guard);
            });
        }
        Ok(Self::Stream {
            ids: Default::default(),
            stream: tx,
            stream_closed_observer,
            waiters,
        })
    }

    async fn execute(
        &mut self,
        request: ExecuteCommand,
        bazel_sandbox_dir: Option<String>,
    ) -> bz_error::Result<ExecuteResponse> {
        match self {
            Self::Single(client) => Self::execute_with_retry(client, request).await,
            Self::Bazel(client) => client.execute(request, bazel_sandbox_dir).await,
            Self::Stream {
                ids,
                stream,
                stream_closed_observer,
                waiters,
            } => {
                let id = ids.fetch_add(1, Ordering::Acquire);
                let req = ExecuteCommandStream {
                    request: Some(request),
                    id,
                };
                let (tx, rx) = tokio::sync::oneshot::channel();
                waiters.insert(id, tx);
                stream.send(req)?;
                tokio::select! {
                    response = rx => Ok(response.map(|response| response.response.unwrap())?),
                    _ = stream_closed_observer.while_alive() => {
                        Err(bz_error::bz_error!(ErrorTag::Tier0, "Stream closed while waiting for response"))
                    },
                }
            }
        }
    }

    async fn execute_with_retry(
        client: &mut worker_client::WorkerClient<Channel>,
        request: ExecuteCommand,
    ) -> bz_error::Result<ExecuteResponse> {
        use tokio_retry::strategy::ExponentialBackoff;

        let retry_delays = ExponentialBackoff::from_millis(100)
            .max_delay(Duration::from_millis(500))
            .take(5);

        let mut last_err = None;
        for delay in std::iter::once(Duration::ZERO).chain(retry_delays) {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            match client.execute(request.clone()).await {
                Ok(response) => return Ok(response.into_inner()),
                Err(status) if status.code() == tonic::Code::Unavailable => {
                    tracing::warn!("Worker connection unavailable, retrying: {:?}", status);
                    last_err = Some(status);
                }
                Err(status) => return Err(status.into()),
            }
        }

        Err(last_err
            .expect("retry loop must have run at least once")
            .into())
    }
}

impl BazelWorkerClient {
    async fn execute(
        &self,
        request: ExecuteCommand,
        sandbox_dir: Option<String>,
    ) -> bz_error::Result<ExecuteResponse> {
        let ExecuteCommand {
            argv,
            env: _,
            timeout_s,
        } = request;
        let _singleplex_guard = if self.multiplex {
            None
        } else {
            Some(self.singleplex_lock.lock().await)
        };
        let request_id = if self.multiplex {
            self.ids.fetch_add(1, Ordering::Acquire) as i32 + 1
        } else {
            0
        };
        let arguments = argv
            .into_iter()
            .map(|arg| {
                String::from_utf8(arg).map_err(|e| {
                    bz_error!(ErrorTag::Input, "Bazel worker arguments must be UTF-8: {e}")
                })
            })
            .collect::<bz_error::Result<Vec<_>>>()?;

        let work_request = BazelWorkRequest {
            arguments,
            request_id,
            cancel: false,
            verbosity: 0,
            sandbox_dir: sandbox_dir.unwrap_or_default(),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.waiters.insert(request_id, tx).is_some() {
            return Err(bz_error!(
                ErrorTag::Tier0,
                "Bazel worker request id collision: {request_id}"
            ));
        }
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(e) = write_bazel_work_request(&mut stdin, work_request).await {
                self.waiters.remove(&request_id);
                return Err(e);
            }
        }

        let wait_for_response = async {
            tokio::select! {
                response = rx => Ok(response?),
                _ = self.stdout_closed_observer.while_alive() => {
                    Err(bz_error!(ErrorTag::Tier0, "Bazel worker stdout closed while waiting for response"))
                },
            }
        };
        let response = if let Some(timeout_s) = timeout_s {
            match tokio::time::timeout(Duration::from_secs(timeout_s), wait_for_response).await {
                Ok(response) => response?,
                Err(_) => {
                    self.waiters.remove(&request_id);
                    if request_id != 0 {
                        let cancel = BazelWorkRequest {
                            arguments: Vec::new(),
                            request_id,
                            cancel: true,
                            verbosity: 0,
                            sandbox_dir: String::new(),
                        };
                        let mut stdin = self.stdin.lock().await;
                        let _ignored = write_bazel_work_request(&mut stdin, cancel).await;
                    }
                    return Ok(ExecuteResponse {
                        exit_code: 1,
                        stderr: String::new(),
                        timed_out_after_s: Some(timeout_s),
                    });
                }
            }
        } else {
            wait_for_response.await?
        };

        Ok(ExecuteResponse {
            exit_code: response.exit_code,
            stderr: response.output,
            timed_out_after_s: None,
        })
    }
}

pub struct WorkerHandle {
    client: WorkerClient,
    child_exited_observer: Arc<dyn LivelinessObserver>,
    std_redirects: StdRedirectPaths,
    _liveliness_guard: LivelinessGuard,
}

impl WorkerHandle {
    fn new(
        client: WorkerClient,
        child_exited_observer: Arc<dyn LivelinessObserver>,
        std_redirects: StdRedirectPaths,
        liveliness_guard: LivelinessGuard,
    ) -> Self {
        Self {
            client,
            child_exited_observer,
            std_redirects,
            _liveliness_guard: liveliness_guard,
        }
    }
}

#[cfg(unix)]
fn env_entries(env: &[(OsString, OsString)]) -> Vec<EnvironmentEntry> {
    use std::os::unix::ffi::OsStrExt;
    env.iter()
        .map(|(k, v)| EnvironmentEntry {
            key: k.as_bytes().into(),
            value: v.as_bytes().into(),
        })
        .collect()
}

#[cfg(not(unix))]
fn env_entries(_env: &[(OsString, OsString)]) -> Vec<EnvironmentEntry> {
    unreachable!("worker should not exist off unix")
}

impl WorkerHandle {
    pub async fn exec_cmd(
        &self,
        args: &[String],
        env: Vec<(OsString, OsString)>,
        timeout: Option<Duration>,
        bazel_sandbox_dir: Option<String>,
    ) -> CommandResult {
        tracing::info!(
            "Sending worker command:\nExecuteCommand {{ argv: {:?}, env: {:?} }}\n",
            args,
            env,
        );
        let argv: Vec<Vec<u8>> = args.iter().map(|s| s.as_str().into()).collect();
        let env: Vec<EnvironmentEntry> = env_entries(&env);

        let request = ExecuteCommand {
            argv,
            env,
            timeout_s: timeout.map(|v| v.as_secs()),
        };

        let mut client = self.client.clone();
        let (status, stdout, stderr) = tokio::select! {
            response = client.execute(request, bazel_sandbox_dir) => {
                match response {
                    Ok(exec_response) => {
                        tracing::info!("Worker response:\n{:?}\n", exec_response);
                        if let Some(timeout) = exec_response.timed_out_after_s {
                            (
                                GatherOutputStatus::TimedOut(Duration::from_secs(timeout)),
                                vec![],
                                exec_response.stderr.into(),
                            )
                        } else {
                            (
                                GatherOutputStatus::Finished {
                                    exit_code: exec_response.exit_code,
                                    execution_stats: None,
                                },
                                vec![],
                                exec_response.stderr.into(),
                            )
                        }
                    }
                    Err(err) => {
                        (
                            GatherOutputStatus::SpawnFailed(format!(
                                "Error sending ExecuteCommand to worker: {:?}, see worker logs:\n{}\n{}",
                                err, self.std_redirects.stdout, self.std_redirects.stderr,
                            )),
                            vec![],
                            vec![],
                        )
                    }
                }
            }
            _ = self.child_exited_observer.while_alive() => {
                (
                    GatherOutputStatus::SpawnFailed(format!(
                        "Worker exited while running command, see worker logs:\n{}\n{}",
                        self.std_redirects.stdout, self.std_redirects.stderr,
                    )),
                    vec![],
                    vec![],
                )
            }
        };

        CommandResult {
            status,
            stdout,
            stderr,
            cgroup_result: None,
            orphan_processes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    use bz_worker_proto::ExecuteCommand;
    use bz_worker_proto::ExecuteEvent;
    use bz_worker_proto::ExecuteResponse;
    use bz_worker_proto::worker_client;
    use bz_worker_proto::worker_server::Worker;
    use bz_worker_proto::worker_server::WorkerServer;
    use tonic::Request;
    use tonic::Response;
    use tonic::Status;
    use tonic::transport::Channel;
    use tonic::transport::Server;

    use super::WorkerClient;

    struct MockWorker {
        attempts: Arc<AtomicU32>,
        fail_until: u32,
        fail_code: tonic::Code,
    }

    #[tonic::async_trait]
    impl Worker for MockWorker {
        async fn execute(
            &self,
            _req: Request<ExecuteCommand>,
        ) -> Result<Response<ExecuteResponse>, Status> {
            let n = self.attempts.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_until {
                Err(Status::new(self.fail_code, "test error"))
            } else {
                Ok(Response::new(ExecuteResponse {
                    exit_code: 0,
                    stderr: String::new(),
                    timed_out_after_s: None,
                }))
            }
        }

        async fn exec(
            &self,
            _req: Request<tonic::Streaming<ExecuteEvent>>,
        ) -> Result<Response<ExecuteResponse>, Status> {
            unimplemented!()
        }
    }

    async fn start_mock_server(
        worker: MockWorker,
    ) -> (
        worker_client::WorkerClient<Channel>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            Server::builder()
                .add_service(WorkerServer::new(worker))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let client = worker_client::WorkerClient::connect(format!("http://{}", addr))
            .await
            .unwrap();
        (client, handle)
    }

    fn empty_request() -> ExecuteCommand {
        ExecuteCommand {
            argv: vec![],
            env: vec![],
            timeout_s: None,
        }
    }

    #[tokio::test]
    async fn test_retry_succeeds_immediately() {
        let attempts = Arc::new(AtomicU32::new(0));
        let (mut client, _server) = start_mock_server(MockWorker {
            attempts: attempts.clone(),
            fail_until: 0,
            fail_code: tonic::Code::Unavailable,
        })
        .await;
        let result = WorkerClient::execute_with_retry(&mut client, empty_request()).await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_recovers_after_transient_unavailable() {
        let attempts = Arc::new(AtomicU32::new(0));
        let (mut client, _server) = start_mock_server(MockWorker {
            attempts: attempts.clone(),
            fail_until: 1,
            fail_code: tonic::Code::Unavailable,
        })
        .await;
        let result = WorkerClient::execute_with_retry(&mut client, empty_request()).await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_retry_does_not_retry_non_unavailable_errors() {
        let attempts = Arc::new(AtomicU32::new(0));
        let (mut client, _server) = start_mock_server(MockWorker {
            attempts: attempts.clone(),
            fail_until: 100,
            fail_code: tonic::Code::Internal,
        })
        .await;
        let result = WorkerClient::execute_with_retry(&mut client, empty_request()).await;
        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "non-Unavailable errors must not be retried"
        );
    }

    #[tokio::test]
    async fn test_retry_gives_up_after_max_retries() {
        let attempts = Arc::new(AtomicU32::new(0));
        let (mut client, _server) = start_mock_server(MockWorker {
            attempts: attempts.clone(),
            fail_until: 100,
            fail_code: tonic::Code::Unavailable,
        })
        .await;
        let result = WorkerClient::execute_with_retry(&mut client, empty_request()).await;
        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            6,
            "initial attempt + 5 retries"
        );
    }
}
