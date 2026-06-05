/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! This module contains support for running actions and asynchronous providers
//!
//! An 'Action' is a unit of work with a set of input files known as 'Artifact's that are required
//! for its execution, and a set of output files called 'BuildArtifact's that are created by its
//! execution. Each 'Action' registered by a rule will only be executed when it's 'BuildArtifact's
//! are requested to be available. It will be guaranteed by the action system that all input
//! 'Artifact's are available before the execution of an 'Action'.
//!
//! 'Actions' struct will act as a general registry where users can create new 'Artifact's that
//! represent the outputs of the execution of their 'Action'. These are 'DeclaredArtifact's that
//! are yet bound to any 'Action's. When 'Action's are registered, they will be bound to their
//! appropriate 'DeclaredArtifact' to create a 'BuildArtifact'
//!
//! An 'Action' can be bound to multiple 'BuildArtifact's, but each 'BuildArtifact' can only be
//! bound to a particular 'Action'.

use std::borrow::Cow;
use std::fmt::Debug;
use std::ops::ControlFlow;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use bz_artifact::actions::key::ActionKey;
use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_build_signals::env::WaitingData;
use bz_common::io::IoProvider;
use bz_core::category::Category;
use bz_core::category::CategoryRef;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::external::external_cell_origin_for_cell;
use bz_core::content_hash::ContentBasedPathHash;
use bz_core::deferred::base_deferred_key::BaseDeferredKey;
use bz_core::execution_types::executor_config::CommandExecutorConfig;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::buck_out_path::BazelOutputPathKind;
use bz_core::fs::buck_out_path::BuildArtifactPath;
use bz_core::target::configured_target_label::ConfiguredTargetLabel;
use bz_events::dispatch::EventDispatcher;
use bz_execute::artifact::fs::ExecutorFs;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest_config::DigestConfig;
use bz_execute::execute::action_digest_and_blobs::ActionDigestAndBlobs;
use bz_execute::execute::blocking::BlockingExecutor;
use bz_execute::execute::cache_uploader::CacheUploadResults;
use bz_execute::execute::cache_uploader::IntoRemoteDepFile;
use bz_execute::execute::manager::CommandExecutionManager;
use bz_execute::execute::prepared::PreparedAction;
use bz_execute::execute::request::CommandExecutionOutput;
use bz_execute::execute::request::CommandExecutionRequest;
use bz_execute::execute::request::ExecutorPreference;
use bz_execute::execute::request::LocalActionCacheKey;
use bz_execute::execute::result::CommandExecutionResult;
use bz_execute::materialize::materializer::Materializer;
use bz_execute::re::manager::UnconfiguredRemoteExecutionClient;
use bz_execute::re::output_trees_download_config::OutputTreesDownloadConfig;
use bz_file_watcher::mergebase::Mergebase;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_hash::BuckHashMap;
use bz_hash::BuckIndexMap;
use bz_hash::BuckIndexSet;
use bz_hash::buck_indexmap;
use bz_http::HttpClient;
use derivative::Derivative;
use derive_more::Display;
use dice_futures::cancellation::CancellationContext;
use remote_execution::TActionResult2;
use starlark::values::Heap;
use starlark::values::OwnedFrozenValue;
use starlark::values::ValueOfUnchecked;
use starlark::values::dict::DictType;
use static_assertions::_core::ops::Deref;

use crate::actions::execute::action_execution_target::ActionExecutionTarget;
use crate::actions::execute::action_executor::ActionExecutionMetadata;
use crate::actions::execute::action_executor::ActionOutputs;
use crate::actions::execute::error::ExecuteError;
use crate::actions::impls::run_action_knobs::RunActionKnobs;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::ArtifactGroupValues;
use crate::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::artifact::starlark_artifact_value::StarlarkArtifactValue;
use crate::interpreter::rule_defs::cmd_args::ArtifactPathMapper;

pub mod artifact;
pub mod box_slice_set;
pub mod calculation;
mod error;
pub mod error_handler;
pub mod execute;
pub mod impls;
pub mod query;
pub mod registry;

/// Represents an unregistered 'Action' that will be registered into the 'Actions' module.
/// The 'UnregisteredAction' is not executable until it is registered, upon which it becomes an
/// 'Action' that is executable.
pub trait UnregisteredAction: Allocative + Send {
    /// consumes the self and becomes a registered 'Action'. The 'Action' will be executable
    /// and no longer bindable to any other 'Artifact's.
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        starlark_data: Option<OwnedFrozenValue>,
        error_handler: Option<OwnedFrozenValue>,
    ) -> bz_error::Result<Box<dyn Action>>;
}

/// A registered, immutable 'Action' that is fully bound. All it's 'Artifact's, both inputs and
/// outputs are verified to exist.
///
/// The 'Action' can be executed to produce the set of 'BuildArtifact's it declares. Before
/// execution, all input 'Artifact's will be made available to access.
#[async_trait]
pub trait Action: Allocative + Debug + Send + Sync + 'static {
    /// A machine readable kind identifying this type of action.
    fn kind(&self) -> bz_data::ActionKind;

    /// All the input 'Artifact's, both sources and built artifacts, that are required for
    /// executing this artifact. While nothing enforces it, this should be a pure function.
    fn inputs(&self) -> bz_error::Result<Cow<'_, [ArtifactGroup]>>;

    /// All the outputs this 'Artifact' will generate. Just like inputs, this should be a pure
    /// function. Note that outputs in action result might be ordered differently.
    fn outputs(&self) -> Cow<'_, [BuildArtifact]>;

    /// Returns a reference to an output of the action. All actions are required to have at least one output.
    fn first_output(&self) -> &BuildArtifact;

    /// Runs the 'Action', where all inputs are available but the output directory may not have
    /// been cleaned up. Upon success, it is expected that all outputs will be available
    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError>;

    /// Inputs needed to prove a persistent local action-cache hit before preparing the full action.
    ///
    /// Bazel checks the persistent action cache once metadata for the action-cache inputs is ready,
    /// and only prepares/executes the full action on a miss. Returning `None` preserves the existing
    /// behavior of ensuring every execution input before calling `execute`.
    fn local_action_cache_inputs(&self) -> bz_error::Result<Option<Cow<'_, [ArtifactGroup]>>> {
        Ok(None)
    }

    /// Try the persistent local action-cache path using `local_action_cache_inputs`.
    ///
    /// Implementations must return `Ok(None)` on a cache miss or when the fast path is unavailable.
    async fn try_execute_local_action_cache(
        &self,
        _ctx: &mut dyn ActionExecutionCtx,
        _waiting_data: WaitingData,
    ) -> Result<Option<(ActionOutputs, ActionExecutionMetadata)>, ExecuteError> {
        Ok(None)
    }

    /// A machine-readable category for this action, intended to be used when analyzing actions outside of bz itself.
    ///
    /// A category provides a namespace for identifiers within the rule that produced this action. Examples of
    /// categories would be things such as `cxx_compile`, `cxx_link`, and so on. Categories are user-specified in the
    /// rule implementation; however, bz enforces some restrictions on category names.
    fn category(&self) -> CategoryRef<'_>;

    /// A machine-readable identifier for this action. Required (but as of now, not yet enforced) to be unique within
    /// a category within a single invocation of a rule. Like categories, identifiers are also user-specified and bz
    /// ascribes no semantics to them. Examples of category-identifier pairs would be `cxx_compile` + `MyCppFile.cpp`,
    /// reflecting a C++ compiler invocation for a file `MyCppFile.cpp`.
    ///
    /// Not required; if None, only one action will be given in the given category. The user should
    /// be given either control over the identifier or the category.
    fn identifier(&self) -> Option<&str>;

    /// Whether to always print stderr, or only print when a user asks for it.
    fn always_print_stderr(&self) -> bool {
        false
    }

    /// Provides a string name for this action, obtained by combining the provided category and identifier.
    fn name(&self) -> String {
        if let Some(identifier) = self.identifier() {
            format!("{} {}", self.category(), identifier)
        } else {
            self.category().to_string()
        }
    }

    fn aquery_attributes(
        &self,
        _fs: &ExecutorFs,
        _artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> BuckIndexMap<String, String> {
        buck_indexmap! {}
    }

    fn error_handler(&self) -> Option<&OwnedFrozenValue> {
        None
    }

    fn failed_action_output_artifacts<'v>(
        &self,
        _artifact_fs: &ArtifactFs,
        _heap: Heap<'v>,
        _outputs: Option<&ActionOutputs>,
    ) -> bz_error::Result<ValueOfUnchecked<'v, DictType<StarlarkArtifact, StarlarkArtifactValue>>>
    {
        Ok(ValueOfUnchecked::new(starlark::values::Value::new_none()))
    }

    fn all_outputs_are_content_based(&self) -> bool {
        for output in self.outputs().iter() {
            if !output.get_path().is_content_based_path() {
                return false;
            }
        }
        true
    }

    fn all_inputs_are_eligible_for_dedupe(&self) -> bool {
        self.all_ineligible_for_dedup_inputs().is_empty()
    }

    fn all_ineligible_for_dedup_inputs(&self) -> Vec<String> {
        let target_platform = if let BaseDeferredKey::TargetLabel(configured_label) =
            self.first_output().key().owner()
        {
            Some(configured_label.cfg())
        } else {
            None
        };
        let mut ineligible_inputs = Vec::new();
        for ag in self.inputs().unwrap_or_default().iter() {
            if ag.is_eligible_for_dedupe(target_platform)
                == bz_data::EligibleForDedupe::IneligibleInput
            {
                ineligible_inputs.push(ag.to_string());
            }
        }
        ineligible_inputs
    }

    fn is_expected_eligible_for_dedupe(&self) -> Option<bool> {
        None
    }

    /// Returns the executor preference for this action, if applicable.
    /// Only command-based actions (like RunAction) have executor preferences.
    /// Returns None for actions that don't support executor preferences.
    fn executor_preference(&self) -> Option<ExecutorPreference> {
        None
    }

    /// Whether this action opts into eager materialization of inputs.
    /// When enabled, input artifacts will start materializing at low priority
    /// immediately after they get declared
    fn eager_materialization_enabled(&self) -> bool {
        false
    }

    // TODO this probably wants more data for execution, like printing a short_name and the target
}

/// The context for actions to use when executing
#[async_trait]
pub trait ActionExecutionCtx: Send + Sync {
    fn target(&self) -> ActionExecutionTarget<'_>;

    /// An 'ArtifactFs' to be used for managing 'Artifact's
    fn fs(&self) -> &ArtifactFs;

    fn executor_fs(&self) -> ExecutorFs<'_>;

    /// A `Materializer` used for expensive materializations
    fn materializer(&self) -> &dyn Materializer;

    fn events(&self) -> &EventDispatcher;

    fn command_execution_manager(&self, waiting_data: WaitingData) -> CommandExecutionManager;

    fn mergebase(&self) -> &Mergebase;

    async fn prepare_action(
        &mut self,
        request: &CommandExecutionRequest,
        re_outputs_required: bool,
    ) -> bz_error::Result<PreparedAction>;

    async fn action_cache(
        &mut self,
        manager: CommandExecutionManager,
        request: &CommandExecutionRequest,
        prepared_action: &PreparedAction,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager>;

    async fn unprepared_action_cache(
        &mut self,
        manager: CommandExecutionManager,
        _local_action_cache_key: &LocalActionCacheKey,
        _outputs: &BuckIndexSet<CommandExecutionOutput>,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        ControlFlow::Continue(manager)
    }

    async fn unprepared_action_cache_declared_by_action(
        &mut self,
        manager: CommandExecutionManager,
        local_action_cache_key: &LocalActionCacheKey,
        outputs: &BuckIndexSet<CommandExecutionOutput>,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        self.unprepared_action_cache(manager, local_action_cache_key, outputs)
            .await
    }

    fn insert_unprepared_action_cache_metadata(
        &mut self,
        _local_action_cache_key: &LocalActionCacheKey,
        _outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
        _remote_cache_entry: bool,
    ) -> bz_error::Result<()> {
        Ok(())
    }

    async fn remote_dep_file_cache(
        &mut self,
        manager: CommandExecutionManager,
        request: &CommandExecutionRequest,
        prepared_action: &PreparedAction,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager>;

    async fn cache_upload(
        &mut self,
        action: &ActionDigestAndBlobs,
        request: &CommandExecutionRequest,
        execution_result: &CommandExecutionResult,
        re_result: Option<TActionResult2>,
        dep_file_entry: Option<&mut dyn IntoRemoteDepFile>,
    ) -> bz_error::Result<CacheUploadResults>;

    /// Executes a command
    /// TODO(bobyf) this seems like it deserves critical sections?
    async fn exec_cmd(
        &mut self,
        manager: CommandExecutionManager,
        request: &CommandExecutionRequest,
        prepared_action: &PreparedAction,
    ) -> CommandExecutionResult;

    fn unpack_command_execution_result(
        &mut self,
        executor_preference: ExecutorPreference,
        result: CommandExecutionResult,
        allows_cache_upload: bool,
        allows_dep_file_cache_upload: bool,
        input_files_bytes: Option<u64>,
        incremental_kind: bz_data::IncrementalKind,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError>;

    /// Clean up all the output directories for this action. This requires a mutable reference
    /// because you shouldn't be doing anything else with the ActionExecutionCtx while cleaning the
    /// outputs.
    async fn cleanup_outputs(&mut self) -> bz_error::Result<()>;

    /// Get the value of an Artifact. This Artifact _must_ have been declared
    /// as an input to the associated action or a panic will be raised.
    fn artifact_values(&self, input: &ArtifactGroup) -> &ArtifactGroupValues;

    /// Digest of the DICE-computed action input set that should be used when forming a
    /// persistent local action-cache key.
    fn local_action_cache_input_set_digest(&self) -> &[u8];

    fn artifact_path_mapping(
        &self,
        filter: Option<BuckIndexSet<ArtifactGroup>>,
    ) -> BuckHashMap<&Artifact, ContentBasedPathHash>;

    fn blocking_executor(&self) -> &dyn BlockingExecutor;

    fn re_client(&self) -> UnconfiguredRemoteExecutionClient;

    fn re_platform(&self) -> &remote_execution::Platform;

    fn digest_config(&self) -> DigestConfig;

    /// Obtain per-command knobs for RunAction.
    fn run_action_knobs(&self) -> &RunActionKnobs;

    fn cancellation_context(&self) -> &CancellationContext;

    /// I/O layer access to add non-source files (e.g. downloaded files) to
    /// offline archive trace. If None, tracing is not enabled.
    fn io_provider(&self) -> Arc<dyn IoProvider>;

    /// Http client used for fetching and downloading remote artifacts.
    fn http_client(&self) -> HttpClient;

    fn output_trees_download_config(&self) -> &OutputTreesDownloadConfig;
}

#[derive(bz_error::Error, Debug)]
#[buck2(input)]
pub enum ActionErrors {
    #[error("Output path for artifact or metadata file cannot be empty.")]
    EmptyOutputPath,
    #[error(
        "Multiple artifacts and/or metadata files are declared at the same output location `{0}` declared at `{1}`."
    )]
    ConflictingOutputPath(ForwardRelativePathBuf, String),
    #[error(
        "Multiple artifacts and/or metadata files are declared at conflicting output locations. Output path `{0}` conflicts with the following output paths: {1:?}."
    )]
    ConflictingOutputPaths(ForwardRelativePathBuf, Vec<String>),
    #[error(
        "Action category `{0}` contains duplicate identifier `{1}`; category-identifier pairs must be unique within a rule"
    )]
    ActionCategoryIdentifierNotUnique(Category, String),
    #[error(
        "Analysis produced multiple actions with category `{0}` and at least one of them had no identifier. Add an identifier to these actions to disambiguate them"
    )]
    ActionCategoryDuplicateSingleton(Category),
}

#[derive(Derivative, Debug, Display, Allocative)]
#[derivative(Eq, Hash, PartialEq)]
#[display("Action(key={}, name={})", key, action.name())]
pub struct RegisteredAction {
    /// The key uniquely identifies a registered action.
    /// The key to the action is a one to one mapping.
    key: ActionKey,
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    action: Box<dyn Action>,
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    executor_config: Arc<CommandExecutorConfig>,
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    target_rule_type_name: Option<Arc<str>>,
}

fn bazel_external_repo_name<'a>(cell: &'a str, origin: &'a ExternalCellOrigin) -> &'a str {
    match origin {
        ExternalCellOrigin::Bundled(cell) => cell.as_str(),
        ExternalCellOrigin::Git(_) => cell,
        ExternalCellOrigin::Bzlmod(setup) => setup.canonical_repo_name.as_ref(),
        ExternalCellOrigin::BzlmodGenerated(setup) => setup.canonical_repo_name.as_ref(),
    }
}

fn push_bazel_path_component(path: &mut String, component: &str) {
    if component.is_empty() {
        return;
    }
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(component);
}

fn bazel_package_exec_path(label: &ConfiguredTargetLabel) -> String {
    let package = label.pkg();
    let cell = package.cell_name();
    let mut path = String::new();
    if let Some(origin) = external_cell_origin_for_cell(cell.as_str()) {
        push_bazel_path_component(&mut path, "external");
        push_bazel_path_component(&mut path, bazel_external_repo_name(cell.as_str(), &origin));
    } else if cell.as_str() != "root" {
        push_bazel_path_component(&mut path, cell.as_str());
    }
    push_bazel_path_component(&mut path, package.cell_relative_path().as_str());
    path
}

fn bazel_logical_output_path<'a>(
    output_path: &'a BuildArtifactPath,
    label: &ConfiguredTargetLabel,
) -> &'a str {
    let path = output_path.path().as_str();
    if output_path.bazel_output_path_kind() != BazelOutputPathKind::PackageRelative {
        return path;
    }

    let package_exec_path = bazel_package_exec_path(label);
    if package_exec_path.is_empty() {
        return path;
    }
    if path == package_exec_path {
        return "";
    }
    path.strip_prefix(&format!("{package_exec_path}/"))
        .unwrap_or(path)
}

fn action_key_output_path(output_path: &BuildArtifactPath) -> ForwardRelativePathBuf {
    let Some(label) = output_path.bazel_owner() else {
        return output_path.path().to_buf();
    };

    let mut path = String::from("_bazel/");
    path.push_str(label.cfg().output_hash().as_str());
    if let Some(exec_cfg) = label.exec_cfg() {
        path.push('-');
        path.push_str(exec_cfg.output_hash().as_str());
    }
    path.push('/');
    path.push_str(output_path.bazel_output_root().as_str());

    if output_path.bazel_output_path_kind() == BazelOutputPathKind::PackageRelative {
        let package_exec_path = bazel_package_exec_path(label);
        if !package_exec_path.is_empty() {
            path.push('/');
            path.push_str(&package_exec_path);
        }
    }

    path.push('/');
    path.push_str(bazel_logical_output_path(output_path, label));
    ForwardRelativePathBuf::new(path).expect("Bazel action key path should be normalized")
}

impl RegisteredAction {
    pub fn new(
        key: ActionKey,
        action: Box<dyn Action>,
        executor_config: Arc<CommandExecutorConfig>,
        target_rule_type_name: Option<Arc<str>>,
    ) -> Self {
        Self {
            key,
            action,
            executor_config,
            target_rule_type_name,
        }
    }

    pub fn action(&self) -> &dyn Action {
        self.action.as_ref()
    }

    /// Gets the target label to the rule that created this action.
    pub fn owner(&self) -> &BaseDeferredKey {
        self.key.owner()
    }

    /// Gets the action key, uniquely identifying this action in a target.
    pub(crate) fn action_key(&self) -> ForwardRelativePathBuf {
        // We want the action key to not cause instability in the RE action.
        // As an artifact can only be bound as an output to one action, we know it uniquely identifies the action and we can
        // derive the scratch path from that and that will be no unstable than the artifact already is.
        let output_path = self.action.first_output().get_path();
        let output_key_path = action_key_output_path(output_path);
        match output_path.dynamic_actions_action_key() {
            Some(k) => k
                .as_file_name()
                .as_forward_rel_path()
                .join(&output_key_path),
            None => output_key_path,
        }
    }

    pub fn key(&self) -> &ActionKey {
        &self.key
    }

    pub(crate) fn execution_config(&self) -> &CommandExecutorConfig {
        &self.executor_config
    }

    pub fn category(&self) -> CategoryRef<'_> {
        self.action.category()
    }

    pub fn identifier(&self) -> Option<&str> {
        self.action.identifier()
    }

    pub fn is_expected_eligible_for_dedupe(&self) -> Option<bool> {
        self.action.is_expected_eligible_for_dedupe()
    }

    pub fn target_rule_type_name(&self) -> Option<&str> {
        self.target_rule_type_name.as_deref()
    }
}

impl Deref for RegisteredAction {
    type Target = dyn Action;

    fn deref(&self) -> &Self::Target {
        self.action.as_ref()
    }
}

/// An 'UnregisteredAction' that is stored by the 'ActionsRegistry' to be registered.
/// The stored inputs have not yet been validated as bound, but will be validated upon registering.
#[derive(Allocative)]
struct ActionToBeRegistered {
    key: ActionKey,
    outputs: BuckIndexSet<BuildArtifact>,
    action: Box<dyn UnregisteredAction>,
}

impl ActionToBeRegistered {
    fn new<A: UnregisteredAction + 'static>(
        key: ActionKey,
        outputs: BuckIndexSet<BuildArtifact>,
        a: A,
    ) -> Self {
        Self {
            key,
            outputs,
            action: Box::new(a),
        }
    }

    pub(crate) fn key(&self) -> &ActionKey {
        &self.key
    }

    fn register(
        self,
        starlark_data: Option<OwnedFrozenValue>,
        error_handler: Option<OwnedFrozenValue>,
    ) -> bz_error::Result<Box<dyn Action>> {
        self.action
            .register(self.outputs, starlark_data, error_handler)
    }
}
