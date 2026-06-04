/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_build_api::artifact_groups::ArtifactGroupValues;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_directory::directory::directory::Directory;
use bz_directory::directory::directory_iterator::DirectoryIterator;
use bz_directory::directory::directory_iterator::DirectoryIteratorPathStack;
use bz_execute::digest_config::DigestConfig;
use bz_execute::directory::ActionDirectoryMember;
use bz_execute::directory::LazyActionDirectoryBuilder;
use bz_execute::directory::finalize_lazy_action_directory;
use bz_execute::execute::paths_with_digest::PathsWithDigestBlobData;
use bz_execute::execute::paths_with_digest::PathsWithDigestBuilder;

pub(crate) fn metadata_content(
    fs: &ArtifactFs,
    inputs: &[&ArtifactGroupValues],
    digest_config: DigestConfig,
) -> bz_error::Result<(PathsWithDigestBlobData, TrackedFileDigest)> {
    let mut blob_builder = PathsWithDigestBuilder::default();

    let mut builder = LazyActionDirectoryBuilder::empty();
    let mut external_symlink_upload_paths = Vec::new();
    let mut resolved_symlink_upload_paths = Vec::new();
    for &group in inputs {
        group.add_to_directory_for_execution(
            &mut builder,
            fs,
            digest_config,
            &mut external_symlink_upload_paths,
            &mut resolved_symlink_upload_paths,
        )?;
    }
    let builder = finalize_lazy_action_directory(builder)?;

    let mut walk = builder.ordered_walk_leaves();
    while let Some((path, item)) = walk.next() {
        match item {
            ActionDirectoryMember::File(metadata) => {
                blob_builder.add(path.get(), metadata.digest.data());
            }
            ActionDirectoryMember::SourceFile(_) => {
                return Err(bz_error::internal_error!(
                    "source file proxy must be resolved before action metadata"
                ));
            }
            // Omit symlinks and let user script detect and handle symlinks in inputs.
            // Metadata will contain artifacts which are symlinked, meaning the user
            // can resolve the symlink and get the digest of the symlinked artifact.
            ActionDirectoryMember::Symlink(_) | ActionDirectoryMember::ExternalSymlink(_) => {}
        }
    }

    let blob = blob_builder.build()?;

    let digest = TrackedFileDigest::from_content(&blob.0.0, digest_config.cas_digest_config());
    Ok((blob, digest))
}

pub(crate) fn metadata_digest(
    fs: &ArtifactFs,
    inputs: &[&ArtifactGroupValues],
    digest_config: DigestConfig,
) -> bz_error::Result<TrackedFileDigest> {
    let mut blob_builder = PathsWithDigestBuilder::default();

    let mut builder = LazyActionDirectoryBuilder::empty();
    let mut external_symlink_upload_paths = Vec::new();
    let mut resolved_symlink_upload_paths = Vec::new();
    for &group in inputs {
        group.add_to_directory_for_execution(
            &mut builder,
            fs,
            digest_config,
            &mut external_symlink_upload_paths,
            &mut resolved_symlink_upload_paths,
        )?;
    }
    let builder = finalize_lazy_action_directory(builder)?;

    let mut walk = builder.ordered_walk_leaves();
    while let Some((path, item)) = walk.next() {
        match item {
            ActionDirectoryMember::File(metadata) => {
                blob_builder.add(path.get(), metadata.digest.data());
            }
            ActionDirectoryMember::SourceFile(_) => {
                return Err(bz_error::internal_error!(
                    "source file proxy must be resolved before action metadata"
                ));
            }
            // Omit symlinks and let user script detect and handle symlinks in inputs.
            // Metadata will contain artifacts which are symlinked, meaning the user
            // can resolve the symlink and get the digest of the symlinked artifact.
            ActionDirectoryMember::Symlink(_) | ActionDirectoryMember::ExternalSymlink(_) => {}
        }
    }

    blob_builder.build_digest(digest_config.cas_digest_config())
}
