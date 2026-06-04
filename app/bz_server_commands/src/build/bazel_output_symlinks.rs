/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use bz_artifact::artifact::artifact_type::BaseArtifactKind;
use bz_build_api::build::BuildProviderType;
use bz_build_api::build::ConfiguredBuildTargetResult;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::buck_out_path::BazelOutputRoot;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_error::BuckErrorContext;
use bz_fs::error::IoResultExt;
use bz_fs::fs_util;
use bz_fs::paths::abs_norm_path::AbsNormPathBuf;
use tracing::warn;

#[derive(Clone, Default)]
pub(crate) struct BazelOutputSymlinkPaths {
    bin: Option<ProjectRelativePathBuf>,
    genfiles: Option<ProjectRelativePathBuf>,
}

impl BazelOutputSymlinkPaths {
    pub(crate) fn path_for(&self, root: BazelOutputRoot) -> Option<&ProjectRelativePath> {
        match root {
            BazelOutputRoot::Bin => self.bin.as_deref(),
            BazelOutputRoot::Genfiles => self.genfiles.as_deref(),
        }
    }
}

#[derive(Default)]
struct CandidateRoots {
    bin: BTreeSet<ProjectRelativePathBuf>,
    genfiles: BTreeSet<ProjectRelativePathBuf>,
}

impl CandidateRoots {
    fn insert(&mut self, root: BazelOutputRoot, path: ProjectRelativePathBuf) {
        match root {
            BazelOutputRoot::Bin => {
                self.bin.insert(path);
            }
            BazelOutputRoot::Genfiles => {
                self.genfiles.insert(path);
            }
        }
    }
}

pub(crate) fn create_bazel_output_symlinks(
    configured: &BTreeMap<ConfiguredProvidersLabel, Option<ConfiguredBuildTargetResult>>,
    artifact_fs: &ArtifactFs,
    fs: &ProjectRoot,
) -> bz_error::Result<BazelOutputSymlinkPaths> {
    let roots = collect_candidate_roots(configured, artifact_fs);
    let bin = create_convenience_symlink(BazelOutputRoot::Bin, &roots.bin, fs)?;
    let genfiles = create_convenience_symlink(BazelOutputRoot::Genfiles, &roots.genfiles, fs)?;
    Ok(BazelOutputSymlinkPaths { bin, genfiles })
}

fn collect_candidate_roots(
    configured: &BTreeMap<ConfiguredProvidersLabel, Option<ConfiguredBuildTargetResult>>,
    artifact_fs: &ArtifactFs,
) -> CandidateRoots {
    let mut roots = CandidateRoots::default();

    for result in configured.values().flatten() {
        for output in result
            .outputs
            .iter()
            .filter_map(|output| output.inner.as_ref().ok())
        {
            if !matches!(output.provider_type, BuildProviderType::Default) {
                continue;
            }

            for (artifact, _) in output.values.iter() {
                let (BaseArtifactKind::Build(build), _) = artifact.as_parts() else {
                    continue;
                };
                let path = build.get_path();
                let Some(label) = path.bazel_owner() else {
                    continue;
                };
                roots.insert(
                    path.bazel_output_root(),
                    artifact_fs
                        .buck_out_path_resolver()
                        .resolve_bazel_physical_output_root(label),
                );
            }
        }
    }

    roots
}

fn create_convenience_symlink(
    root: BazelOutputRoot,
    candidates: &BTreeSet<ProjectRelativePathBuf>,
    fs: &ProjectRoot,
) -> bz_error::Result<Option<ProjectRelativePathBuf>> {
    let link_path = ProjectRelativePathBuf::unchecked_new(root.exec_root().to_owned());
    let link_abs = fs.resolve(&link_path);

    match candidates.len() {
        0 => Ok(None),
        1 => {
            let target = candidates.iter().next().unwrap();
            let target_abs = fs.resolve(target);
            match create_or_replace_symlink(&target_abs, &link_abs) {
                Ok(()) => Ok(Some(target.to_buf())),
                Err(e) => {
                    warn!("{e}");
                    Ok(None)
                }
            }
        }
        _ => {
            remove_existing_symlink(&link_abs)?;
            warn!(
                "Cleared {} because it would not contain all requested targets' outputs: {:?}",
                root.exec_root(),
                candidates
            );
            Ok(None)
        }
    }
}

fn create_or_replace_symlink(
    target: &AbsNormPathBuf,
    link: &AbsNormPathBuf,
) -> bz_error::Result<()> {
    fs_util::create_dir_all(target)
        .with_buck_error_context(|| format!("Error creating Bazel output root `{target}`"))?;
    if let Some(parent) = link.parent() {
        fs_util::create_dir_all(parent).with_buck_error_context(|| {
            format!(
                "Error creating parent directory for Bazel output symlink `{}`",
                link.display()
            )
        })?;
    }

    if fs_util::read_link(link)
        .map(|current| current == target.as_path())
        .unwrap_or(false)
    {
        return Ok(());
    }

    remove_existing_path_for_symlink(link)?;
    fs_util::symlink(target.as_path(), link)
        .categorize_internal()
        .with_buck_error_context(|| {
            format!(
                "Error creating Bazel convenience symlink `{}` -> `{}`",
                link.display(),
                target.display()
            )
        })
}

fn remove_existing_symlink(link: &AbsNormPathBuf) -> bz_error::Result<()> {
    if fs_util::read_link(link).is_ok() {
        fs_util::remove_file(link)
            .categorize_internal()
            .with_buck_error_context(|| {
                format!(
                    "Error removing Bazel convenience symlink `{}`",
                    link.display()
                )
            })?;
    }
    Ok(())
}

fn remove_existing_path_for_symlink(link: &AbsNormPathBuf) -> bz_error::Result<()> {
    let Some(metadata) = fs_util::symlink_metadata_if_exists(link)? else {
        return Ok(());
    };

    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "Cannot replace existing directory `{}` with a Bazel convenience symlink",
            link.display()
        ));
    }

    fs_util::remove_all(link)
        .categorize_internal()
        .with_buck_error_context(|| {
            format!(
                "Error removing existing path before creating Bazel convenience symlink `{}`",
                link.display()
            )
        })
}
