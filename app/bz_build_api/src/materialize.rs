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
use std::time::Instant;

use allocative::Allocative;
use async_trait::async_trait;
use bz_artifact::actions::key::ActionKey;
use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::artifact_type::BaseArtifactKind;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_build_signals::env::NodeDuration;
use bz_build_signals::env::WaitingCategory;
use bz_build_signals::env::WaitingData;
use bz_cli_proto::build_request::Materializations;
use bz_cli_proto::build_request::Uploads;
use bz_common::legacy_configs::dice::HasLegacyConfigs;
use bz_common::legacy_configs::key::BuckconfigKeyRef;
use bz_common::legacy_configs::view::LegacyBuckConfigView;
use bz_core::execution_types::executor_config::RemoteExecutorUseCase;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_data::ToProtoMessage;
use bz_error::BuckErrorContext;
use bz_events::dispatch::current_span;
use bz_events::dispatch::span_async_simple;
use bz_execute::artifact::artifact_dyn::ArtifactDyn;
use bz_execute::artifact_utils::ArtifactValueBuilder;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest_config::HasDigestConfig;
use bz_execute::directory::ActionDirectoryBuilder;
use bz_execute::execute::blobs::ActionBlobs;
use bz_execute::materialize::materializer::CasDownloadInfo;
use bz_execute::materialize::materializer::CasNotFoundError;
use bz_execute::materialize::materializer::DeclareArtifactPayload;
use bz_execute::materialize::materializer::HasMaterializer;
use bz_execute::materialize::materializer::LostRemoteCasArtifacts;
use bz_execute::materialize::materializer::MaterializationError;
use bz_execute::materialize::materializer::Materializer;
use bz_execute::materialize::materializer::RemoteActionCacheOrigin;
use bz_hash::BuckDashSet;
use bz_hash::BuckIndexMap;
use bz_util::time_span::TimeSpan;
use dice::DiceComputations;
use dice::DiceComputationsData;
use dice::UserComputationData;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::StreamExt;
use pagable::Pagable;

use crate::actions::artifact::get_artifact_fs::GetArtifactFs;
use crate::actions::calculation::ActionCalculation;
use crate::actions::calculation::HasLostRemoteRewindTracker;
use crate::actions::execute::dice_data::GetKnownMissingRemoteCasTracker;
use crate::actions::execute::dice_data::GetReClient;
use crate::actions::impls::run_action_knobs::HasRunActionKnobs;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::ArtifactGroupValues;
use crate::artifact_groups::calculation::ArtifactGroupCalculation;
use crate::build_signals::HasBuildSignals;
use crate::lost_remote::LostRemoteRewindGraph;
use crate::lost_remote::LostRemoteRewindGraphBuilder;
use crate::lost_remote::lost_remote_build_restart_error;

#[async_trait]
pub trait RemoteCacheInvalidator: Send + Sync {
    async fn purge_remote_cache_metadata(&self) -> bz_error::Result<()>;

    async fn purge_remote_cache_metadata_for_origins(
        &self,
        origins: Vec<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        let _unused = origins;
        self.purge_remote_cache_metadata().await
    }
}

pub trait SetRemoteCacheInvalidator {
    fn set_remote_cache_invalidator(&mut self, invalidator: Arc<dyn RemoteCacheInvalidator>);
}

pub trait HasRemoteCacheInvalidator {
    fn get_remote_cache_invalidator(&self) -> Option<Arc<dyn RemoteCacheInvalidator>>;
}

#[derive(Clone, Dupe)]
struct RemoteCacheInvalidatorHolder(Arc<dyn RemoteCacheInvalidator>);

impl SetRemoteCacheInvalidator for UserComputationData {
    fn set_remote_cache_invalidator(&mut self, invalidator: Arc<dyn RemoteCacheInvalidator>) {
        self.data.set(RemoteCacheInvalidatorHolder(invalidator));
    }
}

impl HasRemoteCacheInvalidator for UserComputationData {
    fn get_remote_cache_invalidator(&self) -> Option<Arc<dyn RemoteCacheInvalidator>> {
        self.data
            .get::<RemoteCacheInvalidatorHolder>()
            .ok()
            .map(|holder| holder.0.dupe())
    }
}

pub(crate) async fn purge_remote_cache_metadata_for_origins(
    ctx: &mut DiceComputations<'_>,
    origins: Vec<RemoteActionCacheOrigin>,
) -> bz_error::Result<()> {
    if origins.is_empty() {
        return Ok(());
    }

    if let Some(invalidator) = ctx.per_transaction_data().get_remote_cache_invalidator() {
        invalidator
            .purge_remote_cache_metadata_for_origins(origins)
            .await?;
    } else if let Some(extension) = ctx
        .per_transaction_data()
        .get_materializer()
        .as_deferred_materializer_extension()
    {
        extension
            .clear_remote_declared_cas_for_origin_action_digests(
                origins
                    .iter()
                    .map(|origin| origin.action_digest().dupe())
                    .collect(),
            )
            .await?;
    }

    Ok(())
}

pub async fn materialize_and_upload_artifact_group(
    ctx: &mut DiceComputations<'_>,
    _cancellation: &CancellationContext,
    artifact_group: &ArtifactGroup,
    contexts: MaterializationAndUploadContext,
    queue_tracker: &Arc<BuckDashSet<BuildArtifact>>,
) -> bz_error::Result<ArtifactGroupValues> {
    let values = materialize_artifact_group(ctx, artifact_group, contexts.0, queue_tracker).await?;

    if let UploadContext::Upload = contexts.1 {
        if let Err(error) = ensure_uploaded(ctx, &values).await {
            if let Some(lost) = error.find_typed_context::<LostRemoteCasArtifacts>() {
                let restart_error = prepare_lost_remote_upload_rewind_restart(ctx, &values, &lost)
                    .await
                    .unwrap_or_else(|restart_error| restart_error);
                return Err(restart_error);
            }
            return Err(error);
        }
    }

    Ok(values)
}

async fn materialize_artifact_group(
    ctx: &mut DiceComputations<'_>,
    artifact_group: &ArtifactGroup,
    materialization_context: MaterializationContext,
    queue_tracker: &Arc<BuckDashSet<BuildArtifact>>,
) -> bz_error::Result<ArtifactGroupValues> {
    let values = ctx.ensure_artifact_group(artifact_group).await?;

    if let MaterializationContext::Materialize { force } = materialization_context {
        loop {
            match materialize_artifact_group_values(
                ctx,
                artifact_group,
                &values,
                queue_tracker,
                force,
            )
            .await
            {
                Ok(()) => break,
                Err(MaterializationAttemptError::Error(error)) => return Err(error),
                Err(MaterializationAttemptError::LostRemoteOutputs(lost)) => {
                    let plan = prepare_lost_remote_output_rewind_plan(ctx, lost)?;
                    prepare_lost_remote_output_rewind_restart(ctx, &plan).await?;
                    return Err(lost_remote_build_restart_error(plan.rewind_graph()));
                }
            }
        }
    }

    Ok(values)
}

enum MaterializationAttemptError {
    LostRemoteOutputs(Vec<LostRemoteOutput>),
    Error(bz_error::Error),
}

impl From<bz_error::Error> for MaterializationAttemptError {
    fn from(error: bz_error::Error) -> Self {
        Self::Error(error)
    }
}

struct LostRemoteOutput {
    artifact: BuildArtifact,
    source: CasNotFoundError,
    origin: RemoteActionCacheOrigin,
}

struct LostRemoteOutputRewindRecord {
    artifact: BuildArtifact,
    source: CasNotFoundError,
    origin: RemoteActionCacheOrigin,
}

struct LostRemoteOutputRewindPlan {
    records: Vec<LostRemoteOutputRewindRecord>,
    producers: BuckIndexMap<ActionKey, BuildArtifact>,
}

impl LostRemoteOutputRewindPlan {
    fn new(lost: Vec<LostRemoteOutput>) -> Self {
        let mut producers = BuckIndexMap::new();
        let mut records = Vec::with_capacity(lost.len());
        for lost_output in lost {
            let action_key = lost_output.artifact.key().dupe();
            if !producers.contains_key(&action_key) {
                producers.insert(action_key, lost_output.artifact.dupe());
            }
            records.push(LostRemoteOutputRewindRecord {
                artifact: lost_output.artifact,
                source: lost_output.source,
                origin: lost_output.origin,
            });
        }
        Self { records, producers }
    }

    fn repeated_loss_signatures(&self) -> Vec<String> {
        let mut signatures = Vec::new();
        for record in &self.records {
            let missing_digests = record.source.missing_file_digests();
            if missing_digests.is_empty() {
                signatures.push(format!(
                    "top-level-output|{}|{}|<unknown>",
                    record.artifact.key(),
                    record.source.path
                ));
            } else {
                signatures.extend(missing_digests.iter().map(|digest| {
                    format!(
                        "top-level-output|{}|{}|{}",
                        record.artifact.key(),
                        record.source.path,
                        digest
                    )
                }));
            }
        }
        signatures
    }

    fn display_summary(&self) -> String {
        self.records
            .iter()
            .map(|record| {
                let missing_digests = record.source.missing_file_digests();
                let missing_digests = if missing_digests.is_empty() {
                    "<unknown>".to_owned()
                } else {
                    missing_digests
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!(
                    "  `{}` producer `{}` origin action `{}` missing digests `{}`",
                    record.source.path,
                    record.artifact.key(),
                    record.origin.action_digest(),
                    missing_digests,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn restart_reason(&self) -> String {
        format!(
            "remote-backed final outputs are missing from CAS; purged remote cache metadata and invalidated {} producer action(s):\n{}",
            self.producers.len(),
            self.display_summary(),
        )
    }

    fn rewind_graph(&self) -> LostRemoteRewindGraph {
        let mut builder = LostRemoteRewindGraphBuilder::default();
        for (action_key, producer) in &self.producers {
            builder.add_action_key(action_key.dupe());
            builder.add_artifact(&Artifact::from(producer.dupe()));
            builder.add_artifact_group(&ArtifactGroup::Artifact(Artifact::from(producer.dupe())));
        }
        builder.finish(self.restart_reason())
    }

    fn remote_origins(&self) -> Vec<RemoteActionCacheOrigin> {
        let mut origins = Vec::new();
        for record in &self.records {
            if !origins
                .iter()
                .any(|origin: &RemoteActionCacheOrigin| origin == &record.origin)
            {
                origins.push(record.origin.clone());
            }
        }
        origins
    }
}

async fn materialize_artifact_group_values(
    ctx: &mut DiceComputations<'_>,
    artifact_group: &ArtifactGroup,
    values: &ArtifactGroupValues,
    queue_tracker: &Arc<BuckDashSet<BuildArtifact>>,
    force: bool,
) -> Result<(), MaterializationAttemptError> {
    let mut waiting_data = WaitingData::new();
    waiting_data.start_waiting_category_now(WaitingCategory::MaterializerPrepare);
    let artifact_fs = ctx.get_artifact_fs().await?;
    let digest_config = ctx.global_data().get_digest_config();
    let data = ctx.data();
    let materializer = ctx.per_transaction_data().get_materializer();
    let mut lost_remote_outputs = Vec::new();

    for ((artifact, value), remote_cache_cas_info) in values.iter_with_remote_cache_cas_info() {
        let BaseArtifactKind::Build(artifact) = artifact.as_parts().0 else {
            continue;
        };

        if !queue_tracker.insert(artifact.dupe()) {
            // We've already requested this artifact, no use requesting it again.
            continue;
        }

        let artifact = artifact.dupe();
        let result = materialize_build_artifact(
            &data,
            materializer.dupe(),
            &artifact_fs,
            digest_config,
            artifact_group,
            artifact.dupe(),
            value.dupe(),
            remote_cache_cas_info.cloned(),
            waiting_data.clone(),
            force,
        )
        .await;

        match result {
            Ok(()) => {}
            Err(MaterializationAttemptError::LostRemoteOutputs(mut lost)) => {
                queue_tracker.remove(&artifact);
                lost_remote_outputs.append(&mut lost);
            }
            Err(MaterializationAttemptError::Error(error)) => {
                queue_tracker.remove(&artifact);
                return Err(MaterializationAttemptError::Error(error));
            }
        }
    }

    if !lost_remote_outputs.is_empty() {
        return Err(MaterializationAttemptError::LostRemoteOutputs(
            lost_remote_outputs,
        ));
    }

    Ok(())
}

async fn materialize_build_artifact(
    data: &DiceComputationsData,
    materializer: Arc<dyn Materializer>,
    artifact_fs: &bz_core::fs::artifact_path_resolver::ArtifactFs,
    digest_config: bz_execute::digest_config::DigestConfig,
    artifact_group: &ArtifactGroup,
    artifact: BuildArtifact,
    value: ArtifactValue,
    remote_cache_cas_info: Option<Arc<CasDownloadInfo>>,
    waiting_data: WaitingData,
    force: bool,
) -> Result<(), MaterializationAttemptError> {
    let configuration_hash_path =
        artifact_fs.resolve_build_configuration_hash_path(artifact.get_path())?;

    if let Some(remote_cache_cas_info) = remote_cache_cas_info {
        let payload = remote_cas_payload_for_build_artifact(
            materializer.as_ref(),
            artifact_fs,
            &artifact,
            &value,
        )?;
        materializer
            .declare_cas_many(remote_cache_cas_info, vec![payload])
            .await
            .buck_error_context("Failed to declare remote-backed local action-cache output")?;
    }

    if artifact.get_path().is_content_based_path() {
        let content_based_path = artifact_fs
            .resolve_build(artifact.get_path(), Some(&value.content_based_path_hash()))?;
        let mut builder = ArtifactValueBuilder::new(artifact_fs.fs(), digest_config);
        builder.add_symlinked(
            // The materializer doesn't care about the `src_value`.
            &ArtifactValue::dir(digest_config.empty_directory()),
            content_based_path,
            &configuration_hash_path,
        )?;
        let symlink_value = builder.build(&configuration_hash_path)?;

        materializer
            .declare_copy(
                configuration_hash_path.clone(),
                symlink_value,
                Vec::new(),
                None,
            )
            .await
            .buck_error_context(
                "Failed to declare configuration path to content-based path symlinks",
            )?;
    }

    try_materialize_requested_artifact(
        data,
        materializer,
        artifact,
        waiting_data,
        force,
        configuration_hash_path,
        artifact_group,
    )
    .await
}

fn remote_cas_payload_for_build_artifact(
    materializer: &dyn Materializer,
    artifact_fs: &bz_core::fs::artifact_path_resolver::ArtifactFs,
    artifact: &BuildArtifact,
    value: &ArtifactValue,
) -> bz_error::Result<DeclareArtifactPayload> {
    let has_content_based_path = artifact.get_path().is_content_based_path();
    let content_hash = has_content_based_path.then(|| value.content_based_path_hash());
    let path = artifact_fs.resolve_build(artifact.get_path(), content_hash.as_ref())?;
    let configuration_path =
        if materializer.is_eager_materialization_enabled() && has_content_based_path {
            Some(artifact_fs.resolve_build_configuration_hash_path(artifact.get_path())?)
        } else {
            None
        };

    Ok(DeclareArtifactPayload {
        path,
        artifact: value.dupe(),
        configuration_path,
    })
}

async fn try_materialize_requested_artifact(
    data: &DiceComputationsData,
    materializer: Arc<dyn Materializer>,
    artifact: BuildArtifact,
    waiting_data: WaitingData,
    required: bool,
    path: ProjectRelativePathBuf,
    requested_group: &ArtifactGroup,
) -> Result<(), MaterializationAttemptError> {
    let artifact_for_end = artifact.dupe();
    let start_event = bz_data::MaterializeRequestedArtifactStart {
        artifact: Some(artifact.as_proto()),
    };

    span_async_simple(
        start_event,
        async move {
            let now = Instant::now();

            let result = if required {
                let mut stream = materializer.materialize_many(vec![path]).await?;
                let mut result = Ok(());
                while let Some(materialized) = stream.next().await {
                    if let Err(error) = materialized {
                        result = Err(error);
                        break;
                    }
                }
                result
            } else {
                match materializer
                    .try_materialize_final_artifact_with_errors(path)
                    .await?
                {
                    Ok(_) => Ok(()),
                    Err(error) => Err(error),
                }
            };

            if let Some(signals) = data.per_transaction_data().get_build_signals() {
                let duration = Instant::now() - now;
                signals.final_materialization(
                    artifact.dupe(),
                    requested_group.dupe(),
                    NodeDuration {
                        user: duration,
                        total: TimeSpan::from_start_and_duration(now, duration),
                        queue: None,
                    },
                    current_span(),
                    waiting_data,
                );
            }

            result.map_err(|error| classify_materialization_error(artifact, error))
        },
        bz_data::MaterializeRequestedArtifactEnd {
            artifact: Some(artifact_for_end.as_proto()),
        },
    )
    .await
}

fn classify_materialization_error(
    artifact: BuildArtifact,
    error: MaterializationError,
) -> MaterializationAttemptError {
    match error {
        MaterializationError::NotFound { source } => match source.info.remote_origin() {
            Some(origin) => {
                MaterializationAttemptError::LostRemoteOutputs(vec![LostRemoteOutput {
                    artifact,
                    source,
                    origin,
                }])
            }
            None => {
                MaterializationAttemptError::Error(MaterializationError::NotFound { source }.into())
            }
        },
        error => MaterializationAttemptError::Error(error.into()),
    }
}

fn prepare_lost_remote_output_rewind_plan(
    ctx: &mut DiceComputations<'_>,
    lost: Vec<LostRemoteOutput>,
) -> bz_error::Result<LostRemoteOutputRewindPlan> {
    let plan = LostRemoteOutputRewindPlan::new(lost);
    let tracker = ctx
        .per_transaction_data()
        .get_known_missing_remote_cas_tracker();
    for record in &plan.records {
        let missing_digests = record.source.missing_file_digests();
        tracker.record_file_digests(&missing_digests);
    }
    ctx.per_transaction_data()
        .record_lost_remote_rewind_attempt(
            &plan.display_summary(),
            plan.repeated_loss_signatures(),
        )?;
    Ok(plan)
}

async fn prepare_lost_remote_output_rewind_restart(
    ctx: &mut DiceComputations<'_>,
    plan: &LostRemoteOutputRewindPlan,
) -> bz_error::Result<()> {
    tracing::warn!(
        "Remote-backed outputs are missing from CAS; purging remote cache metadata and restarting build after invalidating {} producer action(s): {}",
        plan.producers.len(),
        plan.producers
            .values()
            .map(|owner: &BuildArtifact| owner.key().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    );

    invalidate_lost_remote_output_producer_paths(ctx, plan).await?;
    purge_remote_cache_metadata_for_origins(ctx, plan.remote_origins()).await?;
    ctx.per_transaction_data()
        .record_lost_remote_action_cache_bypass(plan.producers.keys().cloned().collect())?;
    Ok(())
}

async fn invalidate_lost_remote_output_producer_paths(
    ctx: &mut DiceComputations<'_>,
    plan: &LostRemoteOutputRewindPlan,
) -> bz_error::Result<()> {
    let artifact_fs = ctx.get_artifact_fs().await?;
    let mut output_paths = Vec::new();
    for action_key in plan.producers.keys() {
        let producer = ActionCalculation::get_action(ctx, action_key).await?;
        output_paths.extend(
            producer
                .outputs()
                .iter()
                .map(|output| artifact_fs.resolve_build_configuration_hash_path(output.get_path()))
                .collect::<bz_error::Result<Vec<_>>>()?,
        );
    }

    if !output_paths.is_empty() {
        ctx.per_transaction_data()
            .get_materializer()
            .invalidate_many(output_paths)
            .await
            .buck_error_context("Failed to invalidate outputs for lost remote output rewind")?;
    }

    Ok(())
}

async fn ensure_uploaded(
    ctx: &mut DiceComputations<'_>,
    values: &ArtifactGroupValues,
) -> bz_error::Result<()> {
    let digest_config = ctx.global_data().get_digest_config();
    let artifact_fs = ctx.get_artifact_fs().await?;
    let mut dir = ActionDirectoryBuilder::empty();
    for (artifact, value) in values.iter() {
        let path = artifact.resolve_path(
            &artifact_fs,
            if artifact.path_resolution_requires_artifact_value() {
                Some(value.content_based_path_hash())
            } else {
                None
            }
            .as_ref(),
        )?;
        bz_execute::directory::insert_artifact(&mut dir, path, value)?;
    }
    let dir = dir.fingerprint(digest_config.as_directory_serializer());
    let re_use_case = ctx
        .get_legacy_root_config_on_dice()
        .await
        .and_then(|cfg| {
            cfg.view(ctx).get(BuckconfigKeyRef {
                section: "build",
                property: "default_remote_execution_use_case",
            })
        })
        .ok()
        .flatten()
        .map_or_else(RemoteExecutorUseCase::bz_default, |v| {
            RemoteExecutorUseCase::new((*v).to_owned())
        });
    ctx.per_transaction_data()
        .get_re_client()
        .with_use_case(re_use_case)
        .upload(
            artifact_fs.fs(),
            &ctx.per_transaction_data().get_materializer(),
            &ActionBlobs::new(digest_config),
            ProjectRelativePath::empty(),
            &dir,
            None,
            None,
            digest_config,
            ctx.per_transaction_data()
                .get_run_action_knobs()
                .deduplicate_get_digests_ttl_calls,
            false,
        )
        .await?;

    Ok(())
}

async fn prepare_lost_remote_upload_rewind_restart(
    ctx: &mut DiceComputations<'_>,
    values: &ArtifactGroupValues,
    lost: &LostRemoteCasArtifacts,
) -> bz_error::Result<bz_error::Error> {
    ctx.per_transaction_data()
        .get_known_missing_remote_cas_tracker()
        .record_lost_remote_cas_artifacts(lost);

    let summary = lost.display_summary();
    let signatures = lost
        .iter()
        .flat_map(|artifact| {
            if artifact.missing_digests.is_empty() {
                vec![format!("upload|{}|<unknown>", artifact.path)]
            } else {
                artifact
                    .missing_digests
                    .iter()
                    .map(|digest| format!("upload|{}|{}", artifact.path, digest))
                    .collect::<Vec<_>>()
            }
        })
        .collect::<Vec<_>>();
    ctx.per_transaction_data()
        .record_lost_remote_rewind_attempt(&summary, signatures)?;

    let artifact_fs = ctx.get_artifact_fs().await?;
    let mut builder = LostRemoteRewindGraphBuilder::default();
    let mut output_paths = Vec::new();
    let mut action_keys = Vec::new();

    for (artifact, _value) in values.iter() {
        let BaseArtifactKind::Build(build_artifact) = artifact.as_parts().0 else {
            continue;
        };
        let action_key = build_artifact.key().dupe();
        if !action_keys.contains(&action_key) {
            let action = ActionCalculation::get_action(ctx, &action_key).await?;
            output_paths.extend(
                action
                    .outputs()
                    .iter()
                    .map(|output| {
                        artifact_fs.resolve_build_configuration_hash_path(output.get_path())
                    })
                    .collect::<bz_error::Result<Vec<_>>>()?,
            );
            action_keys.push(action_key.dupe());
        }
        builder.add_action_key(action_key);
        builder.add_artifact(artifact);
        builder.add_artifact_group(&ArtifactGroup::Artifact(artifact.dupe()));
    }

    if !output_paths.is_empty() {
        ctx.per_transaction_data()
            .get_materializer()
            .invalidate_many(output_paths)
            .await
            .buck_error_context("Failed to invalidate outputs for lost remote upload rewind")?;
    }

    if action_keys.is_empty() {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "Remote-backed artifacts are missing from CAS while uploading final outputs, but no build producer was found to retry:\n{}",
            summary,
        ));
    }

    let mut origins = Vec::new();
    for artifact in lost.iter() {
        if !origins
            .iter()
            .any(|origin: &RemoteActionCacheOrigin| origin == &artifact.origin)
        {
            origins.push(artifact.origin.clone());
        }
    }
    purge_remote_cache_metadata_for_origins(ctx, origins).await?;
    ctx.per_transaction_data()
        .record_lost_remote_action_cache_bypass(action_keys)?;

    Ok(lost_remote_build_restart_error(builder.finish(format!(
        "remote-backed artifacts are missing from CAS while uploading final outputs; purged remote cache metadata and invalidated affected producer action(s):\n{}",
        summary,
    ))))
}

#[derive(Clone, Dupe, Copy, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
enum MaterializationContext {
    Skip,
    Materialize {
        /// Whether we should force the materialization of requested artifacts, or defer to the
        /// config.
        force: bool,
    },
}
impl From<Materializations> for MaterializationContext {
    fn from(value: Materializations) -> Self {
        match value {
            Materializations::Skip => MaterializationContext::Skip,
            Materializations::Default => MaterializationContext::Materialize { force: false },
            Materializations::Materialize => MaterializationContext::Materialize { force: true },
        }
    }
}

#[derive(Clone, Dupe, Copy, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
enum UploadContext {
    Skip,
    Upload,
}
impl From<Uploads> for UploadContext {
    fn from(value: Uploads) -> Self {
        match value {
            Uploads::Always => UploadContext::Upload,
            Uploads::Never => UploadContext::Skip,
        }
    }
}

#[derive(Clone, Dupe, Copy, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
enum OutputCompletionContext {
    Complete,
    Skip,
}

#[derive(Clone, Dupe, Copy, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
pub struct MaterializationAndUploadContext(
    MaterializationContext,
    UploadContext,
    OutputCompletionContext,
);
impl MaterializationAndUploadContext {
    pub fn skip() -> Self {
        Self::complete(MaterializationContext::Skip, UploadContext::Skip)
    }
    pub fn materialize() -> Self {
        Self::complete(
            MaterializationContext::Materialize { force: true },
            UploadContext::Skip,
        )
    }
    pub fn no_execution() -> Self {
        Self(
            MaterializationContext::Skip,
            UploadContext::Skip,
            OutputCompletionContext::Skip,
        )
    }
    pub fn complete_outputs(self) -> bool {
        matches!(self.2, OutputCompletionContext::Complete)
    }
    fn complete(materialization: MaterializationContext, upload: UploadContext) -> Self {
        Self(materialization, upload, OutputCompletionContext::Complete)
    }
}
impl From<(Materializations, Uploads)> for MaterializationAndUploadContext {
    fn from(value: (Materializations, Uploads)) -> Self {
        Self::complete(value.0.into(), value.1.into())
    }
}

/// This map contains all the artifacts that we enqueued for materialization. This ensures
/// we don't enqueue the same thing more than once. Should be shared across work done
/// in a single DICE transaction.
pub struct MaterializationQueueTrackerHolder(Arc<BuckDashSet<BuildArtifact>>);

pub trait HasMaterializationQueueTracker {
    fn init_materialization_queue_tracker(&mut self);

    fn get_materialization_queue_tracker(&self) -> Arc<BuckDashSet<BuildArtifact>>;
}

impl HasMaterializationQueueTracker for UserComputationData {
    fn init_materialization_queue_tracker(&mut self) {
        self.data.set(MaterializationQueueTrackerHolder(Arc::new(
            BuckDashSet::default(),
        )));
    }

    fn get_materialization_queue_tracker(&self) -> Arc<BuckDashSet<BuildArtifact>> {
        self.data
            .get::<MaterializationQueueTrackerHolder>()
            .expect("MaterializationQueueTracker should be set")
            .0
            .dupe()
    }
}
