/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::cell_path::CellPathRef;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::external::external_cell_origin_for_cell;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePath;
use buck2_error::internal_error;
use buck2_fs::paths::file_name::FileNameBuf;
use buck2_hash::StdBuckHashMap;
use buck2_hash::StdBuckHashSet;
use derive_more::Display;
use dice::DiceComputations;
use dice::DiceTransactionUpdater;
use dice::InvalidationSourcePriority;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::TodoValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::buildfiles::HasBuildfiles;
use crate::dice::cells::HasExternalCellOrigins;
use crate::dice::data::HasIoProvider;
use crate::dice::skyframe::BazelSkyframeFunction;
use crate::dice::skyframe::mark_bazel_skyframe_key_with_detail;
use crate::external_symlink::ExternalSymlink;
use crate::file_ops::delegate::FileOpsDelegateWithIgnores;
use crate::file_ops::delegate::get_delegated_file_ops;
use crate::file_ops::error::FileReadError;
use crate::file_ops::error::extended_ignore_error;
use crate::file_ops::metadata::RawDirEntry;
use crate::file_ops::metadata::RawPathMetadata;
use crate::file_ops::metadata::RawPathMetadataForNoWatchFs;
use crate::file_ops::metadata::RawSymlink;
use crate::file_ops::metadata::ReadDirOutput;
use crate::ignores::file_ignores::FileIgnoreResult;
use crate::invocation_paths::InvocationPaths;
use crate::io::IoProvider;
use crate::io::NoWatchFsMetadataCache;
use crate::io::ReadDirError;
use crate::io::fs::read_external_path_metadata_for_no_watchfs;

pub struct DiceFileComputations;

const NO_WATCHFS_METADATA_CHECK_CONCURRENCY: usize = 200;
const MAX_NO_WATCHFS_FILE_CHANGE_RECORDS: usize = 100;
const MAX_EXTERNAL_SYMLINK_EXPANSIONS: usize = 256;
static EXTERNAL_FILE_STATE_SEEN: AtomicBool = AtomicBool::new(false);

#[derive(Copy, Clone, Dupe, Debug, Eq, PartialEq)]
pub enum FollowedPathType {
    File,
    Directory,
}

/// Functions for accessing files with keys on the dice graph.
impl DiceFileComputations {
    /// Filters out ignored paths
    pub async fn read_dir(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> buck2_error::Result<ReadDirOutput> {
        ctx.compute(&ReadDirKey {
            path: path.to_owned(),
            check_ignores: CheckIgnores::Yes,
        })
        .await?
    }

    /// Returns if a directory or file exists at the given path, but checks for an exact,
    /// case-sensitive match.
    ///
    /// Note that case-sensitive match is only done on the last element of the path, not any of the
    /// elements before.
    pub async fn exists_matching_exact_case(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> buck2_error::Result<bool> {
        ctx.compute(&ExistsMatchingExactCaseKey(path.to_owned()))
            .await?
    }

    pub async fn read_dir_include_ignores(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> buck2_error::Result<ReadDirOutput> {
        ctx.compute(&ReadDirKey {
            path: path.to_owned(),
            check_ignores: CheckIgnores::No,
        })
        .await?
    }

    /// Like read_dir, but with extended error information. This may add additional dice dependencies.
    pub async fn read_dir_ext(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> Result<ReadDirOutput, ReadDirError> {
        read_dir_ext(ctx, path).await
    }

    /// Does not check if the path is ignored
    ///
    /// TODO(cjhopman): error on ignored paths, maybe.
    pub async fn read_file_if_exists(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> buck2_error::Result<Option<String>> {
        (ctx.compute(&ReadFileKey(Arc::new(path.to_owned())))
            .await??
            .proxy
            .0)()
        .await
    }

    /// Does not check if the path is ignored
    pub async fn read_file(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> Result<String, FileReadError> {
        match Self::read_file_if_exists(ctx, path).await {
            Ok(result) => result.ok_or_else(|| FileReadError::NotFound(path.to_string())),
            Err(e) => Err(FileReadError::Buck(e)),
        }
    }

    /// Does not check if the path is ignored
    pub async fn read_path_metadata_if_exists(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
        ctx.compute(&PathMetadataKey(path.to_owned())).await?
    }

    /// Does not check if the path is ignored.
    ///
    /// This returns Bazel-style file-change metadata for files when fast
    /// digests are unavailable, so callers can check for changes without
    /// hashing file contents.
    pub async fn read_path_metadata_for_no_watchfs_if_exists(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        ctx.compute(&PathMetadataForNoWatchFsKey(path.to_owned()))
            .await?
    }

    pub async fn read_path_metadata_for_no_watchfs(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> Result<RawPathMetadataForNoWatchFs, FileReadError> {
        match Self::read_path_metadata_for_no_watchfs_if_exists(ctx, path).await {
            Ok(result) => result.ok_or_else(|| FileReadError::NotFound(path.to_string())),
            Err(e) => Err(FileReadError::Buck(e)),
        }
    }

    /// Does not check if the path is ignored
    pub async fn read_path_metadata(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> Result<RawPathMetadata, FileReadError> {
        match Self::read_path_metadata_if_exists(ctx, path).await {
            Ok(result) => result.ok_or_else(|| FileReadError::NotFound(path.to_string())),
            Err(e) => Err(FileReadError::Buck(e)),
        }
    }

    /// Returns the real file type after resolving symlinks, matching Bazel's `FileValue`
    /// behavior for exact glob patterns.
    pub async fn followed_path_type_if_exists(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> buck2_error::Result<Option<FollowedPathType>> {
        let mut metadata = ctx
            .compute(&PathMetadataForNoWatchFsKey(path.to_owned()))
            .await??;
        let mut seen = StdBuckHashSet::default();
        loop {
            match metadata {
                None => return Ok(None),
                Some(RawPathMetadataForNoWatchFs::File(_)) => {
                    return Ok(Some(FollowedPathType::File));
                }
                Some(RawPathMetadataForNoWatchFs::Directory) => {
                    return Ok(Some(FollowedPathType::Directory));
                }
                Some(RawPathMetadataForNoWatchFs::Symlink {
                    at: _,
                    to: RawSymlink::Relative(target, _),
                }) => {
                    let target = target.as_ref().clone();
                    if !seen.insert(target.clone()) {
                        mark_bazel_skyframe_key_with_detail(
                            ctx,
                            BazelSkyframeFunction::FileSymlinkCycleUniqueness,
                            target.to_string(),
                        )
                        .await?;
                        return Err(internal_error!(
                            "symlink cycle while resolving path metadata at `{}`",
                            target
                        ));
                    }
                    metadata = ctx.compute(&PathMetadataForNoWatchFsKey(target)).await??;
                }
                Some(RawPathMetadataForNoWatchFs::Symlink {
                    at: _,
                    to: RawSymlink::External(target),
                }) => {
                    let external_metadata = ctx
                        .compute(&ExternalPathMetadataKey(target.with_full_target()?))
                        .await??;
                    return Ok(external_metadata.followed_path_type());
                }
            }
        }
    }

    pub async fn is_ignored(
        ctx: &mut DiceComputations<'_>,
        path: CellPathRef<'_>,
    ) -> buck2_error::Result<FileIgnoreResult> {
        get_delegated_file_ops(ctx, path.cell(), CheckIgnores::Yes)
            .await?
            .is_ignored(path.path())
            .await
    }

    pub async fn buildfiles(
        ctx: &mut DiceComputations<'_>,
        cell: CellName,
    ) -> buck2_error::Result<Arc<[FileNameBuf]>> {
        ctx.get_buildfiles(cell).await
    }
}

#[derive(
    Debug, Display, Clone, Dupe, Copy, PartialEq, Eq, Hash, Allocative, Pagable
)]
pub(crate) enum CheckIgnores {
    Yes,
    No,
}

#[derive(Allocative)]
pub struct FileChangeTracker {
    raw_dirs_to_dirty: StdBuckHashSet<ReadDirForNoWatchFsKey>,
    raw_paths_to_dirty: StdBuckHashSet<PathMetadataForNoWatchFsKey>,

    maybe_modified_dirs: StdBuckHashSet<CellPath>,
}

#[derive(Debug, Default)]
pub struct KnownFileStateInvalidationStats {
    pub read_files: usize,
    pub read_dirs: usize,
    pub paths: usize,
    pub exists_matching_exact_case: usize,
    pub events: Vec<buck2_data::FileWatcherEvent>,
    pub timings: KnownFileStateInvalidationTimings,
}

impl KnownFileStateInvalidationStats {
    pub fn total(&self) -> usize {
        self.read_files + self.read_dirs + self.paths + self.exists_matching_exact_case
    }
}

#[derive(Debug, Default)]
pub struct KnownFileStateInvalidationTimings {
    pub introspection_us: u64,
    pub file_ops_us: u64,
    pub file_state_us: u64,
    pub read_dirs_us: u64,
    pub metadata_us: u64,
    pub full_check_us: u64,
}

#[derive(Debug, Default)]
pub struct KnownExternalFileStateInvalidationStats {
    pub paths: usize,
    pub changed: usize,
}

impl FileChangeTracker {
    pub fn new() -> Self {
        Self {
            raw_dirs_to_dirty: Default::default(),
            raw_paths_to_dirty: Default::default(),
            maybe_modified_dirs: Default::default(),
        }
    }

    pub fn write_to_dice(mut self, ctx: &mut DiceTransactionUpdater) -> buck2_error::Result<()> {
        // See comment on `dir_entries_changed_for_watchman_bug`
        for p in self.raw_paths_to_dirty.clone() {
            if let Some(dir) = p.0.parent() {
                if self.maybe_modified_dirs.contains(&dir.to_owned()) {
                    self.entry_added_or_removed(p.0.clone());
                }
            }
        }

        ctx.changed(self.raw_dirs_to_dirty)?;
        ctx.changed(self.raw_paths_to_dirty)?;

        Ok(())
    }

    fn entry_added_or_removed(&mut self, path: CellPath) {
        self.raw_paths_to_dirty
            .insert(PathMetadataForNoWatchFsKey(path.clone()));
        let parent = path.parent();
        if let Some(parent) = parent {
            // The above can be None (validly!) if we have a cell we either create or delete.
            // That never happens in established repos, but if you are setting one up, it's not uncommon.
            // Since we don't include paths in different cells, the fact we don't dirty the parent
            // (which is in an enclosing cell) doesn't matter.
            self.insert_dir_keys(parent.to_owned());
        }
    }

    fn insert_dir_keys(&mut self, path: CellPath) {
        self.raw_dirs_to_dirty.insert(ReadDirForNoWatchFsKey(path));
    }

    pub fn file_added_or_removed(&mut self, path: CellPath) {
        self.file_contents_changed(path.clone());
        self.entry_added_or_removed(path);
    }

    pub fn dir_added_or_removed(&mut self, path: CellPath) {
        self.insert_dir_keys(path.clone());
        self.entry_added_or_removed(path);
    }

    pub fn file_contents_changed(&mut self, path: CellPath) {
        self.raw_paths_to_dirty
            .insert(PathMetadataForNoWatchFsKey(path.clone()));
    }

    /// Normally, buck does not need the file watcher to tell it that a directory's entries have
    /// changed. However, in some cases file watcher want to force-invalidate directory listings,
    /// and so this exists. It should not normally be used.
    pub fn dir_entries_changed_force_invalidate(&mut self, path: CellPath) {
        self.insert_dir_keys(path);
    }

    /// Normally, we ignore directory modification events from file watchers and instead compute
    /// them ourselves when a file in the directory is reported as having been added or removed.
    /// However, watchman has a bug in which it sometimes incorrectly doesn't report files as having
    /// been added/removed. We work around this by implementing some logic that marks a directory
    /// listing as being invalid if both the directory and at least one of its entries is reported
    /// as having been modified.
    ///
    /// We cannot unconditionally respect directory modification events from the file watcher, as it
    /// is not aware of our ignore rules.
    pub fn dir_entries_changed_for_watchman_bug(&mut self, path: CellPath) {
        self.maybe_modified_dirs.insert(path);
    }
}

pub async fn invalidate_changed_file_state(
    ctx: &mut DiceTransactionUpdater,
) -> buck2_error::Result<KnownFileStateInvalidationStats> {
    let total_start = Instant::now();
    let (read_dirs, path_metadata_for_no_watchfs) =
        ctx.existing_key_values_of_two_types_for_introspection::<
            ReadDirForNoWatchFsKey,
            PathMetadataForNoWatchFsKey,
        >();
    let read_dirs = read_dirs
        .into_iter()
        .filter(|(key, _)| !is_bazel_output_or_external_file_state_path(&key.0))
        .collect::<Vec<_>>();
    let path_metadata_for_no_watchfs = path_metadata_for_no_watchfs
        .into_iter()
        .filter(|(key, _)| !is_bazel_output_or_external_file_state_path(&key.0))
        .collect::<Vec<_>>();
    let introspection_us = total_start.elapsed().as_micros() as u64;

    let mut dice = ctx.existing_state().await;
    let no_watchfs_metadata_cache = Arc::new(NoWatchFsMetadataCache::default());

    let mut no_watchfs_cells = StdBuckHashSet::default();
    for (key, _) in &path_metadata_for_no_watchfs {
        no_watchfs_cells.insert((key.0.cell(), CheckIgnores::No));
    }
    for (key, _) in &read_dirs {
        no_watchfs_cells.insert((key.0.cell(), CheckIgnores::No));
    }
    let file_ops_by_cell = dice
        .compute_join(no_watchfs_cells, |ctx, (cell, check_ignores)| {
            async move {
                buck2_error::Ok((
                    (cell, check_ignores),
                    get_delegated_file_ops(ctx, cell, check_ignores).await?,
                ))
            }
            .boxed()
        })
        .await
        .into_iter()
        .collect::<buck2_error::Result<StdBuckHashMap<_, _>>>()?;
    let file_ops_by_cell = Arc::new(file_ops_by_cell);
    let io_provider = dice.global_data().get_io_provider();
    let file_ops_us = total_start.elapsed().as_micros() as u64 - introspection_us;

    let mut changed_read_dirs = Vec::new();
    let mut changed_read_dirs_to_value = Vec::new();
    let mut changed_path_metadata_for_no_watchfs = Vec::new();
    let mut changed_path_metadata_for_no_watchfs_to_value = Vec::new();

    let read_dirs_start = Instant::now();
    let checked_read_dirs = stream::iter(read_dirs)
        .map(|(key, old_value)| {
            check_read_dir_for_no_watchfs_direct(
                key,
                old_value,
                file_ops_by_cell.dupe(),
                io_provider.dupe(),
                no_watchfs_metadata_cache.dupe(),
            )
        })
        .buffer_unordered(NO_WATCHFS_METADATA_CHECK_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let read_dirs_us = read_dirs_start.elapsed().as_micros() as u64;

    let metadata_start = Instant::now();
    let checked_path_metadata = stream::iter(path_metadata_for_no_watchfs)
        .map(|(key, old_value)| {
            check_path_metadata_for_no_watchfs_direct(
                key,
                old_value,
                file_ops_by_cell.dupe(),
                io_provider.dupe(),
                no_watchfs_metadata_cache.dupe(),
            )
        })
        .buffer_unordered(NO_WATCHFS_METADATA_CHECK_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let metadata_us = metadata_start.elapsed().as_micros() as u64;
    let file_state_us = read_dirs_us + metadata_us;

    for dirty in checked_read_dirs {
        match dirty {
            DirtyReadDirForNoWatchFs::WithValue(key, value) => {
                changed_read_dirs_to_value.push((key, value));
            }
            DirtyReadDirForNoWatchFs::WithoutValue(key) => changed_read_dirs.push(key),
            DirtyReadDirForNoWatchFs::Unchanged => {}
        }
    }

    for dirty in checked_path_metadata {
        match dirty {
            DirtyPathMetadataForNoWatchFs::WithValue(key, value) => {
                changed_path_metadata_for_no_watchfs_to_value.push((key, value));
            }
            DirtyPathMetadataForNoWatchFs::WithoutValue(key) => {
                changed_path_metadata_for_no_watchfs.push(key)
            }
            DirtyPathMetadataForNoWatchFs::Unchanged => {}
        }
    }

    let full_check_start = Instant::now();
    let full_check_us = full_check_start.elapsed().as_micros() as u64;

    drop(dice);

    let stats = KnownFileStateInvalidationStats {
        read_files: 0,
        read_dirs: changed_read_dirs.len() + changed_read_dirs_to_value.len(),
        paths: changed_path_metadata_for_no_watchfs.len()
            + changed_path_metadata_for_no_watchfs_to_value.len(),
        exists_matching_exact_case: 0,
        events: no_watchfs_file_change_events(
            &changed_read_dirs,
            &changed_read_dirs_to_value,
            &changed_path_metadata_for_no_watchfs,
            &changed_path_metadata_for_no_watchfs_to_value,
        ),
        timings: KnownFileStateInvalidationTimings {
            introspection_us,
            file_ops_us,
            file_state_us,
            read_dirs_us,
            metadata_us,
            full_check_us,
        },
    };

    ctx.changed(changed_read_dirs)?;
    ctx.changed(changed_path_metadata_for_no_watchfs)?;
    ctx.changed_to(changed_path_metadata_for_no_watchfs_to_value)?;
    ctx.changed_to(changed_read_dirs_to_value)?;

    Ok(stats)
}

fn no_watchfs_file_change_events(
    changed_read_dirs: &[ReadDirForNoWatchFsKey],
    changed_read_dirs_to_value: &[(
        ReadDirForNoWatchFsKey,
        buck2_error::Result<Arc<[RawDirEntry]>>,
    )],
    changed_path_metadata_for_no_watchfs: &[PathMetadataForNoWatchFsKey],
    changed_path_metadata_for_no_watchfs_to_value: &[(
        PathMetadataForNoWatchFsKey,
        buck2_error::Result<Option<RawPathMetadataForNoWatchFs>>,
    )],
) -> Vec<buck2_data::FileWatcherEvent> {
    let total = changed_read_dirs.len()
        + changed_read_dirs_to_value.len()
        + changed_path_metadata_for_no_watchfs.len()
        + changed_path_metadata_for_no_watchfs_to_value.len();
    let mut events = Vec::with_capacity(total.min(MAX_NO_WATCHFS_FILE_CHANGE_RECORDS));

    for key in changed_read_dirs {
        push_no_watchfs_file_change_event(
            &mut events,
            &key.0,
            buck2_data::FileWatcherKind::Directory,
        );
    }
    for (key, _) in changed_read_dirs_to_value {
        push_no_watchfs_file_change_event(
            &mut events,
            &key.0,
            buck2_data::FileWatcherKind::Directory,
        );
    }
    for key in changed_path_metadata_for_no_watchfs {
        push_no_watchfs_file_change_event(&mut events, &key.0, buck2_data::FileWatcherKind::File);
    }
    for (key, value) in changed_path_metadata_for_no_watchfs_to_value {
        push_no_watchfs_file_change_event(&mut events, &key.0, no_watchfs_file_watcher_kind(value));
    }

    events
}

fn push_no_watchfs_file_change_event(
    events: &mut Vec<buck2_data::FileWatcherEvent>,
    path: &CellPath,
    kind: buck2_data::FileWatcherKind,
) {
    if is_bazel_output_or_external_file_state_path(path) {
        return;
    }
    if events.len() < MAX_NO_WATCHFS_FILE_CHANGE_RECORDS {
        events.push(buck2_data::FileWatcherEvent {
            event: buck2_data::FileWatcherEventType::Modify as i32,
            kind: kind as i32,
            path: path.to_string(),
        });
    }
}

fn is_bazel_output_or_external_file_state_path(path: &CellPath) -> bool {
    path.path().starts_with(CellRelativePath::unchecked_new(
        InvocationPaths::buck_out_dir_prefix().as_str(),
    )) || matches!(
        external_cell_origin_for_cell(path.cell().as_str()),
        Some(ExternalCellOrigin::Bzlmod(_) | ExternalCellOrigin::BzlmodGenerated(_))
    )
}

fn no_watchfs_file_watcher_kind(
    value: &buck2_error::Result<Option<RawPathMetadataForNoWatchFs>>,
) -> buck2_data::FileWatcherKind {
    match value {
        Ok(Some(RawPathMetadataForNoWatchFs::Directory)) => buck2_data::FileWatcherKind::Directory,
        Ok(Some(RawPathMetadataForNoWatchFs::Symlink { .. })) => {
            buck2_data::FileWatcherKind::Symlink
        }
        Ok(Some(RawPathMetadataForNoWatchFs::File(_))) | Ok(None) | Err(_) => {
            buck2_data::FileWatcherKind::File
        }
    }
}

pub async fn invalidate_changed_external_file_state(
    ctx: &mut DiceTransactionUpdater,
) -> buck2_error::Result<KnownExternalFileStateInvalidationStats> {
    if !EXTERNAL_FILE_STATE_SEEN.load(Ordering::Relaxed) {
        return Ok(KnownExternalFileStateInvalidationStats::default());
    }

    let external_paths =
        ctx.existing_key_values_of_type_for_introspection::<ExternalPathMetadataKey>();
    let paths = external_paths.len();
    if paths == 0 {
        EXTERNAL_FILE_STATE_SEEN.store(false, Ordering::Relaxed);
        return Ok(KnownExternalFileStateInvalidationStats::default());
    }

    let checked =
        stream::iter(external_paths)
            .map(|(key, old_value)| async move {
                check_external_path_metadata_direct(key, old_value).await
            })
            .buffer_unordered(NO_WATCHFS_METADATA_CHECK_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

    let mut changed_external_paths = Vec::new();
    let mut changed_external_paths_to_value = Vec::new();

    for dirty in checked {
        match dirty {
            DirtyExternalPathMetadata::WithValue(key, value) => {
                changed_external_paths_to_value.push((key, value));
            }
            DirtyExternalPathMetadata::WithoutValue(key) => {
                changed_external_paths.push(key);
            }
            DirtyExternalPathMetadata::Unchanged => {}
        }
    }

    let changed = changed_external_paths.len() + changed_external_paths_to_value.len();
    ctx.changed(changed_external_paths)?;
    ctx.changed_to(changed_external_paths_to_value)?;

    Ok(KnownExternalFileStateInvalidationStats { paths, changed })
}

enum DirtyPathMetadataForNoWatchFs {
    WithValue(
        PathMetadataForNoWatchFsKey,
        buck2_error::Result<Option<RawPathMetadataForNoWatchFs>>,
    ),
    WithoutValue(PathMetadataForNoWatchFsKey),
    Unchanged,
}

enum DirtyReadDirForNoWatchFs {
    WithValue(
        ReadDirForNoWatchFsKey,
        buck2_error::Result<Arc<[RawDirEntry]>>,
    ),
    WithoutValue(ReadDirForNoWatchFsKey),
    Unchanged,
}

enum DirtyExternalPathMetadata {
    WithValue(
        ExternalPathMetadataKey,
        buck2_error::Result<ExternalPathMetadata>,
    ),
    WithoutValue(ExternalPathMetadataKey),
    Unchanged,
}

async fn read_path_metadata_for_no_watchfs_direct(
    path: CellPathRef<'_>,
    file_ops_by_cell: &StdBuckHashMap<(CellName, CheckIgnores), FileOpsDelegateWithIgnores>,
    io_provider: Arc<dyn IoProvider>,
    no_watchfs_metadata_cache: Arc<NoWatchFsMetadataCache>,
) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
    let file_ops = file_ops_by_cell
        .get(&(path.cell(), CheckIgnores::No))
        .ok_or_else(|| internal_error!("missing file ops for no-watchfs cell `{}`", path.cell()))?;
    file_ops
        .read_path_metadata_for_no_watchfs_if_exists_without_dice(
            io_provider,
            path.path(),
            Some(no_watchfs_metadata_cache),
        )
        .await
}

async fn check_path_metadata_for_no_watchfs_direct(
    key: PathMetadataForNoWatchFsKey,
    old: Option<buck2_error::Result<Option<RawPathMetadataForNoWatchFs>>>,
    file_ops_by_cell: Arc<StdBuckHashMap<(CellName, CheckIgnores), FileOpsDelegateWithIgnores>>,
    io_provider: Arc<dyn IoProvider>,
    no_watchfs_metadata_cache: Arc<NoWatchFsMetadataCache>,
) -> DirtyPathMetadataForNoWatchFs {
    let fresh = read_path_metadata_for_no_watchfs_direct(
        key.0.as_ref(),
        &file_ops_by_cell,
        io_provider,
        no_watchfs_metadata_cache,
    )
    .await;

    match fresh {
        Ok(fresh) => {
            let fresh = Ok(fresh);
            if old
                .as_ref()
                .is_some_and(|old| PathMetadataForNoWatchFsKey::equality(old, &fresh))
            {
                DirtyPathMetadataForNoWatchFs::Unchanged
            } else {
                DirtyPathMetadataForNoWatchFs::WithValue(key, fresh)
            }
        }
        Err(_) => DirtyPathMetadataForNoWatchFs::WithoutValue(key),
    }
}

async fn read_dir_for_no_watchfs_direct(
    key: &ReadDirForNoWatchFsKey,
    file_ops_by_cell: &StdBuckHashMap<(CellName, CheckIgnores), FileOpsDelegateWithIgnores>,
    io_provider: Arc<dyn IoProvider>,
    no_watchfs_metadata_cache: Arc<NoWatchFsMetadataCache>,
) -> buck2_error::Result<Arc<[RawDirEntry]>> {
    let file_ops = file_ops_by_cell
        .get(&(key.0.cell(), CheckIgnores::No))
        .ok_or_else(|| {
            internal_error!("missing file ops for no-watchfs cell `{}`", key.0.cell())
        })?;
    file_ops
        .read_raw_dir_for_no_watchfs_without_dice(
            io_provider,
            key.0.as_ref().path(),
            Some(no_watchfs_metadata_cache),
        )
        .await
}

async fn check_read_dir_for_no_watchfs_direct(
    key: ReadDirForNoWatchFsKey,
    old: Option<buck2_error::Result<Arc<[RawDirEntry]>>>,
    file_ops_by_cell: Arc<StdBuckHashMap<(CellName, CheckIgnores), FileOpsDelegateWithIgnores>>,
    io_provider: Arc<dyn IoProvider>,
    no_watchfs_metadata_cache: Arc<NoWatchFsMetadataCache>,
) -> DirtyReadDirForNoWatchFs {
    let fresh = read_dir_for_no_watchfs_direct(
        &key,
        &file_ops_by_cell,
        io_provider,
        no_watchfs_metadata_cache,
    )
    .await;

    match fresh {
        Ok(fresh) => {
            let fresh = Ok(fresh);
            if old
                .as_ref()
                .is_some_and(|old| ReadDirForNoWatchFsKey::equality(old, &fresh))
            {
                DirtyReadDirForNoWatchFs::Unchanged
            } else {
                DirtyReadDirForNoWatchFs::WithValue(key, fresh)
            }
        }
        Err(_) => DirtyReadDirForNoWatchFs::WithoutValue(key),
    }
}

async fn fresh_path_metadata_for_no_watchfs(
    ctx: &mut DiceComputations<'_>,
    path: CellPathRef<'_>,
    no_watchfs_metadata_cache: Option<Arc<NoWatchFsMetadataCache>>,
) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
    let file_ops = get_delegated_file_ops(ctx, path.cell(), CheckIgnores::No).await?;
    file_ops
        .read_path_metadata_for_no_watchfs_if_exists_with_cache(
            ctx,
            path.path(),
            no_watchfs_metadata_cache,
        )
        .await
}

async fn check_external_path_metadata_direct(
    key: ExternalPathMetadataKey,
    old: Option<buck2_error::Result<ExternalPathMetadata>>,
) -> DirtyExternalPathMetadata {
    let fresh = read_external_path_metadata(key.0.dupe()).await;

    match fresh {
        Ok(fresh) => {
            let fresh = Ok(fresh);
            if old
                .as_ref()
                .is_some_and(|old| ExternalPathMetadataKey::equality(old, &fresh))
            {
                DirtyExternalPathMetadata::Unchanged
            } else {
                DirtyExternalPathMetadata::WithValue(key, fresh)
            }
        }
        Err(_) => DirtyExternalPathMetadata::WithoutValue(key),
    }
}

#[derive(Clone, Dupe, PartialEq, Eq, Allocative)]
struct ExternalPathMetadata {
    logical_chain: Arc<[ExternalPathState]>,
}

#[derive(Clone, Dupe, PartialEq, Eq, Allocative)]
struct ExternalPathState {
    path: Arc<ExternalSymlink>,
    metadata: Option<RawPathMetadataForNoWatchFs<Arc<ExternalSymlink>>>,
}

impl ExternalPathMetadata {
    fn followed_path_type(&self) -> Option<FollowedPathType> {
        match self
            .logical_chain
            .last()
            .and_then(|state| state.metadata.as_ref())
        {
            Some(RawPathMetadataForNoWatchFs::File(_)) => Some(FollowedPathType::File),
            Some(RawPathMetadataForNoWatchFs::Directory) => Some(FollowedPathType::Directory),
            Some(RawPathMetadataForNoWatchFs::Symlink { .. }) => {
                unreachable!("external path metadata resolution stops at non-symlink metadata")
            }
            None => None,
        }
    }
}

async fn read_external_path_metadata(
    path: Arc<ExternalSymlink>,
) -> buck2_error::Result<ExternalPathMetadata> {
    EXTERNAL_FILE_STATE_SEEN.store(true, Ordering::Relaxed);

    let mut path = path.with_full_target()?;
    let mut seen = StdBuckHashSet::default();
    let mut logical_chain = Vec::new();

    loop {
        if logical_chain.len() >= MAX_EXTERNAL_SYMLINK_EXPANSIONS {
            return Err(internal_error!(
                "too many external symlink expansions while resolving read-file metadata at `{}`",
                path
            ));
        }
        if !seen.insert(path.dupe()) {
            return Err(internal_error!(
                "external symlink cycle while resolving read-file metadata at `{}`",
                path
            ));
        }

        let metadata = read_external_path_metadata_for_no_watchfs(path.dupe()).await?;
        logical_chain.push(ExternalPathState {
            path: path.dupe(),
            metadata: metadata.dupe(),
        });

        match metadata {
            Some(RawPathMetadataForNoWatchFs::Symlink {
                at: _,
                to: RawSymlink::External(target),
            }) => {
                path = target.with_full_target()?;
            }
            Some(RawPathMetadataForNoWatchFs::Symlink {
                at: _,
                to: RawSymlink::Relative(..),
            }) => {
                return Err(internal_error!(
                    "external path metadata unexpectedly resolved to a relative symlink at `{}`",
                    path
                ));
            }
            Some(RawPathMetadataForNoWatchFs::File(_))
            | Some(RawPathMetadataForNoWatchFs::Directory)
            | None => {
                return Ok(ExternalPathMetadata {
                    logical_chain: logical_chain.into(),
                });
            }
        }
    }
}

/// The return value of a `ReadFileKey` computation.
///
/// Instead of the actual file contents, this is a closure that reads the actual file contents from
/// disk when invoked. This is done to ensure that we don't store the file contents in memory.
// FIXME(JakobDegen): `ReadFileKey` is not marked as transient if this returns an error, which is
// unfortunate.
#[derive(Clone, Dupe, Allocative)]
pub struct ReadFileProxy(
    #[allocative(skip)]
    Arc<dyn Fn() -> BoxFuture<'static, buck2_error::Result<Option<String>>> + Send + Sync>,
);

impl ReadFileProxy {
    /// This is a convenience method that avoids a little bit of boilerplate around boxing, and
    /// cloning the captures
    pub fn new_with_captures<D, F>(data: D, c: impl Fn(D) -> F + Send + Sync + 'static) -> Self
    where
        D: Clone + Send + Sync + 'static,
        F: Future<Output = buck2_error::Result<Option<String>>> + Send + 'static,
    {
        Self(Arc::new(move || {
            let data = data.clone();
            c(data).boxed()
        }))
    }
}

#[derive(Clone, Dupe, Allocative)]
struct ReadFileValue {
    proxy: ReadFileProxy,
    metadata: Option<RawPathMetadata>,
    resolved_metadata: ReadFileResolvedMetadata,
}

#[derive(Clone, Dupe, PartialEq, Eq, Allocative)]
struct ReadFileResolvedMetadata {
    cell_metadata: Option<RawPathMetadata>,
    external_metadata: Option<ExternalPathMetadata>,
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("FILE({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct ReadFileKey(Arc<CellPath>);

#[async_trait]
impl Key for ReadFileKey {
    type Value = buck2_error::Result<ReadFileValue>;
    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let file_ops = get_delegated_file_ops(ctx, self.0.cell(), CheckIgnores::No).await?;
        ctx.compute(&PathMetadataForNoWatchFsKey(self.0.as_ref().clone()))
            .await??;
        let metadata = file_ops
            .read_path_metadata_if_exists(ctx, self.0.path())
            .await?;
        let resolved_metadata = resolve_read_file_metadata(ctx, metadata.dupe()).await?;
        let proxy = file_ops.read_file_if_exists(ctx, self.0.path()).await?;
        Ok(ReadFileValue {
            proxy,
            metadata,
            resolved_metadata,
        })
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => {
                x.metadata == y.metadata && x.resolved_metadata == y.resolved_metadata
            }
            _ => false,
        }
    }

    fn invalidation_source_priority() -> InvalidationSourcePriority {
        InvalidationSourcePriority::High
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        TodoValueSerialize::<Self::Value>::new()
    }
}

async fn resolve_read_file_metadata(
    ctx: &mut DiceComputations<'_>,
    metadata: Option<RawPathMetadata>,
) -> buck2_error::Result<ReadFileResolvedMetadata> {
    let mut resolved_metadata = metadata;
    let mut seen = StdBuckHashSet::default();
    loop {
        match &resolved_metadata {
            Some(RawPathMetadata::Symlink {
                at: _,
                to: RawSymlink::Relative(target, _),
            }) => {
                let target = target.as_ref().clone();
                if !seen.insert(target.clone()) {
                    mark_bazel_skyframe_key_with_detail(
                        ctx,
                        BazelSkyframeFunction::FileSymlinkCycleUniqueness,
                        target.to_string(),
                    )
                    .await?;
                    return Err(internal_error!(
                        "symlink cycle while resolving read-file metadata at `{}`",
                        target
                    ));
                }
                resolved_metadata = ctx.compute(&PathMetadataKey(target)).await??;
            }
            Some(RawPathMetadata::Symlink {
                at: _,
                to: RawSymlink::External(target),
            }) => {
                let external_metadata = ctx
                    .compute(&ExternalPathMetadataKey(target.with_full_target()?))
                    .await??;
                return Ok(ReadFileResolvedMetadata {
                    cell_metadata: resolved_metadata,
                    external_metadata: Some(external_metadata),
                });
            }
            _ => {
                return Ok(ReadFileResolvedMetadata {
                    cell_metadata: resolved_metadata,
                    external_metadata: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use buck2_core::cells::external::BzlmodCellSetup;
    use buck2_core::cells::external::register_external_cell_origin;
    use buck2_core::cells::paths::CellRelativePathBuf;
    use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
    use dice::UserComputationData;
    use dice::testing::DiceBuilder;
    use tempfile::TempDir;

    use super::*;
    use crate::file_ops::testing::TestFileOps;

    fn cell_path(cell: CellName, path: &str) -> CellPath {
        CellPath::new(cell, CellRelativePathBuf::unchecked_new(path.to_owned()))
    }

    fn test_bzlmod_cell_setup(canonical_repo_name: &str) -> BzlmodCellSetup {
        BzlmodCellSetup {
            module_name: Arc::from("test_module"),
            version: Arc::from("1.0.0"),
            canonical_repo_name: Arc::from(canonical_repo_name),
            local_path: None,
            url: Arc::from("https://example.com/test.tar.gz"),
            urls: Arc::new(vec![Arc::from("https://example.com/test.tar.gz")]),
            integrity: Arc::from("sha256-test"),
            strip_prefix: None,
            archive_type: None,
            patches: Arc::new(Vec::new()),
            overlays: Arc::new(Vec::new()),
            patch_strip: 0,
        }
    }

    #[test]
    fn bzlmod_prefix_cell_without_external_origin_is_not_file_state_external() {
        let path = cell_path(CellName::testing_new("bzlmod_regular_cell"), "src/lib.rs");

        assert!(!is_bazel_output_or_external_file_state_path(&path));
    }

    #[test]
    fn bzlmod_external_origin_is_file_state_external() {
        let cell = CellName::testing_new("registered_bzlmod_file_state_cell");
        register_external_cell_origin(
            cell,
            ExternalCellOrigin::Bzlmod(test_bzlmod_cell_setup("registered+repo")),
        );
        let path = cell_path(cell, "src/lib.rs");

        assert!(is_bazel_output_or_external_file_state_path(&path));
    }

    #[tokio::test]
    async fn read_file_key_tracks_relative_symlink_target_metadata() -> buck2_error::Result<()> {
        let cell = CellName::testing_new("cell");
        let link = cell_path(cell, "link");
        let target = cell_path(cell, "target");

        let initial = TestFileOps::new_with_files_and_relative_symlinks(
            BTreeMap::from([(target.clone(), "old".to_owned())]),
            BTreeMap::from([(link.clone(), target.clone())]),
        );
        let mut ctx = initial
            .mock_in_cell(cell, DiceBuilder::new())
            .build(UserComputationData::new())
            .unwrap()
            .commit()
            .await;

        assert_eq!(
            DiceFileComputations::read_file_if_exists(&mut ctx, link.as_ref()).await?,
            Some("old".to_owned())
        );

        let updated = TestFileOps::new_with_files_and_relative_symlinks(
            BTreeMap::from([(target.clone(), "new".to_owned())]),
            BTreeMap::from([(link.clone(), target)]),
        );
        let mut updater = ctx.into_updater();
        updated.update_in_cell(cell, &mut updater)?;
        let mut ctx = updater.commit().await;

        assert_eq!(
            DiceFileComputations::read_file_if_exists(&mut ctx, link.as_ref()).await?,
            Some("new".to_owned())
        );

        Ok(())
    }

    #[tokio::test]
    async fn followed_path_type_resolves_relative_symlink() -> buck2_error::Result<()> {
        let cell = CellName::testing_new("cell");
        let link = cell_path(cell, "link");
        let target = cell_path(cell, "target");

        let file_ops = TestFileOps::new_with_files_and_relative_symlinks(
            BTreeMap::from([(target.clone(), "contents".to_owned())]),
            BTreeMap::from([(link.clone(), target)]),
        );
        let mut ctx = file_ops
            .mock_in_cell(cell, DiceBuilder::new())
            .build(UserComputationData::new())
            .unwrap()
            .commit()
            .await;

        assert_eq!(
            DiceFileComputations::followed_path_type_if_exists(&mut ctx, link.as_ref()).await?,
            Some(FollowedPathType::File)
        );

        Ok(())
    }

    #[tokio::test]
    async fn read_file_key_tracks_external_symlink_target_metadata() -> buck2_error::Result<()> {
        let cell = CellName::testing_new("cell");
        let link = cell_path(cell, "link");
        let tempdir = TempDir::new()?;
        let external_file = tempdir.path().join("external");
        std::fs::write(&external_file, "old")?;

        let symlink = Arc::new(ExternalSymlink::new(
            external_file.clone(),
            ForwardRelativePathBuf::default(),
        )?);
        let file_ops = TestFileOps::new_with_symlinks(BTreeMap::from([(link.clone(), symlink)]));
        let mut ctx = file_ops
            .mock_in_cell(cell, DiceBuilder::new())
            .build(UserComputationData::new())
            .unwrap()
            .commit()
            .await;

        assert_eq!(
            DiceFileComputations::read_file_if_exists(&mut ctx, link.as_ref()).await?,
            Some("old".to_owned())
        );

        std::fs::write(&external_file, "new")?;
        let mut updater = ctx.into_updater();
        let stats = invalidate_changed_external_file_state(&mut updater).await?;
        assert_eq!(stats.changed, 1);
        let mut ctx = updater.commit().await;

        assert_eq!(
            DiceFileComputations::read_file_if_exists(&mut ctx, link.as_ref()).await?,
            Some("new".to_owned())
        );

        Ok(())
    }

    #[tokio::test]
    async fn followed_path_type_resolves_external_symlink() -> buck2_error::Result<()> {
        let cell = CellName::testing_new("cell");
        let link = cell_path(cell, "link");
        let tempdir = TempDir::new()?;
        let external_file = tempdir.path().join("external");
        std::fs::write(&external_file, "contents")?;

        let symlink = Arc::new(ExternalSymlink::new(
            external_file,
            ForwardRelativePathBuf::default(),
        )?);
        let file_ops = TestFileOps::new_with_symlinks(BTreeMap::from([(link.clone(), symlink)]));
        let mut ctx = file_ops
            .mock_in_cell(cell, DiceBuilder::new())
            .build(UserComputationData::new())
            .unwrap()
            .commit()
            .await;

        assert_eq!(
            DiceFileComputations::followed_path_type_if_exists(&mut ctx, link.as_ref()).await?,
            Some(FollowedPathType::File)
        );

        Ok(())
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("DIRECTORY_LISTING({})", path)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct ReadDirKey {
    path: CellPath,
    check_ignores: CheckIgnores,
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("FILE({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct ExternalPathMetadataKey(Arc<ExternalSymlink>);

#[async_trait]
impl Key for ExternalPathMetadataKey {
    type Value = buck2_error::Result<ExternalPathMetadata>;

    async fn compute(
        &self,
        _ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        read_external_path_metadata(self.0.dupe()).await
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn invalidation_source_priority() -> InvalidationSourcePriority {
        InvalidationSourcePriority::High
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        TodoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("FILE_STATE({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct PathMetadataForNoWatchFsKey(CellPath);

#[async_trait]
impl Key for PathMetadataForNoWatchFsKey {
    type Value = buck2_error::Result<Option<RawPathMetadataForNoWatchFs>>;
    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let no_watchfs_metadata_cache = ctx
            .per_transaction_data()
            .data
            .get::<Arc<NoWatchFsMetadataCache>>()
            .ok()
            .map(|cache| cache.dupe());
        fresh_path_metadata_for_no_watchfs(ctx, self.0.as_ref(), no_watchfs_metadata_cache).await
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn invalidation_source_priority() -> InvalidationSourcePriority {
        InvalidationSourcePriority::High
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("DIRECTORY_LISTING_STATE({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct ReadDirForNoWatchFsKey(CellPath);

#[async_trait]
impl Key for ReadDirForNoWatchFsKey {
    type Value = buck2_error::Result<Arc<[RawDirEntry]>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let origin = ctx.get_external_cell_origin(self.0.cell()).await?;
        let file_ops = get_delegated_file_ops(ctx, self.0.cell(), CheckIgnores::No).await?;
        if matches!(origin, Some(ExternalCellOrigin::BzlmodGenerated(_))) {
            return file_ops
                .read_raw_dir_for_no_watchfs(ctx, self.0.as_ref().path())
                .await;
        }
        let no_watchfs_metadata_cache = ctx
            .per_transaction_data()
            .data
            .get::<Arc<NoWatchFsMetadataCache>>()
            .ok()
            .map(|cache| cache.dupe());
        file_ops
            .read_raw_dir_for_no_watchfs_without_dice(
                ctx.global_data().get_io_provider(),
                self.0.as_ref().path(),
                no_watchfs_metadata_cache,
            )
            .await
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn invalidation_source_priority() -> InvalidationSourcePriority {
        InvalidationSourcePriority::High
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        TodoValueSerialize::<Self::Value>::new()
    }
}

#[async_trait]
impl Key for ReadDirKey {
    type Value = buck2_error::Result<ReadDirOutput>;
    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let raw = ctx
            .compute(&ReadDirForNoWatchFsKey(self.path.clone()))
            .await??;
        let file_ops = get_delegated_file_ops(ctx, self.path.cell(), self.check_ignores).await?;
        file_ops.make_read_dir_output(self.path.as_ref().path(), raw)
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Allocative, Debug, Eq, Hash, PartialEq, Pagable)]
#[display("FILE({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct ExistsMatchingExactCaseKey(CellPath);

#[async_trait]
impl Key for ExistsMatchingExactCaseKey {
    type Value = buck2_error::Result<bool>;
    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let file_ops = get_delegated_file_ops(ctx, self.0.cell(), CheckIgnores::Yes).await?;
        if let Some(parent) = self.0.parent() {
            let raw = ctx
                .compute(&ReadDirForNoWatchFsKey(parent.to_owned()))
                .await??;
            return file_ops.exists_matching_exact_case_from_raw_dir(self.0.path(), raw);
        }
        file_ops.exists_matching_exact_case_from_raw_dir(self.0.path(), Arc::from([]))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("FILE({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct PathMetadataKey(CellPath);

#[async_trait]
impl Key for PathMetadataKey {
    type Value = buck2_error::Result<Option<RawPathMetadata>>;
    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        ctx.compute(&PathMetadataForNoWatchFsKey(self.0.clone()))
            .await??;
        let res = get_delegated_file_ops(ctx, self.0.cell(), CheckIgnores::No)
            .await?
            .read_path_metadata_if_exists(ctx, self.0.as_ref().path())
            .await?;

        Ok(res)
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn invalidation_source_priority() -> InvalidationSourcePriority {
        InvalidationSourcePriority::High
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

/// out-of-line impl for DiceComputations::read_dir_ext so it doesn't add noise to the api
async fn read_dir_ext(
    ctx: &mut DiceComputations<'_>,
    path: CellPathRef<'_>,
) -> Result<ReadDirOutput, ReadDirError> {
    match DiceFileComputations::read_dir(ctx, path).await {
        Ok(v) => Ok(v),
        Err(e) => match extended_ignore_error(ctx, path).await {
            Some(e) => Err(e),
            None => Err(e.into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_watchfs_file_change_events_include_sample_paths() {
        let read_dir = ReadDirForNoWatchFsKey(CellPath::testing_new("root//pkg"));
        let path_metadata =
            PathMetadataForNoWatchFsKey(CellPath::testing_new("root//pkg/file.txt"));

        let events = no_watchfs_file_change_events(&[read_dir], &[], &[path_metadata], &[]);

        assert_eq!(events.len(), 2);
        assert_eq!(
            buck2_data::FileWatcherKind::try_from(events[0].kind).unwrap(),
            buck2_data::FileWatcherKind::Directory
        );
        assert_eq!(events[0].path, "root//pkg");
        assert_eq!(
            buck2_data::FileWatcherKind::try_from(events[1].kind).unwrap(),
            buck2_data::FileWatcherKind::File
        );
        assert_eq!(events[1].path, "root//pkg/file.txt");
    }
}
