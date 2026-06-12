/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use bz_action_metadata_proto::REMOTE_DEP_FILE_KEY;
use bz_action_metadata_proto::RemoteDepFile;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::soft_error;
use bz_error::internal_error;
use bz_execute::execute::action_digest::ActionDigest;
use bz_execute::execute::action_digest::ActionDigestKind;
use bz_execute::execute::dep_file_digest::DepFileDigest;
use bz_execute::execute::executor_stage_async;
use bz_execute::execute::kind::CommandExecutionKind;
use bz_execute::execute::kind::RemoteCommandExecutionDetails;
use bz_execute::execute::known_missing::KnownMissingRemoteCasTracker;
use bz_execute::execute::manager::CommandExecutionManager;
use bz_execute::execute::manager::CommandExecutionManagerExt;
use bz_execute::execute::prepared::PreparedCommand;
use bz_execute::execute::prepared::PreparedCommandOptionalExecutor;
use bz_execute::execute::result::CommandExecutionResult;
use bz_execute::knobs::ExecutorGlobalKnobs;
use bz_execute::materialize::materializer::Materializer;
use bz_execute::materialize::materializer::RemoteActionCacheOrigin;
use bz_execute::re::action_identity::ReActionIdentity;
use bz_execute::re::manager::ManagedRemoteExecutionClient;
use bz_execute::re::output_trees_download_config::OutputTreesDownloadConfig;
use bz_execute::re::remote_action_result::ActionCacheResult;
use bz_hash::StdBuckHashMap;
use bz_util::time_span::TimeSpan;
use chrono::Duration as ChronoDuration;
use chrono::Utc;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use once_cell::sync::Lazy;
use prost::Message;
use remote_execution::ActionResultResponse;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;

use crate::executors::local_action_cache::LocalActionCache;
use crate::executors::local_action_cache::local_action_cache_outputs_fingerprint;
use crate::incremental_actions_helper::save_content_based_incremental_state;
use crate::re::download::DownloadResult;
use crate::re::download::download_action_results;
use crate::re::download::missing_mandatory_output;
use crate::re::paranoid_download::ParanoidDownloader;
use crate::sqlite::incremental_state_db::IncrementalDbState;

pub struct ActionCacheChecker {
    pub artifact_fs: ArtifactFs,
    pub materializer: Arc<dyn Materializer>,
    pub incremental_db_state: Arc<IncrementalDbState>,
    pub re_client: ManagedRemoteExecutionClient,
    pub re_action_key: Option<String>,
    pub upload_all_actions: bool,
    pub knobs: ExecutorGlobalKnobs,
    pub paranoid: Option<ParanoidDownloader>,
    pub deduplicate_get_digests_ttl_calls: bool,
    pub output_trees_download_config: OutputTreesDownloadConfig,
    pub remote_action_cache_semaphore: Arc<Semaphore>,
    pub local_action_cache: Arc<LocalActionCache>,
    pub known_missing_remote_cas: Arc<KnownMissingRemoteCasTracker>,
}

enum CacheType {
    ActionCache,
    RemoteDepFileCache(DepFileDigest),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ActionCacheQueryKind {
    ActionCache,
    RemoteDepFileCache,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ActionCacheQueryKey {
    kind: ActionCacheQueryKind,
    digest: String,
}

impl ActionCacheQueryKey {
    fn new(cache_type: &CacheType, digest: &ActionDigest) -> Self {
        let kind = match cache_type {
            CacheType::ActionCache => ActionCacheQueryKind::ActionCache,
            CacheType::RemoteDepFileCache(_) => ActionCacheQueryKind::RemoteDepFileCache,
        };
        Self {
            kind,
            digest: digest.to_string(),
        }
    }
}

type ActionCacheQueryResult = bz_error::Result<Option<ActionResultResponse>>;

enum ActionCacheQueryClaim {
    Owner(ActionCacheQueryOwner),
    Wait(oneshot::Receiver<ActionCacheQueryResult>),
}

struct ActionCacheQueryOwner {
    key: Option<ActionCacheQueryKey>,
}

impl ActionCacheQueryOwner {
    fn complete(mut self, result: &ActionCacheQueryResult) {
        if let Some(key) = self.key.take() {
            ActionCacheQueryDeduper::complete(key, result);
        }
    }
}

impl Drop for ActionCacheQueryOwner {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let error = Err(internal_error!(
                "deduplicated remote action cache query was cancelled"
            ));
            ActionCacheQueryDeduper::complete(key, &error);
        }
    }
}

#[derive(Default)]
struct ActionCacheQueryDeduper {
    in_flight: StdBuckHashMap<ActionCacheQueryKey, Vec<oneshot::Sender<ActionCacheQueryResult>>>,
}

static ACTION_CACHE_QUERY_DEDUPER: Lazy<Mutex<ActionCacheQueryDeduper>> =
    Lazy::new(|| Mutex::new(ActionCacheQueryDeduper::default()));

impl ActionCacheQueryDeduper {
    fn claim(key: ActionCacheQueryKey) -> ActionCacheQueryClaim {
        let mut deduper = ACTION_CACHE_QUERY_DEDUPER.lock().expect("Poisoned lock");
        if let Some(waiters) = deduper.in_flight.get_mut(&key) {
            let (sender, receiver) = oneshot::channel();
            waiters.push(sender);
            ActionCacheQueryClaim::Wait(receiver)
        } else {
            deduper.in_flight.insert(key.clone(), Vec::new());
            ActionCacheQueryClaim::Owner(ActionCacheQueryOwner { key: Some(key) })
        }
    }

    fn complete(key: ActionCacheQueryKey, result: &ActionCacheQueryResult) {
        let waiters = ACTION_CACHE_QUERY_DEDUPER
            .lock()
            .expect("Poisoned lock")
            .in_flight
            .remove(&key);

        if let Some(waiters) = waiters {
            for waiter in waiters {
                let _ = waiter.send(result.clone());
            }
        }
    }
}

impl CacheType {
    fn to_proto(&self) -> bz_data::CacheType {
        match self {
            CacheType::ActionCache => bz_data::CacheType::ActionCache,
            CacheType::RemoteDepFileCache(_) => bz_data::CacheType::RemoteDepFileCache,
        }
    }
}

async fn query_action_cache_and_download_result(
    // Differentiate between regular action cache look up and remote dep file based look up
    cache_type: CacheType,
    artifact_fs: &ArtifactFs,
    materializer: &Arc<dyn Materializer>,
    incremental_db_state: &Arc<IncrementalDbState>,
    re_client: &ManagedRemoteExecutionClient,
    re_action_key: &Option<String>,
    paranoid: &Option<ParanoidDownloader>,
    action_digest: &ActionDigest,
    command: &PreparedCommand<'_, '_>,
    manager: CommandExecutionManager,
    cancellations: &CancellationContext,
    upload_all_actions: bool,
    log_action_keys: bool,
    details: RemoteCommandExecutionDetails,
    deduplicate_get_digests_ttl_calls: bool,
    output_trees_download_config: &OutputTreesDownloadConfig,
    remote_cache_query_semaphore: &Arc<Semaphore>,
    local_action_cache: Option<&Arc<LocalActionCache>>,
    known_missing_remote_cas: &Arc<KnownMissingRemoteCasTracker>,
) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
    let request = command.request;
    let action_blobs = &command.prepared_action.action_and_blobs.blobs;
    let digest_config = command.digest_config;

    let digest = match &cache_type {
        CacheType::RemoteDepFileCache(key) => key.dupe().coerce::<ActionDigestKind>(),
        CacheType::ActionCache => action_digest.dupe(),
    };
    let identity = ReActionIdentity::new(
        command.target,
        re_action_key.as_deref(),
        command.request.paths(),
    );
    let execution_kind = command_execution_kind_for_cache_type(&cache_type, details.clone());
    let action_cache_query_key = ActionCacheQueryKey::new(&cache_type, &digest);

    let action_cache_response = match ActionCacheQueryDeduper::claim(action_cache_query_key) {
        ActionCacheQueryClaim::Owner(owner) => {
            let response = match remote_cache_query_semaphore.acquire().await {
                Ok(_permit) => {
                    executor_stage_async(
                        bz_data::CacheQuery {
                            action_digest: digest.to_string(),
                            cache_type: cache_type.to_proto().into(),
                        },
                        re_client.action_cache(
                            digest.dupe(),
                            &command.prepared_action.platform,
                            Some(&identity),
                        ),
                    )
                    .await
                }
                Err(_) => Err(bz_error::bz_error!(
                    bz_error::ErrorTag::InternalError,
                    "remote cache query semaphore was closed"
                )),
            };
            owner.complete(&response);
            response
        }
        ActionCacheQueryClaim::Wait(receiver) => receiver.await.unwrap_or_else(|_| {
            Err(internal_error!(
                "deduplicated remote action cache query was cancelled"
            ))
        }),
    };
    let manager = manager.with_execution_kind(execution_kind);

    if upload_all_actions {
        if let Err(e) = re_client
            .upload(
                artifact_fs.fs(),
                materializer,
                action_blobs,
                ProjectRelativePath::empty(),
                request.paths().input_directory(),
                Some(request.paths()),
                Some(&identity),
                digest_config,
                deduplicate_get_digests_ttl_calls,
                false,
            )
            .await
        {
            return ControlFlow::Break(manager.error("upload", e));
        }
    }

    let response = match action_cache_response {
        Err(e) => {
            return ControlFlow::Break(manager.error("remote_action_cache", e));
        }
        Ok(Some(response)) => response,
        Ok(None) => return ControlFlow::Continue(manager),
    };

    let response = ActionCacheResult(response, cache_type.to_proto());
    let action_exit_code = response.0.action_result.exit_code;
    if action_exit_code != 0 {
        tracing::debug!(
            "Cached action result for `{}` had non-zero exit code {}; treating it as a cache miss",
            digest,
            action_exit_code,
        );
        return ControlFlow::Continue(manager);
    }
    if let Some(missing_output) =
        missing_mandatory_output(request.paths(), request.working_directory(), &response)
    {
        tracing::debug!(
            "Cached action result for `{}` did not contain mandatory output `{}`; treating it as a cache miss",
            digest,
            missing_output,
        );
        return ControlFlow::Continue(manager);
    }

    let dep_file_metadata: Option<RemoteDepFile> = match &cache_type {
        CacheType::ActionCache => None,
        CacheType::RemoteDepFileCache(_) => {
            let metadata = response
                .0
                .action_result
                .execution_metadata
                .auxiliary_metadata
                .iter()
                .find(|k| k.type_url == REMOTE_DEP_FILE_KEY);

            if metadata.is_none() {
                // No entry found
                return ControlFlow::Continue(manager);
            }
            let dep_file_entry = match RemoteDepFile::decode(metadata.unwrap().value.as_slice()) {
                Ok(entry) => entry,
                Err(e) => {
                    return ControlFlow::Break(manager.error("remote_dep_file", e));
                }
            };
            Some(dep_file_entry)
        }
    };

    let res = download_action_results(
        request,
        TimeSpan::start_now(),
        materializer.as_ref(),
        re_client,
        digest_config,
        manager,
        &identity,
        bz_data::CacheHit {
            action_digest: digest.to_string(),
            action_key: if log_action_keys {
                Some(identity.action_key.clone())
            } else {
                None
            },
            cache_type: cache_type.to_proto().into(),
        }
        .into(),
        request.paths(),
        request.outputs(),
        details,
        &response,
        true,
        paranoid.as_ref(),
        cancellations,
        action_exit_code,
        artifact_fs,
        false,
        false,
        None,
        output_trees_download_config,
        Some(known_missing_remote_cas.as_ref()),
    )
    .await;

    let mut res = match res {
        DownloadResult::Result(res) => res,
        DownloadResult::CacheMiss(manager) => return ControlFlow::Continue(manager),
    };
    let remote_cache_origin = RemoteActionCacheOrigin::new(
        action_digest.dupe(),
        Utc::now(),
        ChronoDuration::seconds(response.0.ttl),
    );
    res.remote_cache_origin = Some(remote_cache_origin.clone());
    match &cache_type {
        CacheType::RemoteDepFileCache(key) => {
            tracing::trace!(
                "Found an action result for remote dep file key`{}`, moving onto dep file verification",
                key,
            );
            res.dep_file_metadata = dep_file_metadata;
        }
        CacheType::ActionCache => {
            tracing::info!(
                "Action result is cached, skipping execution of:\n```\n$ {}\n```\n for action `{}`",
                command.request.all_args_str(),
                action_digest,
            );
            res.action_result = Some(response.0.action_result);
            if let Some(local_action_cache) = local_action_cache {
                let outputs_fingerprint =
                    match local_action_cache_outputs_fingerprint(artifact_fs, &res.outputs) {
                        Ok(fingerprint) => fingerprint,
                        Err(e) => {
                            let _unused = soft_error!("local_action_cache_fingerprint_failed", e);
                            return ControlFlow::Break(res);
                        }
                    };
                if let Err(e) = local_action_cache.insert_remote(
                    action_digest,
                    outputs_fingerprint,
                    res.outputs.values().cloned().collect::<Vec<_>>().into(),
                    remote_cache_origin,
                ) {
                    let _unused = soft_error!("local_action_cache_insert_failed", e);
                }
            }
        }
    }

    if let Some(run_action_key) = request.run_action_key()
        && !request.outputs_cleanup
    {
        save_content_based_incremental_state(
            run_action_key.clone(),
            incremental_db_state,
            artifact_fs,
            &res,
        );
    }

    ControlFlow::Break(res)
}

#[async_trait]
impl PreparedCommandOptionalExecutor for ActionCacheChecker {
    async fn maybe_execute(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        let action_digest = &command.prepared_action.action_and_blobs.action;
        let details = RemoteCommandExecutionDetails::new(
            action_digest.dupe(),
            *command.request.remote_dep_file_key(),
            self.re_client.get_session_id().await.ok(),
            self.re_client.use_case,
            &command.prepared_action.platform,
            false,
        );
        let cache_type = CacheType::ActionCache;
        query_action_cache_and_download_result(
            cache_type,
            &self.artifact_fs,
            &self.materializer,
            &self.incremental_db_state,
            &self.re_client,
            &self.re_action_key,
            &self.paranoid,
            action_digest,
            command,
            manager,
            cancellations,
            self.upload_all_actions,
            self.knobs.log_action_keys,
            details,
            self.deduplicate_get_digests_ttl_calls,
            &self.output_trees_download_config,
            &self.remote_action_cache_semaphore,
            Some(&self.local_action_cache),
            &self.known_missing_remote_cas,
        )
        .await
    }
}

pub struct RemoteDepFileCacheChecker {
    pub artifact_fs: ArtifactFs,
    pub materializer: Arc<dyn Materializer>,
    pub incremental_db_state: Arc<IncrementalDbState>,
    pub re_client: ManagedRemoteExecutionClient,
    pub re_action_key: Option<String>,
    pub upload_all_actions: bool,
    pub knobs: ExecutorGlobalKnobs,
    pub paranoid: Option<ParanoidDownloader>,
    pub deduplicate_get_digests_ttl_calls: bool,
    pub output_trees_download_config: OutputTreesDownloadConfig,
    pub remote_metadata_semaphore: Arc<Semaphore>,
    pub known_missing_remote_cas: Arc<KnownMissingRemoteCasTracker>,
}

#[async_trait]
impl PreparedCommandOptionalExecutor for RemoteDepFileCacheChecker {
    async fn maybe_execute(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        // If the remote dep file key is not set, just fallback to the next execution method
        let remote_dep_file_key = match command.request.remote_dep_file_key() {
            None => {
                return ControlFlow::Continue(manager);
            }
            Some(key) => key.dupe(),
        };

        let cache_type = CacheType::RemoteDepFileCache(remote_dep_file_key);
        let action_digest = remote_dep_file_key.dupe().coerce::<ActionDigestKind>();
        let details = RemoteCommandExecutionDetails::new(
            action_digest.dupe(),
            Some(remote_dep_file_key.dupe()),
            self.re_client.get_session_id().await.ok(),
            self.re_client.use_case,
            &command.prepared_action.platform,
            false,
        );
        query_action_cache_and_download_result(
            cache_type,
            &self.artifact_fs,
            &self.materializer,
            &self.incremental_db_state,
            &self.re_client,
            &self.re_action_key,
            &self.paranoid,
            &action_digest,
            command,
            manager,
            cancellations,
            self.upload_all_actions,
            self.knobs.log_action_keys,
            details,
            self.deduplicate_get_digests_ttl_calls,
            &self.output_trees_download_config,
            &self.remote_metadata_semaphore,
            None,
            &self.known_missing_remote_cas,
        )
        .await
    }
}

fn command_execution_kind_for_cache_type(
    cache_type: &CacheType,
    details: RemoteCommandExecutionDetails,
) -> CommandExecutionKind {
    match cache_type {
        CacheType::ActionCache => CommandExecutionKind::ActionCache { details },
        CacheType::RemoteDepFileCache(_) => CommandExecutionKind::RemoteDepFileCache { details },
    }
}
