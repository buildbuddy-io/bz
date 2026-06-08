/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::any::Any;
use std::sync::Arc;

use allocative::Allocative;
use bz_artifact::actions::key::ActionKey;
use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::artifact_type::ArtifactKind;
use bz_error::internal_error;
use bz_execute::artifact::artifact_dyn::ArtifactDyn;
use dupe::Dupe;

use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::TransitiveSetProjectionKey;

#[derive(Debug, Clone, Allocative)]
pub struct LostRemoteRewindGraph {
    action_keys: Arc<[ActionKey]>,
    artifacts: Arc<[Artifact]>,
    projected_artifacts: Arc<[ArtifactKind]>,
    transitive_set_projections: Arc<[TransitiveSetProjectionKey]>,
    action_input_sets: Arc<[Arc<[ArtifactGroup]>]>,
    reason: Arc<str>,
}

impl LostRemoteRewindGraph {
    fn new(
        action_keys: Vec<ActionKey>,
        artifacts: Vec<Artifact>,
        projected_artifacts: Vec<ArtifactKind>,
        transitive_set_projections: Vec<TransitiveSetProjectionKey>,
        action_input_sets: Vec<Arc<[ArtifactGroup]>>,
        reason: String,
    ) -> Self {
        Self {
            action_keys: Arc::from(action_keys),
            artifacts: Arc::from(artifacts),
            projected_artifacts: Arc::from(projected_artifacts),
            transitive_set_projections: Arc::from(transitive_set_projections),
            action_input_sets: Arc::from(action_input_sets),
            reason: Arc::from(reason),
        }
    }

    pub fn action_keys(&self) -> &[ActionKey] {
        &self.action_keys
    }

    pub fn artifacts(&self) -> &[Artifact] {
        &self.artifacts
    }

    pub fn projected_artifacts(&self) -> &[ArtifactKind] {
        &self.projected_artifacts
    }

    pub fn transitive_set_projections(&self) -> &[TransitiveSetProjectionKey] {
        &self.transitive_set_projections
    }

    pub fn action_input_sets(&self) -> &[Arc<[ArtifactGroup]>] {
        &self.action_input_sets
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn is_empty(&self) -> bool {
        self.action_keys.is_empty()
            && self.artifacts.is_empty()
            && self.projected_artifacts.is_empty()
            && self.transitive_set_projections.is_empty()
            && self.action_input_sets.is_empty()
    }
}

#[derive(Default)]
pub struct LostRemoteRewindGraphBuilder {
    action_keys: Vec<ActionKey>,
    artifacts: Vec<Artifact>,
    projected_artifacts: Vec<ArtifactKind>,
    transitive_set_projections: Vec<TransitiveSetProjectionKey>,
    action_input_sets: Vec<Arc<[ArtifactGroup]>>,
}

impl LostRemoteRewindGraphBuilder {
    pub fn add_action_key(&mut self, action_key: ActionKey) {
        if !self.action_keys.contains(&action_key) {
            self.action_keys.push(action_key);
        }
    }

    pub fn add_artifact(&mut self, artifact: &Artifact) {
        if !self.artifacts.contains(artifact) {
            self.artifacts.push(artifact.dupe());
        }
        if artifact.is_projected() {
            let projected_artifact = artifact.data().dupe();
            if !self.projected_artifacts.contains(&projected_artifact) {
                self.projected_artifacts.push(projected_artifact);
            }
        }
    }

    pub fn add_artifact_group(&mut self, artifact_group: &ArtifactGroup) {
        match artifact_group {
            ArtifactGroup::Artifact(artifact) => self.add_artifact(artifact),
            ArtifactGroup::TransitiveSetProjection(projection) => {
                let key = projection.key.dupe();
                if !self.transitive_set_projections.contains(&key) {
                    self.transitive_set_projections.push(key);
                }
            }
            ArtifactGroup::Promise(_) => {}
        }
    }

    pub fn add_action_input_set(&mut self, action_input_set: Arc<[ArtifactGroup]>) {
        if !self.action_input_sets.contains(&action_input_set) {
            self.action_input_sets.push(action_input_set);
        }
    }

    pub fn finish(self, reason: String) -> LostRemoteRewindGraph {
        LostRemoteRewindGraph::new(
            self.action_keys,
            self.artifacts,
            self.projected_artifacts,
            self.transitive_set_projections,
            self.action_input_sets,
            reason,
        )
    }
}

#[derive(Debug, Clone, Allocative)]
pub struct LostRemoteBuildRestart {
    graph: LostRemoteRewindGraph,
}

impl LostRemoteBuildRestart {
    pub fn new(graph: LostRemoteRewindGraph) -> Self {
        Self { graph }
    }

    pub fn graph(&self) -> &LostRemoteRewindGraph {
        &self.graph
    }

    pub fn action_keys(&self) -> &[ActionKey] {
        self.graph.action_keys()
    }

    pub fn reason(&self) -> &str {
        self.graph.reason()
    }
}

impl bz_error::TypedContext for LostRemoteBuildRestart {
    fn eq(&self, other: &dyn bz_error::TypedContext) -> bool {
        let Some(other) = (other as &dyn Any).downcast_ref::<Self>() else {
            return false;
        };
        self.graph.action_keys == other.graph.action_keys
            && self.graph.artifacts == other.graph.artifacts
            && self.graph.projected_artifacts == other.graph.projected_artifacts
            && self.graph.transitive_set_projections == other.graph.transitive_set_projections
            && self.graph.action_input_sets == other.graph.action_input_sets
            && self.graph.reason == other.graph.reason
    }

    fn display(&self) -> Option<String> {
        None
    }
}

pub fn lost_remote_build_restart_error(graph: LostRemoteRewindGraph) -> bz_error::Error {
    internal_error!("Remote-backed artifacts are missing from CAS; restarting build attempt")
        .context(LostRemoteBuildRestart::new(graph))
}
