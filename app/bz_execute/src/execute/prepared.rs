/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::ops::ControlFlow;
use std::sync::Arc;

use async_trait::async_trait;
use bz_core::execution_types::executor_config::ReGangWorker;
use bz_core::execution_types::executor_config::RemoteExecutorDependency;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use remote_execution as RE;

use crate::artifact_value::ArtifactValue;
use crate::digest_config::DigestConfig;
use crate::execute::action_digest::ActionDigest;
use crate::execute::action_digest_and_blobs::ActionDigestAndBlobs;
use crate::execute::manager::CommandExecutionManager;
use crate::execute::request::CommandExecutionOutput;
use crate::execute::request::CommandExecutionRequest;
use crate::execute::request::ExecutorPreference;
use crate::execute::request::LocalActionCacheKey;
use crate::execute::result::CommandExecutionResult;
use crate::execute::target::CommandExecutionTarget;
use crate::materialize::materializer::RemoteActionCacheOrigin;
use bz_hash::BuckIndexMap;
use bz_hash::BuckIndexSet;

pub struct PreparedAction {
    pub action_and_blobs: ActionDigestAndBlobs,
    pub platform: RE::Platform,
    pub remote_execution_dependencies: Vec<RemoteExecutorDependency>,
    pub re_gang_workers: Vec<ReGangWorker>,
    pub worker_tool_init_action: Option<ActionDigestAndBlobs>,
}

impl PreparedAction {
    pub fn digest(&self) -> ActionDigest {
        self.action_and_blobs.action.dupe()
    }
}

pub struct PreparedCommand<'a, 'b> {
    pub request: &'a CommandExecutionRequest,
    pub target: &'b dyn CommandExecutionTarget,
    pub prepared_action: &'a PreparedAction,
    pub digest_config: DigestConfig,
}

pub struct UnpreparedCommand<'a, 'b> {
    pub target: &'b dyn CommandExecutionTarget,
    pub local_action_cache_key: &'a LocalActionCacheKey,
    pub outputs: &'a BuckIndexSet<CommandExecutionOutput>,
    pub digest_config: DigestConfig,
    pub outputs_declared_by_action: bool,
}

#[async_trait]
pub trait PreparedCommandExecutor: Send + Sync {
    /// Execute a command.
    ///
    /// This intentionally does not return a Result since we want to capture information about the
    /// execution even if there are errors. Any errors can be propagated by converting them
    /// to a result with CommandExecutionManager::error.
    async fn exec_cmd(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> CommandExecutionResult;

    /// Checks if there is any possibility for a command with a given executor preference to
    /// be executed locally.
    fn is_local_execution_possible(&self, executor_preference: ExecutorPreference) -> bool;

    /// Whether this executor is configured for full hybrid execution (racing local and remote).
    fn is_full_hybrid_enabled(&self) -> bool;
}

#[async_trait]
pub trait PreparedCommandOptionalExecutor: Send + Sync {
    /// Take a command and evaluate whether it needs to be actually executed (locally or remotely) or can be skipped.
    /// In the skip case, this should handle all the things that would happen in an actual execution (materialization, output declaration, etc.).
    ///
    /// Given a command, evaluate whether the execution can be skipped. (for example because it is already cached)
    /// If it can be skipped, return a CommandExecutionResult that can be used as if the action was just executed.
    /// Otherwise, return a CommandExecutionManager that can be used to execute the action.
    async fn maybe_execute(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager>;

    async fn maybe_execute_unprepared(
        &self,
        _command: &UnpreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        _cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        ControlFlow::Continue(manager)
    }

    fn insert_unprepared_action_cache_metadata(
        &self,
        _local_action_cache_key: &LocalActionCacheKey,
        _outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
        _remote_cache_origin: Option<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl PreparedCommandOptionalExecutor for Arc<dyn PreparedCommandOptionalExecutor> {
    async fn maybe_execute(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        (**self)
            .maybe_execute(command, manager, cancellations)
            .await
    }

    async fn maybe_execute_unprepared(
        &self,
        command: &UnpreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        (**self)
            .maybe_execute_unprepared(command, manager, cancellations)
            .await
    }

    fn insert_unprepared_action_cache_metadata(
        &self,
        local_action_cache_key: &LocalActionCacheKey,
        outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
        remote_cache_origin: Option<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        (**self).insert_unprepared_action_cache_metadata(
            local_action_cache_key,
            outputs,
            remote_cache_origin,
        )
    }
}

// When we don't want to check a command can be skipped, just use the NoOpCommandOptionalExecutor that always returns the continue case.
pub struct NoOpCommandOptionalExecutor {}

#[async_trait]
impl PreparedCommandOptionalExecutor for NoOpCommandOptionalExecutor {
    async fn maybe_execute(
        &self,
        _command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        _cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        ControlFlow::Continue(manager)
    }
}
