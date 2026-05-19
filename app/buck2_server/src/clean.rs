/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use buck2_cli_proto::CleanRequest;
use buck2_cli_proto::CleanStaleResponse;
use buck2_common::file_ops::metadata::clear_computed_file_digest_cache;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use buck2_events::dispatch::span_async;
use buck2_execute::execute::clean_output_paths::CleanOutputPaths;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_server_ctx::commands::command_end;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::partial_result_dispatcher::NoPartialResult;
use buck2_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use dupe::Dupe;

use crate::ctx::ServerCommandContext;

pub(crate) async fn clean_command(
    context: &ServerCommandContext<'_>,
    _partial_result_dispatcher: PartialResultDispatcher<NoPartialResult>,
    req: CleanRequest,
) -> buck2_error::Result<CleanStaleResponse> {
    let start_event = context
        .command_start_event(buck2_data::CleanCommandStart {}.into())
        .await?;
    span_async(start_event, async {
        let result = clean_impl(context, req).await;
        let clean_stale_stats = result.as_ref().ok().and_then(|res| res.stats.clone());
        let end_event = command_end(&result, buck2_data::CleanCommandEnd { clean_stale_stats });
        (result, end_event)
    })
    .await
}

async fn clean_impl(
    context: &ServerCommandContext<'_>,
    req: CleanRequest,
) -> buck2_error::Result<CleanStaleResponse> {
    if !req.dry_run {
        context
            .base_context
            .daemon
            .local_action_cache
            .clear()
            .await
            .buck_error_context("Failed to clear local action cache")?;

        context
            .base_context
            .daemon
            .incremental_db_state
            .clear()
            .buck_error_context("Failed to clear incremental state")?;

        clear_computed_file_digest_cache();

        context
            .base_context
            .daemon
            .dice_manager
            .reset_dice(context.events().dupe(), "clean".to_owned())
            .await
            .buck_error_context("Failed to reset DICE graph")?;
    }

    let materializer = context.materializer();
    let extension = materializer
        .as_deferred_materializer_extension()
        .ok_or_else(|| internal_error!("Deferred materializer is not in use"))?;

    let response = extension
        .clean_all_artifacts(req.dry_run)
        .await
        .buck_error_context("Failed to clean output artifacts")?;

    if !req.dry_run {
        clean_bazel_execroot(context)
            .await
            .buck_error_context("Failed to clean Bazel execroot")?;
    }

    Ok(response)
}

async fn clean_bazel_execroot(context: &ServerCommandContext<'_>) -> buck2_error::Result<()> {
    let bazel_execroot = context
        .buck_out_dir
        .join(ForwardRelativePath::unchecked_new("__bazel_execroot"));
    context
        .base_context
        .daemon
        .blocking_executor
        .execute_io(
            Box::new(CleanOutputPaths {
                paths: vec![bazel_execroot],
            }),
            context.cancellation_context(),
        )
        .await
}
