/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![feature(error_generic_member_access)]

mod lint;
mod typecheck;
mod util;

use async_trait::async_trait;
use bz_cli_proto::ClientContext;
use bz_cmd_starlark_client::StarlarkSubcommand;
use bz_events::dispatch::span_async;
use bz_server_ctx::commands::command_end;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::late_bindings::STARLARK_SERVER_COMMAND;
use bz_server_ctx::late_bindings::StarlarkServerCommand;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;

pub fn init_late_bindings() {
    STARLARK_SERVER_COMMAND.init(&StarlarkServerCommandImpl);
}

struct StarlarkServerCommandImpl;

#[async_trait]
impl StarlarkServerCommand for StarlarkServerCommandImpl {
    async fn starlark(
        &self,
        ctx: &dyn ServerCommandContextTrait,
        partial_result_dispatcher: PartialResultDispatcher<bz_cli_proto::StdoutBytes>,
        req: bz_cli_proto::GenericRequest,
    ) -> bz_error::Result<bz_cli_proto::GenericResponse> {
        let start_event = ctx
            .command_start_event(bz_data::StarlarkCommandStart {}.into())
            .await?;
        span_async(
            start_event,
            server_starlark_command_inner(ctx, partial_result_dispatcher, req),
        )
        .await
    }
}

#[async_trait]
pub(crate) trait StarlarkServerSubcommand: Send + Sync + 'static {
    async fn server_execute(
        &self,
        server_ctx: &dyn ServerCommandContextTrait,
        stdout: PartialResultDispatcher<bz_cli_proto::StdoutBytes>,
        client_server_ctx: ClientContext,
    ) -> bz_error::Result<()>;
}

async fn server_starlark_command_inner(
    context: &dyn ServerCommandContextTrait,
    partial_result_dispatcher: PartialResultDispatcher<bz_cli_proto::StdoutBytes>,
    req: bz_cli_proto::GenericRequest,
) -> (
    bz_error::Result<bz_cli_proto::GenericResponse>,
    bz_data::CommandEnd,
) {
    let result = parse_command_and_execute(context, partial_result_dispatcher, req).await;
    let end_event = command_end(&result, bz_data::StarlarkCommandEnd {});

    let result = result.map(|()| bz_cli_proto::GenericResponse {});

    (result, end_event)
}

async fn parse_command_and_execute(
    context: &dyn ServerCommandContextTrait,
    partial_result_dispatcher: PartialResultDispatcher<bz_cli_proto::StdoutBytes>,
    req: bz_cli_proto::GenericRequest,
) -> bz_error::Result<()> {
    let command: StarlarkSubcommand = serde_json::from_str(&req.serialized_opts)?;
    as_server_subcommand(&command)
        .server_execute(
            context,
            partial_result_dispatcher,
            req.context.expect("buck cli always sets a client context"),
        )
        .await
}

fn as_server_subcommand(cmd: &StarlarkSubcommand) -> &dyn StarlarkServerSubcommand {
    match cmd {
        StarlarkSubcommand::Lint(cmd) => cmd,
        StarlarkSubcommand::Typecheck(cmd) => cmd,
    }
}
