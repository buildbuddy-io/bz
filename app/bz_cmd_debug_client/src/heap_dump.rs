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
use bz_cli_proto::UnstableHeapDumpRequest;
use bz_client_ctx::client_ctx::ClientCommandContext;
use bz_client_ctx::common::BuckArgMatches;
use bz_client_ctx::common::CommonBuildConfigurationOptions;
use bz_client_ctx::common::CommonEventLogOptions;
use bz_client_ctx::common::CommonStarlarkOptions;
use bz_client_ctx::common::ui::CommonConsoleOptions;
use bz_client_ctx::daemon::client::BuckdClientConnector;
use bz_client_ctx::events_ctx::EventsCtx;
use bz_client_ctx::exit_result::ExitResult;
use bz_client_ctx::path_arg::PathArg;
use bz_client_ctx::streaming::StreamingCommand;

/// Write jemalloc heap profile to a file.
///
/// `mallctl prof.dump`. It is a profile of currently allocated memory,
/// not profile of allocations.
///
/// To use this command, restart buckd with env variable `MALLOC_CONF=prof:true,prof_final:false`.
#[derive(Debug, clap::Parser)]
pub struct HeapDumpCommand {
    /// The path to write the heap dump to.
    #[clap(short, long, value_name = "PATH")]
    path: PathArg,

    /// The path to write the heap dump to.
    #[clap(short, long, value_name = "TEST_PATH")]
    test_executor_path: Option<PathArg>,
}

#[async_trait(?Send)]
impl StreamingCommand for HeapDumpCommand {
    const COMMAND_NAME: &'static str = "heap_dump";

    fn existing_only() -> bool {
        true
    }

    async fn exec_impl(
        self,
        buckd: &mut BuckdClientConnector,
        _matches: BuckArgMatches<'_>,
        ctx: &mut ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        let path = self.path.resolve(&ctx.working_dir);
        let test_executor_path = self
            .test_executor_path
            .map(|path| path.resolve(&ctx.working_dir));
        buckd
            .with_flushing()
            .unstable_heap_dump(
                UnstableHeapDumpRequest {
                    destination_path: path.to_str()?.to_owned(),
                    test_executor_destination_path: test_executor_path
                        .map(|v| -> bz_error::Result<String> { Ok(v.to_str()?.to_owned()) })
                        .transpose()?,
                },
                events_ctx,
            )
            .await?;

        bz_client_ctx::eprintln!("Heap dump written to `{}`", path.to_str()?)?;

        ExitResult::success()
    }

    fn console_opts(&self) -> &CommonConsoleOptions {
        CommonConsoleOptions::none_ref()
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
