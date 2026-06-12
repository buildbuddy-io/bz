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
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use allocative::Allocative;
use async_trait::async_trait;
use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::artifact_type::BaseArtifactKind;
use bz_artifact::artifact::artifact_type::OutputArtifact;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_build_api::actions::Action;
use bz_build_api::actions::ActionExecutionCtx;
use bz_build_api::actions::UnregisteredAction;
use bz_build_api::actions::box_slice_set::BoxSliceSet;
use bz_build_api::actions::execute::action_execution_target::ActionExecutionTarget;
use bz_build_api::actions::execute::action_executor::ActionExecutionKind;
use bz_build_api::actions::execute::action_executor::ActionExecutionMetadata;
use bz_build_api::actions::execute::action_executor::ActionOutputs;
use bz_build_api::actions::execute::error::ExecuteError;
use bz_build_api::actions::impls::expanded_command_line::ExpandedCommandLine;
use bz_build_api::actions::impls::expanded_command_line::ExpandedCommandLineDigest;
use bz_build_api::actions::impls::expanded_command_line::ExpandedCommandLineFingerprinter;
use bz_build_api::artifact_groups::ArtifactGroup;
use bz_build_api::artifact_groups::ArtifactGroupValues;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::bazel_artifact_path;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::bazel_normalize_buck_owned_exec_paths;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact_value::StarlarkArtifactValue;
use bz_build_api::interpreter::rule_defs::artifact::starlark_output_artifact::FrozenStarlarkOutputArtifact;
use bz_build_api::interpreter::rule_defs::artifact::starlark_output_artifact::StarlarkOutputArtifact;
use bz_build_api::interpreter::rule_defs::artifact_tagging::ArtifactTag;
use bz_build_api::interpreter::rule_defs::cmd_args::ArtifactPathMapper;
use bz_build_api::interpreter::rule_defs::cmd_args::ArtifactPathMapperImpl;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineArtifactVisitor;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineBuilder;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineContext;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineLocation;
use bz_build_api::interpreter::rule_defs::cmd_args::DefaultCommandLineContext;
use bz_build_api::interpreter::rule_defs::cmd_args::FrozenStarlarkCmdArgs;
use bz_build_api::interpreter::rule_defs::cmd_args::ParamFileFormat;
use bz_build_api::interpreter::rule_defs::cmd_args::SimpleCommandLineArtifactVisitor;
use bz_build_api::interpreter::rule_defs::cmd_args::StarlarkCmdArgs;
use bz_build_api::interpreter::rule_defs::cmd_args::param_file::bazel_param_file_content;
use bz_build_api::interpreter::rule_defs::cmd_args::param_file::visit_bazel_param_file_content;
use bz_build_api::interpreter::rule_defs::cmd_args::space_separated::SpaceSeparatedCommandLineBuilder;
use bz_build_api::interpreter::rule_defs::cmd_args::value_as::ValueAsCommandLineLike;
use bz_build_api::interpreter::rule_defs::context::bazel_runfiles_prefix;
use bz_build_api::interpreter::rule_defs::provider::builtin::bazel::cc_info::BazelCcCompileCommandLine;
use bz_build_api::interpreter::rule_defs::provider::builtin::bazel::cc_info::FrozenBazelCcCompileCommandLine;
use bz_build_api::interpreter::rule_defs::provider::builtin::worker_info::FrozenWorkerInfo;
use bz_build_api::interpreter::rule_defs::provider::builtin::worker_info::WorkerInfo;
use bz_build_signals::env::WaitingCategory;
use bz_build_signals::env::WaitingData;
use bz_common::cas_digest::CasDigestConfig;
use bz_common::cas_digest::CasDigestData;
use bz_common::cas_digest::DataDigester;
use bz_common::external_symlink::ExternalSymlink;
use bz_common::file_ops::metadata::FileDigest;
use bz_common::file_ops::metadata::FileMetadata;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_common::io::trace::TracingIoProvider;
use bz_core::category::Category;
use bz_core::category::CategoryRef;
use bz_core::cells::cell_path::CellPathRef;
use bz_core::configuration::data::BazelBuildSettingValue;
use bz_core::content_hash::ContentBasedPathHash;
use bz_core::deferred::base_deferred_key::BaseDeferredKey;
use bz_core::deferred::key::DeferredHolderKey;
use bz_core::execution_types::executor_config::PathSeparatorKind;
use bz_core::execution_types::executor_config::ReGangWorker;
use bz_core::execution_types::executor_config::RemoteExecutionExtraParams;
use bz_core::execution_types::executor_config::RemoteExecutorCustomImage;
use bz_core::execution_types::executor_config::RemoteExecutorDependency;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::buck_out_path::BazelOutputRoot;
use bz_core::fs::buck_out_path::BuckOutPathKind;
use bz_core::fs::buck_out_path::BuildArtifactPath;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_directory::directory::dashmap_directory_interner::DashMapDirectoryInterner;
use bz_error::BuckErrorContext;
use bz_error::bz_error;
use bz_error::internal_error;
use bz_events::dispatch::span_async_simple;
use bz_execute::artifact::artifact_dyn::ArtifactDyn;
use bz_execute::artifact::fs::ExecutorFs;
use bz_execute::artifact::group::artifact_group_values_dyn::ArtifactGroupValuesDyn;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest_config::DigestConfig;
use bz_execute::directory::ActionDirectoryMember;
use bz_execute::execute::action_digest::ActionDigest;
use bz_execute::execute::action_digest_and_blobs::ActionDigestAndBlobs;
use bz_execute::execute::cache_uploader::IntoRemoteDepFile;
use bz_execute::execute::cache_uploader::force_cache_upload;
use bz_execute::execute::command_executor::ActionExecutionTimingData;
use bz_execute::execute::dep_file_digest::DepFileDigest;
use bz_execute::execute::environment_inheritance::EnvironmentInheritance;
use bz_execute::execute::request::ActionMetadataBlob;
use bz_execute::execute::request::CommandExecutionInput;
use bz_execute::execute::request::CommandExecutionOutput;
use bz_execute::execute::request::CommandExecutionPaths;
use bz_execute::execute::request::CommandExecutionRequest;
use bz_execute::execute::request::ExecutorPreference;
use bz_execute::execute::request::LocalActionCacheKey;
use bz_execute::execute::request::OutputCreationBehavior;
use bz_execute::execute::request::OutputType;
use bz_execute::execute::request::RemoteWorkerSpec;
use bz_execute::execute::request::WorkerId;
use bz_execute::execute::request::WorkerProtocol;
use bz_execute::execute::request::WorkerSpec;
use bz_execute::execute::result::CommandExecutionResult;
use bz_execute::materialize::materializer::CasDownloadInfo;
use bz_execute::materialize::materializer::WriteRequest;
use bz_fs::fs_util;
use bz_fs::paths::RelativePathBuf;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_hash::BuckIndexMap;
use bz_hash::BuckIndexSet;
use bz_hash::buck_indexmap;
use bz_util::thin_box::ThinBoxSlice;
use derive_more::Display;
use dupe::Dupe;
use either::Either;
use gazebo::prelude::*;
use host_sharing::HostSharingRequirements;
use host_sharing::WeightClass;
use itertools::Itertools;
use pagable::Pagable;
use serde_json::json;
use sha1::Digest as Sha1Digest;
use sha1::Sha1;
use sorted_vector_map::SortedVectorMap;
use starlark::collections::SmallSet;
use starlark::values::Freeze;
use starlark::values::FreezeResult;
use starlark::values::Freezer;
use starlark::values::FrozenStringValue;
use starlark::values::FrozenValue;
use starlark::values::FrozenValueOfUnchecked;
use starlark::values::FrozenValueTyped;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::OwnedFrozenValue;
use starlark::values::OwnedFrozenValueTyped;
use starlark::values::ProvidesStaticType;
use starlark::values::StarlarkPagable;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueOf;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueTyped;
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::dict::DictType;
use starlark::values::list::ListRef;
use starlark::values::starlark_value;
use starlark::values::structs::StructRef;

use self::dep_files::DepFileBundle;
use crate::actions::impls::offline;
use crate::actions::impls::run::dep_files::DepFilesCommandLineVisitor;
use crate::actions::impls::run::dep_files::RunActionDepFiles;
use crate::actions::impls::run::dep_files::make_dep_file_bundle;
use crate::actions::impls::run::dep_files::populate_dep_files;
use crate::actions::impls::run::metadata::metadata_content;
use crate::actions::impls::run::metadata::metadata_digest;
use crate::context::run::RunActionError;

pub(crate) mod audit_dep_files;
pub(crate) mod dep_files;
mod metadata;

#[derive(Debug, Allocative, Pagable)]
pub(crate) struct MetadataParameter {
    /// Name of the environment variable which is set to contain
    /// resolved path of the metadata file when requested by user.
    pub(crate) env_var: String,
    /// User-defined path in the output directory of the metadata file.
    pub(crate) path: ForwardRelativePathBuf,
    /// An artifact that is 'tagged' with any of these tags is ignored
    /// when computing the metadata.
    pub(crate) ignore_tags: SmallSet<ArtifactTag>,
}

impl Display for MetadataParameter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let json = json!({
            "env_var": self.env_var,
            "path": self.path,
            "ignore_tags": self.ignore_tags.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
        });
        write!(f, "{json}")
    }
}

/// A key that uniquely identifies a RunAction.
#[derive(Eq, PartialEq, Hash, Display, Allocative)]
#[display(
    "{} {} {}",
    owner,
    category,
    identifier.as_deref().unwrap_or("<no identifier>")
)]
pub(crate) struct RunActionKey {
    owner: BaseDeferredKey,
    category: Category,
    identifier: Option<String>,
}

impl RunActionKey {
    pub(crate) fn new(
        owner: BaseDeferredKey,
        category: Category,
        identifier: Option<String>,
    ) -> Self {
        Self {
            owner,
            category,
            identifier,
        }
    }

    pub(crate) fn from_action_execution_target(target: ActionExecutionTarget<'_>) -> Self {
        Self {
            owner: target.owner().dupe(),
            category: target.category().to_owned(),
            identifier: target.identifier().map(|t| t.to_owned()),
        }
    }
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum LocalPreferenceError {
    #[error("cannot have `local_only = True` and `prefer_local = True` at the same time")]
    LocalOnlyAndPreferLocal,
    #[error("cannot have `local_only = True` and `prefer_remote = True` at the same time")]
    LocalOnlyAndPreferRemote,
    #[error(
        "cannot have `local_only = True`, `prefer_local = True` and `prefer_remote = True` at the same time"
    )]
    LocalOnlyAndPreferLocalAndPreferRemote,
    #[error("cannot have `prefer_local = True` and `prefer_remote = True` at the same time")]
    PreferLocalAndPreferRemote,
}

pub(crate) fn new_executor_preference(
    local_only: bool,
    prefer_local: bool,
    prefer_remote: bool,
) -> bz_error::Result<ExecutorPreference> {
    match (local_only, prefer_local, prefer_remote) {
        (true, false, false) => Ok(ExecutorPreference::LocalRequired),
        (true, false, true) => Err(LocalPreferenceError::LocalOnlyAndPreferRemote.into()),
        (false, true, false) => Ok(ExecutorPreference::LocalPreferred),
        (false, true, true) => Err(LocalPreferenceError::PreferLocalAndPreferRemote.into()),
        (false, false, false) => Ok(ExecutorPreference::Default),
        (false, false, true) => Ok(ExecutorPreference::RemotePreferred),
        (true, true, false) => Err(LocalPreferenceError::LocalOnlyAndPreferLocal.into()),
        (true, true, true) => {
            Err(LocalPreferenceError::LocalOnlyAndPreferLocalAndPreferRemote.into())
        }
    }
}

#[derive(Debug, Allocative, Pagable)]
pub(crate) struct UnregisteredRunAction {
    pub(crate) executor_preference: ExecutorPreference,
    pub(crate) always_print_stderr: bool,
    pub(crate) eager_materialization_enabled: bool,
    pub(crate) weight: WeightClass,
    pub(crate) low_pass_filter: bool,
    pub(crate) dep_files: RunActionDepFiles,
    // Since this is usually None, use a Box to reduce memory.
    pub(crate) metadata_param: Option<Box<MetadataParameter>>,
    pub(crate) no_outputs_cleanup: bool,
    pub(crate) incremental_remote_outputs: bool,
    pub(crate) allow_cache_upload: Option<bool>,
    pub(crate) allow_dep_file_cache_upload: bool,
    pub(crate) allow_offline_output_cache: bool,
    pub(crate) force_full_hybrid_if_capable: bool,
    pub(crate) unique_input_inodes: bool,
    pub(crate) remote_execution_dependencies: ThinBoxSlice<RemoteExecutorDependency>,
    pub(crate) re_gang_workers: ThinBoxSlice<ReGangWorker>,
    // Since this is usually None, use a Box to avoid using memory that is the size
    // of RemoteExecutorCustomImage.
    pub(crate) remote_execution_custom_image: Option<Box<RemoteExecutorCustomImage>>,
    pub(crate) remote_execution_extra_params: Arc<RemoteExecutionExtraParams>,
    pub(crate) expected_eligible_for_dedupe: Option<bool>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) bazel_use_default_shell_env: Option<bool>,
    pub(crate) supports_bazel_path_mapping: bool,
    pub(crate) bazel_string_args: Option<Box<[String]>>,
    #[pagable(discard = "None")]
    pub(crate) precomputed_local_action_cache_command_line_digest:
        Option<ExpandedCommandLineDigest>,
}

impl UnregisteredAction for UnregisteredRunAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        starlark_data: Option<OwnedFrozenValue>,
        error_handler: Option<OwnedFrozenValue>,
    ) -> bz_error::Result<Box<dyn Action>> {
        let starlark_values =
            starlark_data.ok_or_else(|| internal_error!("module data to be present"))?;
        let run_action = RunAction::new(*self, starlark_values, outputs, error_handler)?;
        Ok(Box::new(run_action))
    }
}

#[derive(Debug, Display, Trace, ProvidesStaticType, NoSerialize, Allocative)]
#[display("RunActionValues")]
pub(crate) struct StarlarkRunActionValues<'v> {
    pub(crate) exe: ValueTyped<'v, StarlarkCmdArgs<'v>>,
    pub(crate) args: ValueTyped<'v, StarlarkCmdArgs<'v>>,
    pub(crate) bazel_inputs: Option<ValueTyped<'v, StarlarkCmdArgs<'v>>>,
    pub(crate) bazel_executable: Option<Value<'v>>,
    pub(crate) bazel_executable_runfiles: Option<Value<'v>>,
    pub(crate) bazel_tool_runfiles: Option<Value<'v>>,
    pub(crate) bazel_cc_command_line: Option<ValueTyped<'v, BazelCcCompileCommandLine<'v>>>,
    pub(crate) env: Option<ValueOfUnchecked<'v, DictType<String, ValueAsCommandLineLike<'static>>>>,
    pub(crate) worker: Option<ValueTypedComplex<'v, WorkerInfo<'v>>>,
    pub(crate) remote_worker: Option<ValueTypedComplex<'v, WorkerInfo<'v>>>,
    pub(crate) category: StringValue<'v>,
    pub(crate) identifier: Option<StringValue<'v>>,
    pub(crate) outputs_for_error_handler: Vec<ValueTyped<'v, StarlarkOutputArtifact<'v>>>,
}

#[derive(
    Debug,
    Display,
    Trace,
    ProvidesStaticType,
    NoSerialize,
    Allocative,
    StarlarkPagable
)]
#[display("RunActionValues")]
pub(crate) struct FrozenStarlarkRunActionValues {
    pub(crate) exe: FrozenValueTyped<'static, FrozenStarlarkCmdArgs>,
    pub(crate) args: FrozenValueTyped<'static, FrozenStarlarkCmdArgs>,
    pub(crate) bazel_inputs: Option<FrozenValueTyped<'static, FrozenStarlarkCmdArgs>>,
    pub(crate) bazel_executable: Option<FrozenValue>,
    pub(crate) bazel_executable_runfiles: Option<FrozenValue>,
    pub(crate) bazel_tool_runfiles: Option<FrozenValue>,
    pub(crate) bazel_cc_command_line:
        Option<FrozenValueTyped<'static, FrozenBazelCcCompileCommandLine>>,
    pub(crate) env:
        Option<FrozenValueOfUnchecked<'static, DictType<String, ValueAsCommandLineLike<'static>>>>,
    pub(crate) worker: Option<FrozenValueTyped<'static, FrozenWorkerInfo>>,
    pub(crate) remote_worker: Option<FrozenValueTyped<'static, FrozenWorkerInfo>>,
    pub(crate) category: FrozenStringValue,
    pub(crate) identifier: Option<FrozenStringValue>,
    pub(crate) outputs_for_error_handler:
        Vec<FrozenValueTyped<'static, FrozenStarlarkOutputArtifact>>,
}

#[starlark_value(type = "RunActionValues")]
impl<'v> StarlarkValue<'v> for StarlarkRunActionValues<'v> {}

#[starlark_value(type = "RunActionValues", skip_pagable)]
impl<'v> StarlarkValue<'v> for FrozenStarlarkRunActionValues {
    type Canonical = StarlarkRunActionValues<'v>;
}

impl<'v> Freeze for StarlarkRunActionValues<'v> {
    type Frozen = FrozenStarlarkRunActionValues;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let StarlarkRunActionValues {
            exe,
            args,
            bazel_inputs,
            bazel_executable,
            bazel_executable_runfiles,
            bazel_tool_runfiles,
            bazel_cc_command_line,
            env,
            worker,
            remote_worker,
            category,
            identifier,
            outputs_for_error_handler,
        } = self;
        Ok(FrozenStarlarkRunActionValues {
            exe: exe.freeze(freezer)?,
            args: args.freeze(freezer)?,
            bazel_inputs: bazel_inputs.freeze(freezer)?,
            bazel_executable: bazel_executable.freeze(freezer)?,
            bazel_executable_runfiles: bazel_executable_runfiles.freeze(freezer)?,
            bazel_tool_runfiles: bazel_tool_runfiles.freeze(freezer)?,
            bazel_cc_command_line: bazel_cc_command_line.freeze(freezer)?,
            env: env.freeze(freezer)?,
            worker: worker.freeze(freezer)?,
            remote_worker: remote_worker.freeze(freezer)?,
            category: category.freeze(freezer)?,
            identifier: identifier.freeze(freezer)?,
            // N.B. collect::<Result<_>> sets the lower bound to zero,
            // which can cause over-allocations in frozen containers.
            outputs_for_error_handler: {
                let mut frozen_outputs = Vec::with_capacity(outputs_for_error_handler.len());
                for output in outputs_for_error_handler {
                    frozen_outputs.push(output.freeze(freezer)?);
                }
                frozen_outputs
            },
        })
    }
}

impl FrozenStarlarkRunActionValues {
    pub(crate) fn worker<'v>(
        &'v self,
    ) -> bz_error::Result<Option<ValueOf<'v, &'v WorkerInfo<'v>>>> {
        let Some(worker) = self.worker else {
            return Ok(None);
        };
        ValueOf::unpack_value_err(worker.to_value())
            .map_err(bz_error::Error::from)
            .map(Some)
    }

    pub(crate) fn remote_worker<'v>(
        &'v self,
    ) -> bz_error::Result<Option<ValueOf<'v, &'v WorkerInfo<'v>>>> {
        let Some(remote_worker) = self.remote_worker else {
            return Ok(None);
        };
        ValueOf::unpack_value_err(remote_worker.to_value())
            .map_err(bz_error::Error::from)
            .map(Some)
    }
}

struct UnpackedWorkerValues<'v> {
    exe: &'v dyn CommandLineArgLike<'v>,
    env: Vec<(&'v str, &'v dyn CommandLineArgLike<'v>)>,
    id: WorkerId,
    concurrency: Option<usize>,
    streaming: bool,
    supports_bazel_local_persistent_worker_protocol: bool,
    supports_bazel_remote_persistent_worker_protocol: bool,
    requires_bazel_worker_sandboxing: bool,
}

struct UnpackedRunActionValues<'v> {
    exe: &'v dyn CommandLineArgLike<'v>,
    args: &'v dyn CommandLineArgLike<'v>,
    bazel_inputs: Option<&'v dyn CommandLineArgLike<'v>>,
    bazel_executable: Option<Value<'v>>,
    bazel_executable_runfiles: Option<Value<'v>>,
    bazel_tool_runfiles: Option<Value<'v>>,
    bazel_cc_command_line: Option<&'v FrozenBazelCcCompileCommandLine>,
    env: Vec<(&'v str, &'v dyn CommandLineArgLike<'v>)>,
    worker: Option<UnpackedWorkerValues<'v>>,
    remote_worker: Option<UnpackedWorkerValues<'v>>,
}

enum LocalActionCacheWorkerRef<'a> {
    Borrowed(&'a WorkerSpec),
    Owned(WorkerSpec),
    Probe(LocalActionCacheWorkerProbe),
}

impl LocalActionCacheWorkerRef<'_> {
    fn id(&self) -> WorkerId {
        match self {
            Self::Borrowed(worker) => worker.id,
            Self::Owned(worker) => worker.id,
            Self::Probe(worker) => worker.id,
        }
    }

    fn protocol(&self) -> WorkerProtocol {
        match self {
            Self::Borrowed(worker) => worker.protocol,
            Self::Owned(worker) => worker.protocol,
            Self::Probe(worker) => worker.protocol,
        }
    }

    fn exe(&self) -> &[String] {
        match self {
            Self::Borrowed(worker) => &worker.exe,
            Self::Owned(worker) => &worker.exe,
            Self::Probe(worker) => &worker.exe,
        }
    }

    fn env(&self) -> &SortedVectorMap<String, String> {
        match self {
            Self::Borrowed(worker) => &worker.env,
            Self::Owned(worker) => &worker.env,
            Self::Probe(worker) => &worker.env,
        }
    }

    fn concurrency(&self) -> Option<usize> {
        match self {
            Self::Borrowed(worker) => worker.concurrency,
            Self::Owned(worker) => worker.concurrency,
            Self::Probe(worker) => worker.concurrency,
        }
    }

    fn streaming(&self) -> bool {
        match self {
            Self::Borrowed(worker) => worker.streaming,
            Self::Owned(worker) => worker.streaming,
            Self::Probe(worker) => worker.streaming,
        }
    }

    fn bazel_worker_sandboxing(&self) -> bool {
        match self {
            Self::Borrowed(worker) => worker.bazel_worker_sandboxing,
            Self::Owned(worker) => worker.bazel_worker_sandboxing,
            Self::Probe(worker) => worker.bazel_worker_sandboxing,
        }
    }

    fn remote_key(&self) -> Option<&TrackedFileDigest> {
        match self {
            Self::Borrowed(worker) => worker.remote_key.as_ref(),
            Self::Owned(worker) => worker.remote_key.as_ref(),
            Self::Probe(worker) => worker.remote_key.as_ref(),
        }
    }

    fn inputs(&self) -> &[CommandExecutionInput] {
        match self {
            Self::Borrowed(worker) => worker.inputs(),
            Self::Owned(worker) => worker.inputs(),
            Self::Probe(worker) => &worker.inputs,
        }
    }
}

struct LocalActionCacheWorkerProbe {
    id: WorkerId,
    protocol: WorkerProtocol,
    exe: Vec<String>,
    env: SortedVectorMap<String, String>,
    concurrency: Option<usize>,
    streaming: bool,
    bazel_worker_sandboxing: bool,
    remote_key: Option<TrackedFileDigest>,
    inputs: Vec<CommandExecutionInput>,
}

enum LocalActionCacheRemoteWorkerRef<'a> {
    Borrowed(&'a RemoteWorkerSpec),
    Owned(RemoteWorkerSpec),
    Probe(LocalActionCacheRemoteWorkerProbe),
}

impl LocalActionCacheRemoteWorkerRef<'_> {
    fn id(&self) -> WorkerId {
        match self {
            Self::Borrowed(remote_worker) => remote_worker.id,
            Self::Owned(remote_worker) => remote_worker.id,
            Self::Probe(remote_worker) => remote_worker.id,
        }
    }

    fn init(&self) -> &[String] {
        match self {
            Self::Borrowed(remote_worker) => &remote_worker.init,
            Self::Owned(remote_worker) => &remote_worker.init,
            Self::Probe(remote_worker) => &remote_worker.init,
        }
    }

    fn env(&self) -> &SortedVectorMap<String, String> {
        match self {
            Self::Borrowed(remote_worker) => &remote_worker.env,
            Self::Owned(remote_worker) => &remote_worker.env,
            Self::Probe(remote_worker) => &remote_worker.env,
        }
    }

    fn concurrency(&self) -> Option<usize> {
        match self {
            Self::Borrowed(remote_worker) => remote_worker.concurrency,
            Self::Owned(remote_worker) => remote_worker.concurrency,
            Self::Probe(remote_worker) => remote_worker.concurrency,
        }
    }

    fn inputs(&self) -> &[CommandExecutionInput] {
        match self {
            Self::Borrowed(remote_worker) => remote_worker.inputs(),
            Self::Owned(remote_worker) => remote_worker.inputs(),
            Self::Probe(remote_worker) => &remote_worker.inputs,
        }
    }
}

struct LocalActionCacheRemoteWorkerProbe {
    id: WorkerId,
    init: Vec<String>,
    env: SortedVectorMap<String, String>,
    concurrency: Option<usize>,
    inputs: Vec<CommandExecutionInput>,
}

#[derive(Debug, Allocative, Pagable)]
pub(crate) struct RunAction {
    inner: UnregisteredRunAction,
    starlark_values: OwnedFrozenValueTyped<FrozenStarlarkRunActionValues>,
    outputs: BoxSliceSet<BuildArtifact>,
    inputs: Box<[ArtifactGroup]>,
    command_inputs: Box<[ArtifactGroup]>,
    local_action_cache_inputs: Box<[ArtifactGroup]>,
    non_hidden_inputs: Box<[ArtifactGroup]>,
    error_handler: Option<OwnedFrozenValue>,
}

#[allow(clippy::large_enum_variant)]
enum ExecuteResult {
    LocalDepFileHit(ActionOutputs, ActionExecutionMetadata),
    LocalActionCacheHit {
        result: CommandExecutionResult,
        executor_preference: ExecutorPreference,
    },
    ExecutedOrReHit {
        result: CommandExecutionResult,
        dep_file_bundle: Option<DepFileBundle>,
        executor_preference: ExecutorPreference,
        request: CommandExecutionRequest,
        action_and_blobs: ActionDigestAndBlobs,
        input_files_bytes: u64,
    },
}

pub struct DepFilesPlaceholderArtifactPathMapper {}

impl ArtifactPathMapper for DepFilesPlaceholderArtifactPathMapper {
    fn get(&self, _artifact: &Artifact) -> Option<&ContentBasedPathHash> {
        Some(&ContentBasedPathHash::DepFilesPlaceholder)
    }
}

struct DepFilesPlaceholderArtifactPathMapperWithValues<'a> {
    values: &'a dyn ArtifactPathMapper,
}

impl ArtifactPathMapper for DepFilesPlaceholderArtifactPathMapperWithValues<'_> {
    fn get(&self, _artifact: &Artifact) -> Option<&ContentBasedPathHash> {
        Some(&ContentBasedPathHash::DepFilesPlaceholder)
    }

    fn artifact_value(
        &self,
        artifact: &Artifact,
    ) -> Option<&bz_execute::artifact_value::ArtifactValue> {
        self.values.artifact_value(artifact)
    }
}

fn artifact_is_run_action_output(
    outputs: &BoxSliceSet<BuildArtifact>,
    artifact: &Artifact,
) -> bool {
    match artifact.as_parts().0 {
        BaseArtifactKind::Build(build) => outputs.iter().any(|output| output == build),
        BaseArtifactKind::Source(_) => false,
    }
}

fn artifact_group_is_run_action_output(
    outputs: &BoxSliceSet<BuildArtifact>,
    artifact_group: &ArtifactGroup,
) -> bool {
    match artifact_group {
        ArtifactGroup::Artifact(artifact) => artifact_is_run_action_output(outputs, artifact),
        ArtifactGroup::TransitiveSetProjection(_) | ArtifactGroup::Promise(_) => false,
    }
}

struct RunActionOutputArtifactPathMapper<'a> {
    outputs: &'a BoxSliceSet<BuildArtifact>,
    inner: &'a dyn ArtifactPathMapper,
    output_hash: ContentBasedPathHash,
}

impl<'a> RunActionOutputArtifactPathMapper<'a> {
    fn new(outputs: &'a BoxSliceSet<BuildArtifact>, inner: &'a dyn ArtifactPathMapper) -> Self {
        Self {
            outputs,
            inner,
            output_hash: ContentBasedPathHash::for_output_artifact(),
        }
    }
}

impl ArtifactPathMapper for RunActionOutputArtifactPathMapper<'_> {
    fn get(&self, artifact: &Artifact) -> Option<&ContentBasedPathHash> {
        if artifact_is_run_action_output(self.outputs, artifact) {
            Some(&self.output_hash)
        } else {
            self.inner.get(artifact)
        }
    }

    fn artifact_value(
        &self,
        artifact: &Artifact,
    ) -> Option<&bz_execute::artifact_value::ArtifactValue> {
        if artifact_is_run_action_output(self.outputs, artifact) {
            None
        } else {
            self.inner.artifact_value(artifact)
        }
    }
}

struct BazelRunActionArtifactPathMapper<'a> {
    ctx: &'a dyn ActionExecutionCtx,
    inputs: &'a [ArtifactGroup],
    values: RefCell<Option<Vec<&'a ArtifactGroupValues>>>,
}

impl<'a> BazelRunActionArtifactPathMapper<'a> {
    fn new(ctx: &'a dyn ActionExecutionCtx, inputs: &'a [ArtifactGroup]) -> Self {
        Self {
            ctx,
            inputs,
            values: RefCell::new(None),
        }
    }
}

impl ArtifactPathMapper for BazelRunActionArtifactPathMapper<'_> {
    fn get(&self, _artifact: &Artifact) -> Option<&ContentBasedPathHash> {
        None
    }

    fn artifact_value(&self, artifact: &Artifact) -> Option<&ArtifactValue> {
        if self.values.borrow().is_none() {
            let values = self
                .inputs
                .iter()
                .map(|input| self.ctx.artifact_values(input))
                .collect();
            *self.values.borrow_mut() = Some(values);
        }
        let values = self.values.borrow();
        let values = values.as_ref()?;
        for values in values {
            for (input_artifact, value) in values.iter() {
                if input_artifact == artifact {
                    return Some(value);
                }
            }
        }
        None
    }
}

enum RunActionInputArtifactPathMapper<'a> {
    Default(ArtifactPathMapperImpl<'a>),
    Bazel(BazelRunActionArtifactPathMapper<'a>),
}

impl ArtifactPathMapper for RunActionInputArtifactPathMapper<'_> {
    fn get(&self, artifact: &Artifact) -> Option<&ContentBasedPathHash> {
        match self {
            Self::Default(mapper) => mapper.get(artifact),
            Self::Bazel(mapper) => mapper.get(artifact),
        }
    }

    fn artifact_value(&self, artifact: &Artifact) -> Option<&ArtifactValue> {
        match self {
            Self::Default(mapper) => mapper.artifact_value(artifact),
            Self::Bazel(mapper) => mapper.artifact_value(artifact),
        }
    }
}

struct BazelCommandLineContext<'v> {
    inner: DefaultCommandLineContext<'v>,
}

impl<'v> BazelCommandLineContext<'v> {
    fn new(fs: &'v ExecutorFs<'v>) -> Self {
        Self {
            inner: DefaultCommandLineContext::new(fs),
        }
    }
}

impl CommandLineContext for BazelCommandLineContext<'_> {
    fn resolve_project_path(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        self.inner.resolve_project_path(path)
    }

    fn fs(&self) -> &ExecutorFs<'_> {
        self.inner.fs()
    }

    fn resolve_artifact(
        &self,
        artifact: &Artifact,
        _artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        Ok(CommandLineLocation::from_relative_path(
            RelativePathBuf::from(bazel_artifact_path(artifact.get_path())),
            self.fs().path_separator(),
        ))
    }

    fn resolve_output_artifact(
        &self,
        artifact: &Artifact,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        Ok(CommandLineLocation::from_relative_path(
            RelativePathBuf::from(bazel_artifact_path(artifact.get_path())),
            self.fs().path_separator(),
        ))
    }

    fn next_macro_file_path(&mut self) -> bz_error::Result<bz_fs::paths::RelativePathBuf> {
        self.inner.next_macro_file_path()
    }
}

#[derive(Clone, Copy, Dupe)]
enum RunActionParamFileMode {
    Record,
    RecordDigestOnly,
    Replay,
}

#[derive(Clone)]
struct RunActionParamFile {
    path: BuildArtifactPath,
    digest: TrackedFileDigest,
    content_hash: ContentBasedPathHash,
    content: Vec<u8>,
    command_line_path: bz_fs::paths::RelativePathBuf,
    bazel_exec_path: Option<String>,
}

struct PendingActionMetadataWrite {
    path: BuildArtifactPath,
    content_hash: ContentBasedPathHash,
    content: Vec<u8>,
}

struct RunActionParamFiles {
    owner: DeferredHolderKey,
    base_path: ForwardRelativePathBuf,
    base_bazel_exec_path: Option<String>,
    bazel_output_root: BazelOutputRoot,
    path_resolution_method: BuckOutPathKind,
    digest_config: DigestConfig,
    files: Vec<RunActionParamFile>,
    replay_cursor: usize,
}

#[derive(Clone)]
struct RunActionParamFilesRef(Rc<RefCell<RunActionParamFiles>>);

impl RunActionParamFilesRef {
    fn new(
        owner: DeferredHolderKey,
        base_path: ForwardRelativePathBuf,
        base_bazel_exec_path: Option<String>,
        bazel_output_root: BazelOutputRoot,
        path_resolution_method: BuckOutPathKind,
        digest_config: DigestConfig,
    ) -> Self {
        Self(Rc::new(RefCell::new(RunActionParamFiles {
            owner,
            base_path,
            base_bazel_exec_path,
            bazel_output_root,
            path_resolution_method,
            digest_config,
            files: Vec::new(),
            replay_cursor: 0,
        })))
    }

    fn add_param_file(
        &self,
        fs: &ExecutorFs<'_>,
        mode: RunActionParamFileMode,
        content: Vec<u8>,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        let mut state = self.0.borrow_mut();
        match mode {
            RunActionParamFileMode::Record | RunActionParamFileMode::RecordDigestOnly => {
                let index = state.files.len();
                let path =
                    BuildArtifactPath::with_dynamic_actions_action_key_and_bazel_owner_and_output_root(
                        state.owner.dupe(),
                        derive_param_file_path(&state.base_path, index)?,
                        state.path_resolution_method,
                        None,
                        state.bazel_output_root,
                    );
                let digest = TrackedFileDigest::from_content(
                    &content,
                    state.digest_config.cas_digest_config(),
                );
                let content_hash = ContentBasedPathHash::new(digest.raw_digest().as_bytes())?;
                let command_line_path =
                    if let Some(base_bazel_exec_path) = &state.base_bazel_exec_path {
                        bz_fs::paths::RelativePathBuf::from(derive_param_file_exec_path(
                            base_bazel_exec_path,
                            index,
                        ))
                    } else {
                        fs.fs()
                            .buck_out_path_resolver()
                            .resolve_gen(&path, Some(&content_hash))?
                            .into()
                    };
                let bazel_exec_path = state
                    .base_bazel_exec_path
                    .as_ref()
                    .map(|base| derive_param_file_exec_path(base, index));
                state.files.push(RunActionParamFile {
                    path,
                    digest,
                    content_hash,
                    content,
                    command_line_path: command_line_path.clone(),
                    bazel_exec_path,
                });
                Ok(CommandLineLocation::from_relative_path(
                    command_line_path,
                    fs.path_separator(),
                ))
            }
            RunActionParamFileMode::Replay => {
                let index = state.replay_cursor;
                let Some(param_file) = state.files.get(index) else {
                    return Err(internal_error!(
                        "param-file replay requested entry {index}, but only {} were recorded",
                        state.files.len()
                    ));
                };
                if param_file.content != content {
                    return Err(internal_error!(
                        "param-file replay content mismatch for entry {index}"
                    ));
                }
                let command_line_path = param_file.command_line_path.clone();
                state.replay_cursor += 1;
                Ok(CommandLineLocation::from_relative_path(
                    command_line_path,
                    fs.path_separator(),
                ))
            }
        }
    }

    fn add_param_file_args(
        &self,
        fs: &ExecutorFs<'_>,
        mode: RunActionParamFileMode,
        args: Vec<String>,
        format: ParamFileFormat,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        match mode {
            RunActionParamFileMode::RecordDigestOnly => {
                let mut state = self.0.borrow_mut();
                let index = state.files.len();
                let path =
                    BuildArtifactPath::with_dynamic_actions_action_key_and_bazel_owner_and_output_root(
                        state.owner.dupe(),
                        derive_param_file_path(&state.base_path, index)?,
                        state.path_resolution_method,
                        None,
                        state.bazel_output_root,
                    );
                let mut digester = FileDigest::digester(state.digest_config.cas_digest_config());
                visit_bazel_param_file_content(args.iter(), format, |bytes| {
                    digester.update(bytes);
                });
                let digest = TrackedFileDigest::new(
                    digester.finalize(),
                    state.digest_config.cas_digest_config(),
                );
                let content_hash = ContentBasedPathHash::new(digest.raw_digest().as_bytes())?;
                let command_line_path =
                    if let Some(base_bazel_exec_path) = &state.base_bazel_exec_path {
                        bz_fs::paths::RelativePathBuf::from(derive_param_file_exec_path(
                            base_bazel_exec_path,
                            index,
                        ))
                    } else {
                        fs.fs()
                            .buck_out_path_resolver()
                            .resolve_gen(&path, Some(&content_hash))?
                            .into()
                    };
                let bazel_exec_path = state
                    .base_bazel_exec_path
                    .as_ref()
                    .map(|base| derive_param_file_exec_path(base, index));
                state.files.push(RunActionParamFile {
                    path,
                    digest,
                    content_hash,
                    content: Vec::new(),
                    command_line_path: command_line_path.clone(),
                    bazel_exec_path,
                });
                Ok(CommandLineLocation::from_relative_path(
                    command_line_path,
                    fs.path_separator(),
                ))
            }
            RunActionParamFileMode::Record | RunActionParamFileMode::Replay => {
                self.add_param_file(fs, mode, bazel_param_file_content(args, format))
            }
        }
    }

    fn files(&self, require_replay: bool) -> bz_error::Result<Vec<RunActionParamFile>> {
        let state = self.0.borrow();
        if require_replay && state.replay_cursor != state.files.len() {
            return Err(internal_error!(
                "param-file replay consumed {} entries, but {} were recorded",
                state.replay_cursor,
                state.files.len()
            ));
        }
        Ok(state.files.clone())
    }
}

fn derive_param_file_path(
    base_path: &bz_fs::paths::forward_rel_path::ForwardRelativePath,
    index: usize,
) -> bz_error::Result<ForwardRelativePathBuf> {
    let file_name = base_path
        .file_name()
        .ok_or_else(|| internal_error!("cannot derive param-file path from empty output path"))?;
    let derived_name = format!("{}-{index}.params", file_name.as_str());
    if let Some(parent) = base_path.parent()
        && !parent.is_empty()
    {
        ForwardRelativePathBuf::new(format!("{}/{derived_name}", parent.as_str()))
    } else {
        ForwardRelativePathBuf::new(derived_name)
    }
}

fn derive_param_file_exec_path(base_exec_path: &str, index: usize) -> String {
    let (parent, file_name) = base_exec_path
        .rsplit_once('/')
        .map_or(("", base_exec_path), |(parent, file_name)| {
            (parent, file_name)
        });
    let derived_name = format!("{file_name}-{index}.params");
    if parent.is_empty() {
        derived_name
    } else {
        format!("{parent}/{derived_name}")
    }
}

fn bazel_param_file_base_path(base_exec_path: &str) -> bz_error::Result<ForwardRelativePathBuf> {
    let rest = base_exec_path
        .strip_prefix("buck-out/bin/")
        .or_else(|| base_exec_path.strip_prefix("buck-out/genfiles/"))
        .ok_or_else(|| {
            internal_error!(
                "Bazel output path `{base_exec_path}` is not under buck-out/bin or buck-out/genfiles"
            )
        })?;
    let (_, path) = rest.split_once('/').ok_or_else(|| {
        internal_error!("Bazel output path `{base_exec_path}` has no configuration segment")
    })?;
    ForwardRelativePathBuf::new(path.to_owned()).buck_error_context("Invalid Bazel param-file path")
}

enum RunActionCommandLineContext<'v> {
    Default {
        inner: DefaultCommandLineContext<'v>,
        param_files: RunActionParamFilesRef,
        param_file_mode: RunActionParamFileMode,
    },
    Bazel {
        inner: BazelCommandLineContext<'v>,
        param_files: RunActionParamFilesRef,
        param_file_mode: RunActionParamFileMode,
        bazel_path_mapping: bool,
    },
}

impl<'v> RunActionCommandLineContext<'v> {
    fn new(
        fs: &'v ExecutorFs<'v>,
        bazel_paths: bool,
        bazel_path_mapping: bool,
        param_files: RunActionParamFilesRef,
        param_file_mode: RunActionParamFileMode,
    ) -> Self {
        if bazel_paths {
            Self::Bazel {
                inner: BazelCommandLineContext::new(fs),
                param_files,
                param_file_mode,
                bazel_path_mapping,
            }
        } else {
            Self::Default {
                inner: DefaultCommandLineContext::new(fs),
                param_files,
                param_file_mode,
            }
        }
    }
}

impl CommandLineContext for RunActionCommandLineContext<'_> {
    fn resolve_project_path(
        &self,
        path: ProjectRelativePathBuf,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        match self {
            Self::Default { inner, .. } => inner.resolve_project_path(path),
            Self::Bazel { inner, .. } => inner.resolve_project_path(path),
        }
    }

    fn fs(&self) -> &ExecutorFs<'_> {
        match self {
            Self::Default { inner, .. } => inner.fs(),
            Self::Bazel { inner, .. } => inner.fs(),
        }
    }

    fn resolve_artifact(
        &self,
        artifact: &Artifact,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        match self {
            Self::Default { inner, .. } => inner.resolve_artifact(artifact, artifact_path_mapping),
            Self::Bazel { inner, .. } => inner.resolve_artifact(artifact, artifact_path_mapping),
        }
    }

    fn resolve_output_artifact(
        &self,
        artifact: &Artifact,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        match self {
            Self::Default { inner, .. } => inner.resolve_output_artifact(artifact),
            Self::Bazel { inner, .. } => inner.resolve_output_artifact(artifact),
        }
    }

    fn next_macro_file_path(&mut self) -> bz_error::Result<bz_fs::paths::RelativePathBuf> {
        match self {
            Self::Default { inner, .. } => inner.next_macro_file_path(),
            Self::Bazel { inner, .. } => inner.next_macro_file_path(),
        }
    }

    fn add_param_file(&mut self, content: Vec<u8>) -> bz_error::Result<CommandLineLocation<'_>> {
        match self {
            Self::Default {
                inner,
                param_files,
                param_file_mode,
            } => param_files.add_param_file(inner.fs(), *param_file_mode, content),
            Self::Bazel {
                inner,
                param_files,
                param_file_mode,
                ..
            } => param_files.add_param_file(inner.fs(), *param_file_mode, content),
        }
    }

    fn add_param_file_args(
        &mut self,
        args: Vec<String>,
        format: ParamFileFormat,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        match self {
            Self::Default {
                inner,
                param_files,
                param_file_mode,
            } => param_files.add_param_file_args(inner.fs(), *param_file_mode, args, format),
            Self::Bazel {
                inner,
                param_files,
                param_file_mode,
                ..
            } => param_files.add_param_file_args(inner.fs(), *param_file_mode, args, format),
        }
    }

    fn normalize_param_file_arg(&self, arg: String) -> String {
        match self {
            Self::Default { .. } => arg,
            Self::Bazel {
                bazel_path_mapping, ..
            } => bazel_normalize_and_map_buck_owned_exec_paths(&arg, *bazel_path_mapping),
        }
    }
}

struct LocalActionCacheCommandLineFingerprinter<'a> {
    inner: &'a mut ExpandedCommandLineFingerprinter,
    bazel_paths: bool,
    bazel_path_mapping: bool,
}

impl CommandLineBuilder for LocalActionCacheCommandLineFingerprinter<'_> {
    fn push_arg(&mut self, s: String) {
        if self.bazel_paths {
            self.inner
                .push_arg(bazel_normalize_and_map_buck_owned_exec_paths(
                    &s,
                    self.bazel_path_mapping,
                ));
        } else {
            self.inner.push_arg(s);
        }
    }
}

fn fingerprint_param_files(
    command_line_digest: &mut ExpandedCommandLineFingerprinter,
    param_files: &[RunActionParamFile],
) {
    for param_file in param_files {
        fingerprint_param_file_digest(command_line_digest, &param_file.digest);
    }
    command_line_digest.push_count();
}

fn fingerprint_param_file_digest(
    command_line_digest: &mut ExpandedCommandLineFingerprinter,
    digest: &TrackedFileDigest,
) {
    command_line_digest.push_arg_bytes(digest.raw_digest().as_bytes());
    command_line_digest.push_arg_bytes(&digest.size().to_le_bytes());
}

fn fingerprint_expanded_command_line_for_local_action_cache(
    expanded: &ExpandedCommandLine,
    param_files: &[RunActionParamFile],
) -> ExpandedCommandLineDigest {
    let mut command_line_digest = ExpandedCommandLineFingerprinter::new();
    for arg in &expanded.exe {
        command_line_digest.push_arg(arg.to_owned());
    }
    command_line_digest.push_count();

    for arg in &expanded.args {
        command_line_digest.push_arg(arg.to_owned());
    }
    command_line_digest.push_count();

    for (key, value) in &expanded.env {
        command_line_digest.push_arg(key.to_owned());
        command_line_digest.push_arg(value.to_owned());
    }
    command_line_digest.push_count();

    fingerprint_param_files(&mut command_line_digest, param_files);
    command_line_digest.finalize()
}

fn push_string_args_for_local_action_cache(
    command_line_digest: &mut ExpandedCommandLineFingerprinter,
    args: &[String],
    bazel_paths: bool,
    bazel_path_mapping: bool,
) {
    for arg in args {
        if bazel_paths {
            command_line_digest.push_arg(bazel_normalize_and_map_buck_owned_exec_paths(
                arg,
                bazel_path_mapping,
            ));
        } else {
            command_line_digest.push_arg(arg.to_owned());
        }
    }
}

fn push_string_args_for_dep_file_digest(
    command_line_digest: &mut ExpandedCommandLineFingerprinter,
    args: &[String],
    bazel_paths: bool,
    bazel_path_mapping: bool,
) {
    push_string_args_for_local_action_cache(
        command_line_digest,
        args,
        bazel_paths,
        bazel_path_mapping,
    );
    command_line_digest.push_count();
}

pub(crate) fn precompute_bazel_local_action_cache_command_line_digest_for_cc_compile_command_line<
    'v,
>(
    exe: &str,
    command_line: &BazelCcCompileCommandLine<'v>,
    heap: Heap<'v>,
) -> starlark::Result<ExpandedCommandLineDigest> {
    let mut command_line_digest = ExpandedCommandLineFingerprinter::new();

    command_line_digest.push_arg(bazel_normalize_buck_owned_exec_paths(exe));
    command_line_digest.push_count();

    command_line.visit_argument_strings(heap, &mut |arg| {
        command_line_digest.push_arg(bazel_normalize_buck_owned_exec_paths(&arg));
        Ok(())
    })?;
    command_line_digest.push_count();

    let env = command_line.environment_strings(heap)?;
    let mut env = env.into_iter().collect::<SortedVectorMap<_, _>>();
    bazel_normalize_command_env(&mut env, false);
    for (key, value) in env {
        command_line_digest.push_arg(key);
        command_line_digest.push_arg(value);
    }
    command_line_digest.push_count();

    command_line_digest.push_count();
    Ok(command_line_digest.finalize())
}

fn expand_bazel_cc_compile_command_line(
    command_line: &FrozenBazelCcCompileCommandLine,
) -> bz_error::Result<(Vec<String>, Vec<(String, String)>)> {
    Heap::temp(|heap| {
        let args = command_line.argument_strings(heap)?;
        let env = command_line.environment_strings(heap)?;
        starlark::Result::Ok((args, env))
    })
    .map_err(bz_error::Error::from)
}

struct EmptyArtifactPathMapper;

impl ArtifactPathMapper for EmptyArtifactPathMapper {
    fn get(&self, _artifact: &Artifact) -> Option<&ContentBasedPathHash> {
        None
    }
}

struct BazelLocalActionCachePrecomputeContext {
    base_bazel_exec_path: String,
    digest_config: DigestConfig,
    param_file_digests: Vec<TrackedFileDigest>,
}

impl BazelLocalActionCachePrecomputeContext {
    fn new(base_bazel_exec_path: String, digest_config: DigestConfig) -> Self {
        Self {
            base_bazel_exec_path,
            digest_config,
            param_file_digests: Vec::new(),
        }
    }

    fn add_param_file_digest(
        &mut self,
        digest: TrackedFileDigest,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        let index = self.param_file_digests.len();
        let command_line_path = RelativePathBuf::from(derive_param_file_exec_path(
            &self.base_bazel_exec_path,
            index,
        ));
        self.param_file_digests.push(digest);
        Ok(CommandLineLocation::from_relative_path(
            command_line_path,
            PathSeparatorKind::system_default(),
        ))
    }
}

impl CommandLineContext for BazelLocalActionCachePrecomputeContext {
    fn resolve_project_path(
        &self,
        _path: ProjectRelativePathBuf,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        Err(internal_error!(
            "project-relative paths are not supported in Bazel local action cache precompute"
        ))
    }

    fn fs(&self) -> &ExecutorFs<'_> {
        panic!("ExecutorFs is not available during Bazel local action cache precompute")
    }

    fn resolve_artifact(
        &self,
        artifact: &Artifact,
        _artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        Ok(CommandLineLocation::from_relative_path(
            RelativePathBuf::from(bazel_artifact_path(artifact.get_path())),
            PathSeparatorKind::system_default(),
        ))
    }

    fn resolve_output_artifact(
        &self,
        artifact: &Artifact,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        self.resolve_artifact(artifact, &EmptyArtifactPathMapper)
    }

    fn resolve_cell_path(&self, _path: CellPathRef) -> bz_error::Result<CommandLineLocation<'_>> {
        Err(internal_error!(
            "cell paths are not supported in Bazel local action cache precompute"
        ))
    }

    fn next_macro_file_path(&mut self) -> bz_error::Result<RelativePathBuf> {
        Err(internal_error!(
            "write-to-file macros are not supported in Bazel local action cache precompute"
        ))
    }

    fn add_param_file(&mut self, content: Vec<u8>) -> bz_error::Result<CommandLineLocation<'_>> {
        let digest =
            TrackedFileDigest::from_content(&content, self.digest_config.cas_digest_config());
        self.add_param_file_digest(digest)
    }

    fn add_param_file_args(
        &mut self,
        args: Vec<String>,
        format: ParamFileFormat,
    ) -> bz_error::Result<CommandLineLocation<'_>> {
        let mut digester = FileDigest::digester(self.digest_config.cas_digest_config());
        visit_bazel_param_file_content(args.iter(), format, |bytes| {
            digester.update(bytes);
        });
        let digest =
            TrackedFileDigest::new(digester.finalize(), self.digest_config.cas_digest_config());
        self.add_param_file_digest(digest)
    }

    fn normalize_param_file_arg(&self, arg: String) -> String {
        bazel_normalize_buck_owned_exec_paths(&arg)
    }
}

pub(crate) fn precompute_bazel_local_action_cache_command_line_digest_for_cmd_args<'v>(
    exe: &StarlarkCmdArgs<'v>,
    args: &StarlarkCmdArgs<'v>,
    env: &[(&'v str, &'v str)],
    outputs: &BuckIndexSet<OutputArtifact<'v>>,
    digest_config: DigestConfig,
) -> Option<ExpandedCommandLineDigest> {
    let base_output = outputs.iter().next()?;
    let base_bazel_exec_path =
        bazel_normalize_buck_owned_exec_paths(&bazel_artifact_path(base_output.get_path()));
    let mut ctx = BazelLocalActionCachePrecomputeContext::new(base_bazel_exec_path, digest_config);
    let artifact_path_mapping = EmptyArtifactPathMapper;
    let mut command_line_digest = ExpandedCommandLineFingerprinter::new();

    {
        let mut command_line_builder = LocalActionCacheCommandLineFingerprinter {
            inner: &mut command_line_digest,
            bazel_paths: true,
            bazel_path_mapping: false,
        };
        exe.add_to_command_line(&mut command_line_builder, &mut ctx, &artifact_path_mapping)
            .ok()?;
    }
    command_line_digest.push_count();

    {
        let mut command_line_builder = LocalActionCacheCommandLineFingerprinter {
            inner: &mut command_line_digest,
            bazel_paths: true,
            bazel_path_mapping: false,
        };
        args.add_to_command_line(&mut command_line_builder, &mut ctx, &artifact_path_mapping)
            .ok()?;
    }
    command_line_digest.push_count();

    let mut env = env
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect::<SortedVectorMap<_, _>>();
    bazel_normalize_command_env(&mut env, false);
    for (key, value) in env {
        command_line_digest.push_arg(key);
        command_line_digest.push_arg(value);
    }
    command_line_digest.push_count();

    for digest in &ctx.param_file_digests {
        fingerprint_param_file_digest(&mut command_line_digest, digest);
    }
    command_line_digest.push_count();
    Some(command_line_digest.finalize())
}

fn bazel_path_mapping_enabled_value(bazel_paths: bool, supports_bazel_path_mapping: bool) -> bool {
    bazel_paths && supports_bazel_path_mapping
}

fn bazel_strip_output_path_config_segments(value: &str, marker: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(marker_offset) = value[cursor..].find(marker) {
        let marker_start = cursor + marker_offset;
        let config_start = marker_start + marker.len();
        let config_len = value[config_start..]
            .find('/')
            .unwrap_or(value.len() - config_start);
        let config_end = config_start + config_len;
        let config = &value[config_start..config_end];
        if !config.is_empty()
            && config
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            result.push_str(&value[cursor..config_start]);
            result.push_str("cfg");
            cursor = config_end;
        } else {
            result.push_str(&value[cursor..config_start]);
            cursor = config_start;
        }
    }
    result.push_str(&value[cursor..]);
    result
}

fn bazel_strip_buck_output_path_config_segments(value: &str) -> String {
    let mut value = Cow::Borrowed(value);
    if value.contains("buck-out/bin/") {
        value = Cow::Owned(bazel_strip_output_path_config_segments(
            &value,
            "buck-out/bin/",
        ));
    }
    if value.contains("buck-out/genfiles/") {
        value = Cow::Owned(bazel_strip_output_path_config_segments(
            &value,
            "buck-out/genfiles/",
        ));
    }
    value.into_owned()
}

fn bazel_normalize_and_map_buck_owned_exec_paths(value: &str, path_mapping: bool) -> String {
    let value = bazel_normalize_buck_owned_exec_paths(value);
    if path_mapping {
        bazel_strip_buck_output_path_config_segments(&value)
    } else {
        value
    }
}

fn bazel_normalize_command_line(args: &mut [String], path_mapping: bool) {
    for arg in args {
        *arg = bazel_normalize_and_map_buck_owned_exec_paths(arg, path_mapping);
    }
}

fn bazel_normalize_command_env(env: &mut SortedVectorMap<String, String>, path_mapping: bool) {
    for (_, value) in env.iter_mut() {
        *value = bazel_normalize_and_map_buck_owned_exec_paths(value, path_mapping);
    }
}

const BAZEL_ACTION_ENV: &str = "//command_line_option:action_env";
const BAZEL_HOST_ACTION_ENV: &str = "//command_line_option:host_action_env";
const BAZEL_STRICT_ACTION_ENV: &str = "//command_line_option:incompatible_strict_action_env";

fn bazel_action_env_values(target: &ActionExecutionTarget<'_>, key: &str) -> Vec<String> {
    let Some(label) = target.owner().configured_label() else {
        return Vec::new();
    };
    let Ok(data) = label.cfg().data() else {
        return Vec::new();
    };
    match data.build_settings.get(key) {
        Some(BazelBuildSettingValue::String(value)) => vec![value.clone()],
        Some(BazelBuildSettingValue::StringList(values)) => values.clone(),
        _ => Vec::new(),
    }
}

fn bazel_action_env_bool(target: &ActionExecutionTarget<'_>, key: &str, default: bool) -> bool {
    let Some(label) = target.owner().configured_label() else {
        return default;
    };
    let Ok(data) = label.cfg().data() else {
        return default;
    };
    match data.build_settings.get(key) {
        Some(BazelBuildSettingValue::Bool(value)) => *value,
        Some(BazelBuildSettingValue::String(value)) => {
            matches!(value.as_str(), "true" | "True" | "1")
        }
        _ => default,
    }
}

fn bazel_action_uses_exec_configuration(target: &ActionExecutionTarget<'_>) -> bool {
    target
        .owner()
        .configured_label()
        .and_then(|label| label.cfg().label().ok().map(str::to_owned))
        .is_some_and(|label| label.starts_with("bazeltr-"))
}

fn bazel_fixed_default_shell_env(
    target: ActionExecutionTarget<'_>,
) -> SortedVectorMap<String, String> {
    let strict_action_env = bazel_action_env_bool(&target, BAZEL_STRICT_ACTION_ENV, true);
    let mut env = BTreeMap::new();
    if strict_action_env {
        if cfg!(windows) {
            env.insert("PATH".to_owned(), "c:/msys64/usr/bin".to_owned());
        } else {
            env.insert(
                "PATH".to_owned(),
                "/bin:/usr/bin:/sbin:/usr/sbin".to_owned(),
            );
        }
    }
    env.insert("LC_CTYPE".to_owned(), "C.UTF-8".to_owned());

    let key = if bazel_action_uses_exec_configuration(&target) {
        BAZEL_HOST_ACTION_ENV
    } else {
        BAZEL_ACTION_ENV
    };
    for entry in bazel_action_env_values(&target, key) {
        if let Some(name) = entry.strip_prefix('=') {
            env.remove(name);
        } else if let Some((name, value)) = entry.split_once('=') {
            env.insert(name.to_owned(), value.to_owned());
        } else {
            env.remove(entry.as_str());
        }
    }

    env.into_iter().collect()
}

pub(crate) fn action_cache_add_bytes(fingerprint: &mut DataDigester, bytes: &[u8]) {
    fingerprint.update(&(bytes.len() as u64).to_le_bytes());
    fingerprint.update(bytes);
}

pub(crate) fn action_cache_add_str(fingerprint: &mut DataDigester, value: &str) {
    action_cache_add_bytes(fingerprint, value.as_bytes());
}

pub(crate) fn action_cache_add_bool(fingerprint: &mut DataDigester, value: bool) {
    fingerprint.update(&[value as u8]);
}

pub(crate) fn action_cache_add_u64(fingerprint: &mut DataDigester, value: u64) {
    fingerprint.update(&value.to_le_bytes());
}

fn action_cache_add_option_usize(fingerprint: &mut DataDigester, value: Option<usize>) {
    match value {
        Some(value) => {
            action_cache_add_bool(fingerprint, true);
            action_cache_add_u64(fingerprint, value as u64);
        }
        None => action_cache_add_bool(fingerprint, false),
    }
}

fn action_cache_add_option_bool(fingerprint: &mut DataDigester, value: Option<bool>) {
    match value {
        Some(value) => {
            action_cache_add_bool(fingerprint, true);
            action_cache_add_bool(fingerprint, value);
        }
        None => action_cache_add_bool(fingerprint, false),
    }
}

fn action_cache_add_option_duration(fingerprint: &mut DataDigester, value: Option<Duration>) {
    match value {
        Some(value) => {
            action_cache_add_bool(fingerprint, true);
            action_cache_add_u64(fingerprint, value.as_secs());
            action_cache_add_u64(fingerprint, u64::from(value.subsec_nanos()));
        }
        None => action_cache_add_bool(fingerprint, false),
    }
}

fn action_cache_add_tracked_file_digest(
    fingerprint: &mut DataDigester,
    digest: &TrackedFileDigest,
) {
    let raw_digest = digest.raw_digest();
    fingerprint.update(&[raw_digest.algorithm() as u8]);
    action_cache_add_bytes(fingerprint, raw_digest.as_bytes());
    action_cache_add_u64(fingerprint, digest.size());
}

fn action_cache_add_option_tracked_file_digest(
    fingerprint: &mut DataDigester,
    digest: Option<&TrackedFileDigest>,
) {
    match digest {
        Some(digest) => {
            action_cache_add_bool(fingerprint, true);
            action_cache_add_tracked_file_digest(fingerprint, digest);
        }
        None => action_cache_add_bool(fingerprint, false),
    }
}

pub(crate) fn action_cache_add_debug(fingerprint: &mut DataDigester, value: impl std::fmt::Debug) {
    action_cache_add_str(fingerprint, &format!("{value:?}"));
}

fn action_cache_add_output_type(fingerprint: &mut DataDigester, output_type: OutputType) {
    let value = match output_type {
        OutputType::FileOrDirectory => 0,
        OutputType::File => 1,
        OutputType::Directory => 2,
        OutputType::Symlink => 3,
    };
    fingerprint.update(&[value]);
}

fn action_cache_add_output_creation_behavior(
    fingerprint: &mut DataDigester,
    behavior: OutputCreationBehavior,
) {
    let value = match behavior {
        OutputCreationBehavior::Create => 0,
        OutputCreationBehavior::Parent => 1,
    };
    fingerprint.update(&[value]);
}

fn action_cache_add_worker_id(fingerprint: &mut DataDigester, worker_id: WorkerId) {
    action_cache_add_u64(fingerprint, worker_id.0);
}

fn action_cache_add_worker_protocol(fingerprint: &mut DataDigester, protocol: WorkerProtocol) {
    let value = match protocol {
        WorkerProtocol::Buck2 => 0,
        WorkerProtocol::Bazel => 1,
    };
    fingerprint.update(&[value]);
}

pub(crate) fn fingerprint_artifact_group_values(
    fingerprint: &mut DataDigester,
    fs: &ArtifactFs,
    values: &dyn ArtifactGroupValuesDyn,
    executable_paths: Option<&[ProjectRelativePathBuf]>,
) -> bz_error::Result<()> {
    if executable_paths.is_none()
        && let Some(bytes) = values.action_cache_fingerprint()
    {
        fingerprint.update(bytes);
        return Ok(());
    }

    action_cache_add_str(fingerprint, "artifact_group");
    if executable_paths.is_none()
        && let Some((directory_fingerprint, directory_size)) =
            values.directory_fingerprint_for_action_cache()
    {
        action_cache_add_str(fingerprint, "directory");
        action_cache_add_tracked_file_digest(fingerprint, directory_fingerprint);
        action_cache_add_u64(fingerprint, directory_size);
        return Ok(());
    }
    for (artifact, value) in values.iter() {
        let path = artifact.resolve_path(
            fs,
            if artifact.has_content_based_path() {
                Some(value.content_based_path_hash())
            } else {
                None
            }
            .as_ref(),
        )?;
        action_cache_add_str(fingerprint, path.as_str());
        if executable_paths.is_some_and(|paths| paths.iter().any(|executable| executable == &path))
        {
            value
                .with_executable_bit(true)
                .hash_action_cache_fingerprint(fingerprint);
        } else {
            value.hash_action_cache_fingerprint(fingerprint);
        }
    }
    Ok(())
}

fn fingerprint_command_execution_input(
    fingerprint: &mut DataDigester,
    fs: &ArtifactFs,
    input: &CommandExecutionInput,
) -> bz_error::Result<()> {
    match input {
        CommandExecutionInput::Artifact(values) => {
            fingerprint_artifact_group_values(fingerprint, fs, values.as_ref(), None)?;
        }
        CommandExecutionInput::ArtifactWithExecutableOverrides {
            group,
            executable_paths,
        } => {
            fingerprint_artifact_group_values(
                fingerprint,
                fs,
                group.as_ref(),
                Some(executable_paths),
            )?;
        }
        CommandExecutionInput::ArtifactPathAlias {
            source_path,
            source_requires_materialization,
            path,
            value,
            ..
        } => {
            action_cache_add_str(fingerprint, "artifact_path_alias");
            action_cache_add_str(fingerprint, source_path.as_str());
            action_cache_add_bool(fingerprint, *source_requires_materialization);
            action_cache_add_str(fingerprint, path.as_str());
            value.hash_action_cache_fingerprint(fingerprint);
        }
        CommandExecutionInput::EmptyFile(path) => {
            action_cache_add_str(fingerprint, "empty_file");
            action_cache_add_str(fingerprint, path.as_str());
        }
        CommandExecutionInput::SyntheticFile { path, content } => {
            action_cache_add_str(fingerprint, "synthetic_file");
            action_cache_add_str(fingerprint, path.as_str());
            action_cache_add_bytes(fingerprint, content);
        }
        CommandExecutionInput::ActionMetadata(metadata) => {
            action_cache_add_str(fingerprint, "action_metadata");
            action_cache_add_tracked_file_digest(fingerprint, &metadata.digest);
            action_cache_add_str(fingerprint, metadata.path.path().as_str());
            action_cache_add_str(fingerprint, metadata.content_hash.as_str());
        }
        CommandExecutionInput::ScratchPath(path) => {
            action_cache_add_str(fingerprint, "scratch_path");
            action_cache_add_debug(fingerprint, path);
        }
        CommandExecutionInput::IncrementalRemoteOutput(path, entry) => {
            action_cache_add_str(fingerprint, "incremental_remote_output");
            action_cache_add_str(fingerprint, path.as_str());
            action_cache_add_debug(fingerprint, entry);
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct LocalActionCacheInputMetadata<'a> {
    input_set_digest: &'a [u8],
    extra_inputs: &'a [CommandExecutionInput],
}

pub(crate) fn fingerprint_command_execution_output(
    fingerprint: &mut DataDigester,
    fs: &ArtifactFs,
    output: &CommandExecutionOutput,
) -> bz_error::Result<()> {
    let resolved_path = output
        .as_ref()
        .resolve(fs, Some(&ContentBasedPathHash::for_output_artifact()))?
        .into_path();
    action_cache_add_str(fingerprint, "output");
    action_cache_add_str(fingerprint, resolved_path.as_str());
    match output {
        CommandExecutionOutput::BuildArtifact {
            output_type,
            produced_path,
            ..
        } => {
            action_cache_add_str(fingerprint, "build_artifact");
            action_cache_add_output_type(fingerprint, *output_type);
            if let Some(produced_path) = produced_path {
                action_cache_add_bool(fingerprint, true);
                action_cache_add_str(fingerprint, produced_path.as_str());
            } else {
                action_cache_add_bool(fingerprint, false);
            }
        }
        CommandExecutionOutput::TestPath { create, .. } => {
            action_cache_add_str(fingerprint, "test_path");
            action_cache_add_output_creation_behavior(fingerprint, *create);
        }
    }
    Ok(())
}

pub(crate) fn finalize_action_cache_digest(fingerprint: DataDigester) -> Vec<u8> {
    fingerprint.finalize().raw_digest().as_bytes().to_vec()
}

pub(crate) fn compose_local_action_cache_fingerprint(
    cas_digest_config: CasDigestConfig,
    action_key_digest: &[u8],
    input_metadata_digest: &[u8],
) -> Vec<u8> {
    let mut fingerprint = CasDigestData::digester(cas_digest_config);
    action_cache_add_str(&mut fingerprint, "buck2-local-action-cache-entry-v1");
    action_cache_add_bytes(&mut fingerprint, action_key_digest);
    action_cache_add_bytes(&mut fingerprint, input_metadata_digest);
    finalize_action_cache_digest(fingerprint)
}

struct RunActionOutputFilteringArtifactVisitor<'a, 'v> {
    outputs: &'a BoxSliceSet<BuildArtifact>,
    inner: &'a mut dyn CommandLineArtifactVisitor<'v>,
}

impl<'a, 'v> RunActionOutputFilteringArtifactVisitor<'a, 'v> {
    fn new(
        outputs: &'a BoxSliceSet<BuildArtifact>,
        inner: &'a mut dyn CommandLineArtifactVisitor<'v>,
    ) -> Self {
        Self { outputs, inner }
    }
}

impl<'v> CommandLineArtifactVisitor<'v> for RunActionOutputFilteringArtifactVisitor<'_, 'v> {
    fn visit_input(&mut self, input: ArtifactGroup, tags: Vec<&ArtifactTag>) {
        if !artifact_group_is_run_action_output(self.outputs, &input) {
            self.inner.visit_input(input, tags);
        }
    }

    fn visit_declared_output(&mut self, artifact: OutputArtifact<'v>, tags: Vec<&ArtifactTag>) {
        self.inner.visit_declared_output(artifact, tags);
    }

    fn visit_frozen_output(&mut self, artifact: Artifact, tags: Vec<&ArtifactTag>) {
        if !artifact_is_run_action_output(self.outputs, &artifact) {
            self.inner.visit_frozen_output(artifact, tags);
        }
    }

    fn push_frame(&mut self) -> bz_error::Result<()> {
        self.inner.push_frame()
    }

    fn pop_frame(&mut self) {
        self.inner.pop_frame()
    }

    fn skip_hidden(&self) -> bool {
        self.inner.skip_hidden()
    }
}

type ExpandedCommandLineDigestForDepFiles = ExpandedCommandLineDigest;

/// A CommandLineArtifactVisitor that gathers non-hidden inputs.
pub struct SkipHiddenCommandLineArtifactVisitor {
    pub inputs: BuckIndexSet<ArtifactGroup>,
}

impl SkipHiddenCommandLineArtifactVisitor {
    pub fn new() -> Self {
        Self {
            inputs: BuckIndexSet::default(),
        }
    }
}

impl CommandLineArtifactVisitor<'_> for SkipHiddenCommandLineArtifactVisitor {
    fn visit_input(&mut self, input: ArtifactGroup, _tags: Vec<&ArtifactTag>) {
        self.inputs.insert(input);
    }

    fn visit_declared_output(&mut self, _artifact: OutputArtifact<'_>, _tags: Vec<&ArtifactTag>) {}

    fn visit_frozen_output(&mut self, _artifact: Artifact, _tags: Vec<&ArtifactTag>) {}

    fn skip_hidden(&self) -> bool {
        true
    }
}

struct BazelOutputExecPathVisitor<'a> {
    outputs: &'a BoxSliceSet<BuildArtifact>,
    paths: &'a mut BuckIndexMap<BuildArtifactPath, String>,
    bazel_path_mapping: bool,
}

impl<'a> BazelOutputExecPathVisitor<'a> {
    fn new(
        outputs: &'a BoxSliceSet<BuildArtifact>,
        paths: &'a mut BuckIndexMap<BuildArtifactPath, String>,
        bazel_path_mapping: bool,
    ) -> Self {
        Self {
            outputs,
            paths,
            bazel_path_mapping,
        }
    }

    fn record_path(&mut self, path: bz_execute::path::artifact_path::ArtifactPath<'_>) {
        let build_path = match path.base_path.as_ref() {
            Either::Left(build_path) => (**build_path).dupe(),
            Either::Right(_) => return,
        };
        self.paths.insert(
            build_path,
            bazel_normalize_and_map_buck_owned_exec_paths(
                &bazel_artifact_path(path),
                self.bazel_path_mapping,
            ),
        );
    }
}

impl<'v> CommandLineArtifactVisitor<'v> for BazelOutputExecPathVisitor<'_> {
    fn visit_input(&mut self, _input: ArtifactGroup, _tags: Vec<&ArtifactTag>) {}

    fn visit_declared_output(&mut self, artifact: OutputArtifact<'v>, _tags: Vec<&ArtifactTag>) {
        self.record_path(artifact.get_path());
    }

    fn visit_frozen_output(&mut self, artifact: Artifact, _tags: Vec<&ArtifactTag>) {
        if artifact_is_run_action_output(self.outputs, &artifact) {
            self.record_path(artifact.get_path());
        }
    }
}

fn visit_run_action_command_line_artifacts<'v>(
    outputs: &BoxSliceSet<BuildArtifact>,
    command_line: &dyn CommandLineArgLike<'v>,
    artifact_visitor: &mut dyn CommandLineArtifactVisitor<'v>,
) -> bz_error::Result<()> {
    let mut artifact_visitor =
        RunActionOutputFilteringArtifactVisitor::new(outputs, artifact_visitor);
    command_line.visit_artifacts(&mut artifact_visitor)
}

struct BazelRunfilesEntry<'v> {
    path: &'v str,
    target_file: Value<'v>,
}

struct BazelToolRunfiles<'v> {
    executable: Value<'v>,
    runfiles: Value<'v>,
}

fn bazel_runfiles_entries<'v>(
    runfiles: Value<'v>,
) -> bz_error::Result<impl Iterator<Item = bz_error::Result<BazelRunfilesEntry<'v>>> + 'v> {
    let entries = ListRef::from_value(runfiles)
        .ok_or_else(|| internal_error!("Bazel executable runfiles should be a list"))?;
    Ok(entries.iter().map(|entry| {
        let entry = StructRef::from_value(entry)
            .ok_or_else(|| internal_error!("Bazel executable runfiles entry should be a struct"))?;
        let mut path = None;
        let mut target_file = None;
        for (name, value) in entry.iter() {
            match name.as_str() {
                "path" => path = value.unpack_str(),
                "target_file" => target_file = Some(value),
                _ => {}
            }
        }
        let path = path.ok_or_else(|| {
            internal_error!("Bazel executable runfiles entry should have string field `path`")
        })?;
        let target_file = target_file.ok_or_else(|| {
            internal_error!("Bazel executable runfiles entry should have field `target_file`")
        })?;
        Ok(BazelRunfilesEntry { path, target_file })
    }))
}

fn bazel_tool_runfiles<'v>(
    tool_runfiles: Value<'v>,
) -> bz_error::Result<impl Iterator<Item = bz_error::Result<BazelToolRunfiles<'v>>> + 'v> {
    let tools = ListRef::from_value(tool_runfiles)
        .ok_or_else(|| internal_error!("Bazel tool runfiles should be a list"))?;
    Ok(tools.iter().map(|tool| {
        let tool = StructRef::from_value(tool)
            .ok_or_else(|| internal_error!("Bazel tool runfiles entry should be a struct"))?;
        let mut executable = None;
        let mut runfiles = None;
        for (name, value) in tool.iter() {
            match name.as_str() {
                "executable" => executable = Some(value),
                "runfiles" => runfiles = Some(value),
                _ => {}
            }
        }
        Ok(BazelToolRunfiles {
            executable: executable.ok_or_else(|| {
                internal_error!("Bazel tool runfiles entry should have field `executable`")
            })?,
            runfiles: runfiles.ok_or_else(|| {
                internal_error!("Bazel tool runfiles entry should have field `runfiles`")
            })?,
        })
    }))
}

fn visit_bazel_runfiles_artifacts<'v>(
    runfiles: Value<'v>,
    artifact_visitor: &mut dyn CommandLineArtifactVisitor<'v>,
) -> bz_error::Result<()> {
    if artifact_visitor.skip_hidden() {
        return Ok(());
    }
    for entry in bazel_runfiles_entries(runfiles)? {
        let entry = entry?;
        let artifact =
            ValueAsInputArtifactLike::unpack_value(entry.target_file)?.ok_or_else(|| {
                internal_error!("Bazel executable runfiles target_file should be File")
            })?;
        artifact_visitor.visit_input(artifact.0.get_artifact_group()?, Vec::new());
    }
    Ok(())
}

fn visit_bazel_tool_runfiles_artifacts<'v>(
    tool_runfiles: Value<'v>,
    artifact_visitor: &mut dyn CommandLineArtifactVisitor<'v>,
) -> bz_error::Result<()> {
    if artifact_visitor.skip_hidden() {
        return Ok(());
    }
    for tool in bazel_tool_runfiles(tool_runfiles)? {
        let tool = tool?;
        let executable = ValueAsInputArtifactLike::unpack_value(tool.executable)?
            .ok_or_else(|| internal_error!("Bazel tool runfiles executable should be File"))?;
        artifact_visitor.visit_input(executable.0.get_artifact_group()?, Vec::new());
        visit_bazel_runfiles_artifacts(tool.runfiles, artifact_visitor)?;
    }
    Ok(())
}

impl RunAction {
    fn outputs_use_bazel_execroot_paths(outputs: &BoxSliceSet<BuildArtifact>) -> bool {
        outputs
            .iter()
            .any(|output| output.get_path().bazel_owner().is_some())
    }

    fn values_use_bazel_execroot_paths(
        inner: &UnregisteredRunAction,
        values: &UnpackedRunActionValues<'_>,
        outputs: &BoxSliceSet<BuildArtifact>,
    ) -> bool {
        inner.bazel_use_default_shell_env.is_some()
            || inner.bazel_string_args.is_some()
            || values.bazel_cc_command_line.is_some()
            || Self::outputs_use_bazel_execroot_paths(outputs)
    }

    fn uses_bazel_execroot_paths(&self) -> bool {
        let values = Self::unpack(&self.starlark_values)
            .expect("RunActionValues were validated when the action was registered");
        Self::values_use_bazel_execroot_paths(&self.inner, &values, &self.outputs)
    }

    fn aquery_command(
        &self,
        fs: &ExecutorFs,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> bz_error::Result<String> {
        let mut cli_rendered = Vec::<String>::new();
        let values = Self::unpack(&self.starlark_values)?;
        let uses_bazel_execroot_paths =
            Self::values_use_bazel_execroot_paths(&self.inner, &values, &self.outputs);
        let base_output = self
            .outputs
            .iter()
            .next()
            .ok_or_else(|| internal_error!("RunAction has no outputs for aquery rendering"))?;
        let base_bazel_exec_path = if uses_bazel_execroot_paths {
            let artifact = Artifact::from(base_output.dupe());
            Some(bazel_normalize_buck_owned_exec_paths(&bazel_artifact_path(
                artifact.get_path(),
            )))
        } else {
            None
        };
        let base_path = if let Some(base_bazel_exec_path) = &base_bazel_exec_path {
            bazel_param_file_base_path(base_bazel_exec_path)?
        } else {
            base_output.get_path().path().to_owned()
        };
        let param_files = RunActionParamFilesRef::new(
            base_output.get_path().owner().dupe(),
            base_path,
            base_bazel_exec_path,
            base_output.get_path().bazel_output_root(),
            if self.all_outputs_are_content_based() {
                BuckOutPathKind::ContentHash
            } else {
                BuckOutPathKind::Configuration
            },
            DigestConfig::testing_default(),
        );
        let mut ctx = RunActionCommandLineContext::new(
            fs,
            uses_bazel_execroot_paths,
            false,
            param_files,
            RunActionParamFileMode::Record,
        );
        values
            .exe
            .add_to_command_line(&mut cli_rendered, &mut ctx, artifact_path_mapping)?;
        if let Some(args) = &self.inner.bazel_string_args {
            cli_rendered.extend(args.iter().cloned());
        } else {
            values
                .args
                .add_to_command_line(&mut cli_rendered, &mut ctx, artifact_path_mapping)?;
        }
        Ok(format!("[{}]", cli_rendered.iter().join(", ")))
    }

    fn bazel_execroot(fs: &ArtifactFs) -> ProjectRelativePathBuf {
        fs.buck_out_path_resolver()
            .root()
            .join(ForwardRelativePathBuf::unchecked_new(
                "__bazel_execroot".to_owned(),
            ))
    }

    fn bazel_action_execroot(
        &self,
        fs: &ArtifactFs,
        target: ActionExecutionTarget<'_>,
    ) -> bz_error::Result<ProjectRelativePathBuf> {
        let values = Self::unpack(&self.starlark_values)?;
        if values.worker.is_some() || values.remote_worker.is_some() {
            return Ok(Self::bazel_execroot(fs));
        }

        let mut hasher = Sha1::new();
        hasher.update(RunActionKey::from_action_execution_target(target).to_string());
        for output in &self.outputs {
            hasher.update(b"\0");
            hasher.update(output.get_path().to_string());
        }
        let digest = hasher.finalize();
        let id = hex::encode(&digest[..8]);

        Ok(Self::bazel_execroot(fs).join(ForwardRelativePathBuf::unchecked_new(id)))
    }

    fn bazel_execroot_path(
        bazel_execroot: &ProjectRelativePath,
        path: String,
    ) -> bz_error::Result<ProjectRelativePathBuf> {
        let path = ForwardRelativePathBuf::try_from(path)
            .buck_error_context("Invalid Bazel execroot path")?;
        Ok(bazel_execroot.join(path))
    }

    fn collect_inputs(
        inner: &UnregisteredRunAction,
        starlark_values: &OwnedFrozenValueTyped<FrozenStarlarkRunActionValues>,
        outputs: &BoxSliceSet<BuildArtifact>,
    ) -> bz_error::Result<(
        Box<[ArtifactGroup]>,
        Box<[ArtifactGroup]>,
        Box<[ArtifactGroup]>,
    )> {
        let mut artifact_visitor = SimpleCommandLineArtifactVisitor::new();
        Self::visit_artifacts_for(inner, starlark_values, outputs, &mut artifact_visitor)?;
        let inputs = artifact_visitor
            .inputs
            .into_iter()
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let command_inputs = if inner.dep_files.is_empty() && inner.metadata_param.is_none() {
            let mut command_input_visitor = DepFilesCommandLineVisitor::new(&inner.dep_files);
            Self::visit_command_artifacts_for(
                inner,
                starlark_values,
                outputs,
                &mut command_input_visitor,
            )?;
            let mut command_inputs = BuckIndexSet::default();
            command_input_visitor
                .inputs
                .iter()
                .flat_map(|inputs| inputs.iter())
                .for_each(|input| {
                    command_inputs.insert(input.dupe());
                });
            command_inputs
                .into_iter()
                .collect::<Vec<_>>()
                .into_boxed_slice()
        } else {
            Box::default()
        };

        let mut non_hidden_visitor = SkipHiddenCommandLineArtifactVisitor::new();
        Self::visit_artifacts_for(inner, starlark_values, outputs, &mut non_hidden_visitor)?;
        let non_hidden_inputs = non_hidden_visitor
            .inputs
            .into_iter()
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Ok((inputs, command_inputs, non_hidden_inputs))
    }

    fn visit_artifacts_for<'a>(
        inner: &'a UnregisteredRunAction,
        starlark_values: &'a OwnedFrozenValueTyped<FrozenStarlarkRunActionValues>,
        outputs: &'a BoxSliceSet<BuildArtifact>,
        artifact_visitor: &mut dyn CommandLineArtifactVisitor<'a>,
    ) -> bz_error::Result<()> {
        let values = Self::unpack(starlark_values)?;
        if Self::values_use_bazel_execroot_paths(inner, &values, outputs) {
            if let Some(bazel_inputs) = values.bazel_inputs {
                visit_run_action_command_line_artifacts(outputs, bazel_inputs, artifact_visitor)?;
            }
            visit_run_action_command_line_artifacts(outputs, values.exe, artifact_visitor)?;
            visit_run_action_command_line_artifacts(outputs, values.args, artifact_visitor)?;
            if let Some(runfiles) = values.bazel_executable_runfiles {
                visit_bazel_runfiles_artifacts(runfiles, artifact_visitor)?;
            }
            if let Some(tool_runfiles) = values.bazel_tool_runfiles {
                visit_bazel_tool_runfiles_artifacts(tool_runfiles, artifact_visitor)?;
            }
        } else {
            visit_run_action_command_line_artifacts(outputs, values.args, artifact_visitor)?;
            visit_run_action_command_line_artifacts(outputs, values.exe, artifact_visitor)?;
        }
        if let Some(worker) = values.worker {
            visit_run_action_command_line_artifacts(outputs, worker.exe, artifact_visitor)?;
        }
        if let Some(remote_worker) = values.remote_worker {
            visit_run_action_command_line_artifacts(outputs, remote_worker.exe, artifact_visitor)?;
            for (_, v) in remote_worker.env.iter() {
                visit_run_action_command_line_artifacts(outputs, *v, artifact_visitor)?;
            }
        }
        for (_, v) in values.env.iter() {
            visit_run_action_command_line_artifacts(outputs, *v, artifact_visitor)?;
        }
        Ok(())
    }

    fn visit_command_artifacts_for<'a>(
        inner: &'a UnregisteredRunAction,
        starlark_values: &'a OwnedFrozenValueTyped<FrozenStarlarkRunActionValues>,
        outputs: &'a BoxSliceSet<BuildArtifact>,
        artifact_visitor: &mut dyn CommandLineArtifactVisitor<'a>,
    ) -> bz_error::Result<()> {
        let values = Self::unpack(starlark_values)?;
        visit_run_action_command_line_artifacts(outputs, values.exe, artifact_visitor)?;
        visit_run_action_command_line_artifacts(outputs, values.args, artifact_visitor)?;
        if Self::values_use_bazel_execroot_paths(inner, &values, outputs) {
            if let Some(bazel_inputs) = values.bazel_inputs {
                visit_run_action_command_line_artifacts(outputs, bazel_inputs, artifact_visitor)?;
            }
            if let Some(runfiles) = values.bazel_executable_runfiles {
                visit_bazel_runfiles_artifacts(runfiles, artifact_visitor)?;
            }
            if let Some(tool_runfiles) = values.bazel_tool_runfiles {
                visit_bazel_tool_runfiles_artifacts(tool_runfiles, artifact_visitor)?;
            }
        }
        for (_, v) in values.env.iter() {
            visit_run_action_command_line_artifacts(outputs, *v, artifact_visitor)?;
        }
        Ok(())
    }

    fn unpack<'v>(
        values: &'v OwnedFrozenValueTyped<FrozenStarlarkRunActionValues>,
    ) -> bz_error::Result<UnpackedRunActionValues<'v>> {
        let exe: &dyn CommandLineArgLike = &*values.exe;
        let args: &dyn CommandLineArgLike = &*values.args;
        let env = match values.env {
            None => Vec::new(),
            Some(env) => {
                let d = DictRef::from_value(env.to_value().get())
                    .ok_or_else(|| internal_error!("expecting dict"))?;
                let mut res = Vec::with_capacity(d.len());
                for (k, v) in d.iter() {
                    res.push((
                        k.unpack_str()
                            .ok_or_else(|| internal_error!("expecting string"))?,
                        ValueAsCommandLineLike::unpack_value_err(v)?.0,
                    ));
                }
                res
            }
        };
        let worker: Option<&WorkerInfo> = values.worker()?.map(|v| v.typed);

        let worker = worker.map(|worker| UnpackedWorkerValues {
            exe: worker.exe_command_line(),
            env: worker.env(),
            id: WorkerId(worker.id),
            concurrency: worker.concurrency(),
            streaming: worker.streaming(),
            supports_bazel_local_persistent_worker_protocol: worker
                .supports_bazel_local_persistent_worker_protocol(),
            supports_bazel_remote_persistent_worker_protocol: worker
                .supports_bazel_remote_persistent_worker_protocol(),
            requires_bazel_worker_sandboxing: worker.requires_bazel_worker_sandboxing(),
        });

        let remote_worker: Option<&WorkerInfo> = values.remote_worker()?.map(|v| v.typed);

        let remote_worker = remote_worker.map(|remote_worker| UnpackedWorkerValues {
            exe: remote_worker.exe_command_line(),
            env: remote_worker.env(),
            id: WorkerId(remote_worker.id),
            concurrency: remote_worker.concurrency(),
            streaming: false,
            supports_bazel_local_persistent_worker_protocol: false,
            supports_bazel_remote_persistent_worker_protocol: false,
            requires_bazel_worker_sandboxing: false,
        });

        Ok(UnpackedRunActionValues {
            exe,
            args,
            bazel_inputs: values
                .bazel_inputs
                .as_ref()
                .map(|bazel_inputs| &**bazel_inputs as &dyn CommandLineArgLike),
            bazel_executable: values
                .bazel_executable
                .map(|executable: FrozenValue| executable.to_value()),
            bazel_executable_runfiles: values
                .bazel_executable_runfiles
                .map(|runfiles: FrozenValue| runfiles.to_value()),
            bazel_tool_runfiles: values
                .bazel_tool_runfiles
                .map(|runfiles: FrozenValue| runfiles.to_value()),
            bazel_cc_command_line: values.bazel_cc_command_line.as_deref(),
            env,
            worker,
            remote_worker,
        })
    }

    /// Get the command line expansion for this RunAction.
    fn prepare_command_line_for_local_action_cache_probe<'v>(
        &'v self,
        action_execution_ctx: &dyn ActionExecutionCtx,
        artifact_visitor: &mut RunActionVisitor<'v>,
        collect_action_inputs: bool,
    ) -> bz_error::Result<(ExpandedCommandLineDigest, Vec<RunActionParamFile>)> {
        let values = Self::unpack(&self.starlark_values)?;
        let bazel_cc_command_line = values
            .bazel_cc_command_line
            .map(expand_bazel_cc_compile_command_line)
            .transpose()?;

        let fs = &action_execution_ctx.executor_fs();
        let bazel_paths = self.uses_bazel_execroot_paths();
        let bazel_path_mapping =
            bazel_path_mapping_enabled_value(bazel_paths, self.inner.supports_bazel_path_mapping);
        let base_output = self
            .outputs
            .iter()
            .next()
            .ok_or_else(|| internal_error!("run actions must have at least one output"))?;
        let base_bazel_exec_path = if bazel_paths {
            let artifact = Artifact::from(base_output.dupe());
            Some(bazel_normalize_and_map_buck_owned_exec_paths(
                &bazel_artifact_path(artifact.get_path()),
                bazel_path_mapping,
            ))
        } else {
            None
        };
        let base_path = if let Some(base_bazel_exec_path) = &base_bazel_exec_path {
            bazel_param_file_base_path(base_bazel_exec_path)?
        } else {
            base_output.get_path().path().to_owned()
        };
        let param_files = RunActionParamFilesRef::new(
            base_output.get_path().owner().dupe(),
            base_path,
            base_bazel_exec_path,
            base_output.get_path().bazel_output_root(),
            if self.all_outputs_are_content_based() {
                BuckOutPathKind::ContentHash
            } else {
                BuckOutPathKind::Configuration
            },
            action_execution_ctx.digest_config(),
        );

        if bazel_paths && collect_action_inputs {
            let mut output_visitor = BazelOutputExecPathVisitor::new(
                &self.outputs,
                &mut artifact_visitor.bazel_output_exec_paths,
                bazel_path_mapping,
            );
            values.exe.visit_artifacts(&mut output_visitor)?;
            if values.bazel_cc_command_line.is_none() {
                values.args.visit_artifacts(&mut output_visitor)?;
            }
            if let Some(bazel_inputs) = values.bazel_inputs {
                bazel_inputs.visit_artifacts(&mut output_visitor)?;
            }
            for (_, value) in &values.env {
                value.visit_artifacts(&mut output_visitor)?;
            }
            if let Some(worker) = &values.worker {
                worker.exe.visit_artifacts(&mut output_visitor)?;
                for (_, value) in &worker.env {
                    value.visit_artifacts(&mut output_visitor)?;
                }
            }
            if let Some(remote_worker) = &values.remote_worker {
                remote_worker.exe.visit_artifacts(&mut output_visitor)?;
                for (_, value) in &remote_worker.env {
                    value.visit_artifacts(&mut output_visitor)?;
                }
            }
        }

        // Creating the artifact_path_mapping isn't free, because we have to iterate TSets.
        // Therefore, only create a mapping if we're going to use it - i.e. if the input
        // is not hidden.
        let input_artifact_group_values;
        let input_artifact_path_mapping = if bazel_paths {
            RunActionInputArtifactPathMapper::Bazel(BazelRunActionArtifactPathMapper::new(
                action_execution_ctx,
                &self.non_hidden_inputs,
            ))
        } else {
            input_artifact_group_values = self
                .non_hidden_inputs
                .iter()
                .map(|group| action_execution_ctx.artifact_values(group))
                .collect::<Vec<_>>();
            RunActionInputArtifactPathMapper::Default(ArtifactPathMapperImpl::from_values(
                input_artifact_group_values.iter().copied(),
            ))
        };
        let artifact_path_mapping =
            RunActionOutputArtifactPathMapper::new(&self.outputs, &input_artifact_path_mapping);

        let mut command_line_digest = ExpandedCommandLineFingerprinter::new();
        if let Some(args) = &self.inner.bazel_string_args {
            push_string_args_for_local_action_cache(
                &mut command_line_digest,
                args,
                bazel_paths,
                bazel_path_mapping,
            );
        } else {
            let mut cli_ctx = RunActionCommandLineContext::new(
                fs,
                bazel_paths,
                bazel_path_mapping,
                param_files.clone(),
                RunActionParamFileMode::RecordDigestOnly,
            );
            let mut command_line_builder = LocalActionCacheCommandLineFingerprinter {
                inner: &mut command_line_digest,
                bazel_paths,
                bazel_path_mapping,
            };
            values.exe.add_to_command_line(
                &mut command_line_builder,
                &mut cli_ctx,
                &artifact_path_mapping,
            )?;
        }
        command_line_digest.push_count();
        if collect_action_inputs {
            visit_run_action_command_line_artifacts(&self.outputs, values.exe, artifact_visitor)?;
        }

        if let Some((args, _)) = &bazel_cc_command_line {
            push_string_args_for_local_action_cache(
                &mut command_line_digest,
                args,
                bazel_paths,
                bazel_path_mapping,
            );
        } else {
            let mut cli_ctx = RunActionCommandLineContext::new(
                fs,
                bazel_paths,
                bazel_path_mapping,
                param_files.clone(),
                RunActionParamFileMode::RecordDigestOnly,
            );
            let mut command_line_builder = LocalActionCacheCommandLineFingerprinter {
                inner: &mut command_line_digest,
                bazel_paths,
                bazel_path_mapping,
            };
            values.args.add_to_command_line(
                &mut command_line_builder,
                &mut cli_ctx,
                &artifact_path_mapping,
            )?;
        }
        command_line_digest.push_count();
        // Bazel actions may execute a pre-expanded command line, but the original args still
        // carry artifacts whose Bazel exec paths must be materialized.
        if collect_action_inputs
            && (bazel_paths
                || (self.inner.bazel_string_args.is_none()
                    && values.bazel_cc_command_line.is_none()))
        {
            visit_run_action_command_line_artifacts(&self.outputs, values.args, artifact_visitor)?;
        }

        if collect_action_inputs && let Some(bazel_inputs) = values.bazel_inputs {
            visit_run_action_command_line_artifacts(&self.outputs, bazel_inputs, artifact_visitor)?;
        }
        if collect_action_inputs && let Some(runfiles) = values.bazel_executable_runfiles {
            visit_bazel_runfiles_artifacts(runfiles, artifact_visitor)?;
        }
        if collect_action_inputs && let Some(tool_runfiles) = values.bazel_tool_runfiles {
            visit_bazel_tool_runfiles_artifacts(tool_runfiles, artifact_visitor)?;
        }

        let explicit_cli_env: bz_error::Result<SortedVectorMap<_, _>> = values
            .env
            .into_iter()
            .map(|(k, v)| {
                let mut env = String::new();
                let mut ctx = RunActionCommandLineContext::new(
                    fs,
                    bazel_paths,
                    bazel_path_mapping,
                    param_files.clone(),
                    RunActionParamFileMode::RecordDigestOnly,
                );
                v.add_to_command_line(
                    &mut SpaceSeparatedCommandLineBuilder::wrap_string(&mut env),
                    &mut ctx,
                    &artifact_path_mapping,
                )?;
                if collect_action_inputs {
                    visit_run_action_command_line_artifacts(&self.outputs, v, artifact_visitor)?;
                }
                Ok((k.to_owned(), env))
            })
            .collect();
        let mut cli_env = if self.inner.bazel_use_default_shell_env == Some(true) {
            bazel_fixed_default_shell_env(action_execution_ctx.target())
        } else {
            SortedVectorMap::new()
        };
        cli_env.extend(explicit_cli_env?);
        if let Some((_, env)) = &bazel_cc_command_line {
            for (key, value) in env {
                cli_env.insert(key.to_owned(), value.to_owned());
            }
        }
        if bazel_paths {
            bazel_normalize_command_env(&mut cli_env, bazel_path_mapping);
        }
        for (k, v) in cli_env {
            command_line_digest.push_arg(k);
            command_line_digest.push_arg(v);
        }
        command_line_digest.push_count();

        let param_files = param_files.files(false)?;
        fingerprint_param_files(&mut command_line_digest, &param_files);

        Ok((command_line_digest.finalize(), param_files))
    }

    fn expand_command_line_and_worker<'v>(
        &'v self,
        action_execution_ctx: &dyn ActionExecutionCtx,
        artifact_visitor: &mut RunActionVisitor<'v>,
        collect_dep_file_digest: bool,
        collect_action_inputs: bool,
    ) -> bz_error::Result<(
        ExpandedCommandLine,
        Option<ExpandedCommandLineDigestForDepFiles>,
        Option<WorkerSpec>,
        Option<RemoteWorkerSpec>,
        Vec<RunActionParamFile>,
    )> {
        let fs = &action_execution_ctx.executor_fs();
        let bazel_paths = self.uses_bazel_execroot_paths();
        let bazel_path_mapping =
            bazel_path_mapping_enabled_value(bazel_paths, self.inner.supports_bazel_path_mapping);
        let base_output = self
            .outputs
            .iter()
            .next()
            .ok_or_else(|| internal_error!("run actions must have at least one output"))?;
        let base_bazel_exec_path = if bazel_paths {
            let artifact = Artifact::from(base_output.dupe());
            Some(bazel_normalize_and_map_buck_owned_exec_paths(
                &bazel_artifact_path(artifact.get_path()),
                bazel_path_mapping,
            ))
        } else {
            None
        };
        let base_path = if let Some(base_bazel_exec_path) = &base_bazel_exec_path {
            bazel_param_file_base_path(base_bazel_exec_path)?
        } else {
            base_output.get_path().path().to_owned()
        };
        let param_files = RunActionParamFilesRef::new(
            base_output.get_path().owner().dupe(),
            base_path,
            base_bazel_exec_path,
            base_output.get_path().bazel_output_root(),
            if self.all_outputs_are_content_based() {
                BuckOutPathKind::ContentHash
            } else {
                BuckOutPathKind::Configuration
            },
            action_execution_ctx.digest_config(),
        );
        let mut cli_ctx = RunActionCommandLineContext::new(
            fs,
            bazel_paths,
            bazel_path_mapping,
            param_files.clone(),
            RunActionParamFileMode::Record,
        );
        let mut cli_digest_ctx = collect_dep_file_digest.then(|| {
            RunActionCommandLineContext::new(
                fs,
                bazel_paths,
                bazel_path_mapping,
                param_files.clone(),
                RunActionParamFileMode::Replay,
            )
        });
        let values = Self::unpack(&self.starlark_values)?;
        let bazel_cc_command_line = values
            .bazel_cc_command_line
            .map(expand_bazel_cc_compile_command_line)
            .transpose()?;
        if bazel_paths && collect_action_inputs {
            let mut output_visitor = BazelOutputExecPathVisitor::new(
                &self.outputs,
                &mut artifact_visitor.bazel_output_exec_paths,
                bazel_path_mapping,
            );
            values.exe.visit_artifacts(&mut output_visitor)?;
            if values.bazel_cc_command_line.is_none() {
                values.args.visit_artifacts(&mut output_visitor)?;
            }
            if let Some(bazel_inputs) = values.bazel_inputs {
                bazel_inputs.visit_artifacts(&mut output_visitor)?;
            }
            for (_, value) in &values.env {
                value.visit_artifacts(&mut output_visitor)?;
            }
            if let Some(worker) = &values.worker {
                worker.exe.visit_artifacts(&mut output_visitor)?;
                for (_, value) in &worker.env {
                    value.visit_artifacts(&mut output_visitor)?;
                }
            }
            if let Some(remote_worker) = &values.remote_worker {
                remote_worker.exe.visit_artifacts(&mut output_visitor)?;
                for (_, value) in &remote_worker.env {
                    value.visit_artifacts(&mut output_visitor)?;
                }
            }
        }

        let mut command_line_digest_for_dep_files =
            collect_dep_file_digest.then(ExpandedCommandLineFingerprinter::new);

        let mut exe_rendered = Vec::<String>::new();

        // Creating the artifact_path_mapping isn't free, because we have to iterate TSets.
        // Therefore, only create a mapping if we're going to use it - i.e. if the input
        // is not hidden.
        let input_artifact_group_values;
        let input_artifact_path_mapping = if bazel_paths {
            RunActionInputArtifactPathMapper::Bazel(BazelRunActionArtifactPathMapper::new(
                action_execution_ctx,
                &self.non_hidden_inputs,
            ))
        } else {
            input_artifact_group_values = self
                .non_hidden_inputs
                .iter()
                .map(|group| action_execution_ctx.artifact_values(group))
                .collect::<Vec<_>>();
            RunActionInputArtifactPathMapper::Default(ArtifactPathMapperImpl::from_values(
                input_artifact_group_values.iter().copied(),
            ))
        };
        let artifact_path_mapping =
            RunActionOutputArtifactPathMapper::new(&self.outputs, &input_artifact_path_mapping);
        let dep_files_artifact_path_mapping = DepFilesPlaceholderArtifactPathMapperWithValues {
            values: &input_artifact_path_mapping,
        };
        let artifact_path_mapping_for_dep_files =
            RunActionOutputArtifactPathMapper::new(&self.outputs, &dep_files_artifact_path_mapping);
        values
            .exe
            .add_to_command_line(&mut exe_rendered, &mut cli_ctx, &artifact_path_mapping)?;
        if let Some(command_line_digest_for_dep_files) = &mut command_line_digest_for_dep_files {
            values.exe.add_to_command_line(
                command_line_digest_for_dep_files,
                cli_digest_ctx
                    .as_mut()
                    .expect("dep-file digest context must exist when digest is collected"),
                &artifact_path_mapping_for_dep_files,
            )?;
            command_line_digest_for_dep_files.push_count();
        }
        if collect_action_inputs {
            visit_run_action_command_line_artifacts(&self.outputs, values.exe, artifact_visitor)?;
        }

        let worker = if let Some(worker) = values.worker {
            let mut worker_rendered = Vec::<String>::new();
            let mut local_worker_visitor = SimpleCommandLineArtifactVisitor::new();
            worker.exe.add_to_command_line(
                &mut worker_rendered,
                &mut cli_ctx,
                &artifact_path_mapping,
            )?;
            if let Some(command_line_digest_for_dep_files) = &mut command_line_digest_for_dep_files
            {
                worker.exe.add_to_command_line(
                    command_line_digest_for_dep_files,
                    cli_digest_ctx
                        .as_mut()
                        .expect("dep-file digest context must exist when digest is collected"),
                    &artifact_path_mapping_for_dep_files,
                )?;
            }
            visit_run_action_command_line_artifacts(
                &self.outputs,
                worker.exe,
                &mut local_worker_visitor,
            )?;
            let worker_env: bz_error::Result<SortedVectorMap<_, _>> = worker
                .env
                .into_iter()
                .map(|(k, v)| {
                    let mut env = String::new();
                    let mut ctx = RunActionCommandLineContext::new(
                        fs,
                        bazel_paths,
                        bazel_path_mapping,
                        param_files.clone(),
                        RunActionParamFileMode::Record,
                    );
                    v.add_to_command_line(
                        &mut SpaceSeparatedCommandLineBuilder::wrap_string(&mut env),
                        &mut ctx,
                        &artifact_path_mapping,
                    )?;
                    visit_run_action_command_line_artifacts(
                        &self.outputs,
                        v,
                        &mut local_worker_visitor,
                    )?;

                    if let Some(command_line_digest_for_dep_files) =
                        &mut command_line_digest_for_dep_files
                    {
                        let mut digest_ctx = RunActionCommandLineContext::new(
                            fs,
                            bazel_paths,
                            bazel_path_mapping,
                            param_files.clone(),
                            RunActionParamFileMode::Replay,
                        );
                        command_line_digest_for_dep_files.push_arg(k.to_owned());
                        v.add_to_command_line(
                            command_line_digest_for_dep_files,
                            &mut digest_ctx,
                            &artifact_path_mapping_for_dep_files,
                        )?;
                        command_line_digest_for_dep_files.push_count();
                    }
                    Ok((k.to_owned(), env))
                })
                .collect();

            let local_worker_inputs: Vec<&ArtifactGroupValues> = local_worker_visitor
                .inputs()
                .map(|group| action_execution_ctx.artifact_values(group))
                .collect();

            let bazel_execroot = if bazel_paths {
                Some(self.bazel_action_execroot(fs.fs(), action_execution_ctx.target())?)
            } else {
                None
            };
            let inputs = self.command_execution_inputs_with_bazel_execroot_aliases(
                &local_worker_inputs,
                fs.fs(),
                bazel_execroot.as_deref(),
                bazel_paths,
            )?;

            let input_paths = CommandExecutionPaths::new(
                inputs,
                BuckIndexSet::default(),
                action_execution_ctx.fs(),
                action_execution_ctx.digest_config(),
                action_execution_ctx
                    .run_action_knobs()
                    .action_paths_interner
                    .as_ref(),
            )?;

            let worker_key = if worker.supports_bazel_remote_persistent_worker_protocol {
                let mut worker_visitor = SimpleCommandLineArtifactVisitor::new();
                visit_run_action_command_line_artifacts(
                    &self.outputs,
                    worker.exe,
                    &mut worker_visitor,
                )?;
                if !worker_visitor.declared_outputs.is_empty()
                    && !worker_visitor.frozen_outputs.is_empty()
                {
                    // TODO[AH] create appropriate error enum value.
                    return Err(bz_error!(
                        bz_error::ErrorTag::ActionMismatchedOutputs,
                        "Remote persistent worker command should not produce outputs."
                    ));
                }
                let worker_inputs: Vec<&ArtifactGroupValues> = worker_visitor
                    .inputs()
                    .map(|group| action_execution_ctx.artifact_values(group))
                    .collect();
                let worker_digest = metadata_digest(
                    fs.fs(),
                    &worker_inputs,
                    action_execution_ctx.digest_config(),
                )?;
                Some(worker_digest)
            } else {
                None
            };

            Some(WorkerSpec {
                exe: worker_rendered,
                id: worker.id,
                protocol: if worker.supports_bazel_local_persistent_worker_protocol {
                    WorkerProtocol::Bazel
                } else {
                    WorkerProtocol::Buck2
                },
                env: worker_env?,
                concurrency: worker.concurrency,
                streaming: worker.streaming,
                bazel_worker_sandboxing: worker.requires_bazel_worker_sandboxing,
                remote_key: worker_key,
                input_paths,
            })
        } else {
            None
        };

        let remote_worker = if let Some(remote_worker) = values.remote_worker {
            let mut remote_worker_init_visitor = SimpleCommandLineArtifactVisitor::new();
            let mut remote_worker_init_rendered = Vec::<String>::new();
            remote_worker.exe.add_to_command_line(
                &mut remote_worker_init_rendered,
                &mut cli_ctx,
                &artifact_path_mapping,
            )?;
            if let Some(command_line_digest_for_dep_files) = &mut command_line_digest_for_dep_files
            {
                remote_worker.exe.add_to_command_line(
                    command_line_digest_for_dep_files,
                    cli_digest_ctx
                        .as_mut()
                        .expect("dep-file digest context must exist when digest is collected"),
                    &artifact_path_mapping_for_dep_files,
                )?;
            }
            visit_run_action_command_line_artifacts(
                &self.outputs,
                remote_worker.exe,
                &mut remote_worker_init_visitor,
            )?;

            let remote_worker_env: bz_error::Result<SortedVectorMap<_, _>> = remote_worker
                .env
                .into_iter()
                .map(|(k, v)| {
                    let mut env = String::new();
                    let mut ctx = RunActionCommandLineContext::new(
                        fs,
                        bazel_paths,
                        bazel_path_mapping,
                        param_files.clone(),
                        RunActionParamFileMode::Record,
                    );
                    v.add_to_command_line(
                        &mut SpaceSeparatedCommandLineBuilder::wrap_string(&mut env),
                        &mut ctx,
                        &artifact_path_mapping,
                    )?;
                    visit_run_action_command_line_artifacts(
                        &self.outputs,
                        v,
                        &mut remote_worker_init_visitor,
                    )?;

                    if let Some(command_line_digest_for_dep_files) =
                        &mut command_line_digest_for_dep_files
                    {
                        let mut digest_ctx = RunActionCommandLineContext::new(
                            fs,
                            bazel_paths,
                            bazel_path_mapping,
                            param_files.clone(),
                            RunActionParamFileMode::Replay,
                        );
                        command_line_digest_for_dep_files.push_arg(k.to_owned());
                        v.add_to_command_line(
                            command_line_digest_for_dep_files,
                            &mut digest_ctx,
                            &artifact_path_mapping_for_dep_files,
                        )?;
                        command_line_digest_for_dep_files.push_count();
                    }
                    Ok((k.to_owned(), env))
                })
                .collect();

            let artifact_inputs: Vec<&ArtifactGroupValues> = remote_worker_init_visitor
                .inputs()
                .map(|group| action_execution_ctx.artifact_values(group))
                .collect();

            let inputs: Vec<CommandExecutionInput> =
                artifact_inputs[..].map(|&i| CommandExecutionInput::Artifact(Box::new(i.dupe())));

            let input_paths = CommandExecutionPaths::new(
                inputs,
                BuckIndexSet::default(),
                action_execution_ctx.fs(),
                action_execution_ctx.digest_config(),
                action_execution_ctx
                    .run_action_knobs()
                    .action_paths_interner
                    .as_ref(),
            )?;
            Some(RemoteWorkerSpec {
                id: remote_worker.id,
                init: remote_worker_init_rendered,
                env: remote_worker_env?,
                input_paths,
                concurrency: remote_worker.concurrency,
            })
        } else {
            None
        };

        let mut args_rendered = Vec::<String>::new();
        if let Some(args) = &self.inner.bazel_string_args {
            args_rendered.extend(args.iter().cloned());
            if let Some(command_line_digest_for_dep_files) = &mut command_line_digest_for_dep_files
            {
                push_string_args_for_dep_file_digest(
                    command_line_digest_for_dep_files,
                    args,
                    bazel_paths,
                    bazel_path_mapping,
                );
            }
        } else if let Some((args, _)) = &bazel_cc_command_line {
            args_rendered.extend(args.iter().cloned());
            if let Some(command_line_digest_for_dep_files) = &mut command_line_digest_for_dep_files
            {
                push_string_args_for_dep_file_digest(
                    command_line_digest_for_dep_files,
                    args,
                    bazel_paths,
                    bazel_path_mapping,
                );
            }
        } else {
            values.args.add_to_command_line(
                &mut args_rendered,
                &mut cli_ctx,
                &artifact_path_mapping,
            )?;
            if let Some(command_line_digest_for_dep_files) = &mut command_line_digest_for_dep_files
            {
                values.args.add_to_command_line(
                    command_line_digest_for_dep_files,
                    cli_digest_ctx
                        .as_mut()
                        .expect("dep-file digest context must exist when digest is collected"),
                    &artifact_path_mapping_for_dep_files,
                )?;
                command_line_digest_for_dep_files.push_count();
            }
        }
        if collect_action_inputs
            && self.inner.bazel_string_args.is_none()
            && values.bazel_cc_command_line.is_none()
        {
            visit_run_action_command_line_artifacts(&self.outputs, values.args, artifact_visitor)?;
        }

        if collect_action_inputs && let Some(bazel_inputs) = values.bazel_inputs {
            visit_run_action_command_line_artifacts(&self.outputs, bazel_inputs, artifact_visitor)?;
        }
        if collect_action_inputs && let Some(runfiles) = values.bazel_executable_runfiles {
            visit_bazel_runfiles_artifacts(runfiles, artifact_visitor)?;
        }
        if collect_action_inputs && let Some(tool_runfiles) = values.bazel_tool_runfiles {
            visit_bazel_tool_runfiles_artifacts(tool_runfiles, artifact_visitor)?;
        }

        let default_shell_env_len = if self.inner.bazel_use_default_shell_env == Some(true) {
            bazel_fixed_default_shell_env(action_execution_ctx.target()).len()
        } else {
            0
        };
        let bazel_cc_env_len = bazel_cc_command_line
            .as_ref()
            .map_or(0, |(_, env)| env.len());
        let env_len = default_shell_env_len + values.env.len() + bazel_cc_env_len;
        let explicit_cli_env: bz_error::Result<SortedVectorMap<_, _>> = values
            .env
            .into_iter()
            .map(|(k, v)| {
                let mut env = String::new();
                let mut ctx = RunActionCommandLineContext::new(
                    fs,
                    bazel_paths,
                    bazel_path_mapping,
                    param_files.clone(),
                    RunActionParamFileMode::Record,
                );
                v.add_to_command_line(
                    &mut SpaceSeparatedCommandLineBuilder::wrap_string(&mut env),
                    &mut ctx,
                    &artifact_path_mapping,
                )?;
                if collect_action_inputs {
                    visit_run_action_command_line_artifacts(&self.outputs, v, artifact_visitor)?;
                }

                if let Some(command_line_digest_for_dep_files) =
                    &mut command_line_digest_for_dep_files
                {
                    let mut digest_ctx = RunActionCommandLineContext::new(
                        fs,
                        bazel_paths,
                        bazel_path_mapping,
                        param_files.clone(),
                        RunActionParamFileMode::Replay,
                    );
                    command_line_digest_for_dep_files.push_arg(k.to_owned());
                    v.add_to_command_line(
                        command_line_digest_for_dep_files,
                        &mut digest_ctx,
                        &artifact_path_mapping_for_dep_files,
                    )?;
                    command_line_digest_for_dep_files.push_count();
                }
                Ok((k.to_owned(), env))
            })
            .collect();
        let mut cli_env = if self.inner.bazel_use_default_shell_env == Some(true) {
            bazel_fixed_default_shell_env(action_execution_ctx.target())
        } else {
            SortedVectorMap::new()
        };
        cli_env.extend(explicit_cli_env?);
        if let Some((_, env)) = &bazel_cc_command_line {
            for (key, value) in env {
                cli_env.insert(key.to_owned(), value.to_owned());
                if let Some(command_line_digest_for_dep_files) =
                    &mut command_line_digest_for_dep_files
                {
                    command_line_digest_for_dep_files.push_arg(key.to_owned());
                    command_line_digest_for_dep_files.push_arg(
                        bazel_normalize_and_map_buck_owned_exec_paths(value, bazel_path_mapping),
                    );
                    command_line_digest_for_dep_files.push_count();
                }
            }
        }

        if let Some(command_line_digest_for_dep_files) = &mut command_line_digest_for_dep_files {
            command_line_digest_for_dep_files.push_arg(env_len.to_string());
            command_line_digest_for_dep_files.push_count();
        }

        let mut worker = worker;
        let mut remote_worker = remote_worker;
        if bazel_paths {
            bazel_normalize_command_line(&mut exe_rendered, bazel_path_mapping);
            bazel_normalize_command_line(&mut args_rendered, bazel_path_mapping);
            bazel_normalize_command_env(&mut cli_env, bazel_path_mapping);
            if let Some(worker) = &mut worker {
                bazel_normalize_command_line(&mut worker.exe, bazel_path_mapping);
                bazel_normalize_command_env(&mut worker.env, bazel_path_mapping);
            }
            if let Some(remote_worker) = &mut remote_worker {
                bazel_normalize_command_line(&mut remote_worker.init, bazel_path_mapping);
                bazel_normalize_command_env(&mut remote_worker.env, bazel_path_mapping);
            }
        }

        Ok((
            ExpandedCommandLine {
                exe: exe_rendered,
                args: args_rendered,
                env: cli_env,
            },
            command_line_digest_for_dep_files.map(|digest| digest.finalize()),
            worker,
            remote_worker,
            param_files.files(collect_dep_file_digest)?,
        ))
    }

    fn prepare_workers_for_local_action_cache_probe<'v>(
        &'v self,
        action_execution_ctx: &dyn ActionExecutionCtx,
        values: &UnpackedRunActionValues<'v>,
    ) -> bz_error::Result<
        Option<(
            Option<LocalActionCacheWorkerRef<'static>>,
            Option<LocalActionCacheRemoteWorkerRef<'static>>,
        )>,
    > {
        if values.worker.is_none() && values.remote_worker.is_none() {
            return Ok(Some((None, None)));
        }

        let fs = &action_execution_ctx.executor_fs();
        let bazel_paths = self.uses_bazel_execroot_paths();
        let bazel_path_mapping =
            bazel_path_mapping_enabled_value(bazel_paths, self.inner.supports_bazel_path_mapping);
        let base_output = self
            .outputs
            .iter()
            .next()
            .ok_or_else(|| internal_error!("run actions must have at least one output"))?;
        let base_bazel_exec_path = if bazel_paths {
            let artifact = Artifact::from(base_output.dupe());
            Some(bazel_normalize_and_map_buck_owned_exec_paths(
                &bazel_artifact_path(artifact.get_path()),
                bazel_path_mapping,
            ))
        } else {
            None
        };
        let base_path = if let Some(base_bazel_exec_path) = &base_bazel_exec_path {
            bazel_param_file_base_path(base_bazel_exec_path)?
        } else {
            base_output.get_path().path().to_owned()
        };
        let param_files = RunActionParamFilesRef::new(
            base_output.get_path().owner().dupe(),
            base_path,
            base_bazel_exec_path,
            base_output.get_path().bazel_output_root(),
            if self.all_outputs_are_content_based() {
                BuckOutPathKind::ContentHash
            } else {
                BuckOutPathKind::Configuration
            },
            action_execution_ctx.digest_config(),
        );

        let input_artifact_group_values;
        let input_artifact_path_mapping = if bazel_paths {
            RunActionInputArtifactPathMapper::Bazel(BazelRunActionArtifactPathMapper::new(
                action_execution_ctx,
                &self.non_hidden_inputs,
            ))
        } else {
            input_artifact_group_values = self
                .non_hidden_inputs
                .iter()
                .map(|group| action_execution_ctx.artifact_values(group))
                .collect::<Vec<_>>();
            RunActionInputArtifactPathMapper::Default(ArtifactPathMapperImpl::from_values(
                input_artifact_group_values.iter().copied(),
            ))
        };
        let artifact_path_mapping =
            RunActionOutputArtifactPathMapper::new(&self.outputs, &input_artifact_path_mapping);

        let worker = if let Some(worker) = &values.worker {
            let mut worker_rendered = Vec::<String>::new();
            let mut cli_ctx = RunActionCommandLineContext::new(
                fs,
                bazel_paths,
                bazel_path_mapping,
                param_files.clone(),
                RunActionParamFileMode::RecordDigestOnly,
            );
            worker.exe.add_to_command_line(
                &mut worker_rendered,
                &mut cli_ctx,
                &artifact_path_mapping,
            )?;

            let mut local_worker_visitor = SimpleCommandLineArtifactVisitor::new();
            visit_run_action_command_line_artifacts(
                &self.outputs,
                worker.exe,
                &mut local_worker_visitor,
            )?;
            let worker_env: bz_error::Result<SortedVectorMap<_, _>> = worker
                .env
                .iter()
                .map(|(k, v)| {
                    let mut env = String::new();
                    let mut ctx = RunActionCommandLineContext::new(
                        fs,
                        bazel_paths,
                        bazel_path_mapping,
                        param_files.clone(),
                        RunActionParamFileMode::RecordDigestOnly,
                    );
                    v.add_to_command_line(
                        &mut SpaceSeparatedCommandLineBuilder::wrap_string(&mut env),
                        &mut ctx,
                        &artifact_path_mapping,
                    )?;
                    visit_run_action_command_line_artifacts(
                        &self.outputs,
                        *v,
                        &mut local_worker_visitor,
                    )?;
                    Ok(((*k).to_owned(), env))
                })
                .collect();

            let local_worker_inputs: Vec<&ArtifactGroupValues> = local_worker_visitor
                .inputs()
                .map(|group| action_execution_ctx.artifact_values(group))
                .collect();

            let bazel_execroot = if bazel_paths {
                Some(self.bazel_action_execroot(fs.fs(), action_execution_ctx.target())?)
            } else {
                None
            };
            let inputs = self.command_execution_inputs_with_bazel_execroot_aliases(
                &local_worker_inputs,
                fs.fs(),
                bazel_execroot.as_deref(),
                bazel_paths,
            )?;

            let worker_key = if worker.supports_bazel_remote_persistent_worker_protocol {
                let worker_digest = metadata_digest(
                    fs.fs(),
                    &local_worker_inputs,
                    action_execution_ctx.digest_config(),
                )?;
                Some(worker_digest)
            } else {
                None
            };

            Some(LocalActionCacheWorkerProbe {
                exe: worker_rendered,
                id: worker.id,
                protocol: if worker.supports_bazel_local_persistent_worker_protocol {
                    WorkerProtocol::Bazel
                } else {
                    WorkerProtocol::Buck2
                },
                env: worker_env?,
                concurrency: worker.concurrency,
                streaming: worker.streaming,
                bazel_worker_sandboxing: worker.requires_bazel_worker_sandboxing,
                remote_key: worker_key,
                inputs,
            })
        } else {
            None
        };

        let remote_worker = if let Some(remote_worker) = &values.remote_worker {
            let mut remote_worker_init_visitor = SimpleCommandLineArtifactVisitor::new();
            let mut remote_worker_init_rendered = Vec::<String>::new();
            let mut cli_ctx = RunActionCommandLineContext::new(
                fs,
                bazel_paths,
                bazel_path_mapping,
                param_files.clone(),
                RunActionParamFileMode::RecordDigestOnly,
            );
            remote_worker.exe.add_to_command_line(
                &mut remote_worker_init_rendered,
                &mut cli_ctx,
                &artifact_path_mapping,
            )?;
            visit_run_action_command_line_artifacts(
                &self.outputs,
                remote_worker.exe,
                &mut remote_worker_init_visitor,
            )?;

            let remote_worker_env: bz_error::Result<SortedVectorMap<_, _>> = remote_worker
                .env
                .iter()
                .map(|(k, v)| {
                    let mut env = String::new();
                    let mut ctx = RunActionCommandLineContext::new(
                        fs,
                        bazel_paths,
                        bazel_path_mapping,
                        param_files.clone(),
                        RunActionParamFileMode::RecordDigestOnly,
                    );
                    v.add_to_command_line(
                        &mut SpaceSeparatedCommandLineBuilder::wrap_string(&mut env),
                        &mut ctx,
                        &artifact_path_mapping,
                    )?;
                    visit_run_action_command_line_artifacts(
                        &self.outputs,
                        *v,
                        &mut remote_worker_init_visitor,
                    )?;
                    Ok(((*k).to_owned(), env))
                })
                .collect();

            let artifact_inputs: Vec<&ArtifactGroupValues> = remote_worker_init_visitor
                .inputs()
                .map(|group| action_execution_ctx.artifact_values(group))
                .collect();
            let inputs: Vec<CommandExecutionInput> =
                artifact_inputs[..].map(|&i| CommandExecutionInput::Artifact(Box::new(i.dupe())));
            Some(LocalActionCacheRemoteWorkerProbe {
                id: remote_worker.id,
                init: remote_worker_init_rendered,
                env: remote_worker_env?,
                concurrency: remote_worker.concurrency,
                inputs,
            })
        } else {
            None
        };

        let mut worker = worker;
        let mut remote_worker = remote_worker;
        if bazel_paths {
            if let Some(worker) = &mut worker {
                bazel_normalize_command_line(&mut worker.exe, bazel_path_mapping);
                bazel_normalize_command_env(&mut worker.env, bazel_path_mapping);
            }
            if let Some(remote_worker) = &mut remote_worker {
                bazel_normalize_command_line(&mut remote_worker.init, bazel_path_mapping);
                bazel_normalize_command_env(&mut remote_worker.env, bazel_path_mapping);
            }
        }

        if !param_files.files(false)?.is_empty() {
            return Ok(None);
        }

        Ok(Some((
            worker.map(LocalActionCacheWorkerRef::Probe),
            remote_worker.map(LocalActionCacheRemoteWorkerRef::Probe),
        )))
    }

    pub(crate) fn new(
        inner: UnregisteredRunAction,
        starlark_values: OwnedFrozenValue,
        outputs: BuckIndexSet<BuildArtifact>,
        error_handler: Option<OwnedFrozenValue>,
    ) -> bz_error::Result<Self> {
        let starlark_values = starlark_values
            .downcast_starlark()
            .internal_error("Must be `RunActionValues`")?;

        Self::unpack(&starlark_values)?;

        // This is checked when declared, but we depend on it so make it clear that it's enforced.
        if outputs.is_empty() {
            return Err(RunActionError::NoOutputsSpecified.into());
        }

        let outputs = BoxSliceSet::from(outputs);
        let (inputs, command_inputs, non_hidden_inputs) =
            Self::collect_inputs(&inner, &starlark_values, &outputs)?;
        let local_action_cache_inputs = Self::collect_local_action_cache_inputs(&inner, &inputs);

        Ok(RunAction {
            inner,
            starlark_values,
            outputs,
            inputs,
            command_inputs,
            local_action_cache_inputs,
            non_hidden_inputs,
            error_handler,
        })
    }

    fn collect_local_action_cache_inputs(
        inner: &UnregisteredRunAction,
        inputs: &[ArtifactGroup],
    ) -> Box<[ArtifactGroup]> {
        if !inner.dep_files.is_empty() || inner.metadata_param.is_some() {
            return Vec::new().into_boxed_slice();
        }

        let mut local_action_cache_inputs = BuckIndexSet::with_capacity(inputs.len());
        for input in inputs.iter() {
            local_action_cache_inputs.insert(input.dupe());
        }
        local_action_cache_inputs
            .into_iter()
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }

    fn add_bazel_execroot_path_aliases(
        &self,
        inputs: &mut Vec<CommandExecutionInput>,
        artifact_inputs: &[&ArtifactGroupValues],
        artifact_fs: &ArtifactFs,
        bazel_execroot: Option<&ProjectRelativePath>,
        stage_bazel_path_mapping_aliases: bool,
        executable_paths: Option<&[ProjectRelativePathBuf]>,
    ) -> bz_error::Result<()> {
        let Some(bazel_execroot) = bazel_execroot else {
            return Ok(());
        };

        let mut aliases = BuckIndexSet::new();
        for artifact_group_values in artifact_inputs {
            for ((artifact, value), remote_cache_cas_info) in
                artifact_group_values.iter_with_remote_cache_cas_info()
            {
                let source_path =
                    Self::bazel_artifact_alias_source_path(artifact, value, artifact_fs)
                        .buck_error_context("Invalid Bazel execroot source path")?;
                let source_requires_materialization =
                    artifact.requires_materialization(artifact_fs);

                let bazel_path = bazel_normalize_buck_owned_exec_paths(&bazel_artifact_path(
                    artifact.get_path(),
                ));
                let bazel_alias = Self::bazel_execroot_path(bazel_execroot, bazel_path.clone())?;
                if aliases.insert(bazel_alias.clone()) && source_path != bazel_alias {
                    let value = Self::bazel_executable_artifact_value(
                        source_path.as_ref(),
                        value,
                        executable_paths,
                    );
                    inputs.push(CommandExecutionInput::ArtifactPathAlias {
                        source_path: source_path.clone(),
                        source_requires_materialization,
                        remote_cache_cas_info: remote_cache_cas_info.cloned(),
                        owner: artifact.input_owner(),
                        path: bazel_alias,
                        value,
                    });
                }

                let source_alias =
                    Self::bazel_execroot_path(bazel_execroot, source_path.as_str().to_owned())?;
                if aliases.insert(source_alias.clone()) && source_path != source_alias {
                    let value = Self::bazel_executable_artifact_value(
                        source_path.as_ref(),
                        value,
                        executable_paths,
                    );
                    inputs.push(CommandExecutionInput::ArtifactPathAlias {
                        source_path: source_path.clone(),
                        source_requires_materialization,
                        remote_cache_cas_info: remote_cache_cas_info.cloned(),
                        owner: artifact.input_owner(),
                        path: source_alias,
                        value,
                    });
                }

                let normalized_source_path =
                    bazel_normalize_buck_owned_exec_paths(source_path.as_str());
                let normalized_source_alias =
                    Self::bazel_execroot_path(bazel_execroot, normalized_source_path.clone())?;
                if aliases.insert(normalized_source_alias.clone())
                    && source_path != normalized_source_alias
                {
                    let value = Self::bazel_executable_artifact_value(
                        source_path.as_ref(),
                        value,
                        executable_paths,
                    );
                    inputs.push(CommandExecutionInput::ArtifactPathAlias {
                        source_path: source_path.clone(),
                        source_requires_materialization,
                        remote_cache_cas_info: remote_cache_cas_info.cloned(),
                        owner: artifact.input_owner(),
                        path: normalized_source_alias,
                        value,
                    });
                }

                if stage_bazel_path_mapping_aliases {
                    // Bazel may let a non-path-mapped action consume strings emitted by an
                    // earlier path-mapped action, such as rules_go GoLink consuming GoStdlib
                    // flags. Stage the stripped cfg aliases for all Bazel execroot actions.
                    let mapped_bazel_path =
                        bazel_strip_buck_output_path_config_segments(&bazel_path);
                    let mapped_bazel_alias =
                        Self::bazel_execroot_path(bazel_execroot, mapped_bazel_path)?;
                    if aliases.insert(mapped_bazel_alias.clone())
                        && source_path != mapped_bazel_alias
                    {
                        let value = Self::bazel_executable_artifact_value(
                            source_path.as_ref(),
                            value,
                            executable_paths,
                        );
                        inputs.push(CommandExecutionInput::ArtifactPathAlias {
                            source_path: source_path.clone(),
                            source_requires_materialization,
                            remote_cache_cas_info: remote_cache_cas_info.cloned(),
                            owner: artifact.input_owner(),
                            path: mapped_bazel_alias,
                            value,
                        });
                    }

                    let mapped_normalized_source_path =
                        bazel_strip_buck_output_path_config_segments(&normalized_source_path);
                    let mapped_normalized_source_alias =
                        Self::bazel_execroot_path(bazel_execroot, mapped_normalized_source_path)?;
                    if aliases.insert(mapped_normalized_source_alias.clone())
                        && source_path != mapped_normalized_source_alias
                    {
                        let value = Self::bazel_executable_artifact_value(
                            source_path.as_ref(),
                            value,
                            executable_paths,
                        );
                        inputs.push(CommandExecutionInput::ArtifactPathAlias {
                            source_path: source_path.clone(),
                            source_requires_materialization,
                            remote_cache_cas_info: remote_cache_cas_info.cloned(),
                            owner: artifact.input_owner(),
                            path: mapped_normalized_source_alias,
                            value,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    fn artifact_inputs_as_command_execution_inputs(
        artifact_inputs: &[&ArtifactGroupValues],
    ) -> Vec<CommandExecutionInput> {
        artifact_inputs[..].map(|&i| CommandExecutionInput::Artifact(Box::new(i.dupe())))
    }

    fn artifact_inputs_as_command_execution_inputs_with_executable_overrides(
        artifact_inputs: &[&ArtifactGroupValues],
        artifact_fs: &ArtifactFs,
        executable_paths: Option<Arc<[ProjectRelativePathBuf]>>,
    ) -> bz_error::Result<Vec<CommandExecutionInput>> {
        let Some(executable_paths) = executable_paths else {
            return Ok(Self::artifact_inputs_as_command_execution_inputs(
                artifact_inputs,
            ));
        };

        let mut inputs = Vec::with_capacity(artifact_inputs.len());
        for artifact_group_values in artifact_inputs {
            if Self::artifact_group_contains_executable_path(
                artifact_group_values,
                artifact_fs,
                &executable_paths,
            )? {
                inputs.push(CommandExecutionInput::ArtifactWithExecutableOverrides {
                    group: Box::new((*artifact_group_values).dupe()),
                    executable_paths: executable_paths.dupe(),
                });
            } else {
                inputs.push(CommandExecutionInput::Artifact(Box::new(
                    (*artifact_group_values).dupe(),
                )));
            }
        }
        Ok(inputs)
    }

    fn artifact_group_contains_executable_path(
        artifact_group_values: &ArtifactGroupValues,
        artifact_fs: &ArtifactFs,
        executable_paths: &[ProjectRelativePathBuf],
    ) -> bz_error::Result<bool> {
        for (artifact, value) in artifact_group_values.iter() {
            let path = artifact.resolve_path(
                artifact_fs,
                if artifact.path_resolution_requires_artifact_value() {
                    Some(value.content_based_path_hash())
                } else {
                    None
                }
                .as_ref(),
            )?;
            if Self::is_bazel_executable_override_path(&path, executable_paths) {
                return Ok(true);
            }
            let alias_source_path =
                Self::bazel_artifact_alias_source_path(artifact, value, artifact_fs)?;
            if Self::is_bazel_executable_override_path(&alias_source_path, executable_paths) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn is_bazel_executable_override_path(
        path: &ProjectRelativePath,
        executable_paths: &[ProjectRelativePathBuf],
    ) -> bool {
        executable_paths
            .iter()
            .any(|executable_path| executable_path == path)
    }

    fn bazel_executable_artifact_value(
        path: &ProjectRelativePath,
        value: &ArtifactValue,
        executable_paths: Option<&[ProjectRelativePathBuf]>,
    ) -> ArtifactValue {
        if executable_paths
            .is_some_and(|paths| Self::is_bazel_executable_override_path(path, paths))
        {
            value.with_executable_bit(true)
        } else {
            value.dupe()
        }
    }

    fn add_bazel_executable_override_paths_for_value(
        paths: &mut BuckIndexSet<ProjectRelativePathBuf>,
        ctx: &dyn ActionExecutionCtx,
        artifact_fs: &ArtifactFs,
        executable: Value<'_>,
    ) -> bz_error::Result<()> {
        let Some(executable) = ValueAsInputArtifactLike::unpack_value(executable)? else {
            return Ok(());
        };
        let artifact_group = executable.0.get_artifact_group()?;
        let artifact_group_values = ctx.artifact_values(&artifact_group);
        for (artifact, value) in artifact_group_values.iter() {
            let path = artifact.resolve_path(
                artifact_fs,
                if artifact.path_resolution_requires_artifact_value() {
                    Some(value.content_based_path_hash())
                } else {
                    None
                }
                .as_ref(),
            )?;
            paths.insert(path);
            paths.insert(Self::bazel_artifact_alias_source_path(
                artifact,
                value,
                artifact_fs,
            )?);
        }
        Ok(())
    }

    fn bazel_executable_override_paths(
        &self,
        ctx: &dyn ActionExecutionCtx,
        artifact_fs: &ArtifactFs,
        values: &UnpackedRunActionValues<'_>,
    ) -> bz_error::Result<Option<Arc<[ProjectRelativePathBuf]>>> {
        let mut paths = BuckIndexSet::new();
        if let Some(executable) = values.bazel_executable {
            Self::add_bazel_executable_override_paths_for_value(
                &mut paths,
                ctx,
                artifact_fs,
                executable,
            )?;
        }
        if let Some(tool_runfiles) = values.bazel_tool_runfiles {
            for tool in bazel_tool_runfiles(tool_runfiles)? {
                Self::add_bazel_executable_override_paths_for_value(
                    &mut paths,
                    ctx,
                    artifact_fs,
                    tool?.executable,
                )?;
            }
        }
        if paths.is_empty() {
            Ok(None)
        } else {
            Ok(Some(paths.into_iter().collect::<Vec<_>>().into()))
        }
    }

    fn command_execution_inputs_with_bazel_execroot_aliases(
        &self,
        artifact_inputs: &[&ArtifactGroupValues],
        artifact_fs: &ArtifactFs,
        bazel_execroot: Option<&ProjectRelativePath>,
        stage_bazel_path_mapping_aliases: bool,
    ) -> bz_error::Result<Vec<CommandExecutionInput>> {
        let mut inputs = Self::artifact_inputs_as_command_execution_inputs(artifact_inputs);
        self.add_bazel_execroot_path_aliases(
            &mut inputs,
            artifact_inputs,
            artifact_fs,
            bazel_execroot,
            stage_bazel_path_mapping_aliases,
            None,
        )?;
        Ok(inputs)
    }

    fn add_bazel_external_repo_path_aliases(
        &self,
        inputs: &mut Vec<CommandExecutionInput>,
        expanded: &ExpandedCommandLine,
        artifact_fs: &ArtifactFs,
        bazel_execroot: Option<&ProjectRelativePath>,
    ) -> bz_error::Result<()> {
        let Some(bazel_execroot) = bazel_execroot else {
            return Ok(());
        };

        let mut references = BuckIndexSet::new();
        for arg in expanded.exe.iter().chain(expanded.args.iter()) {
            Self::add_bazel_external_repo_references(arg, &mut references);
        }
        for (_key, value) in expanded.env.iter() {
            Self::add_bazel_external_repo_references(value, &mut references);
        }

        let mut aliases = inputs
            .iter()
            .filter_map(|input| match input {
                CommandExecutionInput::ArtifactPathAlias { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect::<BuckIndexSet<_>>();
        for (repo, path) in references {
            let Some(source_root) = Self::bazel_external_repo_source_root(artifact_fs, &repo)?
            else {
                continue;
            };
            if path.starts_with("bin/") {
                Self::add_bazel_external_repo_path_alias(
                    inputs,
                    &mut aliases,
                    artifact_fs,
                    bazel_execroot,
                    &repo,
                    source_root.as_ref(),
                    "bin",
                )?;

                if fs_util::try_exists(artifact_fs.fs().resolve(
                    source_root.join(ForwardRelativePathBuf::unchecked_new("lib".to_owned())),
                ))? {
                    Self::add_bazel_external_repo_path_alias(
                        inputs,
                        &mut aliases,
                        artifact_fs,
                        bazel_execroot,
                        &repo,
                        source_root.as_ref(),
                        "lib",
                    )?;
                }
            } else {
                Self::add_bazel_external_repo_path_alias(
                    inputs,
                    &mut aliases,
                    artifact_fs,
                    bazel_execroot,
                    &repo,
                    source_root.as_ref(),
                    path.as_ref(),
                )?;
            }
        }

        Ok(())
    }

    fn add_bazel_external_repo_path_alias(
        inputs: &mut Vec<CommandExecutionInput>,
        aliases: &mut BuckIndexSet<ProjectRelativePathBuf>,
        artifact_fs: &ArtifactFs,
        bazel_execroot: &ProjectRelativePath,
        repo: &str,
        source_root: &ProjectRelativePath,
        repo_path: &str,
    ) -> bz_error::Result<()> {
        let repo_path = ForwardRelativePathBuf::try_from(repo_path.to_owned())
            .buck_error_context("Invalid Bazel external repository path")?;
        let source_path = source_root.join(repo_path.clone());
        if !fs_util::try_exists(artifact_fs.fs().resolve(&source_path))? {
            return Ok(());
        }

        let alias_path =
            Self::bazel_execroot_path(bazel_execroot, format!("external/{repo}/{repo_path}"))?;
        if source_path == alias_path || !aliases.insert(alias_path.clone()) {
            return Ok(());
        }

        let source_abs_path = artifact_fs
            .fs()
            .resolve(&source_path)
            .as_path()
            .to_path_buf();
        inputs.push(CommandExecutionInput::ArtifactPathAlias {
            source_path,
            source_requires_materialization: false,
            remote_cache_cas_info: None,
            owner: None,
            path: alias_path,
            value: ArtifactValue::external_symlink(Arc::new(ExternalSymlink::new(
                source_abs_path,
                ForwardRelativePathBuf::default(),
            )?)),
        });
        Ok(())
    }

    fn add_bazel_external_repo_references(
        value: &str,
        references: &mut BuckIndexSet<(String, String)>,
    ) {
        let mut rest = value;
        while let Some(index) = rest.find("external/") {
            let after_external = &rest[index + "external/".len()..];
            let Some((repo, after_repo)) = after_external.split_once('/') else {
                break;
            };
            let repo_path = after_repo
                .split(|c: char| {
                    matches!(c, ':' | ',' | ';' | '"' | '\'' | ')' | '(' | '[' | ']')
                        || c.is_whitespace()
                })
                .next()
                .unwrap_or_default();
            if !repo.is_empty()
                && !repo_path.is_empty()
                && ForwardRelativePath::new(repo).is_ok()
                && !repo.contains('/')
                && ForwardRelativePathBuf::try_from(repo_path.to_owned()).is_ok()
            {
                references.insert((repo.to_owned(), repo_path.to_owned()));
            }
            rest = after_repo;
        }
    }

    fn bazel_external_repo_source_root(
        artifact_fs: &ArtifactFs,
        repo: &str,
    ) -> bz_error::Result<Option<ProjectRelativePathBuf>> {
        for kind in ["bzlmod_generated", "bzlmod", "bundled", "git"] {
            let source_root = ProjectRelativePathBuf::unchecked_new(format!(
                "{}/external_cells/{kind}/{repo}",
                artifact_fs.buck_out_path_resolver().root()
            ));
            if fs_util::try_exists(artifact_fs.fs().resolve(&source_root))? {
                return Ok(Some(source_root));
            }
        }
        Ok(None)
    }

    fn bazel_artifact_alias_source_path(
        artifact: &Artifact,
        value: &ArtifactValue,
        artifact_fs: &ArtifactFs,
    ) -> bz_error::Result<ProjectRelativePathBuf> {
        if artifact.has_content_based_path() && !artifact.is_projected() {
            return artifact.resolve_configuration_hash_path(artifact_fs);
        }

        artifact.resolve_path(
            artifact_fs,
            if artifact.path_resolution_requires_artifact_value() {
                Some(value.content_based_path_hash())
            } else {
                None
            }
            .as_ref(),
        )
    }

    fn bazel_runfiles_alias_path(
        bazel_execroot: &ProjectRelativePath,
        executable_path: &str,
        runfiles_path: &str,
    ) -> bz_error::Result<ProjectRelativePathBuf> {
        let runfiles_path = runfiles_path.strip_prefix("../").unwrap_or(runfiles_path);
        Self::bazel_execroot_path(
            bazel_execroot,
            format!("{executable_path}.runfiles/{runfiles_path}"),
        )
    }

    fn artifact_value_for<'a>(
        artifact_inputs: &'a [&ArtifactGroupValues],
        artifact: &Artifact,
    ) -> bz_error::Result<(&'a ArtifactValue, Option<Arc<CasDownloadInfo>>)> {
        for artifact_group_values in artifact_inputs {
            for ((input_artifact, value), remote_cache_cas_info) in
                artifact_group_values.iter_with_remote_cache_cas_info()
            {
                if input_artifact == artifact {
                    return Ok((value, remote_cache_cas_info.cloned()));
                }
            }
        }
        Err(internal_error!(
            "Bazel executable runfiles artifact was not present in action inputs"
        ))
    }

    fn add_bazel_runfiles_path_aliases(
        &self,
        inputs: &mut Vec<CommandExecutionInput>,
        artifact_inputs: &[&ArtifactGroupValues],
        artifact_fs: &ArtifactFs,
        bazel_execroot: Option<&ProjectRelativePath>,
        executable_path: Option<&str>,
        runfiles: Option<Value<'_>>,
        executable_paths: Option<&[ProjectRelativePathBuf]>,
    ) -> bz_error::Result<()> {
        let (Some(bazel_execroot), Some(executable_path), Some(runfiles)) =
            (bazel_execroot, executable_path, runfiles)
        else {
            return Ok(());
        };

        let mut aliases = BuckIndexSet::new();
        let mut saw_workspace_runfiles_entry = false;
        let workspace_runfiles_prefix = bazel_runfiles_prefix();
        for entry in bazel_runfiles_entries(runfiles)? {
            let entry = entry?;
            let normalized_entry_path = entry.path.strip_prefix("../").unwrap_or(entry.path);
            if normalized_entry_path == workspace_runfiles_prefix
                || normalized_entry_path
                    .strip_prefix(workspace_runfiles_prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            {
                saw_workspace_runfiles_entry = true;
            }
            let artifact = ValueAsInputArtifactLike::unpack_value(entry.target_file)?
                .ok_or_else(|| {
                    internal_error!("Bazel executable runfiles target_file should be File")
                })?
                .0
                .get_bound_artifact()?;
            let (value, remote_cache_cas_info) =
                Self::artifact_value_for(artifact_inputs, &artifact)?;
            let alias =
                Self::bazel_runfiles_alias_path(bazel_execroot, executable_path, entry.path)?;
            if aliases.insert(alias.clone()) {
                let source_path =
                    Self::bazel_artifact_alias_source_path(&artifact, value, artifact_fs)
                        .buck_error_context("Invalid Bazel runfiles source path")?;
                let source_requires_materialization =
                    artifact.requires_materialization(artifact_fs);
                if source_path == alias {
                    continue;
                }
                let value = Self::bazel_executable_artifact_value(
                    source_path.as_ref(),
                    value,
                    executable_paths,
                );
                inputs.push(CommandExecutionInput::ArtifactPathAlias {
                    source_path,
                    source_requires_materialization,
                    remote_cache_cas_info,
                    owner: artifact.input_owner(),
                    path: alias,
                    value,
                });
            }
        }
        if !saw_workspace_runfiles_entry {
            let alias = Self::bazel_runfiles_alias_path(
                bazel_execroot,
                executable_path,
                &format!("{workspace_runfiles_prefix}/.runfile"),
            )?;
            if aliases.insert(alias.clone()) {
                inputs.push(CommandExecutionInput::EmptyFile(alias));
            }
        }
        Ok(())
    }

    fn add_bazel_tool_runfiles_path_aliases(
        &self,
        inputs: &mut Vec<CommandExecutionInput>,
        artifact_inputs: &[&ArtifactGroupValues],
        artifact_fs: &ArtifactFs,
        bazel_execroot: Option<&ProjectRelativePath>,
        tool_runfiles: Option<Value<'_>>,
        executable_paths: Option<&[ProjectRelativePathBuf]>,
    ) -> bz_error::Result<()> {
        let (Some(bazel_execroot), Some(tool_runfiles)) = (bazel_execroot, tool_runfiles) else {
            return Ok(());
        };

        for tool in bazel_tool_runfiles(tool_runfiles)? {
            let tool = tool?;
            let executable = ValueAsInputArtifactLike::unpack_value(tool.executable)?
                .ok_or_else(|| internal_error!("Bazel tool runfiles executable should be File"))?
                .0
                .get_bound_artifact()?;
            let executable_path =
                bazel_normalize_buck_owned_exec_paths(&bazel_artifact_path(executable.get_path()));
            self.add_bazel_runfiles_path_aliases(
                inputs,
                artifact_inputs,
                artifact_fs,
                Some(bazel_execroot),
                Some(&executable_path),
                Some(tool.runfiles),
                executable_paths,
            )?;
        }
        Ok(())
    }

    async fn prepare_local_action_cache_probe<'v>(
        &'v self,
        visitor: &mut RunActionVisitor<'v>,
        ctx: &mut dyn ActionExecutionCtx,
    ) -> bz_error::Result<Option<(LocalActionCacheKey, BuckIndexSet<CommandExecutionOutput>)>> {
        let collect_action_inputs =
            !self.inner.dep_files.is_empty() || self.inner.metadata_param.is_some();
        let (command_line_digest, worker, remote_worker, _param_files) = {
            let values = Self::unpack(&self.starlark_values)?;
            let has_worker = values.worker.is_some() || values.remote_worker.is_some();
            if !collect_action_inputs
                && let Some(command_line_digest) = self
                    .inner
                    .precomputed_local_action_cache_command_line_digest
                    .as_ref()
                && let Some((worker, remote_worker)) =
                    self.prepare_workers_for_local_action_cache_probe(ctx, &values)?
            {
                (
                    command_line_digest.clone(),
                    worker,
                    remote_worker,
                    Vec::new(),
                )
            } else if has_worker {
                let (expanded, _, worker, remote_worker, param_files) = self
                    .expand_command_line_and_worker(ctx, visitor, false, collect_action_inputs)?;
                (
                    fingerprint_expanded_command_line_for_local_action_cache(
                        &expanded,
                        &param_files,
                    ),
                    worker.map(LocalActionCacheWorkerRef::Owned),
                    remote_worker.map(LocalActionCacheRemoteWorkerRef::Owned),
                    param_files,
                )
            } else {
                let (command_line_digest, param_files) = self
                    .prepare_command_line_for_local_action_cache_probe(
                        ctx,
                        visitor,
                        collect_action_inputs,
                    )?;
                (command_line_digest, None, None, param_files)
            }
        };

        let artifact_fs = ctx.fs().clone();
        let fs = &artifact_fs;
        let bazel_paths = self.uses_bazel_execroot_paths();
        let bazel_path_mapping =
            bazel_path_mapping_enabled_value(bazel_paths, self.inner.supports_bazel_path_mapping);
        let mut local_action_cache_extra_inputs: Vec<CommandExecutionInput> = Vec::with_capacity(2);

        let mut extra_env = Vec::new();
        let executor_fs = ctx.executor_fs();
        let cli_ctx = DefaultCommandLineContext::new(&executor_fs);
        let mut ignored_inputs = Vec::new();
        let mut ignored_pending_action_metadata_writes = Vec::new();
        self.prepare_action_metadata(
            ctx,
            &cli_ctx,
            fs,
            visitor,
            &mut ignored_inputs,
            &mut local_action_cache_extra_inputs,
            &mut extra_env,
            &mut ignored_pending_action_metadata_writes,
            false,
        )
        .await?;

        if !bazel_paths {
            let mut ignored_shared_content_based_paths = Vec::new();
            self.prepare_scratch_path(
                ctx,
                &cli_ctx,
                fs,
                &mut ignored_inputs,
                &mut ignored_shared_content_based_paths,
                &mut extra_env,
            )?;
        }
        let bazel_execroot = if bazel_paths {
            Some(self.bazel_action_execroot(fs, ctx.target())?)
        } else {
            None
        };
        let outputs = self
            .outputs
            .iter()
            .map(|b| {
                let produced_path = if let Some(bazel_execroot) = bazel_execroot.as_deref() {
                    let bazel_path =
                        if let Some(path) = visitor.bazel_output_exec_paths.get(b.get_path()) {
                            path.clone()
                        } else {
                            let artifact = Artifact::from(b.dupe());
                            bazel_normalize_and_map_buck_owned_exec_paths(
                                &bazel_artifact_path(artifact.get_path()),
                                bazel_path_mapping,
                            )
                        };
                    Some(Self::bazel_execroot_path(bazel_execroot, bazel_path)?)
                } else {
                    None
                };
                Ok(CommandExecutionOutput::BuildArtifact {
                    path: b.get_path().dupe(),
                    output_type: b.output_type(),
                    produced_path,
                })
            })
            .collect::<bz_error::Result<BuckIndexSet<_>>>()?;
        let outputs = CommandExecutionPaths::sort_outputs_for_execution(outputs, ctx.fs());

        Ok(self
            .local_action_cache_key(
                ctx,
                &command_line_digest,
                &extra_env,
                LocalActionCacheInputMetadata {
                    input_set_digest: ctx.local_action_cache_input_set_digest(),
                    extra_inputs: &local_action_cache_extra_inputs,
                },
                &outputs,
                worker,
                remote_worker,
            )?
            .map(|key| (key, outputs)))
    }

    async fn prepare<'v>(
        &'v self,
        visitor: &mut RunActionVisitor<'v>,
        ctx: &mut dyn ActionExecutionCtx,
    ) -> bz_error::Result<(
        UnpreparedRunAction,
        Option<ExpandedCommandLineDigestForDepFiles>,
        HostSharingRequirements,
    )> {
        let (
            expanded,
            expanded_command_line_digest_for_dep_files,
            worker,
            remote_worker,
            param_files,
        ) = self.expand_command_line_and_worker(
            ctx,
            visitor,
            !self.inner.dep_files.is_empty(),
            true,
        )?;

        let artifact_fs = ctx.fs().clone();
        let fs = &artifact_fs;

        let bazel_paths = self.uses_bazel_execroot_paths();
        let bazel_path_mapping =
            bazel_path_mapping_enabled_value(bazel_paths, self.inner.supports_bazel_path_mapping);
        // Bazel command lines can be pre-expanded into strings. Use the full action input set so
        // execroot aliases match the action graph even when render-time visits are incomplete.
        let artifact_inputs: Vec<&ArtifactGroupValues> = if bazel_paths {
            self.inputs
                .iter()
                .map(|group| ctx.artifact_values(group))
                .collect()
        } else {
            visitor
                .inputs()
                .map(|group| ctx.artifact_values(group))
                .collect()
        };

        let (bazel_executable_override_paths, bazel_executable_runfiles, bazel_tool_runfiles) = {
            let values = Self::unpack(&self.starlark_values)?;
            (
                self.bazel_executable_override_paths(ctx, fs, &values)?,
                values.bazel_executable_runfiles,
                values.bazel_tool_runfiles,
            )
        };
        let mut local_action_cache_inputs: Vec<CommandExecutionInput> =
            Self::artifact_inputs_as_command_execution_inputs_with_executable_overrides(
                &artifact_inputs,
                fs,
                bazel_executable_override_paths.dupe(),
            )?;
        let mut inputs: Vec<CommandExecutionInput> =
            Self::artifact_inputs_as_command_execution_inputs_with_executable_overrides(
                &artifact_inputs,
                fs,
                bazel_executable_override_paths.dupe(),
            )?;
        let bazel_execroot = if bazel_paths {
            Some(self.bazel_action_execroot(fs, ctx.target())?)
        } else {
            None
        };
        self.add_bazel_execroot_path_aliases(
            &mut inputs,
            &artifact_inputs,
            fs,
            bazel_execroot.as_deref(),
            bazel_paths,
            bazel_executable_override_paths.as_deref(),
        )?;
        self.add_bazel_runfiles_path_aliases(
            &mut inputs,
            &artifact_inputs,
            fs,
            bazel_execroot.as_deref(),
            expanded.exe.first().map(String::as_str),
            bazel_executable_runfiles,
            bazel_executable_override_paths.as_deref(),
        )?;
        self.add_bazel_tool_runfiles_path_aliases(
            &mut inputs,
            &artifact_inputs,
            fs,
            bazel_execroot.as_deref(),
            bazel_tool_runfiles,
            bazel_executable_override_paths.as_deref(),
        )?;
        self.add_bazel_external_repo_path_aliases(
            &mut inputs,
            &expanded,
            fs,
            bazel_execroot.as_deref(),
        )?;

        let mut extra_env = Vec::new();
        let mut pending_action_metadata_writes = Vec::new();
        let executor_fs = ctx.executor_fs();
        let cli_ctx = DefaultCommandLineContext::new(&executor_fs);
        self.prepare_param_files(
            fs,
            &param_files,
            &mut inputs,
            bazel_execroot.as_deref(),
            &mut pending_action_metadata_writes,
        )?;
        let local_action_cache_extra_inputs_start = local_action_cache_inputs.len();
        self.prepare_action_metadata(
            ctx,
            &cli_ctx,
            fs,
            visitor,
            &mut inputs,
            &mut local_action_cache_inputs,
            &mut extra_env,
            &mut pending_action_metadata_writes,
            true,
        )
        .await?;

        let mut shared_content_based_paths = Vec::new();
        if !bazel_paths {
            self.prepare_scratch_path(
                ctx,
                &cli_ctx,
                fs,
                &mut inputs,
                &mut shared_content_based_paths,
                &mut extra_env,
            )?;
        }

        if !bazel_paths {
            for output in self.outputs.iter() {
                if output.get_path().is_content_based_path() {
                    let full_path = cli_ctx
                        .resolve_project_path(fs.buck_out_path_resolver().resolve_gen(
                            output.get_path(),
                            Some(&ContentBasedPathHash::for_output_artifact()),
                        )?)?
                        .into_string();
                    shared_content_based_paths.push(full_path);
                }
            }
        }

        let host_sharing_tokens: BuckIndexSet<String> =
            shared_content_based_paths.into_iter().collect();
        let bazel_shared_execroot = if bazel_paths {
            Some(Self::bazel_execroot(fs))
        } else {
            None
        };
        let mut bazel_shared_action_primary_output = None;
        let outputs = self
            .outputs
            .iter()
            .map(|b| {
                let (produced_path, shared_output_path) =
                    if let Some(bazel_execroot) = bazel_execroot.as_deref() {
                        let bazel_path =
                            if let Some(path) = visitor.bazel_output_exec_paths.get(b.get_path()) {
                                path.clone()
                            } else {
                                let artifact = Artifact::from(b.dupe());
                                bazel_normalize_and_map_buck_owned_exec_paths(
                                    &bazel_artifact_path(artifact.get_path()),
                                    bazel_path_mapping,
                                )
                            };
                        (
                            Some(Self::bazel_execroot_path(
                                bazel_execroot,
                                bazel_path.clone(),
                            )?),
                            bazel_shared_execroot
                                .as_deref()
                                .map(|execroot| Self::bazel_execroot_path(execroot, bazel_path))
                                .transpose()?,
                        )
                    } else {
                        (None, None)
                    };
                if bazel_shared_action_primary_output.is_none() {
                    bazel_shared_action_primary_output = shared_output_path;
                }
                Ok(CommandExecutionOutput::BuildArtifact {
                    path: b.get_path().dupe(),
                    output_type: b.output_type(),
                    produced_path,
                })
            })
            .collect::<bz_error::Result<BuckIndexSet<_>>>()?;
        let outputs = CommandExecutionPaths::sort_outputs_for_execution(outputs, ctx.fs());

        // TODO(ianc) Only do this if we're actually going to run the action?
        let host_sharing_requirements = if !host_sharing_tokens.is_empty() {
            HostSharingRequirements::OnePerTokens(
                host_sharing_tokens.into_iter().collect::<Vec<_>>().into(),
                self.inner.weight,
            )
        } else {
            HostSharingRequirements::Shared(self.inner.weight)
        };

        let command_line_digest =
            fingerprint_expanded_command_line_for_local_action_cache(&expanded, &param_files);
        let local_action_cache_input_metadata = LocalActionCacheInputMetadata {
            input_set_digest: ctx.local_action_cache_input_set_digest(),
            extra_inputs: &local_action_cache_inputs[local_action_cache_extra_inputs_start..],
        };
        let local_action_cache_key = self.local_action_cache_key(
            ctx,
            &command_line_digest,
            &extra_env,
            local_action_cache_input_metadata,
            &outputs,
            worker.as_ref().map(LocalActionCacheWorkerRef::Borrowed),
            remote_worker
                .as_ref()
                .map(LocalActionCacheRemoteWorkerRef::Borrowed),
        )?;

        Ok((
            UnpreparedRunAction {
                expanded,
                extra_env,
                inputs,
                outputs,
                worker,
                remote_worker,
                local_action_cache_key,
                bazel_shared_action_primary_output,
                pending_action_metadata_writes,
            },
            expanded_command_line_digest_for_dep_files,
            host_sharing_requirements,
        ))
    }

    fn prepare_param_files(
        &self,
        fs: &ArtifactFs,
        param_files: &[RunActionParamFile],
        inputs: &mut Vec<CommandExecutionInput>,
        bazel_execroot: Option<&ProjectRelativePath>,
        pending_action_metadata_writes: &mut Vec<PendingActionMetadataWrite>,
    ) -> bz_error::Result<()> {
        for param_file in param_files {
            let project_rel_path = fs
                .buck_out_path_resolver()
                .resolve_gen(&param_file.path, Some(&param_file.content_hash))?;
            pending_action_metadata_writes.push(PendingActionMetadataWrite {
                path: param_file.path.dupe(),
                content_hash: param_file.content_hash.clone(),
                content: param_file.content.clone(),
            });

            let metadata = ActionMetadataBlob {
                digest: param_file.digest.dupe(),
                path: param_file.path.dupe(),
                content_hash: param_file.content_hash.clone(),
            };
            inputs.push(CommandExecutionInput::ActionMetadata(metadata.clone()));

            if let (Some(bazel_execroot), Some(bazel_exec_path)) =
                (bazel_execroot, &param_file.bazel_exec_path)
            {
                inputs.push(CommandExecutionInput::ArtifactPathAlias {
                    source_path: project_rel_path,
                    source_requires_materialization: true,
                    remote_cache_cas_info: None,
                    owner: None,
                    path: Self::bazel_execroot_path(bazel_execroot, bazel_exec_path.clone())?,
                    value: ArtifactValue::file(FileMetadata {
                        digest: param_file.digest.dupe(),
                        is_executable: false,
                    }),
                });
            }
        }
        Ok(())
    }

    /// Handle case when user requested file with action metadata to be generated.
    /// Generate content and output path for the file. It will be either passed
    /// to RE as a blob or written to disk in local executor.
    /// Path to this file is passed to user in environment variable which is selected by user.
    async fn prepare_action_metadata(
        &self,
        ctx: &dyn ActionExecutionCtx,
        cli_ctx: &DefaultCommandLineContext<'_>,
        fs: &ArtifactFs,
        visitor: &mut RunActionVisitor<'_>,
        inputs: &mut Vec<CommandExecutionInput>,
        local_action_cache_inputs: &mut Vec<CommandExecutionInput>,
        extra_env: &mut Vec<(String, String)>,
        pending_action_metadata_writes: &mut Vec<PendingActionMetadataWrite>,
        write_metadata: bool,
    ) -> bz_error::Result<()> {
        if let Some(metadata_param) = &self.inner.metadata_param {
            let path = BuildArtifactPath::new(
                ctx.target().owner().dupe(),
                metadata_param.path.clone(),
                if self.all_outputs_are_content_based() {
                    BuckOutPathKind::ContentHash
                } else {
                    BuckOutPathKind::Configuration
                },
            );

            let artifact_inputs: Vec<&ArtifactGroupValues> = visitor
                .incremental_metadata_inputs
                .iter()
                .map(|group| ctx.artifact_values(group))
                .collect();
            let (digest, content) = if write_metadata {
                let (data, digest) = metadata_content(fs, &artifact_inputs, ctx.digest_config())?;
                (digest, Some(data.0.0))
            } else {
                (
                    metadata_digest(fs, &artifact_inputs, ctx.digest_config())?,
                    None,
                )
            };
            let content_hash = ContentBasedPathHash::new(digest.raw_digest().as_bytes())?;
            let project_rel_path = fs
                .buck_out_path_resolver()
                .resolve_gen(&path, Some(&content_hash))?;
            if let Some(content) = content {
                pending_action_metadata_writes.push(PendingActionMetadataWrite {
                    path: path.dupe(),
                    content_hash: content_hash.clone(),
                    content,
                });
            }

            let metadata = ActionMetadataBlob {
                digest,
                path,
                content_hash,
            };
            inputs.push(CommandExecutionInput::ActionMetadata(metadata.clone()));
            local_action_cache_inputs.push(CommandExecutionInput::ActionMetadata(metadata));

            let env = cli_ctx
                .resolve_project_path(project_rel_path)?
                .into_string();
            extra_env.push((metadata_param.env_var.to_owned(), env));
        }
        Ok(())
    }

    fn prepare_scratch_path(
        &self,
        ctx: &dyn ActionExecutionCtx,
        cli_ctx: &DefaultCommandLineContext,
        fs: &ArtifactFs,
        inputs: &mut Vec<CommandExecutionInput>,
        shared_content_based_paths: &mut Vec<String>,
        extra_env: &mut Vec<(String, String)>,
    ) -> bz_error::Result<()> {
        let scratch = ctx.target().scratch_path();
        let scratch_path = cli_ctx
            .resolve_project_path(fs.buck_out_path_resolver().resolve_scratch(&scratch)?)?
            .into_string();

        if scratch.uses_content_hash() {
            shared_content_based_paths.push(scratch_path.to_owned());
        }

        extra_env.push(("BUCK_SCRATCH_PATH".to_owned(), scratch_path));
        inputs.push(CommandExecutionInput::ScratchPath(scratch));

        Ok(())
    }

    fn local_action_cache_key(
        &self,
        ctx: &dyn ActionExecutionCtx,
        command_line_digest: &ExpandedCommandLineDigest,
        extra_env: &[(String, String)],
        local_action_cache_input_metadata: LocalActionCacheInputMetadata<'_>,
        outputs: &BuckIndexSet<CommandExecutionOutput>,
        worker: Option<LocalActionCacheWorkerRef<'_>>,
        remote_worker: Option<LocalActionCacheRemoteWorkerRef<'_>>,
    ) -> bz_error::Result<Option<LocalActionCacheKey>> {
        let Some(_) = outputs.iter().next() else {
            return Ok(None);
        };

        let cas_digest_config = ctx.digest_config().cas_digest_config();
        let mut action_key = CasDigestData::digester(cas_digest_config);
        action_cache_add_str(&mut action_key, "buck2-local-action-cache-action-key-v3");
        action_cache_add_str(&mut action_key, &ctx.fs().fs().root().to_string());
        action_cache_add_debug(&mut action_key, self.inner.executor_preference);
        action_cache_add_option_duration(&mut action_key, self.inner.timeout);
        action_cache_add_bool(&mut action_key, self.inner.no_outputs_cleanup);
        action_cache_add_bool(&mut action_key, self.inner.unique_input_inodes);
        action_cache_add_option_bool(&mut action_key, self.inner.bazel_use_default_shell_env);
        action_cache_add_debug(&mut action_key, &self.inner.remote_execution_dependencies);
        action_cache_add_debug(&mut action_key, &self.inner.re_gang_workers);
        action_cache_add_debug(&mut action_key, &self.inner.remote_execution_custom_image);
        action_cache_add_debug(&mut action_key, &self.inner.remote_execution_extra_params);

        action_cache_add_str(&mut action_key, "command_line");
        action_cache_add_bytes(&mut action_key, command_line_digest.as_bytes());
        action_cache_add_str(&mut action_key, "extra_env");
        for (key, value) in extra_env {
            action_cache_add_str(&mut action_key, key);
            action_cache_add_str(&mut action_key, value);
        }

        action_cache_add_str(&mut action_key, "outputs");
        for output in outputs {
            fingerprint_command_execution_output(&mut action_key, ctx.fs(), output)?;
        }

        action_cache_add_str(&mut action_key, "worker");
        if let Some(worker) = worker.as_ref() {
            action_cache_add_worker_id(&mut action_key, worker.id());
            action_cache_add_worker_protocol(&mut action_key, worker.protocol());
            action_cache_add_option_usize(&mut action_key, worker.concurrency());
            action_cache_add_bool(&mut action_key, worker.streaming());
            action_cache_add_bool(&mut action_key, worker.bazel_worker_sandboxing());
            action_cache_add_option_tracked_file_digest(&mut action_key, worker.remote_key());
            for arg in worker.exe() {
                action_cache_add_str(&mut action_key, arg);
            }
            for (key, value) in worker.env() {
                action_cache_add_str(&mut action_key, key);
                action_cache_add_str(&mut action_key, value);
            }
        }

        action_cache_add_str(&mut action_key, "remote_worker");
        if let Some(remote_worker) = remote_worker.as_ref() {
            action_cache_add_worker_id(&mut action_key, remote_worker.id());
            action_cache_add_option_usize(&mut action_key, remote_worker.concurrency());
            for arg in remote_worker.init() {
                action_cache_add_str(&mut action_key, arg);
            }
            for (key, value) in remote_worker.env() {
                action_cache_add_str(&mut action_key, key);
                action_cache_add_str(&mut action_key, value);
            }
        }

        let mut input_metadata = CasDigestData::digester(cas_digest_config);
        action_cache_add_str(
            &mut input_metadata,
            "buck2-local-action-cache-input-metadata-v2",
        );
        action_cache_add_str(&mut input_metadata, "artifact_input_set");
        action_cache_add_bytes(
            &mut input_metadata,
            local_action_cache_input_metadata.input_set_digest,
        );
        action_cache_add_str(&mut input_metadata, "extra_inputs");
        for input in local_action_cache_input_metadata.extra_inputs {
            fingerprint_command_execution_input(&mut input_metadata, ctx.fs(), input)?;
        }
        action_cache_add_str(&mut input_metadata, "worker_input_directory");
        if let Some(worker) = worker.as_ref() {
            action_cache_add_bool(&mut input_metadata, true);
            action_cache_add_str(&mut input_metadata, "worker_inputs");
            for input in worker.inputs() {
                fingerprint_command_execution_input(&mut input_metadata, ctx.fs(), input)?;
            }
        } else {
            action_cache_add_bool(&mut input_metadata, false);
        }
        action_cache_add_str(&mut input_metadata, "remote_worker_input_directory");
        if let Some(remote_worker) = remote_worker.as_ref() {
            action_cache_add_bool(&mut input_metadata, true);
            action_cache_add_str(&mut input_metadata, "remote_worker_inputs");
            for input in remote_worker.inputs() {
                fingerprint_command_execution_input(&mut input_metadata, ctx.fs(), input)?;
            }
        } else {
            action_cache_add_bool(&mut input_metadata, false);
        }

        let action_key_digest = finalize_action_cache_digest(action_key);
        let input_metadata_digest = finalize_action_cache_digest(input_metadata);
        let fingerprint = compose_local_action_cache_fingerprint(
            cas_digest_config,
            &action_key_digest,
            &input_metadata_digest,
        );
        let key = format!("action-key:{}", hex::encode(&action_key_digest));

        Ok(Some(LocalActionCacheKey {
            key,
            action_key_digest,
            input_metadata_digest,
            fingerprint,
        }))
    }

    pub(crate) async fn check_cache_result_is_useable(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        request: &CommandExecutionRequest,
        action_digest: &ActionDigest,
        result: CommandExecutionResult,
        dep_file_bundle: &DepFileBundle,
        remote_dep_file_key: &DepFileDigest,
    ) -> bz_error::Result<ControlFlow<CommandExecutionResult, ()>> {
        // If it's served by the regular action cache no need to verify anything here.
        if !result.was_served_by_remote_dep_file_cache() {
            return Ok(ControlFlow::Break(result));
        }

        if let Some(found_dep_file_entry) = &result.dep_file_metadata {
            let can_use = span_async_simple(
                bz_data::MatchDepFilesStart {
                    checking_filtered_inputs: true,
                    remote_cache: true,
                },
                dep_file_bundle.check_remote_dep_file_entry(
                    ctx.digest_config(),
                    ctx.fs(),
                    ctx.materializer(),
                    found_dep_file_entry,
                    &result,
                ),
                bz_data::MatchDepFilesEnd {},
            )
            .await?;

            if can_use {
                tracing::info!(
                    "Action result is cached via remote dep file cache, skipping execution of :\n```\n$ {}\n```\n for action `{}` with remote dep file key `{}`",
                    request.all_args_str(),
                    action_digest,
                    &remote_dep_file_key,
                );
                return Ok(ControlFlow::Break(result));
            }
        }
        // Continue through other options below
        Ok(ControlFlow::Continue(()))
    }

    async fn execute_inner(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        mut waiting_data: WaitingData,
    ) -> Result<ExecuteResult, ExecuteError> {
        let incremental_action_ignore_tags = self
            .inner
            .metadata_param
            .as_ref()
            .map(|metadata_param| &metadata_param.ignore_tags);
        let mut run_action_visitor =
            RunActionVisitor::new(&self.inner.dep_files, incremental_action_ignore_tags);
        waiting_data.start_waiting_category_now(WaitingCategory::PreparingAction);
        if let Some((local_action_cache_key, outputs)) = self
            .prepare_local_action_cache_probe(&mut run_action_visitor, ctx)
            .await?
        {
            waiting_data.start_waiting_category_now(WaitingCategory::CheckingCaches);
            let manager = ctx.command_execution_manager(waiting_data.clone());
            match ctx
                .unprepared_action_cache(manager, &local_action_cache_key, &outputs)
                .await
            {
                ControlFlow::Break(result) => {
                    return Ok(ExecuteResult::LocalActionCacheHit {
                        result,
                        executor_preference: self.inner.executor_preference,
                    });
                }
                ControlFlow::Continue(_) => {
                    waiting_data.start_waiting_category_now(WaitingCategory::PreparingAction);
                    run_action_visitor = RunActionVisitor::new(
                        &self.inner.dep_files,
                        incremental_action_ignore_tags,
                    );
                }
            }
        }

        let (unprepared_run_action, cmdline_digest_for_dep_files, host_sharing_requirements) =
            self.prepare(&mut run_action_visitor, ctx).await?;

        waiting_data.start_waiting_category_now(WaitingCategory::CheckingCaches);
        let manager = ctx.command_execution_manager(waiting_data);
        let manager = if let Some(local_action_cache_key) =
            unprepared_run_action.local_action_cache_key.as_ref()
        {
            match ctx
                .unprepared_action_cache(
                    manager,
                    local_action_cache_key,
                    &unprepared_run_action.outputs,
                )
                .await
            {
                ControlFlow::Break(result) => {
                    return Ok(ExecuteResult::LocalActionCacheHit {
                        result,
                        executor_preference: self.inner.executor_preference,
                    });
                }
                ControlFlow::Continue(manager) => manager,
            }
        } else {
            manager
        };

        unprepared_run_action
            .declare_action_metadata_writes(ctx)
            .await?;

        let prepared_run_action = unprepared_run_action.into_prepared(
            ctx.fs(),
            ctx.digest_config(),
            ctx.run_action_knobs().action_paths_interner.as_ref(),
        )?;

        let dep_file_bundle = cmdline_digest_for_dep_files
            .map(|cmdline_digest_for_dep_files| {
                make_dep_file_bundle(
                    ctx,
                    run_action_visitor.dep_files_visitor,
                    cmdline_digest_for_dep_files,
                    &prepared_run_action.paths,
                    prepared_run_action.worker.as_ref().map(|w| &w.input_paths),
                )
            })
            .transpose()?;

        // First, check in the local dep file cache if an identical action can be found there.
        // Do this before checking the action cache as we can avoid a potentially large download.
        // Once the action cache lookup misses, we will do the full dep file cache look up.
        let (outputs, should_fully_check_dep_file_cache) =
            if let Some(dep_file_bundle) = dep_file_bundle.as_ref() {
                dep_file_bundle
                    .check_local_dep_file_cache_for_identical_action(ctx, self.outputs.as_slice())
                    .await?
            } else {
                (None, false)
            };
        if let Some((outputs, metadata)) = outputs {
            return Ok(ExecuteResult::LocalDepFileHit(outputs, metadata));
        }

        let req =
            self.command_execution_request(ctx, prepared_run_action, host_sharing_requirements)?;

        // Prepare the action, check the action cache, fully check the local dep file cache if needed, then execute the command
        let prepared_action = ctx.prepare_action(&req, true).await?;

        let action_cache_result = ctx.action_cache(manager, &req, &prepared_action).await;

        let (req, result) = match action_cache_result {
            ControlFlow::Break(_) => (req, action_cache_result),
            ControlFlow::Continue(manager) => {
                // If we didn't find anything in the action cache, first do a local dep file cache lookup, and if that fails,
                // try to find a remote dep file cache hit.
                if should_fully_check_dep_file_cache {
                    if let Some(dep_file_bundle) = dep_file_bundle.as_ref() {
                        let lookup = dep_file_bundle
                            .check_local_dep_file_cache(ctx, self.outputs.as_slice())
                            .await?;
                        if let Some((outputs, metadata)) = lookup {
                            return Ok(ExecuteResult::LocalDepFileHit(outputs, metadata));
                        }
                    }
                }

                let supports_remote_dep_files = self.inner.allow_dep_file_cache_upload
                    && dep_file_bundle
                        .as_ref()
                        .is_some_and(DepFileBundle::has_dep_files);

                // Enable remote dep file cache lookup for actions that have remote depfile uploads enabled.
                if supports_remote_dep_files {
                    let dep_file_bundle = dep_file_bundle
                        .as_ref()
                        .expect("remote dep-file cache requires a dep-file bundle");
                    let remote_dep_file_key = dep_file_bundle
                        .remote_dep_file_action(
                            ctx.digest_config(),
                            ctx.mergebase().0.as_ref(),
                            ctx.re_platform(),
                        )
                        .action
                        .coerce();
                    let req = req.with_remote_dep_file_key(&remote_dep_file_key);
                    let remote_dep_file_result = ctx
                        .remote_dep_file_cache(manager, &req, &prepared_action)
                        .await;
                    if let ControlFlow::Break(res) = remote_dep_file_result {
                        // If the result was served by the remote dep file cache, we can't use the result just yet. We need to verify that
                        // the inputs tracked by a depfile that was actually used in the cache hit are identical to the inputs we have for this action.
                        let res = self
                            .check_cache_result_is_useable(
                                ctx,
                                &req,
                                &prepared_action.action_and_blobs.action,
                                res,
                                dep_file_bundle,
                                &remote_dep_file_key,
                            )
                            .await?;
                        (
                            req,
                            res.map_continue(|_| ctx.command_execution_manager(WaitingData::new())),
                        )
                    } else {
                        (req, remote_dep_file_result)
                    }
                } else {
                    (req, ControlFlow::Continue(manager))
                }
            }
        };

        // If the cache queries did not yield to a result, then we need to execute the action.
        let (result, req, action_and_blobs) = match result {
            ControlFlow::Break(res) => (res, req, prepared_action.action_and_blobs),
            ControlFlow::Continue(mut manager) => {
                manager
                    .inner
                    .waiting_data
                    .start_waiting_category_now(WaitingCategory::PreparingExecution);
                let (req, prepared_action) = if self.inner.incremental_remote_outputs {
                    // For the case of incremental remote outputs, we checked the caches using the action which
                    // does not include the outputs as inputs.
                    // To execute such action we first prepare a different action with the outputs added as inputs.
                    let output_paths_as_inputs = self.output_paths_as_inputs(ctx).await?;
                    if !output_paths_as_inputs.is_empty() {
                        let executor_fs = ctx.executor_fs();
                        let fs = executor_fs.fs();
                        let digest_config = ctx.digest_config();
                        let override_req = req.with_outputs_paths_added_as_inputs(
                            output_paths_as_inputs,
                            fs,
                            digest_config,
                            ctx.run_action_knobs().action_paths_interner.as_ref(),
                        )?;
                        let override_prepared_action =
                            ctx.prepare_action(&override_req, true).await?;
                        (override_req, override_prepared_action)
                    } else {
                        (req, prepared_action)
                    }
                } else {
                    (req, prepared_action)
                };
                let execution_result = ctx.exec_cmd(manager, &req, &prepared_action).await;
                (execution_result, req, prepared_action.action_and_blobs)
            }
        };

        let input_files_bytes = req.paths().input_files_bytes();
        Ok(ExecuteResult::ExecutedOrReHit {
            result,
            dep_file_bundle,
            executor_preference: req.executor_preference,
            request: req,
            action_and_blobs,
            input_files_bytes,
        })
    }

    async fn output_paths_as_inputs(
        &self,
        ctx: &dyn ActionExecutionCtx,
    ) -> bz_error::Result<Vec<CommandExecutionInput>> {
        let executor_fs = ctx.executor_fs();
        let fs = executor_fs.fs();
        let output_paths = {
            let mut output_paths = Vec::new();
            for output in &self.outputs {
                // TODO(T219919866): support content based paths
                let path = fs.resolve_build(output.get_path(), None)?;
                output_paths.push(path);
            }
            output_paths
        };
        let entries = ctx
            .materializer()
            .get_artifact_entries_for_materialized_paths(output_paths, false)
            .await?;
        // Only proceed with incremental outputs if every output is present
        Ok(entries
            .into_iter()
            .map(|entry| entry.map(|(p, e)| CommandExecutionInput::IncrementalRemoteOutput(p, e)))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default())
    }

    fn command_execution_request(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        prepared_run_action: PreparedRunAction,
        host_sharing_requirements: HostSharingRequirements,
    ) -> bz_error::Result<CommandExecutionRequest> {
        let outputs_for_error_handler = self.outputs_for_error_handler()?;
        let local_environment_inheritance = match self.inner.bazel_use_default_shell_env {
            None | Some(true) => EnvironmentInheritance::local_command_exclusions(),
            Some(false) => EnvironmentInheritance::empty(),
        };
        let mut req = prepared_run_action
            .into_command_execution_request()
            .with_prefetch_lossy_stderr(true)
            .with_executor_preference(self.inner.executor_preference)
            .with_host_sharing_requirements(host_sharing_requirements.into())
            .with_low_pass_filter(self.inner.low_pass_filter)
            .with_outputs_cleanup(!self.inner.no_outputs_cleanup)
            .with_local_environment_inheritance(local_environment_inheritance)
            .with_force_full_hybrid_if_capable(self.inner.force_full_hybrid_if_capable)
            .with_unique_input_inodes(self.inner.unique_input_inodes)
            .with_remote_execution_dependencies(self.inner.remote_execution_dependencies.to_vec())
            .with_re_gang_workers(self.inner.re_gang_workers.to_vec())
            .with_remote_execution_custom_image(
                self.inner.remote_execution_custom_image.clone().map(|s| *s),
            )
            .with_remote_execution_extra_params(self.inner.remote_execution_extra_params.clone())
            .with_force_remote_input_reupload(ctx.force_remote_input_reupload())
            .with_outputs_for_error_handler(outputs_for_error_handler);

        if self.uses_bazel_execroot_paths() {
            req = req.with_working_directory(
                self.bazel_action_execroot(ctx.executor_fs().fs(), ctx.target())?,
            );
        }

        if let Some(timeout) = self.inner.timeout {
            req = req.with_timeout(timeout);
        }

        if self.inner.no_outputs_cleanup {
            if self
                .outputs
                .iter()
                .any(|o| o.get_path().is_content_based_path())
            {
                req = req.with_run_action_key(Some(
                    // Using string representation as it is going to be stored in db which requires it to be a string
                    // doing it early here prevents us from exposing RunActionKey type
                    RunActionKey::from_action_execution_target(ctx.target()).to_string(),
                ));
            }
        }

        Ok(req)
    }

    fn outputs_for_error_handler(&self) -> bz_error::Result<Vec<BuildArtifactPath>> {
        self.starlark_values
            .outputs_for_error_handler
            .iter()
            .map(|artifact| {
                let a = artifact.inner().artifact();

                match a.as_parts().0 {
                    BaseArtifactKind::Source(s) => Err(bz_error::bz_error!(
                        bz_error::ErrorTag::Input,
                        "Cannot use source artifact `{}` as output for error handler",
                        s.get_path()
                    )),
                    BaseArtifactKind::Build(b) => Ok(b.get_path().dupe()),
                }
            })
            .collect()
    }
}

pub(crate) struct UnpreparedRunAction {
    expanded: ExpandedCommandLine,
    /// Environment which is added on top of the one coming from `ExpandedCommandLine::env`
    extra_env: Vec<(String, String)>,
    inputs: Vec<CommandExecutionInput>,
    outputs: BuckIndexSet<CommandExecutionOutput>,
    worker: Option<WorkerSpec>,
    remote_worker: Option<RemoteWorkerSpec>,
    local_action_cache_key: Option<LocalActionCacheKey>,
    bazel_shared_action_primary_output: Option<ProjectRelativePathBuf>,
    pending_action_metadata_writes: Vec<PendingActionMetadataWrite>,
}

impl UnpreparedRunAction {
    async fn declare_action_metadata_writes(
        &self,
        ctx: &dyn ActionExecutionCtx,
    ) -> bz_error::Result<()> {
        let fs = ctx.fs();
        for write in &self.pending_action_metadata_writes {
            let path = fs
                .buck_out_path_resolver()
                .resolve_gen(&write.path, Some(&write.content_hash))?;
            let content = write.content.clone();
            let configuration_path = ctx
                .materializer()
                .maybe_eager_configuration_path(fs, &write.path)?;
            ctx.materializer()
                .declare_write(Box::new(move || {
                    Ok(vec![WriteRequest {
                        path,
                        content,
                        is_executable: false,
                        configuration_path,
                    }])
                }))
                .await
                .buck_error_context("Failed to write action metadata!")?;
        }
        Ok(())
    }

    fn into_prepared(
        self,
        fs: &ArtifactFs,
        digest_config: DigestConfig,
        interner: Option<&DashMapDirectoryInterner<ActionDirectoryMember, TrackedFileDigest>>,
    ) -> bz_error::Result<PreparedRunAction> {
        let Self {
            expanded,
            extra_env,
            inputs,
            outputs,
            worker,
            remote_worker,
            local_action_cache_key,
            bazel_shared_action_primary_output,
            pending_action_metadata_writes: _,
        } = self;
        let paths = CommandExecutionPaths::new(inputs, outputs, fs, digest_config, interner)?;
        Ok(PreparedRunAction {
            expanded,
            extra_env,
            paths,
            worker,
            remote_worker,
            local_action_cache_key,
            bazel_shared_action_primary_output,
        })
    }
}

pub(crate) struct PreparedRunAction {
    expanded: ExpandedCommandLine,
    /// Environment which is added on top of the one coming from `ExpandedCommandLine::env`
    extra_env: Vec<(String, String)>,
    paths: CommandExecutionPaths,
    worker: Option<WorkerSpec>,
    remote_worker: Option<RemoteWorkerSpec>,
    local_action_cache_key: Option<LocalActionCacheKey>,
    bazel_shared_action_primary_output: Option<ProjectRelativePathBuf>,
}

impl PreparedRunAction {
    fn into_command_execution_request(self) -> CommandExecutionRequest {
        let Self {
            expanded: ExpandedCommandLine { exe, args, mut env },
            extra_env,
            paths,
            worker,
            remote_worker,
            local_action_cache_key,
            bazel_shared_action_primary_output,
        } = self;

        for (k, v) in extra_env {
            env.insert(k, v);
        }

        CommandExecutionRequest::new(exe, args, paths, env)
            .with_worker(worker)
            .with_remote_worker(remote_worker)
            .with_bazel_shared_action_primary_output(bazel_shared_action_primary_output)
            .with_local_action_cache_key(local_action_cache_key)
    }
}

pub struct RunActionVisitor<'a> {
    pub(crate) dep_files_visitor: DepFilesCommandLineVisitor<'a>,
    pub(crate) incremental_metadata_inputs: Vec<ArtifactGroup>,
    pub(crate) bazel_output_exec_paths: BuckIndexMap<BuildArtifactPath, String>,
    incremental_metadata_ignore_tags: Option<&'a SmallSet<ArtifactTag>>,
}

impl<'a> RunActionVisitor<'a> {
    pub(crate) fn new(
        dep_files: &'a RunActionDepFiles,
        incremental_metadata_ignore_tags: Option<&'a SmallSet<ArtifactTag>>,
    ) -> Self {
        Self {
            dep_files_visitor: DepFilesCommandLineVisitor::new(dep_files),
            incremental_metadata_inputs: Vec::new(),
            bazel_output_exec_paths: BuckIndexMap::new(),
            incremental_metadata_ignore_tags,
        }
    }

    pub(crate) fn inputs(&self) -> impl Iterator<Item = &ArtifactGroup> {
        self.dep_files_visitor.inputs()
    }
}

impl<'v> CommandLineArtifactVisitor<'v> for RunActionVisitor<'v> {
    fn visit_input(&mut self, input: ArtifactGroup, tags: Vec<&ArtifactTag>) {
        // If incremental_metadata_ignore_tags is None, then we're not going to produce
        // incremental metadata at all, so there's nothing to do here.
        if let Some(ignore_tags) = self.incremental_metadata_ignore_tags {
            if !tags.iter().any(|t| ignore_tags.contains(*t)) {
                self.incremental_metadata_inputs.push(input.dupe());
            }
        }

        self.dep_files_visitor.visit_input(input, tags);
    }

    fn visit_declared_output(&mut self, artifact: OutputArtifact<'v>, tags: Vec<&ArtifactTag>) {
        let bazel_output_exec_path = {
            let path = artifact.get_path();
            let build_path = match path.base_path.as_ref() {
                Either::Left(build_path) => Some((**build_path).dupe()),
                Either::Right(_) => None,
            };
            build_path.map(|build_path| {
                let exec_path = bazel_normalize_buck_owned_exec_paths(&bazel_artifact_path(path));
                (build_path, exec_path)
            })
        };
        if let Some((build_path, exec_path)) = bazel_output_exec_path {
            self.bazel_output_exec_paths.insert(build_path, exec_path);
        }
        self.dep_files_visitor.visit_declared_output(artifact, tags);
    }

    fn visit_frozen_output(&mut self, artifact: Artifact, tags: Vec<&ArtifactTag>) {
        if let BaseArtifactKind::Build(build) = artifact.as_parts().0 {
            self.bazel_output_exec_paths.insert(
                build.get_path().dupe(),
                bazel_normalize_buck_owned_exec_paths(&bazel_artifact_path(artifact.get_path())),
            );
        }
        self.dep_files_visitor.visit_frozen_output(artifact, tags);
    }
}

impl RunAction {
    /// Execute for offline builds by restoring from cache.
    /// Returns None if cache miss, Some if hit.
    async fn execute_for_offline(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
    ) -> bz_error::Result<Option<(ActionOutputs, ActionExecutionMetadata)>> {
        // Collect references to all outputs
        let output_refs: Vec<&BuildArtifact> = self.outputs.iter().collect();

        // Try to restore ALL outputs - any miss = total miss
        match offline::declare_copy_from_offline_cache(ctx, &output_refs).await {
            Ok(outputs) => Ok(Some((
                outputs,
                ActionExecutionMetadata {
                    execution_kind: ActionExecutionKind::Deferred,
                    timing: ActionExecutionTimingData::default(),
                    input_files_bytes: None,
                    waiting_data: WaitingData::new(),
                    remote_cache_origin: None,
                },
            ))),
            Err(_) => {
                // Cache miss - return None to fall through to normal execution
                Ok(None)
            }
        }
    }
}

#[async_trait]
impl Action for RunAction {
    fn kind(&self) -> bz_data::ActionKind {
        bz_data::ActionKind::Run
    }

    fn inputs(&self) -> bz_error::Result<Cow<'_, [ArtifactGroup]>> {
        Ok(Cow::Borrowed(&self.inputs))
    }

    fn local_action_cache_inputs(&self) -> bz_error::Result<Option<Cow<'_, [ArtifactGroup]>>> {
        if self.local_action_cache_inputs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Cow::Borrowed(&self.local_action_cache_inputs)))
        }
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(self.outputs.as_slice())
    }

    fn first_output(&self) -> &BuildArtifact {
        // Required to have outputs on construction
        &self.outputs.as_slice()[0]
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new(self.starlark_values.category.as_str())
    }

    fn identifier(&self) -> Option<&str> {
        self.starlark_values.identifier.map(|x| x.as_str())
    }

    fn always_print_stderr(&self) -> bool {
        self.inner.always_print_stderr
    }

    fn is_expected_eligible_for_dedupe(&self) -> Option<bool> {
        self.inner.expected_eligible_for_dedupe
    }

    fn executor_preference(&self) -> Option<ExecutorPreference> {
        Some(self.inner.executor_preference)
    }

    fn eager_materialization_enabled(&self) -> bool {
        self.inner.eager_materialization_enabled
    }

    fn aquery_attributes(
        &self,
        fs: &ExecutorFs,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> BuckIndexMap<String, String> {
        let (cmd, error) = match self.aquery_command(fs, artifact_path_mapping) {
            Ok(cmd) => (cmd, None),
            Err(error) => {
                let error = error.to_string();
                (
                    format!("ERROR: constructing command ({error})"),
                    Some(error),
                )
            }
        };
        let mut attributes = buck_indexmap! {
            "cmd".to_owned() => cmd,
            "executor_preference".to_owned() => self.inner.executor_preference.to_string(),
            "always_print_stderr".to_owned() => self.inner.always_print_stderr.to_string(),
            "weight".to_owned() => self.inner.weight.to_string(),
            "dep_files".to_owned() => self.inner.dep_files.to_string(),
            "metadata_param".to_owned() => match &self.inner.metadata_param {
                None => "None".to_owned(),
                Some(x) => x.to_string(),
            },
            "no_outputs_cleanup".to_owned() => self.inner.no_outputs_cleanup.to_string(),
            "allow_cache_upload".to_owned() => match &self.inner.allow_cache_upload {
                None => "None".to_owned(),
                Some(x) => x.to_string(),
            },
            "allow_dep_file_cache_upload".to_owned() => self.inner.allow_dep_file_cache_upload.to_string(),
        };
        if let Some(error) = error {
            attributes.insert("error".to_owned(), error);
        }
        attributes
    }

    fn error_handler(&self) -> Option<&OwnedFrozenValue> {
        self.error_handler.as_ref()
    }

    fn failed_action_output_artifacts<'v>(
        &self,
        artifact_fs: &ArtifactFs,
        heap: Heap<'v>,
        outputs: Option<&ActionOutputs>,
    ) -> bz_error::Result<ValueOfUnchecked<'v, DictType<StarlarkArtifact, StarlarkArtifactValue>>>
    {
        let mut artifact_value_dict =
            Vec::with_capacity(self.starlark_values.outputs_for_error_handler.len());

        for x in self.starlark_values.outputs_for_error_handler.iter() {
            let artifact = x.inner().artifact();

            let content_based_path_hash = if artifact.path_resolution_requires_artifact_value() {
                let outputs = outputs.ok_or_else(|| {
                    bz_error::bz_error!(
                        bz_error::ErrorTag::Input,
                        "Action failed with no outputs available"
                    )
                })?;
                let artifact_value = outputs
                    .get_from_artifact_path(&artifact.get_path())
                    .ok_or_else(|| {
                        bz_error::bz_error!(
                            bz_error::ErrorTag::Input,
                            "ArtifactValue for artifact `{}` was not found in action outputs",
                            artifact.get_path()
                        )
                    })?;
                Some(artifact_value.content_based_path_hash())
            } else {
                None
            };

            let path = artifact
                .get_path()
                .resolve(artifact_fs, content_based_path_hash.as_ref())?;

            let abs = artifact_fs.fs().resolve(&path);
            // Check if the output file specified exists. We will return an error if it doesn't
            if !fs_util::try_exists(&abs)? {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "Output '{}' defined for error handler does not exist. This is likely due to file not being created, please ensure the action would produce an output",
                    &path
                ));
            }

            let artifact_value = StarlarkArtifactValue::new(
                artifact.dupe(),
                path.to_owned(),
                artifact_fs.fs().dupe(),
            );
            let artifact = StarlarkArtifact::new(artifact);

            artifact_value_dict.push((artifact, artifact_value));
        }

        Ok(heap
            .alloc_typed_unchecked(AllocDict(artifact_value_dict))
            .cast())
    }

    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        // Check offline cache first if parameter enabled
        if self.inner.allow_offline_output_cache
            && ctx.run_action_knobs().use_network_action_output_cache
        {
            if let Some((outputs, metadata)) = self.execute_for_offline(ctx).await? {
                return Ok((outputs, metadata));
            }
            // Cache miss - fall through to normal execution
        }

        let allow_cache_upload = self
            .inner
            .allow_cache_upload
            .unwrap_or_else(|| ctx.run_action_knobs().default_allow_cache_upload);
        let incremental_kind = match (
            self.inner.no_outputs_cleanup,
            self.inner.incremental_remote_outputs,
        ) {
            (true, true) => bz_data::IncrementalKind::IncrementalLocalAndRemote,
            (false, true) => bz_data::IncrementalKind::IncrementalRemote,
            (true, false) => bz_data::IncrementalKind::IncrementalLocal,
            (false, false) => bz_data::IncrementalKind::NonIncremental,
        };

        let (
            mut result,
            mut dep_file_bundle,
            executor_preference,
            request,
            action_and_blobs,
            input_files_bytes,
        ) = match self.execute_inner(ctx, waiting_data).await? {
            ExecuteResult::LocalDepFileHit(outputs, metadata) => {
                return Ok((outputs, metadata));
            }
            ExecuteResult::LocalActionCacheHit {
                result,
                executor_preference,
            } => {
                return ctx.unpack_command_execution_result(
                    executor_preference,
                    result,
                    allow_cache_upload,
                    self.inner.allow_dep_file_cache_upload,
                    None,
                    incremental_kind,
                );
            }
            ExecuteResult::ExecutedOrReHit {
                result,
                dep_file_bundle,
                executor_preference,
                request,
                action_and_blobs,
                input_files_bytes,
            } => (
                result,
                dep_file_bundle,
                executor_preference,
                request,
                action_and_blobs,
                input_files_bytes,
            ),
        };

        let supports_remote_dep_files = self.inner.allow_dep_file_cache_upload
            && dep_file_bundle
                .as_ref()
                .is_some_and(DepFileBundle::has_dep_files);

        // If there is a dep file entry AND if dep file cache upload is enabled, upload it
        if result.was_success()
            && !result.was_served_by_remote_dep_file_cache()
            && (allow_cache_upload || supports_remote_dep_files || force_cache_upload()?)
        {
            let re_result = result.action_result.take();
            let upload_result = ctx
                .cache_upload(
                    &action_and_blobs,
                    &request,
                    &result,
                    re_result,
                    // match needed for coercion, https://github.com/rust-lang/rust/issues/108999
                    if supports_remote_dep_files {
                        dep_file_bundle
                            .as_mut()
                            .map(|dep_file_bundle| dep_file_bundle as &mut dyn IntoRemoteDepFile)
                    } else {
                        None
                    },
                )
                .await?;

            result.did_cache_upload = upload_result.did_cache_upload;
            result.did_dep_file_cache_upload = upload_result.did_dep_file_cache_upload;
            result.dep_file_key = upload_result.dep_file_cache_upload_key;
        }

        if result.was_success()
            && !result.was_locally_executed()
            && let Some(local_action_cache_key) = request.local_action_cache_key()
            && let Some(remote_cache_origin) = result.remote_cache_origin.clone()
        {
            ctx.insert_unprepared_action_cache_metadata(
                local_action_cache_key,
                &result.outputs,
                Some(remote_cache_origin),
            )
            .buck_error_context(
                "Failed to persist remote output metadata in the local action cache",
            )?;
        }

        let was_locally_executed = result.was_locally_executed();
        let (outputs, metadata) = ctx.unpack_command_execution_result(
            executor_preference,
            result,
            allow_cache_upload,
            self.inner.allow_dep_file_cache_upload,
            Some(input_files_bytes),
            incremental_kind,
        )?;

        // Cache outputs if tracing and parameter enabled
        if self.inner.allow_offline_output_cache {
            let io_provider = ctx.io_provider();
            if let Some(tracer) = TracingIoProvider::from_io(&*io_provider) {
                for output in self.outputs.iter() {
                    if let Some(value) = outputs.get(output.get_path()) {
                        let offline_cache_path = offline::declare_copy_to_offline_output_cache(
                            ctx,
                            output,
                            value.dupe(),
                        )
                        .await?;
                        tracer.add_buck_out_entry(offline_cache_path);
                    }
                }
            }
        }

        if let Some(dep_file_bundle) = dep_file_bundle {
            populate_dep_files(ctx, dep_file_bundle, &outputs, was_locally_executed).await?;
        }

        Ok((outputs, metadata))
    }

    async fn try_execute_local_action_cache(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        mut waiting_data: WaitingData,
    ) -> Result<Option<(ActionOutputs, ActionExecutionMetadata)>, ExecuteError> {
        let incremental_action_ignore_tags = self
            .inner
            .metadata_param
            .as_ref()
            .map(|metadata_param| &metadata_param.ignore_tags);
        let mut run_action_visitor =
            RunActionVisitor::new(&self.inner.dep_files, incremental_action_ignore_tags);

        waiting_data.start_waiting_category_now(WaitingCategory::PreparingAction);
        let Some((local_action_cache_key, outputs)) = self
            .prepare_local_action_cache_probe(&mut run_action_visitor, ctx)
            .await?
        else {
            return Ok(None);
        };

        waiting_data.start_waiting_category_now(WaitingCategory::CheckingCaches);
        let manager = ctx.command_execution_manager(waiting_data);
        match ctx
            .unprepared_action_cache(manager, &local_action_cache_key, &outputs)
            .await
        {
            ControlFlow::Break(result) => {
                let allow_cache_upload = self
                    .inner
                    .allow_cache_upload
                    .unwrap_or_else(|| ctx.run_action_knobs().default_allow_cache_upload);
                let incremental_kind = match (
                    self.inner.no_outputs_cleanup,
                    self.inner.incremental_remote_outputs,
                ) {
                    (true, true) => bz_data::IncrementalKind::IncrementalLocalAndRemote,
                    (false, true) => bz_data::IncrementalKind::IncrementalRemote,
                    (true, false) => bz_data::IncrementalKind::IncrementalLocal,
                    (false, false) => bz_data::IncrementalKind::NonIncremental,
                };

                ctx.unpack_command_execution_result(
                    self.inner.executor_preference,
                    result,
                    allow_cache_upload,
                    self.inner.allow_dep_file_cache_upload,
                    None,
                    incremental_kind,
                )
                .map(Some)
            }
            ControlFlow::Continue(_) => Ok(None),
        }
    }
}
