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
use bz_cli_proto::AqueryRequest;
use bz_cli_proto::AqueryResponse;
use bz_cli_proto::CqueryRequest;
use bz_cli_proto::CqueryResponse;
use bz_cli_proto::StdoutBytes;
use bz_cli_proto::UqueryRequest;
use bz_cli_proto::UqueryResponse;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::late_bindings::QueryServerCommands;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;

pub(crate) mod dot;
pub(crate) mod html;
pub(crate) mod query;
mod query_output_format;

use query::aquery::aquery_command;
use query::cquery::cquery_command;
use query::uquery::uquery_command;

struct QueryServerCommandsInstance;

#[async_trait]
impl QueryServerCommands for QueryServerCommandsInstance {
    async fn uquery(
        &self,
        ctx: &dyn ServerCommandContextTrait,
        partial_result_dispatcher: PartialResultDispatcher<StdoutBytes>,
        req: UqueryRequest,
    ) -> bz_error::Result<UqueryResponse> {
        uquery_command(ctx, partial_result_dispatcher, req).await
    }

    async fn cquery(
        &self,
        ctx: &dyn ServerCommandContextTrait,
        partial_result_dispatcher: PartialResultDispatcher<StdoutBytes>,
        req: CqueryRequest,
    ) -> bz_error::Result<CqueryResponse> {
        cquery_command(ctx, partial_result_dispatcher, req).await
    }

    async fn aquery(
        &self,
        ctx: &dyn ServerCommandContextTrait,
        partial_result_dispatcher: PartialResultDispatcher<StdoutBytes>,
        req: AqueryRequest,
    ) -> bz_error::Result<AqueryResponse> {
        aquery_command(ctx, partial_result_dispatcher, req).await
    }
}

pub fn init_late_bindings() {
    bz_server_ctx::late_bindings::QUERY_SERVER_COMMANDS.init(&QueryServerCommandsInstance);
    query::printer::init_print_action_node();
}
