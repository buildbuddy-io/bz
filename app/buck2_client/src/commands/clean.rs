/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use buck2_cli_proto::CleanRequest;
use buck2_cli_proto::CleanStaleResponse;
use buck2_client_ctx::client_ctx::BuckSubcommand;
use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::common::BuckArgMatches;
use buck2_client_ctx::common::CommonBuildConfigurationOptions;
use buck2_client_ctx::common::CommonCommandOptions;
use buck2_client_ctx::common::CommonEventLogOptions;
use buck2_client_ctx::common::CommonStarlarkOptions;
use buck2_client_ctx::common::target_cfg::TargetCfgUnusedOptions;
use buck2_client_ctx::common::ui::CommonConsoleOptions;
use buck2_client_ctx::common::ui::ConsoleType;
use buck2_client_ctx::daemon::client::BuckdClientConnector;
use buck2_client_ctx::daemon::client::BuckdLifecycleLock;
use buck2_client_ctx::daemon::client::NoPartialResultHandler;
use buck2_client_ctx::daemon::client::kill::kill_command_impl;
use buck2_client_ctx::events_ctx::EventsCtx;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_client_ctx::final_console::FinalConsole;
use buck2_client_ctx::startup_deadline::StartupDeadline;
use buck2_client_ctx::streaming::StreamingCommand;
use buck2_client_ctx::subscribers::superconsole::StatefulSuperConsole;
use buck2_common::daemon_dir::DaemonDir;
use buck2_error::BuckErrorContext;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::abs_path::AbsPath;
use dupe::Dupe;
use gazebo::prelude::SliceExt;
use superconsole::Line;
use superconsole::SuperConsole;
use superconsole::components::Spinner;
use threadpool::ThreadPool;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::commands::clean_stale::CleanStaleCommand;
use crate::commands::clean_stale::parse_clean_stale_args;

/// Delete generated files and caches.
///
/// By default this keeps the buck2 daemon running, like Bazel clean. Use
/// --expunge to remove daemon state and stop the daemon.
#[derive(Debug, clap::Parser)]
pub struct CleanCommand {
    #[clap(
        long = "dry-run",
        help = "Performs a dry-run and prints the paths that would be removed."
    )]
    dry_run: bool,

    #[clap(
        long = "stale",
        help = "Delete artifacts from buck-out older than 1 week or older than
the specified duration, without killing the daemon",
        value_name = "DURATION"
    )]
    stale: Option<Option<humantime::Duration>>,

    // Like stale but since a specific timestamp, for testing
    #[clap(long = "keep-since-time", conflicts_with = "stale", hide = true)]
    keep_since_time: Option<i64>,

    /// Only considers tracked artifacts for cleanup.
    ///
    /// `buck-out` can contain untracked artifacts for different reasons:
    ///  - Outputs from aborted actions
    ///  - State getting deleted (e.g., new buckversion that changes the on-disk state format)
    ///  - Writing to `buck-out` without being expected by Buck
    #[clap(long = "tracked-only", requires = "stale")]
    tracked_only: bool,

    #[clap(
        long = "background",
        help = "Run the clean operation in the background"
    )]
    background: bool,

    #[clap(
        long = "expunge",
        help = "Remove daemon state and stop the buck2 daemon, like Bazel clean --expunge"
    )]
    expunge: bool,

    #[clap(
        long = "expunge-bzlmod-caches",
        help = "Also delete bzlmod repository caches and materialized external repository roots. By default, normal clean preserves them like Bazel preserves fetched external repositories."
    )]
    expunge_bzlmod_caches: bool,

    /// Command doesn't need these flags, but they are used in mode files, so we need to keep them.
    #[clap(flatten)]
    _target_cfg: TargetCfgUnusedOptions,

    #[clap(flatten)]
    common_opts: CommonCommandOptions,
}

impl CleanCommand {
    pub fn exec(
        self,
        matches: BuckArgMatches<'_>,
        ctx: ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        if let Some(keep_since_arg) = parse_clean_stale_args(self.stale, self.keep_since_time)? {
            let cmd = CleanStaleCommand {
                common_opts: self.common_opts,
                keep_since_arg,
                dry_run: self.dry_run,
                tracked_only: self.tracked_only,
            };
            ctx.exec(cmd, matches, events_ctx)
        } else {
            let clean_mode = clean_mode(
                self.dry_run,
                self.background,
                self.expunge,
                self.expunge_bzlmod_caches,
            );
            if clean_mode.use_inner_clean {
                ctx.exec(
                    InnerCleanCommand {
                        dry_run: self.dry_run,
                        background: self.background,
                        stop_daemon: clean_mode.stop_daemon,
                        delete_daemon_dir: clean_mode.delete_daemon_dir,
                        preserve_bzlmod_caches: clean_mode.preserve_bzlmod_caches,
                        common_opts: self.common_opts,
                    },
                    matches,
                    events_ctx,
                )
            } else {
                ctx.exec(
                    DaemonCleanCommand {
                        common_opts: self.common_opts,
                    },
                    matches,
                    events_ctx,
                )
            }
        }
    }

    pub fn command_name(&self) -> &'static str {
        if let Ok(Some(_)) = parse_clean_stale_args(self.stale, self.keep_since_time) {
            "clean-stale"
        } else {
            "clean"
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CleanMode {
    use_inner_clean: bool,
    stop_daemon: bool,
    delete_daemon_dir: bool,
    preserve_bzlmod_caches: bool,
}

fn clean_mode(
    dry_run: bool,
    background: bool,
    expunge: bool,
    expunge_bzlmod_caches: bool,
) -> CleanMode {
    CleanMode {
        use_inner_clean: dry_run || background || expunge || expunge_bzlmod_caches,
        stop_daemon: expunge,
        delete_daemon_dir: expunge,
        preserve_bzlmod_caches: !(expunge || expunge_bzlmod_caches),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_mode_background_does_not_stop_daemon() {
        assert_eq!(
            clean_mode(false, true, false, false),
            CleanMode {
                use_inner_clean: true,
                stop_daemon: false,
                delete_daemon_dir: false,
                preserve_bzlmod_caches: true,
            }
        );
    }

    #[test]
    fn clean_mode_expunge_bzlmod_caches_does_not_stop_daemon() {
        assert_eq!(
            clean_mode(false, false, false, true),
            CleanMode {
                use_inner_clean: true,
                stop_daemon: false,
                delete_daemon_dir: false,
                preserve_bzlmod_caches: false,
            }
        );
    }

    #[test]
    fn clean_mode_expunge_stops_daemon_and_deletes_daemon_dir() {
        assert_eq!(
            clean_mode(false, false, true, false),
            CleanMode {
                use_inner_clean: true,
                stop_daemon: true,
                delete_daemon_dir: true,
                preserve_bzlmod_caches: false,
            }
        );
    }
}

struct InnerCleanCommand {
    dry_run: bool,
    background: bool,
    stop_daemon: bool,
    delete_daemon_dir: bool,
    preserve_bzlmod_caches: bool,
    common_opts: CommonCommandOptions,
}

struct DaemonCleanCommand {
    common_opts: CommonCommandOptions,
}

fn format_clean_stats(stats: buck2_data::CleanStaleStats) -> String {
    let mut output = String::new();
    output += &format!(
        "Found {} output roots ({})\n",
        stats.stale_artifact_count,
        bytesize::ByteSize::b(stats.stale_bytes).display().iec(),
    );
    output += &format!("Cleaned {} paths\n", stats.cleaned_artifact_count,);
    output += &format!(
        "{} bytes cleaned ({})\n",
        stats.cleaned_bytes,
        bytesize::ByteSize::b(stats.cleaned_bytes).display().iec(),
    );
    output
}

#[async_trait(?Send)]
impl StreamingCommand for DaemonCleanCommand {
    const COMMAND_NAME: &'static str = "clean";

    async fn exec_impl(
        self,
        buckd: &mut BuckdClientConnector,
        matches: BuckArgMatches<'_>,
        ctx: &mut ClientCommandContext<'_>,
        events_ctx: &mut EventsCtx,
    ) -> ExitResult {
        let context = ctx.client_context(matches, &self)?;
        let response: CleanStaleResponse = buckd
            .with_flushing()
            .clean(
                CleanRequest {
                    context: Some(context),
                    dry_run: false,
                },
                events_ctx,
                ctx.console_interaction_stream(&self.common_opts.console_opts),
                &mut NoPartialResultHandler,
            )
            .await??;

        if let Some(message) = response.message {
            buck2_client_ctx::eprintln!("{}", message)?;
        }
        if let Some(stats) = response.stats {
            buck2_client_ctx::eprintln!("{}", format_clean_stats(stats))?;
        }
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

impl BuckSubcommand for InnerCleanCommand {
    const COMMAND_NAME: &'static str = "clean";

    async fn exec_impl(
        self,
        _matches: BuckArgMatches<'_>,
        ctx: ClientCommandContext<'_>,
        _events_ctx: &mut buck2_client_ctx::events_ctx::EventsCtx,
    ) -> ExitResult {
        let paths = ctx.paths()?;
        let buck_out_dir = paths.buck_out_path();
        let daemon_dir = paths.daemon_dir()?;
        let trash_dir = paths.trash_dir();
        let console = &self.common_opts.console_opts.final_console();

        if self.dry_run {
            return clean(
                buck_out_dir,
                daemon_dir,
                trash_dir,
                console,
                self.common_opts.console_opts.console_type,
                None,
                true,
                self.background,
                self.delete_daemon_dir,
                self.preserve_bzlmod_caches,
            )
            .await
            .into();
        }

        let lifecycle_lock = if self.stop_daemon {
            // Kill the daemon and make sure a new daemon does not spin up while we're performing
            // clean up operations. This ensures exclusive access to daemon state for expunge.
            let lifecycle_lock = BuckdLifecycleLock::lock_with_timeout(
                daemon_dir.clone(),
                StartupDeadline::duration_from_now(Duration::from_secs(10))?,
            )
            .await?;

            kill_command_impl(&lifecycle_lock, "`buck2 clean --expunge` was invoked").await?;
            Some(lifecycle_lock)
        } else {
            None
        };

        clean(
            buck_out_dir,
            daemon_dir,
            trash_dir,
            console,
            self.common_opts.console_opts.console_type,
            lifecycle_lock.as_ref(),
            false,
            self.background,
            self.delete_daemon_dir,
            self.preserve_bzlmod_caches,
        )
        .await
        .into()
    }

    fn event_log_opts(&self) -> &CommonEventLogOptions {
        &self.common_opts.event_log_opts
    }
}

async fn clean(
    buck_out_dir: AbsNormPathBuf,
    daemon_dir: DaemonDir,
    trash_dir: AbsNormPathBuf,
    console: &FinalConsole,
    console_type: ConsoleType,
    lifecycle_lock: Option<&BuckdLifecycleLock>,
    dry_run: bool,
    background: bool,
    delete_daemon_dir: bool,
    preserve_bzlmod_caches: bool,
) -> buck2_error::Result<()> {
    let paths_to_clean = if dry_run {
        let mut paths_to_clean = Vec::new();
        if buck_out_dir.exists() {
            paths_to_clean = collect_paths_to_clean(&buck_out_dir, preserve_bzlmod_caches)?
                .map(|path| path.display().to_string());
        }
        if delete_daemon_dir && daemon_dir.path.exists() {
            paths_to_clean.push(daemon_dir.to_string());
        }
        paths_to_clean
    } else if background {
        let trash_uuid = Uuid::new_v4();
        let trash_target = trash_dir.as_abs_path().join(trash_uuid.to_string());
        let mut trash_target_normalized = None;

        // Create trash directory if it doesn't exist
        if !trash_dir.exists() {
            fs_util::create_dir_all(&trash_dir)?;
        }

        // Move buck-out to trash folder
        if buck_out_dir.exists() {
            console.print_stderr(&format!(
                "Moving {} to {}",
                buck_out_dir.display(),
                trash_target.display()
            ))?;
            fs_util::rename(&buck_out_dir, &trash_target).categorize_internal()?;
            let normalized = AbsNormPathBuf::new(trash_target.to_path_buf())?;
            if preserve_bzlmod_caches {
                restore_preserved_bzlmod_paths(&normalized, &buck_out_dir)?;
            }
            trash_target_normalized = Some(normalized);
        }

        // Clean the daemon_dir first
        let mut paths_to_clean = Vec::new();
        if delete_daemon_dir && daemon_dir.path.exists() {
            paths_to_clean.push(daemon_dir.to_string());
            if let Some(lifecycle_lock) = lifecycle_lock {
                lifecycle_lock.clean_daemon_dir(false)?;
            }
        }

        if let Some(trash_target_normalized) = trash_target_normalized {
            console
                .print_stderr("Buck-out moved to trash. Deletion continues in the background.")?;
            console.print_stderr("You can run other buck2 commands while this completes.")?;
            paths_to_clean.push(trash_target_normalized.display().to_string());
            spawn_background_cleaner(&trash_target_normalized)?;
        }
        paths_to_clean
    } else {
        let mut paths_to_clean = Vec::new();

        if buck_out_dir.exists() {
            paths_to_clean = collect_paths_to_clean(&buck_out_dir, preserve_bzlmod_caches)?
                .map(|path| path.display().to_string());
            tokio::task::spawn_blocking(move || {
                clean_buck_out_with_retry(&buck_out_dir, console_type, preserve_bzlmod_caches)
            })
            .await?
            .buck_error_context("Failed to spawn clean")?;
        }

        if delete_daemon_dir && daemon_dir.path.exists() {
            paths_to_clean.push(daemon_dir.to_string());
            if let Some(lifecycle_lock) = lifecycle_lock {
                lifecycle_lock.clean_daemon_dir(false)?;
            }
        }

        paths_to_clean
    };

    if paths_to_clean.is_empty() {
        console.print_stderr("Nothing to clean.")?;
    }
    for path in paths_to_clean {
        console.print_stderr(&path)?;
    }

    Ok(())
}

fn spawn_background_cleaner(path: &AbsNormPathBuf) -> buck2_error::Result<()> {
    #[cfg(unix)]
    {
        let child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(
                "/usr/bin/find \"$1\" -type d -not -perm -u=rwx -exec /bin/chmod -f u=rwx {} + 2>/dev/null; /bin/rm -rf \"$1\"",
            )
            .arg("buck2-background-clean")
            .arg(path.as_path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .buck_error_context("Failed to start background clean process")?;
        tracing::info!(
            path = %path.display(),
            pid = child.id(),
            "started background clean process"
        );
        drop(child);
        Ok(())
    }
    #[cfg(windows)]
    {
        let child = std::process::Command::new("cmd")
            .arg("/C")
            .arg("rmdir")
            .arg("/S")
            .arg("/Q")
            .arg(path.as_path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .buck_error_context("Failed to start background clean process")?;
        tracing::info!(
            path = %path.display(),
            pid = child.id(),
            "started background clean process"
        );
        drop(child);
        Ok(())
    }
}

const PRESERVED_BZLMOD_PATHS: &[&str] = &[
    "bzlmod_bcr_discovery",
    "bzlmod_cell_graph_module_extensions",
    "bzlmod_repo_contents",
    "../external_cells/bzlmod",
    "../external_cells/bzlmod_generated",
];

fn cache_dir(buck_out_path: &AbsNormPathBuf) -> AbsNormPathBuf {
    buck_out_path
        .join(buck2_fs::paths::forward_rel_path::ForwardRelativePath::unchecked_new("cache"))
}

fn preserved_bzlmod_path(
    buck_out_path: &AbsNormPathBuf,
    preserved_path: &str,
) -> buck2_error::Result<AbsNormPathBuf> {
    if let Some(path) = preserved_path.strip_prefix("../") {
        return Ok(buck_out_path.join(
            buck2_fs::paths::forward_rel_path::ForwardRelativePath::new(path)?,
        ));
    }
    Ok(
        cache_dir(buck_out_path).join(buck2_fs::paths::forward_rel_path::ForwardRelativePath::new(
            preserved_path,
        )?),
    )
}

fn preserved_bzlmod_paths(
    buck_out_path: &AbsNormPathBuf,
    preserve_bzlmod_caches: bool,
) -> buck2_error::Result<Vec<AbsNormPathBuf>> {
    if !preserve_bzlmod_caches {
        return Ok(Vec::new());
    }
    PRESERVED_BZLMOD_PATHS
        .iter()
        .map(|path| preserved_bzlmod_path(buck_out_path, path))
        .collect()
}

fn is_preserved_path(path: &Path, preserved_paths: &[AbsNormPathBuf]) -> bool {
    preserved_paths
        .iter()
        .any(|preserved| path.starts_with(preserved.as_path()))
}

fn contains_preserved_path(path: &Path, preserved_paths: &[AbsNormPathBuf]) -> bool {
    preserved_paths
        .iter()
        .any(|preserved| preserved.as_path().starts_with(path))
}

fn restore_preserved_bzlmod_paths(
    from_buck_out: &AbsNormPathBuf,
    to_buck_out: &AbsNormPathBuf,
) -> buck2_error::Result<()> {
    for preserved_path in PRESERVED_BZLMOD_PATHS {
        let from = preserved_bzlmod_path(from_buck_out, preserved_path)?;
        if fs_util::symlink_metadata_if_exists(&from)?.is_none() {
            continue;
        }
        let to = preserved_bzlmod_path(to_buck_out, preserved_path)?;
        if let Some(parent) = to.parent() {
            fs_util::create_dir_all(parent)?;
        }
        fs_util::remove_all(&to).categorize_internal()?;
        fs_util::rename(&from, &to).categorize_internal()?;
    }
    Ok(())
}

fn collect_paths_to_clean(
    buck_out_path: &AbsNormPathBuf,
    preserve_bzlmod_caches: bool,
) -> buck2_error::Result<Vec<AbsNormPathBuf>> {
    let preserved_paths = preserved_bzlmod_paths(buck_out_path, preserve_bzlmod_caches)?;
    collect_paths_to_clean_with_preserved(buck_out_path, &preserved_paths)
}

fn collect_paths_to_clean_with_preserved(
    buck_out_path: &AbsNormPathBuf,
    preserved_paths: &[AbsNormPathBuf],
) -> buck2_error::Result<Vec<AbsNormPathBuf>> {
    if !buck_out_path.exists() {
        return Ok(vec![]);
    }
    let mut paths_to_clean = vec![];
    let dir = fs_util::read_dir(buck_out_path).categorize_internal()?;
    for entry in dir {
        let entry = entry?;
        let path = entry.path();
        if is_preserved_path(path.as_path(), &preserved_paths) {
            continue;
        }
        if contains_preserved_path(path.as_path(), &preserved_paths) {
            let child = path;
            paths_to_clean.extend(collect_paths_to_clean_with_preserved(
                &child,
                preserved_paths,
            )?);
        } else {
            paths_to_clean.push(path);
        }
    }

    Ok(paths_to_clean)
}

/// In Windows, we've observed the buck-out clean immediately after killing
/// the daemon can fail with this error: `The process cannot access the
/// file because it is being used by another process.`. To get around this,
/// add a single retry.
fn clean_buck_out_with_retry(
    path: &AbsNormPathBuf,
    console_type: ConsoleType,
    preserve_bzlmod_caches: bool,
) -> buck2_error::Result<()> {
    let mut result = clean_buck_out(path, console_type, preserve_bzlmod_caches);
    match result {
        Ok(_) => {
            return result;
        }
        Err(e) => {
            tracing::info!(
                "Retrying buck-out clean, first attempted failed with: {:#}",
                e
            );
            result = clean_buck_out(path, console_type, preserve_bzlmod_caches);
        }
    }
    result
}

/// State shared between the progress display and the file deletion threads.
struct CleanProgressState {
    files_deleted: Arc<AtomicUsize>,
    start_time: Instant,
}

impl CleanProgressState {
    fn new() -> Self {
        Self {
            files_deleted: Arc::new(AtomicUsize::new(0)),
            start_time: Instant::now(),
        }
    }

    fn counter(&self) -> Arc<AtomicUsize> {
        self.files_deleted.dupe()
    }

    fn files_deleted(&self) -> usize {
        self.files_deleted.load(Ordering::Relaxed)
    }

    fn format_message(&self) -> Line {
        let elapsed = Instant::now() - self.start_time;
        Line::sanitized(&format!(
            "Cleaning buck-out: {} files deleted ({}s)",
            self.files_deleted(),
            elapsed.as_secs()
        ))
    }

    fn format_final_message(&self) -> Line {
        let elapsed = Instant::now() - self.start_time;
        Line::sanitized(&format!(
            "Cleaned {} files in {:.1}s",
            self.files_deleted(),
            elapsed.as_secs_f64()
        ))
    }
}

/// Runs the progress display loop using superconsole.
fn run_superconsole_progress(
    mut console: SuperConsole,
    state: &CleanProgressState,
    stop: impl Fn() -> bool,
) {
    let mut tick = 0;
    while !stop() {
        let spinner = Spinner::new(tick, state.format_message());
        if console.render(&spinner).is_err() {
            break;
        }
        tick += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    // Finalize with the final message (no spinner prefix in Final mode)
    let final_spinner = Spinner::new(tick, state.format_final_message());
    drop(console.finalize(&final_spinner));
}

/// Handle for the superconsole-based progress display.
/// When dropped, it stops the display thread and shows the completion message.
struct CleanProgressHandle {
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CleanProgressHandle {
    fn new(state: Arc<CleanProgressState>, console: SuperConsole) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = stop_flag.dupe();

        let handle = std::thread::spawn(move || {
            run_superconsole_progress(console, &state, || stop_flag_clone.load(Ordering::Relaxed));
        });

        Self {
            stop_flag,
            handle: Some(handle),
        }
    }
}

impl Drop for CleanProgressHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

fn clean_buck_out(
    path: &AbsNormPathBuf,
    console_type: ConsoleType,
    preserve_bzlmod_caches: bool,
) -> buck2_error::Result<()> {
    let preserved_paths = preserved_bzlmod_paths(path, preserve_bzlmod_caches)?;
    let walk = WalkDir::new(path);
    let thread_pool = ThreadPool::new(buck2_util::threads::available_parallelism());
    let error = Arc::new(Mutex::new(None));

    let state = Arc::new(CleanProgressState::new());
    let counter = state.counter();

    // Show progress using superconsole, respecting the --console option.
    // Use the same console_builder() as other buck2 commands to ensure consistent behavior.
    let _progress_handle = match console_type {
        ConsoleType::None
        | ConsoleType::Simple
        | ConsoleType::SimpleNoTty
        | ConsoleType::SimpleTty => None,
        ConsoleType::Auto | ConsoleType::Super => StatefulSuperConsole::console_builder()
            .build()
            .ok()
            .flatten()
            .map(|console| CleanProgressHandle::new(state, console)),
    };

    for dir_entry in walk
        .into_iter()
        .filter_entry(|entry| !is_preserved_path(entry.path(), &preserved_paths))
    {
        let dir_entry = dir_entry.map_err(|error| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "failed to walk `{}` while cleaning: {}",
                path,
                error
            )
        })?;
        let file_type = dir_entry.file_type();
        // As in the daemon, heavily parallel writes to directories in btrfs perform really poorly,
        // so we only parallelize file deletions and do the rest synchronously.
        //
        // FIXME(JakobDegen): The parallelism cap for file deletions in the daemon is much smaller
        // than it is here. Change that or write a comment justifying it.
        if !file_type.is_dir() && !file_type.is_symlink() {
            let error = error.dupe();
            let counter = counter.dupe();
            thread_pool.execute(move || {
                // The wlak gives us back absolute paths since we give it absolute paths.
                let res = AbsPath::new(dir_entry.path()).and_then(|p| {
                    fs_util::remove_file(p)
                        .categorize_internal()
                        .map_err(Into::into)
                });

                match res {
                    Ok(_) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        let mut error = error.lock().unwrap();
                        if error.is_none() {
                            *error = Some(e);
                        }
                    }
                }
            })
        }
    }

    thread_pool.join();

    // Drop the progress handle to stop the display and show final message
    drop(_progress_handle);

    if let Some(e) = error.lock().unwrap().take() {
        return Err(e);
    }

    // Buck's cwd is typically the directory that is passed in here, which means that on Windows we
    // often fail to delete this if we don't clean up all our child processes. Leaving zombies
    // around isn't great though...
    remove_unpreserved_children(path, &preserved_paths)?;
    Ok(())
}

fn remove_unpreserved_children(
    path: &AbsNormPathBuf,
    preserved_paths: &[AbsNormPathBuf],
) -> buck2_error::Result<()> {
    let dir = fs_util::read_dir(path).categorize_internal()?;
    for entry in dir {
        let entry = entry?;
        let child = entry.path();
        if is_preserved_path(child.as_path(), preserved_paths) {
            continue;
        }
        if contains_preserved_path(child.as_path(), preserved_paths) {
            remove_unpreserved_children(&child, preserved_paths)?;
        } else {
            fs_util::remove_all(&child).categorize_internal()?;
        }
    }
    Ok(())
}
