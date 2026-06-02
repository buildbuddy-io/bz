/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

pub mod fs;
pub mod trace;

use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use bz_core::cells::cell_path::CellPath;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_error::BuckErrorContext;
use bz_error::ErrorTag;
use bz_fs::paths::file_name::FileName;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_hash::BuckDashMap;

use crate::file_ops::metadata::FileType;
use crate::file_ops::metadata::RawDirEntry;
use crate::file_ops::metadata::RawPathMetadata;
use crate::file_ops::metadata::RawPathMetadataForNoWatchFs;
use crate::ignores::file_ignores::FileIgnoreReason;

pub(crate) enum CachedDirentType {
    Found(FileType),
    NotFound,
    Unknown,
}

pub struct NoWatchFsMetadataCache {
    pub(crate) metadata: BuckDashMap<
        ForwardRelativePathBuf,
        Option<RawPathMetadataForNoWatchFs<ForwardRelativePathBuf>>,
    >,
    readdirs: BuckDashMap<ForwardRelativePathBuf, Arc<[RawDirEntry]>>,
}

impl Default for NoWatchFsMetadataCache {
    fn default() -> Self {
        Self {
            metadata: Default::default(),
            readdirs: Default::default(),
        }
    }
}

impl NoWatchFsMetadataCache {
    pub fn seed_readdir(&self, dir: ForwardRelativePathBuf, entries: Arc<[RawDirEntry]>) {
        self.readdirs
            .entry(dir.clone())
            .or_insert_with(|| entries.clone());
        self.metadata
            .entry(dir.clone())
            .or_insert(Some(RawPathMetadataForNoWatchFs::Directory));

        for entry in entries.iter() {
            if entry.file_type != FileType::Directory {
                continue;
            }
            let Ok(file_name) = FileName::new(entry.file_name.as_str()) else {
                continue;
            };
            let mut child = dir.clone();
            child.push(file_name);
            self.metadata
                .entry(child)
                .or_insert(Some(RawPathMetadataForNoWatchFs::Directory));
        }
    }

    pub(crate) fn cached_dirent_type(&self, path: &ForwardRelativePath) -> CachedDirentType {
        let Some((parent, file_name)) = path.split_last() else {
            return CachedDirentType::Unknown;
        };
        let Some(parent) = self.readdirs.get(parent) else {
            return CachedDirentType::Unknown;
        };
        match parent.binary_search_by(|entry| entry.file_name.as_str().cmp(file_name.as_str())) {
            Ok(index) => CachedDirentType::Found(parent[index].file_type),
            Err(_) => {
                if cached_readdir_proves_absence(file_name.as_str(), parent.iter()) {
                    CachedDirentType::NotFound
                } else {
                    CachedDirentType::Unknown
                }
            }
        }
    }
}

fn cached_readdir_proves_absence<'a>(
    file_name: &str,
    entries: impl IntoIterator<Item = &'a RawDirEntry>,
) -> bool {
    if !cfg!(any(target_os = "macos", windows)) {
        return true;
    }

    if !file_name.is_ascii() {
        return false;
    }

    !entries.into_iter().any(|entry| {
        let entry_name = entry.file_name.as_str();
        !entry_name.is_ascii() || entry_name.eq_ignore_ascii_case(file_name)
    })
}

#[derive(Debug, Allocative, bz_error::Error)]
#[buck2(tag = Input)]
pub enum ReadDirError {
    #[error("Directory `{path}` does not exist")]
    DirectoryDoesNotExist {
        path: CellPath,
        suggestion: DirectoryDoesNotExistSuggestion,
    },
    #[error("Directory `{0}` is ignored ({})", .1.describe())]
    DirectoryIsIgnored(CellPath, FileIgnoreReason),
    #[error("Path `{0}` is `{1}`, not a directory")]
    NotADirectory(CellPath, String),
    #[error(transparent)]
    Error(bz_error::Error),
}

#[derive(Debug, Allocative)]
pub enum DirectoryDoesNotExistSuggestion {
    Cell(Vec<String>),
    Typo(String),
    NoSuggestion,
}

impl From<bz_error::Error> for ReadDirError {
    fn from(value: bz_error::Error) -> Self {
        Self::Error(value)
    }
}

#[async_trait]
pub trait IoProvider: Allocative + Send + Sync {
    async fn read_file_if_exists_impl(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<Option<String>>;

    async fn read_dir_impl(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<Vec<RawDirEntry>>;

    async fn read_path_metadata_if_exists_impl(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<Option<RawPathMetadata<ProjectRelativePathBuf>>>;

    async fn read_path_metadata_if_exists_for_no_watchfs_impl(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs<ProjectRelativePathBuf>>> {
        Ok(self
            .read_path_metadata_if_exists_impl(path)
            .await?
            .map(RawPathMetadataForNoWatchFs::from))
    }

    async fn read_path_metadata_if_exists_for_no_watchfs_impl_with_cache(
        &self,
        path: ProjectRelativePathBuf,
        _cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs<ProjectRelativePathBuf>>> {
        self.read_path_metadata_if_exists_for_no_watchfs_impl(path)
            .await
    }

    /// Request that this I/O provider be up to date with whatever I/O operations the user might
    /// have done until this point.
    async fn settle(&self) -> bz_error::Result<()>;

    fn name(&self) -> &'static str;

    /// Returns the Eden version of the underlying system of the IoProvider, if available.
    async fn eden_version(&self) -> bz_error::Result<Option<String>>;

    fn project_root(&self) -> &ProjectRoot;

    fn as_any(&self) -> &dyn std::any::Any;
}

impl dyn IoProvider + '_ {
    pub async fn read_file_if_exists(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<Option<String>> {
        self.read_file_if_exists_impl(path)
            .await
            .tag(ErrorTag::IoSource)
    }

    pub async fn read_dir(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<Vec<RawDirEntry>> {
        self.read_dir_impl(path).await.tag(ErrorTag::IoSource)
    }

    pub async fn read_path_metadata_if_exists(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<Option<RawPathMetadata<ProjectRelativePathBuf>>> {
        self.read_path_metadata_if_exists_impl(path)
            .await
            .tag(ErrorTag::IoSource)
    }

    pub async fn read_path_metadata_if_exists_for_no_watchfs(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs<ProjectRelativePathBuf>>> {
        self.read_path_metadata_if_exists_for_no_watchfs_impl(path)
            .await
            .tag(ErrorTag::IoSource)
    }

    pub async fn read_path_metadata_if_exists_for_no_watchfs_with_cache(
        &self,
        path: ProjectRelativePathBuf,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs<ProjectRelativePathBuf>>> {
        self.read_path_metadata_if_exists_for_no_watchfs_impl_with_cache(path, cache)
            .await
            .tag(ErrorTag::IoSource)
    }
}
