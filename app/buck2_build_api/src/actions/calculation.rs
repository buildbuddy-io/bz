/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::iter::zip;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_artifact::actions::key::ActionKey;
use buck2_artifact::artifact::artifact_type::BaseArtifactKind;
use buck2_artifact::artifact::build_artifact::BuildArtifact;
use buck2_build_signals::env::NodeDuration;
use buck2_build_signals::env::WaitingData;
use buck2_common::cas_digest::CasDigestData;
use buck2_common::cas_digest::DataDigester;
use buck2_common::events::HasEvents;
use buck2_core::deferred::base_deferred_key::BaseDeferredKey;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
use buck2_data::ActionErrorDiagnostics;
use buck2_data::ActionSubErrors;
use buck2_data::ToProtoMessage;
use buck2_data::get_action_digest;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use buck2_event_observer::action_util::get_execution_time_ms;
use buck2_events::dispatch::async_record_root_spans;
use buck2_events::dispatch::get_dispatcher;
use buck2_events::dispatch::span_async;
use buck2_events::span::SpanId;
use buck2_execute::artifact::artifact_dyn::ArtifactDyn;
use buck2_execute::artifact::group::artifact_group_values_dyn::ArtifactGroupValuesDyn;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_execute::execute::kind::CommandExecutionKind;
use buck2_execute::execute::output::CommandStdStreams;
use buck2_execute::execute::result::CommandExecutionReport;
use buck2_execute::execute::result::CommandExecutionStatus;
use buck2_execute::output_size::OutputSize;
use buck2_hash::BuckIndexMap;
use buck2_interpreter::print_handler::EventDispatcherPrintHandler;
use buck2_interpreter::soft_error::Buck2StarlarkSoftErrorHandler;
use buck2_node::nodes::configured_frontend::ConfiguredTargetNodeCalculation;
use buck2_util::time_span::TimeSpan;
use buck2_util::time_span::TimeSpanBuilder;
use derive_more::Display;
use dice::DiceComputations;
use dice::DiceTrackedInvalidationPath;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::{self};
use pagable::Pagable;
use pagable::PagablePanic;
use pagable::pagable_typetag;
use ref_cast::RefCast;
use smallvec::SmallVec;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use tracing::debug;

use crate::actions::RegisteredAction;
use crate::actions::artifact::get_artifact_fs::GetArtifactFs;
use crate::actions::error::ActionError;
use crate::actions::error_handler::ActionErrorHandlerError;
use crate::actions::error_handler::ActionSubErrorResult;
use crate::actions::error_handler::StarlarkActionErrorContext;
use crate::actions::execute::action_executor::ActionExecutionKind;
use crate::actions::execute::action_executor::ActionExecutionMetadata;
use crate::actions::execute::action_executor::ActionOutputs;
use crate::actions::execute::action_executor::BuckActionExecutor;
use crate::actions::execute::action_executor::HasActionExecutor;
use crate::actions::execute::error::ExecuteError;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::ArtifactGroupValues;
use crate::artifact_groups::calculation::ArtifactGroupCalculation;
use crate::build::detailed_aggregated_metrics::dice::HasDetailedAggregatedMetrics;
use crate::build::detailed_aggregated_metrics::types::ActionExecutionMetrics;
use crate::build::overlap::HasBuildOverlapTracker;
use crate::deferred::calculation::ActionLookup;
use crate::deferred::calculation::lookup_deferred_holder;
use crate::keep_going::KeepGoing;
use crate::starlark::values::UnpackValue;
use crate::starlark::values::type_repr::StarlarkTypeRepr;

pub struct ActionCalculation;

async fn build_action_impl(
    ctx: &mut DiceComputations<'_>,
    cancellation: &CancellationContext,
    key: &ActionKey,
) -> buck2_error::Result<ActionOutputs> {
    // Compute is only called if we have cache miss
    debug!("compute {}", key);

    let action = ActionCalculation::get_action(ctx, key).await?;

    if action.key() != key {
        // The action key we start with is on the DICE graph, and thus cached
        // and properly deduplicated. But if the underlying has a different key,
        // e.g. due to dynamic_output, then we might have two different action keys
        // pointing at the same underlying action. We need to make sure that
        // underlying action only gets called once, so call build_action once
        // again with the new key to get DICE deduplication.
        let res = ActionCalculation::build_action(ctx, action.key()).await;
        return res;
    }

    build_action_no_redirect(ctx, cancellation, action).await
}

async fn build_action_no_redirect(
    ctx: &mut DiceComputations<'_>,
    cancellation: &CancellationContext,
    action: Arc<RegisteredAction>,
) -> buck2_error::Result<ActionOutputs> {
    let inputs = action.inputs()?;
    let waiting_data = WaitingData::new();
    let executor = ctx
        .get_action_executor(action.execution_config())
        .await
        .buck_error_context(format!("for action `{action}`"))?;

    let _eager_guard = if executor.materializer().is_eager_materialization_enabled()
        && action.eager_materialization_enabled()
        && action.executor_preference().is_some_and(|pref| {
            !pref.prefers_remote()
                && executor.is_local_execution_possible(pref)
                && (pref.prefers_local() || executor.is_full_hybrid_enabled())
        }) {
        let artifact_fs = ctx.get_artifact_fs().await?;
        let eager_paths = collect_eager_paths(ctx, &inputs, &artifact_fs).await?;

        if eager_paths.is_empty() {
            None
        } else {
            Some(
                executor
                    .materializer()
                    .register_eager_paths(eager_paths, get_dispatcher())
                    .await?,
            )
        }
    } else {
        None
    };

    let target_rule_type_name = action.target_rule_type_name().map(str::to_owned);
    let is_eligible_for_dedupe = is_action_eligible_for_dedupe(&action, inputs.iter());
    let is_expected_eligible_for_dedupe = expected_eligible_for_dedupe(&action);

    if let Some(local_action_cache_inputs) = action.local_action_cache_inputs()? {
        let mut ensured_inputs_for_execution = None;
        let ensured_local_action_cache_inputs =
            ensure_action_input_set(ctx, &local_action_cache_inputs).await?;
        let local_action_cache_input_set_digest =
            ensured_local_action_cache_inputs.input_set_digest.dupe();
        if local_action_cache_inputs.as_ref() == inputs.as_ref() {
            ensured_inputs_for_execution = Some(ensured_local_action_cache_inputs.inputs.dupe());
        }
        let (execute_result, command_reports) = executor
            .try_execute_local_action_cache(
                waiting_data.clone(),
                ensured_local_action_cache_inputs.inputs,
                local_action_cache_input_set_digest.dupe(),
                action.as_ref(),
                cancellation,
            )
            .await;

        let execute_result = match execute_result {
            Ok(Some(result)) => Some(Ok(result)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        };

        if let Some(execute_result) = execute_result {
            let start_event = action_execution_start_event(&action);
            ctx.per_transaction_data()
                .record_action_started_for_overlap(|| action.key().to_string());
            let now = TimeSpan::start_now();
            let fut = build_action_result(
                ctx,
                &executor,
                execute_result,
                command_reports,
                &action,
                target_rule_type_name.clone(),
                is_eligible_for_dedupe,
                is_expected_eligible_for_dedupe,
            );

            let (action_execution_data, spans) =
                async_record_root_spans(span_async(start_event, fut.boxed())).await;
            return finish_action_execution(ctx, &action, now, action_execution_data, spans);
        }

        let ensured_inputs = match ensured_inputs_for_execution {
            Some(ensured_inputs) => ensured_inputs,
            None => ensure_action_input_set(ctx, &inputs).await?.inputs,
        };

        return build_action_after_inputs(
            ctx,
            cancellation,
            action,
            executor,
            waiting_data,
            ensured_inputs,
            local_action_cache_input_set_digest,
            target_rule_type_name,
            is_eligible_for_dedupe,
            is_expected_eligible_for_dedupe,
        )
        .await;
    }

    let ensured_input_set = ensure_action_input_set(ctx, &inputs).await?;
    build_action_after_inputs(
        ctx,
        cancellation,
        action,
        executor,
        waiting_data,
        ensured_input_set.inputs,
        ensured_input_set.input_set_digest,
        target_rule_type_name,
        is_eligible_for_dedupe,
        is_expected_eligible_for_dedupe,
    )
    .await
}

async fn build_action_after_inputs(
    ctx: &mut DiceComputations<'_>,
    cancellation: &CancellationContext,
    action: Arc<RegisteredAction>,
    executor: Arc<BuckActionExecutor>,
    waiting_data: WaitingData,
    ensured_inputs: Arc<BuckIndexMap<ArtifactGroup, ArtifactGroupValues>>,
    local_action_cache_input_set_digest: Arc<[u8]>,
    target_rule_type_name: Option<String>,
    is_eligible_for_dedupe: buck2_data::EligibleForDedupe,
    is_expected_eligible_for_dedupe: buck2_data::ExpectedEligibleForDedupe,
) -> buck2_error::Result<ActionOutputs> {
    let start_event = action_execution_start_event(&action);
    ctx.per_transaction_data()
        .record_action_started_for_overlap(|| action.key().to_string());

    let now = TimeSpan::start_now();
    let action = &action;

    let fut = build_action_inner(
        ctx,
        cancellation,
        &executor,
        waiting_data,
        ensured_inputs,
        local_action_cache_input_set_digest,
        action,
        target_rule_type_name,
        is_eligible_for_dedupe,
        is_expected_eligible_for_dedupe,
    );

    // boxed() the future so that we don't need to allocate space for it while waiting on input dependencies.
    let (action_execution_data, spans) =
        async_record_root_spans(span_async(start_event, fut.boxed())).await;

    finish_action_execution(ctx, action, now, action_execution_data, spans)
}

async fn ensure_action_input_set(
    ctx: &mut DiceComputations<'_>,
    inputs: &[ArtifactGroup],
) -> buck2_error::Result<ActionInputSet> {
    let inputs: Arc<[ArtifactGroup]> = inputs.iter().cloned().collect::<Vec<_>>().into();
    ctx.compute(&ActionInputSetKey(inputs)).await?
}

#[derive(Clone, Dupe, Allocative, PagablePanic)]
struct ActionInputSet {
    inputs: Arc<BuckIndexMap<ArtifactGroup, ArtifactGroupValues>>,
    input_set_digest: Arc<[u8]>,
}

#[derive(Clone, Dupe, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct ActionInputSetKey(Arc<[ArtifactGroup]>);

impl fmt::Display for ActionInputSetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ACTION_INPUT_SET({} inputs)", self.0.len())
    }
}

#[async_trait]
impl Key for ActionInputSetKey {
    type Value = buck2_error::Result<ActionInputSet>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let ready_inputs: Vec<_> = tokio::task::unconstrained(KeepGoing::try_compute_join_all(
            ctx,
            self.0.iter(),
            |ctx, v| async move { ctx.ensure_artifact_group(v).await }.boxed(),
        ))
        .await?;

        let mut fingerprint =
            CasDigestData::digester(ctx.global_data().get_digest_config().cas_digest_config());
        action_cache_add_str(
            &mut fingerprint,
            "buck2-local-action-cache-artifact-input-set-v1",
        );
        action_cache_add_str(&mut fingerprint, "inputs");

        let mut results = BuckIndexMap::with_capacity(self.0.len());
        for (artifact, ready) in zip(self.0.iter(), ready_inputs) {
            let action_cache_fingerprint = ready.action_cache_fingerprint().ok_or_else(|| {
                internal_error!("missing action-cache fingerprint for action input `{artifact}`")
            })?;
            action_cache_add_bytes(&mut fingerprint, action_cache_fingerprint);
            results.insert(artifact.clone(), ready);
        }

        let input_set_digest: Arc<[u8]> = finalize_action_cache_digest(fingerprint)
            .into_boxed_slice()
            .into();
        Ok(ActionInputSet {
            inputs: Arc::new(results),
            input_set_digest,
        })
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x.input_set_digest == y.input_set_digest,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

fn action_cache_add_bytes(fingerprint: &mut DataDigester, bytes: &[u8]) {
    fingerprint.update(&(bytes.len() as u64).to_le_bytes());
    fingerprint.update(bytes);
}

fn action_cache_add_str(fingerprint: &mut DataDigester, value: &str) {
    action_cache_add_bytes(fingerprint, value.as_bytes());
}

fn finalize_action_cache_digest(fingerprint: DataDigester) -> Vec<u8> {
    fingerprint.finalize().raw_digest().as_bytes().to_vec()
}

fn action_execution_start_event(action: &RegisteredAction) -> buck2_data::ActionExecutionStart {
    buck2_data::ActionExecutionStart {
        key: Some(action.key().as_proto()),
        kind: action.kind().into(),
        name: Some(buck2_data::ActionName {
            category: action.category().as_str().to_owned(),
            identifier: action.identifier().unwrap_or("").to_owned(),
        }),
    }
}

fn finish_action_execution(
    ctx: &mut DiceComputations<'_>,
    action: &Arc<RegisteredAction>,
    now: TimeSpanBuilder,
    action_execution_data: ActionExecutionData,
    spans: SmallVec<[SpanId; 1]>,
) -> buck2_error::Result<ActionOutputs> {
    let execution_metrics = ActionExecutionMetrics {
        key: action.key().dupe(),
        execution_time_ms: action_execution_data
            .extra_data
            .execution_time_ms
            .unwrap_or_default(),
        execution_kind: action_execution_data.extra_data.execution_kind,
        output_size_bytes: action_execution_data.extra_data.output_size,
        memory_peak: action_execution_data.memory_peak,
        re_platform_name: action_execution_data.extra_data.re_platform_name.clone(),
    };
    ctx.store_evaluation_data(BuildKeyActivationData {
        action_with_extra_data: ActionWithExtraData {
            action: action.dupe(),
            extra_data: action_execution_data.extra_data,
        },
        duration: NodeDuration {
            user: action_execution_data.wall_time.unwrap_or_default(),
            total: now.end_now(),
            queue: action_execution_data.queue_duration,
        },
        spans,
        waiting_data: action_execution_data.waiting_data,
    })?;

    ctx.action_executed(execution_metrics)?;

    action_execution_data.action_result
}

/// Collect all materializable artifact paths from an `ArtifactGroup` list,
/// traversing transitive set projections via BFS.
async fn collect_eager_paths(
    ctx: &mut DiceComputations<'_>,
    inputs: &[ArtifactGroup],
    artifact_fs: &ArtifactFs,
) -> buck2_error::Result<Vec<ProjectRelativePathBuf>> {
    let mut eager_paths = HashSet::new();
    let mut queue: Vec<ArtifactGroup> = inputs.to_vec();
    let mut visited = HashSet::new();

    while let Some(input) = queue.pop() {
        if !visited.insert(input.dupe()) {
            continue;
        }

        match &input {
            ArtifactGroup::Artifact(a) => {
                if a.requires_materialization(artifact_fs) {
                    // For projected artifacts (a file inside a directory output), register
                    // the base directory's configuration path. The materializer only declares
                    // base artifact paths, so the projected sub-path would never match a
                    // Declare. Materializing the base directory covers all projected files.
                    let path = if a.is_projected() {
                        match a.as_parts().0 {
                            BaseArtifactKind::Build(b) => {
                                artifact_fs.resolve_build_configuration_hash_path(b.get_path())?
                            }
                            BaseArtifactKind::Source(s) => {
                                artifact_fs.resolve_source(s.get_path())?
                            }
                        }
                    } else {
                        a.resolve_configuration_hash_path(artifact_fs)?
                    };
                    eager_paths.insert(path);
                }
            }
            ArtifactGroup::TransitiveSetProjection(tset) => {
                let set = tset.key.key.lookup(ctx).await?;
                queue.extend(set.get_projection_sub_inputs(tset.key.projection)?);
            }
            ArtifactGroup::Promise(_) => {
                // Skip promise artifacts - they should not be eagerly materialized
            }
        }
    }

    Ok(eager_paths.into_iter().collect())
}

async fn build_action_inner(
    ctx: &mut DiceComputations<'_>,
    cancellation: &CancellationContext,
    executor: &BuckActionExecutor,
    waiting_data: WaitingData,
    ensured_inputs: Arc<BuckIndexMap<ArtifactGroup, ArtifactGroupValues>>,
    local_action_cache_input_set_digest: Arc<[u8]>,
    action: &Arc<RegisteredAction>,
    target_rule_type_name: Option<String>,
    is_eligible_for_dedupe: buck2_data::EligibleForDedupe,
    is_expected_eligible_for_dedupe: buck2_data::ExpectedEligibleForDedupe,
) -> (ActionExecutionData, Box<buck2_data::ActionExecutionEnd>) {
    let (execute_result, command_reports) = executor
        .execute(
            waiting_data,
            ensured_inputs,
            local_action_cache_input_set_digest,
            action,
            cancellation,
        )
        .await;

    build_action_result(
        ctx,
        executor,
        execute_result,
        command_reports,
        action,
        target_rule_type_name,
        is_eligible_for_dedupe,
        is_expected_eligible_for_dedupe,
    )
    .await
}

async fn build_action_result(
    ctx: &mut DiceComputations<'_>,
    executor: &BuckActionExecutor,
    execute_result: Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError>,
    command_reports: Vec<CommandExecutionReport>,
    action: &Arc<RegisteredAction>,
    target_rule_type_name: Option<String>,
    is_eligible_for_dedupe: buck2_data::EligibleForDedupe,
    is_expected_eligible_for_dedupe: buck2_data::ExpectedEligibleForDedupe,
) -> (ActionExecutionData, Box<buck2_data::ActionExecutionEnd>) {
    let local_action_cache_action_digest =
        local_action_cache_action_digest(&execute_result, &command_reports);
    if let Some(action_digest) = local_action_cache_action_digest {
        let Ok((outputs, meta)) = execute_result else {
            unreachable!("local action cache digest only exists for successful executions")
        };
        let queue_duration = command_reports.last().and_then(|r| r.timing.queue_duration);
        let action_key = action.key().as_proto();
        let action_name = buck2_data::ActionName {
            category: action.category().as_str().to_owned(),
            identifier: action.identifier().unwrap_or("").to_owned(),
        };
        let wall_time = Some(meta.timing.wall_time);
        let execution_kind = meta.execution_kind.as_enum();
        let invalidation_info = action_invalidation_info(ctx, executor);

        return (
            ActionExecutionData {
                action_result: Ok(outputs),
                wall_time,
                queue_duration,
                memory_peak: None,
                extra_data: ActionExtraData {
                    execution_kind,
                    target_rule_type_name: target_rule_type_name.clone(),
                    action_digest: Some(action_digest),
                    invalidation_info: invalidation_info.clone(),
                    execution_time_ms: Some(0),
                    output_size: 0,
                    re_platform_name: None,
                },
                waiting_data: meta.waiting_data,
            },
            Box::new(buck2_data::ActionExecutionEnd {
                key: Some(action_key),
                kind: action.kind().into(),
                name: Some(action_name),
                failed: false,
                error: None,
                always_print_stderr: action.always_print_stderr(),
                wall_time: wall_time.and_then(|d| d.try_into().ok()),
                execution_kind: execution_kind as i32,
                output_size: 0,
                commands: Vec::new(),
                outputs: Vec::new(),
                prefers_local: false,
                requires_local: false,
                allows_cache_upload: false,
                cache_upload_result: buck2_data::UploadResult::DidNotUploadUnspecified as i32,
                allows_dep_file_cache_upload: false,
                dep_file_cache_upload_result: buck2_data::UploadResult::DidNotUploadUnspecified
                    as i32,
                dep_file_key: None,
                eligible_for_full_hybrid: None,
                buck2_revision: None,
                buck2_build_time: None,
                hostname: None,
                error_diagnostics: None,
                input_files_bytes: meta.input_files_bytes,
                invalidation_info,
                target_rule_type_name,
                scheduling_mode: None,
                incremental_kind: None,
                eligible_for_dedupe: is_eligible_for_dedupe as i32,
                expected_eligible_for_dedupe: is_expected_eligible_for_dedupe as i32,
            }),
        );
    }

    let allow_omit_details = execute_result.is_ok();

    let commands = if allow_omit_details {
        let fast_commands = command_reports
            .iter()
            .map(local_action_cache_command_execution_report_to_proto)
            .collect::<Option<Vec<_>>>();
        match fast_commands {
            Some(commands) => commands,
            None => {
                future::join_all(
                    command_reports
                        .iter()
                        .map(|r| command_execution_report_to_proto(r, allow_omit_details)),
                )
                .await
            }
        }
    } else {
        future::join_all(
            command_reports
                .iter()
                .map(|r| command_execution_report_to_proto(r, allow_omit_details)),
        )
        .await
    };

    let action_digest = get_action_digest(&commands);

    let queue_duration = command_reports.last().and_then(|r| r.timing.queue_duration);
    let memory_peak = command_reports
        .last()
        .and_then(|r| r.timing.execution_stats.and_then(|s| s.memory_peak));

    let action_key = action.key().as_proto();

    let action_name = buck2_data::ActionName {
        category: action.category().as_str().to_owned(),
        identifier: action.identifier().unwrap_or("").to_owned(),
    };

    let action_result;
    let execution_kind;
    let wall_time;
    let error;
    let output_size;

    let mut prefers_local = None;
    let mut requires_local = None;
    let mut allows_cache_upload = None;
    let mut did_cache_upload = None;
    let mut allows_dep_file_cache_upload = None;
    let mut did_dep_file_cache_upload = None;
    let mut dep_file_key = None;
    let mut eligible_for_full_hybrid = None;

    let mut buck2_revision = None;
    let mut buck2_build_time = None;
    let mut hostname = None;
    let mut input_files_bytes = None;
    let mut scheduling_mode = None;
    let mut incremental_kind = None;
    let mut waiting_data = None;
    let error_diagnostics = match execute_result {
        Ok((outputs, meta)) => {
            output_size = outputs.calc_output_count_and_bytes(false).bytes;
            action_result = Ok(outputs);
            execution_kind = Some(meta.execution_kind.as_enum());
            wall_time = Some(meta.timing.wall_time);
            error = None;
            input_files_bytes = meta.input_files_bytes;
            waiting_data = Some(meta.waiting_data);

            if let Some(command) = meta.execution_kind.command() {
                prefers_local = Some(command.prefers_local);
                requires_local = Some(command.requires_local);
                allows_cache_upload = Some(command.allows_cache_upload);
                did_cache_upload = Some(command.did_cache_upload);
                allows_dep_file_cache_upload = Some(command.allows_dep_file_cache_upload);
                did_dep_file_cache_upload = Some(command.did_dep_file_cache_upload);
                dep_file_key = *command.dep_file_key;
                eligible_for_full_hybrid = Some(command.eligible_for_full_hybrid);
                scheduling_mode = command.scheduling_mode;
                incremental_kind = Some(command.incremental_kind);
            }

            None
        }
        Err(e) => {
            // TODO (torozco): Remove (see protobuf file)?
            execution_kind = command_reports
                .last()
                .and_then(|r| r.status.execution_kind())
                .map(|e| e.as_enum());
            wall_time = command_reports
                .last()
                .map(|r| r.timing.time_span.duration());
            output_size = 0;
            // We define the below fields only in the instance of an action error
            // so as to reduce Scribe traffic and log it in buck2_action_errors
            buck2_revision = buck2_build_info::revision().map(|s| s.to_owned());
            buck2_build_time = buck2_build_info::time_iso8601().map(|s| s.to_owned());
            hostname = buck2_events::metadata::hostname();

            let last_command = commands.last().cloned();

            let outputs = match &e {
                ExecuteError::CommandExecutionError { action_outputs, .. } => Some(action_outputs),
                _ => None,
            };

            let error_diagnostics = try_run_error_handler(
                action.dupe(),
                last_command.as_ref(),
                ctx.get_artifact_fs().await,
                outputs,
            );

            let infra_error_tag = check_infra_error_patterns(last_command.as_ref());

            let e = ActionError::new(
                e,
                action_name.clone(),
                action_key.clone(),
                last_command.clone(),
                error_diagnostics.clone(),
                infra_error_tag,
            );

            error = Some(e.as_proto_field());

            ctx.per_transaction_data()
                .get_dispatcher()
                .instant_event(e.as_proto_event());

            action_result = Err(buck2_error::Error::from(e)
                // Make sure to mark the error as emitted so that it is not printed out to console
                // again in this command. We still need to keep it around for the build report (and
                // in the future) other commands
                .mark_emitted({
                    let owner = action.owner().dupe();
                    Arc::new(move |f| write!(f, "Failed to build '{owner}'"))
                }));

            error_diagnostics
        }
    };

    let outputs = action_result
        .as_ref()
        .map(|outputs| {
            outputs
                .iter()
                .filter_map(|(_artifact, value)| {
                    Some(buck2_data::ActionOutput {
                        tiny_digest: value.digest()?.tiny_digest().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let invalidation_info = action_invalidation_info(ctx, executor);

    let execution_kind = execution_kind.unwrap_or(buck2_data::ActionExecutionKind::NotSet);

    let re_platform_name = command_reports
        .last()
        .and_then(|r| r.status.execution_kind())
        .and_then(|k| k.re_platform_name())
        .map(|s| s.to_owned());

    (
        ActionExecutionData {
            action_result,
            wall_time,
            queue_duration,
            memory_peak,
            extra_data: ActionExtraData {
                execution_kind,
                target_rule_type_name: target_rule_type_name.clone(),
                action_digest,
                invalidation_info,
                execution_time_ms: get_execution_time_ms(&commands),
                output_size,
                re_platform_name,
            },
            waiting_data: waiting_data.unwrap_or_default(),
        },
        Box::new(buck2_data::ActionExecutionEnd {
            key: Some(action_key),
            kind: action.kind().into(),
            name: Some(action_name),
            failed: error.is_some(),
            error,
            always_print_stderr: action.always_print_stderr(),
            wall_time: wall_time.and_then(|d| d.try_into().ok()),
            execution_kind: execution_kind as i32,
            output_size,
            commands,
            outputs,
            prefers_local: prefers_local.unwrap_or_default(),
            requires_local: requires_local.unwrap_or_default(),
            allows_cache_upload: allows_cache_upload.unwrap_or_default(),
            cache_upload_result: if did_cache_upload.unwrap_or_default() {
                buck2_data::UploadResult::Uploaded as i32
            } else {
                buck2_data::UploadResult::DidNotUploadUnspecified as i32
            },
            allows_dep_file_cache_upload: allows_dep_file_cache_upload.unwrap_or_default(),
            dep_file_cache_upload_result: if did_dep_file_cache_upload.unwrap_or_default() {
                buck2_data::UploadResult::Uploaded as i32
            } else {
                buck2_data::UploadResult::DidNotUploadUnspecified as i32
            },
            dep_file_key: dep_file_key.map(|d| d.to_string()),
            eligible_for_full_hybrid,
            buck2_revision,
            buck2_build_time,
            hostname,
            error_diagnostics,
            input_files_bytes,
            invalidation_info,
            target_rule_type_name,
            scheduling_mode: scheduling_mode.map(|h| h as i32),
            incremental_kind: incremental_kind.map(|k| k as i32),
            eligible_for_dedupe: is_eligible_for_dedupe as i32,
            expected_eligible_for_dedupe: is_expected_eligible_for_dedupe as i32,
        }),
    )
}

fn is_action_eligible_for_dedupe<'a>(
    action: &Arc<RegisteredAction>,
    inputs: impl IntoIterator<Item = &'a ArtifactGroup>,
) -> buck2_data::EligibleForDedupe {
    let target_platform =
        if let BaseDeferredKey::TargetLabel(configured_label) = action.key().owner() {
            Some(configured_label.cfg())
        } else {
            None
        };

    if !action.all_outputs_are_content_based() {
        return buck2_data::EligibleForDedupe::IneligibleOutput;
    }

    for ag in inputs {
        let eligibility = ag.is_eligible_for_dedupe(target_platform);
        if eligibility != buck2_data::EligibleForDedupe::Eligible {
            return eligibility;
        }
    }

    buck2_data::EligibleForDedupe::Eligible
}

fn expected_eligible_for_dedupe(
    action: &Arc<RegisteredAction>,
) -> buck2_data::ExpectedEligibleForDedupe {
    match action.is_expected_eligible_for_dedupe() {
        Some(true) => buck2_data::ExpectedEligibleForDedupe::ExpectedEligible,
        Some(false) => buck2_data::ExpectedEligibleForDedupe::ExpectedIneligible,
        None => buck2_data::ExpectedEligibleForDedupe::UnknownEligibility,
    }
}

fn action_invalidation_info(
    ctx: &mut DiceComputations<'_>,
    executor: &BuckActionExecutor,
) -> Option<buck2_data::CommandInvalidationInfo> {
    if !executor.invalidation_tracking_enabled() {
        return None;
    }

    fn to_proto(
        invalidation_path: &DiceTrackedInvalidationPath,
    ) -> Option<buck2_data::command_invalidation_info::InvalidationSource> {
        match invalidation_path {
            DiceTrackedInvalidationPath::Clean | DiceTrackedInvalidationPath::Unknown => None,
            DiceTrackedInvalidationPath::Invalidated(_) => {
                Some(buck2_data::command_invalidation_info::InvalidationSource {})
            }
        }
    }

    let invalidation_paths = ctx.get_invalidation_paths();
    Some(buck2_data::CommandInvalidationInfo {
        changed_any: to_proto(&invalidation_paths.normal_priority_path),
        changed_file: to_proto(&invalidation_paths.high_priority_path),
    })
}

fn local_action_cache_action_digest(
    execute_result: &Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError>,
    command_reports: &[CommandExecutionReport],
) -> Option<String> {
    let Ok((_outputs, meta)) = execute_result else {
        return None;
    };
    match &meta.execution_kind {
        ActionExecutionKind::Command { kind, .. } => match kind.as_ref() {
            CommandExecutionKind::LocalActionCache { digest } => Some(digest.to_string()),
            _ => None,
        },
        ActionExecutionKind::LocalActionCache => command_reports.iter().find_map(|report| {
            let CommandExecutionStatus::Success {
                execution_kind: CommandExecutionKind::LocalActionCache { digest },
            } = &report.status
            else {
                return None;
            };
            Some(digest.to_string())
        }),
        _ => None,
    }
}

fn check_infra_error_patterns(
    last_command: Option<&buck2_data::CommandExecution>,
) -> Option<buck2_error::ErrorTag> {
    use buck2_error::ErrorTag;

    let stderr = last_command
        .and_then(|c| c.details.as_ref())
        .map_or("", |d| d.cmd_stderr.as_str());

    const INFRA_PATTERNS: &[(&str, ErrorTag)] = &[(
        "transport endpoint is not connected",
        ErrorTag::IoNotConnected,
    )];

    let stderr_lower = stderr.to_lowercase();
    INFRA_PATTERNS
        .iter()
        .find(|(pattern, _)| stderr_lower.contains(pattern))
        .map(|(_, tag)| *tag)
}

// Attempt to run the error handler if one was specified. Returns either the error diagnostics, or
// an actual error if the handler failed to run successfully.
fn try_run_error_handler(
    action: Arc<RegisteredAction>,
    last_command: Option<&buck2_data::CommandExecution>,
    artifact_fs: buck2_error::Result<ArtifactFs>,
    outputs: Option<&ActionOutputs>,
) -> Option<ActionErrorDiagnostics> {
    use buck2_data::action_error_diagnostics::Data;

    fn create_error(
        e: buck2_error::Error,
    ) -> (
        Option<ActionErrorDiagnostics>,
        buck2_data::ActionErrorHandlerExecutionEnd,
    ) {
        (
            Some(ActionErrorDiagnostics {
                data: Some(Data::HandlerInvocationError(format!("{e:#}"))),
            }),
            buck2_data::ActionErrorHandlerExecutionEnd {},
        )
    }

    match action.action.error_handler() {
        Some(error_handler) => {
            let dispatcher = get_dispatcher();

            dispatcher
                .clone()
                .span(buck2_data::ActionErrorHandlerExecutionStart {}, || {
                    // patternlint-disable-next-line buck2-no-starlark-module: FIXME(JakobDegen): Wrong
                    Module::with_temp_heap(|env| {
                        let heap = env.heap();
                        let print = EventDispatcherPrintHandler(get_dispatcher());
                        let mut eval = Evaluator::new(&env);
                        eval.set_print_handler(&print);
                        eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);

                        let artifact_fs = match artifact_fs {
                            Ok(fs) => fs,
                            Err(e) => return create_error(e),
                        };

                        let outputs_artifacts = match action.action.failed_action_output_artifacts(
                            &artifact_fs,
                            heap,
                            outputs,
                        ) {
                            Ok(v) => v,
                            Err(e) => return create_error(e),
                        };

                        let error_handler_ctx =
                            StarlarkActionErrorContext::new_from_command_execution(
                                last_command,
                                outputs_artifacts,
                            );

                        let error_handler_result = eval.eval_function(
                            heap.access_owned_frozen_value(error_handler),
                            &[heap.alloc(error_handler_ctx)],
                            &[],
                        );

                        let data = match error_handler_result {
                            Ok(result) => match ActionSubErrorResult::unpack_value_err(result) {
                                Ok(result) => Data::SubErrors(ActionSubErrors {
                                    sub_errors: result
                                        .items
                                        .into_iter()
                                        .map(|s| s.to_proto())
                                        .collect(),
                                }),
                                Err(_) => Data::HandlerInvocationError(format!(
                                    "{}",
                                    ActionErrorHandlerError::TypeError(
                                        ActionSubErrorResult::starlark_type_repr(),
                                        result.get_type().to_owned()
                                    )
                                )),
                            },
                            Err(e) => {
                                let e = buck2_error::Error::from(e).context("Error handler failed");
                                Data::HandlerInvocationError(format!("{e:#}"))
                            }
                        };
                        (
                            Some(ActionErrorDiagnostics { data: Some(data) }),
                            buck2_data::ActionErrorHandlerExecutionEnd {},
                        )
                    })
                })
        }
        None => None,
    }
}

pub struct BuildKeyActivationData {
    pub action_with_extra_data: ActionWithExtraData,
    pub duration: NodeDuration,
    pub waiting_data: WaitingData,
    pub spans: SmallVec<[SpanId; 1]>,
}

#[derive(Clone)]
pub struct ActionWithExtraData {
    pub action: Arc<RegisteredAction>,
    pub extra_data: ActionExtraData,
}

#[derive(Clone)]
pub struct ActionExtraData {
    pub execution_kind: buck2_data::ActionExecutionKind,
    pub execution_time_ms: Option<u64>,
    pub output_size: u64,
    pub target_rule_type_name: Option<String>,
    pub action_digest: Option<String>,
    pub invalidation_info: Option<buck2_data::CommandInvalidationInfo>,
    /// RE platform name if the action ran remotely.
    pub re_platform_name: Option<String>,
}

struct ActionExecutionData {
    action_result: buck2_error::Result<ActionOutputs>,
    wall_time: Option<std::time::Duration>,
    queue_duration: Option<std::time::Duration>,
    memory_peak: Option<u64>,
    extra_data: ActionExtraData,
    waiting_data: WaitingData,
}

/// The cost of these calls are particularly critical. To control the cost (particularly size) of these calls
/// we drop the `async_trait` common in other `*Calculation` types and avoid `async fn` (for
/// build_action/build_artifact at least).
impl ActionCalculation {
    pub async fn get_action(
        ctx: &mut DiceComputations<'_>,
        action_key: &ActionKey,
    ) -> buck2_error::Result<Arc<RegisteredAction>> {
        // In the typical case, this lookup is only going to require a single deferred holder lookup. There's three cases:
        // 1. a normal action defined in analysis: lookup the holder for that analysis, get the action
        // 2. an action bound to a dynamic_output and then bound to an action there: the initial holder_key will actually
        //    point to the dynamic_output (not the analysis that first created the action key) and then the action will be found there
        // 3. an action bound to a dynamic_output, and then in that dynamic_output bound to another dynamic_output: only in this case
        //    will the initial lookup not find the key and we'll recurse.
        //
        // We could introduce a dice key to cache the recursive resolution, but that would only be valuable if we had long nested chains
        // of dynamic_output that were re-binding artifacts. In practice we've not yet encountered that.
        let deferred_holder = lookup_deferred_holder(ctx, action_key.holder_key()).await?;
        match deferred_holder.lookup_action(action_key)? {
            ActionLookup::Action(action) => Ok(action),
            ActionLookup::Deferred(action_key) => {
                fn get_action_recurse<'a>(
                    ctx: &'a mut DiceComputations<'_>,
                    action_key: &'a ActionKey,
                ) -> BoxFuture<'a, buck2_error::Result<Arc<RegisteredAction>>> {
                    async move { ActionCalculation::get_action(ctx, action_key).await }.boxed()
                }
                get_action_recurse(ctx, &action_key).await
            }
        }
    }

    pub fn build_action<'a>(
        ctx: &'a mut DiceComputations<'_>,
        action_key: &ActionKey,
    ) -> impl Future<Output = buck2_error::Result<ActionOutputs>> + use<'a> {
        // build_action is called for every action key. We don't use `async fn` to ensure that it has minimal cost.
        // We don't currently consume this in buck_e2e but it's good to log for debugging purposes.
        debug!("build_action {}", action_key);
        ctx.compute(BuildKey::ref_cast(action_key)).map(|v| v?)
    }

    pub fn build_artifact<'a>(
        ctx: &'a mut DiceComputations<'_>,
        artifact: &BuildArtifact,
    ) -> impl Future<Output = buck2_error::Result<ActionOutputs>> + use<'a> {
        Self::build_action(ctx, artifact.key())
    }
}

#[derive(
    Clone, Dupe, Display, Debug, Eq, PartialEq, Hash, Allocative, RefCast, Pagable
)]
#[display("ACTION_EXECUTION({})", _0)]
#[repr(transparent)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct BuildKey(pub ActionKey);

#[async_trait]
impl Key for BuildKey {
    type Value = buck2_error::Result<ActionOutputs>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        cancellation: &CancellationContext,
    ) -> Self::Value {
        build_action_impl(ctx, cancellation, &self.0).await
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        // we don't cache any kind of errors. Ideally, we could try to distinguish different
        // error types and try to cache non-transient error types, but practically there
        // are too many unknowns that may cause more harm than good if we cached errors.
        // So, don't cache it for now, until someday we decide to really need to.
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

fn local_action_cache_command_execution_report_to_proto(
    report: &CommandExecutionReport,
) -> Option<buck2_data::CommandExecution> {
    let CommandExecutionStatus::Success {
        execution_kind: kind @ CommandExecutionKind::LocalActionCache { .. },
    } = &report.status
    else {
        return None;
    };
    if !matches!(&report.std_streams, CommandStdStreams::Empty) {
        return None;
    }

    Some(buck2_data::CommandExecution {
        details: Some(buck2_data::CommandExecutionDetails {
            cmd_stdout: String::new(),
            cmd_stderr: String::new(),
            command_kind: Some(kind.to_proto(true)),
            signed_exit_code: report.exit_code,
            metadata: Some(report.timing.to_proto()),
            additional_message: report.additional_message.clone(),
        }),
        status: Some(buck2_data::command_execution::Success {}.into()),
        inline_environment_metadata: Some(report.inline_environment_metadata),
    })
}

async fn command_execution_report_to_proto(
    report: &CommandExecutionReport,
    allow_omit_details: bool,
) -> buck2_data::CommandExecution {
    let details = command_details(report, allow_omit_details).await;

    let status = match &report.status {
        CommandExecutionStatus::Success { .. } => buck2_data::command_execution::Success {}.into(),
        CommandExecutionStatus::Cancelled { .. } => {
            buck2_data::command_execution::Cancelled {}.into()
        }
        CommandExecutionStatus::Failure { .. } => buck2_data::command_execution::Failure {}.into(),
        CommandExecutionStatus::WorkerFailure { .. } => {
            buck2_data::command_execution::WorkerFailure {}.into()
        }
        CommandExecutionStatus::TimedOut { duration, .. } => {
            buck2_data::command_execution::Timeout {
                duration: (*duration).try_into().ok(),
            }
            .into()
        }
        CommandExecutionStatus::Error { stage, error, .. } => {
            buck2_data::command_execution::Error {
                stage: (*stage).to_owned(),
                error: format!("{error:#}"),
            }
            .into()
        }
    };

    buck2_data::CommandExecution {
        details: Some(details),
        status: Some(status),
        inline_environment_metadata: Some(report.inline_environment_metadata),
    }
}

pub async fn command_details(
    command: &CommandExecutionReport,
    allow_omit_details: bool,
) -> buck2_data::CommandExecutionDetails {
    // If the top-level command failed then we don't want to omit any details. If it succeeded and
    // so did this command (it could succeed while not having a success here if we have rejected
    // executions), then we'll strip non-relevant stuff.
    let omit_details =
        allow_omit_details && matches!(command.status, CommandExecutionStatus::Success { .. });

    let signed_exit_code = command.exit_code;

    let stdout;
    let stderr;

    if omit_details {
        stdout = Default::default();
        stderr = match &command.std_streams {
            CommandStdStreams::Empty => String::new(),
            _ => command.std_streams.to_lossy_stderr().await,
        };
    } else {
        let pair = command.std_streams.to_lossy().await;
        stdout = pair.stdout;
        stderr = pair.stderr;
    };

    let command_kind = command
        .status
        .execution_kind()
        .map(|k| k.to_proto(omit_details));

    buck2_data::CommandExecutionDetails {
        cmd_stdout: stdout,
        cmd_stderr: stderr,
        command_kind,
        signed_exit_code,
        metadata: Some(command.timing.to_proto()),
        additional_message: command.additional_message.clone(),
    }
}

pub async fn get_target_rule_type_name(
    ctx: &mut DiceComputations<'_>,
    label: &ConfiguredTargetLabel,
) -> buck2_error::Result<String> {
    Ok(ctx
        .compute(&TargetRuleTypeNameKey(label.dupe()))
        .await??
        .to_string())
}

#[derive(Clone, Dupe, Eq, PartialEq, Hash, Display, Debug, Allocative, Pagable)]
#[display("target_rule_type_name({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct TargetRuleTypeNameKey(ConfiguredTargetLabel);

#[async_trait]
impl Key for TargetRuleTypeNameKey {
    type Value = buck2_error::Result<Arc<str>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        Ok(Arc::from(
            ctx.get_configured_target_node(&self.0)
                .await
                .require_compatible()?
                .underlying_rule_type()
                .name(),
        ))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}
