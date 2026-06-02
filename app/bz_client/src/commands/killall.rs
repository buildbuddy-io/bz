/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_client_ctx::client_ctx::BuckSubcommand;
use bz_client_ctx::client_ctx::ClientCommandContext;
use bz_client_ctx::common::BuckArgMatches;
use bz_client_ctx::common::CommonEventLogOptions;
use bz_client_ctx::events_ctx::EventsCtx;
use bz_client_ctx::exit_result::ExitResult;
use bz_wrapper_common::is_buck2::WhoIsAsking;

#[derive(Debug, clap::Parser)]
#[clap(about = "Kill all buck2 processes on the machine")]
pub struct KillallCommand {
    #[clap(flatten)]
    pub(crate) event_log_opts: CommonEventLogOptions,
}

impl BuckSubcommand for KillallCommand {
    const COMMAND_NAME: &'static str = "killall";

    async fn exec_impl(
        self,
        _matches: BuckArgMatches<'_>,
        _ctx: ClientCommandContext<'_>,
        _events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        bz_wrapper_common::killall(WhoIsAsking::Buck2, |s| {
            let _ignored = bz_client_ctx::eprintln!("{}", s);
        })
        .then_some(())
        .ok_or(bz_error::bz_error!(
            bz_error::ErrorTag::KillAll,
            "Killall command failed"
        ))
        .into()
    }

    fn event_log_opts(&self) -> &CommonEventLogOptions {
        &self.event_log_opts
    }
}
