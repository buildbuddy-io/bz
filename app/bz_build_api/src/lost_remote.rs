use std::sync::Arc;

use allocative::Allocative;
use bz_artifact::actions::key::ActionKey;
use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::artifact_type::ArtifactKind;
use bz_execute::artifact::artifact_dyn::ArtifactDyn;
use dice::DiceComputations;
use dupe::Dupe;

use crate::actions::calculation::ActionInputSetKey;
use crate::actions::calculation::BuildKey;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::TransitiveSetProjectionKey;
use crate::artifact_groups::calculation::EnsureArtifactGroupValuesKey;
use crate::artifact_groups::calculation::EnsureProjectedArtifactKey;
use crate::artifact_groups::calculation::EnsureTransitiveSetProjectionKey;

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

/// Rewinds every DICE key in `graph` at the current version (Skyframe-style action
/// rewinding): their cached results are discarded so that re-requesting them
/// recomputes within the same transaction, re-running producer actions whose
/// remote-backed outputs were lost from CAS. The build keeps running; the caller
/// must re-request whatever it consumed after this returns.
pub async fn rewind_lost_remote_graph(
    ctx: &mut DiceComputations<'_>,
    graph: &LostRemoteRewindGraph,
) -> usize {
    let mut rewound = 0;
    rewound += ctx
        .rewind_keys(
            graph
                .action_keys()
                .iter()
                .cloned()
                .map(BuildKey)
                .collect::<Vec<_>>(),
        )
        .await;
    rewound += ctx
        .rewind_keys(
            graph
                .artifacts()
                .iter()
                .cloned()
                .map(EnsureArtifactGroupValuesKey)
                .collect::<Vec<_>>(),
        )
        .await;
    rewound += ctx
        .rewind_keys(
            graph
                .projected_artifacts()
                .iter()
                .cloned()
                .map(EnsureProjectedArtifactKey)
                .collect::<Vec<_>>(),
        )
        .await;
    rewound += ctx
        .rewind_keys(
            graph
                .transitive_set_projections()
                .iter()
                .cloned()
                .map(EnsureTransitiveSetProjectionKey)
                .collect::<Vec<_>>(),
        )
        .await;
    rewound += ctx
        .rewind_keys(
            graph
                .action_input_sets()
                .iter()
                .cloned()
                .map(ActionInputSetKey)
                .collect::<Vec<_>>(),
        )
        .await;
    rewound
}
