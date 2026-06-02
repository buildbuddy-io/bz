use std::borrow::Cow;

use allocative::Allocative;
use async_trait::async_trait;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_build_signals::env::WaitingData;
use bz_core::category::CategoryRef;
use bz_core::content_hash::ContentBasedPathHash;
use bz_execute::artifact::artifact_dyn::ArtifactDyn;
use bz_execute::artifact_utils::ArtifactValueBuilder;
use bz_execute::execute::command_executor::ActionExecutionTimingData;
use bz_execute::materialize::materializer::CopiedArtifact;
use bz_hash::BuckIndexSet;
use dupe::Dupe;
use gazebo::prelude::*;
use pagable::Pagable;
use starlark::values::OwnedFrozenValue;

use crate::actions::Action;
use crate::actions::ActionExecutionCtx;
use crate::actions::UnregisteredAction;
use crate::actions::execute::action_executor::ActionExecutionKind;
use crate::actions::execute::action_executor::ActionExecutionMetadata;
use crate::actions::execute::action_executor::ActionOutputs;
use crate::actions::execute::error::ExecuteError;
use crate::artifact_groups::ArtifactGroup;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Tier0)]
enum SolibSymlinkActionError {
    #[error("SolibSymlink action received no outputs")]
    NoOutputs,
    #[error("SolibSymlink action received more than one output")]
    TooManyOutputs,
    #[error("SolibSymlink action input did not dereference to exactly one artifact")]
    WrongNumberOfInputs,
}

#[derive(Allocative)]
pub struct UnregisteredSolibSymlinkAction {
    src: ArtifactGroup,
}

impl UnregisteredSolibSymlinkAction {
    pub fn new(src: ArtifactGroup) -> Self {
        Self { src }
    }
}

impl UnregisteredAction for UnregisteredSolibSymlinkAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        starlark_data: Option<OwnedFrozenValue>,
        error_handler: Option<OwnedFrozenValue>,
    ) -> bz_error::Result<Box<dyn Action>> {
        let _unused = (starlark_data, error_handler);
        let mut outputs = outputs.into_iter();
        let output = outputs.next().ok_or(SolibSymlinkActionError::NoOutputs)?;
        if outputs.next().is_some() {
            return Err(SolibSymlinkActionError::TooManyOutputs.into());
        }
        Ok(Box::new(SolibSymlinkAction {
            src: self.src,
            output,
        }))
    }
}

#[derive(Debug, Allocative, Pagable)]
struct SolibSymlinkAction {
    src: ArtifactGroup,
    output: BuildArtifact,
}

#[async_trait]
impl Action for SolibSymlinkAction {
    fn kind(&self) -> bz_data::ActionKind {
        bz_data::ActionKind::Copy
    }

    fn inputs(&self) -> bz_error::Result<Cow<'_, [ArtifactGroup]>> {
        Ok(Cow::Borrowed(std::slice::from_ref(&self.src)))
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(std::slice::from_ref(&self.output))
    }

    fn first_output(&self) -> &BuildArtifact {
        &self.output
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new("SolibSymlink")
    }

    fn identifier(&self) -> Option<&str> {
        Some(self.output.get_path().path().as_str())
    }

    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        let input_values = ctx.artifact_values(&self.src);
        let (input, src_value) = input_values.iter().into_singleton().ok_or_else(|| {
            bz_error::Error::from(SolibSymlinkActionError::WrongNumberOfInputs)
        })?;
        let input = input.dupe();
        let src_value = src_value.dupe();

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
            self.output.get_path(),
            Some(&ContentBasedPathHash::for_output_artifact()),
        )?;

        let value = {
            let mut builder = ArtifactValueBuilder::new(artifact_fs.fs(), ctx.digest_config());
            builder.add_symlinked(&src_value, src.clone(), tmp_dest.as_ref())?;
            builder.build(tmp_dest.as_ref())?
        };

        let dest = if self.output.get_path().is_content_based_path() {
            artifact_fs.resolve_build(
                self.output.get_path(),
                Some(&value.content_based_path_hash()),
            )?
        } else {
            tmp_dest
        };
        let configuration_path = ctx
            .materializer()
            .maybe_eager_configuration_path(ctx.fs(), self.output.get_path())?;

        ctx.materializer()
            .declare_copy(
                dest.clone(),
                value.dupe(),
                vec![CopiedArtifact::new(
                    src,
                    dest,
                    value.entry().dupe().map_dir(|d| d.as_immutable()),
                    None,
                )],
                configuration_path,
            )
            .await?;

        Ok((
            ActionOutputs::from_single(self.output.get_path().dupe(), value),
            ActionExecutionMetadata {
                execution_kind: ActionExecutionKind::Simple,
                timing: ActionExecutionTimingData::default(),
                input_files_bytes: None,
                waiting_data,
            },
        ))
    }
}
