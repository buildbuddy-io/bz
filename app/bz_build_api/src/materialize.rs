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

use allocative::Allocative;
use bz_artifact::artifact::artifact_type::BaseArtifactKind;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_build_signals::env::WaitingCategory;
use bz_build_signals::env::WaitingData;
use bz_cli_proto::build_request::Materializations;
use bz_cli_proto::build_request::Uploads;
use bz_common::legacy_configs::dice::HasLegacyConfigs;
use bz_common::legacy_configs::key::BuckconfigKeyRef;
use bz_common::legacy_configs::view::LegacyBuckConfigView;
use bz_core::execution_types::executor_config::RemoteExecutorUseCase;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_error::BuckErrorContext;
use bz_execute::artifact::artifact_dyn::ArtifactDyn;
use bz_execute::artifact_utils::ArtifactValueBuilder;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest_config::HasDigestConfig;
use bz_execute::directory::ActionDirectoryBuilder;
use bz_execute::execute::blobs::ActionBlobs;
use bz_execute::materialize::materializer::HasMaterializer;
use bz_hash::BuckDashSet;
use dice::DiceComputations;
use dice::UserComputationData;
use dice_futures::spawn::spawn_dropcancel;
use dupe::Dupe;
use futures::FutureExt;
use pagable::Pagable;

use crate::actions::artifact::get_artifact_fs::GetArtifactFs;
use crate::actions::artifact::materializer::ArtifactMaterializer;
use crate::actions::execute::dice_data::GetReClient;
use crate::actions::impls::run_action_knobs::HasRunActionKnobs;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::ArtifactGroupValues;
use crate::artifact_groups::calculation::ArtifactGroupCalculation;

pub async fn materialize_and_upload_artifact_group(
    ctx: &mut DiceComputations<'_>,
    artifact_group: &ArtifactGroup,
    contexts: MaterializationAndUploadContext,
    queue_tracker: &Arc<BuckDashSet<BuildArtifact>>,
) -> bz_error::Result<ArtifactGroupValues> {
    let (values, _) = {
        let fut = ctx.try_compute2(
            |ctx| {
                let group = artifact_group;
                async move {
                    materialize_artifact_group(ctx, group, contexts.0, queue_tracker).await
                }
                .boxed()
            },
            |ctx| {
                let group = artifact_group;
                async move {
                    match contexts.1 {
                        UploadContext::Skip => Ok(()),
                        UploadContext::Upload => ensure_uploaded(ctx, group).await,
                    }
                }
                .boxed()
            },
        );

        tokio::task::unconstrained(fut).await?
    };

    Ok(values)
}

async fn materialize_artifact_group(
    ctx: &mut DiceComputations<'_>,
    artifact_group: &ArtifactGroup,
    materialization_context: MaterializationContext,
    queue_tracker: &Arc<BuckDashSet<BuildArtifact>>,
) -> bz_error::Result<ArtifactGroupValues> {
    let values = ctx.ensure_artifact_group(artifact_group).await?;

    let mut waiting_data = WaitingData::new();

    if let MaterializationContext::Materialize { force } = materialization_context {
        waiting_data.start_waiting_category_now(WaitingCategory::MaterializerPrepare);
        let artifact_fs = ctx.get_artifact_fs().await?;
        let digest_config = ctx.global_data().get_digest_config();

        let data = ctx.data();
        let shared_data = Arc::new((
            data.dupe(),
            artifact_fs.clone(),
            ctx.per_transaction_data().get_materializer(),
        ));

        let mut materialize_futs = Vec::new();

        for (artifact, value) in values.iter() {
            if let BaseArtifactKind::Build(artifact) = artifact.as_parts().0 {
                if !queue_tracker.insert(artifact.dupe()) {
                    // We've already requested this artifact, no use requesting it again.
                    continue;
                }

                let fut = {
                    let waiting_data = waiting_data.clone();
                    let artifact = artifact.dupe();
                    let value = value.dupe();
                    let shared_data = shared_data.dupe();
                    let artifact_group = artifact_group.dupe();

                    async move {
                        let (data, artifact_fs, materializer) = &*shared_data;

                        let configuration_hash_path = artifact_fs
                            .resolve_build_configuration_hash_path(artifact.get_path())?;

                        if artifact.get_path().is_content_based_path() {
                            let content_based_path = artifact_fs.resolve_build(
                                artifact.get_path(),
                                Some(&value.content_based_path_hash()),
                            )?;
                            let mut builder =
                                ArtifactValueBuilder::new(artifact_fs.fs(), digest_config);
                            builder.add_symlinked(
                                // The materializer doesn't care about the `src_value`.
                                &ArtifactValue::dir(digest_config.empty_directory()),
                                content_based_path,
                                &configuration_hash_path,
                            )?;
                            let symlink_value = builder.build(&configuration_hash_path)?;

                            materializer
                            .declare_copy(configuration_hash_path.clone(), symlink_value, Vec::new(), None)
                            .await
                            .buck_error_context(
                                "Failed to declare configuration path to content-based path symlinks",
                            )?;
                        }

                        data.try_materialize_requested_artifact(
                            &artifact,
                            waiting_data,
                            force,
                            configuration_hash_path,
                            &artifact_group,
                        )
                        .await
                        .buck_error_context("Failed to materialize artifacts")?;
                        bz_error::Ok(())
                    }
                };
                materialize_futs.push(spawn_dropcancel(
                    move |_cancellations| fut.boxed(),
                    &*data.per_transaction_data().spawner,
                    data.per_transaction_data(),
                ));
            }
        }

        bz_util::future::try_join_all(materialize_futs).await?;
    }

    Ok(values)
}

async fn ensure_uploaded(
    ctx: &mut DiceComputations<'_>,
    artifact_group: &ArtifactGroup,
) -> bz_error::Result<()> {
    let digest_config = ctx.global_data().get_digest_config();
    let artifact_fs = ctx.get_artifact_fs().await?;
    let mut dir = ActionDirectoryBuilder::empty();
    let values = ctx.ensure_artifact_group(artifact_group).await?;
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
        )
        .await?;

    Ok(())
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
