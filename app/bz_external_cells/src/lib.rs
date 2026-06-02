/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![feature(assert_matches)]
#![feature(error_generic_member_access)]
#![feature(once_cell_try)]

use std::sync::Arc;

use async_trait::async_trait;
use bz_common::bazel::bzlmod::BZLMOD_MODULE_EXTENSION_EVALUATOR;
use bz_common::bazel::bzlmod::BzlmodModuleExtensionEvaluator;
use bz_common::dice::data::HasIoProvider;
use bz_common::file_ops::delegate::FileOpsDelegate;
use bz_common::file_ops::metadata::RawPathMetadata;
use bz_core::cells::cell_root_path::CellRootPath;
use bz_core::cells::external::BzlmodModuleExtensionRepoSetup;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::name::CellName;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use dice::CancellationContext;
use dice::DiceComputations;

mod bundled;
mod bzlmod;
mod git;

struct ConcreteExternalCellsImpl;

struct ConcreteBzlmodModuleExtensionEvaluator;

#[derive(bz_error::Error, Debug)]
#[buck2(tag = Tier0)]
enum ExternalCellsError {
    #[error("Tried to expand external cell to `{0}`, but that directory already contains data!")]
    ExpandDataAlreadyPresent(ProjectRelativePathBuf),
}

#[async_trait]
impl bz_common::external_cells::ExternalCellsImpl for ConcreteExternalCellsImpl {
    async fn get_file_ops_delegate(
        &self,
        ctx: &mut DiceComputations<'_>,
        cell_name: CellName,
        origin: ExternalCellOrigin,
    ) -> bz_error::Result<Arc<dyn FileOpsDelegate>> {
        match origin {
            ExternalCellOrigin::Bundled(cell_name) => {
                Ok(bundled::get_file_ops_delegate(ctx, cell_name).await? as _)
            }
            ExternalCellOrigin::Git(setup) => {
                Ok(git::get_file_ops_delegate(ctx, cell_name, setup).await? as _)
            }
            ExternalCellOrigin::Bzlmod(setup) => {
                Ok(bzlmod::get_file_ops_delegate(ctx, cell_name, setup).await? as _)
            }
            ExternalCellOrigin::BzlmodGenerated(setup) => {
                Ok(bzlmod::get_generated_file_ops_delegate(ctx, cell_name, setup).await? as _)
            }
        }
    }

    async fn ensure_cell_alias_resolver_ready(
        &self,
        ctx: &mut DiceComputations<'_>,
        cell_name: CellName,
        origin: ExternalCellOrigin,
    ) -> bz_error::Result<()> {
        match origin {
            ExternalCellOrigin::BzlmodGenerated(setup) => {
                bzlmod::ensure_generated_cell_alias_resolver_ready(ctx, cell_name, setup).await
            }
            _ => Ok(()),
        }
    }

    async fn prepare_cached_cell_root(
        &self,
        ctx: &mut DiceComputations<'_>,
        cell_name: CellName,
        origin: ExternalCellOrigin,
        cancellations: &CancellationContext,
    ) -> bz_error::Result<()> {
        match origin {
            ExternalCellOrigin::Bzlmod(setup) => {
                bzlmod::prepare_cached_cell_root(ctx, cell_name, setup, cancellations).await
            }
            ExternalCellOrigin::BzlmodGenerated(setup) => {
                bzlmod::prepare_cached_generated_cell_root(ctx, cell_name, setup, cancellations)
                    .await
            }
            _ => Ok(()),
        }
    }

    fn check_bundled_cell_exists(&self, cell_name: CellName) -> bz_error::Result<()> {
        bundled::find_bundled_data(cell_name).map(|_| ())
    }

    async fn expand(
        &self,
        ctx: &mut DiceComputations<'_>,
        cell: CellName,
        origin: ExternalCellOrigin,
        path: &CellRootPath,
    ) -> bz_error::Result<()> {
        let dest_path = path.as_project_relative_path().to_buf();
        let io = ctx.global_data().get_io_provider();

        // Make sure we're not about to overwrite existing data
        match io.read_path_metadata_if_exists(dest_path.clone()).await? {
            None => (),
            Some(RawPathMetadata::Directory) => {
                let data = io.read_dir(dest_path.clone()).await?;
                if !data.is_empty() {
                    return Err(ExternalCellsError::ExpandDataAlreadyPresent(dest_path).into());
                }
            }
            Some(_) => {
                return Err(ExternalCellsError::ExpandDataAlreadyPresent(dest_path).into());
            }
        }

        // Materialize the whole cell, and then copy it into the repository.
        //
        // FIXME(JakobDegen): Ideally we'd be able to ask the materializer to just make a copy
        // without doing the actual materialization. However, that's not currently possible without
        // it resulting in the materializer tracking paths in the repo, so this will have to do for
        // now.
        let materialized_path = match origin {
            ExternalCellOrigin::Bundled(cell) => bundled::materialize_all(ctx, cell).await?,
            ExternalCellOrigin::Git(setup) => git::materialize_all(ctx, cell, setup).await?,
            ExternalCellOrigin::Bzlmod(setup) => bzlmod::materialize_all(ctx, cell, setup).await?,
            ExternalCellOrigin::BzlmodGenerated(setup) => {
                bzlmod::materialize_generated_all(ctx, cell, setup).await?
            }
        };

        Ok(io.project_root().copy(&materialized_path, &dest_path)?)
    }
}

#[async_trait]
impl BzlmodModuleExtensionEvaluator for ConcreteBzlmodModuleExtensionEvaluator {
    async fn evaluate_bzlmod_module_extension(
        &self,
        ctx: &mut DiceComputations<'_>,
        setup: BzlmodModuleExtensionRepoSetup,
    ) -> bz_error::Result<Vec<String>> {
        bzlmod::evaluate_module_extension(ctx, setup).await
    }
}

pub fn init_late_bindings() {
    bz_common::external_cells::EXTERNAL_CELLS_IMPL.init(&ConcreteExternalCellsImpl);
    BZLMOD_MODULE_EXTENSION_EVALUATOR.init(&ConcreteBzlmodModuleExtensionEvaluator);
}
