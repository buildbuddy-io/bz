use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use bz_common::liveliness_observer::LivelinessObserver;
use bz_core::configuration::compatibility::MaybeCompatible;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_error::BuckErrorContext;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::Key;
use dice::LinearRecomputeDiceComputations;
use dice::NoValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use futures::FutureExt;
use futures::future::Either;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::build::BuildConfiguredLabelOptions;
use crate::build::BuildEventConsumer;
use crate::build::BuildEventSink;
use crate::build::ConfiguredBuildEvent;
use crate::build::ConfiguredBuildEventVariant;
use crate::build::HasBuildEventSink;
use crate::build::ProvidersToBuild;
use crate::build::completion::TargetCompletionKey;
use crate::build::completion::emit_configured_build_event;
use crate::build::eager::HasEagerBuildExecution;
use crate::build::graph_properties::GraphPropertiesOptions;
use crate::materialize::MaterializationAndUploadContext;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct BuildDriverKey {
    providers_label: ConfiguredProvidersLabel,
    providers_to_build: ProvidersToBuild,
    materialization_and_upload: MaterializationAndUploadContext,
    graph_properties: GraphPropertiesOptions,
    skippable: bool,
}

impl BuildDriverKey {
    pub fn new(
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

impl std::fmt::Display for BuildDriverKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BUILD_DRIVER({}, {:?})",
            self.providers_label, self.providers_to_build
        )
    }
}

#[async_trait]
impl Key for BuildDriverKey {
    type Value = bz_error::Result<()>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let result = compute_build_driver(ctx, self)
            .await
            .with_buck_error_context(|| {
                format!(
                    "Error building `{}` through the build driver",
                    self.providers_label
                )
            });

        if let Err(e) = &result {
            emit_configured_build_event(
                ctx,
                ConfiguredBuildEvent {
                    label: self.providers_label.dupe(),
                    variant: ConfiguredBuildEventVariant::Error { err: e.dupe() },
                },
            )?;
        }

        result
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
) -> bz_error::Result<()> {
    let completion_key = TargetCompletionKey::new(
        key.providers_label.dupe(),
        key.providers_to_build.clone(),
        key.materialization_and_upload,
        key.graph_properties,
        key.skippable,
    );

    ctx.per_transaction_data().enable_eager_build_execution()?;
    let completion = ctx.compute(&completion_key).await;
    ctx.per_transaction_data().cancel_eager_build_execution();

    match completion?? {
        MaybeCompatible::Compatible(_) => Ok(()),
        MaybeCompatible::Incompatible(reason) => {
            let variant = if key.skippable {
                ConfiguredBuildEventVariant::SkippedIncompatible
            } else {
                ConfiguredBuildEventVariant::Error {
                    err: reason.to_err(),
                }
            };
            emit_configured_build_event(
                ctx,
                ConfiguredBuildEvent {
                    label: key.providers_label.dupe(),
                    variant,
                },
            )?;
            Ok(())
        }
    }
}

pub async fn build_configured_label(
    event_consumer: &BuildEventSink,
    ctx: &LinearRecomputeDiceComputations<'_>,
    materialization_and_upload: MaterializationAndUploadContext,
    providers_label: ConfiguredProvidersLabel,
    providers_to_build: &ProvidersToBuild,
    opts: BuildConfiguredLabelOptions,
    timeout_observer: Option<&Arc<dyn LivelinessObserver>>,
) {
    if let Err(e) = ctx
        .get()
        .per_transaction_data()
        .set_build_event_sink(event_consumer.clone())
    {
        event_consumer.consume_configured(ConfiguredBuildEvent {
            label: providers_label,
            variant: ConfiguredBuildEventVariant::Error { err: e },
        });
        return;
    }

    let key = BuildDriverKey::new(
        providers_label.dupe(),
        providers_to_build.clone(),
        materialization_and_upload,
        opts.graph_properties,
        opts.skippable,
    );

    let build = async { ctx.get().compute(&key).await };
    let error_label = providers_label.dupe();

    match timeout_observer {
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
                }
                Either::Right((Err(e), _alive)) => {
                    event_consumer.consume_configured(ConfiguredBuildEvent {
                        label: error_label,
                        variant: ConfiguredBuildEventVariant::Error { err: e.into() },
                    });
                }
                Either::Right((Ok(_), _alive)) => {}
            }
        }
        None => {
            if let Err(e) = build.await {
                event_consumer.consume_configured(ConfiguredBuildEvent {
                    label: error_label,
                    variant: ConfiguredBuildEventVariant::Error { err: e.into() },
                });
            }
        }
    }
}
