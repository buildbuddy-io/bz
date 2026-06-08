/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::borrow::Cow;
use std::ops::ControlFlow;

use allocative::Allocative;
use async_trait::async_trait;
use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_build_api::actions::Action;
use bz_build_api::actions::ActionExecutionCtx;
use bz_build_api::actions::UnregisteredAction;
use bz_build_api::actions::box_slice_set::BoxSliceSet;
use bz_build_api::actions::execute::action_executor::ActionExecutionKind;
use bz_build_api::actions::execute::action_executor::ActionExecutionMetadata;
use bz_build_api::actions::execute::action_executor::ActionOutputs;
use bz_build_api::actions::execute::error::ExecuteError;
use bz_build_api::artifact_groups::ArtifactGroup;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::bazel_artifact_path;
use bz_build_signals::env::WaitingData;
use bz_common::cas_digest::CasDigestData;
use bz_core::category::CategoryRef;
use bz_core::content_hash::ContentBasedPathHash;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_execute::artifact::artifact_dyn::ArtifactDyn;
use bz_execute::artifact_utils::ArtifactValueBuilder;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::directory::ActionDirectoryEntry;
use bz_execute::directory::new_symlink;
use bz_execute::execute::command_executor::ActionExecutionTimingData;
use bz_execute::execute::request::CommandExecutionOutput;
use bz_execute::execute::request::ExecutorPreference;
use bz_execute::execute::request::LocalActionCacheKey;
use bz_execute::materialize::materializer::CopiedArtifact;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_hash::BuckIndexSet;
use bz_hash::buck_indexmap;
use bz_hash::buck_indexset;
use dupe::Dupe;
use gazebo::prelude::*;
use pagable::Pagable;
use starlark::values::OwnedFrozenValue;

use crate::actions::impls::run::action_cache_add_bool;
use crate::actions::impls::run::action_cache_add_bytes;
use crate::actions::impls::run::action_cache_add_str;
use crate::actions::impls::run::compose_local_action_cache_fingerprint;
use crate::actions::impls::run::finalize_action_cache_digest;
use crate::actions::impls::run::fingerprint_command_execution_output;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum CopyActionValidationError {
    #[error("Exactly one output file must be specified for a copy action, got {0}")]
    WrongNumberOfOutputs(usize),
    #[error("Only artifact inputs are supported in copy actions, got {0}")]
    UnsupportedInput(ArtifactGroup),
}

#[derive(Debug, Allocative, Pagable)]
pub(crate) enum CopyMode {
    Copy {
        // Override the destination executable bit to +x (true) or -x (false)
        executable_bit_override: Option<bool>,
    },
    Symlink {
        use_exec_root_for_source: bool,
    },
}

fn symlink_source_path(
    artifact_fs: &ArtifactFs,
    input: &Artifact,
    src: ProjectRelativePathBuf,
    use_exec_root_for_source: bool,
) -> bz_error::Result<ProjectRelativePathBuf> {
    if !use_exec_root_for_source || !input.is_source() {
        return Ok(src);
    }

    let bazel_path = ForwardRelativePathBuf::try_from(bazel_artifact_path(input.get_path()))
        .buck_error_context("Invalid Bazel symlink source path")?;
    Ok(artifact_fs
        .buck_out_path_resolver()
        .root()
        .join(ForwardRelativePathBuf::unchecked_new(
            "__bazel_execroot".to_owned(),
        ))
        .join(bazel_path))
}

#[derive(Allocative)]
pub(crate) struct UnregisteredCopyAction {
    src: ArtifactGroup,
    copy: CopyMode,
}

impl UnregisteredCopyAction {
    pub(crate) fn new(src: ArtifactGroup, copy: CopyMode) -> Self {
        Self { src, copy }
    }
}

impl UnregisteredAction for UnregisteredCopyAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        _starlark_data: Option<OwnedFrozenValue>,
        _error_handler: Option<OwnedFrozenValue>,
    ) -> bz_error::Result<Box<dyn Action>> {
        Ok(Box::new(CopyAction::new(self.copy, self.src, outputs)?))
    }
}

#[derive(Allocative)]
pub(crate) struct UnregisteredSymlinkAction {
    target_path: String,
}

impl UnregisteredSymlinkAction {
    pub(crate) fn new(target_path: String) -> Self {
        Self { target_path }
    }
}

impl UnregisteredAction for UnregisteredSymlinkAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        _starlark_data: Option<OwnedFrozenValue>,
        _error_handler: Option<OwnedFrozenValue>,
    ) -> bz_error::Result<Box<dyn Action>> {
        Ok(Box::new(SymlinkAction::new(self.target_path, outputs)?))
    }
}

#[derive(Debug, Allocative, Pagable)]
struct CopyAction {
    copy: CopyMode,
    inputs: BoxSliceSet<ArtifactGroup>,
    outputs: BoxSliceSet<BuildArtifact>,
}

impl CopyAction {
    fn new(
        copy: CopyMode,
        src: ArtifactGroup,
        outputs: BuckIndexSet<BuildArtifact>,
    ) -> bz_error::Result<Self> {
        // TODO: Exclude other variants once they become available here. For now, this is a noop.
        match src {
            ArtifactGroup::Artifact(..) | ArtifactGroup::Promise(..) => {}
            ArtifactGroup::TransitiveSetProjection(..) => {
                return Err(CopyActionValidationError::UnsupportedInput(src.dupe()).into());
            }
        };

        if outputs.len() != 1 {
            Err(CopyActionValidationError::WrongNumberOfOutputs(outputs.len()).into())
        } else {
            Ok(CopyAction {
                copy,
                inputs: BoxSliceSet::from(buck_indexset![src]),
                outputs: BoxSliceSet::from(outputs),
            })
        }
    }

    fn input(&self) -> &ArtifactGroup {
        self.inputs
            .iter()
            .next()
            .expect("a single input by construction")
    }

    fn output(&self) -> &BuildArtifact {
        self.outputs
            .iter()
            .next()
            .expect("a single artifact by construction")
    }

    fn local_action_cache_output(&self) -> CommandExecutionOutput {
        CommandExecutionOutput::BuildArtifact {
            path: self.output().get_path().dupe(),
            output_type: self.output().output_type(),
            produced_path: None,
        }
    }

    fn local_action_cache_key(
        &self,
        ctx: &dyn ActionExecutionCtx,
        output: &CommandExecutionOutput,
    ) -> bz_error::Result<LocalActionCacheKey> {
        let key = output
            .as_ref()
            .resolve(ctx.fs(), Some(&ContentBasedPathHash::for_output_artifact()))?
            .into_path()
            .to_string();

        let cas_digest_config = ctx.digest_config().cas_digest_config();
        let mut action_key = CasDigestData::digester(cas_digest_config);
        action_cache_add_str(
            &mut action_key,
            "buck2-local-action-cache-simple-action-key-v1",
        );
        action_cache_add_str(&mut action_key, &ctx.fs().fs().root().to_string());
        action_cache_add_str(&mut action_key, "copy");
        match self.copy {
            CopyMode::Copy {
                executable_bit_override,
            } => {
                action_cache_add_str(&mut action_key, "copy");
                match executable_bit_override {
                    Some(value) => {
                        action_cache_add_bool(&mut action_key, true);
                        action_cache_add_bool(&mut action_key, value);
                    }
                    None => action_cache_add_bool(&mut action_key, false),
                }
            }
            CopyMode::Symlink {
                use_exec_root_for_source,
            } => {
                action_cache_add_str(&mut action_key, "symlink");
                action_cache_add_bool(&mut action_key, use_exec_root_for_source);
            }
        }
        fingerprint_command_execution_output(&mut action_key, ctx.fs(), output)?;

        let mut input_metadata = CasDigestData::digester(cas_digest_config);
        action_cache_add_str(
            &mut input_metadata,
            "buck2-local-action-cache-simple-input-metadata-v2",
        );
        action_cache_add_str(&mut input_metadata, "artifact_input_set");
        action_cache_add_bytes(
            &mut input_metadata,
            ctx.local_action_cache_input_set_digest(),
        );

        let action_key_digest = finalize_action_cache_digest(action_key);
        let input_metadata_digest = finalize_action_cache_digest(input_metadata);
        let fingerprint = compose_local_action_cache_fingerprint(
            cas_digest_config,
            &action_key_digest,
            &input_metadata_digest,
        );

        Ok(LocalActionCacheKey {
            key,
            action_key_digest,
            input_metadata_digest,
            fingerprint,
        })
    }

    async fn declare_copy_value(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        input: &Artifact,
        src_value: &ArtifactValue,
        value: ArtifactValue,
    ) -> Result<(), ExecuteError> {
        let artifact_fs = ctx.fs();
        let src = input.resolve_path(
            artifact_fs,
            if input.path_resolution_requires_artifact_value() {
                Some(src_value.content_based_path_hash())
            } else {
                None
            }
            .as_ref(),
        )?;
        let copied_src = match self.copy {
            CopyMode::Copy { .. } => src,
            CopyMode::Symlink {
                use_exec_root_for_source,
            } => symlink_source_path(artifact_fs, input, src, use_exec_root_for_source)?,
        };

        let tmp_dest = artifact_fs.resolve_build(
            self.output().get_path(),
            Some(&ContentBasedPathHash::for_output_artifact()),
        )?;
        let dest = if self.output().get_path().is_content_based_path() {
            artifact_fs.resolve_build(
                self.output().get_path(),
                Some(&value.content_based_path_hash()),
            )?
        } else {
            tmp_dest
        };
        let configuration_path = ctx
            .materializer()
            .maybe_eager_configuration_path(ctx.fs(), self.output().get_path())?;

        ctx.materializer()
            .declare_copy(
                dest.clone(),
                value.dupe(),
                // FIXME(JakobDegen): This is wrong in cases where the input artifact is a source
                // directory with ignored paths, as the materializer will incorrectly assume that
                // the source directory matches the artifact value when it doesn't.
                vec![CopiedArtifact::new(
                    copied_src,
                    dest,
                    value.entry().dupe().map_dir(|d| d.as_immutable()),
                    match self.copy {
                        CopyMode::Copy {
                            executable_bit_override,
                        } => executable_bit_override,
                        CopyMode::Symlink { .. } => None,
                    },
                )],
                configuration_path,
            )
            .await?;

        Ok(())
    }
}

#[derive(Debug, Allocative, Pagable)]
struct SymlinkAction {
    target_path: String,
    outputs: BoxSliceSet<BuildArtifact>,
}

impl SymlinkAction {
    fn new(target_path: String, outputs: BuckIndexSet<BuildArtifact>) -> bz_error::Result<Self> {
        if outputs.len() != 1 {
            Err(CopyActionValidationError::WrongNumberOfOutputs(outputs.len()).into())
        } else {
            Ok(SymlinkAction {
                target_path,
                outputs: BoxSliceSet::from(outputs),
            })
        }
    }

    fn output(&self) -> &BuildArtifact {
        self.outputs
            .iter()
            .next()
            .expect("a single artifact by construction")
    }

    async fn declare_symlink_value(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        value: ArtifactValue,
    ) -> Result<(), ExecuteError> {
        let artifact_fs = ctx.fs();
        let tmp_dest = artifact_fs.resolve_build(
            self.output().get_path(),
            Some(&ContentBasedPathHash::for_output_artifact()),
        )?;

        let dest = if self.output().get_path().is_content_based_path() {
            artifact_fs.resolve_build(
                self.output().get_path(),
                Some(&value.content_based_path_hash()),
            )?
        } else {
            tmp_dest
        };

        let configuration_path = ctx
            .materializer()
            .maybe_eager_configuration_path(ctx.fs(), self.output().get_path())?;

        ctx.materializer()
            .declare_copy(dest, value, Vec::new(), configuration_path)
            .await?;

        Ok(())
    }

    fn local_action_cache_output(&self) -> CommandExecutionOutput {
        CommandExecutionOutput::BuildArtifact {
            path: self.output().get_path().dupe(),
            output_type: self.output().output_type(),
            produced_path: None,
        }
    }

    fn local_action_cache_key(
        &self,
        ctx: &dyn ActionExecutionCtx,
        output: &CommandExecutionOutput,
    ) -> bz_error::Result<LocalActionCacheKey> {
        let key = output
            .as_ref()
            .resolve(ctx.fs(), Some(&ContentBasedPathHash::for_output_artifact()))?
            .into_path()
            .to_string();

        let cas_digest_config = ctx.digest_config().cas_digest_config();
        let mut action_key = CasDigestData::digester(cas_digest_config);
        action_cache_add_str(
            &mut action_key,
            "buck2-local-action-cache-simple-action-key-v1",
        );
        action_cache_add_str(&mut action_key, &ctx.fs().fs().root().to_string());
        action_cache_add_str(&mut action_key, "symlink");
        action_cache_add_str(&mut action_key, &self.target_path);
        fingerprint_command_execution_output(&mut action_key, ctx.fs(), output)?;

        let mut input_metadata = CasDigestData::digester(cas_digest_config);
        action_cache_add_str(
            &mut input_metadata,
            "buck2-local-action-cache-simple-input-metadata-v2",
        );
        action_cache_add_str(&mut input_metadata, "artifact_input_set");
        action_cache_add_bytes(
            &mut input_metadata,
            ctx.local_action_cache_input_set_digest(),
        );

        let action_key_digest = finalize_action_cache_digest(action_key);
        let input_metadata_digest = finalize_action_cache_digest(input_metadata);
        let fingerprint = compose_local_action_cache_fingerprint(
            cas_digest_config,
            &action_key_digest,
            &input_metadata_digest,
        );

        Ok(LocalActionCacheKey {
            key,
            action_key_digest,
            input_metadata_digest,
            fingerprint,
        })
    }
}

#[async_trait]
impl Action for CopyAction {
    fn kind(&self) -> bz_data::ActionKind {
        bz_data::ActionKind::Copy
    }

    fn inputs(&self) -> bz_error::Result<Cow<'_, [ArtifactGroup]>> {
        Ok(Cow::Borrowed(self.inputs.as_slice()))
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(self.outputs.as_slice())
    }

    fn first_output(&self) -> &BuildArtifact {
        self.output()
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new("copy")
    }

    fn identifier(&self) -> Option<&str> {
        Some(self.output().get_path().path().as_str())
    }

    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        let local_action_cache_output = self.local_action_cache_output();
        let local_action_cache_outputs = buck_indexset![local_action_cache_output.clone()];
        let (local_action_cache_key, input, src_value) = {
            let input_values = ctx.artifact_values(self.input());
            let local_action_cache_key =
                self.local_action_cache_key(ctx, &local_action_cache_output)?;
            let (input, src_value) = input_values.iter().into_singleton().ok_or_else(|| {
                internal_error!("Input did not dereference to exactly one artifact")
            })?;
            (local_action_cache_key, input.dupe(), src_value.dupe())
        };

        let manager = ctx.command_execution_manager(waiting_data.clone());
        match ctx
            .unprepared_action_cache_declared_by_action(
                manager,
                &local_action_cache_key,
                &local_action_cache_outputs,
            )
            .await
        {
            ControlFlow::Break(result) => {
                let (outputs, metadata) = ctx.unpack_command_execution_result(
                    ExecutorPreference::LocalRequired,
                    result,
                    false,
                    false,
                    None,
                    bz_data::IncrementalKind::NonIncremental,
                )?;
                let value = outputs
                    .get(self.output().get_path())
                    .ok_or_else(|| internal_error!("Copy action cache hit did not produce output"))?
                    .dupe();
                self.declare_copy_value(ctx, &input, &src_value, value)
                    .await?;
                return Ok((outputs, metadata));
            }
            ControlFlow::Continue(_) => {}
        }

        let artifact_fs = ctx.fs();
        let src = input.resolve_path(
            artifact_fs,
            if input.path_resolution_requires_artifact_value() {
                Some(src_value.content_based_path_hash())
            } else {
                None
            }
            .as_ref(),
        )?;
        let tmp_dest = artifact_fs.resolve_build(
            self.output().get_path(),
            Some(&ContentBasedPathHash::for_output_artifact()),
        )?;

        let value = {
            let fs = artifact_fs.fs();
            let mut builder = ArtifactValueBuilder::new(fs, ctx.digest_config());
            match self.copy {
                CopyMode::Copy {
                    executable_bit_override,
                } => {
                    builder.add_copied(
                        &src_value,
                        src.as_ref(),
                        tmp_dest.as_ref(),
                        executable_bit_override,
                    )?;
                }
                CopyMode::Symlink {
                    use_exec_root_for_source,
                } => {
                    let copied_src = symlink_source_path(
                        artifact_fs,
                        &input,
                        src.clone(),
                        use_exec_root_for_source,
                    )?;
                    builder.add_symlinked(&src_value, copied_src.clone(), tmp_dest.as_ref())?;
                }
            }

            builder.build(tmp_dest.as_ref())?
        };

        self.declare_copy_value(ctx, &input, &src_value, value.dupe())
            .await?;

        ctx.insert_unprepared_action_cache_metadata(
            &local_action_cache_key,
            &buck_indexmap![local_action_cache_output => value.dupe()],
            None,
        )?;

        Ok((
            ActionOutputs::from_single(self.output().get_path().dupe(), value),
            ActionExecutionMetadata {
                execution_kind: ActionExecutionKind::Simple,
                timing: ActionExecutionTimingData::default(),
                input_files_bytes: None,
                waiting_data,
                remote_cache_origin: None,
            },
        ))
    }
}

#[async_trait]
impl Action for SymlinkAction {
    fn kind(&self) -> bz_data::ActionKind {
        bz_data::ActionKind::Copy
    }

    fn inputs(&self) -> bz_error::Result<Cow<'_, [ArtifactGroup]>> {
        Ok(Cow::Borrowed(&[]))
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(self.outputs.as_slice())
    }

    fn first_output(&self) -> &BuildArtifact {
        self.output()
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new("symlink")
    }

    fn identifier(&self) -> Option<&str> {
        Some(self.output().get_path().path().as_str())
    }

    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        let local_action_cache_output = self.local_action_cache_output();
        let local_action_cache_outputs = buck_indexset![local_action_cache_output.clone()];
        let local_action_cache_key =
            self.local_action_cache_key(ctx, &local_action_cache_output)?;
        let manager = ctx.command_execution_manager(waiting_data.clone());
        match ctx
            .unprepared_action_cache_declared_by_action(
                manager,
                &local_action_cache_key,
                &local_action_cache_outputs,
            )
            .await
        {
            ControlFlow::Break(result) => {
                let (outputs, metadata) = ctx.unpack_command_execution_result(
                    ExecutorPreference::LocalRequired,
                    result,
                    false,
                    false,
                    None,
                    bz_data::IncrementalKind::NonIncremental,
                )?;
                let value = outputs
                    .get(self.output().get_path())
                    .ok_or_else(|| {
                        internal_error!("Symlink action cache hit did not produce output")
                    })?
                    .dupe();
                self.declare_symlink_value(ctx, value).await?;
                return Ok((outputs, metadata));
            }
            ControlFlow::Continue(_) => {}
        }

        let value = ArtifactValue::new(
            ActionDirectoryEntry::Leaf(new_symlink(&self.target_path)?),
            None,
        );

        self.declare_symlink_value(ctx, value.dupe()).await?;

        ctx.insert_unprepared_action_cache_metadata(
            &local_action_cache_key,
            &buck_indexmap![local_action_cache_output => value.dupe()],
            None,
        )?;

        Ok((
            ActionOutputs::from_single(self.output().get_path().dupe(), value),
            ActionExecutionMetadata {
                execution_kind: ActionExecutionKind::Simple,
                timing: ActionExecutionTimingData::default(),
                input_files_bytes: None,
                waiting_data,
                remote_cache_origin: None,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    // TODO: This needs proper tests, but right now it's kind of a pain to get the
    //       action framework up and running to test actions
    #[test]
    fn copies_file() {}
}
