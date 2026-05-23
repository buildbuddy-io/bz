/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::borrow::Cow;
use std::slice;
use std::time::Instant;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_artifact::artifact::build_artifact::BuildArtifact;
use buck2_build_signals::env::WaitingData;
use buck2_common::file_ops::metadata::TrackedFileDigest;
use buck2_core::category::CategoryRef;
use buck2_core::content_hash::ContentBasedPathHash;
use buck2_error::internal_error;
use buck2_execute::execute::command_executor::ActionExecutionTimingData;
use buck2_execute::materialize::materializer::WriteRequest;
use buck2_hash::BuckIndexSet;
use buck2_hash::buck_indexmap;
use dupe::Dupe;
use starlark::values::OwnedFrozenValue;

use crate::actions::Action;
use crate::actions::ActionExecutionCtx;
use crate::actions::UnregisteredAction;
use crate::actions::execute::action_executor::ActionExecutionKind;
use crate::actions::execute::action_executor::ActionExecutionMetadata;
use crate::actions::execute::action_executor::ActionOutputs;
use crate::actions::execute::error::ExecuteError;
use crate::artifact_groups::ArtifactGroup;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Tier0)]
enum WorkspaceStatusActionError {
    #[error("workspace status action received no outputs")]
    NoOutputs,
    #[error("workspace status action received more than one output")]
    TooManyOutputs,
}

#[derive(Clone, Copy, Debug, Allocative)]
pub enum WorkspaceStatusKind {
    Stable,
    Volatile,
}

impl WorkspaceStatusKind {
    pub fn entries(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Stable => &[
                ("BUILD_EMBED_LABEL", ""),
                ("BUILD_HOST", "hostname"),
                ("BUILD_USER", "username"),
            ],
            Self::Volatile => &[
                ("BUILD_TIMESTAMP", "0"),
                ("FORMATTED_DATE", "1970 Jan 1 00 00 00 Thu"),
            ],
        }
    }

    pub fn output_path(self) -> &'static str {
        match self {
            Self::Stable => "__bazel_workspace_status/stable-status.txt",
            Self::Volatile => "__bazel_workspace_status/volatile-status.txt",
        }
    }

    fn identifier(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Volatile => "volatile",
        }
    }

    fn content(self) -> String {
        let mut content = String::new();
        for (key, value) in self.entries() {
            content.push_str(key);
            content.push(' ');
            content.push_str(value);
            content.push('\n');
        }
        content
    }
}

#[derive(Debug, Allocative)]
pub struct UnregisteredWorkspaceStatusAction {
    kind: WorkspaceStatusKind,
}

impl UnregisteredWorkspaceStatusAction {
    pub fn new(kind: WorkspaceStatusKind) -> Self {
        Self { kind }
    }
}

impl UnregisteredAction for UnregisteredWorkspaceStatusAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        starlark_data: Option<OwnedFrozenValue>,
        error_handler: Option<OwnedFrozenValue>,
    ) -> buck2_error::Result<Box<dyn Action>> {
        let _unused = (starlark_data, error_handler);
        let mut outputs = outputs.into_iter();
        let output = outputs
            .next()
            .ok_or(WorkspaceStatusActionError::NoOutputs)?;
        if outputs.next().is_some() {
            return Err(WorkspaceStatusActionError::TooManyOutputs.into());
        }
        Ok(Box::new(WorkspaceStatusAction {
            kind: self.kind,
            output,
        }))
    }
}

#[derive(Debug, Allocative)]
struct WorkspaceStatusAction {
    kind: WorkspaceStatusKind,
    output: BuildArtifact,
}

#[async_trait]
impl Action for WorkspaceStatusAction {
    fn kind(&self) -> buck2_data::ActionKind {
        buck2_data::ActionKind::Write
    }

    fn inputs(&self) -> buck2_error::Result<Cow<'_, [ArtifactGroup]>> {
        Ok(Cow::Borrowed(&[]))
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(slice::from_ref(&self.output))
    }

    fn first_output(&self) -> &BuildArtifact {
        &self.output
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new("bazel_workspace_status")
    }

    fn identifier(&self) -> Option<&str> {
        Some(self.kind.identifier())
    }

    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        let fs = ctx.fs();
        let content = self.kind.content().into_bytes();
        let mut execution_start = None;

        let value = ctx
            .materializer()
            .declare_write(Box::new(|| {
                execution_start = Some(Instant::now());
                let content_based_path_hash = if self.output.get_path().is_content_based_path() {
                    let digest = TrackedFileDigest::from_content(
                        &content,
                        ctx.digest_config().cas_digest_config(),
                    );
                    Some(ContentBasedPathHash::new(digest.raw_digest().as_bytes())?)
                } else {
                    None
                };
                let path =
                    fs.resolve_build(self.output.get_path(), content_based_path_hash.as_ref())?;
                let configuration_path = ctx
                    .materializer()
                    .maybe_eager_configuration_path(fs, self.output.get_path())?;
                Ok(vec![WriteRequest {
                    path,
                    content,
                    is_executable: false,
                    configuration_path,
                }])
            }))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| internal_error!("workspace status action did not execute"))?;

        let wall_time = Instant::now()
            - execution_start
                .ok_or_else(|| internal_error!("workspace status action did not set start time"))?;

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
