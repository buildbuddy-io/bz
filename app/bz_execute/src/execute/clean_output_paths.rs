/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_error::BuckErrorContext;
use buck2_error::buck2_error;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;

use crate::execute::blocking::IoRequest;

/// IoRequest we dispatch to the blocking executor to clear output paths.
pub struct CleanOutputPaths {
    pub paths: Vec<ProjectRelativePathBuf>,
}

/// IoRequest we dispatch to retire output paths and delete them in the background.
pub struct BackgroundCleanOutputPaths {
    pub paths: Vec<ProjectRelativePathBuf>,
}

impl CleanOutputPaths {
    pub fn clean<'a>(
        paths: impl IntoIterator<Item = &'a ProjectRelativePath>,
        fs: &'a ProjectRoot,
    ) -> buck2_error::Result<()> {
        for path in paths {
            cleanup_path(fs, path)
                .with_buck_error_context(|| format!("Error cleaning up output path `{path}`"))?;
        }
        Ok(())
    }
}

impl BackgroundCleanOutputPaths {
    pub fn clean<'a>(
        paths: impl IntoIterator<Item = &'a ProjectRelativePath>,
        fs: &'a ProjectRoot,
    ) -> buck2_error::Result<()> {
        for path in paths {
            background_cleanup_path(fs, path)
                .with_buck_error_context(|| format!("Error retiring output path `{path}`"))?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn tag_environment_error(error: buck2_error::Error) -> buck2_error::Error {
    error
}

#[cfg(windows)]
fn tag_environment_error(error: buck2_error::Error) -> buck2_error::Error {
    use buck2_error::ErrorTag;
    if error.has_tag(ErrorTag::IoWindowsSharingViolation)
        | error.has_tag(ErrorTag::IoPermissionDenied)
    {
        error
            .tag([ErrorTag::IoMaterializerFileBusy])
            .context("Binary being executed, please close the process first")
    } else {
        error
    }
}

#[tracing::instrument(level = "debug", skip(fs), fields(path = %path))]
pub fn cleanup_path(fs: &ProjectRoot, path: &ProjectRelativePath) -> buck2_error::Result<()> {
    let path = fs.resolve(path);

    // This will remove the path if it exists.
    fs_util::remove_all(&path)
        .categorize_internal()
        .map_err(tag_environment_error)?;

    let mut path: &AbsNormPath = &path;

    // Be aware of T85589819 - the parent directory might already exist, but as a _file_.  It might
    // be even worse, it might be 2 parents up, which will cause create_dir to fail when we try to
    // execute. So, we walk up the tree until we either find a dir we're happy with, or a file we
    // can delete. It's safe to delete this file because we know it doesn't overlap with a current
    // output, or that check would have failed, so it must be a stale file.
    loop {
        path = match path.parent() {
            Some(path) => path,
            None => {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::CleanOutputs,
                    "Internal Error: reached root before finding a directory that exists!"
                ));
            }
        };

        match fs_util::symlink_metadata_if_exists(path) {
            Ok(Some(m)) => {
                if m.is_dir() {
                    // It's a dir, no need to go further, and no need to delete.
                    tracing::trace!(path = %path, "skip (is dir)");
                } else {
                    // There was a file or a symlink, so it's safe to delete and then we can exit
                    // because we'll be able to create a dir here.
                    tracing::trace!(path = %path, "remove_file");

                    fs_util::remove_file(path)
                        .categorize_internal()
                        .map_err(tag_environment_error)?;
                }
                return Ok(());
            }
            Ok(None) if cfg!(unix) => {
                // If we get ENOENT that guarantees there is no file on the path. If there was
                // one, we would get ENOTDIR. TODO (T123279320) This probably works on Windows,
                // but it wasn't tested there.
                //
                // On non-Unix we don't have this optimization. Recursing all the way up
                // until we find the first dir (or file to delete) is fine. There will
                // eventually be *a* directory (at buck-out, then another one at the empty
                // directory, which is our cwd, and should exist by now).
                tracing::trace!(path = %path, "skip (ENOENT)");
                return Ok(());
            }
            Ok(None) => {
                tracing::trace!(path = %path, "continue (ENOENT)");
            }
            Err(e) => {
                // Continue going up. Eventually we should reach the output directory, which should
                // exist.
                tracing::trace!(path = %path, "continue (error: {:?})", e);
            }
        }
    }
}

static BACKGROUND_CLEAN_COUNTER: AtomicU64 = AtomicU64::new(0);

fn background_cleanup_path(
    fs: &ProjectRoot,
    path: &ProjectRelativePath,
) -> buck2_error::Result<()> {
    let path = fs.resolve(path);
    if fs_util::symlink_metadata_if_exists(&path)?.is_none() {
        return Ok(());
    }

    let parent = path.parent().ok_or_else(|| {
        buck2_error!(
            buck2_error::ErrorTag::CleanOutputs,
            "Internal Error: output path `{path}` has no parent"
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            buck2_error!(
                buck2_error::ErrorTag::CleanOutputs,
                "Internal Error: output path `{path}` has no file name"
            )
        })?;

    let retired = retired_path(parent, file_name)?;
    fs_util::rename(&path, &retired).categorize_internal()?;
    spawn_background_cleaner(&retired)?;
    Ok(())
}

fn retired_path(parent: &AbsNormPath, file_name: &str) -> buck2_error::Result<AbsNormPathBuf> {
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = BACKGROUND_CLEAN_COUNTER.fetch_add(1, Ordering::Relaxed);
    AbsNormPathBuf::new(parent.as_path().join(format!(
        ".{file_name}.buck2-clean-{}-{now_nanos}-{counter}",
        std::process::id()
    )))
    .map_err(Into::into)
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

impl IoRequest for CleanOutputPaths {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
        Self::clean(self.paths.iter().map(AsRef::as_ref), project_fs)
    }
}

impl IoRequest for BackgroundCleanOutputPaths {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
        Self::clean(self.paths.iter().map(AsRef::as_ref), project_fs)
    }
}
