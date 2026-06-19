use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use bz_core::configuration::compatibility::MaybeCompatible;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_error::BuckErrorContext;
use bz_events::dispatch::console_message;
use bz_node::nodes::configured_frontend::ConfiguredTargetNodeCalculation;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::Key;
use dice::NoValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use dupe::IterDupedExt;
use dupe::OptionDupedExt;
use futures::FutureExt;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::analysis::calculation::RuleAnalysisCalculation;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::ArtifactGroupValues;
use crate::artifact_groups::ResolvedArtifactGroup;
use crate::artifact_groups::ResolvedArtifactGroupBuildSignalsKey;
use crate::artifact_groups::calculation::EnsureTransitiveSetProjectionKey;
use crate::build::BuildEventConsumer;
use crate::build::BuildProviderType;
use crate::build::ConfiguredBuildEvent;
use crate::build::ConfiguredBuildEventExecutionVariant;
use crate::build::ConfiguredBuildEventVariant;
use crate::build::HasBuildEventSink;
use crate::build::ProviderArtifacts;
use crate::build::ProvidersToBuild;
use crate::build::detailed_aggregated_metrics::dice::HasDetailedAggregatedMetrics;
use crate::build::detailed_aggregated_metrics::types::TopLevelTargetSpec;
use crate::build::graph_properties;
use crate::build::graph_properties::GraphPropertiesOptions;
use crate::build::outputs::get_outputs_for_top_level_target;
use crate::build_signals::HasBuildSignals;
use crate::keep_going::KeepGoing;
use crate::materialize::HasMaterializationQueueTracker;
use crate::materialize::MaterializationAndUploadContext;
use crate::materialize::materialize_and_upload_artifact_group;
use crate::validation::validation_impl::VALIDATION_IMPL;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub(crate) struct TargetCompletionKey {
    providers_label: ConfiguredProvidersLabel,
    providers_to_build: ProvidersToBuild,
    materialization_and_upload: MaterializationAndUploadContext,
    graph_properties: GraphPropertiesOptions,
    skippable: bool,
}

impl TargetCompletionKey {
    pub(crate) fn new(
        providers_label: ConfiguredProvidersLabel,
        providers_to_build: ProvidersToBuild,
        materialization_and_upload: MaterializationAndUploadContext,
        graph_properties: GraphPropertiesOptions,
        skippable: bool,
    ) -> Self {
        Self {
            providers_label,
            providers_to_build,
            materialization_and_upload,
            graph_properties,
            skippable,
        }
    }
}

impl std::fmt::Display for TargetCompletionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TARGET_COMPLETION({}, {:?})",
            self.providers_label, self.providers_to_build
        )
    }
}

#[derive(Allocative)]
pub(crate) struct TargetCompletionValue;

#[async_trait]
impl Key for TargetCompletionKey {
    type Value = bz_error::Result<MaybeCompatible<Arc<TargetCompletionValue>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        compute_target_completion(ctx, self)
            .await
            .with_buck_error_context(|| format!("Error completing `{}`", self.providers_label))
    }

    fn equality(_: &Self::Value, _: &Self::Value) -> bool {
        false
    }

    fn validity(_: &Self::Value) -> bool {
        false
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct ArtifactCompletionKey {
    artifact_group: ArtifactGroup,
    materialization_and_upload: MaterializationAndUploadContext,
}

impl ArtifactCompletionKey {
    fn new(
        artifact_group: ArtifactGroup,
        materialization_and_upload: MaterializationAndUploadContext,
    ) -> Self {
        Self {
            artifact_group,
            materialization_and_upload,
        }
    }
}

impl std::fmt::Display for ArtifactCompletionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ARTIFACT_COMPLETION({})", self.artifact_group)
    }
}

#[async_trait]
impl Key for ArtifactCompletionKey {
    type Value = bz_error::Result<ArtifactGroupValues>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let queue_tracker = ctx
            .per_transaction_data()
            .get_materialization_queue_tracker();
        materialize_and_upload_artifact_group(
            ctx,
            _cancellation,
            &self.artifact_group,
            self.materialization_and_upload,
            &queue_tracker,
        )
        .await
        .with_buck_error_context(|| {
            format!("Error completing artifact group `{}`", self.artifact_group)
        })
    }

    fn equality(_: &Self::Value, _: &Self::Value) -> bool {
        false
    }

    fn validity(_: &Self::Value) -> bool {
        false
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn compute_target_completion(
    ctx: &mut DiceComputations<'_>,
    key: &TargetCompletionKey,
) -> bz_error::Result<MaybeCompatible<Arc<TargetCompletionValue>>> {
    let outputs =
        match get_outputs_for_top_level_target(ctx, &key.providers_label, &key.providers_to_build)
            .await?
        {
            MaybeCompatible::Incompatible(reason) => {
                return Ok(MaybeCompatible::Incompatible(reason));
            }
            MaybeCompatible::Compatible(outputs) => outputs,
        };

    let node = ctx
        .get_configured_target_node(key.providers_label.target())
        .await
        .require_compatible()?;

    ctx.top_level_target(TopLevelTargetSpec {
        label: key.providers_label.dupe(),
        target: node,
        outputs: outputs.dupe(),
    })?;

    let target_rule_type_name =
        crate::actions::calculation::get_target_rule_type_name(ctx, key.providers_label.target())
            .await?;

    let provider_collection = if key.providers_to_build.run {
        let analysis = ctx
            .get_analysis_result(key.providers_label.target())
            .await?
            .require_compatible()?;
        Some(analysis.lookup_inner(&key.providers_label)?)
    } else {
        None
    };

    if !key.skippable && outputs.is_empty() {
        let docs = "https://buck2.build/docs/users/faq/common_issues/#why-does-my-target-not-have-any-outputs";
        console_message(format!(
            "Target {} does not have any outputs. This means the rule did not define any outputs. See {} for more information",
            key.providers_label.target(),
            docs,
        ));
    }

    emit_configured_build_event(
        ctx,
        ConfiguredBuildEvent {
            label: key.providers_label.dupe(),
            variant: ConfiguredBuildEventVariant::Prepared {
                provider_collection: provider_collection.clone(),
                target_rule_type_name: target_rule_type_name.clone(),
            },
        },
    )?;

    if !key.materialization_and_upload.complete_outputs() {
        return Ok(MaybeCompatible::Compatible(Arc::new(TargetCompletionValue)));
    }

    publish_build_signal_edges(ctx, &key.providers_label, outputs.as_ref()).await?;

    let output_items: Vec<_> = outputs
        .iter()
        .duped()
        .enumerate()
        .map(|(index, (output, provider_type))| (index, output, provider_type))
        .collect();

    let validation_impl = VALIDATION_IMPL.get()?;
    let target = key.providers_label.target().dupe();
    let materialization_and_upload = key.materialization_and_upload;
    let graph_properties = key.graph_properties;
    let providers_label = key.providers_label.dupe();

    let (_outputs, _validation_result, _graph_properties) = ctx
        .compute3(
            |ctx| {
                let providers_label = providers_label.dupe();
                async move {
                    ctx.compute_join(
                        output_items,
                        |ctx: &mut DiceComputations<'_>, (index, output, provider_type)| {
                            let providers_label = providers_label.dupe();
                            async move {
                                let mut output = match ctx
                                    .compute(&ArtifactCompletionKey::new(
                                        output.dupe(),
                                        materialization_and_upload,
                                    ))
                                    .await
                                {
                                    Ok(Ok(values)) => Ok(ProviderArtifacts {
                                        values,
                                        provider_type,
                                    }),
                                    Ok(Err(e)) => Err(e),
                                    Err(e) => Err(e.into()),
                                };
                                if let Err(e) = emit_configured_build_event(
                                    ctx,
                                    ConfiguredBuildEvent {
                                        label: providers_label,
                                        variant: ConfiguredBuildEventVariant::Execution(
                                            ConfiguredBuildEventExecutionVariant::BuildOutput {
                                                index,
                                                output: output.clone(),
                                            },
                                        ),
                                    },
                                ) {
                                    output = Err(e);
                                }
                                (index, output)
                            }
                            .boxed()
                        },
                    )
                    .await
                }
                .boxed()
            },
            |ctx| {
                let providers_label = providers_label.dupe();
                async move {
                    let mut result = validation_impl
                        .validate_target_node_transitively(ctx, target)
                        .await;
                    if let Err(e) = emit_configured_build_event(
                        ctx,
                        ConfiguredBuildEvent {
                            label: providers_label,
                            variant: ConfiguredBuildEventVariant::Execution(
                                ConfiguredBuildEventExecutionVariant::Validation {
                                    result: result.clone(),
                                },
                            ),
                        },
                    ) {
                        result = Err(e);
                    }
                    result
                }
                .boxed()
            },
            |ctx| {
                let providers_label = providers_label.dupe();
                async move {
                    if graph_properties.is_empty() {
                        None
                    } else {
                        let result = graph_properties::get_graph_properties(
                            ctx,
                            key.providers_label.target(),
                            graph_properties.should_compute_configured_graph_sketch(),
                            graph_properties.retained_analysis_memory_sketch,
                        )
                        .await
                        .ok();
                        if let Err(e) = emit_configured_build_event(
                            ctx,
                            ConfiguredBuildEvent {
                                label: providers_label,
                                variant: ConfiguredBuildEventVariant::GraphProperties {
                                    graph_properties: result.clone(),
                                },
                            },
                        ) {
                            let result = Err(e);
                            return Some(result);
                        }
                        Some(result)
                    }
                }
                .boxed()
            },
        )
        .await;

    Ok(MaybeCompatible::Compatible(Arc::new(TargetCompletionValue)))
}

pub(crate) fn emit_configured_build_event(
    ctx: &DiceComputations<'_>,
    ev: ConfiguredBuildEvent,
) -> bz_error::Result<()> {
    ctx.per_transaction_data()
        .get_build_event_sink()?
        .consume_configured(ev);
    Ok(())
}

async fn publish_build_signal_edges(
    ctx: &mut DiceComputations<'_>,
    providers_label: &ConfiguredProvidersLabel,
    outputs: &[(ArtifactGroup, BuildProviderType)],
) -> bz_error::Result<()> {
    let Some(signals) = ctx.per_transaction_data().get_build_signals().cloned() else {
        return Ok(());
    };

    let resolved_artifacts: Vec<_> = tokio::task::unconstrained(KeepGoing::try_compute_join_all(
        ctx,
        outputs.iter(),
        |ctx, (output, _type)| async move { output.resolved_artifact(ctx).await }.boxed(),
    ))
    .await?;

    let node_keys = resolved_artifacts
        .iter()
        .filter_map(|resolved| match resolved.dupe() {
            ResolvedArtifactGroup::Artifact(artifact) => artifact
                .action_key()
                .duped()
                .map(crate::actions::calculation::BuildKey)
                .map(ResolvedArtifactGroupBuildSignalsKey::BuildKey),
            ResolvedArtifactGroup::TransitiveSetProjection(key) => Some(
                ResolvedArtifactGroupBuildSignalsKey::EnsureTransitiveSetProjectionKey(
                    EnsureTransitiveSetProjectionKey(key.dupe().dupe()),
                ),
            ),
        })
        .collect();

    signals.top_level_target(providers_label.target().dupe(), node_keys);
    Ok(())
}
