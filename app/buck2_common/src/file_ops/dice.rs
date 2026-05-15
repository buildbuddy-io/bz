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

use allocative::Allocative;
use async_trait::async_trait;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::cell_path::CellPathRef;
use buck2_core::cells::name::CellName;
use buck2_fs::paths::file_name::FileNameBuf;
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
use futures::future::BoxFuture;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::buildfiles::HasBuildfiles;
use crate::file_ops::delegate::get_delegated_file_ops;
use crate::file_ops::error::FileReadError;
use crate::file_ops::error::extended_ignore_error;
use crate::file_ops::metadata::RawPathMetadata;
use crate::file_ops::metadata::RawPathMetadataForNoWatchFs;
use crate::file_ops::metadata::ReadDirOutput;
use crate::ignores::file_ignores::FileIgnoreResult;
use crate::io::NoWatchFsMetadataCache;
use crate::io::ReadDirError;

pub struct DiceFileComputations;

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
    files_to_dirty: StdBuckHashSet<ReadFileKey>,
    dirs_to_dirty: StdBuckHashSet<ReadDirKey>,
    paths_to_dirty: StdBuckHashSet<PathMetadataKey>,
    exists_matching_exact_case_to_dirty: StdBuckHashSet<ExistsMatchingExactCaseKey>,

    maybe_modified_dirs: StdBuckHashSet<CellPath>,
}

#[derive(Debug, Default)]
pub struct KnownFileStateInvalidationStats {
    pub read_files: usize,
    pub read_dirs: usize,
    pub paths: usize,
    pub exists_matching_exact_case: usize,
}

impl KnownFileStateInvalidationStats {
    pub fn total(&self) -> usize {
        self.read_files + self.read_dirs + self.paths + self.exists_matching_exact_case
    }
}

impl FileChangeTracker {
    pub fn new() -> Self {
        Self {
            files_to_dirty: Default::default(),
            dirs_to_dirty: Default::default(),
            paths_to_dirty: Default::default(),
            maybe_modified_dirs: Default::default(),
            exists_matching_exact_case_to_dirty: Default::default(),
        }
    }

    pub fn write_to_dice(mut self, ctx: &mut DiceTransactionUpdater) -> buck2_error::Result<()> {
        // See comment on `dir_entries_changed_for_watchman_bug`
        for p in self.paths_to_dirty.clone() {
            if let Some(dir) = p.0.parent() {
                if self.maybe_modified_dirs.contains(&dir.to_owned()) {
                    self.entry_added_or_removed(p.0.clone());
                }
            }
        }

        ctx.changed(self.files_to_dirty)?;
        ctx.changed(self.dirs_to_dirty)?;
        ctx.changed(self.paths_to_dirty)?;
        ctx.changed(self.exists_matching_exact_case_to_dirty)?;

        Ok(())
    }

    fn entry_added_or_removed(&mut self, path: CellPath) {
        self.paths_to_dirty.insert(PathMetadataKey(path.clone()));
        self.exists_matching_exact_case_to_dirty
            .insert(ExistsMatchingExactCaseKey(path.clone()));
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
        self.dirs_to_dirty.insert(ReadDirKey {
            path: path.clone(),
            check_ignores: CheckIgnores::No,
        });
        self.dirs_to_dirty.insert(ReadDirKey {
            path,
            check_ignores: CheckIgnores::Yes,
        });
    }

    pub fn file_added_or_removed(&mut self, path: CellPath) {
        self.file_contents_changed(path.clone());
        self.entry_added_or_removed(path);
    }

    pub fn dir_added_or_removed(&mut self, path: CellPath) {
        self.entry_added_or_removed(path);
    }

    pub fn file_contents_changed(&mut self, path: CellPath) {
        self.files_to_dirty
            .insert(ReadFileKey(Arc::new(path.clone())));
        self.paths_to_dirty.insert(PathMetadataKey(path.clone()));
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
    let mut read_files = Vec::new();
    let mut read_dirs = Vec::new();
    let mut paths = Vec::new();
    let mut exists_matching_exact_case = Vec::new();
    let mut paths_for_no_watchfs = StdBuckHashSet::default();

    for key in ctx.existing_keys_for_introspection() {
        if let Some(key) = key.downcast_ref::<ReadFileKey>() {
            read_files.push(key.clone());
        } else if let Some(key) = key.downcast_ref::<ReadDirKey>() {
            read_dirs.push(key.clone());
        } else if let Some(key) = key.downcast_ref::<PathMetadataKey>() {
            paths.push(key.clone());
        } else if let Some(key) = key.downcast_ref::<ExistsMatchingExactCaseKey>() {
            exists_matching_exact_case.push(key.clone());
        } else if let Some(key) = key.downcast_ref::<PathMetadataForNoWatchFsKey>() {
            paths_for_no_watchfs.insert(key.0.clone());
        }
    }

    let mut dice = ctx.existing_state().await;
    let no_watchfs_metadata_cache = Arc::new(NoWatchFsMetadataCache::default());

    let mut metadata_paths = StdBuckHashSet::default();
    for key in &read_files {
        metadata_paths.insert(key.0.as_ref().clone());
    }
    for key in &paths {
        metadata_paths.insert(key.0.clone());
    }

    let mut metadata_paths_with_no_watchfs = Vec::new();
    let mut metadata_paths_without_no_watchfs = Vec::new();
    for path in metadata_paths {
        if paths_for_no_watchfs.contains(&path) {
            metadata_paths_with_no_watchfs.push(path);
        } else {
            metadata_paths_without_no_watchfs.push(path);
        }
    }

    let checked_path_metadata_for_no_watchfs = dice
        .compute_join(metadata_paths_with_no_watchfs, |ctx, path| {
            let no_watchfs_metadata_cache = no_watchfs_metadata_cache.dupe();
            async move {
                let key = PathMetadataForNoWatchFsKey(path);
                let fresh = fresh_path_metadata_for_no_watchfs(
                    ctx,
                    key.0.as_ref(),
                    Some(no_watchfs_metadata_cache),
                )
                .await;

                match fresh {
                    Ok(fresh) => DirtyPathMetadataForNoWatchFs::WithValue(key, Ok(fresh)),
                    Err(_) => DirtyPathMetadataForNoWatchFs::WithoutValue(key),
                }
            }
            .boxed()
        })
        .await;

    let mut changed_path_metadata_for_no_watchfs = Vec::new();
    let mut changed_path_metadata_for_no_watchfs_to_value = Vec::new();
    for dirty in checked_path_metadata_for_no_watchfs {
        match dirty {
            DirtyPathMetadataForNoWatchFs::WithValue(key, value) => {
                changed_path_metadata_for_no_watchfs_to_value.push((key, value));
            }
            DirtyPathMetadataForNoWatchFs::WithoutValue(key) => {
                changed_path_metadata_for_no_watchfs.push(key);
            }
        }
    }

    let seeded_path_metadata_for_no_watchfs = dice
        .compute_join(metadata_paths_without_no_watchfs.clone(), |ctx, path| {
            let no_watchfs_metadata_cache = no_watchfs_metadata_cache.dupe();
            async move {
                let key = PathMetadataForNoWatchFsKey(path);
                fresh_path_metadata_for_no_watchfs(
                    ctx,
                    key.0.as_ref(),
                    Some(no_watchfs_metadata_cache),
                )
                .await
                .ok()
                .map(|fresh| (key, Ok(fresh)))
            }
            .boxed()
        })
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    changed_path_metadata_for_no_watchfs_to_value.extend(seeded_path_metadata_for_no_watchfs);

    let metadata_paths_without_no_watchfs = metadata_paths_without_no_watchfs
        .into_iter()
        .collect::<StdBuckHashSet<_>>();

    let mut changed_read_files = Vec::new();
    let changed_read_files_from_full_check = dice
        .compute_join(
            read_files
                .into_iter()
                .filter(|key| metadata_paths_without_no_watchfs.contains(key.0.as_ref())),
            |ctx, key| {
                async move {
                    if read_file_key_is_dirty(ctx, &key).await {
                        Some(key)
                    } else {
                        None
                    }
                }
                .boxed()
            },
        )
        .await
        .into_iter()
        .flatten();
    changed_read_files.extend(changed_read_files_from_full_check);

    let mut changed_paths = Vec::new();
    let changed_paths_from_full_check = dice
        .compute_join(
            paths
                .into_iter()
                .filter(|key| metadata_paths_without_no_watchfs.contains(&key.0)),
            |ctx, key| {
                async move {
                    if path_metadata_key_is_dirty(ctx, &key).await {
                        Some(key)
                    } else {
                        None
                    }
                }
                .boxed()
            },
        )
        .await
        .into_iter()
        .flatten();
    changed_paths.extend(changed_paths_from_full_check);

    let changed_read_dirs = dice
        .compute_join(read_dirs, |ctx, key| {
            async move {
                if read_dir_key_is_dirty(ctx, &key).await {
                    Some(key)
                } else {
                    None
                }
            }
            .boxed()
        })
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    let changed_exists_matching_exact_case = dice
        .compute_join(exists_matching_exact_case, |ctx, key| {
            async move {
                if exists_matching_exact_case_key_is_dirty(ctx, &key).await {
                    Some(key)
                } else {
                    None
                }
            }
            .boxed()
        })
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    drop(dice);

    let stats = KnownFileStateInvalidationStats {
        read_files: changed_read_files.len(),
        read_dirs: changed_read_dirs.len(),
        paths: changed_paths.len(),
        exists_matching_exact_case: changed_exists_matching_exact_case.len(),
    };

    ctx.changed(changed_read_files)?;
    ctx.changed(changed_read_dirs)?;
    ctx.changed(changed_paths)?;
    ctx.changed(changed_exists_matching_exact_case)?;
    ctx.changed(changed_path_metadata_for_no_watchfs)?;
    ctx.changed_to(changed_path_metadata_for_no_watchfs_to_value)?;

    Ok(stats)
}

enum DirtyPathMetadataForNoWatchFs {
    WithValue(
        PathMetadataForNoWatchFsKey,
        buck2_error::Result<Option<RawPathMetadataForNoWatchFs>>,
    ),
    WithoutValue(PathMetadataForNoWatchFsKey),
}

async fn read_file_key_is_dirty(ctx: &mut DiceComputations<'_>, key: &ReadFileKey) -> bool {
    let old = ctx.compute(key).await;
    let fresh = fresh_path_metadata(ctx, key.0.as_ref().as_ref()).await;

    match (old, fresh) {
        (Ok(Ok(old)), Ok(fresh)) => old.metadata != fresh,
        _ => true,
    }
}

async fn read_dir_key_is_dirty(ctx: &mut DiceComputations<'_>, key: &ReadDirKey) -> bool {
    let old = ctx.compute(key).await;
    let fresh = fresh_read_dir(ctx, key).await;

    match (old, fresh) {
        (Ok(Ok(old)), Ok(fresh)) => old != fresh,
        _ => true,
    }
}

async fn exists_matching_exact_case_key_is_dirty(
    ctx: &mut DiceComputations<'_>,
    key: &ExistsMatchingExactCaseKey,
) -> bool {
    let old = ctx.compute(key).await;
    let fresh = fresh_exists_matching_exact_case(ctx, key).await;

    match (old, fresh) {
        (Ok(Ok(old)), Ok(fresh)) => old != fresh,
        _ => true,
    }
}

async fn path_metadata_key_is_dirty(ctx: &mut DiceComputations<'_>, key: &PathMetadataKey) -> bool {
    let old = ctx.compute(key).await;
    let fresh = fresh_path_metadata(ctx, key.0.as_ref()).await;

    match (old, fresh) {
        (Ok(Ok(old)), Ok(fresh)) => old != fresh,
        _ => true,
    }
}

async fn fresh_path_metadata(
    ctx: &mut DiceComputations<'_>,
    path: CellPathRef<'_>,
) -> buck2_error::Result<Option<RawPathMetadata>> {
    let file_ops = get_delegated_file_ops(ctx, path.cell(), CheckIgnores::No).await?;
    file_ops
        .read_path_metadata_if_exists_for_no_watchfs(ctx, path.path())
        .await
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

async fn fresh_read_dir(
    ctx: &mut DiceComputations<'_>,
    key: &ReadDirKey,
) -> buck2_error::Result<ReadDirOutput> {
    let file_ops = get_delegated_file_ops(ctx, key.path.cell(), key.check_ignores).await?;
    file_ops
        .read_dir_for_no_watchfs(ctx, key.path.as_ref().path())
        .await
}

async fn fresh_exists_matching_exact_case(
    ctx: &mut DiceComputations<'_>,
    key: &ExistsMatchingExactCaseKey,
) -> buck2_error::Result<bool> {
    let file_ops = get_delegated_file_ops(ctx, key.0.cell(), CheckIgnores::Yes).await?;
    file_ops
        .exists_matching_exact_case_for_no_watchfs(key.0.path(), ctx)
        .await
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
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
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
        let proxy = file_ops.read_file_if_exists(ctx, self.0.path()).await?;
        Ok(ReadFileValue { proxy, metadata })
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x.metadata == y.metadata,
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

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("{}", path)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct ReadDirKey {
    path: CellPath,
    check_ignores: CheckIgnores,
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("{}", _0)]
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
        fresh_path_metadata_for_no_watchfs(ctx, self.0.as_ref(), None).await
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

#[async_trait]
impl Key for ReadDirKey {
    type Value = buck2_error::Result<ReadDirOutput>;
    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let file_ops = get_delegated_file_ops(ctx, self.path.cell(), self.check_ignores).await?;
        file_ops.read_dir(ctx, self.path.as_ref().path()).await
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
#[display("{}", _0)]
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
        get_delegated_file_ops(ctx, self.0.cell(), CheckIgnores::Yes)
            .await?
            .exists_matching_exact_case(self.0.path(), ctx)
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

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
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

        if let Some(RawPathMetadata::Symlink {
            at: ref path,
            to: _,
        }) = res
        {
            ctx.compute(&ReadFileKey(path.dupe())).await??;
        }

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
