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
use std::slice;
use std::time::Instant;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_artifact::artifact::artifact_type::OutputArtifact;
use buck2_artifact::artifact::build_artifact::BuildArtifact;
use buck2_build_api::actions::Action;
use buck2_build_api::actions::ActionExecutionCtx;
use buck2_build_api::actions::UnregisteredAction;
use buck2_build_api::actions::execute::action_executor::ActionExecutionKind;
use buck2_build_api::actions::execute::action_executor::ActionExecutionMetadata;
use buck2_build_api::actions::execute::action_executor::ActionOutputs;
use buck2_build_api::actions::execute::error::ExecuteError;
use buck2_build_api::artifact_groups::ArtifactGroup;
use buck2_build_api::interpreter::rule_defs::artifact_tagging::ArtifactTag;
use buck2_build_api::interpreter::rule_defs::cmd_args::AbsCommandLineContext;
use buck2_build_api::interpreter::rule_defs::cmd_args::ArtifactPathMapper;
use buck2_build_api::interpreter::rule_defs::cmd_args::CommandLineArtifactVisitor;
use buck2_build_api::interpreter::rule_defs::cmd_args::DefaultCommandLineContext;
use buck2_build_api::interpreter::rule_defs::cmd_args::value_as::ValueAsCommandLineLike;
use buck2_build_signals::env::WaitingData;
use buck2_common::cas_digest::CasDigestData;
use buck2_common::file_ops::metadata::TrackedFileDigest;
use buck2_core::category::CategoryRef;
use buck2_core::content_hash::ContentBasedPathHash;
use buck2_error::internal_error;
use buck2_execute::artifact::artifact_dyn::ArtifactDyn;
use buck2_execute::artifact::fs::ExecutorFs;
use buck2_execute::execute::command_executor::ActionExecutionTimingData;
use buck2_execute::execute::request::CommandExecutionOutput;
use buck2_execute::execute::request::ExecutorPreference;
use buck2_execute::execute::request::LocalActionCacheKey;
use buck2_execute::materialize::materializer::WriteRequest;
use buck2_fs::fs_util::uncategorized as fs_util;
use buck2_hash::BuckIndexMap;
use buck2_hash::BuckIndexSet;
use buck2_hash::buck_indexmap;
use buck2_hash::buck_indexset;
use dupe::Dupe;
use pagable::Pagable;
use starlark::values::OwnedFrozenValue;
use starlark::values::UnpackValue;

use crate::actions::impls::run::DepFilesPlaceholderArtifactPathMapper;
use crate::actions::impls::run::action_cache_add_bool;
use crate::actions::impls::run::action_cache_add_str;
use crate::actions::impls::run::compose_local_action_cache_fingerprint;
use crate::actions::impls::run::finalize_action_cache_digest;
use crate::actions::impls::run::fingerprint_artifact_group_values;
use crate::actions::impls::run::fingerprint_command_execution_output;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Tier0)]
enum WriteActionValidationError {
    #[error("WriteAction received no outputs")]
    NoOutputs,
    #[error("WriteAction received more than one output")]
    TooManyOutputs,
    #[error("Expected command line value, got {0}")]
    ContentsNotCommandLineValue(String),
}

pub(crate) struct CommandLineContentBasedInputVisitor {
    pub(crate) content_based_inputs: BuckIndexSet<ArtifactGroup>,
}

impl CommandLineContentBasedInputVisitor {
    pub(crate) fn new() -> Self {
        Self {
            content_based_inputs: Default::default(),
        }
    }
}

impl<'v> CommandLineArtifactVisitor<'v> for CommandLineContentBasedInputVisitor {
    fn visit_input(&mut self, input: ArtifactGroup, _tags: Vec<&ArtifactTag>) {
        if input.path_resolution_may_require_artifact_value() {
            self.content_based_inputs.insert(input);
        }
    }

    fn visit_declared_output(&mut self, _artifact: OutputArtifact<'v>, _tags: Vec<&ArtifactTag>) {}

    fn visit_frozen_output(&mut self, _artifact: Artifact, _tags: Vec<&ArtifactTag>) {}

    fn visit_declared_artifact(
        &mut self,
        declared_artifact: buck2_artifact::artifact::artifact_type::DeclaredArtifact<'v>,
        tags: Vec<&ArtifactTag>,
    ) -> buck2_error::Result<()> {
        if declared_artifact.has_content_based_path() {
            let artifact = declared_artifact.ensure_bound()?.into_artifact();
            self.visit_input(ArtifactGroup::Artifact(artifact), tags);
        }

        Ok(())
    }

    fn skip_hidden(&self) -> bool {
        true
    }
}

#[derive(Allocative, Debug, Pagable)]
pub(crate) struct UnregisteredWriteAction {
    pub(crate) is_executable: bool,
    pub(crate) absolute: bool,
    pub(crate) macro_files: Option<BuckIndexSet<Artifact>>,
    pub(crate) use_dep_files_placeholder_for_content_based_paths: bool,
}

#[derive(Allocative)]
pub(crate) struct UnregisteredTemplateExpansionAction {
    template: ArtifactGroup,
    substitutions: Vec<(String, String)>,
    is_executable: bool,
}

impl UnregisteredTemplateExpansionAction {
    pub(crate) fn new(
        template: ArtifactGroup,
        substitutions: Vec<(String, String)>,
        is_executable: bool,
    ) -> Self {
        Self {
            template,
            substitutions,
            is_executable,
        }
    }
}

impl TemplateExpansionAction {
    fn local_action_cache_output(&self) -> CommandExecutionOutput {
        CommandExecutionOutput::BuildArtifact {
            path: self.output.get_path().dupe(),
            output_type: self.output.output_type(),
            produced_path: None,
        }
    }

    fn local_action_cache_key(
        &self,
        ctx: &dyn ActionExecutionCtx,
        input_values: &buck2_build_api::artifact_groups::ArtifactGroupValues,
        output: &CommandExecutionOutput,
    ) -> buck2_error::Result<LocalActionCacheKey> {
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
        action_cache_add_str(&mut action_key, "expand_template");
        action_cache_add_bool(&mut action_key, self.is_executable);
        action_cache_add_str(&mut action_key, "substitutions");
        for (key, value) in &self.substitutions {
            action_cache_add_str(&mut action_key, key);
            action_cache_add_str(&mut action_key, value);
        }
        fingerprint_command_execution_output(&mut action_key, ctx.fs(), output)?;

        let mut input_metadata = CasDigestData::digester(cas_digest_config);
        action_cache_add_str(
            &mut input_metadata,
            "buck2-local-action-cache-simple-input-metadata-v1",
        );
        fingerprint_artifact_group_values(&mut input_metadata, ctx.fs(), input_values)?;

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

impl UnregisteredAction for UnregisteredWriteAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        starlark_data: Option<OwnedFrozenValue>,
        _error_handler: Option<OwnedFrozenValue>,
    ) -> buck2_error::Result<Box<dyn Action>> {
        let contents = starlark_data.expect("module data to be present");

        let write_action = WriteAction::new(contents, outputs, *self)?;
        Ok(Box::new(write_action))
    }
}

impl UnregisteredAction for UnregisteredTemplateExpansionAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        _starlark_data: Option<OwnedFrozenValue>,
        _error_handler: Option<OwnedFrozenValue>,
    ) -> buck2_error::Result<Box<dyn Action>> {
        let mut outputs = outputs.into_iter();
        let output = match (outputs.next(), outputs.next()) {
            (Some(o), None) => o,
            (None, ..) => return Err(WriteActionValidationError::NoOutputs.into()),
            (Some(..), Some(..)) => return Err(WriteActionValidationError::TooManyOutputs.into()),
        };
        Ok(Box::new(TemplateExpansionAction {
            template: self.template,
            substitutions: self.substitutions,
            is_executable: self.is_executable,
            output,
        }))
    }
}

#[derive(Debug, Allocative, Pagable)]
struct WriteAction {
    contents: OwnedFrozenValue, // StarlarkCmdArgs
    output: BuildArtifact,
    inner: UnregisteredWriteAction,
}

#[derive(Debug, Allocative, Pagable)]
struct TemplateExpansionAction {
    template: ArtifactGroup,
    substitutions: Vec<(String, String)>,
    is_executable: bool,
    output: BuildArtifact,
}

impl WriteAction {
    fn new(
        contents: OwnedFrozenValue,
        outputs: BuckIndexSet<BuildArtifact>,
        inner: UnregisteredWriteAction,
    ) -> buck2_error::Result<Self> {
        let mut outputs = outputs.into_iter();

        let output = match (outputs.next(), outputs.next()) {
            (Some(o), None) => o,
            (None, ..) => return Err(WriteActionValidationError::NoOutputs.into()),
            (Some(..), Some(..)) => return Err(WriteActionValidationError::TooManyOutputs.into()),
        };

        if ValueAsCommandLineLike::unpack_value(contents.value())?.is_none() {
            return Err(WriteActionValidationError::ContentsNotCommandLineValue(
                contents.value().to_repr(),
            )
            .into());
        }

        Ok(WriteAction {
            contents,
            output,
            inner,
        })
    }

    fn get_contents(
        &self,
        fs: &ExecutorFs,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<String> {
        let mut cli = Vec::<String>::new();

        let macro_files = self.inner.macro_files.as_ref().map(|macro_files| {
            macro_files
                .iter()
                .map(|a| (a, artifact_path_mapping.get(a)))
                .collect()
        });

        let mut ctx = if let Some(ref macro_files) = macro_files {
            DefaultCommandLineContext::new_with_write_to_file_macros_support(fs, macro_files)
        } else {
            DefaultCommandLineContext::new(fs)
        };

        let mut abs;

        let ctx = if self.inner.absolute {
            abs = AbsCommandLineContext::wrap(ctx);
            &mut abs as _
        } else {
            &mut ctx as _
        };

        ValueAsCommandLineLike::unpack_value_err(self.contents.value())
            .unwrap()
            .0
            .add_to_command_line(&mut cli, ctx, artifact_path_mapping)?;

        Ok(cli.join("\n"))
    }
}

#[async_trait]
impl Action for WriteAction {
    fn kind(&self) -> buck2_data::ActionKind {
        buck2_data::ActionKind::Write
    }

    fn inputs(&self) -> buck2_error::Result<Cow<'_, [ArtifactGroup]>> {
        if self.inner.use_dep_files_placeholder_for_content_based_paths {
            return Ok(Cow::Borrowed(&[]));
        }

        let mut visitor = CommandLineContentBasedInputVisitor::new();
        ValueAsCommandLineLike::unpack_value_err(self.contents.value())?
            .0
            .visit_artifacts(&mut visitor)?;
        let mut content_based_inputs = visitor.content_based_inputs;
        if let Some(macro_files) = &self.inner.macro_files {
            for artifact in macro_files {
                if artifact.path_resolution_requires_artifact_value() {
                    content_based_inputs.insert(ArtifactGroup::Artifact(artifact.dupe()));
                }
            }
        }
        Ok(Cow::Owned(content_based_inputs.into_iter().collect()))
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(slice::from_ref(&self.output))
    }

    fn first_output(&self) -> &BuildArtifact {
        &self.output
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new("write")
    }

    fn identifier(&self) -> Option<&str> {
        Some(self.output.get_path().path().as_str())
    }

    fn aquery_attributes(
        &self,
        fs: &ExecutorFs,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> BuckIndexMap<String, String> {
        // TODO(cjhopman): We should change this api to support returning a Result.
        buck_indexmap! {
            "contents".to_owned() => match self.get_contents(fs, artifact_path_mapping) {
                Ok(v) => v,
                Err(e) => format!("ERROR: constructing contents ({e})")
            },
            "absolute".to_owned() => self.inner.absolute.to_string(),
        }
    }

    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        let fs = ctx.fs();

        let mut execution_start = None;

        let value = ctx
            .materializer()
            .declare_write(Box::new(|| {
                execution_start = Some(Instant::now());
                let content = if self.inner.use_dep_files_placeholder_for_content_based_paths {
                    self.get_contents(
                        &ctx.executor_fs(),
                        &DepFilesPlaceholderArtifactPathMapper {},
                    )?
                } else {
                    self.get_contents(&ctx.executor_fs(), &ctx.artifact_path_mapping(None))?
                }
                .into_bytes();
                let path = fs.resolve_build(
                    self.output.get_path(),
                    if self.output.get_path().is_content_based_path() {
                        let digest = TrackedFileDigest::from_content(
                            &content,
                            ctx.digest_config().cas_digest_config(),
                        );
                        Some(ContentBasedPathHash::new(digest.raw_digest().as_bytes())?)
                    } else {
                        None
                    }
                    .as_ref(),
                )?;
                let configuration_path = ctx
                    .materializer()
                    .maybe_eager_configuration_path(fs, self.output.get_path())?;
                Ok(vec![WriteRequest {
                    path,
                    content,
                    is_executable: self.inner.is_executable,
                    configuration_path,
                }])
            }))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| internal_error!("Write did not execute"))?;

        let wall_time = Instant::now()
            - execution_start
                .ok_or_else(|| internal_error!("Action did not set execution_start"))?;

        Ok((
            ActionOutputs::new(buck_indexmap![self.output.get_path().dupe() => value]),
            ActionExecutionMetadata {
                execution_kind: ActionExecutionKind::Simple,
                timing: ActionExecutionTimingData { wall_time },
                input_files_bytes: None,
                waiting_data,
            },
        ))
    }
}

#[async_trait]
impl Action for TemplateExpansionAction {
    fn kind(&self) -> buck2_data::ActionKind {
        buck2_data::ActionKind::Write
    }

    fn inputs(&self) -> buck2_error::Result<Cow<'_, [ArtifactGroup]>> {
        Ok(Cow::Borrowed(slice::from_ref(&self.template)))
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(slice::from_ref(&self.output))
    }

    fn first_output(&self) -> &BuildArtifact {
        &self.output
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new("expand_template")
    }

    fn identifier(&self) -> Option<&str> {
        Some(self.output.get_path().path().as_str())
    }

    fn aquery_attributes(
        &self,
        _fs: &ExecutorFs,
        _artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> BuckIndexMap<String, String> {
        buck_indexmap! {
            "substitutions".to_owned() => self.substitutions
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(","),
            "is_executable".to_owned() => self.is_executable.to_string(),
        }
    }

    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        let local_action_cache_output = self.local_action_cache_output();
        let local_action_cache_outputs = buck_indexset![local_action_cache_output.clone()];
        let (local_action_cache_key, input, input_value) = {
            let values = ctx.artifact_values(&self.template);
            let local_action_cache_key =
                self.local_action_cache_key(ctx, values, &local_action_cache_output)?;
            let mut iter = values.iter();
            let Some((input, input_value)) = iter.next() else {
                return Err(internal_error!("Template did not dereference to an artifact").into());
            };
            if iter.next().is_some() {
                return Err(
                    internal_error!("Template dereferenced to more than one artifact").into(),
                );
            }
            (local_action_cache_key, input.dupe(), input_value.dupe())
        };

        let manager = ctx.command_execution_manager(waiting_data.clone());
        match ctx
            .unprepared_action_cache(
                manager,
                &local_action_cache_key,
                &local_action_cache_outputs,
            )
            .await
        {
            ControlFlow::Break(result) => {
                return ctx.unpack_command_execution_result(
                    ExecutorPreference::LocalRequired,
                    result,
                    false,
                    false,
                    None,
                    buck2_data::IncrementalKind::NonIncremental,
                );
            }
            ControlFlow::Continue(_) => {}
        }

        let template_path = {
            let artifact_fs = ctx.fs();
            input.resolve_path(
                artifact_fs,
                if input.path_resolution_requires_artifact_value() {
                    Some(input_value.content_based_path_hash())
                } else {
                    None
                }
                .as_ref(),
            )?
        };
        ctx.materializer()
            .ensure_materialized(vec![template_path.clone()])
            .await?;
        let template_path = ctx.fs().fs().resolve(template_path);
        let fs = ctx.fs();

        let mut execution_start = None;
        let value = ctx
            .materializer()
            .declare_write(Box::new(|| {
                execution_start = Some(Instant::now());
                let mut content = fs_util::read_to_string(&template_path)?;
                for (key, value) in &self.substitutions {
                    content = content.replace(key, value);
                }
                let content = content.into_bytes();
                let path = fs.resolve_build(
                    self.output.get_path(),
                    if self.output.get_path().is_content_based_path() {
                        let digest = TrackedFileDigest::from_content(
                            &content,
                            ctx.digest_config().cas_digest_config(),
                        );
                        Some(ContentBasedPathHash::new(digest.raw_digest().as_bytes())?)
                    } else {
                        None
                    }
                    .as_ref(),
                )?;
                let configuration_path = ctx
                    .materializer()
                    .maybe_eager_configuration_path(fs, self.output.get_path())?;
                Ok(vec![WriteRequest {
                    path,
                    content,
                    is_executable: self.is_executable,
                    configuration_path,
                }])
            }))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| internal_error!("Template expansion did not execute"))?;

        ctx.insert_unprepared_action_cache_metadata(
            &local_action_cache_key,
            &buck_indexmap![local_action_cache_output => value.dupe()],
        )?;

        let wall_time = Instant::now()
            - execution_start
                .ok_or_else(|| internal_error!("Action did not set execution_start"))?;

        Ok((
            ActionOutputs::new(buck_indexmap![self.output.get_path().dupe() => value]),
            ActionExecutionMetadata {
                execution_kind: ActionExecutionKind::Simple,
                timing: ActionExecutionTimingData { wall_time },
                input_files_bytes: None,
                waiting_data,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    // TODO: This needs proper tests, but right now it's kind of a pain to get the
    //       action framework up and running to test actions
    #[test]
    fn writes_file() {}
}
