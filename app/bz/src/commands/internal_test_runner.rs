/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_client_ctx::client_ctx::ClientCommandContext;
use bz_client_ctx::common::BuckArgMatches;
use bz_client_ctx::events_ctx::EventsCtx;
use bz_client_ctx::exit_result::ExitResult;
use clap::Parser;
use tokio::runtime::Runtime;

#[derive(Debug, Parser)]
#[clap(about = "run the internal test runner")]
pub(crate) struct InternalTestRunnerCommand {
    #[cfg(unix)]
    #[clap(flatten)]
    unix_runner: bz_test_runner::unix::Buck2TestRunnerUnix,

    #[cfg(not(unix))]
    #[clap(flatten)]
    tcp_runner: bz_test_runner::tcp::Buck2TestRunnerTcp,
}

impl InternalTestRunnerCommand {
    pub(crate) fn exec(
        self,
        _matches: BuckArgMatches<'_>,
        _ctx: ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        events_ctx.log_invocation_record = false;

        let runtime = Runtime::new().expect("Failed to create Tokio runtime");
        runtime
            .block_on(async move {
                #[cfg(unix)]
                {
                    self.unix_runner.run().await
                }
                #[cfg(not(unix))]
                {
                    self.tcp_runner.run().await
                }
            })
            .into()
    }
}
