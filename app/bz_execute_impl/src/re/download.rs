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
use bz_directory::directory::entry::DirectoryEntry;
use bz_error::BuckErrorContext;
use bz_events::dispatch::console_message;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest::CasDigestFromReExt;
use bz_execute::digest_config::DigestConfig;
use bz_execute::directory::ActionDirectoryBuilder;
use bz_execute::directory::ActionDirectoryMember;
use bz_execute::directory::extract_artifact_value;
use bz_execute::directory::re_tree_to_directory;
use bz_execute::execute::action_digest::TrackedActionDigest;
use bz_execute::execute::executor_stage_async;
use bz_execute::execute::kind::RemoteCommandExecutionDetails;
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
use bz_execute::materialize::materializer::Materializer;
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
    paranoid: Option<&ParanoidDownloader>,
    cancellations: &CancellationContext,
    action_exit_code: i32,
    artifact_fs: &ArtifactFs,
    materialize_failed_re_action_inputs: bool,
    materialize_failed_re_action_outputs: bool,
    additional_message: Option<String>,
    output_trees_download_config: &OutputTreesDownloadConfig,
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

/// Did this download work out?
pub enum DownloadResult {
    /// Got a result: might be a success, might be a failure. Caller needs to deal with this
    /// result.
    Result(CommandExecutionResult),
}

impl FromResidual<ControlFlow<Self, Infallible>> for DownloadResult {
    fn from_residual(residual: ControlFlow<Self, Infallible>) -> Self {
        match residual {
            ControlFlow::Break(v) => v,
        }
    }
}
