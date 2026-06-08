/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::sync::Arc;

use allocative::Allocative;
use bz_core::content_hash::ContentBasedPathHash;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative)]
pub struct CommandExecutionInputOwner {
    id: Arc<str>,
}

impl CommandExecutionInputOwner {
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self { id: id.into() }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for CommandExecutionInputOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.id.fmt(f)
    }
}

pub trait ArtifactDyn: Send + Sync + 'static {
    /// Returns the project relative path of the artifact.
    /// A build artifact that is declared to be content-based must have a content hash
    /// provided, otherwise an error is returned.
    fn resolve_path(
        &self,
        fs: &ArtifactFs,
        content_hash: Option<&ContentBasedPathHash>,
    ) -> bz_error::Result<ProjectRelativePathBuf>;

    /// This function will return the same project relative path as `resolve_path` except
    /// for content-based artifacts, where it will return a path that uses the configuration
    /// hash instead of the content hash.
    fn resolve_configuration_hash_path(
        &self,
        fs: &ArtifactFs,
    ) -> bz_error::Result<ProjectRelativePathBuf>;
    /// Build artifacts and source artifacts from external cells require materialization. Other
    /// source artifacts do not.
    fn requires_materialization(&self, fs: &ArtifactFs) -> bool;

    fn has_content_based_path(&self) -> bool;

    fn is_projected(&self) -> bool;

    fn input_owner(&self) -> Option<CommandExecutionInputOwner> {
        None
    }
}
