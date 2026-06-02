/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::time::Instant;

use async_trait::async_trait;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_build_signals::env::NodeDuration;
use bz_build_signals::env::WaitingData;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_data::ToProtoMessage;
use bz_events::dispatch::current_span;
use bz_events::dispatch::span_async_simple;
use bz_execute::materialize::materializer::HasMaterializer;
use bz_util::time_span::TimeSpan;
use dice::DiceComputations;
use dice::DiceComputationsData;
use dupe::Dupe;

use crate::artifact_groups::ArtifactGroup;
use crate::build_signals::HasBuildSignals;

#[async_trait]
pub trait ArtifactMaterializer {
    /// Called to materialize the final set of requested artifacts for the build of a target.
    /// This method will render events in superconsole.
    ///
    /// `requested_group` is the top-level `ArtifactGroup` that this artifact belongs to,
    /// used to record correct critical path dependencies (e.g. tset ensure vs individual action).
    async fn try_materialize_requested_artifact(
        &self,
        artifact: &BuildArtifact,
        waiting_data: WaitingData,
        required: bool,
        path: ProjectRelativePathBuf,
        requested_group: &ArtifactGroup,
    ) -> bz_error::Result<()>;
}

#[async_trait]
impl ArtifactMaterializer for DiceComputationsData {
    async fn try_materialize_requested_artifact(
        &self,
        artifact: &BuildArtifact,
        waiting_data: WaitingData,
        required: bool,
        path: ProjectRelativePathBuf,
        requested_group: &ArtifactGroup,
    ) -> bz_error::Result<()> {
        let materializer = self.per_transaction_data().get_materializer();
        let start_event = bz_data::MaterializeRequestedArtifactStart {
            artifact: Some(artifact.as_proto()),
        };

        span_async_simple(
            start_event,
            async move {
                let now = Instant::now();

                let result: bz_error::Result<_> = try {
                    if required {
                        materializer.ensure_materialized(vec![path]).await?;
                    } else {
                        materializer.try_materialize_final_artifact(path).await?;
                    }
                };

                if let Some(signals) = self.per_transaction_data().get_build_signals() {
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

                result
            },
            bz_data::MaterializeRequestedArtifactEnd {
                artifact: Some(artifact.as_proto()),
            },
        )
        .await
    }
}

#[async_trait]
impl ArtifactMaterializer for DiceComputations<'_> {
    async fn try_materialize_requested_artifact(
        &self,
        artifact: &BuildArtifact,
        waiting_data: WaitingData,
        required: bool,
        path: ProjectRelativePathBuf,
        requested_group: &ArtifactGroup,
    ) -> bz_error::Result<()> {
        self.data()
            .try_materialize_requested_artifact(
                artifact,
                waiting_data,
                required,
                path,
                requested_group,
            )
            .await
    }
}
