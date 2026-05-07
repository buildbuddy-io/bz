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

use async_trait::async_trait;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePathBuf;
use dice::DiceComputations;

use crate::dice::cells::HasCellResolver;
use crate::file_ops::dice::DiceFileComputations;
use crate::ignores::file_ignores::CellFileIgnores;
use crate::ignores::ignore_set::bazelignore_to_ignore_spec;
use crate::legacy_configs::dice::HasLegacyConfigs;
use crate::legacy_configs::dice::OpaqueLegacyBuckConfigOnDice;
use crate::legacy_configs::key::BuckconfigKeyRef;

#[async_trait]
pub(crate) trait HasCellFileIgnores {
    async fn new_cell_ignores(
        &mut self,
        cell_name: CellName,
    ) -> buck2_error::Result<Arc<CellFileIgnores>>;
}

#[async_trait]
impl HasCellFileIgnores for DiceComputations<'_> {
    async fn new_cell_ignores(
        &mut self,
        cell_name: CellName,
    ) -> buck2_error::Result<Arc<CellFileIgnores>> {
        let cells = self.get_cell_resolver().await?;
        let instance = cells.get(cell_name)?;
        if matches!(
            instance.external(),
            Some(ExternalCellOrigin::Bzlmod(_)) | Some(ExternalCellOrigin::BzlmodGenerated(_))
        ) {
            let ignore_spec = read_bazelignore_spec(self, cell_name).await?;
            let cell_ignores = CellFileIgnores::new_for_interpreter(
                &ignore_spec,
                instance.nested_cells().clone(),
                cells.is_root_cell(cell_name),
            )?;
            return Ok(Arc::new(cell_ignores));
        }

        let config = self.get_legacy_config_on_dice(cell_name).await?;

        let ignore_spec = config.lookup(
            self,
            BuckconfigKeyRef {
                section: "project",
                property: "ignore",
            },
        )?;
        let ignore_spec = ignore_spec.as_ref().map_or("", |s| &**s);
        let ignore_spec = if bazel_compat_enabled(self, &config)? {
            let bazelignore_spec = read_bazelignore_spec(self, cell_name).await?;
            merge_ignore_specs(ignore_spec, &bazelignore_spec)
        } else {
            ignore_spec.to_owned()
        };

        let cell_ignores = CellFileIgnores::new_for_interpreter(
            &ignore_spec,
            instance.nested_cells().clone(),
            cells.is_root_cell(cell_name),
        )?;

        Ok(Arc::new(cell_ignores))
    }
}

async fn read_bazelignore_spec(
    ctx: &mut DiceComputations<'_>,
    cell_name: CellName,
) -> buck2_error::Result<String> {
    let bazelignore_path = CellPath::new(
        cell_name,
        CellRelativePathBuf::unchecked_new(".bazelignore".to_owned()),
    );
    let bazelignore =
        DiceFileComputations::read_file_if_exists(ctx, bazelignore_path.as_ref()).await?;
    match &bazelignore {
        Some(contents) => bazelignore_to_ignore_spec(contents),
        None => Ok(String::new()),
    }
}

fn bazel_compat_enabled(
    ctx: &mut DiceComputations<'_>,
    config: &OpaqueLegacyBuckConfigOnDice,
) -> buck2_error::Result<bool> {
    let enabled = config.lookup(
        ctx,
        BuckconfigKeyRef {
            section: "bazel",
            property: "compatibility",
        },
    )?;
    Ok(enabled
        .as_deref()
        .map(|value| matches!(value.trim(), "1" | "true" | "True" | "TRUE"))
        .unwrap_or(false))
}

fn merge_ignore_specs(project_ignore: &str, bazelignore: &str) -> String {
    match (
        project_ignore.trim().is_empty(),
        bazelignore.trim().is_empty(),
    ) {
        (true, true) => String::new(),
        (false, true) => project_ignore.to_owned(),
        (true, false) => bazelignore.to_owned(),
        (false, false) => format!("{project_ignore},{bazelignore}"),
    }
}
