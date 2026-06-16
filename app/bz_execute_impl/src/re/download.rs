/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashSet;
use std::convert::Infallible;
use std::ops::ControlFlow;
use std::ops::FromResidual;
use std::path::Path;
use std::sync::Arc;

use bz_common::file_ops::metadata::FileDigest;
use bz_common::file_ops::metadata::FileMetadata;
use bz_common::file_ops::metadata::Symlink;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::buck_out_path::BuildArtifactPath;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_directory::directory::directory::Directory;
use bz_directory::directory::directory_iterator::DirectoryIterator;
use bz_directory::directory::entry::DirectoryEntry;
use bz_directory::directory::walk::unordered_entry_walk;
use bz_error::BuckErrorContext;
use bz_events::dispatch::console_message;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest::CasDigestFromReExt;
use bz_execute::digest::CasDigestToReExt;
use bz_execute::digest_config::DigestConfig;
use bz_execute::directory::ActionDirectoryBuilder;
use bz_execute::directory::ActionDirectoryMember;
use bz_execute::directory::extract_artifact_value;
use bz_execute::directory::re_tree_to_directory;
use bz_execute::execute::action_digest::TrackedActionDigest;
use bz_execute::execute::executor_stage_async;
use bz_execute::execute::kind::RemoteCommandExecutionDetails;
use bz_execute::execute::known_missing::KnownMissingRemoteCasTracker;
use bz_execute::execute::manager::CommandExecutionManager;
use bz_execute::execute::manager::CommandExecutionManagerExt;
use bz_execute::execute::manager::CommandExecutionManagerWithClaim;
use bz_execute::execute::output::CommandStdStreams;
use bz_execute::execute::request::CommandExecutionOutput;
use bz_execute::execute::request::CommandExecutionOutputRef;
use bz_execute::execute::request::CommandExecutionPaths;
use bz_execute::execute::request::CommandExecutionRequest;
use bz_execute::execute::result::CommandExecutionErrorType;
use bz_execute::execute::result::CommandExecutionMetadata;
use bz_execute::execute::result::CommandExecutionResult;
use bz_execute::materialize::materializer::CasDownloadInfo;
use bz_execute::materialize::materializer::DeclareArtifactPayload;
use bz_execute::materialize::materializer::LostRemoteCasArtifact;
use bz_execute::materialize::materializer::LostRemoteCasArtifacts;
use bz_execute::materialize::materializer::Materializer;
use bz_execute::materialize::materializer::RemoteActionCacheOrigin;
use bz_execute::re::action_identity::ReActionIdentity;
use bz_execute::re::error::RemoteExecutionError;
use bz_execute::re::manager::ManagedRemoteExecutionClient;
use bz_execute::re::output_trees_download_config::OutputTreesDownloadConfig;
use bz_execute::re::remote_action_result::RemoteActionResult;
use bz_fs::paths::RelativePathBuf;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use bz_hash::BuckIndexMap;
use bz_hash::StdBuckHashSet;
use bz_util::time_span::TimeSpan;
use bz_util::time_span::TimeSpanBuilder;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::FutureExt;
use futures::future;
use remote_execution as RE;

use crate::executors::local::materialize_input_path_aliases;
use crate::executors::local::materialize_inputs;
use crate::re::paranoid_download::ParanoidDownloader;
use crate::storage_resource_exhausted::is_storage_resource_exhausted;

pub fn missing_mandatory_output(
    paths: &CommandExecutionPaths,
    working_directory: &ProjectRelativePath,
    output_spec: &dyn RemoteActionResult,
) -> Option<String> {
    let mut actual_outputs = HashSet::new();
    for output in output_spec.output_files() {
        actual_outputs.insert(output.name.as_str());
    }
    for output in output_spec.output_directories() {
        actual_outputs.insert(output.path.as_str());
    }
    for output in output_spec.output_symlinks() {
        actual_outputs.insert(output.name.as_str());
    }

    let output_paths = match paths.output_paths_relative_to_working_directory(working_directory) {
        Ok(output_paths) => output_paths,
        Err(e) => return Some(e.to_string()),
    };

    output_paths
        .iter()
        .map(|(path, _)| path.as_str())
        .find(|path| !actual_outputs.contains(*path))
        .map(str::to_owned)
}

pub enum RemoteCacheDigestPresence {
    Present,
    Missing(Vec<TrackedFileDigest>),
}

pub async fn remote_artifact_values_presence(
    re_client: &ManagedRemoteExecutionClient,
    outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
) -> bz_error::Result<RemoteCacheDigestPresence> {
    remote_cache_digests_presence(re_client, artifact_value_file_digests(outputs)).await
}

fn artifact_value_file_digests(
    outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
) -> Vec<TrackedFileDigest> {
    let mut digests = Vec::new();
    for value in outputs.values() {
        digests.extend(artifact_file_digests(value));
    }
    digests
}

fn artifact_file_digests(value: &ArtifactValue) -> impl Iterator<Item = TrackedFileDigest> + '_ {
    unordered_entry_walk(value.entry().as_ref().map_dir(Directory::as_ref))
        .without_paths()
        .filter_map(|entry| match entry {
            DirectoryEntry::Leaf(ActionDirectoryMember::File(file)) => Some(file.digest.dupe()),
            _ => None,
        })
}

async fn remote_cache_digests_presence(
    re_client: &ManagedRemoteExecutionClient,
    digests: Vec<TrackedFileDigest>,
) -> bz_error::Result<RemoteCacheDigestPresence> {
    let mut seen = StdBuckHashSet::default();
    let mut unique = Vec::new();
    for digest in digests {
        if seen.insert(digest.dupe()) {
            unique.push(digest);
        }
    }

    if unique.is_empty() {
        return Ok(RemoteCacheDigestPresence::Present);
    }

    let requested: StdBuckHashSet<_> = unique.iter().map(|digest| digest.to_re()).collect();
    let expirations = match re_client
        .get_digest_expirations(unique.iter().map(|digest| digest.to_re()).collect())
        .await
    {
        Ok(expirations) => expirations,
        Err(e) if is_remote_execution_not_found(&e) => {
            return Ok(RemoteCacheDigestPresence::Missing(unique));
        }
        Err(e) => return Err(e),
    };

    let now = Utc::now();
    let mut present = StdBuckHashSet::default();
    for (digest, expires) in expirations {
        if expires > now {
            present.insert(digest);
        }
    }

    let missing = unique
        .into_iter()
        .filter(|digest| !present.contains(&digest.to_re()))
        .collect::<Vec<_>>();
    if missing.is_empty() && present.len() == requested.len() {
        Ok(RemoteCacheDigestPresence::Present)
    } else {
        Ok(RemoteCacheDigestPresence::Missing(missing))
    }
}

fn is_remote_execution_not_found(error: &bz_error::Error) -> bool {
    error
        .find_typed_context::<RemoteExecutionError>()
        .is_some_and(|re_client_error| re_client_error.code == RE::TCode::NOT_FOUND)
}

pub async fn download_action_results<'a>(
    request: &CommandExecutionRequest,
    execution_time: TimeSpanBuilder,
    materializer: &dyn Materializer,
    re_client: &ManagedRemoteExecutionClient,
    digest_config: DigestConfig,
    manager: CommandExecutionManager,
    identity: &ReActionIdentity<'_>,
    stage: bz_data::executor_stage_start::Stage,
    paths: &CommandExecutionPaths,
    requested_outputs: impl IntoIterator<Item = CommandExecutionOutputRef<'a>>,
    details: RemoteCommandExecutionDetails,
    response: &dyn RemoteActionResult,
    missing_cas_is_cache_miss: bool,
    paranoid: Option<&ParanoidDownloader>,
    cancellations: &CancellationContext,
    action_exit_code: i32,
    artifact_fs: &ArtifactFs,
    materialize_failed_re_action_inputs: bool,
    materialize_failed_re_action_outputs: bool,
    additional_message: Option<String>,
    output_trees_download_config: &OutputTreesDownloadConfig,
    known_missing_remote_cas: Option<&KnownMissingRemoteCasTracker>,
) -> DownloadResult {
    let std_streams = response.std_streams(re_client, digest_config);
    let std_streams = async {
        if request.prefetch_lossy_stderr() {
            std_streams.prefetch_lossy_stderr().await
        } else {
            std_streams
        }
    };

    if action_exit_code != 0 && manager.inner.intend_to_fallback_on_failure {
        // Do not attempt to download outputs in this case so
        // as to avoid cancelling in-flight local execution:
        // either local already finished and the outputs are
        // already there, or local hasn't finished, and then
        // it will produce outputs.

        let std_streams = std_streams.await;
        return DownloadResult::Result(manager.failure(
            response.execution_kind(details),
            BuckIndexMap::default(),
            CommandStdStreams::Remote(std_streams),
            Some(action_exit_code),
            CommandExecutionMetadata::from_re_timing(response.timing(), TimeSpan::empty_now()),
            additional_message,
        ));
    }
    let downloader = CasDownloader {
        materializer,
        re_client,
        digest_config,
        paranoid,
        output_trees_download_config,
    };

    let download = downloader.download(
        artifact_fs,
        manager,
        identity,
        stage,
        paths,
        request.working_directory(),
        requested_outputs,
        response,
        &details,
        missing_cas_is_cache_miss,
        known_missing_remote_cas,
        cancellations,
    );

    let (download, std_streams) = future::join(download, std_streams).await;
    let (manager, outputs) = download?;

    let res = match action_exit_code {
        0 => manager.success(
            response.execution_kind(details),
            outputs,
            CommandStdStreams::Remote(std_streams),
            CommandExecutionMetadata::from_re_timing(response.timing(), execution_time.end_now()),
        ),
        e => {
            let materialized_inputs = if materialize_failed_re_action_inputs {
                executor_stage_async(
                    bz_data::ReStage {
                        stage: Some(bz_data::MaterializeFailedInputs {}.into()),
                    },
                    async move {
                        match materialize_inputs(artifact_fs, materializer, request, digest_config)
                            .await
                        {
                            Ok(materialized_paths) => {
                                if let Err(e) =
                                    materialize_input_path_aliases(artifact_fs, &materialized_paths)
                                {
                                    console_message(format!(
                                        "Failed to materialize input aliases for failed action: {e}"
                                    ));
                                    None
                                } else {
                                    Some(materialized_paths.paths.clone())
                                }
                            }
                            Err(e) => {
                                // TODO(minglunli): Properly handle this and the error below and add a test for it.
                                console_message(format!(
                                    "Failed to materialize inputs for failed action: {e}"
                                ));
                                None
                            }
                        }
                    },
                )
                .await
            } else {
                None
            };

            let materialized_outputs = match materialize_failed_build_outputs(
                artifact_fs,
                materializer,
                request,
                &outputs,
                materialize_failed_re_action_outputs,
            )
            .await
            {
                Ok(materialized_paths) => Some(materialized_paths.clone()),
                Err(e) => {
                    console_message(format!(
                        "Failed to materialize outputs for failed action: {e}"
                    ));
                    None
                }
            };

            manager.failure(
                response.execution_kind_for_failed_actions(
                    details,
                    materialized_inputs,
                    materialized_outputs,
                ),
                outputs,
                CommandStdStreams::Remote(std_streams),
                Some(e),
                CommandExecutionMetadata::from_re_timing(
                    response.timing(),
                    execution_time.end_now(),
                ),
                additional_message,
            )
        }
    };

    DownloadResult::Result(res)
}

async fn materialize_failed_build_outputs(
    artifact_fs: &ArtifactFs,
    materializer: &dyn Materializer,
    request: &CommandExecutionRequest,
    available_outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
    materialize_failed_re_action_outputs: bool,
) -> bz_error::Result<Vec<ProjectRelativePathBuf>> {
    let mut paths = vec![];
    if !materialize_failed_re_action_outputs && request.outputs_for_error_handler().is_empty() {
        // Nothing to materialize
        return Ok(paths);
    }

    let materialize_select_outputs: StdBuckHashSet<&BuildArtifactPath> =
        request.outputs_for_error_handler().iter().collect();

    for output in request.outputs() {
        if let CommandExecutionOutputRef::BuildArtifact { path, .. } = output {
            // If materialize_failed_re_action_outputs is not set and materialize_select_outputs is not empty,
            // only materialize outputs in the set. Otherwise, materialize all outputs.
            if !materialize_failed_re_action_outputs
                && !materialize_select_outputs.is_empty()
                && !materialize_select_outputs.contains(path)
            {
                continue;
            }

            let content_hash = available_outputs.get(&output.cloned()).and_then(|value| {
                if path.is_content_based_path() {
                    Some(value.content_based_path_hash())
                } else {
                    None
                }
            });

            paths.push(artifact_fs.resolve_build(path, content_hash.as_ref())?);
        }
    }

    materializer.ensure_materialized(paths.clone()).await?;

    Ok(paths)
}

pub struct CasDownloader<'a> {
    pub materializer: &'a dyn Materializer,
    pub re_client: &'a ManagedRemoteExecutionClient,
    pub digest_config: DigestConfig,
    pub paranoid: Option<&'a ParanoidDownloader>,
    pub output_trees_download_config: &'a OutputTreesDownloadConfig,
}

impl CasDownloader<'_> {
    async fn download<'a>(
        &self,
        artifact_fs: &ArtifactFs,
        manager: CommandExecutionManager,
        identity: &ReActionIdentity<'_>,
        stage: bz_data::executor_stage_start::Stage,
        paths: &CommandExecutionPaths,
        working_directory: &ProjectRelativePath,
        requested_outputs: impl IntoIterator<Item = CommandExecutionOutputRef<'a>>,
        output_spec: &dyn RemoteActionResult,
        details: &RemoteCommandExecutionDetails,
        missing_cas_is_cache_miss: bool,
        known_missing_remote_cas: Option<&KnownMissingRemoteCasTracker>,
        cancellations: &CancellationContext,
    ) -> ControlFlow<
        DownloadResult,
        (
            CommandExecutionManagerWithClaim,
            BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
        ),
    > {
        let manager = manager.with_execution_kind(output_spec.execution_kind(details.clone()));
        executor_stage_async(stage, async {
            let artifacts = self
                .extract_artifacts(
                    artifact_fs,
                    identity,
                    paths,
                    working_directory,
                    requested_outputs,
                    output_spec,
                )
                .await;

            let artifacts =
                match artifacts {
                    Ok(artifacts) => artifacts,
                    Err(e) => {
                        let error: bz_error::Error =
                            e.context(format!("action_digest={}", details.action_digest));
                        if missing_cas_is_cache_miss
                            && is_remote_execution_not_found(&error)
                        {
                            tracing::debug!(
                                "Remote result for `{}` referenced missing CAS metadata; treating it as a miss",
                                details.action_digest,
                            );
                            return ControlFlow::Break(DownloadResult::CacheMiss {
                                manager,
                                missing: None,
                            });
                        }
                        let is_storage_resource_exhausted = error
                            .find_typed_context::<RemoteExecutionError>()
                            .is_some_and(|re_client_error| {
                                is_storage_resource_exhausted(re_client_error.as_ref())
                            });
                        let error_type = if is_storage_resource_exhausted {
                            CommandExecutionErrorType::StorageResourceExhausted
                        } else {
                            CommandExecutionErrorType::Other
                        };
                        return ControlFlow::Break(DownloadResult::Result(
                            manager.error_classified("extract_artifacts", error, error_type),
                        ));
                    }
                };

            if missing_cas_is_cache_miss {
                if let Some(known_missing_remote_cas) = known_missing_remote_cas
                    && known_missing_remote_cas
                        .contains_artifact_values(artifacts.mapped_outputs.values())
                {
                    tracing::debug!(
                        "Remote result for `{}` referenced a known-missing CAS blob; treating it as a miss",
                        details.action_digest,
                    );
                    let origin = Some(RemoteActionCacheOrigin::new(
                        details.action_digest.dupe(),
                        artifacts.now,
                        artifacts.ttl,
                    ));
                    let missing = match lost_remote_cas_artifacts_for_outputs(
                        artifact_fs,
                        &artifacts.mapped_outputs,
                        |digest| known_missing_remote_cas.contains_tracked_file_digest(digest),
                        origin,
                    ) {
                        Ok(missing) => missing,
                        Err(e) => {
                            return ControlFlow::Break(DownloadResult::Result(manager.error(
                                "verify_cached_outputs",
                                e.context(format!("action_digest={}", details.action_digest)),
                            )));
                        }
                    };
                    known_missing_remote_cas.remove_artifact_values(artifacts.mapped_outputs.values());
                    return ControlFlow::Break(DownloadResult::CacheMiss { manager, missing });
                }

                match remote_artifact_values_presence(self.re_client, &artifacts.mapped_outputs)
                    .await
                {
                    Ok(RemoteCacheDigestPresence::Present) => {}
                    Ok(RemoteCacheDigestPresence::Missing(missing)) => {
                        if let Some(known_missing_remote_cas) = known_missing_remote_cas {
                            known_missing_remote_cas.record_file_digests(&missing);
                        }
                        tracing::debug!(
                            "Remote result for `{}` referenced missing output CAS blobs; treating it as a miss",
                            details.action_digest,
                        );
                        let origin = Some(RemoteActionCacheOrigin::new(
                            details.action_digest.dupe(),
                            artifacts.now,
                            artifacts.ttl,
                        ));
                        let missing = match lost_remote_cas_artifacts_for_outputs(
                            artifact_fs,
                            &artifacts.mapped_outputs,
                            |digest| {
                                missing
                                    .iter()
                                    .any(|missing_digest| missing_digest.data() == digest.data())
                            },
                            origin,
                        ) {
                            Ok(missing) => missing,
                            Err(e) => {
                                return ControlFlow::Break(DownloadResult::Result(manager.error(
                                    "verify_cached_outputs",
                                    e.context(format!("action_digest={}", details.action_digest)),
                                )));
                            }
                        };
                        return ControlFlow::Break(DownloadResult::CacheMiss { manager, missing });
                    }
                    Err(e) => {
                        return ControlFlow::Break(DownloadResult::Result(manager.error(
                            "verify_cached_outputs",
                            e.context(format!("action_digest={}", details.action_digest)),
                        )));
                    }
                }
            }

            let info = CasDownloadInfo::new_execution(
                TrackedActionDigest::new_expires(
                    details.action_digest.dupe(),
                    artifacts.expires,
                    self.digest_config.cas_digest_config(),
                ),
                self.re_client.use_case,
                artifacts.now,
                artifacts.ttl,
            );

            let (manager, outputs) = match self.paranoid {
                Some(paranoid) => {
                    let manager = paranoid
                        .declare_cas_many(
                            self.materializer,
                            manager,
                            info,
                            artifacts.to_declare,
                            cancellations,
                        )
                        .await
                        .map_break(DownloadResult::Result)?;
                    (manager, artifacts.mapped_outputs)
                }
                None => {
                    // Claim the request before starting the download.
                    let manager = manager.claim().await;

                    let outputs = self.materialize_outputs(artifacts, info).await;

                    let outputs = match outputs {
                        Ok(outputs) => outputs,
                        Err(e) => {
                            return ControlFlow::Break(DownloadResult::Result(manager.error(
                                "materialize_outputs",
                                e.context(format!("action_digest={}", details.action_digest)),
                            )));
                        }
                    };

                    (manager, outputs)
                }
            };

            ControlFlow::Continue((manager, outputs))
        })
        .await
    }

    async fn extract_artifacts<'a>(
        &self,
        artifact_fs: &ArtifactFs,
        identity: &ReActionIdentity<'_>,
        paths: &CommandExecutionPaths,
        working_directory: &ProjectRelativePath,
        requested_outputs: impl IntoIterator<Item = CommandExecutionOutputRef<'a>>,
        output_spec: &dyn RemoteActionResult,
    ) -> bz_error::Result<ExtractedArtifacts> {
        let now = Utc::now();
        let ttl = Duration::seconds(output_spec.ttl());
        let expires = now + ttl;

        // Bazel's remote cache-hit path derives output metadata from the ActionResult and
        // injects it into the action output metadata store. Mirror that shape here: build an
        // output-only metadata tree instead of cloning the full input tree just to overlay outputs.
        let output_paths = paths.output_paths();
        let mut output_dir = ActionDirectoryBuilder::empty();

        for x in output_spec.output_files() {
            let digest = FileDigest::from_re(&x.digest.digest, self.digest_config)?;
            let digest = TrackedFileDigest::new_expires(
                digest,
                expires,
                self.digest_config.cas_digest_config(),
            );

            let entry = DirectoryEntry::Leaf(ActionDirectoryMember::File(FileMetadata {
                digest,
                is_executable: x.executable,
            }));

            let output_path = re_output_path_to_project_path(working_directory, x.name.as_str())?;
            output_dir.insert(&output_path, entry)?;
        }

        for x in output_spec.output_symlinks() {
            let entry = DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(Arc::new(
                Symlink::new(RelativePathBuf::from_path(Path::new(&x.target))?),
            )));
            let output_path = re_output_path_to_project_path(working_directory, x.name.as_str())?;
            output_dir.insert(&output_path, entry)?;
        }

        {
            let _permit = if let Some(semaphore) = self.output_trees_download_config.semaphore() {
                let blob_size = output_spec
                    .output_directories()
                    .iter()
                    .filter(|x| !is_empty_re_tree_digest(x.tree_digest.size_in_bytes))
                    .map(|x| x.tree_digest.size_in_bytes)
                    .sum::<i64>();

                let blob_size: u32 = blob_size
                    .try_into()
                    .unwrap_or(semaphore.max_concurrent_bytes);

                Some(
                    semaphore
                        .semaphore
                        .acquire_many(blob_size.min(semaphore.max_concurrent_bytes))
                        .await?,
                )
            } else {
                None
            };

            // Compute output metadata from output_directories. Empty Tree messages are common and
            // always encode to two bytes, so avoid a CAS roundtrip for them like Bazel does.
            let tree_digests = output_spec
                .output_directories()
                .iter()
                .filter(|x| !is_empty_re_tree_digest(x.tree_digest.size_in_bytes))
                .map(|x| x.tree_digest.clone())
                .collect();
            let trees = self
                .re_client
                .download_typed_blobs::<RE::Tree>(Some(identity), tree_digests)
                .boxed()
                .await
                .buck_error_context("Failed to download trees")?;
            let mut trees = trees.into_iter();

            for dir in output_spec.output_directories() {
                let tree = if is_empty_re_tree_digest(dir.tree_digest.size_in_bytes) {
                    RE::Tree {
                        root: Some(RE::Directory::default()),
                        children: Vec::new(),
                    }
                } else {
                    trees.next().ok_or_else(|| {
                        bz_error::bz_error!(
                            bz_error::ErrorTag::Tier0,
                            "missing downloaded tree metadata for `{}`",
                            dir.path
                        )
                    })?
                };
                let entry = re_tree_to_directory(
                    &tree,
                    &expires,
                    self.digest_config,
                    self.output_trees_download_config
                        .fingerprint_re_output_trees_eagerly(),
                )?;
                let output_path =
                    re_output_path_to_project_path(working_directory, dir.path.as_str())?;
                output_dir.insert(&output_path, DirectoryEntry::Dir(entry))?;
            }
        }

        let mut to_declare = Vec::with_capacity(output_paths.len());
        let mut mapped_outputs = BuckIndexMap::with_capacity(output_paths.len());

        for (requested, (path, _)) in requested_outputs.into_iter().zip(output_paths.iter()) {
            let value = extract_artifact_value(&output_dir, path, self.digest_config)?;
            if let Some(value) = value {
                let configuration_path = if self.materializer.is_eager_materialization_enabled()
                    && requested.has_content_based_path()
                {
                    Some(
                        requested
                            .resolve_configuration_hash_path(artifact_fs)?
                            .path
                            .to_owned(),
                    )
                } else {
                    None
                };
                to_declare.push(DeclareArtifactPayload {
                    path: requested
                        .resolve(
                            artifact_fs,
                            if requested.has_content_based_path() {
                                Some(value.content_based_path_hash())
                            } else {
                                None
                            }
                            .as_ref(),
                        )?
                        .path
                        .to_owned(),
                    artifact: value.dupe(),
                    configuration_path,
                });
                mapped_outputs.insert(requested.cloned(), value);
            }
        }

        Ok(ExtractedArtifacts {
            to_declare,
            mapped_outputs,
            now,
            expires,
            ttl,
        })
    }

    async fn materialize_outputs(
        &self,
        artifacts: ExtractedArtifacts,
        info: CasDownloadInfo,
    ) -> bz_error::Result<BuckIndexMap<CommandExecutionOutput, ArtifactValue>> {
        // Declare the outputs to the materializer
        self.materializer
            .declare_cas_many(Arc::new(info), artifacts.to_declare)
            .boxed()
            .await
            .buck_error_context("Failed to declare in materializer")?;

        Ok(artifacts.mapped_outputs)
    }
}

fn is_empty_re_tree_digest(size_in_bytes: i64) -> bool {
    size_in_bytes == 2
}

/// Takes a path that came from RE and tries to convert it to
/// a `ForwardRelativePath`. These paths are supposed to be forward relative,
/// so if the conversion fails, RE is broken.
fn re_forward_path(re_path: &str) -> bz_error::Result<&ForwardRelativePath> {
    // RE sends us paths with trailing slash.
    ForwardRelativePath::new_trim_trailing_slashes(re_path)
        .buck_error_context("Path received from RE is not normalized.")
}

fn re_output_path_to_project_path(
    working_directory: &ProjectRelativePath,
    re_path: &str,
) -> bz_error::Result<ProjectRelativePathBuf> {
    Ok(working_directory.join(re_forward_path(re_path)?))
}

struct ExtractedArtifacts {
    to_declare: Vec<DeclareArtifactPayload>,
    mapped_outputs: BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
    now: DateTime<Utc>,
    expires: DateTime<Utc>,
    ttl: Duration,
}

fn lost_remote_cas_artifacts_for_outputs(
    artifact_fs: &ArtifactFs,
    outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
    is_missing: impl Fn(&TrackedFileDigest) -> bool,
    origin: Option<RemoteActionCacheOrigin>,
) -> bz_error::Result<Option<LostRemoteCasArtifacts>> {
    let mut lost = Vec::new();
    for (output, value) in outputs {
        let missing_digests = artifact_file_digests(value)
            .into_iter()
            .filter(|digest| is_missing(digest))
            .collect::<Vec<_>>();
        if missing_digests.is_empty() {
            continue;
        }

        let content_hash = output
            .has_content_based_path()
            .then(|| value.content_based_path_hash());
        let path = output
            .as_ref()
            .resolve(artifact_fs, content_hash.as_ref())?
            .path;
        lost.push(LostRemoteCasArtifact {
            path: Arc::new(path),
            owner: None,
            missing_digests: Arc::from(missing_digests),
            producer_path_hint: None,
            origin: origin.clone(),
        });
    }

    Ok((!lost.is_empty()).then(|| LostRemoteCasArtifacts::new(lost)))
}

/// Did this download work out?
pub enum DownloadResult {
    /// Got a result: might be a success, might be a failure. Caller needs to deal with this
    /// result.
    Result(CommandExecutionResult),
    /// A remote result referenced missing CAS data before claiming outputs. The caller may retry the
    /// action with cache lookup disabled.
    CacheMiss {
        manager: CommandExecutionManager,
        missing: Option<LostRemoteCasArtifacts>,
    },
}

impl FromResidual<ControlFlow<Self, Infallible>> for DownloadResult {
    fn from_residual(residual: ControlFlow<Self, Infallible>) -> Self {
        match residual {
            ControlFlow::Break(v) => v,
        }
    }
}
