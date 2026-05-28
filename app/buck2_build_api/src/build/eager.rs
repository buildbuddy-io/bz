/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dice::CancellationHandle;
use dice::DiceComputations;
use dice::UserComputationData;
use dupe::Dupe;
use futures::FutureExt;

use crate::analysis::AnalysisResult;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::calculation::ArtifactGroupCalculation;

pub struct EagerBuildExecutionState {
    enabled: AtomicBool,
    scheduled: Mutex<HashSet<ArtifactGroup>>,
    cancellations: Mutex<Vec<CancellationHandle>>,
}

impl EagerBuildExecutionState {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            scheduled: Mutex::new(HashSet::new()),
            cancellations: Mutex::new(Vec::new()),
        }
    }

    fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
    }

    fn cancel(&self) {
        self.enabled.store(false, Ordering::Relaxed);
        let Ok(mut cancellations) = self.cancellations.lock() else {
            return;
        };
        for cancellation in cancellations.drain(..) {
            cancellation.cancel();
        }
    }

    fn claim_inputs(
        &self,
        inputs: impl IntoIterator<Item = ArtifactGroup>,
    ) -> buck2_error::Result<Vec<ArtifactGroup>> {
        if !self.enabled.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }

        let mut scheduled = self
            .scheduled
            .lock()
            .map_err(|_| buck2_error::internal_error!("eager build execution lock poisoned"))?;

        Ok(inputs
            .into_iter()
            .filter(|input| scheduled.insert(input.dupe()))
            .collect())
    }

    fn push_cancellation(&self, cancellation: CancellationHandle) -> buck2_error::Result<()> {
        if !self.enabled.load(Ordering::Relaxed) {
            cancellation.cancel();
            return Ok(());
        }
        self.cancellations
            .lock()
            .map_err(|_| buck2_error::internal_error!("eager build execution lock poisoned"))?
            .push(cancellation);
        Ok(())
    }
}

pub trait HasEagerBuildExecution {
    fn init_eager_build_execution(&mut self);
    fn enable_eager_build_execution(&self) -> buck2_error::Result<()>;
    fn cancel_eager_build_execution(&self);
}

impl HasEagerBuildExecution for UserComputationData {
    fn init_eager_build_execution(&mut self) {
        self.data.set(EagerBuildExecutionState::new());
    }

    fn enable_eager_build_execution(&self) -> buck2_error::Result<()> {
        self.data
            .get::<EagerBuildExecutionState>()
            .map_err(|e| buck2_error::internal_error!("per-transaction data invalid: {}", e))?
            .enable();
        Ok(())
    }

    fn cancel_eager_build_execution(&self) {
        if let Ok(state) = self.data.get::<EagerBuildExecutionState>() {
            state.cancel();
        }
    }
}

fn should_eager_ensure_input(input: &ArtifactGroup) -> bool {
    match input {
        ArtifactGroup::Artifact(artifact) => artifact.action_key().is_some(),
        ArtifactGroup::Promise(_) | ArtifactGroup::TransitiveSetProjection(_) => false,
    }
}

pub fn schedule_eager_inputs_from_analysis(
    ctx: &mut DiceComputations<'_>,
    result: &AnalysisResult,
) -> buck2_error::Result<()> {
    let inputs = {
        let Ok(state) = ctx
            .per_transaction_data()
            .data
            .get::<EagerBuildExecutionState>()
        else {
            return Ok(());
        };

        let mut inputs = Vec::new();
        for action in result.analysis_values().iter_actions() {
            for input in action.action().inputs()?.iter() {
                if should_eager_ensure_input(input) {
                    inputs.push(input.dupe());
                }
            }
        }

        state.claim_inputs(inputs)?
    };
    if inputs.is_empty() {
        return Ok(());
    }

    let cancellation = ctx.spawn_detached(move |ctx, _cancellation| {
        async move {
            let _ignored = ctx
                .compute_join(inputs, |ctx, input| {
                    async move {
                        let _ignored = ctx.ensure_artifact_group(&input).await;
                    }
                    .boxed()
                })
                .await;
        }
        .boxed()
    });

    let state = ctx
        .per_transaction_data()
        .data
        .get::<EagerBuildExecutionState>()
        .map_err(|e| buck2_error::internal_error!("per-transaction data invalid: {}", e))?;
    state.push_cancellation(cancellation)
}
