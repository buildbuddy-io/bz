/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::io::Write;

use async_trait::async_trait;
use bz_build_api::actions::query::ActionQueryNode;
use bz_build_api::query::oneshot::QUERY_FRONTEND;
use bz_cli_proto::HasClientContext;
use bz_common::dice::cells::HasCellResolver;
use bz_error::internal_error;
use bz_query::query::environment::AttrFmtOptions;
use bz_query::query::syntax::simple::eval::values::QueryEvaluationResult;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::global_cfg_options::global_cfg_options_from_client_context;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use bz_server_ctx::template::ServerCommandTemplate;
use bz_server_ctx::template::run_server_command;
use dice::DiceTransaction;

use crate::query::printer::QueryResultPrinter;
use crate::query::printer::ShouldPrintProviders;
use crate::query::query_target_ext::QueryCommandTarget;

impl QueryCommandTarget for ActionQueryNode {
    fn call_stack(&self) -> Option<String> {
        None
    }

    fn attr_to_string_alternate(&self, _options: AttrFmtOptions, attr: &Self::Attr<'_>) -> String {
        format!("{attr:#}")
    }

    fn attr_serialize<S: serde::Serializer>(
        &self,
        attr: &Self::Attr<'_>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(attr, serializer)
    }

    fn attr_fmt(
        &self,
        fmt: &mut std::fmt::Formatter<'_>,
        _options: AttrFmtOptions,
        attr: &Self::Attr<'_>,
    ) -> std::fmt::Result {
        std::fmt::Display::fmt(attr, fmt)
    }
}

pub(crate) async fn aquery_command(
    ctx: &dyn ServerCommandContextTrait,
    partial_result_dispatcher: PartialResultDispatcher<bz_cli_proto::StdoutBytes>,
    req: bz_cli_proto::AqueryRequest,
) -> bz_error::Result<bz_cli_proto::AqueryResponse> {
    run_server_command(AqueryServerCommand { req }, ctx, partial_result_dispatcher).await
}

struct AqueryServerCommand {
    req: bz_cli_proto::AqueryRequest,
}

#[async_trait]
impl ServerCommandTemplate for AqueryServerCommand {
    type StartEvent = bz_data::AqueryCommandStart;
    type EndEvent = bz_data::AqueryCommandEnd;
    type Response = bz_cli_proto::AqueryResponse;
    type PartialResult = bz_cli_proto::StdoutBytes;

    async fn command(
        &self,
        server_ctx: &dyn ServerCommandContextTrait,
        mut partial_result_dispatcher: PartialResultDispatcher<Self::PartialResult>,
        ctx: DiceTransaction,
    ) -> bz_error::Result<Self::Response> {
        aquery(
            server_ctx,
            partial_result_dispatcher.as_writer(),
            ctx,
            &self.req,
        )
        .await
    }
}

async fn aquery(
    server_ctx: &dyn ServerCommandContextTrait,
    mut stdout: impl Write,
    mut ctx: DiceTransaction,
    request: &bz_cli_proto::AqueryRequest,
) -> bz_error::Result<bz_cli_proto::AqueryResponse> {
    let cell_resolver = ctx.get_cell_resolver().await?;

    let output_configuration = QueryResultPrinter::from_request_options(
        &cell_resolver,
        &request.output_attributes,
        request.unstable_output_format,
        request.client_context()?.trace_id.clone(),
    )?;

    let bz_cli_proto::AqueryRequest {
        query, query_args, ..
    } = request;

    let global_cfg_options = global_cfg_options_from_client_context(
        request
            .target_cfg
            .as_ref()
            .ok_or_else(|| internal_error!("target_cfg must be set"))?,
        server_ctx,
        &mut ctx,
    )
    .await?;

    let query_result = QUERY_FRONTEND
        .get()?
        .eval_aquery(
            &mut ctx,
            server_ctx.working_dir(),
            query,
            query_args,
            global_cfg_options,
        )
        .await?;

    match query_result {
        QueryEvaluationResult::Single(targets) => {
            output_configuration
                .print_single_output(&mut stdout, targets, false, ShouldPrintProviders::No)
                .await?
        }
        QueryEvaluationResult::Multiple(results) => {
            output_configuration
                .print_multi_output(&mut stdout, results, false, ShouldPrintProviders::No)
                .await?
        }
    };
    Ok(bz_cli_proto::AqueryResponse {})
}
