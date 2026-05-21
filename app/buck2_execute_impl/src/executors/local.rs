/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::ffi::OsStr;
use std::ffi::OsString;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_build_signals::env::WaitingCategory;
use buck2_common::file_ops::metadata::FileDigestConfig;
use buck2_common::liveliness_observer::LivelinessObserver;
use buck2_common::liveliness_observer::LivelinessObserverExt;
use buck2_common::liveliness_observer::NoopLivelinessObserver;
use buck2_common::local_resource_state::LocalResourceHolder;
use buck2_core::content_hash::ContentBasedPathHash;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::buck_out_path::BuildArtifactPath;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_core::soft_error;
use buck2_core::tag_error;
use buck2_core::tag_result;
use buck2_directory::directory::directory::Directory;
use buck2_directory::directory::directory_iterator::DirectoryIterator;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_error::BuckErrorContext;
use buck2_error::buck2_error;
use buck2_events::daemon_id::DaemonId;
use buck2_events::dispatch::EventDispatcher;
use buck2_events::dispatch::get_dispatcher_opt;
use buck2_execute::artifact_utils::ArtifactValueBuilder;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::directory::ActionDirectoryBuilder;
use buck2_execute::directory::ActionDirectoryEntry;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::extract_artifact_value;
use buck2_execute::directory::insert_entry;
use buck2_execute::entry::HashingInfo;
use buck2_execute::entry::build_entry_from_disk;
use buck2_execute::execute::action_digest::ActionDigest;
use buck2_execute::execute::blocking::BlockingExecutor;
use buck2_execute::execute::clean_output_paths::CleanOutputPaths;
use buck2_execute::execute::environment_inheritance::EnvironmentInheritance;
use buck2_execute::execute::executor_stage_async;
use buck2_execute::execute::inputs_directory::inputs_directory;
use buck2_execute::execute::kind::CommandExecutionKind;
use buck2_execute::execute::manager::CommandExecutionManager;
use buck2_execute::execute::manager::CommandExecutionManagerExt;
use buck2_execute::execute::manager::CommandExecutionManagerWithClaim;
use buck2_execute::execute::output::CommandStdStreams;
use buck2_execute::execute::prepared::PreparedCommand;
use buck2_execute::execute::prepared::PreparedCommandExecutor;
use buck2_execute::execute::prepared::PreparedCommandOptionalExecutor;
use buck2_execute::execute::prepared::UnpreparedCommand;
use buck2_execute::execute::request::CommandExecutionInput;
use buck2_execute::execute::request::CommandExecutionOutput;
use buck2_execute::execute::request::CommandExecutionOutputRef;
use buck2_execute::execute::request::CommandExecutionRequest;
use buck2_execute::execute::request::ExecutorPreference;
use buck2_execute::execute::request::NetworkAccess;
use buck2_execute::execute::request::WorkerProtocol;
use buck2_execute::execute::result::CommandExecutionMetadata;
use buck2_execute::execute::result::CommandExecutionResult;
use buck2_execute::knobs::ExecutorGlobalKnobs;
use buck2_execute::materialize::materializer::CopiedArtifact;
use buck2_execute::materialize::materializer::DeclareArtifactPayload;
use buck2_execute::materialize::materializer::MaterializationError;
use buck2_execute::materialize::materializer::Materializer;
use buck2_execute_local::CommandResult;
use buck2_execute_local::DefaultKillProcess;
use buck2_execute_local::GatherOutputStatus;
use buck2_execute_local::decode_command_event_stream;
use buck2_execute_local::maybe_absolutize_exe;
use buck2_execute_local::spawn_command_and_stream_events;
use buck2_execute_local::status_decoder::DefaultStatusDecoder;
use buck2_fs::IoResultExt;
use buck2_fs::async_fs_util;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::abs_path::AbsPath;
use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use buck2_hash::BuckIndexMap;
use buck2_hash::BuckIndexSet;
use buck2_resource_control::ActionFreezeEvent;
use buck2_resource_control::ActionFreezeEventReceiver;
use buck2_resource_control::CommandType;
use buck2_resource_control::action_scene::ActionCgroupSession;
use buck2_resource_control::memory_tracker::MemoryTrackerHandle;
use buck2_resource_control::path::CgroupPathBuf;
use buck2_util::process::background_command;
use buck2_util::time_span::TimeSpan;
use derive_more::From;
use dice_futures::cancellation::CancellationContext;
use dice_futures::cancellation::CancellationObserver;
use dupe::Dupe;
use futures::future;
use futures::future::Either;
use futures::future::FutureExt;
use futures::future::Shared;
use futures::future::join_all;
use futures::stream::StreamExt;
use gazebo::prelude::*;
use host_sharing::HostSharingBroker;
use host_sharing::HostSharingRequirements;
use host_sharing::host_sharing::HostSharingGuard;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::info;

use crate::executors::local_action_cache::LocalActionCache;
use crate::executors::local_action_cache::local_action_cache_outputs_fingerprint;
use crate::executors::worker::WorkerHandle;
use crate::executors::worker::WorkerPool;
use crate::incremental_actions_helper::get_incremental_path_map;
use crate::incremental_actions_helper::save_content_based_incremental_state;
use crate::sqlite::incremental_state_db::IncrementalDbState;

static ARTIFACT_PATH_ALIAS_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static BAZEL_WORKER_SANDBOX_COUNTER: AtomicU64 = AtomicU64::new(0);

fn bazel_local_tmpdir() -> OsString {
    std::env::var_os("TMPDIR")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("/tmp"))
}

#[derive(Debug, buck2_error::Error)]
#[buck2(input)]
enum LocalExecutionError {
    #[error("Args list was empty")]
    NoArgs,

    #[error("Trying to execute a remote-only action on a local executor")]
    RemoteOnlyAction,
}

enum LocalActionCacheMetadataLookup {
    Hit(BuckIndexMap<CommandExecutionOutput, ArtifactValue>),
    MissingMetadata,
    Stale,
}

#[derive(Clone, Dupe, Allocative)]
pub enum ForkserverAccess {
    None,
    #[cfg(unix)]
    Client(buck2_forkserver::client::ForkserverClient),
}

#[derive(Clone)]
pub struct LocalExecutor {
    artifact_fs: ArtifactFs,
    materializer: Arc<dyn Materializer>,
    incremental_db_state: Arc<IncrementalDbState>,
    local_action_cache: Arc<LocalActionCache>,
    blocking_executor: Arc<dyn BlockingExecutor>,
    pub(crate) host_sharing_broker: Arc<HostSharingBroker>,
    root: AbsNormPathBuf,
    forkserver: ForkserverAccess,
    #[allow(unused)]
    knobs: ExecutorGlobalKnobs,
    worker_pool: Option<Arc<WorkerPool>>,
    memory_tracker: Option<MemoryTrackerHandle>,
    daemon_id: DaemonId,
}

impl LocalExecutor {
    pub fn new(
        artifact_fs: ArtifactFs,
        materializer: Arc<dyn Materializer>,
        incremental_db_state: Arc<IncrementalDbState>,
        local_action_cache: Arc<LocalActionCache>,
        blocking_executor: Arc<dyn BlockingExecutor>,
        host_sharing_broker: Arc<HostSharingBroker>,
        root: AbsNormPathBuf,
        forkserver: ForkserverAccess,
        knobs: ExecutorGlobalKnobs,
        worker_pool: Option<Arc<WorkerPool>>,
        memory_tracker: Option<MemoryTrackerHandle>,
        daemon_id: DaemonId,
    ) -> Self {
        Self {
            artifact_fs,
            materializer,
            incremental_db_state,
            local_action_cache,
            blocking_executor,
            host_sharing_broker,
            root,
            forkserver,
            knobs,
            worker_pool,
            memory_tracker,
            daemon_id,
        }
    }

    // Compiler gets confused (on the not(unix) branch only, weirdly) if you use an async fn.
    #[allow(clippy::manual_async_fn)]
    fn exec<'a>(
        &'a self,
        exe: &'a str,
        args: impl IntoIterator<Item = impl AsRef<OsStr> + Send> + Send + 'a,
        env: impl IntoIterator<Item = (impl AsRef<OsStr> + Send, impl AsRef<OsStr> + Send)> + Send + 'a,
        working_directory: &'a ProjectRelativePath,
        timeout: Option<Duration>,
        env_inheritance: Option<&'a EnvironmentInheritance>,
        liveliness_observer: impl LivelinessObserver + 'static,
        disable_miniperf: bool,
        cgroup: Option<CgroupPathBuf>,
        freeze_rx: impl ActionFreezeEventReceiver,
        network_access: Option<NetworkAccess>,
    ) -> impl futures::future::Future<Output = buck2_error::Result<CommandResult>> + Send + 'a {
        async move {
            let working_directory = self.root.join_cow(working_directory);
            prepare_bazel_execroot_working_directory(&self.root, &working_directory)?;

            let result = match &self.forkserver {
                #[cfg(unix)]
                ForkserverAccess::Client(forkserver) => {
                    unix::exec_via_forkserver(
                        forkserver,
                        exe,
                        args,
                        env,
                        &working_directory,
                        timeout,
                        env_inheritance,
                        liveliness_observer,
                        self.knobs.enable_miniperf && !disable_miniperf,
                        cgroup,
                        freeze_rx,
                        network_access,
                    )
                    .await
                }
                ForkserverAccess::None => {
                    let _disable_miniperf = disable_miniperf;
                    let _network_access = network_access;
                    let exe = maybe_absolutize_exe(exe, &working_directory)?;
                    let mut cmd = background_command(exe.as_ref());
                    cmd.current_dir(working_directory.as_path());
                    cmd.args(args);
                    apply_local_execution_environment(
                        &mut cmd,
                        &working_directory,
                        env,
                        env_inheritance,
                    );

                    let alive = liveliness_observer
                        .while_alive()
                        .map(|()| Ok(GatherOutputStatus::Cancelled));

                    let stream = spawn_command_and_stream_events(
                        cmd,
                        timeout,
                        alive,
                        DefaultStatusDecoder,
                        DefaultKillProcess::default(),
                        None,
                        true,
                        cgroup,
                        freeze_rx,
                    )
                    .await?;
                    decode_command_event_stream(stream).await
                }
                .with_buck_error_context(|| format!("Failed to gather output from command: {exe}")),
            }?;

            if !result.orphan_processes.is_empty() {
                buck2_events::dispatch::instant_event(buck2_data::OrphanProcessesKilled {
                    orphan_processes: result
                        .orphan_processes
                        .iter()
                        .map(|o| buck2_data::orphan_processes_killed::OrphanProcess {
                            pid: o.pid,
                            comm: o.comm.clone(),
                        })
                        .collect(),
                });
            }

            buck2_error::Ok(result)
        }
    }

    async fn exec_once(
        &self,
        action_digest: &ActionDigest,
        request: &CommandExecutionRequest,
        manager: CommandExecutionManagerWithClaim,
        cancellations: &CancellationContext,
        liveliness_observer: impl LivelinessObserver + 'static,
        scratch_path: &ScratchPath,
        args: &[String],
        worker: Option<&WorkerHandle>,
        env: &[(&str, StrOrOsStr<'_>)],
        materialized_inputs: &MaterializedInputPaths,
        cgroup: Option<CgroupPathBuf>,
        freeze_rx: impl ActionFreezeEventReceiver,
    ) -> Result<
        (
            TimeSpan,
            SystemTime,
            CommandResult,
            CommandExecutionManagerWithClaim,
        ),
        CommandExecutionResult,
    > {
        let bazel_worker_sandbox = match executor_stage_async(
            buck2_data::LocalStage {
                stage: Some(buck2_data::LocalPrepareOutputDirs {}.into()),
            },
            async {
                let working_directory = self.root.join_cow(request.working_directory());
                prepare_bazel_execroot_working_directory(&self.root, &working_directory)?;

                tokio::try_join!(
                    create_output_dirs(
                        &self.artifact_fs,
                        request,
                        self.materializer.dupe(),
                        self.blocking_executor.dupe(),
                        cancellations,
                    ),
                    prep_scratch_path(scratch_path, &self.artifact_fs),
                )
                .buck_error_context("Error creating output directories")?;

                let bazel_worker_sandbox = if request.worker().as_ref().is_some_and(|worker| {
                    worker.protocol == WorkerProtocol::Bazel && worker.bazel_worker_sandboxing
                }) {
                    Some(
                        prepare_bazel_worker_sandbox(
                            &self.artifact_fs,
                            request,
                            materialized_inputs,
                            action_digest,
                            self.blocking_executor.as_ref(),
                        )
                        .await?,
                    )
                } else {
                    None
                };

                buck2_error::Ok(bazel_worker_sandbox)
            },
        )
        .boxed()
        .await
        {
            Ok(bazel_worker_sandbox) => bazel_worker_sandbox,
            Err(e) => return Err(manager.error("prepare_output_dirs_failed", e)),
        };

        let (time_span, start_time, res) = executor_stage_async(
            {
                let env = env
                    .iter()
                    .copied()
                    .map(|(k, v)| buck2_data::EnvironmentEntry {
                        key: k.to_owned(),
                        value: v.into_string_lossy(),
                    })
                    .collect();

                let stage = match worker {
                    None => buck2_data::LocalExecute {
                        command: Some(buck2_data::LocalCommand {
                            action_digest: action_digest.to_string(),
                            argv: args.to_vec(),
                            env,
                        }),
                    }
                    .into(),
                    Some(_) => buck2_data::WorkerExecute {
                        command: Some(buck2_data::WorkerCommand {
                            action_digest: action_digest.to_string(),
                            argv: request.args().to_vec(),
                            env,
                            fallback_exe: request.exe().to_vec(),
                        }),
                    }
                    .into(),
                };
                buck2_data::LocalStage { stage: Some(stage) }
            },
            async move {
                let execution_start = TimeSpan::start_now();
                let start_time = SystemTime::now();

                let env = env.iter().map(|(k, v)| (k, v.into_os_str()));
                let r = if let Some(worker) = worker {
                    let env: Vec<(OsString, OsString)> = env
                        .into_iter()
                        .map(|(k, v)| (OsString::from(k), v.to_owned()))
                        .collect();
                    match expand_bazel_worker_args(&self.artifact_fs, request, materialized_inputs)
                    {
                        Ok(worker_args) => Ok(worker
                            .exec_cmd(
                                &worker_args,
                                env,
                                request.timeout(),
                                bazel_worker_sandbox
                                    .as_ref()
                                    .map(|sandbox| sandbox.relative_path.clone()),
                            )
                            .await),
                        Err(e) => Err(e),
                    }
                } else {
                    self.exec(
                        &args[0],
                        &args[1..],
                        env,
                        request.working_directory(),
                        request.timeout(),
                        request.local_environment_inheritance(),
                        liveliness_observer,
                        request.disable_miniperf(),
                        cgroup,
                        freeze_rx,
                        request.network_access(),
                    )
                    .await
                };

                let time_span = execution_start.end_now();

                let r = match (r, &bazel_worker_sandbox) {
                    (Ok(res), Some(sandbox)) => match promote_bazel_worker_sandbox_outputs(
                        &self.artifact_fs,
                        request,
                        sandbox,
                        self.blocking_executor.as_ref(),
                    )
                    .await
                    {
                        Ok(()) => Ok(res),
                        Err(e) => Err(e),
                    },
                    (r, _) => r,
                };

                (time_span, start_time, r)
            },
        )
        .boxed()
        .await;

        match res {
            Ok(res) => Ok((time_span, start_time, res, manager)),
            Err(e) => Err(manager.error("exec_failed", e)),
        }
    }

    async fn exec_with_resource_control(
        &self,
        action_digest: &ActionDigest,
        request: &CommandExecutionRequest,
        mut manager: CommandExecutionManagerWithClaim,
        cancellations: &CancellationContext,
        liveliness_observer: impl LivelinessObserver + 'static,
        scratch_path: &ScratchPath,
        args: &[String],
        worker: Option<&WorkerHandle>,
        env: &[(&str, StrOrOsStr<'_>)],
        materialized_inputs: &MaterializedInputPaths,
    ) -> Result<
        (
            TimeSpan,
            SystemTime,
            CommandResult,
            CommandExecutionManagerWithClaim,
        ),
        CommandExecutionResult,
    > {
        let (cgroup_session, mut start_future) =
            if worker.is_some() || request.skip_resource_control() {
                (None, None)
            } else {
                let command_type = if request.is_test() {
                    CommandType::Test
                } else {
                    CommandType::Build
                };
                let disable_kill_and_retry_suspend = !request.outputs_cleanup;
                match ActionCgroupSession::maybe_create(
                    self.memory_tracker.dupe(),
                    command_type,
                    Some(action_digest.to_string()),
                    disable_kill_and_retry_suspend,
                )
                .await
                {
                    Ok(Some((session, start_future))) => (Some(session), Some(start_future)),
                    Ok(None) => (None, None),
                    Err(e) => return Err(manager.error("initializing_resource_control", e)),
                }
            };

        let liveliness_observer: Arc<dyn LivelinessObserver> = Arc::new(liveliness_observer);

        let mut res = loop {
            let (kill_future, freeze_rx) = if let Some(start_future) = start_future {
                start_future.0.await.ok().unzip()
            } else {
                (None, None)
            };
            let freeze_rx = match freeze_rx {
                Some(x) => Either::Left(UnboundedReceiverStream::new(x)),
                None => Either::Right(futures::stream::pending::<ActionFreezeEvent>()),
            };

            let retry_future = Arc::new(std::sync::Mutex::new(None));

            let kill_observer = if let Some(kill_future) = kill_future {
                let kill_awaiter = buck2_util::async_move_clone!(retry_future, {
                    if let Ok(r) = kill_future.0.await {
                        *retry_future.lock().unwrap() = Some(r);
                    } else {
                        // If the other end hung up for some reason, we definitely do not want to
                        // treat that as indicating a kill, so never return from this future
                        std::future::pending().await
                    }
                });

                struct FutureLivelinessObserver<F: Future<Output = ()> + Send + Sync>(Shared<F>);

                #[async_trait::async_trait]
                impl<F: Future<Output = ()> + Send + Sync> LivelinessObserver for FutureLivelinessObserver<F> {
                    async fn while_alive(&self) {
                        self.0.clone().await
                    }
                }

                Arc::new(FutureLivelinessObserver(kill_awaiter.shared()))
                    as Arc<dyn LivelinessObserver>
            } else {
                Arc::new(NoopLivelinessObserver) as Arc<dyn LivelinessObserver>
            };

            let liveliness_observer = liveliness_observer.dupe().and(kill_observer);
            let res = self
                .exec_once(
                    action_digest,
                    request,
                    manager,
                    cancellations,
                    liveliness_observer,
                    scratch_path,
                    args,
                    worker,
                    env,
                    materialized_inputs,
                    cgroup_session.as_ref().map(|s| s.path.clone()),
                    freeze_rx,
                )
                .await;

            let res = match res {
                Ok((time_span, start_time, status, res_manager)) => {
                    if matches!(status.status, GatherOutputStatus::Cancelled) {
                        let f = retry_future.lock().unwrap().take();
                        if let Some(retry_future) = f {
                            start_future = Some(retry_future);
                            manager = res_manager;
                            continue;
                        }
                    }
                    Ok((time_span, start_time, status, res_manager))
                }
                Err(e) => Err(e),
            };

            break res;
        };

        if let Some(cgroup_session) = cgroup_session {
            let cgroup_res = cgroup_session.action_finished().await;
            if let Ok(res) = &mut res {
                res.2.cgroup_result = Some(cgroup_res);
            }
        }

        res
    }

    async fn exec_request(
        &self,
        action_digest: &ActionDigest,
        request: &CommandExecutionRequest,
        mut manager: CommandExecutionManager,
        cancellation: CancellationObserver,
        cancellations: &CancellationContext,
        digest_config: DigestConfig,
        local_resource_holders: &[LocalResourceHolder],
    ) -> CommandExecutionResult {
        let args = &request.all_args_vec();
        if args.is_empty() {
            return manager.error("no_args", LocalExecutionError::NoArgs);
        }

        manager.start_waiting_category(WaitingCategory::MaterializingInputs);
        let executor_stage_result = executor_stage_async(
            buck2_data::LocalStage {
                stage: Some(buck2_data::LocalMaterializeInputs {}.into()),
            },
            async {
                let start = Instant::now();

                let working_directory = self.root.join_cow(request.working_directory());
                prepare_bazel_execroot_working_directory(&self.root, &working_directory)?;

                let (r1, r2) = future::join(
                    async {
                        materialize_inputs(
                            &self.artifact_fs,
                            self.materializer.as_ref(),
                            request,
                            digest_config,
                        )
                        .await
                    },
                    async {
                        if !request.outputs_cleanup {
                            // When user requests to not perform a cleanup for a specific action
                            // output from previous run of that action could actually be used as the
                            // input during current run (e.g. extra output which is an incremental state describing the actual output).
                            materialize_build_outputs(
                                &self.artifact_fs,
                                &self.incremental_db_state,
                                self.materializer.as_ref(),
                                request,
                            )
                            .await?;

                            // TODO(minglunli): There might be a dedup opportunity here to save some copying/materialization
                            // if the paths already exist on disk, should explore that
                            self.prepare_content_based_incremental_actions(request, cancellations)
                                .await?;

                            buck2_error::Ok(())
                        } else {
                            Ok(())
                        }
                    },
                )
                .await;

                let materialized_inputs = r1?;
                r2?;

                buck2_error::Ok((materialized_inputs, Instant::now() - start))
            },
        )
        .boxed()
        .await;

        let (materialized_inputs, input_materialization_duration) = match executor_stage_result {
            Ok((materialized_inputs, input_materialization_duration)) => {
                (materialized_inputs, input_materialization_duration)
            }
            Err(e) => return manager.error("materialize_inputs_failed", e),
        };
        let scratch_path = &materialized_inputs.scratch;

        manager.start_waiting_category(WaitingCategory::Unknown);

        // TODO: Release here.
        let manager = manager.claim().boxed().await;

        info!(
            "Local execution command line:\n```\n$ {}\n```",
            args.join(" "),
        );

        let dispatcher = match get_dispatcher_opt() {
            Some(dispatcher) => dispatcher,
            None => {
                return manager.error(
                    "no_dispatcher",
                    buck2_error!(
                        buck2_error::ErrorTag::DispatcherUnavailable,
                        "No dispatcher available"
                    ),
                );
            }
        };
        let build_id: &str = &dispatcher.trace_id().to_string();

        let mut env = vec![];
        let working_directory_abs = self.root.join_cow(request.working_directory());
        let is_bazel_execroot_action =
            is_bazel_execroot_working_directory(working_directory_abs.as_ref());

        let scratch_path_abs;
        let local_tmpdir;

        if let Some(scratch_path) = &scratch_path.0 {
            scratch_path_abs = self.artifact_fs.fs().resolve(scratch_path);

            if cfg!(windows) {
                const MAX_PATH: usize = 260;
                if scratch_path_abs.as_os_str().len() > MAX_PATH {
                    return manager.error(
                        "scratch_dir_too_long",
                        buck2_error!(
                            buck2_error::ErrorTag::Environment,
                            "Scratch directory path is longer than MAX_PATH: {}",
                            scratch_path_abs
                        ),
                    );
                }
                env.push(("TEMP", StrOrOsStr::OsStr(scratch_path_abs.as_os_str())));
                env.push(("TMP", StrOrOsStr::OsStr(scratch_path_abs.as_os_str())));
            } else {
                local_tmpdir = bazel_local_tmpdir();
                env.push(("TMPDIR", StrOrOsStr::OsStr(local_tmpdir.as_os_str())));
            }
        }
        env.extend(
            request
                .env()
                .iter()
                .map(|(k, v)| (k.as_str(), StrOrOsStr::from(v.as_str()))),
        );

        env.extend(local_resource_holders.iter().flat_map(|h| {
            h.as_ref().0.iter().map(|env_var| {
                (
                    env_var.key.as_str(),
                    StrOrOsStr::from(env_var.value.as_str()),
                )
            })
        }));
        let daemon_id;
        if !is_bazel_execroot_action {
            daemon_id = self.daemon_id.to_string();
            env.push(("BUCK2_DAEMON_UUID", StrOrOsStr::from(&*daemon_id)));
            env.push(("BUCK_BUILD_ID", StrOrOsStr::from(build_id)));
        }

        let liveliness_observer = manager.inner.liveliness_observer.dupe().and(cancellation);

        let (worker, manager) = self
            .initialize_worker(request, manager, dispatcher.dupe())
            .boxed()
            .await?;

        let execution_kind = match worker {
            None => CommandExecutionKind::Local {
                digest: action_digest.dupe(),
                command: args.to_vec(),
                env: request.env().clone(),
            },
            Some(_) => CommandExecutionKind::LocalWorker {
                digest: action_digest.dupe(),
                command: request.args().to_vec(),
                env: request.env().clone(),
                fallback_exe: request.exe().to_vec(),
            },
        };

        let (time_span, start_time, res, manager) = match self
            .exec_with_resource_control(
                action_digest,
                request,
                manager,
                cancellations,
                liveliness_observer,
                scratch_path,
                args,
                worker.as_deref(),
                &env,
                &materialized_inputs,
            )
            .await
        {
            Ok(x) => x,
            Err(e) => return e,
        };

        let CommandResult {
            status,
            stdout,
            stderr,
            cgroup_result,
            ..
        } = res;

        let std_streams = CommandStdStreams::Local { stdout, stderr };

        let mut timing = Box::new(CommandExecutionMetadata {
            time_span,
            execution_time: time_span.duration(),
            start_time,
            execution_stats: None, // We fill this in later if available.
            input_materialization_duration,
            hashing_duration: Duration::ZERO, // We fill hashing info in later if available.
            hashed_artifacts_count: 0,
            queue_duration: None,
            suspend_duration: None,
            suspend_count: None,
        });

        let result = match status {
            GatherOutputStatus::Finished {
                exit_code,
                execution_stats,
            } => {
                // N.B. calculate_and_declare_output_values ignores missing
                // output files in order to guarantee we run the accounting
                // checks below. If the output is missing because the action
                // failed, we'll run the `exit_code != 0` branch below, allowing
                // us to detect corrupted materializer state in check_inputs. If
                // the output is just missing because the action didn't produce
                // it, that's detected when BuckActionExecutor.execute validates
                // that all outputs were actually returned.
                let (outputs, hashing_time) = match self
                    .calculate_and_declare_output_values(request, digest_config, true, true)
                    .boxed()
                    .await
                {
                    Ok((output_values, hashing_time)) => (output_values, hashing_time),
                    Err(e) => {
                        return manager.error("calculate_output_values_failed", e);
                    }
                };

                let mut execution_stats =
                    execution_stats.map(|s| buck2_data::CommandExecutionStats {
                        cpu_instructions_user: s.cpu_instructions_user,
                        cpu_instructions_kernel: s.cpu_instructions_kernel,
                        userspace_events: s.userspace_events,
                        kernel_events: s.kernel_events,
                        memory_peak: None,
                    });

                if let Some(memory_peak) =
                    cgroup_result.as_ref().and_then(|r| r.memory_peak.as_ref())
                {
                    execution_stats.get_or_insert_default().memory_peak = Some(*memory_peak);
                }

                timing.execution_stats = execution_stats;
                if let Some(cgroup_result) = cgroup_result {
                    if let Some(e) = cgroup_result.error {
                        let _unused = soft_error!("action_cgroup_error", e);
                    }
                    timing.suspend_duration = cgroup_result.suspend_duration;
                    timing.suspend_count = Some(cgroup_result.suspend_count);
                }

                timing.hashing_duration = hashing_time.hashing_duration;
                timing.hashed_artifacts_count = hashing_time.hashed_artifacts_count;

                if exit_code == 0 {
                    let outputs_fingerprint =
                        match local_action_cache_outputs_fingerprint(&self.artifact_fs, &outputs) {
                            Ok(fingerprint) => fingerprint,
                            Err(e) => {
                                return manager.error("local_action_cache_fingerprint_failed", e);
                            }
                        };
                    if let Err(e) = self
                        .local_action_cache
                        .insert(action_digest, outputs_fingerprint.clone())
                    {
                        return manager.error("local_action_cache_insert_failed", e);
                    }
                    if let Err(e) = self.insert_local_action_cache_metadata(
                        request,
                        &outputs_fingerprint,
                        &outputs,
                    ) {
                        return manager.error("local_action_cache_insert_metadata_failed", e);
                    }
                    manager.success(execution_kind, outputs, std_streams, *timing)
                } else {
                    let manager = check_inputs(
                        manager,
                        &self.artifact_fs,
                        self.blocking_executor.as_ref(),
                        request,
                    )
                    .boxed()
                    .await?;

                    manager.failure(
                        execution_kind,
                        outputs,
                        std_streams,
                        Some(exit_code),
                        *timing,
                        None,
                    )
                }
            }
            GatherOutputStatus::SpawnFailed(reason) => {
                let manager = check_inputs(
                    manager,
                    &self.artifact_fs,
                    self.blocking_executor.as_ref(),
                    request,
                )
                .boxed()
                .await?;

                // We are lying about the std streams here because we don't have a good mechanism
                // to report that the command does not exist, and because that's exactly what RE
                // also does when this happens.
                if matches!(execution_kind, CommandExecutionKind::Local { .. }) {
                    manager.failure(
                        execution_kind,
                        Default::default(),
                        CommandStdStreams::Local {
                            stdout: Default::default(),
                            stderr: format!("Spawning executable `{}` failed: {}", args[0], reason)
                                .into_bytes(),
                        },
                        None,
                        *timing,
                        None,
                    )
                } else {
                    // Workers executing tests often employ a health check to avoid producing
                    // invalid test results. Differentiating a worker spawn failure from a normal
                    // spawn or execution failure allows the test runner to handle this case
                    // accordingly.
                    manager.worker_failure(
                        execution_kind,
                        // Could probably use a better error message.
                        format!("Spawning executable `{}` failed: {}", args[0], reason),
                        *timing,
                    )
                }
            }
            GatherOutputStatus::TimedOut(duration) => {
                let (outputs, hashing_time) = match self
                    .calculate_and_declare_output_values(request, digest_config, true, true)
                    .boxed()
                    .await
                {
                    Ok((output_values, hashing_time)) => (output_values, hashing_time),
                    Err(e) => {
                        return manager.error("calculate_output_values_failed", e);
                    }
                };

                timing.hashing_duration = hashing_time.hashing_duration;
                timing.hashed_artifacts_count = hashing_time.hashed_artifacts_count;

                manager.timeout(
                    execution_kind,
                    outputs,
                    duration,
                    std_streams,
                    *timing,
                    None,
                )
            }
            GatherOutputStatus::Cancelled => manager.cancel_claim(execution_kind, *timing),
        };

        if let Some(run_action_key) = request.run_action_key()
            && !request.outputs_cleanup
        {
            save_content_based_incremental_state(
                run_action_key.clone(),
                &self.incremental_db_state,
                &self.artifact_fs,
                &result,
            );
        }

        result
    }

    async fn calculate_and_declare_output_values(
        &self,
        request: &CommandExecutionRequest,
        digest_config: DigestConfig,
        declare_outputs: bool,
        promote_outputs: bool,
    ) -> buck2_error::Result<(
        BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
        HashingInfo,
    )> {
        // Read outputs from disk and add them to the builder
        let mut entries = Vec::new();
        let mut output_entries = Vec::new();
        let mut output_contains_symlink = false;
        let mut total_hashing_time = Duration::ZERO;
        let mut total_hashed_outputs = 0;
        for output in request.outputs() {
            let produced_path = output
                .resolve_for_execution(
                    &self.artifact_fs,
                    Some(&ContentBasedPathHash::for_output_artifact()),
                )?
                .into_path();
            let path = output
                .resolve(
                    &self.artifact_fs,
                    Some(&ContentBasedPathHash::for_output_artifact()),
                )?
                .into_path();
            if promote_outputs {
                promote_produced_output_path(&self.artifact_fs, &produced_path, &path)?;
            }
            let abspath = self.root.join(&path);
            let (entry, hashing_info) = build_entry_from_disk(
                abspath,
                FileDigestConfig::build(digest_config.cas_digest_config()),
                self.blocking_executor.as_ref(),
                self.artifact_fs.fs().root(),
            )
            .await
            .with_buck_error_context(|| format!("collecting output {path:?}"))?;
            total_hashing_time += hashing_info.hashing_duration;
            total_hashed_outputs += hashing_info.hashed_artifacts_count;
            if let Some(entry) = entry {
                output_contains_symlink |= output_entry_contains_symlink(&entry);
                output_entries.push((path.clone(), entry));
                entries.push((output.cloned(), path));
            }
        }

        // Bazel's ActionOutputMetadataStore constructs output metadata from the output tree
        // directly. Only fall back to the full input+output directory when an output symlink may
        // need Buck's dependency extraction logic to resolve targets through known inputs.
        let mut builder = if output_contains_symlink {
            inputs_directory(request.inputs(), digest_config, &self.artifact_fs)?
        } else {
            ActionDirectoryBuilder::empty()
        };
        for (path, entry) in output_entries {
            insert_entry(&mut builder, path, entry)?;
        }

        let mut to_declare = vec![];
        let mut mapped_outputs = BuckIndexMap::with_capacity(entries.len());
        let mut configuration_path_to_content_based_path_symlinks = vec![];
        let mut output_path_to_content_based_path_copies = vec![];

        for (output, output_path) in entries {
            let value = extract_artifact_value(&builder, &output_path, digest_config)?;
            if let Some(value) = value {
                match output {
                    CommandExecutionOutput::BuildArtifact { .. } => {
                        // For content-based paths, things are a bit complicated here, because (a) the action
                        // wrote outputs at "placeholder" paths, not the final content-based paths (because
                        // they are not know until the output is produced), and (b) other actions can declare
                        // outputs at the same content-based path. Note that only remote actions can do that
                        // concurrently (with this local action), as we prevent any local actions with any of
                        // the same placeholder output paths from running at the same time.
                        // We do the following:
                        // (1) We create a symlink from the configuration-based path to the content-based path
                        //     (for any users/tooling that only has access to the configuration-based path)
                        // (2) Declare an existing artifact at the "placeholder" output path that the action wrote to.
                        // (3) Then we declare a copy from the "placeholder" output path to the content-based path.
                        // (4) Finally, we ensure everything is materialized.
                        // (5) Note that we don't need to invalidate the "placeholder" output path, as that is
                        //     the responsibility of any action that subsequently uses it.
                        if output.as_ref().has_content_based_path() {
                            let hashed_path = output
                                .as_ref()
                                .resolve(&self.artifact_fs, Some(&value.content_based_path_hash()))?
                                .into_path();

                            let configuration_hash_path = output
                                .as_ref()
                                .resolve_configuration_hash_path(&self.artifact_fs)?
                                .into_path();
                            let mut builder =
                                ArtifactValueBuilder::new(self.artifact_fs.fs(), digest_config);
                            builder.add_symlinked(
                                &value,
                                hashed_path.clone(),
                                &configuration_hash_path,
                            )?;
                            let symlink_value = builder.build(&configuration_hash_path)?;
                            let cfg_path = if self.materializer.is_eager_materialization_enabled() {
                                Some(configuration_hash_path.clone())
                            } else {
                                None
                            };
                            to_declare.push(DeclareArtifactPayload {
                                path: output_path.clone(),
                                artifact: value.dupe(),
                                configuration_path: None,
                            });
                            configuration_path_to_content_based_path_symlinks
                                .push((configuration_hash_path, symlink_value));
                            output_path_to_content_based_path_copies.push((
                                hashed_path.clone(),
                                value.dupe(),
                                vec![CopiedArtifact {
                                    src: output_path.clone(),
                                    dest: hashed_path,
                                    dest_entry: value.entry().dupe().map_dir(|d| d.as_immutable()),
                                    executable_bit_override: None,
                                }],
                                cfg_path,
                            ));
                        } else {
                            to_declare.push(DeclareArtifactPayload {
                                path: output_path,
                                artifact: value.dupe(),
                                configuration_path: None,
                            });
                        }
                    }
                    CommandExecutionOutput::TestPath { .. } => {
                        // Don't declare those as we don't currently have any form of GC so this
                        // would take up space for nothing, and most importantly, we will never
                        // need them to be in materializer state for e.g. matching as nothing
                        // should depend on them.
                    }
                }

                mapped_outputs.insert(output, value);
            }
        }

        if declare_outputs {
            let configuration_paths = configuration_path_to_content_based_path_symlinks
                .iter()
                .map(|(p, _)| p.clone())
                .collect();
            self.materializer.declare_existing(to_declare).await?;
            buck2_util::future::try_join_all(
                output_path_to_content_based_path_copies.into_iter().map(
                    |(path, value, copied_artifacts, cfg_path)| {
                        self.materializer
                            .declare_copy(path, value, copied_artifacts, cfg_path)
                    },
                ),
            )
            .await?;
            buck2_util::future::try_join_all(
                configuration_path_to_content_based_path_symlinks
                    .into_iter()
                    .map(|(path, value)| self.materializer.declare_copy(path, value, vec![], None)),
            )
            .await?;

            self.materializer
                .ensure_materialized(configuration_paths)
                .await?;
        }

        Ok((
            mapped_outputs,
            HashingInfo {
                hashing_duration: total_hashing_time,
                hashed_artifacts_count: total_hashed_outputs,
            },
        ))
    }

    async fn local_action_cache_outputs_from_materializer(
        &self,
        outputs_to_check: Vec<CommandExecutionOutput>,
        expected_fingerprint: &[u8],
    ) -> buck2_error::Result<LocalActionCacheMetadataLookup> {
        let mut output_paths = Vec::new();
        let mut output_keys = Vec::new();
        for output in outputs_to_check {
            let path = output
                .as_ref()
                .resolve(
                    &self.artifact_fs,
                    Some(&ContentBasedPathHash::for_output_artifact()),
                )?
                .into_path();
            output_paths.push(path);
            output_keys.push(output);
        }

        let values = self
            .materializer
            .get_declared_artifact_values(output_paths)
            .await?;
        if values.iter().any(Option::is_none) {
            return Ok(LocalActionCacheMetadataLookup::MissingMetadata);
        }

        let mut outputs = BuckIndexMap::with_capacity(values.len());
        for (output, value) in std::iter::zip(output_keys, values) {
            let Some(value) = value else {
                return Ok(LocalActionCacheMetadataLookup::MissingMetadata);
            };
            outputs.insert(output, value);
        }

        let actual_fingerprint =
            local_action_cache_outputs_fingerprint(&self.artifact_fs, &outputs)?;
        if actual_fingerprint.as_slice() != expected_fingerprint {
            return Ok(LocalActionCacheMetadataLookup::Stale);
        }

        // For ordinary outputs the value above came from the same materializer path that
        // `declare_match` would check, so another materializer round trip is redundant. Bazel's
        // action cache similarly injects cached output metadata on hits instead of revalidating the
        // same metadata through a second path. Content-based outputs still need the extra match
        // because their configuration path and content-addressed path are distinct declarations.
        if !outputs
            .keys()
            .any(CommandExecutionOutput::has_content_based_path)
        {
            return Ok(LocalActionCacheMetadataLookup::Hit(outputs));
        }

        let output_matches = outputs
            .iter()
            .map(|(output, value)| {
                Ok((
                    output
                        .as_ref()
                        .resolve(&self.artifact_fs, Some(&value.content_based_path_hash()))?
                        .into_path(),
                    value.dupe(),
                ))
            })
            .collect::<buck2_error::Result<Vec<_>>>()?;
        if !self
            .materializer
            .declare_match(output_matches)
            .await?
            .is_match()
        {
            return Ok(LocalActionCacheMetadataLookup::Stale);
        }

        Ok(LocalActionCacheMetadataLookup::Hit(outputs))
    }

    fn insert_local_action_cache_metadata(
        &self,
        request: &CommandExecutionRequest,
        outputs_fingerprint: &[u8],
        outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
    ) -> buck2_error::Result<()> {
        if let Some(local_action_cache_key) = request.local_action_cache_key() {
            self.insert_local_action_cache_key_metadata(
                local_action_cache_key,
                outputs_fingerprint,
                outputs,
            )?;
        }
        Ok(())
    }

    fn insert_local_action_cache_key_metadata(
        &self,
        local_action_cache_key: &buck2_execute::execute::request::LocalActionCacheKey,
        outputs_fingerprint: &[u8],
        outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
    ) -> buck2_error::Result<()> {
        let output_values: Arc<[ArtifactValue]> =
            outputs.values().cloned().collect::<Vec<_>>().into();
        self.local_action_cache.insert_action_metadata(
            local_action_cache_key.key.clone(),
            local_action_cache_key.action_key_digest.clone(),
            local_action_cache_key.input_metadata_digest.clone(),
            local_action_cache_key.fingerprint.clone(),
            outputs_fingerprint.to_vec(),
            output_values,
        )?;
        Ok(())
    }

    fn local_action_cache_outputs_from_entry(
        outputs_to_check: &BuckIndexSet<CommandExecutionOutput>,
        entry: &crate::executors::local_action_cache::LocalActionCacheEntry,
    ) -> buck2_error::Result<Option<BuckIndexMap<CommandExecutionOutput, ArtifactValue>>> {
        if entry.output_values.len() != outputs_to_check.len() {
            return Ok(None);
        }

        let outputs = outputs_to_check
            .iter()
            .cloned()
            .zip(entry.output_values.iter().cloned())
            .collect::<BuckIndexMap<_, _>>();

        Ok(Some(outputs))
    }

    async fn acquire_worker_permit(
        &self,
        request: &CommandExecutionRequest,
    ) -> Option<HostSharingGuard> {
        if let (Some(worker_spec), Some(worker_pool)) = (request.worker(), self.worker_pool.dupe())
        {
            let working_directory;
            let worker_root: &AbsNormPath = if worker_spec.protocol == WorkerProtocol::Bazel {
                working_directory = self.root.join_cow(request.working_directory());
                working_directory.as_ref()
            } else {
                self.root.as_ref()
            };

            if let Some(broker) = &worker_pool.get_worker_broker(worker_spec, worker_root) {
                Some(
                    executor_stage_async(
                        buck2_data::LocalStage {
                            stage: Some(buck2_data::WorkerQueued {}.into()),
                        },
                        broker.acquire(&HostSharingRequirements::default()),
                    )
                    .await,
                )
            } else {
                None
            }
        } else {
            None
        }
    }

    #[cfg(not(unix))]
    async fn initialize_worker(
        &self,
        _request: &CommandExecutionRequest,
        manager: CommandExecutionManagerWithClaim,
        _dispatcher: EventDispatcher,
    ) -> ControlFlow<
        CommandExecutionResult,
        (Option<Arc<WorkerHandle>>, CommandExecutionManagerWithClaim),
    > {
        ControlFlow::Continue((None, manager))
    }

    #[cfg(unix)]
    async fn initialize_worker(
        &self,
        request: &CommandExecutionRequest,
        manager: CommandExecutionManagerWithClaim,
        dispatcher: EventDispatcher,
    ) -> ControlFlow<
        CommandExecutionResult,
        (Option<Arc<WorkerHandle>>, CommandExecutionManagerWithClaim),
    > {
        if let (Some(worker_spec), Some(worker_pool), ForkserverAccess::Client(_)) =
            (request.worker(), self.worker_pool.dupe(), &self.forkserver)
        {
            let working_directory;
            let worker_root: &AbsNormPath = if worker_spec.protocol == WorkerProtocol::Bazel {
                working_directory = self.root.join_cow(request.working_directory());
                working_directory.as_ref()
            } else {
                self.root.as_ref()
            };
            let env = worker_spec
                .env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)));
            let (new_worker, worker_fut) = worker_pool.get_or_create_worker(
                worker_spec,
                env,
                worker_root,
                self.forkserver.dupe(),
                dispatcher,
            );

            if let Some(Ok(worker)) = worker_fut.peek() {
                return ControlFlow::Continue((Some(worker.clone()), manager));
            }

            // Might make more sense for the stage to always be `WorkerWait` and for `WorkerInit` to be a separate, top level event
            let stage = if new_worker {
                buck2_data::LocalStage {
                    stage: Some(
                        buck2_data::WorkerInit {
                            command: Some(buck2_data::WorkerInitCommand {
                                argv: worker_spec.exe.clone(),
                                env: worker_spec
                                    .env
                                    .iter()
                                    .map(|(k, v)| buck2_data::EnvironmentEntry {
                                        key: k.to_owned(),
                                        value: v.to_owned(),
                                    })
                                    .collect(),
                            }),
                        }
                        .into(),
                    ),
                }
            } else {
                buck2_data::LocalStage {
                    stage: Some(buck2_data::WorkerWait {}.into()),
                }
            };

            match executor_stage_async(stage, worker_fut).await {
                Ok(worker) => ControlFlow::Continue((Some(worker), manager)),
                Err(e) => {
                    let res = {
                        let manager = check_inputs(
                            manager,
                            &self.artifact_fs,
                            self.blocking_executor.as_ref(),
                            request,
                        )
                        .await?;

                        e.to_command_execution_result(request, manager)
                    };
                    ControlFlow::Break(res)
                }
            }
        } else {
            ControlFlow::Continue((None, manager))
        }
    }

    async fn prepare_content_based_incremental_actions(
        &self,
        request: &CommandExecutionRequest,
        cancellations: &CancellationContext,
    ) -> buck2_error::Result<()> {
        let declared_content_based_outputs: Vec<BuildArtifactPath> = request
            .outputs()
            .filter_map(|output| match output {
                CommandExecutionOutputRef::BuildArtifact { path, .. }
                    if path.is_content_based_path() =>
                {
                    Some(path.clone())
                }
                _ => None,
            })
            .collect();

        let outputs_to_delete = declared_content_based_outputs
            .iter()
            .map(|path| {
                self.artifact_fs
                    .resolve_build(path, Some(&ContentBasedPathHash::OutputArtifact))
            })
            .collect::<buck2_error::Result<Vec<_>>>()?;

        self.materializer
            .invalidate_many(outputs_to_delete.clone())
            .await?;

        // Need to clean the placeholder paths before execution as there could be stale outputs that can cause unexpected behavior
        self.blocking_executor
            .execute_io(
                Box::new(CleanOutputPaths {
                    paths: outputs_to_delete,
                }),
                cancellations,
            )
            .await
            .buck_error_context("Failed to cleanup output directory")?;

        if let Some(state) =
            get_incremental_path_map(&self.incremental_db_state, request.run_action_key())
        {
            let mut copy_futs = Vec::new();

            for output in declared_content_based_outputs {
                let p = output.path().to_buf();

                if let Some(content_path) = state.get(&p) {
                    copy_futs.push(async move {
                        self.blocking_executor
                            .execute_io_inline(|| {
                                self.artifact_fs.fs().copy(
                                    content_path.clone(),
                                    self.artifact_fs.resolve_build(
                                        &output,
                                        Some(&ContentBasedPathHash::OutputArtifact),
                                    )?,
                                )
                            })
                            .await
                    })
                }
            }

            // The materialization we do for incremental action outputs is best-effort. The copy
            // will fail if the materialization failed, and that's okay.
            join_all(copy_futs).await;
        }

        Ok(())
    }
}

#[async_trait]
impl PreparedCommandExecutor for LocalExecutor {
    async fn exec_cmd(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> CommandExecutionResult {
        let mut manager = manager.with_execution_kind(CommandExecutionKind::Local {
            digest: command.prepared_action.digest(),
            command: command.request.all_args_vec(),
            env: command.request.env().clone(),
        });
        if command.request.executor_preference().requires_remote() {
            return manager.error("local_prepare", LocalExecutionError::RemoteOnlyAction);
        }

        let PreparedCommand {
            request,
            target: _,
            prepared_action,
            digest_config,
        } = command;

        manager.start_waiting_category(WaitingCategory::LocalQueued);
        let local_resource_holders = executor_stage_async(
            buck2_data::LocalStage {
                stage: Some(buck2_data::AcquireLocalResource {}.into()),
            },
            async move {
                let mut holders = vec![];
                // Acquire resources in a sorted way to avoid deadlock.
                // It might happen if 2 tests both requiring resources A and B are run simultaneously and there is only 1 instance of resource per type.
                // If tests are not acquiring them in a sorted way the following situation might happen:
                // Test 1 acquires resource B and test 2 acquires resource A.
                // Now test 1 is waiting on resource B and test 2 is waiting on resource A.
                for r in request.required_local_resources() {
                    holders.push(r.acquire_resource().await);
                }
                holders
            },
        )
        .await;

        let _permit = executor_stage_async(
            buck2_data::LocalStage {
                stage: Some(buck2_data::LocalQueued {}.into()),
            },
            self.host_sharing_broker
                .acquire(request.host_sharing_requirements()),
        )
        .await;

        let _worker_permit = self.acquire_worker_permit(request).await;
        manager.start_waiting_category(WaitingCategory::Unknown);

        // If we start running something, we don't want this task to get dropped, because if we do
        // we might interfere with e.g. clean up.
        cancellations
            .with_structured_cancellation(|cancellation| {
                Self::exec_request(
                    self,
                    &prepared_action.action_and_blobs.action,
                    request,
                    manager,
                    cancellation,
                    cancellations,
                    *digest_config,
                    &local_resource_holders,
                )
            })
            .await
    }

    fn is_local_execution_possible(&self, _executor_preference: ExecutorPreference) -> bool {
        true
    }

    fn is_full_hybrid_enabled(&self) -> bool {
        false
    }
}

#[async_trait]
impl PreparedCommandOptionalExecutor for LocalExecutor {
    fn insert_unprepared_action_cache_metadata(
        &self,
        local_action_cache_key: &buck2_execute::execute::request::LocalActionCacheKey,
        outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
    ) -> buck2_error::Result<()> {
        let outputs_fingerprint =
            local_action_cache_outputs_fingerprint(&self.artifact_fs, outputs)?;
        self.insert_local_action_cache_key_metadata(
            local_action_cache_key,
            &outputs_fingerprint,
            outputs,
        )
    }

    async fn maybe_execute_unprepared(
        &self,
        command: &UnpreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        _cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        let Some(entry) = self
            .local_action_cache
            .get_action_metadata(&command.local_action_cache_key.key)
        else {
            return ControlFlow::Continue(manager);
        };
        if entry.action_key_digest.as_ref()
            != command.local_action_cache_key.action_key_digest.as_slice()
            || entry.input_metadata_digest.as_ref()
                != command
                    .local_action_cache_key
                    .input_metadata_digest
                    .as_slice()
            || entry.action_fingerprint.as_ref()
                != command.local_action_cache_key.fingerprint.as_slice()
        {
            if let Err(e) = self
                .local_action_cache
                .remove_action_metadata(&command.local_action_cache_key.key)
            {
                return ControlFlow::Break(
                    manager.error("local_action_cache_remove_metadata_failed", e),
                );
            }
            return ControlFlow::Continue(manager);
        }

        let start = TimeSpan::start_now();
        let start_time = SystemTime::now();
        match Self::local_action_cache_outputs_from_entry(command.outputs, &entry) {
            Ok(Some(outputs)) => {
                let time_span = start.end_now();
                let timing = CommandExecutionMetadata {
                    time_span,
                    execution_time: Duration::ZERO,
                    start_time,
                    execution_stats: None,
                    input_materialization_duration: Duration::ZERO,
                    hashing_duration: Duration::ZERO,
                    hashed_artifacts_count: 0,
                    queue_duration: None,
                    suspend_duration: None,
                    suspend_count: None,
                };
                let digest = ActionDigest::from_content(
                    &command.local_action_cache_key.fingerprint,
                    command.digest_config.cas_digest_config(),
                );

                ControlFlow::Break(manager.success_without_claim(
                    CommandExecutionKind::LocalActionCache { digest },
                    outputs,
                    CommandStdStreams::Empty,
                    timing,
                ))
            }
            Ok(None) => {
                if let Err(e) = self
                    .local_action_cache
                    .remove_action_metadata(&command.local_action_cache_key.key)
                {
                    return ControlFlow::Break(
                        manager.error("local_action_cache_remove_metadata_failed", e),
                    );
                }
                ControlFlow::Continue(manager)
            }
            Err(e) => {
                ControlFlow::Break(manager.error("local_action_cache_metadata_decode_failed", e))
            }
        }
    }

    async fn maybe_execute(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        _cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        let action_digest = command.prepared_action.digest();
        let expected_fingerprint = match self.local_action_cache.get(&action_digest) {
            Some(fingerprint) => fingerprint,
            None => return ControlFlow::Continue(manager),
        };

        let start = TimeSpan::start_now();
        let start_time = SystemTime::now();
        match self
            .local_action_cache_outputs_from_materializer(
                command
                    .request
                    .outputs()
                    .map(|output| output.cloned())
                    .collect(),
                expected_fingerprint.as_ref(),
            )
            .await
        {
            Ok(LocalActionCacheMetadataLookup::Hit(outputs)) => {
                if let Err(e) = self.insert_local_action_cache_metadata(
                    command.request,
                    expected_fingerprint.as_ref(),
                    &outputs,
                ) {
                    return ControlFlow::Break(
                        manager.error("local_action_cache_insert_metadata_failed", e),
                    );
                }
                let time_span = start.end_now();
                let timing = CommandExecutionMetadata {
                    time_span,
                    execution_time: Duration::ZERO,
                    start_time,
                    execution_stats: None,
                    input_materialization_duration: Duration::ZERO,
                    hashing_duration: Duration::ZERO,
                    hashed_artifacts_count: 0,
                    queue_duration: None,
                    suspend_duration: None,
                    suspend_count: None,
                };

                return ControlFlow::Break(manager.success_without_claim(
                    CommandExecutionKind::LocalActionCache {
                        digest: action_digest,
                    },
                    outputs,
                    CommandStdStreams::Empty,
                    timing,
                ));
            }
            Ok(LocalActionCacheMetadataLookup::MissingMetadata) => {}
            Ok(LocalActionCacheMetadataLookup::Stale) => {
                tracing::debug!(
                    "local action cache miss for `{}` because persisted output metadata did not match",
                    action_digest
                );
                if let Err(e) = self.local_action_cache.remove(&action_digest) {
                    return ControlFlow::Break(
                        manager.error("local_action_cache_remove_failed", e),
                    );
                }
                return ControlFlow::Continue(manager);
            }
            Err(e) => {
                return ControlFlow::Break(
                    manager.error("local_action_cache_metadata_lookup_failed", e),
                );
            }
        }

        // The persisted cache entry proves these outputs should exist, and the
        // fingerprint check below proves the bytes still match. Declare the
        // verified outputs so a cold daemon can reuse materializer metadata on
        // the next invocation, but do not promote produced paths: this is a
        // cache hit, so nothing just wrote to the execution-only output path.
        let (outputs, hashing_info) = match self
            .calculate_and_declare_output_values(
                command.request,
                command.digest_config,
                true,
                false,
            )
            .boxed()
            .await
        {
            Ok(outputs) => outputs,
            Err(e) => {
                tracing::debug!(
                    "local action cache miss for `{}` while reading outputs: {}",
                    action_digest,
                    e
                );
                if let Err(e) = self.local_action_cache.remove(&action_digest) {
                    return ControlFlow::Break(
                        manager.error("local_action_cache_remove_failed", e),
                    );
                }
                return ControlFlow::Continue(manager);
            }
        };

        let actual_fingerprint =
            match local_action_cache_outputs_fingerprint(&self.artifact_fs, &outputs) {
                Ok(fingerprint) => fingerprint,
                Err(e) => {
                    return ControlFlow::Break(
                        manager.error("local_action_cache_fingerprint_failed", e),
                    );
                }
            };

        if actual_fingerprint.as_slice() != expected_fingerprint.as_ref() {
            tracing::debug!(
                "local action cache miss for `{}` because outputs changed",
                action_digest
            );
            if let Err(e) = self.local_action_cache.remove(&action_digest) {
                return ControlFlow::Break(manager.error("local_action_cache_remove_failed", e));
            }
            return ControlFlow::Continue(manager);
        }

        if !outputs
            .keys()
            .any(CommandExecutionOutput::has_content_based_path)
        {
            if let Err(e) = self.insert_local_action_cache_metadata(
                command.request,
                expected_fingerprint.as_ref(),
                &outputs,
            ) {
                return ControlFlow::Break(
                    manager.error("local_action_cache_insert_metadata_failed", e),
                );
            }
            let time_span = start.end_now();
            let timing = CommandExecutionMetadata {
                time_span,
                execution_time: Duration::ZERO,
                start_time,
                execution_stats: None,
                input_materialization_duration: Duration::ZERO,
                hashing_duration: hashing_info.hashing_duration,
                hashed_artifacts_count: hashing_info.hashed_artifacts_count,
                queue_duration: None,
                suspend_duration: None,
                suspend_count: None,
            };

            return ControlFlow::Break(manager.success_without_claim(
                CommandExecutionKind::LocalActionCache {
                    digest: action_digest,
                },
                outputs,
                CommandStdStreams::Empty,
                timing,
            ));
        }

        let output_matches = match outputs
            .iter()
            .map(|(output, value)| {
                Ok((
                    output
                        .as_ref()
                        .resolve(&self.artifact_fs, Some(&value.content_based_path_hash()))?
                        .into_path(),
                    value.dupe(),
                ))
            })
            .collect::<buck2_error::Result<Vec<_>>>()
        {
            Ok(output_matches) => output_matches,
            Err(e) => {
                return ControlFlow::Break(
                    manager.error("local_action_cache_match_paths_failed", e),
                );
            }
        };
        let materializer_accepts = match self.materializer.declare_match(output_matches).await {
            Ok(outcome) => outcome.is_match(),
            Err(e) => {
                return ControlFlow::Break(manager.error("local_action_cache_match_failed", e));
            }
        };
        if !materializer_accepts {
            tracing::debug!(
                "local action cache miss for `{}` because materializer state did not match",
                action_digest
            );
            if let Err(e) = self.local_action_cache.remove(&action_digest) {
                return ControlFlow::Break(manager.error("local_action_cache_remove_failed", e));
            }
            return ControlFlow::Continue(manager);
        }

        let time_span = start.end_now();
        if let Err(e) = self.insert_local_action_cache_metadata(
            command.request,
            expected_fingerprint.as_ref(),
            &outputs,
        ) {
            return ControlFlow::Break(
                manager.error("local_action_cache_insert_metadata_failed", e),
            );
        }
        let timing = CommandExecutionMetadata {
            time_span,
            execution_time: Duration::ZERO,
            start_time,
            execution_stats: None,
            input_materialization_duration: Duration::ZERO,
            hashing_duration: hashing_info.hashing_duration,
            hashed_artifacts_count: hashing_info.hashed_artifacts_count,
            queue_duration: None,
            suspend_duration: None,
            suspend_count: None,
        };

        ControlFlow::Break(manager.success_without_claim(
            CommandExecutionKind::LocalActionCache {
                digest: action_digest,
            },
            outputs,
            CommandStdStreams::Empty,
            timing,
        ))
    }
}

/// Either a str or a OsStr, so that we can turn it back into a String without having to check for
/// valid utf-8, while using the same struct.
#[derive(Copy, Clone, Dupe, From)]
enum StrOrOsStr<'a> {
    Str(&'a str),
    OsStr(&'a OsStr),
}

impl<'a> StrOrOsStr<'a> {
    fn into_string_lossy(self) -> String {
        match self {
            Self::Str(s) => s.to_owned(),
            Self::OsStr(s) => s.to_string_lossy().into_owned(),
        }
    }

    fn into_os_str(self) -> &'a OsStr {
        match self {
            Self::Str(s) => OsStr::new(s),
            Self::OsStr(s) => s,
        }
    }
}

fn prepare_bazel_execroot_working_directory(
    project_root: &AbsNormPathBuf,
    working_directory: &AbsNormPath,
) -> buck2_error::Result<()> {
    if !is_bazel_execroot_working_directory(working_directory) {
        return Ok(());
    }

    fs_util::create_dir_all(working_directory)
        .buck_error_context("Error creating Bazel execroot working directory")?;

    for entry in fs_util::read_dir(project_root).categorize_internal()? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        // Match Bazel's single-package-path symlink forest: source-tree entries live at
        // execroot top level, while convenience links and external repos are not planted.
        // Buck keeps a private output tree under the execroot, so do not mirror project buck-out.
        if file_name.starts_with("bazel-") || file_name == "buck-out" || file_name == "external" {
            continue;
        }
        let source = entry.path();
        let dest =
            AbsNormPathBuf::unchecked_new(working_directory.as_path().join(file_name.as_ref()));
        if let Err(e) = create_or_replace_symlink(source.as_path(), &dest) {
            if fs_util::read_link(&dest)
                .map(|target| target == source.as_path())
                .unwrap_or(false)
            {
                continue;
            }
            return Err(e).with_buck_error_context(|| {
                format!(
                    "Error creating Bazel execroot source symlink `{}` -> `{}`",
                    dest.display(),
                    source.display()
                )
            });
        }
    }

    // Bazel local actions share a durable execroot. Keep a Buck-local buck-out under it so
    // execroot-relative generated paths do not race the project buck-out tree.
    let buck_out = ForwardRelativePathBuf::unchecked_new("buck-out".to_owned());
    fs_util::create_dir_all(working_directory.join(&buck_out))
        .buck_error_context("Error creating Bazel execroot buck-out directory")
}

fn is_bazel_execroot_working_directory(working_directory: &AbsNormPath) -> bool {
    let path = working_directory.as_path();
    path.ends_with("__bazel_execroot")
        || path
            .parent()
            .is_some_and(|parent| parent.ends_with("__bazel_execroot"))
}

fn output_entry_contains_symlink(entry: &ActionDirectoryEntry<ActionDirectoryBuilder>) -> bool {
    match entry {
        DirectoryEntry::Leaf(
            ActionDirectoryMember::Symlink(_) | ActionDirectoryMember::ExternalSymlink(_),
        ) => true,
        DirectoryEntry::Leaf(_) => false,
        DirectoryEntry::Dir(dir) => {
            let mut leaves = dir.unordered_walk_leaves();
            while let Some((_path, member)) = leaves.next() {
                if matches!(
                    member,
                    ActionDirectoryMember::Symlink(_) | ActionDirectoryMember::ExternalSymlink(_)
                ) {
                    return true;
                }
            }
            false
        }
    }
}

struct BazelWorkerSandbox {
    relative_path: String,
    project_path: ProjectRelativePathBuf,
}

async fn prepare_bazel_worker_sandbox(
    artifact_fs: &ArtifactFs,
    request: &CommandExecutionRequest,
    materialized_inputs: &MaterializedInputPaths,
    _action_digest: &ActionDigest,
    blocking_executor: &dyn BlockingExecutor,
) -> buck2_error::Result<BazelWorkerSandbox> {
    let sandbox_id = BAZEL_WORKER_SANDBOX_COUNTER.fetch_add(1, Ordering::Relaxed);
    let relative_path = format!(
        "__buck2_worker_sandbox/{}-{}",
        std::process::id(),
        sandbox_id
    );
    let relative_path_buf = ForwardRelativePathBuf::unchecked_new(relative_path.clone());
    let sandbox_project_path = request.working_directory().join(&relative_path_buf);

    blocking_executor
        .execute_io_inline(|| {
            let fs = artifact_fs.fs();
            CleanOutputPaths::clean(std::iter::once(sandbox_project_path.as_ref()), fs)?;

            let working_directory_abs = fs.resolve(request.working_directory());
            let sandbox_abs = fs.resolve(&sandbox_project_path);
            fs_util::create_dir_all(&sandbox_abs)
                .buck_error_context("Error creating Bazel worker sandbox directory")?;

            // Mirror the execroot shape with symlinks, but keep buck-out private so worker
            // outputs are written under the per-request sandbox_dir.
            for entry in fs_util::read_dir(&working_directory_abs).categorize_internal()? {
                let entry = entry?;
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if file_name == "buck-out" || file_name == "__buck2_worker_sandbox" {
                    continue;
                }
                let source = entry.path();
                let dest = AbsNormPathBuf::unchecked_new(sandbox_abs.as_path().join(file_name.as_ref()));
                create_or_replace_symlink(source.as_path(), &dest)?;
            }

            for input_path in &materialized_inputs.paths {
                let Some(relative) = input_path.strip_prefix_opt(request.working_directory())
                else {
                    continue;
                };
                if !relative.as_str().starts_with("buck-out/") {
                    continue;
                }
                let sandbox_input_path = sandbox_project_path.join(relative);
                let source = fs.resolve(input_path);
                let dest = fs.resolve(&sandbox_input_path);
                create_or_replace_symlink(source.as_path(), &dest)?;
            }

            for (source_path, path) in &materialized_inputs.artifact_path_aliases {
                let Some(relative) = path.strip_prefix_opt(request.working_directory()) else {
                    continue;
                };
                let sandbox_input_path = sandbox_project_path.join(relative);
                let source = fs.resolve(source_path);
                let dest = fs.resolve(&sandbox_input_path);
                create_or_replace_symlink(source.as_path(), &dest)?;
            }

            for output in request.outputs() {
                let produced = output.resolve_for_execution(
                    artifact_fs,
                    Some(&ContentBasedPathHash::for_output_artifact()),
                )?;
                let output_path = produced.path();
                let relative = output_path
                    .strip_prefix_opt(request.working_directory())
                    .ok_or_else(|| {
                        buck2_error::internal_error!(
                            "Bazel worker output path `{}` is outside working directory `{}`",
                            output_path,
                            request.working_directory()
                        )
                    })?;
                let sandbox_output_path = sandbox_project_path.join(relative);
                CleanOutputPaths::clean(std::iter::once(sandbox_output_path.as_ref()), fs)?;

                if let Some(path_to_create) = produced.path_to_create() {
                    let relative = path_to_create
                        .strip_prefix_opt(request.working_directory())
                        .ok_or_else(|| {
                            buck2_error::internal_error!(
                                "Bazel worker output directory `{}` is outside working directory `{}`",
                                path_to_create,
                                request.working_directory()
                            )
                        })?;
                    fs_util::create_dir_all(fs.resolve(&sandbox_project_path.join(relative)))?;
                }
            }

            buck2_error::Ok(())
        })
        .await?;

    Ok(BazelWorkerSandbox {
        relative_path,
        project_path: sandbox_project_path,
    })
}

async fn promote_bazel_worker_sandbox_outputs(
    artifact_fs: &ArtifactFs,
    request: &CommandExecutionRequest,
    sandbox: &BazelWorkerSandbox,
    blocking_executor: &dyn BlockingExecutor,
) -> buck2_error::Result<()> {
    blocking_executor
        .execute_io_inline(|| {
            for output in request.outputs() {
                let produced = output.resolve_for_execution(
                    artifact_fs,
                    Some(&ContentBasedPathHash::for_output_artifact()),
                )?;
                let output_path = produced.path();
                let relative = output_path
                    .strip_prefix_opt(request.working_directory())
                    .ok_or_else(|| {
                        buck2_error::internal_error!(
                            "Bazel worker output path `{}` is outside working directory `{}`",
                            output_path,
                            request.working_directory()
                        )
                    })?;
                let sandbox_output_path = sandbox.project_path.join(relative);
                promote_produced_output_path(artifact_fs, &sandbox_output_path, output_path)?;
            }
            CleanOutputPaths::clean(
                std::iter::once(sandbox.project_path.as_ref()),
                artifact_fs.fs(),
            )
        })
        .await
}

fn create_or_replace_symlink(source: &Path, dest: &AbsNormPath) -> buck2_error::Result<()> {
    if fs_util::read_link(dest)
        .map(|target| target == source)
        .unwrap_or(false)
    {
        return Ok(());
    }

    fs_util::remove_all(dest).categorize_internal()?;
    if let Some(parent) = dest.parent() {
        fs_util::create_dir_all(parent)?;
    }
    match fs_util::symlink(source, dest).categorize_internal() {
        Ok(()) => Ok(()),
        Err(e) => {
            if fs_util::read_link(dest)
                .map(|target| target == source)
                .unwrap_or(false)
            {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

pub struct MaterializedInputPaths {
    pub scratch: ScratchPath,
    pub paths: Vec<ProjectRelativePathBuf>,
    pub artifact_path_aliases: Vec<(ProjectRelativePathBuf, ProjectRelativePathBuf)>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExternalCellRoot {
    source_root: ProjectRelativePathBuf,
    kind: String,
    repo: String,
}

fn external_cell_root(path: &ProjectRelativePath) -> Option<ExternalCellRoot> {
    let path = path.as_str();
    let rest = path.strip_prefix("buck-out/v2/external_cells/")?;
    let (kind, rest) = rest.split_once('/')?;
    if kind.is_empty() {
        return None;
    }
    let repo = rest.split_once('/').map_or(rest, |(repo, _)| repo);
    if repo.is_empty() {
        return None;
    }

    Some(ExternalCellRoot {
        source_root: ProjectRelativePathBuf::unchecked_new(format!(
            "buck-out/v2/external_cells/{kind}/{repo}"
        )),
        kind: kind.to_owned(),
        repo: repo.to_owned(),
    })
}

fn bazel_execroot_prefix(path: &ProjectRelativePath) -> Option<String> {
    let path = path.as_str();
    let (prefix, rest) = path.split_once("__bazel_execroot/")?;
    let (first_component, _) = rest.split_once('/')?;
    if first_component.len() == 16 && first_component.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Some(format!("{prefix}__bazel_execroot/{first_component}"));
    }
    Some(format!("{prefix}__bazel_execroot"))
}

fn path_is_at_or_under(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn external_cell_root_alias(
    source_path: &ProjectRelativePath,
    alias_path: &ProjectRelativePath,
) -> Option<(ProjectRelativePathBuf, ProjectRelativePathBuf)> {
    let external_root = external_cell_root(source_path)?;
    let execroot = bazel_execroot_prefix(alias_path)?;
    let alias = alias_path.as_str();

    let bazel_external_root = format!("{execroot}/external/{}", external_root.repo);
    if path_is_at_or_under(alias, &bazel_external_root) {
        return Some((
            external_root.source_root,
            ProjectRelativePathBuf::unchecked_new(bazel_external_root),
        ));
    }

    let source_alias_root = format!(
        "{execroot}/buck-out/v2/external_cells/{}/{}",
        external_root.kind, external_root.repo
    );
    if path_is_at_or_under(alias, &source_alias_root) {
        return Some((
            external_root.source_root,
            ProjectRelativePathBuf::unchecked_new(source_alias_root),
        ));
    }

    None
}

fn expand_bazel_worker_args(
    artifact_fs: &ArtifactFs,
    request: &CommandExecutionRequest,
    materialized_inputs: &MaterializedInputPaths,
) -> buck2_error::Result<Vec<String>> {
    if !request
        .worker()
        .as_ref()
        .is_some_and(|worker| worker.protocol == WorkerProtocol::Bazel)
    {
        return Ok(request.args().to_vec());
    }

    let mut expanded = Vec::new();
    for arg in request.args() {
        expand_bazel_worker_arg(
            artifact_fs,
            request,
            materialized_inputs,
            arg,
            &mut expanded,
        )?;
    }
    Ok(expanded)
}

fn expand_bazel_worker_arg(
    artifact_fs: &ArtifactFs,
    request: &CommandExecutionRequest,
    materialized_inputs: &MaterializedInputPaths,
    arg: &str,
    expanded: &mut Vec<String>,
) -> buck2_error::Result<()> {
    if !is_bazel_worker_flag_file_arg(arg) {
        expanded.push(arg.to_owned());
        return Ok(());
    }

    let arg_path = &arg[1..];
    let path = resolve_bazel_worker_flag_file_path(request, materialized_inputs, arg_path)?;
    let content = fs_util::read_to_string(artifact_fs.fs().resolve(&path))
        .categorize_internal()
        .with_buck_error_context(|| format!("Error reading Bazel worker flag file `{arg_path}`"))?;
    for line in content.lines() {
        expand_bazel_worker_arg(artifact_fs, request, materialized_inputs, line, expanded)?;
    }
    Ok(())
}

fn is_bazel_worker_flag_file_arg(arg: &str) -> bool {
    arg.starts_with('@') && !arg.starts_with("@@") && !arg[1..].contains("//")
}

fn resolve_bazel_worker_flag_file_path(
    request: &CommandExecutionRequest,
    materialized_inputs: &MaterializedInputPaths,
    arg_path: &str,
) -> buck2_error::Result<ProjectRelativePathBuf> {
    let relative = ForwardRelativePathBuf::new(arg_path.to_owned())
        .buck_error_context("Invalid Bazel worker flag-file path")?;
    let execroot_path = request.working_directory().join(relative);
    for (source_path, alias_path) in &materialized_inputs.artifact_path_aliases {
        if alias_path == &execroot_path {
            return Ok(source_path.clone());
        }
    }
    Ok(execroot_path)
}

fn materialize_artifact_path_alias(
    artifact_fs: &ArtifactFs,
    source_path: &ProjectRelativePath,
    path: &ProjectRelativePath,
) -> buck2_error::Result<()> {
    let fs = artifact_fs.fs();
    let source = fs.resolve(source_path);
    let dest = fs.resolve(path);
    if artifact_path_alias_is_current(&dest, &source) {
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        fs_util::create_dir_all(parent).with_buck_error_context(|| {
            format!("Error creating parent directory for artifact path alias `{path}`")
        })?;
    }

    match create_artifact_path_alias_symlink(&source, &dest) {
        Ok(()) => Ok(()),
        Err(e) if artifact_path_alias_is_current(&dest, &source) => Ok(()),
        Err(_) => {
            CleanOutputPaths::clean(std::iter::once(path), fs)?;
            match create_artifact_path_alias_symlink(&source, &dest) {
                Ok(()) => Ok(()),
                Err(e) if artifact_path_alias_is_current(&dest, &source) => Ok(()),
                Err(e) => Err(e).with_buck_error_context(|| {
                    format!("Error creating artifact path alias `{path}` -> `{source_path}`")
                }),
            }
        }
    }
}

fn materialize_external_cell_root_alias(
    artifact_fs: &ArtifactFs,
    source_root: &ProjectRelativePath,
    alias_root: &ProjectRelativePath,
) -> buck2_error::Result<()> {
    let fs = artifact_fs.fs();
    let source = fs.resolve(source_root);
    let dest = fs.resolve(alias_root);

    create_or_replace_symlink(source.as_path(), &dest).with_buck_error_context(|| {
        format!("Error creating external repository alias `{alias_root}` -> `{source_root}`")
    })
}

fn create_artifact_path_alias_symlink(
    source: &AbsNormPathBuf,
    dest: &AbsNormPathBuf,
) -> buck2_error::Result<()> {
    if artifact_path_alias_is_current(dest, source) {
        return Ok(());
    }

    match fs_util::symlink(source, dest).categorize_internal() {
        Ok(()) => Ok(()),
        Err(e) if artifact_path_alias_is_current(dest, source) => Ok(()),
        Err(e) => {
            let tmp = artifact_path_alias_tmp_path(dest)?;
            let _ignored = fs_util::remove_file(&tmp);
            fs_util::symlink(source, &tmp)
                .categorize_internal()
                .buck_error_context("Error creating temporary artifact path alias")?;
            match fs_util::rename(&tmp, dest).categorize_internal() {
                Ok(()) => Ok(()),
                Err(rename_error) if artifact_path_alias_is_current(dest, source) => {
                    let _ignored = fs_util::remove_file(&tmp);
                    Ok(())
                }
                Err(rename_error) => {
                    let _ignored = fs_util::remove_file(&tmp);
                    if artifact_path_alias_is_current(dest, source) {
                        Ok(())
                    } else {
                        Err(rename_error).buck_error_context(e.to_string())
                    }
                }
            }
        }
    }
}

fn artifact_path_alias_tmp_path(dest: &AbsNormPathBuf) -> buck2_error::Result<AbsNormPathBuf> {
    let parent = dest.parent().ok_or_else(|| {
        buck2_error!(
            buck2_error::ErrorTag::Tier0,
            "Artifact path alias has no parent directory"
        )
    })?;
    let file_name = dest
        .as_path()
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "alias".into());
    let id = ARTIFACT_PATH_ALIAS_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut tmp = parent.as_path().to_owned();
    tmp.push(format!(
        ".{file_name}.buck2-tmp-{}-{id}",
        std::process::id()
    ));
    AbsNormPathBuf::new(tmp)
}

fn artifact_path_alias_is_current(dest: &AbsNormPathBuf, source: &AbsNormPathBuf) -> bool {
    fs_util::read_link(dest)
        .map(|target| artifact_path_alias_target_is_compatible(dest, source, &target))
        .unwrap_or(false)
}

fn artifact_path_alias_target_is_compatible(
    dest: &AbsNormPathBuf,
    source: &AbsNormPathBuf,
    target: &Path,
) -> bool {
    let Some(target) = resolve_artifact_path_alias_target(dest, source, target) else {
        return false;
    };

    target.as_path() == source.as_path()
        || artifact_path_alias_files_are_equivalent(&target, source)
}

fn resolve_artifact_path_alias_target(
    dest: &AbsNormPathBuf,
    source: &AbsNormPathBuf,
    target: &Path,
) -> Option<AbsNormPathBuf> {
    if target == source.as_path() {
        return Some(source.clone());
    }

    if target.is_absolute() {
        return AbsNormPathBuf::new(target.to_owned()).ok();
    }

    let Some(parent) = dest.parent() else {
        return None;
    };
    fs_util::relative_path_from_system(target)
        .and_then(|target| parent.join_normalized(target.as_ref()))
        .ok()
}

fn artifact_path_alias_files_are_equivalent(
    target: &AbsNormPathBuf,
    source: &AbsNormPathBuf,
) -> bool {
    let Ok(target_metadata) = fs_util::metadata(target) else {
        return false;
    };
    let Ok(source_metadata) = fs_util::metadata(source) else {
        return false;
    };
    if !target_metadata.is_file()
        || !source_metadata.is_file()
        || target_metadata.len() != source_metadata.len()
    {
        return false;
    }

    let Ok(target_content) = fs_util::read(target) else {
        return false;
    };
    let Ok(source_content) = fs_util::read(source) else {
        return false;
    };
    target_content == source_content
}

fn promote_produced_output_path(
    artifact_fs: &ArtifactFs,
    produced_path: &ProjectRelativePath,
    output_path: &ProjectRelativePath,
) -> buck2_error::Result<()> {
    if produced_path == output_path {
        return Ok(());
    }

    let fs = artifact_fs.fs();
    let produced = fs.resolve(produced_path);
    if !fs_util::try_exists(&produced)? {
        return Ok(());
    }

    CleanOutputPaths::clean(std::iter::once(output_path), fs)?;
    let output = fs.resolve(output_path);
    if let Some(parent) = output.parent() {
        fs_util::create_dir_all(parent).with_buck_error_context(|| {
            format!("Error creating parent directory for output path `{output_path}`")
        })?;
    }
    match fs_util::rename(&produced, &output) {
        Ok(()) => return Ok(()),
        Err(_rename_error) => {
            fs.copy(produced_path, output_path)
                .with_buck_error_context(|| {
                    format!("Error copying produced output `{produced_path}` to `{output_path}`")
                })?;
        }
    }

    Ok(())
}

/// Materialize all inputs artifact for CommandExecutionRequest so the command can be executed
/// locally.
///
/// This also discovers the scratch directory if any was passed, but does not yet do anything with
/// it - call `prep_scratch_path`.
pub async fn materialize_inputs(
    artifact_fs: &ArtifactFs,
    materializer: &dyn Materializer,
    request: &CommandExecutionRequest,
    digest_config: DigestConfig,
) -> buck2_error::Result<MaterializedInputPaths> {
    let mut paths = vec![];
    let mut scratch = ScratchPath(None);
    let mut configuration_path_to_content_based_path_symlinks = vec![];
    let mut shared_artifact_path_aliases = vec![];
    let mut sandbox_artifact_path_aliases = vec![];
    let mut external_cell_roots_to_materialize = BuckIndexSet::new();
    let use_bazel_worker_sandbox = request.worker().as_ref().is_some_and(|worker| {
        worker.protocol == WorkerProtocol::Bazel && worker.bazel_worker_sandboxing
    });

    let worker_inputs = request
        .worker()
        .as_ref()
        .map(|w| w.inputs())
        .unwrap_or_default();
    for (input, is_worker_input) in request
        .inputs()
        .iter()
        .map(|input| (input, false))
        .chain(worker_inputs.iter().map(|input| (input, true)))
    {
        match input {
            CommandExecutionInput::Artifact(group) => {
                for (artifact, artifact_value) in group.iter() {
                    if artifact.requires_materialization(artifact_fs) {
                        let configuration_hash_path =
                            artifact.resolve_configuration_hash_path(artifact_fs)?;

                        if artifact.has_content_based_path() {
                            let content_based_path = artifact.resolve_path(
                                artifact_fs,
                                Some(&artifact_value.content_based_path_hash()),
                            )?;

                            // TODO(ianc) We want to also create symlinks here for projected artifacts.
                            if artifact.is_projected() {
                                paths.push(content_based_path);
                            } else {
                                let mut builder =
                                    ArtifactValueBuilder::new(artifact_fs.fs(), digest_config);
                                builder.add_symlinked(
                                    artifact_value,
                                    content_based_path,
                                    &configuration_hash_path,
                                )?;
                                let symlink_value = builder.build(&configuration_hash_path)?;
                                configuration_path_to_content_based_path_symlinks
                                    .push((configuration_hash_path.clone(), symlink_value));
                                paths.push(configuration_hash_path);
                            }
                        } else {
                            paths.push(configuration_hash_path);
                        }
                    }
                }
            }
            CommandExecutionInput::ArtifactPathAlias {
                source_path,
                source_requires_materialization,
                path,
                ..
            } => {
                if *source_requires_materialization {
                    if let Some(root) = external_cell_root(source_path.as_ref()) {
                        external_cell_roots_to_materialize.insert(root.source_root);
                    } else {
                        paths.push(source_path.clone());
                    }
                }
                let sandbox_alias = use_bazel_worker_sandbox
                    && !is_worker_input
                    && path
                        .strip_prefix_opt(request.working_directory())
                        .is_some_and(|relative| relative.as_str().starts_with("buck-out/"));
                if sandbox_alias {
                    sandbox_artifact_path_aliases.push((source_path.clone(), path.clone()));
                } else {
                    shared_artifact_path_aliases.push((source_path.clone(), path.clone()));
                }
            }
            CommandExecutionInput::ActionMetadata(metadata) => {
                let path = artifact_fs
                    .buck_out_path_resolver()
                    .resolve_gen(&metadata.path, Some(&metadata.content_hash))?;
                paths.push(path);
            }
            CommandExecutionInput::ScratchPath(path) => {
                let path = artifact_fs.buck_out_path_resolver().resolve_scratch(path)?;

                if scratch.0.is_some() {
                    return Err(buck2_error::internal_error!(
                        "Multiple scratch paths for one action"
                    ));
                }
                scratch.0 = Some(path);
            }
            CommandExecutionInput::IncrementalRemoteOutput(..) => {
                // Ignore, should be already materialized
            }
        }
    }

    paths.extend(external_cell_roots_to_materialize);

    buck2_util::future::try_join_all(
        configuration_path_to_content_based_path_symlinks
            .into_iter()
            .map(|(path, value)| materializer.declare_copy(path, value, vec![], None)),
    )
    .await?;
    let mut stream = materializer.materialize_many(paths.clone()).await?;
    while let Some(res) = stream.next().await {
        match res {
            Ok(()) => {}
            Err(MaterializationError::NotFound { source }) => {
                let corrupted = source.info.origin.guaranteed_by_action_cache();

                return Err(tag_error!(
                    "cas_missing_fatal",
                    MaterializationError::NotFound { source }.into(),
                    quiet: true,
                    task: false,
                    daemon_in_memory_state_is_corrupted: true,
                    action_cache_is_corrupted: corrupted
                ));
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    let mut external_cell_root_aliases = BuckIndexSet::new();
    for (source_path, path) in &shared_artifact_path_aliases {
        if let Some((source_root, alias_root)) =
            external_cell_root_alias(source_path.as_ref(), path.as_ref())
        {
            if external_cell_root_aliases.insert((source_root.clone(), alias_root.clone())) {
                materialize_external_cell_root_alias(
                    artifact_fs,
                    source_root.as_ref(),
                    alias_root.as_ref(),
                )?;
                paths.push(alias_root);
            }
        } else {
            materialize_artifact_path_alias(artifact_fs, source_path.as_ref(), path.as_ref())?;
            paths.push(path.clone());
        }
    }

    Ok(MaterializedInputPaths {
        scratch,
        paths,
        artifact_path_aliases: sandbox_artifact_path_aliases,
    })
}

/// A scratch path discovered during `materialize_inputs`.
pub struct ScratchPath(Option<ProjectRelativePathBuf>);

pub async fn prep_scratch_path(
    scratch_path: &ScratchPath,
    artifact_fs: &ArtifactFs,
) -> buck2_error::Result<()> {
    let Some(path) = scratch_path.0.as_ref() else {
        return Ok(());
    };
    CleanOutputPaths::clean(std::iter::once(path.as_ref()), artifact_fs.fs())?;
    async_fs_util::create_dir_all(artifact_fs.fs().resolve(path)).await
}

async fn check_inputs(
    manager: CommandExecutionManagerWithClaim,
    artifact_fs: &ArtifactFs,
    blocking_executor: &dyn BlockingExecutor,
    request: &CommandExecutionRequest,
) -> ControlFlow<CommandExecutionResult, CommandExecutionManagerWithClaim> {
    let res = blocking_executor
        .execute_io_inline(|| {
            for input in request.inputs() {
                match input {
                    CommandExecutionInput::Artifact(group) => {
                        for (artifact, artifact_value) in group.iter() {
                            if artifact.requires_materialization(artifact_fs) {
                                let path = artifact.resolve_path(artifact_fs,
                                    if artifact.has_content_based_path() {
                                        Some(artifact_value.content_based_path_hash())
                                    } else {
                                        None
                                    }.as_ref())?;
                                let abs_path = artifact_fs.fs().resolve(&path);

                                // We ignore the result here because while we want to tag it, we'd
                                // prefer to just show the normal error to the user, so we don't
                                // want to propagate it.
                                let _ignored = tag_result!(
                                    "missing_local_inputs",
                                    fs_util::symlink_metadata(&abs_path).categorize_internal().buck_error_context("Missing input"),
                                    quiet: true,
                                    task: false,
                                    daemon_materializer_state_is_corrupted: true
                                );
                            }
                        }
                    }
                    CommandExecutionInput::ArtifactPathAlias { path, .. } => {
                        let abs_path = artifact_fs.fs().resolve(path);

                        // We ignore the result here because while we want to tag it, we'd
                        // prefer to just show the normal error to the user, so we don't
                        // want to propagate it.
                        let _ignored = tag_result!(
                            "missing_local_inputs",
                            fs_util::symlink_metadata(&abs_path).categorize_internal().buck_error_context("Missing input"),
                            quiet: true,
                            task: false,
                            daemon_materializer_state_is_corrupted: true
                        );
                    }
                    CommandExecutionInput::ActionMetadata(..) => {
                        // Ignore those here.
                    }
                    CommandExecutionInput::ScratchPath(..) => {
                        // Nothing to look at
                    }
                    CommandExecutionInput::IncrementalRemoteOutput(..) => {
                        // Ignore
                    }
                }
            }

            Ok(())
        })
        .await;

    match res {
        Ok(()) => ControlFlow::Continue(manager),
        Err(err) => ControlFlow::Break(manager.error("local_check_inputs", err)),
    }
}

/// Materialize all output artifact for CommandExecutionRequest.
///
/// Note that the outputs could be from the previous run of the same command if cleanup on the action was not performed.
/// The above is useful when executing incremental actions first remotely and then locally.
/// In that case output from remote execution which is incremental state should be materialized prior to local execution.
/// Such incremental state in fact serves as the input while being output as well.
async fn materialize_build_outputs(
    artifact_fs: &ArtifactFs,
    incremental_db_state: &Arc<IncrementalDbState>,
    materializer: &dyn Materializer,
    request: &CommandExecutionRequest,
) -> buck2_error::Result<Vec<ProjectRelativePathBuf>> {
    let mut paths = vec![];
    let path_map = get_incremental_path_map(incremental_db_state, request.run_action_key());
    for output in request.outputs() {
        match output {
            CommandExecutionOutputRef::BuildArtifact { path, .. } => {
                if path.is_content_based_path() {
                    if let Some(ref state) = path_map {
                        let p = path.path().to_buf();
                        if let Some(content_path) = state.get(&p) {
                            paths.push(content_path.clone());
                        }
                    }
                } else {
                    paths.push(artifact_fs.resolve_build(path, None)?);
                }
            }
            CommandExecutionOutputRef::TestPath { .. } => {}
        }
    }

    materializer.ensure_materialized(paths.clone()).await?;

    Ok(paths)
}

/// Create any output dirs requested by the command. Note that this makes no effort to delete
/// the output paths first. Eventually it should, but right now this happens earlier. This
/// would be a separate refactor.
pub async fn create_output_dirs(
    artifact_fs: &ArtifactFs,
    request: &CommandExecutionRequest,
    materializer: Arc<dyn Materializer>,
    blocking_executor: Arc<dyn BlockingExecutor>,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    let outputs: Vec<_> = request
        .outputs()
        .map(|output| {
            let produced = output.resolve_for_execution(
                artifact_fs,
                Some(&ContentBasedPathHash::for_output_artifact()),
            )?;
            let declared = output.resolve(
                artifact_fs,
                Some(&ContentBasedPathHash::for_output_artifact()),
            )?;
            Ok((produced, declared))
        })
        .collect::<buck2_error::Result<Vec<_>>>()?;

    // Invalidate all the output paths this action might provide. Note that this is a bit
    // approximative: we might have previous instances of this action that declared
    // different outputs with a different materialization method that will become invalid
    // now. However, nothing should reference those stale outputs, so while this does not
    // do a good job of cleaning up garbage, it prevents using invalid artifacts.
    let mut output_paths = Vec::new();
    for (produced, declared) in &outputs {
        output_paths.push(produced.path().to_owned());
        if produced.path() != declared.path() {
            output_paths.push(declared.path().to_owned());
        }
    }
    materializer.invalidate_many(output_paths.clone()).await?;

    if request.outputs_cleanup {
        // TODO(scottcao): Move this deletion logic into materializer itself.
        blocking_executor
            .execute_io(
                Box::new(CleanOutputPaths {
                    paths: output_paths,
                }),
                cancellations,
            )
            .await
            .buck_error_context("Failed to cleanup output directory")?;
    }

    let project_fs = artifact_fs.fs();
    for (produced, declared) in outputs {
        if let Some(path) = produced.path_to_create() {
            fs_util::create_dir_all(project_fs.resolve(path))?;
        }
        if produced.path() != declared.path()
            && let Some(path) = declared.path_to_create()
        {
            fs_util::create_dir_all(project_fs.resolve(path))?;
        }
    }

    Ok(())
}

pub fn apply_local_execution_environment(
    builder: &mut impl EnvironmentBuilder,
    working_directory: &AbsPath,
    env: impl IntoIterator<Item = (impl AsRef<OsStr>, impl AsRef<OsStr>)>,
    env_inheritance: Option<&EnvironmentInheritance>,
) {
    if let Some(env_inheritance) = env_inheritance {
        if env_inheritance.clear() {
            builder.clear();
        }

        for key in env_inheritance.exclusions() {
            builder.remove(key);
        }

        for (key, val) in env_inheritance.values() {
            builder.set(key, val);
        }
    }
    for (key, val) in env {
        builder.set(key, val);
    }
    builder.set("PWD", working_directory.as_path());
}

pub trait EnvironmentBuilder {
    fn clear(&mut self);

    fn set<K, V>(&mut self, key: K, val: V)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>;

    fn remove<K>(&mut self, key: K)
    where
        K: AsRef<OsStr>;
}

impl EnvironmentBuilder for Command {
    fn clear(&mut self) {
        Command::env_clear(self);
    }

    fn set<K, V>(&mut self, key: K, val: V)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        Command::env(self, key, val);
    }

    fn remove<K>(&mut self, key: K)
    where
        K: AsRef<OsStr>,
    {
        Command::env_remove(self, key);
    }
}

#[cfg(unix)]
mod unix {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    pub async fn exec_via_forkserver(
        forkserver: &buck2_forkserver::client::ForkserverClient,
        exe: impl AsRef<OsStr>,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
        env: impl IntoIterator<Item = (impl AsRef<OsStr>, impl AsRef<OsStr>)>,
        working_directory: &AbsPath,
        command_timeout: Option<Duration>,
        env_inheritance: Option<&EnvironmentInheritance>,
        liveliness_observer: impl LivelinessObserver + 'static,
        enable_miniperf: bool,
        cgroup_path: Option<CgroupPathBuf>,
        freeze_rx: impl ActionFreezeEventReceiver,
        network_access: Option<NetworkAccess>,
    ) -> buck2_error::Result<CommandResult> {
        let exe = exe.as_ref();

        let mut req = buck2_forkserver_proto::CommandRequest {
            exe: exe.as_bytes().to_vec(),
            argv: args
                .into_iter()
                .map(|s| s.as_ref().as_bytes().to_vec())
                .collect(),
            cwd: Some(buck2_forkserver_proto::WorkingDirectory {
                path: working_directory.as_path().as_os_str().as_bytes().to_vec(),
            }),
            env: vec![],
            timeout: command_timeout.try_map(|d| d.try_into())?,
            enable_miniperf,
            std_redirects: None,
            graceful_shutdown_timeout_s: None,
            command_cgroup: cgroup_path.map(|p| p.to_string()),
            network_access: network_access.map(|n| n.into()),
        };
        apply_local_execution_environment(&mut req, working_directory, env, env_inheritance);
        forkserver
            .execute(
                req,
                async move { liveliness_observer.while_alive().await },
                freeze_rx,
            )
            .await
    }

    trait CommandRequestExt {
        fn push_env_directive<D>(&mut self, directive: D)
        where
            D: Into<buck2_forkserver_proto::env_directive::Data>;
    }

    impl CommandRequestExt for buck2_forkserver_proto::CommandRequest {
        fn push_env_directive<D>(&mut self, directive: D)
        where
            D: Into<buck2_forkserver_proto::env_directive::Data>,
        {
            self.env.push(buck2_forkserver_proto::EnvDirective {
                data: Some(directive.into()),
            });
        }
    }

    impl EnvironmentBuilder for buck2_forkserver_proto::CommandRequest {
        fn clear(&mut self) {
            self.push_env_directive(buck2_forkserver_proto::EnvClear {});
        }

        fn set<K, V>(&mut self, key: K, val: V)
        where
            K: AsRef<OsStr>,
            V: AsRef<OsStr>,
        {
            self.push_env_directive(buck2_forkserver_proto::EnvSet {
                key: key.as_ref().as_bytes().to_vec(),
                value: val.as_ref().as_bytes().to_vec(),
            })
        }

        fn remove<K>(&mut self, key: K)
        where
            K: AsRef<OsStr>,
        {
            self.push_env_directive(buck2_forkserver_proto::EnvRemove {
                key: key.as_ref().as_bytes().to_vec(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str;

    use assert_matches::assert_matches;
    use buck2_common::liveliness_observer::NoopLivelinessObserver;
    use buck2_core::cells::CellResolver;
    use buck2_core::cells::cell_root_path::CellRootPathBuf;
    use buck2_core::cells::name::CellName;
    use buck2_core::fs::buck_out_path::BuckOutPathResolver;
    use buck2_core::fs::project::ProjectRoot;
    use buck2_core::fs::project::ProjectRootTemp;
    use buck2_execute::execute::blocking::testing::DummyBlockingExecutor;
    use buck2_execute::materialize::nodisk::NoDiskMaterializer;
    use buck2_hash::StdBuckHashMap;
    use host_sharing::HostSharingStrategy;

    use super::*;

    fn artifact_fs(project_fs: ProjectRoot) -> ArtifactFs {
        ArtifactFs::new(
            CellResolver::testing_with_name_and_path(
                CellName::testing_new("cell"),
                CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("cell_path".into())),
            ),
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new("buck_out/v2".into())),
            project_fs,
        )
    }

    #[test]
    fn test_artifact_path_alias_replaces_stale_symlink() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        temp.write_file("source", "new");
        temp.write_file("old_source", "old");
        let source = temp.path().resolve(ProjectRelativePath::new("source")?);
        let old_source = temp.path().resolve(ProjectRelativePath::new("old_source")?);
        let dest = temp.path().resolve(ProjectRelativePath::new("dest")?);

        fs_util::symlink(&old_source, &dest).categorize_internal()?;
        create_artifact_path_alias_symlink(&source, &dest)?;

        assert!(artifact_path_alias_is_current(&dest, &source));
        assert_eq!(
            fs_util::read_link(&dest).categorize_internal()?.as_path(),
            source.as_path()
        );
        Ok(())
    }

    fn test_executor() -> buck2_error::Result<(LocalExecutor, AbsNormPathBuf, ProjectRootTemp)> {
        let temp = ProjectRootTemp::new().unwrap();
        let project_fs = temp.path();
        let artifact_fs = artifact_fs(project_fs.dupe());

        let executor = LocalExecutor::new(
            artifact_fs,
            Arc::new(NoDiskMaterializer),
            Arc::new(IncrementalDbState::db_disabled()),
            Arc::new(LocalActionCache::testing_new_in_memory()?),
            Arc::new(DummyBlockingExecutor {
                fs: project_fs.dupe(),
            }),
            Arc::new(HostSharingBroker::new(
                HostSharingStrategy::SmallerTasksFirst,
                1,
            )),
            temp.path().root().to_buf(),
            ForkserverAccess::None,
            ExecutorGlobalKnobs::default(),
            None,
            None,
            DaemonId::new(),
        );

        Ok((executor, temp.path().root().to_buf(), temp))
    }

    #[tokio::test]
    async fn test_exec_cmd_environment() -> buck2_error::Result<()> {
        let (executor, root, _tmpdir) = test_executor()?;

        let interpreter = if cfg!(windows) { "powershell" } else { "sh" };
        let CommandResult { status, stdout, .. } = executor
            .exec(
                interpreter,
                ["-c", "echo $PWD; pwd"],
                &StdBuckHashMap::<String, String>::default(),
                ProjectRelativePath::empty(),
                None,
                None,
                NoopLivelinessObserver::create(),
                false,
                None,
                futures::stream::pending(),
                None,
            )
            .await?;
        assert_matches!(status, GatherOutputStatus::Finished { exit_code, .. } if exit_code == 0);

        let stdout = std::str::from_utf8(&stdout).buck_error_context("Invalid stdout")?;

        if cfg!(windows) {
            let lines: Vec<&str> = stdout.split("\r\n").collect();
            let expected_path = format!("{root}");

            assert_eq!(lines[3], expected_path);
            assert_eq!(lines[4], expected_path);
        } else {
            assert_eq!(stdout, format!("{root}\n{root}\n"));
        }

        Ok(())
    }

    #[cfg(fbcode_build)]
    #[tokio::test]
    async fn test_exec_cmd_timeout() -> buck2_error::Result<()> {
        let (executor, _, _tmpdir) = test_executor()?;

        let interpreter = if cfg!(windows) { "powershell" } else { "sh" };
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 2"
        } else {
            "sleep 2"
        };
        let CommandResult { status, .. } = executor
            .exec(
                interpreter,
                ["-c", command],
                &StdBuckHashMap::<String, String>::default(),
                ProjectRelativePath::empty(),
                Some(Duration::from_secs(1)),
                None,
                NoopLivelinessObserver::create(),
                false,
                None,
                futures::stream::pending(),
                None,
            )
            .await?;
        assert_matches!(status, GatherOutputStatus::TimedOut ( duration ) if duration == Duration::from_secs(1));

        Ok(())
    }

    #[cfg(unix)] // TODO: something similar on Windows: T123279320
    #[tokio::test]
    async fn test_exec_cmd_environment_filtering() -> buck2_error::Result<()> {
        use buck2_execute::execute::environment_inheritance::EnvironmentInheritance;

        let (executor, _root, _tmpdir) = test_executor()?;

        let CommandResult { status, stdout, .. } = executor
            .exec(
                "sh",
                ["-c", "echo $USER"],
                &StdBuckHashMap::<String, String>::default(),
                ProjectRelativePath::empty(),
                None,
                Some(&EnvironmentInheritance::empty()),
                NoopLivelinessObserver::create(),
                false,
                None,
                futures::stream::pending(),
                None,
            )
            .await?;
        assert_matches!(status, GatherOutputStatus::Finished { exit_code, .. } if exit_code == 0);
        assert_eq!(stdout, b"\n");

        Ok(())
    }
}
