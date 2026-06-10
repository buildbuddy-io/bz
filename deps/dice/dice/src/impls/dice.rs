/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;

use allocative::Allocative;
use dupe::Dupe;

use crate::DiceTransactionUpdater;
use crate::DiceTransactionUpdaterImpl;
use crate::api::cycles::DetectCycles;
use crate::api::data::DiceData;
use crate::api::key::Key;
use crate::api::user_data::UserComputationData;
use crate::impls::core::state::CoreStateHandle;
use crate::impls::core::state::init_state;
use crate::impls::key_index::DiceKeyIndex;
use crate::impls::storage::DiceStorage;
use crate::impls::transaction::TransactionUpdater;
use crate::introspection::graph::GraphIntrospectable;
use crate::metrics::Metrics;

/// An incremental computation engine that executes arbitrary computations that
/// maps `Key`s to values.
#[derive(Allocative)]
pub struct Dice {
    pub(crate) key_index: DiceKeyIndex,
    pub(crate) state_handle: CoreStateHandle,
    pub(crate) global_data: DiceData,
    pub(crate) pagable_storage: Option<DiceStorage>,
}

impl Debug for Dice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dice").finish_non_exhaustive()
    }
}

enum ExistingKeyOfTwoTypes<K1, K2> {
    First(K1),
    Second(K2),
}

pub struct DiceDataBuilder {
    data: DiceData,
    pagable_storage: Option<DiceStorage>,
}

impl DiceDataBuilder {
    pub(crate) fn new() -> Self {
        Self {
            data: DiceData::new(),
            pagable_storage: None,
        }
    }

    pub fn set<K: Send + Sync + 'static>(&mut self, val: K) {
        self.data.set(val);
    }

    /// Configures pagable storage for this DICE instance, enabling
    /// [`Dice::page_out`].
    pub fn set_pagable_storage(&mut self, storage: DiceStorage) {
        self.pagable_storage = Some(storage);
    }

    pub fn build(self, _detect_cycles: DetectCycles) -> Arc<Dice> {
        Dice::new(self.data, self.pagable_storage)
    }
}

impl Dice {
    pub(crate) fn new(global_data: DiceData, pagable_storage: Option<DiceStorage>) -> Arc<Self> {
        let state_handle = init_state();

        Arc::new(Dice {
            key_index: Default::default(),
            state_handle,
            global_data,
            pagable_storage,
        })
    }

    pub fn builder() -> DiceDataBuilder {
        DiceDataBuilder::new()
    }

    pub fn updater(self: &Arc<Self>) -> DiceTransactionUpdater {
        self.updater_with_data(UserComputationData::new())
    }

    pub fn updater_with_data(
        self: &Arc<Self>,
        extra: UserComputationData,
    ) -> DiceTransactionUpdater {
        DiceTransactionUpdater(DiceTransactionUpdaterImpl(TransactionUpdater::new(
            self.dupe(),
            Arc::new(extra),
        )))
    }

    pub fn metrics(&self) -> Metrics {
        self.state_handle.metrics()
    }

    pub(crate) fn existing_keys_of_type_for_introspection<K>(&self) -> Vec<K>
    where
        K: Key + Clone,
    {
        self.state_handle
            .existing_graph_keys()
            .into_iter()
            .filter_map(|key| self.key_index.get_typed_key::<K>(key))
            .collect()
    }

    pub(crate) fn existing_key_values_of_type_for_introspection<K>(
        &self,
    ) -> Vec<(K, Option<K::Value>)>
    where
        K: Key + Clone,
        K::Value: Clone,
    {
        let keys: Vec<_> = self
            .state_handle
            .existing_graph_keys()
            .into_iter()
            .filter_map(|dice_key| {
                self.key_index
                    .get_typed_key::<K>(dice_key)
                    .map(|key| (dice_key, key))
            })
            .collect();
        let values = self
            .state_handle
            .current_graph_values(keys.iter().map(|(dice_key, _)| *dice_key).collect());

        keys.into_iter()
            .zip(values)
            .map(|((_, key), value)| {
                let value =
                    value.and_then(|value| value.downcast_maybe_transient::<K::Value>().cloned());
                (key, value)
            })
            .collect()
    }

    pub(crate) fn existing_key_values_of_two_types_for_introspection<K1, K2>(
        &self,
    ) -> (Vec<(K1, Option<K1::Value>)>, Vec<(K2, Option<K2::Value>)>)
    where
        K1: Key + Clone,
        K1::Value: Clone,
        K2: Key + Clone,
        K2::Value: Clone,
    {
        let keys: Vec<_> = self
            .state_handle
            .existing_graph_keys()
            .into_iter()
            .filter_map(|dice_key| {
                if let Some(key) = self.key_index.get_typed_key::<K1>(dice_key) {
                    Some((dice_key, ExistingKeyOfTwoTypes::First(key)))
                } else {
                    self.key_index
                        .get_typed_key::<K2>(dice_key)
                        .map(|key| (dice_key, ExistingKeyOfTwoTypes::Second(key)))
                }
            })
            .collect();
        let values = self
            .state_handle
            .current_graph_values(keys.iter().map(|(dice_key, _)| *dice_key).collect());

        let mut first = Vec::new();
        let mut second = Vec::new();

        for ((_, key), value) in keys.into_iter().zip(values) {
            match key {
                ExistingKeyOfTwoTypes::First(key) => {
                    let value = value
                        .and_then(|value| value.downcast_maybe_transient::<K1::Value>().cloned());
                    first.push((key, value));
                }
                ExistingKeyOfTwoTypes::Second(key) => {
                    let value = value
                        .and_then(|value| value.downcast_maybe_transient::<K2::Value>().cloned());
                    second.push((key, value));
                }
            }
        }

        (first, second)
    }

    pub fn to_introspectable(&self) -> GraphIntrospectable {
        let (graph_introspectable, version_introspectable) = self.state_handle.introspection();
        // a bit subtle, but make sure we introspect the key_index after we get the graphs as
        // there may still be new keys added and running. A snapshot of `key_index` prior to
        // snapshotting the graphs will result in missing keys
        let key_index = self.key_index.introspect();

        GraphIntrospectable {
            graph: graph_introspectable,
            version_data: version_introspectable,
            key_map: key_index,
        }
    }

    /// Note: modern dice does not support cycle detection yet
    pub fn detect_cycles(&self) -> &DetectCycles {
        // TODO(bobyf) actually have cycles for dice modern
        const CYCLES: DetectCycles = DetectCycles::Disabled;
        &CYCLES
    }

    /// Wait until all active versions have exited.
    pub fn wait_for_idle(&self) -> impl Future<Output = ()> + 'static + use<> {
        let rx = self.state_handle.get_tasks_pending_cancellation();
        async move {
            let tasks = rx.await;
            futures::future::join_all(tasks).await;
        }
    }

    /// true when there are no tasks pending cancellation
    pub async fn is_idle(&self) -> bool {
        let tasks = self.state_handle.get_tasks_pending_cancellation().await;

        tasks.iter().all(|task| task.is_terminated())
    }

    /// Page out every hydrated `OccupiedGraphNode` value to the configured `DiceStorage`.
    ///
    /// **Caller must ensure DICE is idle** before calling this — typically by awaiting
    /// `wait_for_idle()` first. Page-out runs on the dice core thread and blocks all
    /// other state operations until it finishes.
    ///
    /// No-op if `DiceStorage` was not configured on the builder.
    pub async fn page_out(self: &Arc<Self>) -> anyhow::Result<()> {
        if !self.is_idle().await {
            return Err(anyhow::anyhow!(
                "Dice::page_out called while DICE is not idle; call `wait_for_idle()` first"
            ));
        }
        self.state_handle.page_out(self.dupe()).await
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use dupe::Dupe;

    use crate::impls::ctx::SharedLiveTransactionCtx;
    use crate::impls::dice::Dice;
    use crate::impls::transaction::ActiveTransactionGuard;
    use crate::versions::VersionNumber;

    impl Dice {
        pub(crate) async fn testing_shared_ctx(
            &self,
            v: VersionNumber,
        ) -> (SharedLiveTransactionCtx, ActiveTransactionGuard) {
            let guard = ActiveTransactionGuard::new(v, self.state_handle.dupe());
            self.state_handle.ctx_at_version(v, guard).await
        }
    }
}
