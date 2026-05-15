/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_core::cells::CellResolver;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_error::BuckErrorContext;
use buck2_hash::BuckDashMap;
use cmp_any::PartialEqAny;
use derivative::Derivative;
use dice::DiceComputations;
use dice::UserComputationData;
use dupe::Dupe;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::dice::data::HasIoProvider;
use crate::file_ops::delegate::FileOpsDelegate;
use crate::file_ops::dice::ReadFileProxy;
use crate::file_ops::metadata::RawDirEntry;
use crate::file_ops::metadata::RawPathMetadata;
use crate::file_ops::metadata::RawPathMetadataForNoWatchFs;
use crate::io::IoProvider;
use crate::io::NoWatchFsMetadataCache;

/// A `FileOpsDelegate` implementation that calls out to the `IoProvider` to read files.
///
/// This is used for everything except 1) tests, and 2) external cells.
#[derive(Clone, Dupe, Derivative, Allocative, Pagable)]
#[derivative(PartialEq)]
pub(super) struct IoFileOpsDelegate {
    pub(super) cells: CellResolver,
    pub(super) cell: CellName,
}

impl IoFileOpsDelegate {
    fn resolve(&self, path: &CellRelativePath) -> buck2_error::Result<ProjectRelativePathBuf> {
        let cell_root = self.cells.get(self.cell)?.path();
        Ok(cell_root.as_project_relative_path().join(path))
    }

    fn get_cell_path(&self, path: &ProjectRelativePath) -> CellPath {
        self.cells.get_cell_path(path)
    }

    async fn read_dir_uncached(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir_uncached_with_io_provider(ctx.global_data().get_io_provider(), path)
            .await
    }

    async fn read_dir_uncached_with_io_provider(
        &self,
        io_provider: Arc<dyn IoProvider>,
        path: &CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir_uncached_with_io_provider_and_metadata_cache(io_provider, path, None)
            .await
    }

    async fn read_dir_uncached_with_io_provider_and_metadata_cache(
        &self,
        io_provider: Arc<dyn IoProvider>,
        path: &CellRelativePath,
        metadata_cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        let project_path = self.resolve(path)?;
        let forward_project_path = metadata_cache
            .as_ref()
            .map(|_| project_path.as_forward_relative_path().to_owned());
        let mut entries = io_provider
            .read_dir(project_path)
            .await
            .with_buck_error_context(|| format!("Error listing dir `{path}`"))?;

        // Make sure entries are deterministic, since read_dir isn't.
        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        if let (Some(metadata_cache), Some(forward_project_path)) =
            (metadata_cache, forward_project_path)
        {
            let entries = Arc::<[RawDirEntry]>::from(entries);
            metadata_cache.seed_readdir(forward_project_path, entries.clone());
            return Ok(entries);
        }

        Ok(Arc::from(entries))
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_impl(
        &self,
        io_provider: Arc<dyn IoProvider>,
        path: &CellRelativePath,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        let project_path = self.resolve(path)?;

        let res = io_provider
            .read_path_metadata_if_exists_for_no_watchfs_with_cache(project_path, cache)
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?;
        Ok(res.map(|meta| meta.map(|path| Arc::new(self.get_cell_path(&path)))))
    }
}

#[pagable_typetag]
#[async_trait]
impl FileOpsDelegate for IoFileOpsDelegate {
    async fn read_file_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<ReadFileProxy> {
        Ok(ReadFileProxy::new_with_captures(
            (self.resolve(path)?, ctx.global_data().get_io_provider()),
            |(project_path, io)| async move { io.read_file_if_exists(project_path).await },
        ))
    }

    async fn read_dir(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        let project_path = self.resolve(path)?;
        {
            let read_dir_cache = ctx
                .per_transaction_data()
                .data
                .get::<ReadDirCache>()
                .expect("ReadDirCache is expected to be set.");
            if let Some(cached) = read_dir_cache.0.get(&project_path) {
                return Ok(cached.clone());
            };
        }
        let entries = self.read_dir_uncached(ctx, path).await?;
        let read_dir_cache = ctx
            .per_transaction_data()
            .data
            .get::<ReadDirCache>()
            .expect("ReadDirCache is expected to be set.");
        read_dir_cache.0.insert(project_path, entries.clone());

        Ok(entries)
    }

    async fn read_dir_for_no_watchfs(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir_uncached(ctx, path).await
    }

    async fn read_dir_for_no_watchfs_without_dice(
        &self,
        io_provider: Arc<dyn IoProvider>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir_uncached_with_io_provider(io_provider, path)
            .await
    }

    async fn read_dir_for_no_watchfs_without_dice_with_metadata_cache(
        &self,
        io_provider: Arc<dyn IoProvider>,
        path: &'async_trait CellRelativePath,
        metadata_cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir_uncached_with_io_provider_and_metadata_cache(
            io_provider,
            path,
            metadata_cache,
        )
        .await
    }

    async fn read_path_metadata_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
        let project_path = self.resolve(path)?;

        let res = ctx
            .global_data()
            .get_io_provider()
            .read_path_metadata_if_exists(project_path)
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?;
        Ok(res.map(|meta| meta.map(|path| Arc::new(self.get_cell_path(&path)))))
    }

    async fn read_path_metadata_for_no_watchfs_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        let cache = ctx
            .per_transaction_data()
            .data
            .get::<Arc<NoWatchFsMetadataCache>>()
            .ok()
            .map(|cache| cache.dupe());

        self.read_path_metadata_for_no_watchfs_if_exists_impl(
            ctx.global_data().get_io_provider(),
            path,
            cache,
        )
        .await
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_without_dice(
        &self,
        io_provider: Arc<dyn IoProvider>,
        path: &'async_trait CellRelativePath,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_impl(io_provider, path, cache)
            .await
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_with_cache(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_impl(
            ctx.global_data().get_io_provider(),
            path,
            cache,
        )
        .await
    }

    async fn exists_matching_exact_case(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<bool> {
        let Some(dir) = path.parent() else {
            // FIXME(JakobDegen): Blindly assuming that cell roots exist isn't quite right, I'll fix
            // this later in the stack
            return Ok(true);
        };
        // FIXME(JakobDegen): Unwrap is ok because a parent exists, but there should be a better API
        // for this
        let entry = path.file_name().unwrap();
        let dir = self.read_dir(ctx, dir).await?;
        Ok(dir.iter().any(|f| &*f.file_name == entry))
    }

    async fn exists_matching_exact_case_for_no_watchfs(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<bool> {
        let Some(dir) = path.parent() else {
            // FIXME(JakobDegen): Blindly assuming that cell roots exist isn't quite right, I'll fix
            // this later in the stack
            return Ok(true);
        };
        // FIXME(JakobDegen): Unwrap is ok because a parent exists, but there should be a better API
        // for this
        let entry = path.file_name().unwrap();
        let dir = self.read_dir_for_no_watchfs(ctx, dir).await?;
        Ok(dir.iter().any(|f| &*f.file_name == entry))
    }

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::new(self)
    }
}

struct ReadDirCache(BuckDashMap<ProjectRelativePathBuf, Arc<[RawDirEntry]>>);

pub fn initialize_read_dir_cache(data: &mut UserComputationData) {
    data.data.set(ReadDirCache(Default::default()));
    data.data.set(Arc::new(NoWatchFsMetadataCache::default()));
}
