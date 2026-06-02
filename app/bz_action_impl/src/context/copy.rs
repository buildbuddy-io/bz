/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_build_api::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use bz_build_api::interpreter::rule_defs::artifact::output_artifact_like::OutputArtifactArg;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use bz_build_api::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use bz_build_api::interpreter::rule_defs::context::AnalysisActions;
use bz_execute::execute::request::OutputType;
use bz_hash::buck_indexset;
use dupe::OptionDupedExt;
use starlark::environment::MethodsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::ValueTyped;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::none::NoneOr;

use crate::actions::impls::copy::CopyMode;
use crate::actions::impls::copy::UnregisteredCopyAction;
use crate::actions::impls::copy::UnregisteredSymlinkAction;
use crate::actions::impls::symlinked_dir::UnregisteredSymlinkedDirAction;

fn create_dir_tree<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    this: &AnalysisActions<'v>,
    output: OutputArtifactArg<'v>,
    srcs: UnpackDictEntries<&'v str, ValueAsInputArtifactLike<'v>>,
    copy: CopyMode,
    has_content_based_path: Option<bool>,
) -> bz_error::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
    let action = UnregisteredSymlinkedDirAction::new(copy, srcs)?;
    let unioned_associated_artifacts = action.unioned_associated_artifacts();

    let mut this = this.state()?;
    let (declaration, output_artifact) =
        this.get_or_declare_output(eval, output, OutputType::Directory, has_content_based_path)?;
    this.register_action(buck_indexset![output_artifact], action, None, None)?;

    Ok(declaration.into_declared_artifact(unioned_associated_artifacts))
}

fn copy_file_impl<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    this: &AnalysisActions<'v>,
    dest: OutputArtifactArg<'v>,
    src: ValueAsInputArtifactLike<'v>,
    copy: CopyMode,
    output_type: OutputType,
    has_content_based_path: Option<bool>,
) -> bz_error::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
    let src = src.0;

    let artifact = src.get_artifact_group()?;
    let associated_artifacts = src.get_associated_artifacts();
    let mut this = this.state()?;
    let (declaration, output_artifact) =
        this.get_or_declare_output(eval, dest, output_type, has_content_based_path)?;
    let action_signature = format!("{output_type:?}:{copy:?}:{artifact:?}");
    if !this.should_register_bazel_shareable_action(&output_artifact, action_signature)? {
        return Ok(declaration.into_declared_artifact(
            associated_artifacts
                .duped()
                .unwrap_or_else(AssociatedArtifacts::new),
        ));
    }

    this.register_action(
        buck_indexset![output_artifact],
        UnregisteredCopyAction::new(artifact, copy),
        None,
        None,
    )?;

    Ok(declaration.into_declared_artifact(
        associated_artifacts
            .duped()
            .unwrap_or_else(AssociatedArtifacts::new),
    ))
}

#[starlark_module]
pub(crate) fn analysis_actions_methods_copy(methods: &mut MethodsBuilder) {
    /// Copies the source `artifact` to the destination (which can be a string representing a
    /// filename or an output `artifact`) and returns the output `artifact`. The copy works for
    /// files or directories.
    fn copy_file<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = pos)] dest: OutputArtifactArg<'v>,
        #[starlark(require = pos)] src: ValueAsInputArtifactLike<'v>,
        #[starlark(require = named, default = NoneOr::None)] has_content_based_path: NoneOr<bool>,
        #[starlark(require = named, default = NoneOr::None)] executable_bit_override: NoneOr<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        // `copy_file` can copy either a file or a directory, even though its name has the word
        // `file` in it
        Ok(copy_file_impl(
            eval,
            this,
            dest,
            src,
            CopyMode::Copy {
                executable_bit_override: executable_bit_override.into_option(),
            },
            OutputType::FileOrDirectory,
            has_content_based_path.into_option(),
        )?)
    }

    /// Creates a symlink to the source `artifact` at the destination (which can be a string
    /// representing a filename or an output `artifact`) and returns the output `artifact`. The
    /// symlink works for files or directories.
    fn symlink_file<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = pos)] dest: OutputArtifactArg<'v>,
        #[starlark(require = pos)] src: ValueAsInputArtifactLike<'v>,
        #[starlark(require = named, default = NoneOr::None)] has_content_based_path: NoneOr<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        // `copy_file` can copy either a file or a directory, even though its name has the word
        // `file` in it
        Ok(copy_file_impl(
            eval,
            this,
            dest,
            src,
            CopyMode::Symlink {
                use_exec_root_for_source: false,
            },
            OutputType::FileOrDirectory,
            has_content_based_path.into_option(),
        )?)
    }

    /// Bazel spelling for creating a symlink output.
    fn symlink<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = named)] output: OutputArtifactArg<'v>,
        #[starlark(require = named, default = NoneOr::None)] target_file: NoneOr<
            ValueAsInputArtifactLike<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] target_path: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] target_type: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] is_executable: NoneOr<bool>,
        #[starlark(require = named, default = NoneOr::None)] progress_message: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] use_exec_root_for_source: NoneOr<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        let _ = progress_message;
        let target_file = target_file.into_option();
        let target_path = target_path.into_option();
        let target_type = target_type.into_option();
        let is_executable = is_executable.into_option().unwrap_or(false);
        let use_exec_root_for_source = use_exec_root_for_source.into_option().unwrap_or(false);

        if target_file.is_some() == target_path.is_some() {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "Exactly one of `target_file` or `target_path` is required"
            )
            .into());
        }

        if let Some(target_path) = target_path {
            if is_executable {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "`is_executable` cannot be True when using `target_path`"
                )
                .into());
            }
            if let Some(target_type) = target_type
                && target_type != "file"
                && target_type != "directory"
            {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "`target_type` must be one of `file` or `directory`"
                )
                .into());
            }

            let mut this = this.state()?;
            let (declaration, output_artifact) =
                this.get_or_declare_output(eval, output, OutputType::Symlink, None)?;
            let action_signature = format!(
                "{:?}:{:?}:{}",
                OutputType::Symlink,
                target_type,
                target_path
            );
            if !this.should_register_bazel_shareable_action(&output_artifact, action_signature)? {
                return Ok(declaration.into_declared_artifact(AssociatedArtifacts::new()));
            }
            this.register_action(
                buck_indexset![output_artifact],
                UnregisteredSymlinkAction::new(target_path.to_owned()),
                None,
                None,
            )?;

            return Ok(declaration.into_declared_artifact(AssociatedArtifacts::new()));
        }

        if target_type.is_some() {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "`target_type` cannot be used with `target_file`"
            )
            .into());
        }
        let target_file =
            target_file.expect("validated exactly one of target_file and target_path");

        Ok(copy_file_impl(
            eval,
            this,
            output,
            target_file,
            CopyMode::Symlink {
                use_exec_root_for_source,
            },
            OutputType::FileOrDirectory,
            None,
        )?)
    }

    /// Make a copy of a directory.
    fn copy_dir<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = pos)] dest: OutputArtifactArg<'v>,
        #[starlark(require = pos)] src: ValueAsInputArtifactLike<'v>,
        #[starlark(require = named, default = NoneOr::None)] has_content_based_path: NoneOr<bool>,
        #[starlark(require = named, default = NoneOr::None)] executable_bit_override: NoneOr<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        Ok(copy_file_impl(
            eval,
            this,
            dest,
            src,
            CopyMode::Copy {
                executable_bit_override: executable_bit_override.into_option(),
            },
            OutputType::Directory,
            has_content_based_path.into_option(),
        )?)
    }

    /// Returns an `artifact` that is a directory containing symlinks.
    /// The srcs must be a dictionary of path (as string, relative to the result directory) to bound `artifact`, which will be laid out in the directory.
    fn symlinked_dir<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = pos)] output: OutputArtifactArg<'v>,
        #[starlark(require = pos)] srcs: UnpackDictEntries<&'v str, ValueAsInputArtifactLike<'v>>,
        #[starlark(require = named, default = NoneOr::None)] has_content_based_path: NoneOr<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        Ok(create_dir_tree(
            eval,
            this,
            output,
            srcs,
            CopyMode::Symlink {
                use_exec_root_for_source: false,
            },
            has_content_based_path.into_option(),
        )?)
    }

    /// Returns an `artifact` which is a directory containing copied files.
    /// The srcs must be a dictionary of path (as string, relative to the result directory) to the bound `artifact`, which will be laid out in the directory.
    fn copied_dir<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = pos)] output: OutputArtifactArg<'v>,
        #[starlark(require = pos)] srcs: UnpackDictEntries<&'v str, ValueAsInputArtifactLike<'v>>,
        #[starlark(require = named, default = NoneOr::None)] has_content_based_path: NoneOr<bool>,
        #[starlark(require = named, default = NoneOr::None)] executable_bit_override: NoneOr<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        Ok(create_dir_tree(
            eval,
            this,
            output,
            srcs,
            CopyMode::Copy {
                executable_bit_override: executable_bit_override.into_option(),
            },
            has_content_based_path.into_option(),
        )?)
    }
}
