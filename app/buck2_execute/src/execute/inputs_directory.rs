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

use buck2_common::file_ops::metadata::FileMetadata;
use buck2_common::file_ops::metadata::Symlink;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_error::BuckErrorContext;
use buck2_fs::paths::RelativePath;
use dupe::Dupe;

use crate::artifact_value::ArtifactValue;
use crate::digest_config::DigestConfig;
use crate::directory::ActionDirectoryBuilder;
use crate::directory::ActionDirectoryMember;
use crate::directory::ExternalSymlinkUploadPath;
use crate::directory::LazyActionDirectoryBuilder;
use crate::directory::finalize_lazy_action_directory;
use crate::directory::insert_artifact_lazy_for_execution;
use crate::execute::request::CommandExecutionInput;

pub fn inputs_directory(
    inputs: &[CommandExecutionInput],
    digest_config: DigestConfig,
    fs: &ArtifactFs,
) -> buck2_error::Result<(ActionDirectoryBuilder, Vec<ExternalSymlinkUploadPath>)> {
    let mut builder = LazyActionDirectoryBuilder::empty();
    let mut external_symlink_upload_paths = Vec::new();
    for input in inputs {
        match input {
            CommandExecutionInput::Artifact(group) => {
                group.add_to_directory_for_execution(
                    &mut builder,
                    fs,
                    digest_config,
                    &mut external_symlink_upload_paths,
                )?;
            }
            CommandExecutionInput::ArtifactPathAlias {
                source_path,
                path,
                value,
                ..
            } => {
                let abs_path = fs.fs().resolve(source_path);
                let value =
                    value.resolve_source_file_proxy(abs_path.as_abs_path(), digest_config)?;
                let value = rebase_target_file_symlink_alias(source_path, path, value)?;
                insert_artifact_lazy_for_execution(
                    &mut builder,
                    path.clone(),
                    &value,
                    digest_config,
                    &mut external_symlink_upload_paths,
                )?;
            }
            CommandExecutionInput::EmptyFile(path) => {
                builder.insert(
                    path.clone().into(),
                    DirectoryEntry::Leaf(ActionDirectoryMember::File(digest_config.empty_file())),
                )?;
            }
            CommandExecutionInput::ActionMetadata(metadata) => {
                let path = fs
                    .buck_out_path_resolver()
                    .resolve_gen(&metadata.path, Some(&metadata.content_hash))?;
                builder.insert(
                    path.into(),
                    DirectoryEntry::Leaf(ActionDirectoryMember::File(FileMetadata {
                        digest: metadata.digest.dupe(),
                        is_executable: false,
                    })),
                )?;
            }
            CommandExecutionInput::ScratchPath(path) => {
                let path = fs.buck_out_path_resolver().resolve_scratch(path)?;
                builder.insert(
                    path.into(),
                    DirectoryEntry::Dir(digest_config.empty_directory()),
                )?;
            }
            CommandExecutionInput::IncrementalRemoteOutput(path, entry) => match entry {
                DirectoryEntry::Dir(d) => {
                    builder.insert(path.clone().into(), DirectoryEntry::Dir(d.dupe()))?;
                }
                DirectoryEntry::Leaf(m) => {
                    builder.insert(path.clone().into(), DirectoryEntry::Leaf(m.dupe()))?;
                }
            },
        };
    }
    Ok((
        finalize_lazy_action_directory(builder)?,
        external_symlink_upload_paths,
    ))
}

fn rebase_target_file_symlink_alias(
    source_path: &ProjectRelativePath,
    path: &ProjectRelativePath,
    value: ArtifactValue,
) -> buck2_error::Result<ArtifactValue> {
    let DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(symlink)) = value.entry() else {
        return Ok(value);
    };
    if value.deps().is_none() {
        return Ok(value);
    }

    // Bazel target_file symlink outputs resolve through the action input tree. When Buck exposes
    // that output at an additional execroot path, keep the symlink pointing at the original input.
    let source_parent = source_path.parent().unwrap_or(ProjectRelativePath::empty());
    let resolved_target = source_parent
        .join_normalized(symlink.target())
        .with_buck_error_context(|| {
            format!("Error rebasing symlink artifact path alias `{path}` -> `{source_path}`")
        })?;
    let path_parent = path.parent().unwrap_or(ProjectRelativePath::empty());
    let path_parent: &RelativePath = path_parent.as_ref();
    let resolved_target: &RelativePath = resolved_target.as_ref();
    let rebased_target = path_parent.relative(resolved_target);

    Ok(ArtifactValue::new(
        DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(Arc::new(Symlink::new(
            rebased_target,
        )))),
        value.deps().map(Dupe::dupe),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use buck2_common::file_ops::metadata::Symlink;
    use buck2_core::fs::project_rel_path::ProjectRelativePath;
    use buck2_directory::directory::entry::DirectoryEntry;
    use buck2_fs::paths::RelativePathBuf;

    use crate::artifact_value::ArtifactValue;
    use crate::digest_config::DigestConfig;
    use crate::directory::ActionDirectoryBuilder;
    use crate::directory::ActionDirectoryEntry;
    use crate::directory::ActionDirectoryMember;
    use crate::directory::INTERNER;
    use crate::execute::inputs_directory::rebase_target_file_symlink_alias;

    fn empty_deps() -> crate::directory::ActionSharedDirectory {
        ActionDirectoryBuilder::empty()
            .fingerprint(DigestConfig::testing_default().as_directory_serializer())
            .shared(&*INTERNER)
    }

    #[test]
    fn target_file_symlink_alias_is_rebased_to_original_target() -> buck2_error::Result<()> {
        let value = ArtifactValue::new(
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::Symlink(Arc::new(Symlink::new(
                RelativePathBuf::from("../builder"),
            )))),
            Some(empty_deps()),
        );

        let value = rebase_target_file_symlink_alias(
            ProjectRelativePath::new("buck-out/bin/hash/external/sdk/builder_reset/builder")?,
            ProjectRelativePath::new(
                "buck-out/v2/__bazel_execroot/action/buck-out/bin/hash/external/sdk/builder_reset/builder",
            )?,
            value,
        )?;

        let DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(symlink)) = value.entry() else {
            panic!("expected symlink");
        };
        assert_eq!(
            symlink.target().as_str(),
            "../../../../../../../../../bin/hash/external/sdk/builder",
        );
        assert!(value.deps().is_some());
        Ok(())
    }

    #[test]
    fn symlink_alias_without_deps_is_left_unchanged() -> buck2_error::Result<()> {
        let value = ArtifactValue::new(
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::Symlink(Arc::new(Symlink::new(
                RelativePathBuf::from("../builder"),
            )))),
            None,
        );

        let value = rebase_target_file_symlink_alias(
            ProjectRelativePath::new("buck-out/bin/hash/external/sdk/builder_reset/builder")?,
            ProjectRelativePath::new(
                "buck-out/v2/__bazel_execroot/action/buck-out/bin/hash/external/sdk/builder_reset/builder",
            )?,
            value,
        )?;

        let DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(symlink)) = value.entry() else {
            panic!("expected symlink");
        };
        assert_eq!(symlink.target().as_str(), "../builder");
        assert!(value.deps().is_none());
        Ok(())
    }
}
