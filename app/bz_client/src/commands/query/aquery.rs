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
use bz_client_ctx::client_ctx::ClientCommandContext;
use bz_client_ctx::common::BuckArgMatches;
use bz_client_ctx::common::CommonBuildConfigurationOptions;
use bz_client_ctx::common::CommonCommandOptions;
use bz_client_ctx::common::CommonEventLogOptions;
use bz_client_ctx::common::CommonStarlarkOptions;
use bz_client_ctx::common::target_cfg::TargetCfgOptions;
use bz_client_ctx::common::ui::CommonConsoleOptions;
use bz_client_ctx::daemon::client::BuckdClientConnector;
use bz_client_ctx::daemon::client::StdoutPartialResultHandler;
use bz_client_ctx::events_ctx::EventsCtx;
use bz_client_ctx::exit_result::ExitResult;
use bz_client_ctx::streaming::StreamingCommand;
use bz_core::if_else_opensource;

use crate::commands::query::common::CommonQueryOptions;

fn help() -> &'static str {
    concat!(
        r#"Perform queries on the action graph (experimental)

The action graph consists of all the declared actions for a build,
with dependencies when one action consumes the outputs of another
action.

Run `bz docs aquery` or
"#,
        if_else_opensource!(
            "https://buck2.build/docs/users/query/aquery/",
            "https://www.internalfb.com/intern/staticdocs/buck2/docs/users/query/aquery/",
        ),
        r#"
for more documentation about the functions available in aquery
expressions.

Examples:

Print the action producing a target's default output

`bz aquery //java/com/example/app:amazing`

List all the commands for run actions for building a target

`bz aquery 'kind(run, deps("//java/com/example/app:amazing+more"))' --output-attribute=cmd`

Dynamic outputs (`ctx.actions.dynamic_output`):

Currently, aquery interacts poorly with dynamic outputs. It may
return incorrect results or otherwise behave unexpectedly.
"#
    )
}

#[derive(Debug, clap::Parser)]
#[clap(
    name = "aquery",
    about = "Perform queries on the action graph (experimental)",
    long_about = help(),
    after_help = "Run `bz docs aquery` for detailed information about query functions such as allbuildfiles, allpaths, attrfilter, and more.",
    after_long_help = "",
    verbatim_doc_comment,
)]
pub struct AqueryCommand {
    #[clap(flatten)]
    query_common: CommonQueryOptions,

    #[clap(flatten)]
    target_cfg: TargetCfgOptions,

    #[clap(flatten)]
    common_opts: CommonCommandOptions,
}

#[async_trait(?Send)]
impl StreamingCommand for AqueryCommand {
    const COMMAND_NAME: &'static str = "aquery";

    async fn exec_impl(
        self,
        buckd: &mut BuckdClientConnector,
        matches: BuckArgMatches<'_>,
        ctx: &mut ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        let (query, query_args) = self.query_common.get_query();
        let unstable_output_format = self.query_common.output_format() as i32;
        let output_attributes = self.query_common.attributes.get()?;
        let context = ctx.client_context(matches, &self)?;

        let AqueryResponse {} = buckd
            .with_flushing()
            .aquery(
                AqueryRequest {
                    query,
                    query_args,
                    target_cfg: Some(self.target_cfg.target_cfg()),
                    context: Some(context),
                    output_attributes,
                    unstable_output_format,
                },
                events_ctx,
                ctx.console_interaction_stream(&self.common_opts.console_opts),
                &mut StdoutPartialResultHandler,
            )
            .await??;

        ExitResult::success()
    }

    fn console_opts(&self) -> &CommonConsoleOptions {
        &self.common_opts.console_opts
    }

    fn event_log_opts(&self) -> &CommonEventLogOptions {
        &self.common_opts.event_log_opts
    }

    fn build_config_opts(&self) -> &CommonBuildConfigurationOptions {
        &self.common_opts.config_opts
    }

    fn starlark_opts(&self) -> &CommonStarlarkOptions {
        &self.common_opts.starlark_opts
    }
}
