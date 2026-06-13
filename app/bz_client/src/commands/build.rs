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
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use bz_cli_proto::BuildRequest;
use bz_cli_proto::BuildTarget;
use bz_cli_proto::TargetCfg;
use bz_cli_proto::build_request::BuildProviders;
use bz_cli_proto::build_request::ResponseOptions;
use bz_cli_proto::build_request::build_providers;
use bz_cli_proto::build_target::BuildOutput;
use bz_client_ctx::client_ctx::ClientCommandContext;
use bz_client_ctx::command_outcome::CommandOutcome;
use bz_client_ctx::common::BuckArgMatches;
use bz_client_ctx::common::CommonBuildConfigurationOptions;
use bz_client_ctx::common::CommonCommandOptions;
use bz_client_ctx::common::CommonEventLogOptions;
use bz_client_ctx::common::CommonStarlarkOptions;
use bz_client_ctx::common::PrintOutputsFormat;
use bz_client_ctx::common::build::CommonBuildOptions;
use bz_client_ctx::common::build::CommonOutputOptions;
use bz_client_ctx::common::target_cfg::TargetCfgWithUniverseOptions;
use bz_client_ctx::common::timeout::CommonTimeoutOptions;
use bz_client_ctx::common::ui::CommonConsoleOptions;
use bz_client_ctx::daemon::client::BuckdClientConnector;
use bz_client_ctx::daemon::client::NoPartialResultHandler;
use bz_client_ctx::events_ctx::EventsCtx;
use bz_client_ctx::exit_result::ClientIoError;
use bz_client_ctx::exit_result::ExitResult;
use bz_client_ctx::final_console::FinalConsole;
use bz_client_ctx::output_destination_arg::OutputDestinationArg;
use bz_client_ctx::streaming::StreamingCommand;
use bz_client_ctx::subscribers::recorder::BuildSummaryStats;
use bz_core::bz_env;
use bz_error::BuckErrorContext;
use bz_error::bz_error;
use dupe::Dupe;

use crate::commands::build::out::copy_to_out;
use crate::print::PrintOutputs;

mod out;

#[derive(Debug, clap::Parser)]
#[clap(name = "build", about = "Build the specified targets")]
pub struct BuildCommand {
    #[clap(flatten)]
    show_output: CommonOutputOptions,

    #[clap(
        long = "materializations",
        short = 'M',
        help = "Materialize (or skip) the final artifacts, bypassing buckconfig.",
        ignore_case = true,
        value_enum
    )]
    materializations: Option<FinalArtifactMaterializations>,

    #[clap(
        long = "upload-final-artifacts",
        help = "Upload (or skip) the final artifacts.",
        ignore_case = true,
        value_enum
    )]
    upload_final_artifacts: Option<FinalArtifactUploads>,

    #[allow(unused)]
    #[clap(
        long,
        group = "default-info",
        help = "Build default info (this is the default)"
    )]
    build_default_info: bool,

    #[clap(
        long,
        group = "default-info",
        help = "Do not build default info (this is not the default)"
    )]
    skip_default_info: bool,

    #[allow(unused)]
    #[clap(
        long,
        group = "run-info",
        help = "Build runtime dependencies (this is the default)"
    )]
    build_run_info: bool,

    #[clap(
        long,
        group = "run-info",
        help = "Do not build runtime dependencies (this is not the default)"
    )]
    skip_run_info: bool,

    #[clap(
        long,
        group = "test-info",
        help = "Build tests (this is not the default)"
    )]
    build_test_info: bool,

    #[allow(unused)]
    #[clap(
        long,
        group = "test-info",
        help = "Do not build tests (this is the default)"
    )]
    skip_test_info: bool,

    #[clap(
        long = "out",
        help = "Copy the output of the built target to this path (`-` to stdout)"
    )]
    output_path: Option<OutputDestinationArg>,

    #[clap(
        long = "show_result",
        default_value_t = 1,
        help = "Show build results for up to this many output-bearing targets."
    )]
    show_result: usize,

    #[clap(name = "TARGET_PATTERNS", help = "Patterns to build", value_hint = clap::ValueHint::Other)]
    patterns: Vec<String>,

    /// This option does nothing. It is here to keep compatibility with Buck1 and ci
    #[clap(long = "deep", hide = true)]
    _deep: bool,

    #[clap(flatten)]
    build_opts: CommonBuildOptions,

    #[clap(flatten)]
    target_cfg: TargetCfgWithUniverseOptions,

    #[clap(flatten)]
    timeout_options: CommonTimeoutOptions,

    #[clap(flatten)]
    common_opts: CommonCommandOptions,
}

impl BuildCommand {
    fn default_info(&self) -> build_providers::Action {
        if self.skip_default_info {
            return build_providers::Action::Skip;
        }
        build_providers::Action::Build
    }

    fn run_info(&self) -> build_providers::Action {
        if self.skip_run_info {
            return build_providers::Action::Skip;
        }
        build_providers::Action::BuildIfAvailable
    }

    fn test_info(&self) -> build_providers::Action {
        if self.build_test_info {
            return build_providers::Action::BuildIfAvailable;
        }
        build_providers::Action::Skip
    }

    pub(crate) fn patterns(&self) -> &Vec<String> {
        &self.patterns
    }

    pub(crate) fn target_universe(&self) -> &Vec<String> {
        &self.target_cfg.target_universe
    }

    pub(crate) fn target_cfg(&self) -> TargetCfg {
        self.target_cfg.target_cfg_with_default_platform(
            self.common_opts.config_opts.implied_target_platform(),
        )
    }

    fn should_print_build_output_locations(&self) -> bool {
        self.show_result > 0 && self.show_output.format().is_none() && self.output_path.is_none()
    }
}

#[derive(Debug, Clone, Dupe, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum FinalArtifactMaterializations {
    All,
    None,
}
pub trait MaterializationsToProto {
    fn to_proto(&self) -> bz_cli_proto::build_request::Materializations;
}
impl MaterializationsToProto for Option<FinalArtifactMaterializations> {
    fn to_proto(&self) -> bz_cli_proto::build_request::Materializations {
        match self {
            Some(FinalArtifactMaterializations::All) => {
                bz_cli_proto::build_request::Materializations::Materialize
            }
            Some(FinalArtifactMaterializations::None) => {
                bz_cli_proto::build_request::Materializations::Skip
            }
            None => bz_cli_proto::build_request::Materializations::Default,
        }
    }
}

#[derive(Debug, Clone, Dupe, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum FinalArtifactUploads {
    Always,
    Never,
}
pub trait UploadsToProto {
    fn to_proto(&self) -> bz_cli_proto::build_request::Uploads;
}
impl UploadsToProto for Option<FinalArtifactUploads> {
    fn to_proto(&self) -> bz_cli_proto::build_request::Uploads {
        match self {
            Some(FinalArtifactUploads::Always) => bz_cli_proto::build_request::Uploads::Always,
            Some(FinalArtifactUploads::Never) => bz_cli_proto::build_request::Uploads::Never,
            None => bz_cli_proto::build_request::Uploads::Never,
        }
    }
}

pub fn print_build_result(
    console: &FinalConsole,
    errors: &[bz_data::ErrorReport],
) -> bz_error::Result<()> {
    for error in errors {
        console.print_error(&error.message)?;
    }
    Ok(())
}

#[async_trait(?Send)]
impl StreamingCommand for BuildCommand {
    const COMMAND_NAME: &'static str = "build";

    async fn exec_impl(
        self,
        buckd: &mut BuckdClientConnector,
        matches: BuckArgMatches<'_>,
        ctx: &mut ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        let context = ctx.client_context(matches, &self)?;
        let print_build_output_locations = self.should_print_build_output_locations();
        let show_result = self.show_result;

        let result = buckd
            .with_flushing()
            .build(
                BuildRequest {
                    context: Some(context),
                    target_patterns: self.patterns.clone(),
                    target_cfg: Some(self.target_cfg()),
                    build_providers: Some(BuildProviders {
                        default_info: self.default_info() as i32,
                        run_info: self.run_info() as i32,
                        test_info: self.test_info() as i32,
                    }),
                    response_options: Some(ResponseOptions {
                        return_outputs: print_build_output_locations
                            || self.show_output.format().is_some()
                            || self.output_path.is_some(),
                    }),
                    build_opts: Some(
                        self.build_opts
                            .to_proto_with_remote_only(ctx.rbe_implies_remote_only())?,
                    ),
                    final_artifact_materializations: self.materializations.to_proto() as i32,
                    final_artifact_uploads: self.upload_final_artifacts.to_proto() as i32,
                    target_universe: self.target_cfg.target_universe,
                    timeout: self.timeout_options.overall_timeout()?,
                    run_args_missing_separator: false,
                },
                events_ctx,
                ctx.console_interaction_stream(&self.common_opts.console_opts),
                &mut NoPartialResultHandler,
            )
            .await;
        let success = match &result {
            Ok(CommandOutcome::Success(response)) => response.errors.is_empty(),
            Ok(CommandOutcome::Failure(_)) => false,
            Err(_) => false,
        };
        let has_build_response = matches!(&result, Ok(CommandOutcome::Success(_)));
        let build_output_locations = match &result {
            Ok(CommandOutcome::Success(response))
                if print_build_output_locations && response.errors.is_empty() =>
            {
                format_build_output_locations(&response.build_targets, show_result)
            }
            _ => None,
        };

        let console = self.common_opts.console_opts.final_console();
        let final_bes_results_url =
            bes_results_url(&self.common_opts.event_log_opts, ctx.buildbuddy_bes())
                .map(ToOwned::to_owned);
        let invocation_id = ctx.trace_id.to_string();
        print_build_id_after_superconsole(
            &console,
            ctx,
            events_ctx.used_superconsole,
            final_bes_results_url.is_some(),
        )?;
        let summary_stats = events_ctx
            .recorder
            .as_ref()
            .map(|recorder| recorder.build_summary_stats());

        if success {
            if self.patterns.is_empty() {
                console.print_warning("NO BUILD TARGET PATTERNS SPECIFIED")?;
            } else {
                print_build_succeeded_with_stats(
                    &console,
                    ctx,
                    summary_stats.as_ref(),
                    build_output_locations.as_deref(),
                )?;
            }
        } else if !has_build_response {
            print_build_failed_with_stats(
                &console,
                ctx.start_time.elapsed().unwrap_or_default(),
                summary_stats.as_ref(),
            )?;
            print_bes_results_url_after_build(
                &console,
                final_bes_results_url.as_deref(),
                &invocation_id,
            )?;
        }

        if bz_env!("BUCK2_TEST_BUILD_ERROR", bool, applicability = testing)? {
            return bz_error!(
                bz_error::ErrorTag::TestOnly,
                "Injected Build Response Error"
            )
            .into();
        }

        // Most build errors are returned in the `result.errors` field, but some are not and printed
        // here.
        let response = result??;

        print_build_result(&console, &response.errors)?;

        let mut stdout = Vec::new();

        if let Some(build_report) = response.serialized_build_report {
            stdout.extend(build_report.as_bytes());
            writeln!(&mut stdout)?;
        }

        if let Some(format) = self.show_output.format() {
            print_outputs(
                &mut stdout,
                &response.build_targets,
                self.show_output.is_full().then_some(response.project_root),
                format,
            )?;
        }

        let res = if success {
            if let Some(stdout) = &self.output_path {
                copy_to_out(
                    &response.build_targets,
                    ctx.paths()?.project_root(),
                    &ctx.working_dir,
                    stdout,
                )
                .await
                .buck_error_context("Error requesting specific output path for --out")?;
            }

            ExitResult::success()
        } else {
            print_build_failed_with_stats(
                &console,
                ctx.start_time.elapsed().unwrap_or_default(),
                summary_stats.as_ref(),
            )?;
            ExitResult::from_command_result_errors(response.errors)
        };

        print_bes_results_url_after_build(
            &console,
            final_bes_results_url.as_deref(),
            &invocation_id,
        )?;

        res.with_stdout(stdout)
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

    fn build_event_protocol_target_patterns(&self) -> Vec<String> {
        self.patterns.clone()
    }
}

pub(crate) fn print_build_succeeded(
    console: &FinalConsole,
    ctx: &ClientCommandContext<'_>,
    extra: Option<&str>,
) -> bz_error::Result<()> {
    print_build_succeeded_with_stats(console, ctx, None, extra)
}

fn print_build_succeeded_with_stats(
    console: &FinalConsole,
    ctx: &ClientCommandContext<'_>,
    stats: Option<&BuildSummaryStats>,
    extra: Option<&str>,
) -> bz_error::Result<()> {
    if !ctx.verbosity.print_success_message() {
        return Ok(());
    }

    let suffix = print_success_extra(console, extra)?;
    print_build_timing_summary(console, ctx.start_time.elapsed().unwrap_or_default(), stats)?;
    print_build_process_summary(console, stats)?;

    let mut message = if let Some(stats) = stats {
        format!(
            "Build completed successfully, {} {}",
            format_count(stats.total_actions()),
            pluralize("total action", stats.total_actions())
        )
    } else {
        "Build completed successfully".to_owned()
    };
    message.push_str(suffix);
    console.print_info_prefix(&message)?;
    Ok(())
}

fn print_success_extra<'a>(
    console: &FinalConsole,
    extra: Option<&'a str>,
) -> bz_error::Result<&'a str> {
    let Some(extra) = extra else {
        return Ok("");
    };
    if let Some(output_locations) = extra.strip_prefix('\n') {
        let output_locations = output_locations.trim_matches('\n');
        if !output_locations.is_empty() {
            console.print_stderr("")?;
            console.print_stderr(output_locations)?;
            console.print_stderr("")?;
        }
        Ok("")
    } else {
        Ok(extra)
    }
}

fn print_build_timing_summary(
    console: &FinalConsole,
    elapsed: Duration,
    stats: Option<&BuildSummaryStats>,
) -> bz_error::Result<()> {
    let mut message = format!("Elapsed time: {:.3}s", elapsed.as_secs_f64());
    if let Some(critical_path) = stats.and_then(|stats| stats.critical_path_duration) {
        message.push_str(&format!(
            ", Critical Path: {:.2}s",
            critical_path.as_secs_f64()
        ));
    }
    console.print_info_prefix(&message)
}

fn print_build_process_summary(
    console: &FinalConsole,
    stats: Option<&BuildSummaryStats>,
) -> bz_error::Result<()> {
    let Some(stats) = stats else {
        return Ok(());
    };
    let Some(summary) = stats.process_summary() else {
        return Ok(());
    };
    console.print_info_prefix(&summary)
}

fn pluralize(noun: &str, count: u64) -> String {
    format!("{noun}{}", if count == 1 { "" } else { "s" })
}

fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted
}

/// Reprints the build ID after superconsole exits so it remains visible in scrollback.
pub(crate) fn print_build_id_after_superconsole(
    console: &FinalConsole,
    ctx: &ClientCommandContext<'_>,
    used_superconsole: bool,
    printed_bes_results_url: bool,
) -> bz_error::Result<()> {
    if should_reprint_build_id(used_superconsole, printed_bes_results_url) {
        console.print_stderr(&format!("Build ID: {}", ctx.trace_id))?;
    }
    Ok(())
}

pub(crate) fn has_bes_results_url(
    event_log_opts: &CommonEventLogOptions,
    buildbuddy_bes: bool,
) -> bool {
    bes_results_url(event_log_opts, buildbuddy_bes).is_some()
}

pub(crate) fn bes_results_url(
    event_log_opts: &CommonEventLogOptions,
    buildbuddy_bes: bool,
) -> Option<&str> {
    event_log_opts
        .bes_backend_with_buildbuddy_default(buildbuddy_bes)
        .and_then(|_| event_log_opts.bes_results_url_with_buildbuddy_default(buildbuddy_bes))
}

fn print_bes_results_url_after_build(
    console: &FinalConsole,
    results_url: Option<&str>,
    invocation_id: &str,
) -> bz_error::Result<()> {
    let Some(results_url) = results_url else {
        return Ok(());
    };

    console.print_info_prefix(&format!(
        "Streaming build results to: {}",
        bes_invocation_url(results_url, invocation_id)
    ))
}

fn bes_invocation_url(results_url: &str, invocation_id: &str) -> String {
    let separator = if results_url.ends_with('/') { "" } else { "/" };
    format!("{results_url}{separator}{invocation_id}")
}

fn should_reprint_build_id(used_superconsole: bool, printed_bes_results_url: bool) -> bool {
    used_superconsole && !printed_bes_results_url
}

pub(crate) fn print_build_failed(console: &FinalConsole) -> bz_error::Result<()> {
    console.print_error_prefix("Build did NOT complete successfully")
}

fn print_build_failed_with_stats(
    console: &FinalConsole,
    elapsed: Duration,
    stats: Option<&BuildSummaryStats>,
) -> bz_error::Result<()> {
    print_build_timing_summary(console, elapsed, stats)?;
    print_build_process_summary(console, stats)?;
    console.print_error_prefix("Build did NOT complete successfully")
}

pub(crate) fn print_outputs(
    out: impl Write,
    targets: &[BuildTarget],
    root_path: Option<String>,
    format: PrintOutputsFormat,
) -> Result<(), ClientIoError> {
    let root_path = root_path.map(PathBuf::from);
    let mut print = PrintOutputs::new(out, root_path, format)?;

    for build_target in targets {
        // just print the default info for build command
        let outputs = build_target
            .outputs
            .iter()
            .filter(|output| output_is_default_info_main_artifact(output));

        // only print the unconfigured target for now until we migrate everything to support
        // also printing configurations
        if outputs.clone().count() > 1 {
            // FIXME(JakobDegen): Why exactly do we not show the path?
            print.output(&build_target.target, None)?;
            continue;
        }
        for output in outputs {
            print.output(&build_target.target, Some(&output.path))?;
        }
    }

    print.finish()
}

fn output_is_default_info_main_artifact(output: &BuildOutput) -> bool {
    output
        .providers
        .as_ref()
        .is_none_or(|p| p.default_info && !p.other)
}

fn format_build_output_locations(targets: &[BuildTarget], show_result: usize) -> Option<String> {
    if show_result == 0 {
        return None;
    }

    let targets_with_outputs = targets
        .iter()
        .map(|target| {
            let outputs = target
                .outputs
                .iter()
                .filter(|output| output_is_default_info_main_artifact(output))
                .collect::<Vec<_>>();
            (target, outputs)
        })
        .filter(|(_, outputs)| !outputs.is_empty())
        .collect::<Vec<_>>();

    if targets_with_outputs.len() > show_result {
        return None;
    }

    let omit_nothing_to_build = targets.len() > show_result;
    let mut lines = Vec::new();

    for (target, outputs) in targets_with_outputs {
        lines.push(format!("Target {} up-to-date:", target.target));
        for output in outputs {
            lines.push(format!("  {}", output.path));
        }
    }

    if !omit_nothing_to_build {
        for target in targets {
            let outputs = target
                .outputs
                .iter()
                .filter(|output| output_is_default_info_main_artifact(output))
                .collect::<Vec<_>>();

            if outputs.is_empty() {
                lines.push(format!(
                    "Target {} up-to-date (nothing to build)",
                    target.target
                ));
            }
        }
    }

    (!lines.is_empty()).then(|| format!("\n {}", lines.join("\n ")))
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use build_providers::Action;
    use bz_cli_proto::build_target::BuildOutput;
    use bz_cli_proto::build_target::build_output::BuildOutputProviders;
    use clap::Parser;

    use super::*;

    fn parse(args: &[&str]) -> bz_error::Result<BuildCommand> {
        Ok(BuildCommand::try_parse_from(
            std::iter::once("program").chain(args.iter().copied()),
        )?)
    }

    fn build_output(path: &str, providers: BuildOutputProviders) -> BuildOutput {
        BuildOutput {
            path: path.to_owned(),
            providers: Some(providers),
        }
    }

    fn build_target(target: &str, outputs: Vec<BuildOutput>) -> BuildTarget {
        BuildTarget {
            target: target.to_owned(),
            run_args: Vec::new(),
            outputs,
            configuration: "cfg".to_owned(),
            configured_graph_size: None,
            target_rule_type_name: None,
            run_environment: Vec::new(),
            run_inherited_environment: Vec::new(),
            run: None,
        }
    }

    #[test]
    fn infos_default() -> bz_error::Result<()> {
        let opts = parse(&[])?;

        assert_eq!(opts.default_info(), Action::Build);
        assert_eq!(opts.run_info(), Action::BuildIfAvailable);
        assert_eq!(opts.test_info(), Action::Skip);

        Ok(())
    }

    #[test]
    fn formats_build_output_locations() {
        let default_main = BuildOutputProviders {
            default_info: true,
            run_info: false,
            other: false,
            test_info: false,
        };
        let default_other = BuildOutputProviders {
            default_info: true,
            run_info: false,
            other: true,
            test_info: false,
        };
        let run_only = BuildOutputProviders {
            default_info: false,
            run_info: true,
            other: false,
            test_info: false,
        };

        let targets = vec![build_target(
            "root//:one",
            vec![
                build_output("buck-out/v2/art/root/cfg/one", default_main.clone()),
                build_output("buck-out/v2/art/root/cfg/one.other", default_other),
                build_output("buck-out/v2/art/root/cfg/one.run", run_only),
            ],
        )];

        assert_eq!(
            format_build_output_locations(&targets, 1).as_deref(),
            Some("\n Target root//:one up-to-date:\n   buck-out/v2/art/root/cfg/one")
        );
    }

    #[test]
    fn formats_nothing_to_build_when_under_show_result_limit() {
        let targets = vec![build_target(
            "root//:run",
            vec![build_output(
                "buck-out/v2/art/root/cfg/run",
                BuildOutputProviders {
                    default_info: false,
                    run_info: true,
                    other: false,
                    test_info: false,
                },
            )],
        )];

        assert_eq!(
            format_build_output_locations(&targets, 1).as_deref(),
            Some("\n Target root//:run up-to-date (nothing to build)")
        );
    }

    #[test]
    fn skips_build_output_locations_over_show_result_limit() {
        let default_main = BuildOutputProviders {
            default_info: true,
            run_info: false,
            other: false,
            test_info: false,
        };
        let targets = vec![
            build_target(
                "root//:one",
                vec![build_output(
                    "buck-out/v2/art/root/cfg/one",
                    default_main.clone(),
                )],
            ),
            build_target(
                "root//:two",
                vec![build_output("buck-out/v2/art/root/cfg/two", default_main)],
            ),
        ];

        assert_eq!(format_build_output_locations(&targets, 1), None);
        assert_eq!(
            format_build_output_locations(&targets, 2).as_deref(),
            Some(
                "\n Target root//:one up-to-date:\n   buck-out/v2/art/root/cfg/one\n Target root//:two up-to-date:\n   buck-out/v2/art/root/cfg/two"
            )
        );
    }

    #[test]
    fn build_output_locations_follow_show_result_and_output_flags() -> bz_error::Result<()> {
        assert!(parse(&["//app:bz"])?.should_print_build_output_locations());
        assert!(parse(&[":bazelisk"])?.should_print_build_output_locations());
        assert!(parse(&["//app:a", "//app:b"])?.should_print_build_output_locations());
        assert!(parse(&["//..."])?.should_print_build_output_locations());
        assert!(parse(&["//app:all"])?.should_print_build_output_locations());
        assert!(parse(&["//app:*"])?.should_print_build_output_locations());

        assert!(!parse(&["//app:a", "--show_result=0"])?.should_print_build_output_locations());
        assert!(!parse(&["//app:a", "--show-output"])?.should_print_build_output_locations());
        assert!(!parse(&["//app:a", "--out", "out"])?.should_print_build_output_locations());

        Ok(())
    }

    #[test]
    fn infos_noop() -> bz_error::Result<()> {
        let opts = parse(&[
            "--skip-test-info",
            "--build-default-info",
            "--build-run-info",
        ])?;

        assert_eq!(opts.default_info(), Action::Build);
        assert_eq!(opts.run_info(), Action::BuildIfAvailable);
        assert_eq!(opts.test_info(), Action::Skip);

        Ok(())
    }

    #[test]
    fn infos_configure() -> bz_error::Result<()> {
        let opts = parse(&["--skip-default-info"])?;
        assert_eq!(opts.default_info(), Action::Skip);

        let opts = parse(&["--skip-run-info"])?;
        assert_eq!(opts.run_info(), Action::Skip);

        let opts = parse(&["--build-test-info"])?;
        assert_eq!(opts.test_info(), Action::BuildIfAvailable);

        Ok(())
    }

    #[test]
    fn infos_validation() -> bz_error::Result<()> {
        // Test duplicate args
        assert_matches!(
            parse(&["--build-default-info", "--skip-default-info"]),
            Err(..)
        );
        assert_matches!(parse(&["--build-run-info", "--skip-run-info"]), Err(..));
        assert_matches!(parse(&["--build-test-info", "--skip-test-info"]), Err(..));

        // Test args across all groups.
        assert_matches!(
            parse(&[
                "--skip-default-info",
                "--skip-run-info",
                "--build-test-info"
            ]),
            Ok(..)
        );

        Ok(())
    }

    #[test]
    fn bep_sets_buildbuddy_bes_defaults() -> bz_error::Result<()> {
        let opts = parse(&["--bep"])?;
        let event_log_opts = &opts.common_opts.event_log_opts;

        assert_eq!(event_log_opts.bes_backend(), Some("remote.buildbuddy.dev"));
        assert_eq!(
            event_log_opts.bes_results_url(),
            Some("https://app.buildbuddy.dev/invocation/")
        );

        Ok(())
    }

    #[test]
    fn bes_sets_buildbuddy_bes_defaults() -> bz_error::Result<()> {
        let opts = parse(&["--bes"])?;
        let event_log_opts = &opts.common_opts.event_log_opts;

        assert_eq!(event_log_opts.bes_backend(), Some("remote.buildbuddy.dev"));
        assert_eq!(
            event_log_opts.bes_results_url(),
            Some("https://app.buildbuddy.dev/invocation/")
        );

        Ok(())
    }

    #[test]
    fn bep_allows_explicit_bes_overrides() -> bz_error::Result<()> {
        let opts = parse(&[
            "--bep",
            "--bes_backend=grpc://example.com",
            "--bes_results_url=https://example.com/invocation/",
        ])?;
        let event_log_opts = &opts.common_opts.event_log_opts;

        assert_eq!(event_log_opts.bes_backend(), Some("grpc://example.com"));
        assert_eq!(
            event_log_opts.bes_results_url(),
            Some("https://example.com/invocation/")
        );

        Ok(())
    }

    #[test]
    fn bes_results_url_suppresses_final_build_id_reprint() -> bz_error::Result<()> {
        let opts = parse(&["--bes"])?;
        let event_log_opts = &opts.common_opts.event_log_opts;

        assert!(has_bes_results_url(event_log_opts, false));
        assert!(!should_reprint_build_id(true, true));

        Ok(())
    }

    #[test]
    fn final_build_id_reprint_stays_enabled_without_bes_results_url() -> bz_error::Result<()> {
        let opts = parse(&[])?;
        let event_log_opts = &opts.common_opts.event_log_opts;

        assert!(!has_bes_results_url(event_log_opts, false));
        assert!(should_reprint_build_id(true, false));
        assert!(!should_reprint_build_id(false, false));

        Ok(())
    }

    #[test]
    fn buildbuddy_default_counts_as_bes_results_url() -> bz_error::Result<()> {
        let opts = parse(&[])?;

        assert!(has_bes_results_url(&opts.common_opts.event_log_opts, true));

        Ok(())
    }
}
