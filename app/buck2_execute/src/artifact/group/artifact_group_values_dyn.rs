/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_common::file_ops::metadata::TrackedFileDigest;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;

use crate::artifact::artifact_dyn::ArtifactDyn;
use crate::artifact_value::ArtifactValue;
use crate::digest_config::DigestConfig;
use crate::directory::LazyActionDirectoryBuilder;

/// This is like `ArtifactGroupValues`, but without dependency on `Artifact`.
pub trait ArtifactGroupValuesDyn: Send + Sync + 'static {
    fn iter(&self) -> Box<dyn Iterator<Item = (&dyn ArtifactDyn, &ArtifactValue)> + '_>;

    fn action_cache_fingerprint(&self) -> Option<&[u8]> {
        None
    }

    fn directory_fingerprint_for_action_cache(&self) -> Option<(&TrackedFileDigest, u64)> {
        None
    }

    fn add_to_directory(
        &self,
        builder: &mut LazyActionDirectoryBuilder,
        artifact_fs: &ArtifactFs,
    ) -> buck2_error::Result<()>;

    fn add_to_directory_for_execution(
        &self,
        builder: &mut LazyActionDirectoryBuilder,
        artifact_fs: &ArtifactFs,
        digest_config: DigestConfig,
    ) -> buck2_error::Result<()> {
        let _ = digest_config;
        self.add_to_directory(builder, artifact_fs)
    }
}
