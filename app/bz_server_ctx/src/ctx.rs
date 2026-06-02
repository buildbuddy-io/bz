/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use allocative::Allocative;
use async_trait::async_trait;
use bz_build_signals::env::BuildSignalsContext;
use bz_build_signals::env::DeferredBuildSignals;
use bz_build_signals::env::EarlyCommandTimingBuilder;
use bz_build_signals::env::HasCriticalPathBackend;
use bz_certs::validate::CertState;
use bz_cli_proto::client_context::ExitWhen;
use bz_cli_proto::client_context::PreemptibleWhen;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::pattern::pattern::ParsedPattern;
use bz_core::pattern::pattern::ParsedPatternWithModifiers;
use bz_core::pattern::pattern_type::ConfiguredProvidersPatternExtra;
use bz_data::CommandCriticalEnd;
use bz_data::CommandCriticalStart;
use bz_data::DiceCriticalSectionEnd;
use bz_data::DiceCriticalSectionStart;
use bz_events::dispatch::EventDispatcher;
use bz_execute::materialize::materializer::Materializer;
use bz_fs::paths::file_name::FileName;
use bz_fs::working_dir::AbsWorkingDir;
use bz_hash::StdBuckHashMap;
use bz_wrapper_common::invocation_id::TraceId;
use dice::DiceComputations;
use dice::DiceTransaction;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;

use crate::concurrency::ConcurrencyHandler;
use crate::concurrency::DiceUpdater;
use crate::stderr_output_guard::StderrOutputGuard;

#[derive(Allocative, Debug)]
pub struct PreviousCommandDataInternal {
    pub external_and_local_configs: Vec<bz_data::BuckconfigComponent>,
    pub sanitized_argv: Vec<String>,
    pub trace_id: TraceId,
}

#[derive(Allocative, Debug, Default)]
pub struct PreviousCommandData {
    pub data: Option<PreviousCommandDataInternal>,
}

impl PreviousCommandData {
    pub fn process_current_command(
        &mut self,
        event_dispatcher: EventDispatcher,
        current_external_and_local_configs: Vec<bz_data::BuckconfigComponent>,
        current_sanitized_argv: Vec<String>,
        current_trace: TraceId,
    ) {
        if let Some(PreviousCommandDataInternal {
            external_and_local_configs: external_configs,
            sanitized_argv,
            trace_id,
        }) = self.data.as_ref()
        {
            if *current_external_and_local_configs != *external_configs {
                event_dispatcher.instant_event(bz_data::PreviousCommandWithMismatchedConfig {
                    sanitized_argv: sanitized_argv.clone(),
                    trace_id: trace_id.to_string(),
                });
            }
        }

        self.data = Some(PreviousCommandDataInternal {
            external_and_local_configs: current_external_and_local_configs,
            sanitized_argv: current_sanitized_argv,
            trace_id: current_trace,
        });
    }
}

#[derive(Allocative, Debug, Default)]
pub struct LockedPreviousCommandData {
    pub data: Mutex<PreviousCommandData>,
}
impl LockedPreviousCommandData {
    pub fn new() -> Arc<Self> {
        Arc::new(LockedPreviousCommandData {
            data: Mutex::new(PreviousCommandData { data: None }),
        })
    }
}

#[async_trait]
pub trait ServerCommandContextTrait: Send + Sync {
    fn working_dir(&self) -> &ProjectRelativePath;

    fn working_dir_abs(&self) -> &AbsWorkingDir;

    fn command_name(&self) -> &str;

    fn isolation_prefix(&self) -> &FileName;

    fn project_root(&self) -> &ProjectRoot;

    fn cert_state(&self) -> CertState;

    fn materializer(&self) -> Arc<dyn Materializer>;

    /// exposes the dice for scoped access, but isn't intended to be callable by anyone
    async fn dice_accessor<'a>(
        &'a self,
        private: PrivateStruct,
    ) -> bz_error::Result<DiceAccessor<'a>>;

    fn events(&self) -> &EventDispatcher;

    fn previous_command_data(&self) -> Arc<LockedPreviousCommandData>;

    fn stderr(&self) -> bz_error::Result<StderrOutputGuard<'_>>;

    async fn command_start_event(
        &self,
        data: bz_data::command_start::Data,
    ) -> bz_error::Result<bz_data::CommandStart>;

    async fn request_metadata(&self) -> bz_error::Result<StdBuckHashMap<String, String>>;

    async fn config_metadata(
        &self,
        ctx: &mut DiceComputations<'_>,
    ) -> bz_error::Result<StdBuckHashMap<String, String>>;

    fn log_target_pattern(
        &self,
        providers_patterns: &[ParsedPattern<ConfiguredProvidersPatternExtra>],
    );

    fn log_target_pattern_with_modifiers(
        &self,
        providers_patterns_with_modifiers: &[ParsedPatternWithModifiers<
            ConfiguredProvidersPatternExtra,
        >],
    );

    fn cancellation_context(&self) -> &CancellationContext;

    fn command_start(&self) -> Instant;
}

pub struct PrivateStruct(());

pub struct DiceAccessor<'a> {
    pub dice_handler: Arc<ConcurrencyHandler>,
    pub setup: Box<dyn DiceUpdater + 'a>,
    pub is_nested_invocation: bool,
    pub sanitized_argv: Vec<String>,
    pub preemptible: PreemptibleWhen,
    pub build_signals: Box<dyn DeferredBuildSignals>,
    pub exit_when: ExitWhen,
}

#[async_trait]
pub trait ServerCommandDiceContext {
    async fn with_dice_ctx<'v, F, Fut, R>(&'v self, exec: F) -> bz_error::Result<R>
    where
        F: FnOnce(&'v dyn ServerCommandContextTrait, DiceTransaction) -> Fut + Send,
        Fut: Future<Output = bz_error::Result<R>> + Send,
        R: Send;

    async fn with_dice_ctx_maybe_exclusive<'v, F, Fut, R>(
        &'v self,
        exec: F,
        exclusive_cmd: Option<String>,
    ) -> bz_error::Result<R>
    where
        F: FnOnce(&'v dyn ServerCommandContextTrait, DiceTransaction) -> Fut + Send,
        Fut: Future<Output = bz_error::Result<R>> + Send,
        R: Send;
}

#[async_trait]
impl ServerCommandDiceContext for dyn ServerCommandContextTrait + '_ {
    /// Allows running a section of code that uses the shared DiceTransaction
    async fn with_dice_ctx<'v, F, Fut, R>(&'v self, exec: F) -> bz_error::Result<R>
    where
        F: FnOnce(&'v dyn ServerCommandContextTrait, DiceTransaction) -> Fut + Send,
        Fut: Future<Output = bz_error::Result<R>> + Send,
        R: Send,
    {
        self.with_dice_ctx_maybe_exclusive(exec, None).await
    }

    async fn with_dice_ctx_maybe_exclusive<'v, F, Fut, R>(
        &'v self,
        exec: F,
        exclusive_cmd: Option<String>,
    ) -> bz_error::Result<R>
    where
        F: FnOnce(&'v dyn ServerCommandContextTrait, DiceTransaction) -> Fut + Send,
        Fut: Future<Output = bz_error::Result<R>> + Send,
        R: Send,
    {
        let DiceAccessor {
            dice_handler,
            setup,
            is_nested_invocation,
            sanitized_argv,
            preemptible,
            build_signals,
            exit_when,
        } = self.dice_accessor(PrivateStruct(())).await?;

        let early_command_timing = EarlyCommandTimingBuilder::new(self.command_start());

        let events = self.events().dupe();
        events
            .span_async(DiceCriticalSectionStart {}, async move {
                (
                    dice_handler
                        .enter(
                            self.events().dupe(),
                            &*setup,
                            |mut dice, early_command_timing| async move {
                                let events = self.events().dupe();

                                let request_metadata = self.request_metadata().await?;
                                let config_metadata = self.config_metadata(&mut dice).await?;
                                events
                                    .span_async(
                                        CommandCriticalStart {
                                            metadata: config_metadata.clone(),
                                            dice_version: dice.equality_token().to_string(),
                                        },
                                        async move {
                                            let res = bz_build_signals::env::scope(
                                                build_signals,
                                                self.events().dupe(),
                                                dice.per_transaction_data()
                                                    .get_critical_path_backend(),
                                                BuildSignalsContext {
                                                    command_name: self.command_name().to_owned(),
                                                    metadata: request_metadata
                                                        .into_iter()
                                                        .chain(
                                                            config_metadata.iter().map(|(k, v)| {
                                                                (k.clone(), v.clone())
                                                            }),
                                                        )
                                                        .collect(),
                                                    isolation_prefix: self
                                                        .isolation_prefix()
                                                        .to_owned(),
                                                    early_command_timing: early_command_timing
                                                        .finish_early_command_timing(),
                                                },
                                                || exec(self, dice),
                                            )
                                            .await;

                                            (
                                                res,
                                                CommandCriticalEnd {
                                                    metadata: config_metadata,
                                                },
                                            )
                                        },
                                    )
                                    .await
                            },
                            is_nested_invocation,
                            sanitized_argv,
                            exclusive_cmd,
                            self.cancellation_context(),
                            preemptible,
                            self.previous_command_data(),
                            self.project_root(),
                            exit_when,
                            early_command_timing,
                        )
                        .await,
                    DiceCriticalSectionEnd {},
                )
            })
            .await?
    }
}
