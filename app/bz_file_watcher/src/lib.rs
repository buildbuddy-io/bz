/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![feature(error_generic_member_access)]
#![feature(used_with_arg)]

use bz_core::cells::CellResolver;
use bz_core::cells::cell_path::CellPath;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::external::external_cell_origin_for_cell;
use bz_core::fs::project_rel_path::ProjectRelativePath;

pub mod dep_files;
pub mod file_watcher;
mod fs_hash_crawler;
pub mod mergebase;
mod no_watchfs;
mod notify;
mod stats;
mod watchman;

/// Returns true if the given path is a Watchman cookie file.
///
/// Watchman creates and deletes `.watchman-cookie-*` files as synchronization
/// markers to establish ordering barriers with the underlying filesystem
/// notification backend. These are not user source changes and should never
/// trigger DICE invalidation or rebuilds.
pub(crate) fn is_watchman_cookie(path: &ProjectRelativePath) -> bool {
    path.file_name()
        .is_some_and(|f| f.as_str().starts_with(".watchman-cookie-"))
}

/// Bazel treats generated external repository contents as repository state, not
/// as ordinary source files. Ignore their filesystem notifications here and let
/// the repository materialization keys decide whether the repo is current.
pub(crate) fn is_bzlmod_external_cell_path(cells: &CellResolver, cell_path: &CellPath) -> bool {
    if matches!(
        cells
            .get(cell_path.cell())
            .ok()
            .and_then(|cell| cell.external()),
        Some(ExternalCellOrigin::Bzlmod(_) | ExternalCellOrigin::BzlmodGenerated(_))
    ) {
        return true;
    }

    matches!(
        external_cell_origin_for_cell(cell_path.cell().as_str()),
        Some(ExternalCellOrigin::Bzlmod(_) | ExternalCellOrigin::BzlmodGenerated(_))
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bz_core::cells::cell_root_path::CellRootPathBuf;
    use bz_core::cells::external::BzlmodCellSetup;
    use bz_core::cells::external::register_external_cell_origin;
    use bz_core::cells::name::CellName;
    use bz_core::cells::paths::CellRelativePathBuf;

    use super::*;

    fn cell_path(cell: CellName, path: &str) -> CellPath {
        CellPath::new(cell, CellRelativePathBuf::unchecked_new(path.to_owned()))
    }

    fn cell_resolver(cell: CellName) -> CellResolver {
        CellResolver::testing_with_name_and_path(cell, CellRootPathBuf::testing_new(""))
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
    fn bzlmod_prefix_cell_without_external_origin_is_not_ignored() {
        let cell = CellName::testing_new("bzlmod_regular_watched_cell");
        let cells = cell_resolver(cell);
        let path = cell_path(cell, "src/lib.rs");

        assert!(!is_bzlmod_external_cell_path(&cells, &path));
    }

    #[test]
    fn registered_bzlmod_external_origin_is_ignored() {
        let cell = CellName::testing_new("registered_bzlmod_watched_cell");
        register_external_cell_origin(
            cell,
            ExternalCellOrigin::Bzlmod(test_bzlmod_cell_setup("registered+watched")),
        );
        let cells = cell_resolver(cell);
        let path = cell_path(cell, "src/lib.rs");

        assert!(is_bzlmod_external_cell_path(&cells, &path));
    }
}
