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
use async_trait::async_trait;
use buck2_common::liveliness_observer::LivelinessObserver;
use buck2_core::configuration::compatibility::MaybeCompatible;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_error::BuckErrorContext;
use buck2_events::dispatch::console_message;
use buck2_node::nodes::configured_frontend::ConfiguredTargetNodeCalculation;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::Key;
use dice::LinearRecomputeDiceComputations;
use dice::NoValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use dupe::IterDupedExt;
use dupe::OptionDupedExt;
use futures::FutureExt;
use futures::future::Either;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::analysis::calculation::RuleAnalysisCalculation;
use crate::artifact_groups::ResolvedArtifactGroup;
use crate::artifact_groups::ResolvedArtifactGroupBuildSignalsKey;
use crate::artifact_groups::calculation::EnsureTransitiveSetProjectionKey;
use crate::build::BuildConfiguredLabelOptions;
use crate::build::BuildEventConsumer;
use crate::build::BuildProviderType;
use crate::build::ConfiguredBuildEvent;
use crate::build::ConfiguredBuildEventExecutionVariant;
use crate::build::ConfiguredBuildEventVariant;
use crate::build::ProviderArtifacts;
use crate::build::ProvidersToBuild;
use crate::build::detailed_aggregated_metrics::dice::HasDetailedAggregatedMetrics;
use crate::build::detailed_aggregated_metrics::types::TopLevelTargetSpec;
use crate::build::graph_properties;
use crate::build::graph_properties::GraphPropertiesOptions;
use crate::build::graph_properties::GraphPropertiesValues;
use crate::build::outputs::get_outputs_for_top_level_target;
use crate::build_signals::HasBuildSignals;
use crate::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValue;
use crate::keep_going::KeepGoing;
use crate::materialize::HasMaterializationQueueTracker;
use crate::materialize::MaterializationAndUploadContext;
use crate::materialize::materialize_and_upload_artifact_group;
use crate::validation::validation_impl::VALIDATION_IMPL;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct BuildDriverKey {
    providers_label: ConfiguredProvidersLabel,
    providers_to_build: ProvidersToBuild,
    materialization_and_upload: MaterializationAndUploadContext,
    graph_properties: GraphPropertiesOptions,
}

impl BuildDriverKey {
    pub fn new(
        providers_label: ConfiguredProvidersLabel,
        providers_to_build: ProvidersToBuild,
        materialization_and_upload: MaterializationAndUploadContext,
        graph_properties: GraphPropertiesOptions,
    ) -> Self {
        Self {
            providers_label,
            providers_to_build,
            materialization_and_upload,
            graph_properties,
        }
    }
}

impl std::fmt::Display for BuildDriverKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BUILD_DRIVER({}, {:?})",
            self.providers_label, self.providers_to_build
        )
    }
}

#[derive(Allocative)]
pub struct BuildDriverValue {
    pub provider_collection: Option<FrozenProviderCollectionValue>,
    pub target_rule_type_name: String,
    pub outputs: Vec<(usize, buck2_error::Result<ProviderArtifacts>)>,
    pub validation_result: buck2_error::Result<()>,
    pub graph_properties: Option<buck2_error::Result<MaybeCompatible<GraphPropertiesValues>>>,
}

#[async_trait]
impl Key for BuildDriverKey {
    type Value = buck2_error::Result<MaybeCompatible<Arc<BuildDriverValue>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        compute_build_driver(ctx, self)
            .await
            .with_buck_error_context(|| {
                format!(
                    "Error building `{}` through the build driver",
                    self.providers_label
                )
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

async fn compute_build_driver(
    ctx: &mut DiceComputations<'_>,
    key: &BuildDriverKey,
) -> buck2_error::Result<MaybeCompatible<Arc<BuildDriverValue>>> {
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
        let providers = ctx
            .get_providers(&key.providers_label)
            .await?
            .require_compatible()?;
        Some(providers)
    } else {
        None
    };

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

    let (outputs, validation_result, graph_properties) = ctx
        .compute3(
            |ctx| {
                async move {
                    ctx.compute_join(
                        output_items,
                        |ctx: &mut DiceComputations<'_>, (index, output, provider_type)| {
                            async move {
                                let queue_tracker = ctx
                                    .per_transaction_data()
                                    .get_materialization_queue_tracker();
                                let output = materialize_and_upload_artifact_group(
                                    ctx,
                                    &output,
                                    materialization_and_upload,
                                    &queue_tracker,
                                )
                                .await
                                .map(|values| ProviderArtifacts {
                                    values,
                                    provider_type,
                                });
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
                async move {
                    validation_impl
                        .validate_target_node_transitively(ctx, target)
                        .await
                }
                .boxed()
            },
            |ctx| {
                async move {
                    if graph_properties.is_empty() {
                        None
                    } else {
                        Some(
                            graph_properties::get_graph_properties(
                                ctx,
                                key.providers_label.target(),
                                graph_properties.should_compute_configured_graph_sketch(),
                                graph_properties.retained_analysis_memory_sketch,
                            )
                            .await
                            .ok(),
                        )
                    }
                }
                .boxed()
            },
        )
        .await;

    Ok(MaybeCompatible::Compatible(Arc::new(BuildDriverValue {
        provider_collection,
        target_rule_type_name,
        outputs,
        validation_result,
        graph_properties,
    })))
}

async fn publish_build_signal_edges(
    ctx: &mut DiceComputations<'_>,
    providers_label: &ConfiguredProvidersLabel,
    outputs: &[(crate::artifact_groups::ArtifactGroup, BuildProviderType)],
) -> buck2_error::Result<()> {
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

pub async fn build_configured_label(
    event_consumer: &dyn BuildEventConsumer,
    ctx: &LinearRecomputeDiceComputations<'_>,
    materialization_and_upload: MaterializationAndUploadContext,
    providers_label: ConfiguredProvidersLabel,
    providers_to_build: &ProvidersToBuild,
    opts: BuildConfiguredLabelOptions,
    timeout_observer: Option<&Arc<dyn LivelinessObserver>>,
) {
    let key = BuildDriverKey::new(
        providers_label.dupe(),
        providers_to_build.clone(),
        materialization_and_upload,
        opts.graph_properties,
    );

    let build = async {
        let value = ctx.get().compute(&key).await?;
        buck2_error::Result::<_>::Ok(value)
    };

    let value = match timeout_observer {
        Some(timeout_observer) => {
            let alive = timeout_observer
                .while_alive()
                .map(|()| ConfiguredBuildEventVariant::Timeout);
            futures::pin_mut!(alive);
            futures::pin_mut!(build);
            match futures::future::select(alive, build).await {
                Either::Left((timeout, _build)) => {
                    event_consumer.consume_configured(ConfiguredBuildEvent {
                        label: providers_label,
                        variant: timeout,
                    });
                    return;
                }
                Either::Right((value, _alive)) => value,
            }
        }
        None => build.await,
    };

    let value = match value {
        Ok(value) => value,
        Err(e) => {
            event_consumer.consume_configured(ConfiguredBuildEvent {
                label: providers_label,
                variant: ConfiguredBuildEventVariant::Error { err: e },
            });
            return;
        }
    };

    let value = match value {
        Ok(MaybeCompatible::Compatible(value)) => value,
        Ok(MaybeCompatible::Incompatible(reason)) => {
            let variant = if opts.skippable {
                ConfiguredBuildEventVariant::SkippedIncompatible
            } else {
                ConfiguredBuildEventVariant::Error {
                    err: reason.to_err(),
                }
            };
            event_consumer.consume_configured(ConfiguredBuildEvent {
                label: providers_label,
                variant,
            });
            return;
        }
        Err(e) => {
            event_consumer.consume_configured(ConfiguredBuildEvent {
                label: providers_label,
                variant: ConfiguredBuildEventVariant::Error { err: e },
            });
            return;
        }
    };

    if !opts.skippable && value.outputs.is_empty() {
        let docs = "https://buck2.build/docs/users/faq/common_issues/#why-does-my-target-not-have-any-outputs"; // @oss-enable
        // @oss-disable: let docs = "https://www.internalfb.com/intern/staticdocs/buck2/docs/users/faq/common_issues/#why-does-my-target-not-have-any-outputs";
        console_message(format!(
            "Target {} does not have any outputs. This means the rule did not define any outputs. See {} for more information",
            providers_label.target(),
            docs,
        ));
    }

    event_consumer.consume_configured(ConfiguredBuildEvent {
        label: providers_label.dupe(),
        variant: ConfiguredBuildEventVariant::Prepared {
            provider_collection: value.provider_collection.clone(),
            target_rule_type_name: value.target_rule_type_name.clone(),
        },
    });

    for (index, output) in &value.outputs {
        event_consumer.consume_configured(ConfiguredBuildEvent {
            label: providers_label.dupe(),
            variant: ConfiguredBuildEventVariant::Execution(
                ConfiguredBuildEventExecutionVariant::BuildOutput {
                    index: *index,
                    output: output.clone(),
                },
            ),
        });
    }

    event_consumer.consume_configured(ConfiguredBuildEvent {
        label: providers_label.dupe(),
        variant: ConfiguredBuildEventVariant::Execution(
            ConfiguredBuildEventExecutionVariant::Validation {
                result: value.validation_result.clone(),
            },
        ),
    });

    if let Some(graph_properties) = &value.graph_properties {
        event_consumer.consume_configured(ConfiguredBuildEvent {
            label: providers_label,
            variant: ConfiguredBuildEventVariant::GraphProperties {
                graph_properties: graph_properties.clone(),
            },
        });
    }
}
