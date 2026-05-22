/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::time::Instant;

use allocative::Allocative;
use buck2_common::liveliness_observer::LivelinessObserver;
use buck2_core::configuration::compatibility::IncompatiblePlatformReason;
use buck2_core::configuration::compatibility::MaybeCompatible;
use buck2_core::pattern::pattern::Modifiers;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
use buck2_error::internal_error;
use buck2_events::dispatch::console_message;
use dice::LinearRecomputeDiceComputations;
use dice::UserComputationData;
use dupe::Dupe;
use itertools::Itertools;
use pagable::Pagable;
use starlark::collections::SmallSet;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

use crate::artifact_groups::ArtifactGroupValues;
use crate::build::graph_properties::GraphPropertiesOptions;
use crate::build::graph_properties::GraphPropertiesValues;
use crate::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValue;
use crate::materialize::MaterializationAndUploadContext;

mod action_error;
pub mod build_report;
mod completion;
pub mod detailed_aggregated_metrics;
mod driver;
pub mod graph_properties;
pub mod outputs;
pub(crate) mod sketch_impl;

pub use driver::BuildDriverKey;

/// The types of provider to build on the configured providers label
#[derive(Debug, Clone, Dupe, Copy, Allocative, PartialEq, Pagable)]
pub enum BuildProviderType {
    Default,
    DefaultOther,
    Run,
    Test,
}

/// An output or error paired with the wall-clock elapsed time from build start
/// at which it was produced.
#[derive(Clone, Debug)]
pub struct Timed<T> {
    pub inner: T,
    pub elapsed: Duration,
}

// Duration has no heap allocations, so Allocative is trivially empty.
impl<T: Allocative> Allocative for Timed<T> {
    fn visit<'a, 'b: 'a>(&self, visitor: &'a mut allocative::Visitor<'b>) {
        self.inner.visit(visitor);
    }
}

#[derive(Clone, Debug, Allocative)]
pub struct ConfiguredBuildTargetResultGen<T> {
    pub outputs: Vec<Timed<T>>,
    pub provider_collection: Option<FrozenProviderCollectionValue>,
    pub target_rule_type_name: Option<String>,
    pub graph_properties: Option<buck2_error::Result<MaybeCompatible<GraphPropertiesValues>>>,
    pub errors: Vec<Timed<buck2_error::Error>>,
}

impl<T> ConfiguredBuildTargetResultGen<T> {
    /// Wall-clock time from build start at which this target completed (or
    /// timed out), defined as the max elapsed time across all outputs and
    /// errors.
    pub fn wall_clock_completion(&self) -> Option<Duration> {
        self.outputs
            .iter()
            .map(|o| o.elapsed)
            .chain(self.errors.iter().map(|e| e.elapsed))
            .max()
    }
}

pub type ConfiguredBuildTargetResult =
    ConfiguredBuildTargetResultGen<buck2_error::Result<ProviderArtifacts>>;

pub enum FailFastState {
    Continue,
    Breakpoint,
}

pub struct AsyncBuildTargetResultBuilder {
    event_rx: UnboundedReceiver<BuildEvent>,
    builder: BuildTargetResultBuilder,
}

impl AsyncBuildTargetResultBuilder {
    pub fn new(
        mut streaming_build_result_tx: Option<UnboundedSender<BuildTargetResult>>,
        build_start: Instant,
    ) -> (Self, BuildEventSink) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

        (
            Self {
                event_rx,
                builder: BuildTargetResultBuilder::new(
                    streaming_build_result_tx.take(),
                    build_start,
                ),
            },
            BuildEventSink { event_tx },
        )
    }

    pub async fn wait_for(
        mut self,
        fail_fast: bool,
        fut: impl Future<Output = ()>,
    ) -> buck2_error::Result<BuildTargetResult> {
        let mut fut = pin!(fut);
        loop {
            tokio::select! {
                event = self.event_rx.recv() => {
                    match event {
                        Some(event) => {
                            if let FailFastState::Breakpoint = self.builder.event(event)? {
                                if fail_fast {
                                    break;
                                }
                            }
                        }
                        None => {
                            // Intentionally don't break early in this case.
                            // The None indicates that the event sender has been dropped, but a caller is going to expect
                            // the future to be driven to completion except for in the fail_fast case.
                        }
                    }
                }
                _ = &mut fut => {
                    // The future is done, but make sure to drain the queue of events.
                    // Unlike poll_recv, try_recv never spuriously returns empty.
                    while let Ok(event) = self.event_rx.try_recv() {
                        self.builder.event(event)?;
                    }
                    break;
                }
            }
        }

        Ok(self.builder.build())
    }
}

pub struct BuildTargetResultBuilder {
    res: HashMap<
        ConfiguredProvidersLabel,
        Option<ConfiguredBuildTargetResultGen<(usize, buck2_error::Result<ProviderArtifacts>)>>,
    >,
    configured_to_pattern_modifiers: HashMap<ConfiguredProvidersLabel, Vec<Modifiers>>,
    other_errors: BTreeMap<Option<ProvidersLabel>, Vec<buck2_error::Error>>,
    build_failed: bool,
    incompatible_targets: SmallSet<ConfiguredTargetLabel>,
    streaming_build_result_tx: Option<UnboundedSender<BuildTargetResult>>,
    build_start: Instant,
}

impl BuildTargetResultBuilder {
    pub fn new(
        mut streaming_build_result_tx: Option<UnboundedSender<BuildTargetResult>>,
        build_start: Instant,
    ) -> Self {
        Self {
            res: HashMap::new(),
            configured_to_pattern_modifiers: HashMap::new(),
            other_errors: BTreeMap::new(),
            incompatible_targets: SmallSet::new(),
            build_failed: false,
            streaming_build_result_tx: streaming_build_result_tx.take(),
            build_start,
        }
    }

    pub fn event(&mut self, event: BuildEvent) -> buck2_error::Result<FailFastState> {
        let ConfiguredBuildEvent { variant, label } = match event {
            BuildEvent::Configured(variant) => variant,
            BuildEvent::OtherError { label: target, err } => {
                self.other_errors.entry(target).or_default().push(err);
                self.build_failed = true;
                // TODO(cjhopman): Why don't we break here?
                return Ok(FailFastState::Continue);
            }
        };
        let elapsed = Instant::now() - self.build_start;
        match variant {
            ConfiguredBuildEventVariant::SkippedIncompatible => {
                self.incompatible_targets.insert(label.target().dupe());
                self.res.entry(label.dupe()).or_insert(None);
            }
            ConfiguredBuildEventVariant::MapModifiers { modifiers } => {
                self.configured_to_pattern_modifiers
                    .entry(label.dupe())
                    .or_default()
                    .push(modifiers);
            }
            ConfiguredBuildEventVariant::Prepared {
                provider_collection,
                target_rule_type_name,
            } => {
                self.res
                    .entry(label.dupe())
                    .or_insert(Some(ConfiguredBuildTargetResultGen {
                        outputs: Vec::new(),
                        provider_collection,
                        target_rule_type_name: Some(target_rule_type_name),
                        graph_properties: None,
                        errors: Vec::new(),
                    }));
            }
            ConfiguredBuildEventVariant::Execution(execution_variant) => {
                let is_err = {
                    let results = self.res.get_mut(&label)
                        .ok_or_else(|| internal_error!("ConfiguredBuildEventVariant::Execution before ConfiguredBuildEventVariant::Prepared for {label}"))?
                        .as_mut()
                        .ok_or_else(|| internal_error!("ConfiguredBuildEventVariant::Execution for a skipped target: `{label}`"))?;
                    match execution_variant {
                        ConfiguredBuildEventExecutionVariant::Validation { result } => {
                            if let Err(e) = result {
                                results.errors.push(Timed { inner: e, elapsed });
                                true
                            } else {
                                false
                            }
                        }
                        ConfiguredBuildEventExecutionVariant::BuildOutput { index, output } => {
                            let is_err = output.is_err();
                            results.outputs.push(Timed {
                                inner: (index, output),
                                elapsed,
                            });
                            // update the streaming build result
                            if let Some(tx) = &self.streaming_build_result_tx.clone() {
                                let result = self.build();
                                let _ignored = tx.send(result);
                            }

                            is_err
                        }
                    }
                };
                if is_err {
                    self.build_failed = true;
                    return Ok(FailFastState::Breakpoint);
                }
            }
            ConfiguredBuildEventVariant::GraphProperties { graph_properties } => {
                self.res.get_mut(&label)
                     .ok_or_else(|| internal_error!("ConfiguredBuildEventVariant::GraphProperties before ConfiguredBuildEventVariant::Prepared for {label}"))?
                     .as_mut()
                     .ok_or_else(|| internal_error!("ConfiguredBuildEventVariant::GraphProperties for a skipped target: `{label}`"))?
                     .graph_properties = Some(graph_properties);
            }
            ConfiguredBuildEventVariant::Timeout => {
                let results = self.res.get_mut(&label)
                     .ok_or_else(|| internal_error!("ConfiguredBuildEventVariant::Timeout before ConfiguredBuildEventVariant::Prepared for {label}"))?
                     .as_mut()
                     .ok_or_else(|| internal_error!("ConfiguredBuildEventVariant::Timeout for a skipped target: `{label}`"))?;
                results.errors.push(Timed {
                    inner: buck2_error::Error::from(BuildDeadlineExpired),
                    elapsed,
                });
                // TODO(cjhopman): Why don't we break here?
                self.build_failed = true;
            }
            ConfiguredBuildEventVariant::Error { err } => {
                self.build_failed = true;
                self.res
                    .entry(label.dupe())
                    .or_insert(Some(ConfiguredBuildTargetResultGen {
                        outputs: Vec::new(),
                        provider_collection: None,
                        target_rule_type_name: None,
                        graph_properties: None,
                        errors: Vec::new(),
                    }))
                    .as_mut()
                    .unwrap()
                    .errors
                    .push(Timed {
                        inner: err,
                        elapsed,
                    });
                return Ok(FailFastState::Breakpoint);
            }
        }
        Ok(FailFastState::Continue)
    }

    pub fn build(&self) -> BuildTargetResult {
        // This function can be called several times during a build in order to produce
        // intermediary/streaming build reports as well as the final build report.
        // It intentionally does not consume self and copies the arrays in the return object.

        if !self.incompatible_targets.is_empty() {
            // TODO(cjhopman): Probably better to return this in the result and let the caller decide what to do with it.
            console_message(IncompatiblePlatformReason::skipping_message_for_multiple(
                &self.incompatible_targets,
            ));
        }

        // Sort our outputs within each individual BuildTargetResult, then return those.
        // Also, turn our HashMap into a BTreeMap.
        let res = self
            .res
            .iter()
            .map(|(label, result)| {
                let result = result.as_ref().map(|result| {
                    // TODO: This whole building thing needs quite a bit of
                    // refactoring. We might request the same targets multiple
                    // times here, but since we know that ConfiguredTargetLabel
                    // -> Output is going to be deterministic, we just dedupe
                    // them using the index, keeping the min elapsed time (this
                    // is somewhat arbitrary but the outputs are all secretly
                    // the "same" output anyway, and keeping the min elapsed
                    // time ensures we don't report a time then update it to
                    // "later" in another call).
                    let mut indexed: Vec<_> = result
                        .outputs
                        .iter()
                        .map(|timed| {
                            let (index, output) = &timed.inner;
                            (*index, output.clone(), timed.elapsed)
                        })
                        .collect();
                    indexed.sort_unstable_by_key(|(index, _, _)| *index);

                    let outputs: Vec<_> = indexed
                        .into_iter()
                        .chunk_by(|(index, _, _)| *index)
                        .into_iter()
                        .map(|(_index, group)| {
                            let (_, output, elapsed) =
                                group.min_by_key(|(_, _, elapsed)| *elapsed).unwrap();
                            Timed {
                                inner: output.clone(),
                                elapsed,
                            }
                        })
                        .collect();

                    ConfiguredBuildTargetResult {
                        outputs,
                        provider_collection: result.provider_collection.clone(),
                        target_rule_type_name: result.target_rule_type_name.clone(),
                        graph_properties: result.graph_properties.clone(),
                        errors: result.errors.clone(),
                    }
                });

                (label.clone(), result)
            })
            .collect();

        let configured_to_pattern_modifiers = self
            .configured_to_pattern_modifiers
            .iter()
            .map(|(label, modifiers)| {
                (
                    label.clone(),
                    BTreeSet::from_iter(modifiers.iter().cloned()),
                )
            })
            .collect();

        BuildTargetResult {
            configured: res,
            configured_to_pattern_modifiers,
            other_errors: self.other_errors.clone(),
            build_failed: self.build_failed,
        }
    }
}

pub struct BuildTargetResult {
    pub configured: BTreeMap<ConfiguredProvidersLabel, Option<ConfiguredBuildTargetResult>>,
    pub configured_to_pattern_modifiers: HashMap<ConfiguredProvidersLabel, BTreeSet<Modifiers>>,
    /// Errors that could not be associated with a specific configured target. These errors may be
    /// associated with a providers label, or might not be associated with any target at all.
    pub other_errors: BTreeMap<Option<ProvidersLabel>, Vec<buck2_error::Error>>,
    pub build_failed: bool,
}

impl BuildTargetResult {
    pub fn new() -> Self {
        Self {
            configured: BTreeMap::new(),
            configured_to_pattern_modifiers: HashMap::new(),
            other_errors: BTreeMap::new(),
            build_failed: false,
        }
    }

    pub fn extend(&mut self, other: BuildTargetResult) {
        self.configured.extend(other.configured);
        self.other_errors.extend(other.other_errors);

        for (label, modifiers_set) in other.configured_to_pattern_modifiers {
            self.configured_to_pattern_modifiers
                .entry(label)
                .or_default()
                .extend(modifiers_set);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.configured.is_empty() && self.other_errors.is_empty()
    }
}

pub enum ConfiguredBuildEventExecutionVariant {
    BuildOutput {
        output: buck2_error::Result<ProviderArtifacts>,
        /// Ensure a stable ordering of outputs.
        index: usize,
    },
    Validation {
        result: buck2_error::Result<()>,
    },
}

pub enum ConfiguredBuildEventVariant {
    SkippedIncompatible,
    MapModifiers {
        modifiers: Modifiers,
    },
    Prepared {
        provider_collection: Option<FrozenProviderCollectionValue>,
        target_rule_type_name: String,
    },
    Execution(ConfiguredBuildEventExecutionVariant),
    GraphProperties {
        graph_properties: buck2_error::Result<MaybeCompatible<GraphPropertiesValues>>,
    },
    Error {
        /// An error that can't be associated with a single artifact.
        err: buck2_error::Error,
    },
    // This target did not build within the allocated time.
    Timeout,
}

/// Events to be accumulated using BuildTargetResult::collect_stream.
pub struct ConfiguredBuildEvent {
    label: ConfiguredProvidersLabel,
    variant: ConfiguredBuildEventVariant,
}

pub enum BuildEvent {
    Configured(ConfiguredBuildEvent),
    // An error that cannot be associated with a specific configured target
    OtherError {
        label: Option<ProvidersLabel>,
        err: buck2_error::Error,
    },
}

impl BuildEvent {
    pub fn new_configured(
        label: ConfiguredProvidersLabel,
        variant: ConfiguredBuildEventVariant,
    ) -> Self {
        Self::Configured(ConfiguredBuildEvent { label, variant })
    }
}

pub trait BuildEventConsumer: Sync {
    fn consume(&self, ev: BuildEvent);
    fn consume_configured(&self, ev: ConfiguredBuildEvent) {
        self.consume(BuildEvent::Configured(ev))
    }
}

#[derive(Clone)]
pub struct BuildEventSink {
    event_tx: UnboundedSender<BuildEvent>,
}

impl BuildEventConsumer for BuildEventSink {
    fn consume(&self, ev: BuildEvent) {
        let _ignored = self.event_tx.send(ev);
    }
}

struct BuildEventSinkHolder {
    sink: StdMutex<Option<BuildEventSink>>,
}

impl BuildEventSinkHolder {
    fn new() -> Self {
        Self {
            sink: StdMutex::new(None),
        }
    }
}

pub trait HasBuildEventSink {
    fn init_build_event_sink(&mut self);
    fn set_build_event_sink(&self, sink: BuildEventSink) -> buck2_error::Result<()>;
    fn get_build_event_sink(&self) -> buck2_error::Result<BuildEventSink>;
}

impl HasBuildEventSink for UserComputationData {
    fn init_build_event_sink(&mut self) {
        self.data.set(BuildEventSinkHolder::new());
    }

    fn set_build_event_sink(&self, sink: BuildEventSink) -> buck2_error::Result<()> {
        let holder = self
            .data
            .get::<BuildEventSinkHolder>()
            .map_err(|e| internal_error!("per-transaction data invalid: {}", e))?;
        *holder
            .sink
            .lock()
            .map_err(|_| internal_error!("build event sink lock poisoned"))? = Some(sink);
        Ok(())
    }

    fn get_build_event_sink(&self) -> buck2_error::Result<BuildEventSink> {
        let holder = self
            .data
            .get::<BuildEventSinkHolder>()
            .map_err(|e| internal_error!("per-transaction data invalid: {}", e))?;
        holder
            .sink
            .lock()
            .map_err(|_| internal_error!("build event sink lock poisoned"))?
            .clone()
            .ok_or_else(|| internal_error!("build event sink was not installed"))
    }
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = BuildDeadlineExpired)]
#[error("Build timed out")]
struct BuildDeadlineExpired;

#[derive(Copy, Clone, Dupe, Debug)]
pub struct BuildConfiguredLabelOptions {
    pub skippable: bool,
    pub graph_properties: GraphPropertiesOptions,
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
    driver::build_configured_label(
        event_consumer,
        ctx,
        materialization_and_upload,
        providers_label,
        providers_to_build,
        opts,
        timeout_observer,
    )
    .await;
}

#[derive(Clone, Allocative)]
pub struct ProviderArtifacts {
    pub values: ArtifactGroupValues,
    pub provider_type: BuildProviderType,
}

// what type of artifacts to build based on the provider it came from
#[derive(Default, Allocative, Debug, Clone, Dupe, Eq, PartialEq, Hash, Pagable)]
pub struct ProvidersToBuild {
    pub default: bool,
    pub default_other: bool,
    pub run: bool,
    pub tests: bool,
}

impl Debug for ProviderArtifacts {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderArtifacts")
            .field("values", &self.values.iter().collect::<Vec<_>>())
            .field("provider_type", &self.provider_type)
            .finish()
    }
}

pub trait HasCreateUnhashedSymlinkLock {
    fn set_create_unhashed_symlink_lock(&mut self, lock: Arc<Mutex<()>>);

    fn get_create_unhashed_symlink_lock(&self) -> Arc<Mutex<()>>;
}

impl HasCreateUnhashedSymlinkLock for UserComputationData {
    fn set_create_unhashed_symlink_lock(&mut self, lock: Arc<Mutex<()>>) {
        self.data.set(lock);
    }

    fn get_create_unhashed_symlink_lock(&self) -> Arc<Mutex<()>> {
        self.data
            .get::<Arc<Mutex<()>>>()
            .expect("Lock for creating unhashed symlinks should be set")
            .dupe()
    }
}
