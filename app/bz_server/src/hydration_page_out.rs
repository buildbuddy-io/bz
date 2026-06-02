/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;

use async_trait::async_trait;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::partial_result_dispatcher::NoPartialResult;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use bz_server_ctx::template::ServerCommandTemplate;
use bz_server_ctx::template::run_server_command;
use dice::Dice;
use dice::DiceTransaction;
use dupe::Dupe;

use crate::ctx::ServerCommandContext;

pub(crate) async fn hydration_page_out_command(
    ctx: &ServerCommandContext<'_>,
    partial_result_dispatcher: PartialResultDispatcher<NoPartialResult>,
    _req: bz_cli_proto::HydrationPageOutRequest,
) -> bz_error::Result<bz_cli_proto::GenericResponse> {
    let dice = ctx.base_context.daemon.dice_manager.unsafe_dice().dupe();
    run_server_command(
        HydrationPageOutServerCommand { dice },
        ctx,
        partial_result_dispatcher,
    )
    .await
}

struct HydrationPageOutServerCommand {
    dice: Arc<Dice>,
}

#[async_trait]
impl ServerCommandTemplate for HydrationPageOutServerCommand {
    type StartEvent = bz_data::HydrationPageOutCommandStart;
    type EndEvent = bz_data::HydrationPageOutCommandEnd;
    type Response = bz_cli_proto::GenericResponse;
    type PartialResult = NoPartialResult;

    fn exclusive_command_name(&self) -> Option<String> {
        Some("hydration-page-out".to_owned())
    }

    async fn command(
        &self,
        _server_ctx: &dyn ServerCommandContextTrait,
        _partial_result_dispatcher: PartialResultDispatcher<Self::PartialResult>,
        _ctx: DiceTransaction,
    ) -> bz_error::Result<Self::Response> {
        self.dice.page_out().await.map_err(|e| {
            bz_error::conversion::from_any_with_tag(e, bz_error::ErrorTag::Environment)
        })?;
        Ok(bz_cli_proto::GenericResponse {})
    }
}
