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
use std::sync::OnceLock;

use async_trait::async_trait;
use bz_build_api::actions::execute::dice_data::CommandExecutorResponse;
use bz_build_api::actions::execute::dice_data::HasCommandExecutor;
use bz_cli_proto::client_context::HostPlatformOverride;
use bz_cli_proto::common_build_options::ExecutionStrategy;
use bz_common::init::RemoteExecutionStartupConfig;
use bz_core::bz_env;
use bz_core::execution_types::executor_config::CacheUploadBehavior;
use bz_core::execution_types::executor_config::CommandExecutorConfig;
use bz_core::execution_types::executor_config::CommandGenerationOptions;
use bz_core::execution_types::executor_config::Executor;
use bz_core::execution_types::executor_config::HybridExecutionLevel;
use bz_core::execution_types::executor_config::LocalExecutorOptions;
use bz_core::execution_types::executor_config::MetaInternalExtraParams;
use bz_core::execution_types::executor_config::PathSeparatorKind;
use bz_core::execution_types::executor_config::ReGangWorker;
use bz_core::execution_types::executor_config::RePlatformFields;
use bz_core::execution_types::executor_config::RemoteEnabledExecutor;
use bz_core::execution_types::executor_config::RemoteEnabledExecutorOptions;
use bz_core::execution_types::executor_config::RemoteExecutorDependency;
use bz_core::execution_types::executor_config::RemoteExecutorOptions;
use bz_core::execution_types::executor_config::RemoteExecutorUseCase;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::project::ProjectRoot;
use bz_events::daemon_id::DaemonId;
use bz_execute::execute::blocking::BlockingExecutor;
use bz_execute::execute::cache_uploader::NoOpCacheUploader;
use bz_execute::execute::cache_uploader::force_cache_upload;
use bz_execute::execute::prepared::NoOpCommandOptionalExecutor;
use bz_execute::execute::prepared::PreparedCommandExecutor;
use bz_execute::execute::prepared::PreparedCommandOptionalExecutor;
use bz_execute::execute::request::ExecutorPreference;
use bz_execute::knobs::ExecutorGlobalKnobs;
use bz_execute::materialize::materializer::Materializer;
use bz_execute::re::manager::ManagedRemoteExecutionClient;
use bz_execute::re::manager::ReConnectionHandle;
use bz_execute::re::output_trees_download_config::OutputTreesDownloadConfig;
use bz_execute_impl::executors::action_cache::ActionCacheChecker;
use bz_execute_impl::executors::action_cache::RemoteDepFileCacheChecker;
use bz_execute_impl::executors::action_cache_upload_permission_checker::ActionCacheUploadPermissionChecker;
use bz_execute_impl::executors::caching::CacheUploader;
use bz_execute_impl::executors::hybrid::FallbackTracker;
use bz_execute_impl::executors::hybrid::HybridExecutor;
use bz_execute_impl::executors::local::ForkserverAccess;
use bz_execute_impl::executors::local::LocalExecutor;
use bz_execute_impl::executors::local::LocalExecutorSharedState;
use bz_execute_impl::executors::local_action_cache::ChainedCommandOptionalExecutor;
use bz_execute_impl::executors::local_action_cache::LocalActionCache;
use bz_execute_impl::executors::re::ReExecutor;
use bz_execute_impl::executors::stacked::StackedExecutor;
use bz_execute_impl::executors::to_re_platform::RePlatformFieldsToRePlatform;
use bz_execute_impl::executors::worker::WorkerPool;
use bz_execute_impl::low_pass_filter::LowPassFilter;
use bz_execute_impl::re::paranoid_download::ParanoidDownloader;
use bz_execute_impl::sqlite::incremental_state_db::IncrementalDbState;
use bz_resource_control::memory_tracker::MemoryTrackerHandle;
use dupe::Dupe;
use host_sharing::HostSharingBroker;
use tokio::sync::Semaphore;

/// For each buck invocations, we'll have a single CommandExecutorFactory. This contains shared
/// state used by all command executor strategies.
pub struct CommandExecutorFactory {
    re_connection: Arc<ReConnectionHandle>,
    // TODO(cjhopman): This should probably be a global limit, otherwise simultaneous commands may
    // use more resources than intended (this might no longer be accurate since only instances
    // sharing the same DICE context should be allowed to proceed concurrently, and we only have
    // one CommandExecutorFactory per DICE context).
    host_sharing_broker: Arc<HostSharingBroker>,
    low_pass_filter: Arc<LowPassFilter>,
    materializer: Arc<dyn Materializer>,
    blocking_executor: Arc<dyn BlockingExecutor>,
    strategy: ExecutionStrategy,
    executor_global_knobs: ExecutorGlobalKnobs,
    upload_all_actions: bool,
    forkserver: ForkserverAccess,
    skip_cache_read: bool,
    skip_cache_write: bool,
    project_root: ProjectRoot,
    worker_pool: Arc<WorkerPool>,
    paranoid: Option<ParanoidDownloader>,
    materialize_failed_inputs: bool,
    materialize_failed_outputs: bool,
    /// Cache permission checks per command.
    cache_upload_permission_checker: Arc<ActionCacheUploadPermissionChecker>,
    fallback_tracker: Arc<FallbackTracker>,
    re_use_case_override: Option<RemoteExecutorUseCase>,
    memory_tracker: Option<MemoryTrackerHandle>,
    incremental_db_state: Arc<IncrementalDbState>,
    local_action_cache: Arc<LocalActionCache>,
    local_executor_shared_state: LocalExecutorSharedState,
    deduplicate_get_digests_ttl_calls: bool,
    output_trees_download_config: OutputTreesDownloadConfig,
    remote_action_building_semaphore: Arc<Semaphore>,
    remote_metadata_semaphore: Arc<Semaphore>,
    remote_action_cache_semaphore: Arc<Semaphore>,
    daemon_id: DaemonId,
    bazel_remote_endpoint_overrides: BazelRemoteEndpointOverrides,
}

impl CommandExecutorFactory {
    pub fn new(
        re_connection: Arc<ReConnectionHandle>,
        host_sharing_broker: HostSharingBroker,
        low_pass_filter: LowPassFilter,
        materializer: Arc<dyn Materializer>,
        blocking_executor: Arc<dyn BlockingExecutor>,
        strategy: ExecutionStrategy,
        executor_global_knobs: ExecutorGlobalKnobs,
        upload_all_actions: bool,
        forkserver: ForkserverAccess,
        skip_cache_read: bool,
        skip_cache_write: bool,
        project_root: ProjectRoot,
        worker_pool: Arc<WorkerPool>,
        paranoid: Option<ParanoidDownloader>,
        materialize_failed_inputs: bool,
        materialize_failed_outputs: bool,
        re_use_case_override: Option<RemoteExecutorUseCase>,
        memory_tracker: Option<MemoryTrackerHandle>,
        incremental_db_state: Arc<IncrementalDbState>,
        local_action_cache: Arc<LocalActionCache>,
        deduplicate_get_digests_ttl_calls: bool,
        output_trees_download_config: OutputTreesDownloadConfig,
        remote_metadata_concurrency: usize,
        remote_action_cache_concurrency: usize,
        daemon_id: DaemonId,
        remote_execution_startup_config: &RemoteExecutionStartupConfig,
    ) -> Self {
        let cache_upload_permission_checker = Arc::new(ActionCacheUploadPermissionChecker::new());

        Self {
            re_connection,
            host_sharing_broker: Arc::new(host_sharing_broker),
            low_pass_filter: Arc::new(low_pass_filter),
            materializer,
            blocking_executor,
            strategy,
            executor_global_knobs,
            upload_all_actions,
            forkserver,
            skip_cache_read,
            skip_cache_write,
            project_root,
            worker_pool,
            paranoid,
            materialize_failed_inputs,
            materialize_failed_outputs,
            cache_upload_permission_checker,
            fallback_tracker: Arc::new(FallbackTracker::new()),
            re_use_case_override,
            memory_tracker,
            incremental_db_state,
            local_action_cache,
            local_executor_shared_state: LocalExecutorSharedState::default(),
            deduplicate_get_digests_ttl_calls,
            output_trees_download_config,
            remote_action_building_semaphore: Arc::new(Semaphore::new(
                std::thread::available_parallelism().map_or(1, |value| value.get()),
            )),
            remote_metadata_semaphore: Arc::new(Semaphore::new(remote_metadata_concurrency)),
            remote_action_cache_semaphore: Arc::new(Semaphore::new(
                remote_action_cache_concurrency,
            )),
            daemon_id,
            bazel_remote_endpoint_overrides: BazelRemoteEndpointOverrides::from_startup_config(
                remote_execution_startup_config,
            ),
        }
    }

    fn get_prepared_re_client(
        &self,
        use_case: RemoteExecutorUseCase,
    ) -> ManagedRemoteExecutionClient {
        let use_case = self.re_use_case_override.unwrap_or(use_case);
        self.re_connection.get_client().with_use_case(use_case)
    }

    fn executor_config_with_bazel_remote_endpoint_overrides(
        &self,
        executor_config: &CommandExecutorConfig,
    ) -> CommandExecutorConfig {
        executor_config_with_bazel_remote_endpoint_overrides(
            executor_config,
            self.bazel_remote_endpoint_overrides.clone(),
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct BazelRemoteEndpointOverrides {
    remote_cache: Option<bool>,
    remote_executor: bool,
    remote_default_exec_properties: Option<RePlatformFields>,
}

impl BazelRemoteEndpointOverrides {
    fn from_startup_config(config: &RemoteExecutionStartupConfig) -> Self {
        Self {
            remote_cache: config.remote_cache_endpoint_enabled(),
            remote_executor: config.remote_executor_endpoint_enabled(),
            remote_default_exec_properties: config.remote_default_exec_properties.as_ref().map(
                |properties| RePlatformFields {
                    properties: Arc::new(
                        properties
                            .iter()
                            .map(|property| (property.name.clone(), property.value.clone()))
                            .collect(),
                    ),
                },
            ),
        }
    }

    fn is_empty(&self) -> bool {
        self.remote_cache.is_none()
            && !self.remote_executor
            && self.remote_default_exec_properties.is_none()
    }
}

fn apply_bazel_remote_default_exec_properties(
    options: &mut RemoteEnabledExecutorOptions,
    overrides: &BazelRemoteEndpointOverrides,
) {
    if let Some(properties) = &overrides.remote_default_exec_properties
        && options.re_properties.properties.is_empty()
    {
        options.re_properties = properties.clone();
    }
}

fn cache_upload_behavior_for_bazel_remote_cache(
    overrides: &BazelRemoteEndpointOverrides,
    existing: CacheUploadBehavior,
) -> CacheUploadBehavior {
    match overrides.remote_cache {
        Some(true) => match existing {
            CacheUploadBehavior::Disabled => CacheUploadBehavior::Enabled { max_bytes: None },
            existing => existing,
        },
        Some(false) => CacheUploadBehavior::Disabled,
        None => existing,
    }
}

fn remote_executor_for_bazel_remote_executor(
    executor: &RemoteEnabledExecutor,
) -> RemoteEnabledExecutor {
    match executor {
        RemoteEnabledExecutor::Remote(remote) => RemoteEnabledExecutor::Remote(remote.clone()),
        RemoteEnabledExecutor::Hybrid {
            local,
            remote,
            level,
        } => RemoteEnabledExecutor::Hybrid {
            local: local.clone(),
            remote: remote.clone(),
            level: *level,
        },
        RemoteEnabledExecutor::Local(local) => RemoteEnabledExecutor::Hybrid {
            local: local.clone(),
            remote: RemoteExecutorOptions::default(),
            level: HybridExecutionLevel::Limited,
        },
    }
}

fn local_plus_remote_executor_for_bazel_remote_executor(
    local: &LocalExecutorOptions,
) -> RemoteEnabledExecutor {
    RemoteEnabledExecutor::Hybrid {
        local: local.clone(),
        remote: RemoteExecutorOptions::default(),
        level: HybridExecutionLevel::Limited,
    }
}

fn local_or_remote_executor_for_bazel_overrides(
    local: &LocalExecutorOptions,
    overrides: &BazelRemoteEndpointOverrides,
) -> RemoteEnabledExecutor {
    if overrides.remote_executor {
        local_plus_remote_executor_for_bazel_remote_executor(local)
    } else {
        RemoteEnabledExecutor::Local(local.clone())
    }
}

fn remote_enabled_executor_for_bazel_overrides(
    remote_options: &RemoteEnabledExecutorOptions,
    overrides: &BazelRemoteEndpointOverrides,
) -> RemoteEnabledExecutor {
    if overrides.remote_executor {
        remote_executor_for_bazel_remote_executor(&remote_options.executor)
    } else {
        remote_options.executor.clone()
    }
}

fn remote_enabled_executor_options_for_bazel_overrides(
    remote_options: &RemoteEnabledExecutorOptions,
    overrides: &BazelRemoteEndpointOverrides,
) -> RemoteEnabledExecutorOptions {
    let mut remote_options = remote_options.clone();
    remote_options.executor =
        remote_enabled_executor_for_bazel_overrides(&remote_options, overrides);
    remote_options
}

fn local_remote_enabled_executor_options_for_bazel_overrides(
    local: &LocalExecutorOptions,
    overrides: &BazelRemoteEndpointOverrides,
) -> RemoteEnabledExecutorOptions {
    RemoteEnabledExecutorOptions {
        executor: local_or_remote_executor_for_bazel_overrides(local, overrides),
        re_properties: overrides
            .remote_default_exec_properties
            .clone()
            .unwrap_or_default(),
        re_use_case: RemoteExecutorUseCase::bz_default(),
        re_action_key: None,
        cache_upload_behavior: cache_upload_behavior_for_bazel_remote_cache(
            overrides,
            CacheUploadBehavior::Disabled,
        ),
        remote_cache_enabled: overrides.remote_cache.unwrap_or(false),
        remote_dep_file_cache_enabled: false,
        dependencies: Vec::new(),
        gang_workers: Vec::new(),
        custom_image: None,
        meta_internal_extra_params: MetaInternalExtraParams::default_arc(),
        priority: None,
    }
}

fn executor_with_bazel_remote_endpoint_overrides(
    executor: &Executor,
    overrides: BazelRemoteEndpointOverrides,
) -> Executor {
    match executor {
        Executor::Local(local) => {
            if overrides.remote_cache.is_none() && !overrides.remote_executor {
                return Executor::Local(local.clone());
            }
            Executor::RemoteEnabled(local_remote_enabled_executor_options_for_bazel_overrides(
                local, &overrides,
            ))
        }
        Executor::RemoteEnabled(remote_options) => {
            let mut remote_options =
                remote_enabled_executor_options_for_bazel_overrides(remote_options, &overrides);
            if let Some(remote_cache) = overrides.remote_cache {
                remote_options.remote_cache_enabled = remote_cache;
            }
            remote_options.cache_upload_behavior = cache_upload_behavior_for_bazel_remote_cache(
                &overrides,
                remote_options.cache_upload_behavior,
            );
            apply_bazel_remote_default_exec_properties(&mut remote_options, &overrides);
            Executor::RemoteEnabled(remote_options)
        }
        Executor::None => Executor::None,
    }
}

pub fn executor_config_with_bazel_remote_startup_overrides(
    executor_config: &CommandExecutorConfig,
    startup_config: &RemoteExecutionStartupConfig,
) -> CommandExecutorConfig {
    executor_config_with_bazel_remote_endpoint_overrides(
        executor_config,
        BazelRemoteEndpointOverrides::from_startup_config(startup_config),
    )
}

fn executor_config_with_bazel_remote_endpoint_overrides(
    executor_config: &CommandExecutorConfig,
    overrides: BazelRemoteEndpointOverrides,
) -> CommandExecutorConfig {
    if overrides.is_empty() {
        return executor_config.clone();
    }

    CommandExecutorConfig {
        executor: executor_with_bazel_remote_endpoint_overrides(
            &executor_config.executor,
            overrides,
        ),
        options: executor_config.options,
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(bz_error::Error, Debug)]
#[buck2(input)]
enum ExecutorCompatibilityError {
    #[error("The desired execution strategy (`{0:?}`) is incompatible with the local executor")]
    LocalIncompatible(ExecutionStrategy),
    #[error(
        "The desired execution strategy (`{0:?}`) is incompatible with the executor config that was selected: {1:?}"
    )]
    SelectedConfig(ExecutionStrategy, CommandExecutorConfig),
}

#[async_trait]
impl HasCommandExecutor for CommandExecutorFactory {
    async fn get_command_executor(
        &self,
        artifact_fs: &ArtifactFs,
        executor_config: &CommandExecutorConfig,
    ) -> bz_error::Result<CommandExecutorResponse> {
        // 30GB is the max RE can currently support.
        const DEFAULT_RE_MAX_INPUT_FILE_BYTES: u64 = 30 * 1024 * 1024 * 1024;

        self.local_action_cache.load().await?;
        let executor_config =
            self.executor_config_with_bazel_remote_endpoint_overrides(executor_config);

        let local_executor_new =
            |options: &LocalExecutorOptions,
             local_action_cache_re_use_case: RemoteExecutorUseCase,
             local_action_cache_re_client: Option<ManagedRemoteExecutionClient>| {
                let worker_pool = if options.use_persistent_workers {
                    Some(self.worker_pool.dupe())
                } else {
                    None
                };
                LocalExecutor::new(
                    artifact_fs.clone(),
                    self.materializer.dupe(),
                    self.incremental_db_state.dupe(),
                    self.local_action_cache.dupe(),
                    local_action_cache_re_use_case,
                    local_action_cache_re_client,
                    self.local_executor_shared_state.clone(),
                    self.blocking_executor.dupe(),
                    self.host_sharing_broker.dupe(),
                    self.project_root.root().to_owned(),
                    self.forkserver.dupe(),
                    self.executor_global_knobs.dupe(),
                    worker_pool,
                    self.memory_tracker.dupe(),
                    self.daemon_id.dupe(),
                )
            };
        let local_action_cache_checker_new =
            |local_action_cache_re_use_case: RemoteExecutorUseCase,
             local_action_cache_re_client: Option<ManagedRemoteExecutionClient>|
             -> Arc<dyn PreparedCommandOptionalExecutor> {
                Arc::new(local_executor_new(
                    &LocalExecutorOptions::default(),
                    local_action_cache_re_use_case,
                    local_action_cache_re_client,
                ))
            };

        if !bz_core::is_open_source() && !cfg!(fbcode_build) {
            static WARN: OnceLock<()> = OnceLock::new();
            WARN.get_or_init(|| {
                tracing::warn!("Cargo build detected: disabling remote execution and caching!")
            });

            if self.strategy.ban_local() {
                return Err(ExecutorCompatibilityError::LocalIncompatible(self.strategy).into());
            }

            let local_executor = Arc::new(local_executor_new(
                &LocalExecutorOptions::default(),
                RemoteExecutorUseCase::bz_default(),
                None,
            ));
            return Ok(CommandExecutorResponse {
                executor: local_executor.dupe(),
                platform: Default::default(),
                action_cache_checker: local_executor,
                remote_dep_file_cache_checker: Arc::new(NoOpCommandOptionalExecutor {}),
                cache_uploader: Arc::new(NoOpCacheUploader {}),
                output_trees_download_config: self.output_trees_download_config.dupe(),
                remote_action_building_semaphore: self.remote_action_building_semaphore.dupe(),
            });
        }

        let remote_executor_new = |options: &RemoteExecutorOptions,
                                   re_use_case: &RemoteExecutorUseCase,
                                   re_action_key: &Option<String>,
                                   remote_cache_enabled: bool,
                                   dependencies: &[RemoteExecutorDependency],
                                   gang_workers: &[ReGangWorker],
                                   priority: Option<i32>| {
            ReExecutor {
                artifact_fs: artifact_fs.clone(),
                project_fs: self.project_root.clone(),
                materializer: self.materializer.dupe(),
                incremental_db_state: self.incremental_db_state.dupe(),
                re_client: self.get_prepared_re_client(*re_use_case),
                re_action_key: re_action_key.clone(),
                re_max_queue_time: options.re_max_queue_time,
                re_resource_units: options.re_resource_units,
                knobs: self.executor_global_knobs.dupe(),
                skip_cache_read: self.skip_cache_read || !remote_cache_enabled,
                skip_cache_write: self.skip_cache_write || !remote_cache_enabled,
                paranoid: self.paranoid.dupe(),
                materialize_failed_inputs: self.materialize_failed_inputs,
                materialize_failed_outputs: self.materialize_failed_outputs,
                dependencies: dependencies.to_vec(),
                gang_workers: gang_workers.to_vec(),
                deduplicate_get_digests_ttl_calls: self.deduplicate_get_digests_ttl_calls,
                output_trees_download_config: self.output_trees_download_config.dupe(),
                priority,
            }
        };

        let response = match &executor_config.executor {
            Executor::None => None,
            Executor::Local(local) => {
                if self.strategy.ban_local() {
                    None
                } else {
                    let local_executor = Arc::new(local_executor_new(
                        local,
                        RemoteExecutorUseCase::bz_default(),
                        None,
                    ));
                    Some(CommandExecutorResponse {
                        executor: local_executor.dupe(),
                        platform: Default::default(),
                        action_cache_checker: local_executor,
                        remote_dep_file_cache_checker: Arc::new(NoOpCommandOptionalExecutor {}),
                        cache_uploader: Arc::new(NoOpCacheUploader {}),
                        output_trees_download_config: self.output_trees_download_config.dupe(),
                        remote_action_building_semaphore: self
                            .remote_action_building_semaphore
                            .dupe(),
                    })
                }
            }
            Executor::RemoteEnabled(remote_options) => {
                // NOTE: While we now have a legit flag for this, we keep the env var. This has been used
                // in remediating prod incidents in the past, and this is the kind of thing that can easily
                // become tribal knowledge. Keeping this does not hurt us.
                let disable_caching =
                    bz_env!("BUCK2_TEST_DISABLE_CACHING", type=bool, applicability=testing)?
                        .unwrap_or(self.skip_cache_read);

                let disable_caching = disable_caching
                    || (!remote_options.remote_cache_enabled
                        && !remote_options.remote_dep_file_cache_enabled);

                // This is for test only as in real life, it would be silly to only use the remote dep file cache and not the regular cache
                // This will only do anything if cache is not disabled and remote dep file cache is enabled
                let only_remote_dep_file_cache = bz_env!(
                    "BUCK2_TEST_ONLY_REMOTE_DEP_FILE_CACHE",
                    bool,
                    applicability = testing
                )?;

                let cache_checker_new = || -> (Arc<dyn PreparedCommandOptionalExecutor>, Arc<dyn PreparedCommandOptionalExecutor>) {
                    if disable_caching {
                        return (
                            local_action_cache_checker_new(remote_options.re_use_case, None),
                            Arc::new(NoOpCommandOptionalExecutor {}) as _,
                        );
                    }
                    let local_action_cache_re_client = || {
                        remote_options
                            .remote_cache_enabled
                            .then(|| self.get_prepared_re_client(remote_options.re_use_case))
                    };

                    let remote_dep_file_cache_checker: Arc<dyn PreparedCommandOptionalExecutor> =
                        if remote_options.remote_dep_file_cache_enabled {
                            Arc::new(RemoteDepFileCacheChecker {
                                artifact_fs: artifact_fs.clone(),
                                materializer: self.materializer.dupe(),
                                incremental_db_state: self.incremental_db_state.dupe(),
                                re_client: self.get_prepared_re_client(remote_options.re_use_case),
                                re_action_key: remote_options.re_action_key.clone(),
                                upload_all_actions: self.upload_all_actions,
                                knobs: self.executor_global_knobs.dupe(),
                                paranoid: self.paranoid.dupe(),
                                deduplicate_get_digests_ttl_calls: self.deduplicate_get_digests_ttl_calls,
                                output_trees_download_config: self.output_trees_download_config.dupe(),
                                remote_metadata_semaphore: self.remote_metadata_semaphore.dupe(),
                            }) as _
                        } else {
                            Arc::new(NoOpCommandOptionalExecutor {}) as _
                        };

                    let remote_action_cache_checker: Arc<dyn PreparedCommandOptionalExecutor> =
                        if only_remote_dep_file_cache {
                            Arc::new(NoOpCommandOptionalExecutor {}) as _
                        } else {
                            Arc::new(ActionCacheChecker {
                                artifact_fs: artifact_fs.clone(),
                                materializer: self.materializer.dupe(),
                                incremental_db_state: self.incremental_db_state.dupe(),
                                re_client: self.get_prepared_re_client(remote_options.re_use_case),
                                re_action_key: remote_options.re_action_key.clone(),
                                upload_all_actions: self.upload_all_actions,
                                knobs: self.executor_global_knobs.dupe(),
                                paranoid: self.paranoid.dupe(),
                                deduplicate_get_digests_ttl_calls: self.deduplicate_get_digests_ttl_calls,
                                output_trees_download_config: self.output_trees_download_config.dupe(),
                                remote_action_cache_semaphore: self.remote_action_cache_semaphore.dupe(),
                                local_action_cache: self.local_action_cache.dupe(),
                            }) as _
                    };
                    let action_cache_checker: Arc<dyn PreparedCommandOptionalExecutor> =
                        if only_remote_dep_file_cache {
                            local_action_cache_checker_new(
                                remote_options.re_use_case,
                                local_action_cache_re_client(),
                            )
                        } else {
                            Arc::new(ChainedCommandOptionalExecutor {
                                first: local_action_cache_checker_new(
                                    remote_options.re_use_case,
                                    local_action_cache_re_client(),
                                ),
                                second: remote_action_cache_checker,
                            }) as _
                        };

                    (action_cache_checker, remote_dep_file_cache_checker)
                };

                let executor: Option<Arc<dyn PreparedCommandExecutor>> =
                    match &remote_options.executor {
                        RemoteEnabledExecutor::Local(local) if !self.strategy.ban_local() => {
                            let local: Arc<dyn PreparedCommandExecutor> =
                                Arc::new(local_executor_new(
                                    local,
                                    remote_options.re_use_case,
                                    remote_options.remote_cache_enabled.then(|| {
                                        self.get_prepared_re_client(remote_options.re_use_case)
                                    }),
                                ));
                            Some(local)
                        }
                        RemoteEnabledExecutor::Remote(remote) if !self.strategy.ban_remote() => {
                            Some(Arc::new(remote_executor_new(
                                remote,
                                &remote_options.re_use_case,
                                &remote_options.re_action_key,
                                remote_options.remote_cache_enabled,
                                &remote_options.dependencies,
                                &remote_options.gang_workers,
                                remote_options.priority,
                            )))
                        }
                        RemoteEnabledExecutor::Hybrid {
                            local,
                            remote,
                            level,
                        } if !self.strategy.ban_hybrid() => {
                            let re_max_input_files_bytes = remote
                                .re_max_input_files_bytes
                                .unwrap_or(DEFAULT_RE_MAX_INPUT_FILE_BYTES);
                            let local = local_executor_new(
                                local,
                                remote_options.re_use_case,
                                remote_options.remote_cache_enabled.then(|| {
                                    self.get_prepared_re_client(remote_options.re_use_case)
                                }),
                            );
                            let remote = remote_executor_new(
                                remote,
                                &remote_options.re_use_case,
                                &remote_options.re_action_key,
                                remote_options.remote_cache_enabled,
                                &remote_options.dependencies,
                                &remote_options.gang_workers,
                                remote_options.priority,
                            );
                            let executor_preference = self.strategy.hybrid_preference();
                            let low_pass_filter = self.low_pass_filter.dupe();
                            let fallback_tracker = self.fallback_tracker.dupe();

                            if self.paranoid.is_some() {
                                let executor_preference = executor_preference
                                    .and(ExecutorPreference::DefaultErasePreferences)?;

                                let (action_cache_checker, remote_dep_file_cache_checker) =
                                    cache_checker_new();
                                Some(Arc::new(HybridExecutor {
                                    local,
                                    remote: StackedExecutor {
                                        optional1: action_cache_checker,
                                        optional2: remote_dep_file_cache_checker,
                                        fallback: remote,
                                    },
                                    level: HybridExecutionLevel::Full {
                                        fallback_on_failure: true,
                                        low_pass_filter: false,
                                    },
                                    executor_preference,
                                    re_max_input_files_bytes,
                                    low_pass_filter,
                                    fallback_tracker,
                                }))
                            } else {
                                Some(Arc::new(HybridExecutor {
                                    local,
                                    remote,
                                    level: *level,
                                    executor_preference,
                                    re_max_input_files_bytes,
                                    low_pass_filter,
                                    fallback_tracker,
                                }))
                            }
                        }
                        _ => None,
                    };

                let (action_cache_checker, remote_dep_file_cache_checker) =
                    if self.paranoid.is_some() {
                        (
                            Arc::new(NoOpCommandOptionalExecutor {}) as _,
                            Arc::new(NoOpCommandOptionalExecutor {}) as _,
                        )
                    } else {
                        cache_checker_new()
                    };

                let cache_uploader = if force_cache_upload()? {
                    Arc::new(CacheUploader::new(
                        artifact_fs.clone(),
                        self.materializer.dupe(),
                        self.get_prepared_re_client(remote_options.re_use_case),
                        remote_options.re_properties.clone(),
                        None,
                        self.cache_upload_permission_checker.dupe(),
                        self.deduplicate_get_digests_ttl_calls,
                    )) as _
                } else if disable_caching {
                    Arc::new(NoOpCacheUploader {}) as _
                } else if let CacheUploadBehavior::Enabled { max_bytes } =
                    remote_options.cache_upload_behavior
                {
                    Arc::new(CacheUploader::new(
                        artifact_fs.clone(),
                        self.materializer.dupe(),
                        self.get_prepared_re_client(remote_options.re_use_case),
                        remote_options.re_properties.clone(),
                        max_bytes,
                        self.cache_upload_permission_checker.dupe(),
                        self.deduplicate_get_digests_ttl_calls,
                    )) as _
                } else {
                    Arc::new(NoOpCacheUploader {}) as _
                };

                executor.map(|executor| CommandExecutorResponse {
                    executor,
                    platform: remote_options.re_properties.to_re_platform(),
                    action_cache_checker,
                    remote_dep_file_cache_checker,
                    cache_uploader,
                    output_trees_download_config: self.output_trees_download_config.dupe(),
                    remote_action_building_semaphore: self.remote_action_building_semaphore.dupe(),
                })
            }
        };

        let response = response.ok_or_else(|| {
            ExecutorCompatibilityError::SelectedConfig(self.strategy, executor_config.clone())
        })?;
        Ok(response)
    }
}

trait ExecutionStrategyExt {
    fn ban_local(&self) -> bool;
    fn ban_remote(&self) -> bool;
    fn ban_hybrid(&self) -> bool;
    fn hybrid_preference(&self) -> ExecutorPreference;
}

impl ExecutionStrategyExt for ExecutionStrategy {
    fn ban_local(&self) -> bool {
        matches!(self, Self::RemoteOnly | Self::NoExecution)
    }

    fn ban_remote(&self) -> bool {
        matches!(self, Self::LocalOnly | Self::NoExecution)
    }

    fn ban_hybrid(&self) -> bool {
        matches!(self, Self::NoExecution)
    }

    fn hybrid_preference(&self) -> ExecutorPreference {
        match self {
            Self::HybridPreferLocal => ExecutorPreference::LocalPreferred,
            Self::HybridPreferRemote => ExecutorPreference::RemotePreferred,
            Self::LocalOnly => ExecutorPreference::LocalRequired,
            Self::RemoteOnly => ExecutorPreference::RemoteRequired,
            _ => ExecutorPreference::Default,
        }
    }
}

/// This is used when execution platforms are not configured.
pub fn get_default_executor_config(host_platform: HostPlatformOverride) -> CommandExecutorConfig {
    let executor = if bz_core::is_open_source() {
        Executor::Local(LocalExecutorOptions::default())
    } else {
        Executor::RemoteEnabled(RemoteEnabledExecutorOptions {
            executor: RemoteEnabledExecutor::Hybrid {
                local: LocalExecutorOptions::default(),
                remote: RemoteExecutorOptions::default(),
                level: HybridExecutionLevel::Limited,
            },
            re_properties: get_default_re_properties(host_platform),
            re_use_case: RemoteExecutorUseCase::bz_default(),
            re_action_key: None,
            cache_upload_behavior: CacheUploadBehavior::Disabled,
            remote_cache_enabled: true,
            remote_dep_file_cache_enabled: false,
            dependencies: vec![],
            gang_workers: vec![],
            custom_image: None,
            meta_internal_extra_params: MetaInternalExtraParams::default_arc(),
            priority: None,
        })
    };

    CommandExecutorConfig {
        executor,
        options: CommandGenerationOptions {
            path_separator: get_default_path_separator(host_platform),
            output_paths_behavior: Default::default(),
            use_bazel_protocol_remote_persistent_workers: false,
        },
    }
}

fn get_default_re_properties(host_platform: HostPlatformOverride) -> RePlatformFields {
    let linux = &[("platform", "linux-remote-execution")];
    let macos = &[("platform", "mac"), ("subplatform", "any")];
    let windows = &[("platform", "windows")];

    let props = match host_platform {
        HostPlatformOverride::Linux => linux.as_slice(),
        HostPlatformOverride::MacOs => macos.as_slice(),
        HostPlatformOverride::Windows => windows.as_slice(),
        HostPlatformOverride::DefaultPlatform => match std::env::consts::OS {
            "linux" => linux.as_slice(),
            "macos" => macos.as_slice(),
            "windows" => windows.as_slice(),
            v => unimplemented!("no support yet for operating system `{}`", v),
        },
    };

    RePlatformFields {
        properties: Arc::new(
            props
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        ),
    }
}

fn get_default_path_separator(host_platform: HostPlatformOverride) -> PathSeparatorKind {
    match host_platform {
        HostPlatformOverride::Linux => PathSeparatorKind::Unix,
        HostPlatformOverride::MacOs => PathSeparatorKind::Unix,
        HostPlatformOverride::Windows => PathSeparatorKind::Windows,
        HostPlatformOverride::DefaultPlatform => PathSeparatorKind::system_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bz_common::init::RemoteDefaultExecProperty;

    fn remote_cache_overrides() -> BazelRemoteEndpointOverrides {
        BazelRemoteEndpointOverrides::from_startup_config(&RemoteExecutionStartupConfig {
            remote_cache: Some("remote.buildbuddy.dev".to_owned()),
            ..Default::default()
        })
    }

    fn remote_executor_overrides() -> BazelRemoteEndpointOverrides {
        BazelRemoteEndpointOverrides::from_startup_config(&RemoteExecutionStartupConfig {
            remote_executor: Some("remote.buildbuddy.dev".to_owned()),
            ..Default::default()
        })
    }

    fn remote_default_exec_properties() -> Vec<RemoteDefaultExecProperty> {
        vec![RemoteDefaultExecProperty {
            name: "container-image".to_owned(),
            value: "docker://example/image@sha256:abc".to_owned(),
        }]
    }

    #[test]
    fn remote_cache_flag_wraps_local_executor_with_remote_cache() {
        let executor = executor_with_bazel_remote_endpoint_overrides(
            &Executor::Local(LocalExecutorOptions::default()),
            remote_cache_overrides(),
        );

        let Executor::RemoteEnabled(options) = executor else {
            panic!("expected remote-enabled executor");
        };
        assert!(matches!(options.executor, RemoteEnabledExecutor::Local(_)));
        assert!(options.remote_cache_enabled);
        assert!(matches!(
            options.cache_upload_behavior,
            CacheUploadBehavior::Enabled { max_bytes: None }
        ));
    }

    #[test]
    fn remote_executor_flag_wraps_local_executor_with_bazel_execution_policy() {
        let executor = executor_with_bazel_remote_endpoint_overrides(
            &Executor::Local(LocalExecutorOptions::default()),
            remote_executor_overrides(),
        );

        let Executor::RemoteEnabled(options) = executor else {
            panic!("expected remote-enabled executor");
        };
        assert!(matches!(
            options.executor,
            RemoteEnabledExecutor::Hybrid { .. }
        ));
        assert!(options.remote_cache_enabled);
    }

    #[test]
    fn remote_executor_flag_applies_default_exec_properties() {
        let executor = executor_with_bazel_remote_endpoint_overrides(
            &Executor::Local(LocalExecutorOptions::default()),
            BazelRemoteEndpointOverrides::from_startup_config(&RemoteExecutionStartupConfig {
                remote_executor: Some("remote.buildbuddy.dev".to_owned()),
                remote_default_exec_properties: Some(remote_default_exec_properties()),
                ..Default::default()
            }),
        );

        let Executor::RemoteEnabled(options) = executor else {
            panic!("expected remote-enabled executor");
        };
        assert_eq!(
            options
                .re_properties
                .properties
                .get("container-image")
                .map(|value| value.as_str()),
            Some("docker://example/image@sha256:abc")
        );
    }

    #[test]
    fn remote_default_exec_properties_do_not_wrap_local_without_remote_endpoint() {
        let executor = executor_with_bazel_remote_endpoint_overrides(
            &Executor::Local(LocalExecutorOptions::default()),
            BazelRemoteEndpointOverrides::from_startup_config(&RemoteExecutionStartupConfig {
                remote_default_exec_properties: Some(remote_default_exec_properties()),
                ..Default::default()
            }),
        );

        assert!(matches!(executor, Executor::Local(_)));
    }

    #[test]
    fn remote_default_exec_properties_do_not_replace_existing_properties() {
        let executor = executor_with_bazel_remote_endpoint_overrides(
            &Executor::RemoteEnabled(RemoteEnabledExecutorOptions {
                executor: RemoteEnabledExecutor::Remote(RemoteExecutorOptions::default()),
                re_properties: RePlatformFields {
                    properties: Arc::new(
                        [("container-image".to_owned(), "docker://existing".to_owned())]
                            .into_iter()
                            .collect(),
                    ),
                },
                re_use_case: RemoteExecutorUseCase::bz_default(),
                re_action_key: None,
                cache_upload_behavior: CacheUploadBehavior::Disabled,
                remote_cache_enabled: true,
                remote_dep_file_cache_enabled: false,
                dependencies: Vec::new(),
                gang_workers: Vec::new(),
                custom_image: None,
                meta_internal_extra_params: MetaInternalExtraParams::default_arc(),
                priority: None,
            }),
            BazelRemoteEndpointOverrides::from_startup_config(&RemoteExecutionStartupConfig {
                remote_default_exec_properties: Some(remote_default_exec_properties()),
                ..Default::default()
            }),
        );

        let Executor::RemoteEnabled(options) = executor else {
            panic!("expected remote-enabled executor");
        };
        assert_eq!(
            options
                .re_properties
                .properties
                .get("container-image")
                .map(|value| value.as_str()),
            Some("docker://existing")
        );
    }

    #[test]
    fn remote_executor_flag_preserves_local_policy_lane_for_hybrid_executor() {
        let configured_remote = RemoteExecutorOptions {
            re_max_input_files_bytes: Some(123),
            ..Default::default()
        };
        let executor = executor_with_bazel_remote_endpoint_overrides(
            &Executor::RemoteEnabled(RemoteEnabledExecutorOptions {
                executor: RemoteEnabledExecutor::Hybrid {
                    local: LocalExecutorOptions::default(),
                    remote: configured_remote.clone(),
                    level: HybridExecutionLevel::Limited,
                },
                re_properties: RePlatformFields::default(),
                re_use_case: RemoteExecutorUseCase::bz_default(),
                re_action_key: None,
                cache_upload_behavior: CacheUploadBehavior::Disabled,
                remote_cache_enabled: false,
                remote_dep_file_cache_enabled: false,
                dependencies: Vec::new(),
                gang_workers: Vec::new(),
                custom_image: None,
                meta_internal_extra_params: MetaInternalExtraParams::default_arc(),
                priority: None,
            }),
            remote_executor_overrides(),
        );

        let Executor::RemoteEnabled(options) = executor else {
            panic!("expected remote-enabled executor");
        };
        let RemoteEnabledExecutor::Hybrid { remote, .. } = options.executor else {
            panic!("expected hybrid executor");
        };
        assert_eq!(remote, configured_remote);
        assert!(options.remote_cache_enabled);
    }

    #[test]
    fn empty_remote_cache_flag_disables_cache_for_remote_executor() {
        let overrides =
            BazelRemoteEndpointOverrides::from_startup_config(&RemoteExecutionStartupConfig {
                remote_cache: Some(String::new()),
                remote_executor: Some("remote.buildbuddy.dev".to_owned()),
                ..Default::default()
            });

        let executor = executor_with_bazel_remote_endpoint_overrides(
            &Executor::Local(LocalExecutorOptions::default()),
            overrides,
        );

        let Executor::RemoteEnabled(options) = executor else {
            panic!("expected remote-enabled executor");
        };
        assert!(matches!(
            options.executor,
            RemoteEnabledExecutor::Hybrid { .. }
        ));
        assert!(!options.remote_cache_enabled);
        assert!(matches!(
            options.cache_upload_behavior,
            CacheUploadBehavior::Disabled
        ));
    }
}
