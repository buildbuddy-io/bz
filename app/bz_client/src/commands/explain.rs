/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_cli_proto::new_generic::ExplainRequest;
use bz_cli_proto::new_generic::NewGenericRequest;
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
use bz_common::artifact_upload::Bucket;
use bz_common::artifact_upload::ArtifactUploadClient;
use bz_event_log::file_names::get_local_logs;
use clap::Parser as _;
use tonic::async_trait;

use crate::commands::build::BuildCommand;

/// Generates web browser view that shows actions that ran in the last build
/// mapped to the target graph
#[derive(Debug, clap::Parser)]
#[clap(name = "explain")]
pub struct ExplainCommand {
    /// Output file path for profile data.
    ///
    /// File will be created if it does not exist, and overwritten if it does.
    #[clap(long, short = 'o')]
    output: Option<PathArg>,
    /// Upload the output to a configured artifact store.
    #[clap(long)]
    upload: bool,
    /// Add target code pointer. This invalidates cache, slowing things down
    #[clap(long)]
    stack: bool,
    /// Dev only: dump the flatbuffer info to file path
    #[clap(long, hide = true)]
    fbs_dump: Option<PathArg>,
}

// TODO: not sure I need StreamingCommand
#[async_trait(?Send)]
impl StreamingCommand for ExplainCommand {
    const COMMAND_NAME: &'static str = "explain";

    async fn exec_impl(
        self,
        buckd: &mut BuckdClientConnector,
        _matches: BuckArgMatches<'_>,
        ctx: &mut ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        if cfg!(windows) {
            return ExitResult::bail("Not implemented for windows");
        }

        let output = self.output.clone().map(|o| o.resolve(&ctx.working_dir));
        if output.is_none() && !self.upload {
            return ExitResult::bail(
                "Specify --output to write explain HTML locally, or --upload to use a configured artifact store.",
            );
        }

        // Get the most recent log
        let paths = ctx.paths()?;
        let logs = get_local_logs(&paths.log_dir())?; // oldest first
        let mut logs = logs
            .into_iter()
            .filter(|l| match l.command_from_filename().ok() {
                // Support only build commands for now
                Some(c) => c == "build",
                None => false,
            });

        let build_log = match logs.next_back() {
            Some(log) => log,
            None => {
                return ExitResult::bail(
                    "No recent build commands found, did you try building something first?",
                );
            }
        };

        // Check things are the same as last build
        let (invocation, _) = build_log.unpack_stream().await?;
        bz_client_ctx::eprintln!(
            "\nUsing last build invocation `bz {}`\n",
            invocation.command_line_args[1..].join(" ")
        )?;

        if invocation.working_dir != ctx.working_dir.to_string() {
            return ExitResult::bail(format!(
                "working dir mismatch {} and {}",
                invocation.working_dir, ctx.working_dir,
            ));
        }

        let uuid = invocation.trace_id;

        // We are interested in the args passed only to a build command
        let command = invocation.expanded_command_line_args;
        let build_index = command.iter().position(|word| word == "build");
        let index = match build_index {
            Some(index) => index,
            None => return ExitResult::bail("Only build command is supported"),
        };
        let command = &command[index..];

        // Parse retrived args
        let build_args = BuildCommand::parse_from(command);

        // TODO iguridi: get things like configs and target universe too
        let patterns = build_args.patterns();
        if patterns.len() != 1 {
            return ExitResult::bail("Only one target pattern is supported");
        }
        let target = patterns[0].to_owned();
        let target_universe = build_args.target_universe().clone();
        let target_cfg = build_args.target_cfg();

        let artifact_path = if self.upload {
            let artifact_client = ArtifactUploadClient::new().await?;
            if !artifact_client.is_available() {
                return ExitResult::bail("No artifact upload endpoint is configured in this build");
            }
            Some(format!("flat/{uuid}-explain.html"))
        } else {
            None
        };

        let mut context = ctx.empty_client_context("explain")?;
        context.target_call_stacks = self.stack;
        context.reuse_current_config = true;

        buckd
            .with_flushing()
            .new_generic(
                context,
                NewGenericRequest::Explain(ExplainRequest {
                    output,
                    target,
                    fbs_dump: self.fbs_dump.map(|x| x.resolve(&ctx.working_dir)),
                    artifact_path: artifact_path.clone(),
                    target_universe,
                    target_cfg,
                    log_path: build_log.path().to_owned(),
                }),
                events_ctx,
                None,
            )
            .await??;

        if let Some(p) = artifact_path {
            bz_client_ctx::eprintln!(
                "\nView html in your browser: {}\n",
                Bucket::EVENT_LOGS.artifact_url(&p)
            )?;
        }

        ExitResult::success()
    }

    fn existing_only() -> bool {
        true
    }

    fn console_opts(&self) -> &CommonConsoleOptions {
        CommonConsoleOptions::default_ref()
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
