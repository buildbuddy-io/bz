/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use async_trait::async_trait;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::partial_result_dispatcher::NoPartialResult;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use bz_server_ctx::template::ServerCommandTemplate;
use bz_server_ctx::template::run_server_command;
use chrono::TimeZone;
use chrono::Utc;
use dice::DiceTransaction;

use crate::ctx::ServerCommandContext;

pub(crate) async fn clean_stale_command(
    ctx: &ServerCommandContext<'_>,
    partial_result_dispatcher: PartialResultDispatcher<NoPartialResult>,
    req: bz_cli_proto::CleanStaleRequest,
) -> bz_error::Result<bz_cli_proto::CleanStaleResponse> {
    run_server_command(
        CleanStaleServerCommand { req },
        ctx,
        partial_result_dispatcher,
    )
    .await
}

struct CleanStaleServerCommand {
    req: bz_cli_proto::CleanStaleRequest,
}

#[async_trait]
impl ServerCommandTemplate for CleanStaleServerCommand {
    type StartEvent = bz_data::CleanCommandStart;
    type EndEvent = bz_data::CleanCommandEnd;
    type Response = bz_cli_proto::CleanStaleResponse;
    type PartialResult = NoPartialResult;

    async fn command(
        &self,
        server_ctx: &dyn ServerCommandContextTrait,
        _partial_result_dispatcher: PartialResultDispatcher<Self::PartialResult>,
        _ctx: DiceTransaction,
    ) -> bz_error::Result<Self::Response> {
        server_ctx
            .cancellation_context()
            .critical_section(|| async move {
                let deferred_materializer = server_ctx.materializer();

                let extension = deferred_materializer
                    .as_deferred_materializer_extension()
                    .ok_or_else(|| internal_error!("Deferred materializer is not in use"))?;

                let keep_since_time = Utc
                    .timestamp_opt(self.req.keep_since_time, 0)
                    .single()
                    .ok_or_else(|| internal_error!("Invalid timestamp"))?;

                extension
                    .clean_stale_artifacts(keep_since_time, self.req.dry_run, self.req.tracked_only)
                    .await
                    .buck_error_context("Failed to clean stale artifacts.")
            })
            .await
    }

    fn end_event(&self, response: &bz_error::Result<Self::Response>) -> Self::EndEvent {
        let clean_stale_stats = if let Ok(res) = response {
            res.stats
        } else {
            None
        };
        bz_data::CleanCommandEnd { clean_stale_stats }
    }
}
