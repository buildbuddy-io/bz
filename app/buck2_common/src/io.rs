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
use buck2_core::cells::cell_path::CellPath;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_error::BuckErrorContext;
use buck2_error::ErrorTag;
use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use buck2_hash::BuckDashMap;

use crate::file_ops::metadata::RawDirEntry;
use crate::file_ops::metadata::RawPathMetadata;
use crate::file_ops::metadata::RawPathMetadataForNoWatchFs;
use crate::ignores::file_ignores::FileIgnoreReason;

pub struct NoWatchFsMetadataCache(
    pub(crate) BuckDashMap<
        ForwardRelativePathBuf,
        Option<RawPathMetadataForNoWatchFs<ForwardRelativePathBuf>>,
    >,
);

impl Default for NoWatchFsMetadataCache {
    fn default() -> Self {
        Self(Default::default())
    }
}

#[derive(Debug, Allocative, buck2_error::Error)]
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
    Error(buck2_error::Error),
}

#[derive(Debug, Allocative)]
pub enum DirectoryDoesNotExistSuggestion {
    Cell(Vec<String>),
    Typo(String),
    NoSuggestion,
}

impl From<buck2_error::Error> for ReadDirError {
    fn from(value: buck2_error::Error) -> Self {
        Self::Error(value)
    }
}

#[async_trait]
pub trait IoProvider: Allocative + Send + Sync {
    async fn read_file_if_exists_impl(
        &self,
        path: ProjectRelativePathBuf,
    ) -> buck2_error::Result<Option<String>>;

    async fn read_dir_impl(
        &self,
        path: ProjectRelativePathBuf,
    ) -> buck2_error::Result<Vec<RawDirEntry>>;

    async fn read_path_metadata_if_exists_impl(
        &self,
        path: ProjectRelativePathBuf,
    ) -> buck2_error::Result<Option<RawPathMetadata<ProjectRelativePathBuf>>>;

    async fn read_path_metadata_if_exists_for_no_watchfs_impl(
        &self,
        path: ProjectRelativePathBuf,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs<ProjectRelativePathBuf>>> {
        Ok(self
            .read_path_metadata_if_exists_impl(path)
            .await?
            .map(RawPathMetadataForNoWatchFs::from))
    }

    async fn read_path_metadata_if_exists_for_no_watchfs_impl_with_cache(
        &self,
        path: ProjectRelativePathBuf,
        _cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs<ProjectRelativePathBuf>>> {
        self.read_path_metadata_if_exists_for_no_watchfs_impl(path)
            .await
    }

    /// Request that this I/O provider be up to date with whatever I/O operations the user might
    /// have done until this point.
    async fn settle(&self) -> buck2_error::Result<()>;

    fn name(&self) -> &'static str;

    /// Returns the Eden version of the underlying system of the IoProvider, if available.
    async fn eden_version(&self) -> buck2_error::Result<Option<String>>;

    fn project_root(&self) -> &ProjectRoot;

    fn as_any(&self) -> &dyn std::any::Any;
}

impl dyn IoProvider + '_ {
    pub async fn read_file_if_exists(
        &self,
        path: ProjectRelativePathBuf,
    ) -> buck2_error::Result<Option<String>> {
        self.read_file_if_exists_impl(path)
            .await
            .tag(ErrorTag::IoSource)
    }

    pub async fn read_dir(
        &self,
        path: ProjectRelativePathBuf,
    ) -> buck2_error::Result<Vec<RawDirEntry>> {
        self.read_dir_impl(path).await.tag(ErrorTag::IoSource)
    }

    pub async fn read_path_metadata_if_exists(
        &self,
        path: ProjectRelativePathBuf,
    ) -> buck2_error::Result<Option<RawPathMetadata<ProjectRelativePathBuf>>> {
        self.read_path_metadata_if_exists_impl(path)
            .await
            .tag(ErrorTag::IoSource)
    }

    pub async fn read_path_metadata_if_exists_for_no_watchfs(
        &self,
        path: ProjectRelativePathBuf,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs<ProjectRelativePathBuf>>> {
        self.read_path_metadata_if_exists_for_no_watchfs_impl(path)
            .await
            .tag(ErrorTag::IoSource)
    }

    pub async fn read_path_metadata_if_exists_for_no_watchfs_with_cache(
        &self,
        path: ProjectRelativePathBuf,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs<ProjectRelativePathBuf>>> {
        self.read_path_metadata_if_exists_for_no_watchfs_impl_with_cache(path, cache)
            .await
            .tag(ErrorTag::IoSource)
    }
}
