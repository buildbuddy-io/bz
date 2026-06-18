/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use async_trait::async_trait;
use bz_cli_proto::BuildRequest;
use bz_cli_proto::BuildTarget;
use bz_cli_proto::build_request::BuildProviders;
use bz_cli_proto::build_request::Materializations;
use bz_cli_proto::build_request::Uploads;
use bz_cli_proto::build_request::build_providers;
use bz_cli_proto::build_target::RunSpec;
use bz_client_ctx::client_ctx::ClientCommandContext;
use bz_client_ctx::command_outcome::CommandOutcome;
use bz_client_ctx::common::BuckArgMatches;
use bz_client_ctx::common::CommonBuildConfigurationOptions;
use bz_client_ctx::common::CommonCommandOptions;
use bz_client_ctx::common::CommonEventLogOptions;
use bz_client_ctx::common::CommonStarlarkOptions;
use bz_client_ctx::common::build::CommonBuildOptions;
use bz_client_ctx::common::target_cfg::TargetCfgWithUniverseOptions;
use bz_client_ctx::common::ui::CommonConsoleOptions;
use bz_client_ctx::daemon::client::BuckdClientConnector;
use bz_client_ctx::daemon::client::NoPartialResultHandler;
use bz_client_ctx::events_ctx::EventsCtx;
use bz_client_ctx::exit_result::ExitResult;
use bz_client_ctx::path_arg::PathArg;
use bz_client_ctx::streaming::StreamingCommand;
use bz_common::argv::Argv;
use bz_common::argv::SanitizedArgv;
use bz_error::BuckErrorContext;
use bz_error::conversion::from_any_with_tag;
use bz_fs::paths::abs_path::AbsPathBuf;
use bz_hash::StdBuckHashMap;
use bz_hash::StdBuckHashSet;
use bz_wrapper_common::BUCK_WRAPPER_START_TIME_ENV_VAR;
use bz_wrapper_common::BUCK_WRAPPER_UUID_ENV_VAR;
use bz_wrapper_common::BUCK2_WRAPPER_ENV_VAR;
use serde::Serialize;

use crate::commands::build::has_bes_results_url;
use crate::commands::build::print_build_failed;
use crate::commands::build::print_build_id_after_superconsole;
use crate::commands::build::print_build_result;
use crate::commands::build::print_build_succeeded;

const BAZEL_RUN_ENV_VARS_TO_CLEAR: &[&str] = &[
    "JAVA_RUNFILES",
    "RUNFILES_DIR",
    "RUNFILES_MANIFEST_FILE",
    "RUNFILES_MANIFEST_ONLY",
    "TEST_SRCDIR",
];
const BAZEL_RUNFILES_MANIFEST: &str = "MANIFEST";

/// Build and run the selected target.
///
/// Use `--` to separate arguments to the target from arguments to bz:
///
/// bz run //my/target -- --arg1 --arg2
///
/// The Build ID for the underlying build execution is made available to the target in
/// the `BUCK_RUN_BUILD_ID` environment variable.
#[derive(Debug, clap::Parser)]
// FIXME(JakobDegen): Remove usage override once soft error is removed
#[clap(
    name = "run",
    trailing_var_arg = true,
    override_usage = "bz run [OPTIONS] <TARGET> [-- <TARGET_ARGS>...]"
)]
pub struct RunCommand {
    #[clap(
        long = "command-args-file",
        help = "Write the command to a file instead of executing it.",
        group = "exec_options"
    )]
    command_args_file: Option<String>,

    #[clap(
        long = "chdir",
        help = "Set the current working directory of the executable being run",
        group = "exec_options"
    )]
    chdir: Option<PathArg>,

    /// Instead of running the command, print out the command
    /// formatted for shell interpolation, use as: $(bz run --emit-shell ...)
    #[clap(long, group = "exec_options")]
    emit_shell: bool,

    #[clap(
        long = "run_in_cwd",
        help = "Run from the current working directory instead of the executable runfiles tree"
    )]
    run_in_cwd: bool,

    #[clap(
        long = "run_env",
        value_name = "VAR[=VALUE]",
        help = "Environment variable to set or inherit when running the target"
    )]
    run_env: Vec<String>,

    #[clap(
        long = "run_under",
        value_name = "COMMAND",
        help = "Prefix the target command with another command"
    )]
    run_under: Option<String>,

    #[clap(name = "TARGET", help = "Target to build and run", value_hint = clap::ValueHint::Other)]
    target: String,

    #[clap(
        name = "TARGET_ARGS",
        help = "Additional arguments passed to the target when running it"
    )]
    extra_run_args: Vec<String>,

    #[clap(flatten)]
    build_opts: CommonBuildOptions,

    #[clap(flatten)]
    target_cfg: TargetCfgWithUniverseOptions,

    #[clap(flatten)]
    common_opts: CommonCommandOptions,
}

#[async_trait(?Send)]
impl StreamingCommand for RunCommand {
    const COMMAND_NAME: &'static str = "run";

    async fn exec_impl(
        self,
        buckd: &mut BuckdClientConnector,
        matches: BuckArgMatches<'_>,
        ctx: &mut ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        let run_args_missing_separator =
            // We will soon require a separator before the start of the runs args.
            // Check that the expanded argv has a separator (so we catch them in @ files), 
            // and if not print a warning.
            !self.extra_run_args.is_empty() && !ctx.expanded_argv_has_separator();

        let parsed_run_under = self.run_under.as_deref().map(parse_run_under).transpose()?;
        let target_patterns = run_target_patterns(&self.target, parsed_run_under.as_ref());
        let context = ctx.client_context(matches, &self)?;
        let has_target_universe = !self.target_cfg.target_universe.is_empty();
        // TODO(rafaelc): fail fast on the daemon if the target doesn't have RunInfo
        let response = buckd
            .with_flushing()
            .build(
                BuildRequest {
                    context: Some(context),
                    // TODO(wendyy): glob patterns should be prohibited, and command should fail before the build event happens.
                    target_patterns,
                    target_cfg: Some(self.target_cfg.target_cfg_with_default_platform(
                        self.common_opts.config_opts.implied_target_platform(),
                    )),
                    build_providers: Some(BuildProviders {
                        default_info: build_providers::Action::Skip as i32,
                        run_info: build_providers::Action::Build as i32,
                        test_info: build_providers::Action::Skip as i32,
                    }),
                    response_options: None,
                    build_opts: Some(
                        self.build_opts
                            .to_proto_with_remote_only(ctx.rbe_implies_remote_only())?,
                    ),
                    final_artifact_materializations: Materializations::Materialize as i32,
                    final_artifact_uploads: Uploads::Never as i32,
                    target_universe: self.target_cfg.target_universe,
                    timeout: None, // TODO: maybe it shouild be supported here?
                    run_args_missing_separator,
                },
                events_ctx,
                ctx.console_interaction_stream(&self.common_opts.console_opts),
                &mut NoPartialResultHandler,
            )
            .await;

        let console = self.common_opts.console_opts.final_console();
        let success = match &response {
            Ok(CommandOutcome::Success(response)) => response.errors.is_empty(),
            Ok(CommandOutcome::Failure(_)) => false,
            Err(_) => false,
        };
        if !success {
            print_build_failed(&console)?;
        }
        let response = response??;
        print_build_result(&console, &response.errors)?;

        if !success {
            return ExitResult::from_command_result_errors(response.errors);
        }

        if has_target_universe && response.build_targets.is_empty() {
            return ExitResult::err(
                RunCommandError::TargetNotFoundInTargetUniverse(self.target).into(),
            );
        }

        let (build_target, run_under_target) = select_run_targets(
            &response.build_targets,
            &self.target,
            parsed_run_under.as_ref(),
        )?;
        let run_spec = build_target
            .run
            .as_ref()
            .ok_or_else(|| RunCommandError::NonBinaryRule(self.target.clone()))?;
        let mut run_args = build_run_args(run_spec, ctx, &self.extra_run_args)?;
        let project_root = ctx.paths()?.project_root().root().as_path().to_path_buf();
        materialize_runfiles_tree(run_spec, &project_root)?;
        if let Some(run_under_prefix) = run_under_prefix(
            parsed_run_under.as_ref(),
            run_under_target,
            ctx,
            &project_root,
        )? {
            run_args = apply_run_under(run_under_prefix.as_str(), run_args)?;
        }
        let (run_environment, run_environment_to_clear) =
            bazel_run_environment(ctx, run_spec, &self.run_env)?;

        let extra = if !self.emit_shell {
            Some(" - starting your binary")
        } else {
            None
        };

        let printed_bes_results_url = has_bes_results_url(
            &self.common_opts.event_log_opts,
            ctx.buildbuddy_bes(),
            ctx.dev(),
        );
        print_build_id_after_superconsole(
            &console,
            ctx,
            events_ctx.used_superconsole,
            printed_bes_results_url,
        )?;
        print_build_succeeded(&console, ctx, extra)?;

        // Special case for recursive invocations of buck; `BUCK2_WRAPPER` is set by wrapper scripts that execute
        // Buck2. We're not a wrapper script, so we unset it to prevent `run` from inheriting it.
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var(BUCK2_WRAPPER_ENV_VAR) };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var(BUCK_WRAPPER_UUID_ENV_VAR) };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var(BUCK_WRAPPER_START_TIME_ENV_VAR) };

        if let Some(file_path) = self.command_args_file {
            let mut output = File::create(&file_path).with_buck_error_context(|| {
                format!("Failed to create/open `{file_path}` to print command")
            })?;

            let command = CommandArgsFile {
                path: run_args[0].clone(),
                argv: run_args,
                envp: command_envp(&run_environment, &run_environment_to_clear),
                is_fix_script: false,
                print_command: false,
            };
            let serialized = serde_json::to_string(&command)
                .buck_error_context("Failed to serialize command")?;
            output
                .write_all(serialized.as_bytes())
                .buck_error_context("Failed to write command")?;

            return ExitResult::success();
        }

        if self.emit_shell {
            if cfg!(unix) {
                bz_client_ctx::println!(
                    "{}",
                    shlex::try_join(run_args.iter().map(|a| a.as_str()))
                        .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Tier0))?
                )?;
                return ExitResult::success();
            } else {
                return ExitResult::err(RunCommandError::EmitShellNotSupportedOnWindows.into());
            }
        }

        let chdir = if let Some(chdir) = self.chdir {
            Some(chdir.resolve(&ctx.working_dir))
        } else if self.run_in_cwd {
            Some(ctx.working_dir.path().to_buf().into_abs_path_buf())
        } else if run_spec.working_directory.is_empty() {
            None
        } else {
            Some(resolve_run_path_to_abs(
                &run_spec.working_directory,
                &project_root,
            )?)
        };

        ExitResult::exec_with_env(
            run_args[0].clone().into(),
            run_args.into_iter().map(|arg| arg.into()).collect(),
            chdir,
            run_environment,
            run_environment_to_clear,
        )
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

    fn sanitize_argv(&self, argv: Argv) -> SanitizedArgv {
        let to_redact: StdBuckHashSet<_> = self.extra_run_args.iter().collect();
        argv.redacted(to_redact)
    }

    fn build_event_protocol_target_patterns(&self) -> Vec<String> {
        let run_under = self.run_under.as_deref().and_then(|run_under| {
            parse_run_under(run_under)
                .ok()
                .and_then(|run_under| match run_under {
                    ParsedRunUnder::Target { target, .. } => Some(target),
                    ParsedRunUnder::Prefix(_) => None,
                })
        });
        let mut patterns = vec![self.target.clone()];
        if let Some(run_under) = run_under
            && run_under != self.target
        {
            patterns.push(run_under);
        }
        patterns
    }
}

#[derive(Serialize)]
struct CommandArgsFile {
    path: String,
    argv: Vec<String>,
    envp: StdBuckHashMap<String, String>,
    // Not used. For buck_v1 back compatibility only.
    is_fix_script: bool,
    // Not used. For buck_v1 back compatibility only.
    print_command: bool,
}

#[derive(Clone, Debug)]
enum ParsedRunUnder {
    Prefix(String),
    Target {
        target: String,
        options: Vec<String>,
    },
}

fn parse_run_under(run_under: &str) -> bz_error::Result<ParsedRunUnder> {
    let tokens = shlex::split(run_under)
        .ok_or_else(|| RunCommandError::InvalidRunUnder(run_under.to_owned()))?;
    let Some(first) = tokens.first() else {
        return Err(RunCommandError::InvalidRunUnder(run_under.to_owned()).into());
    };
    if is_run_under_target(first) {
        Ok(ParsedRunUnder::Target {
            target: first.clone(),
            options: tokens[1..].to_vec(),
        })
    } else {
        Ok(ParsedRunUnder::Prefix(run_under.to_owned()))
    }
}

fn is_run_under_target(token: &str) -> bool {
    token.starts_with("//") || token.starts_with(':') || token.starts_with('@')
}

fn run_target_patterns(target: &str, run_under: Option<&ParsedRunUnder>) -> Vec<String> {
    let mut patterns = vec![target.to_owned()];
    if let Some(ParsedRunUnder::Target {
        target: run_under_target,
        ..
    }) = run_under
        && run_under_target != target
    {
        patterns.push(run_under_target.clone());
    }
    patterns
}

fn select_run_targets<'a>(
    targets: &'a [BuildTarget],
    requested_target: &str,
    run_under: Option<&ParsedRunUnder>,
) -> bz_error::Result<(&'a BuildTarget, Option<&'a BuildTarget>)> {
    let Some(run_under_target) = run_under.and_then(|run_under| match run_under {
        ParsedRunUnder::Target { target, .. } => Some(target.as_str()),
        ParsedRunUnder::Prefix(_) => None,
    }) else {
        if targets.len() > 1 {
            return Err(RunCommandError::MultipleTargets.into());
        }
        return targets
            .first()
            .map(|target| (target, None))
            .ok_or_else(|| RunCommandError::NonBinaryRule(requested_target.to_owned()).into());
    };

    let main = find_unique_target(targets, requested_target)?
        .ok_or_else(|| RunCommandError::TargetNotFound(requested_target.to_owned()))?;
    let under = find_unique_target(targets, run_under_target)?
        .ok_or_else(|| RunCommandError::RunUnderTargetNotFound(run_under_target.to_owned()))?;
    Ok((main, Some(under)))
}

fn find_unique_target<'a>(
    targets: &'a [BuildTarget],
    pattern: &str,
) -> bz_error::Result<Option<&'a BuildTarget>> {
    let mut matches = targets
        .iter()
        .filter(|target| target_matches_pattern(&target.target, pattern));
    let Some(target) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(RunCommandError::AmbiguousRunTarget(pattern.to_owned()).into());
    }
    Ok(Some(target))
}

fn target_matches_pattern(target: &str, pattern: &str) -> bool {
    if target == pattern || target.ends_with(pattern) {
        return true;
    }
    if pattern.starts_with(':') {
        return target.ends_with(pattern);
    }
    if !pattern.contains("//") && !pattern.contains(':') {
        return target.ends_with(&format!(":{pattern}"))
            || target.ends_with(&format!("//{pattern}:{pattern}"));
    }
    false
}

fn build_run_args(
    run_spec: &RunSpec,
    ctx: &ClientCommandContext<'_>,
    extra_run_args: &[String],
) -> bz_error::Result<Vec<String>> {
    let mut run_args = Vec::with_capacity(1 + run_spec.target_args.len() + extra_run_args.len());
    run_args.push(resolve_run_spec_executable(run_spec, ctx)?);
    run_args.extend(run_spec.target_args.iter().cloned());
    run_args.extend(extra_run_args.iter().cloned());
    Ok(run_args)
}

fn run_under_prefix(
    run_under: Option<&ParsedRunUnder>,
    run_under_target: Option<&BuildTarget>,
    ctx: &ClientCommandContext<'_>,
    project_root: &Path,
) -> bz_error::Result<Option<String>> {
    match run_under {
        None => Ok(None),
        Some(ParsedRunUnder::Prefix(prefix)) => Ok(Some(prefix.clone())),
        Some(ParsedRunUnder::Target { target, options }) => {
            let build_target = run_under_target
                .ok_or_else(|| RunCommandError::RunUnderTargetNotFound(target.clone()))?;
            let run_spec = build_target
                .run
                .as_ref()
                .ok_or_else(|| RunCommandError::RunUnderTargetNotBinary(target.clone()))?;
            materialize_runfiles_tree(run_spec, project_root)?;
            let mut prefix_args = Vec::with_capacity(1 + options.len());
            prefix_args.push(resolve_run_spec_executable(run_spec, ctx)?);
            prefix_args.extend(options.iter().cloned());
            Ok(Some(shell_join_args(&prefix_args)?))
        }
    }
}

fn resolve_run_spec_executable(
    run_spec: &RunSpec,
    ctx: &ClientCommandContext<'_>,
) -> bz_error::Result<String> {
    let (resolved_executable, should_validate_executable) =
        resolve_run_executable_path(&run_spec.executable, ctx)?;
    if should_validate_executable {
        validate_run_executable(Path::new(&resolved_executable))?;
    }
    Ok(resolved_executable)
}

fn apply_run_under(run_under: &str, run_args: Vec<String>) -> bz_error::Result<Vec<String>> {
    #[cfg(unix)]
    {
        let run_command = shell_join_args(&run_args)?;
        Ok(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("{run_under} {run_command}"),
        ])
    }

    #[cfg(not(unix))]
    {
        let mut prefix = shlex::split(run_under)
            .ok_or_else(|| RunCommandError::InvalidRunUnder(run_under.to_owned()))?;
        if prefix.is_empty() {
            return Err(RunCommandError::InvalidRunUnder(run_under.to_owned()).into());
        }
        prefix.extend(run_args);
        Ok(prefix)
    }
}

fn shell_join_args(args: &[String]) -> bz_error::Result<String> {
    shlex::try_join(args.iter().map(String::as_str))
        .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Tier0))
}

fn materialize_runfiles_tree(run_spec: &RunSpec, project_root: &Path) -> bz_error::Result<()> {
    if run_spec.runfiles_dir.is_empty() {
        return Ok(());
    }

    let runfiles_dir = resolve_run_path_to_path_buf(&run_spec.runfiles_dir, project_root);
    if !runfiles_dir.starts_with(project_root) {
        return Err(RunCommandError::RunfilesDirectoryOutsideProject {
            path: runfiles_dir.display().to_string(),
            project_root: project_root.display().to_string(),
        }
        .into());
    }

    let manifest = runfiles_manifest_content(run_spec, project_root)?;
    let manifest_path = runfiles_dir.join(BAZEL_RUNFILES_MANIFEST);
    let existing_manifest = std::fs::read_to_string(&manifest_path).ok();
    if runfiles_dir.is_dir() && existing_manifest.as_deref() == Some(manifest.as_str()) {
        return Ok(());
    }

    std::fs::create_dir_all(&runfiles_dir).with_buck_error_context(|| {
        format!(
            "Failed to create runfiles tree `{}`",
            runfiles_dir.display()
        )
    })?;
    ensure_runfiles_workspace_dir(&runfiles_dir, run_spec.workspace_name.as_str())?;

    let desired_paths = runfiles_manifest_paths(&manifest);
    if let Some(existing_manifest) = existing_manifest {
        for stale_path in runfiles_manifest_paths(&existing_manifest).difference(&desired_paths) {
            let stale_path = validate_runfiles_relative_path(stale_path)?;
            let stale_path = runfiles_dir.join(stale_path);
            remove_path_if_exists(&stale_path).with_buck_error_context(|| {
                format!("Failed to remove stale runfile `{}`", stale_path.display())
            })?;
        }
    }

    for empty_filename in &run_spec.empty_filenames {
        let relative_path = validate_runfiles_relative_path(empty_filename)?;
        let path = runfiles_dir.join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_buck_error_context(|| {
                format!("Failed to create runfiles directory `{}`", parent.display())
            })?;
        }
        remove_path_if_exists(&path).with_buck_error_context(|| {
            format!("Failed to remove existing runfile `{}`", path.display())
        })?;
        File::create(&path).with_buck_error_context(|| {
            format!("Failed to create empty runfile `{}`", path.display())
        })?;
    }

    for runfile in &run_spec.runfiles {
        let relative_path = validate_runfiles_relative_path(&runfile.path)?;
        let link_path = runfiles_dir.join(relative_path);
        let target_path = resolve_run_path_to_path_buf(&runfile.target_path, project_root);
        if let Some(parent) = link_path.parent() {
            std::fs::create_dir_all(parent).with_buck_error_context(|| {
                format!("Failed to create runfiles directory `{}`", parent.display())
            })?;
        }
        remove_path_if_exists(&link_path).with_buck_error_context(|| {
            format!(
                "Failed to remove existing runfile `{}`",
                link_path.display()
            )
        })?;
        create_runfile_link(&target_path, &link_path).with_buck_error_context(|| {
            format!(
                "Failed to link runfile `{}` to `{}`",
                link_path.display(),
                target_path.display()
            )
        })?;
    }

    std::fs::write(&manifest_path, manifest).with_buck_error_context(|| {
        format!(
            "Failed to write runfiles manifest `{}`",
            manifest_path.display()
        )
    })?;

    Ok(())
}

fn ensure_runfiles_workspace_dir(
    runfiles_dir: &Path,
    workspace_name: &str,
) -> bz_error::Result<()> {
    if workspace_name.is_empty() {
        return Ok(());
    }

    let workspace_name = validate_runfiles_relative_path(workspace_name)?;
    let workspace_dir = runfiles_dir.join(workspace_name);
    std::fs::create_dir_all(&workspace_dir).with_buck_error_context(|| {
        format!(
            "Failed to create runfiles workspace directory `{}`",
            workspace_dir.display()
        )
    })?;
    Ok(())
}

fn runfiles_manifest_paths(manifest: &str) -> BTreeSet<String> {
    manifest
        .lines()
        .filter_map(|line| {
            if line.is_empty() {
                return None;
            }
            let (path, _) = line.split_once(' ').unwrap_or((line, ""));
            Some(path.to_owned())
        })
        .collect()
}

fn runfiles_manifest_content(run_spec: &RunSpec, project_root: &Path) -> bz_error::Result<String> {
    let mut entries = Vec::new();
    for empty_filename in &run_spec.empty_filenames {
        validate_runfiles_relative_path(empty_filename)?;
        entries.push(format!("{empty_filename}\n"));
    }
    for runfile in &run_spec.runfiles {
        validate_runfiles_relative_path(&runfile.path)?;
        let target_path = resolve_run_path_to_path_buf(&runfile.target_path, project_root);
        entries.push(format!("{} {}\n", runfile.path, target_path.display()));
    }
    entries.sort();
    Ok(entries.concat())
}

fn validate_runfiles_relative_path(path: &str) -> bz_error::Result<&Path> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
            )
        })
    {
        return Err(RunCommandError::InvalidRunfilesPath(path.display().to_string()).into());
    }
    Ok(path)
}

fn remove_path_if_exists(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(unix)]
fn create_runfile_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_runfile_link(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

fn bazel_run_environment(
    ctx: &ClientCommandContext<'_>,
    run_spec: &RunSpec,
    run_env: &[String],
) -> bz_error::Result<(Vec<(String, String)>, Vec<String>)> {
    let paths = ctx.paths()?;
    let mut environment = StdBuckHashMap::default();
    environment.insert(
        "BUILD_WORKSPACE_DIRECTORY".to_owned(),
        paths.project_root().root().to_string(),
    );
    environment.insert(
        "BUILD_WORKING_DIRECTORY".to_owned(),
        ctx.working_dir.path().to_string(),
    );
    let execroot = if run_spec.execroot.is_empty() {
        paths.project_root().root().to_string()
    } else {
        run_spec.execroot.clone()
    };
    environment.insert(
        "BUILD_EXECROOT".to_owned(),
        resolve_run_path_to_abs(&execroot, paths.project_root().root().as_path())?.to_string(),
    );
    environment.insert("BUILD_ID".to_owned(), ctx.trace_id.to_string());
    environment.insert("BUCK_RUN_BUILD_ID".to_owned(), ctx.trace_id.to_string());

    let mut environment_to_clear = bazel_run_environment_to_clear(run_spec);
    for variable in run_env {
        apply_run_env(variable, &mut environment, &mut environment_to_clear)?;
    }

    for name in &run_spec.inherited_environment {
        if let Ok(value) = std::env::var(name) {
            environment.insert(name.clone(), value);
        } else {
            environment.remove(name.as_str());
        }
    }
    for variable in &run_spec.environment {
        if let Some(value) = &variable.value {
            environment.insert(variable.name.clone(), value.clone());
        } else {
            environment.remove(variable.name.as_str());
            environment_to_clear.push(variable.name.clone());
        }
    }

    let mut environment: Vec<_> = environment.into_iter().collect();
    environment.sort_by(|(left, _), (right, _)| left.cmp(right));
    environment_to_clear.sort();
    environment_to_clear.dedup();
    Ok((environment, environment_to_clear))
}

fn bazel_run_environment_to_clear(run_spec: &RunSpec) -> Vec<String> {
    let mut environment_to_clear: Vec<_> = BAZEL_RUN_ENV_VARS_TO_CLEAR
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    environment_to_clear.extend(run_spec.environment_to_clear.iter().cloned());
    environment_to_clear.sort();
    environment_to_clear.dedup();
    environment_to_clear
}

fn apply_run_env(
    variable: &str,
    environment: &mut StdBuckHashMap<String, String>,
    environment_to_clear: &mut Vec<String>,
) -> bz_error::Result<()> {
    if variable.is_empty() || variable == "=" {
        return Err(RunCommandError::InvalidRunEnv(variable.to_owned()).into());
    }
    if let Some(name) = variable.strip_prefix('=') {
        validate_env_name(name)?;
        environment.remove(name);
        environment_to_clear.push(name.to_owned());
        return Ok(());
    }
    if let Some((name, value)) = variable.split_once('=') {
        validate_env_name(name)?;
        environment.insert(name.to_owned(), value.to_owned());
        environment_to_clear.retain(|to_clear| to_clear != name);
    } else {
        validate_env_name(variable)?;
        match std::env::var(variable) {
            Ok(value) => {
                environment.insert(variable.to_owned(), value);
            }
            Err(_) => {
                environment.remove(variable);
            }
        }
        environment_to_clear.retain(|to_clear| to_clear != variable);
    }
    Ok(())
}

fn validate_env_name(name: &str) -> bz_error::Result<()> {
    if name.is_empty() || name.contains('=') {
        return Err(RunCommandError::InvalidRunEnv(name.to_owned()).into());
    }
    Ok(())
}

fn command_envp(
    run_environment: &[(String, String)],
    run_environment_to_clear: &[String],
) -> StdBuckHashMap<String, String> {
    let mut envp: StdBuckHashMap<_, _> = std::env::vars().collect();
    for name in run_environment_to_clear {
        envp.remove(name.as_str());
    }
    for (name, value) in run_environment {
        envp.insert(name.clone(), value.clone());
    }
    envp
}

fn resolve_run_path_to_path_buf(path: &str, project_root: &Path) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn resolve_run_path_to_abs(path: &str, project_root: &Path) -> bz_error::Result<AbsPathBuf> {
    AbsPathBuf::new(resolve_run_path_to_path_buf(path, project_root))
}

fn resolve_run_executable_path(
    executable: &str,
    ctx: &ClientCommandContext<'_>,
) -> bz_error::Result<(String, bool)> {
    let path = Path::new(executable);
    if path.is_absolute() {
        return Ok((executable.to_owned(), true));
    }

    let project_relative_executable = ctx.paths()?.project_root().root().as_path().join(path);
    if project_relative_executable.exists() || executable_contains_path_separator(executable) {
        return Ok((
            project_relative_executable.to_string_lossy().into_owned(),
            true,
        ));
    }

    Ok((executable.to_owned(), false))
}

fn executable_contains_path_separator(executable: &str) -> bool {
    executable.contains('/') || executable.contains('\\')
}

fn validate_run_executable(path: &Path) -> bz_error::Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(
                RunCommandError::NonExistentOrNonExecutable(path.display().to_string()).into(),
            );
        }
        Err(error) => {
            return Err(RunCommandError::ExecutableValidation {
                path: path.display().to_string(),
                error: error.to_string(),
            }
            .into());
        }
    };
    if !metadata.is_file() || !metadata_is_executable(&metadata) {
        return Err(RunCommandError::NonExistentOrNonExecutable(path.display().to_string()).into());
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn metadata_is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[derive(bz_error::Error, Debug)]
#[buck2(tag = Input)]
pub enum RunCommandError {
    #[error("Target `{0}` is not a binary rule (only binary rules can be `run`)")]
    NonBinaryRule(String),
    #[error("`--emit-shell` is not supported on Windows")]
    EmitShellNotSupportedOnWindows,
    #[error("`bz run` only supports a single target, but multiple targets were requested.")]
    MultipleTargets,
    #[error("Target `{0}` is not found in the specified target universe")]
    TargetNotFoundInTargetUniverse(String),
    #[error("Target `{0}` was not found in the build result")]
    TargetNotFound(String),
    #[error("Target pattern `{0}` matched multiple run targets")]
    AmbiguousRunTarget(String),
    #[error("`--run_under` target `{0}` was not found in the build result")]
    RunUnderTargetNotFound(String),
    #[error("`--run_under` target `{0}` is not a binary rule")]
    RunUnderTargetNotBinary(String),
    #[error("Non-existent or non-executable `{0}`")]
    NonExistentOrNonExecutable(String),
    #[error("Error checking `{path}`: {error}")]
    ExecutableValidation { path: String, error: String },
    #[error("Invalid `--run_env` value `{0}`")]
    InvalidRunEnv(String),
    #[error("Invalid `--run_under` command `{0}`")]
    InvalidRunUnder(String),
    #[error("Invalid runfiles path `{0}`")]
    InvalidRunfilesPath(String),
    #[error("Refusing to materialize runfiles tree `{path}` outside project root `{project_root}`")]
    RunfilesDirectoryOutsideProject { path: String, project_root: String },
    #[error(
        "`bz run` will require a `--` separator before target arguments in the future. \
         Please use `bz run <target> -- <args>` instead of `bz run <target> <args>`"
    )]
    MissingSeparator,
}
