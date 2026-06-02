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
use buck2_cli_proto::HydrationPageOutRequest;
use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::common::BuckArgMatches;
use buck2_client_ctx::common::CommonBuildConfigurationOptions;
use buck2_client_ctx::common::CommonEventLogOptions;
use buck2_client_ctx::common::CommonStarlarkOptions;
use buck2_client_ctx::common::ui::CommonConsoleOptions;
use buck2_client_ctx::daemon::client::BuckdClientConnector;
use buck2_client_ctx::daemon::client::NoPartialResultHandler;
use buck2_client_ctx::events_ctx::EventsCtx;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::streaming::StreamingCommand;

/// Subcommands for `buck2 debug hydration`. Controls DICE node value page-out / page-in.
///
/// Pagable storage is opt-in: set the `BUCK2_DICE_DB_PATH` environment variable when
/// starting the daemon to choose a sled database location. Without it, `page-out` is
/// a no-op.
#[derive(Debug, clap::Parser)]
pub enum HydrationCommand {
    /// Serialize all hydrated `OccupiedGraphNode` values in DICE to disk and replace
    /// them in-memory with paged-out markers. Subsequent lookups hydrate values back
    /// on demand off the DICE core thread.
    PageOut(HydrationPageOutCommand),
}

#[derive(Debug, clap::Parser)]
pub struct HydrationPageOutCommand {}

#[async_trait(?Send)]
impl StreamingCommand for HydrationPageOutCommand {
    const COMMAND_NAME: &'static str = "HydrationPageOut";

    fn existing_only() -> bool {
        true
    }

    async fn exec_impl(
        self,
        buckd: &mut BuckdClientConnector,
        matches: BuckArgMatches<'_>,
        ctx: &mut ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        let context = ctx.client_context(matches, &self)?;
        buckd
            .with_flushing()
            .hydration_page_out(
                HydrationPageOutRequest {
                    context: Some(context),
                },
                events_ctx,
                ctx.console_interaction_stream(&self.console_opts()),
                &mut NoPartialResultHandler,
            )
            .await??;
        ExitResult::success()
    }

    fn console_opts(&self) -> &CommonConsoleOptions {
        CommonConsoleOptions::simple_ref()
    }

    fn event_log_opts(&self) -> &CommonEventLogOptions {
        CommonEventLogOptions::default_ref()
    }

    fn build_config_opts(&self) -> &CommonBuildConfigurationOptions {
        CommonBuildConfigurationOptions::default_ref()
    }

    fn starlark_opts(&self) -> &CommonStarlarkOptions {
        CommonStarlarkOptions::default_ref()
    }
}

impl HydrationCommand {
    pub fn exec(
        self,
        matches: BuckArgMatches<'_>,
        ctx: ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        let matches = matches.unwrap_subcommand();
        match self {
            HydrationCommand::PageOut(cmd) => ctx.exec(cmd, matches, events_ctx),
        }
    }
}
