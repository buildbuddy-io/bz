/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::artifact_type::ArtifactErrors;
use bz_artifact::artifact::artifact_type::DeclaredArtifact;
use bz_artifact::artifact::artifact_type::OutputArtifact;
use bz_build_api::actions::impls::expanded_command_line::ExpandedCommandLineDigest;
use bz_build_api::analysis::bazel_action_key::BazelActionOwnerKey;
use bz_build_api::analysis::registry::BazelShareableActionIdentity;
use bz_build_api::artifact_groups::ArtifactGroup;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use bz_build_api::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use bz_build_api::interpreter::rule_defs::artifact::starlark_output_artifact::StarlarkOutputArtifact;
use bz_build_api::interpreter::rule_defs::artifact_tagging::ArtifactTag;
use bz_build_api::interpreter::rule_defs::bazel::depset::BazelDepset;
use bz_build_api::interpreter::rule_defs::bazel::depset::bazel_depset_to_list;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineArtifactVisitor;
use bz_build_api::interpreter::rule_defs::cmd_args::SimpleCommandLineArtifactVisitor;
use bz_build_api::interpreter::rule_defs::cmd_args::StarlarkCmdArgs;
use bz_build_api::interpreter::rule_defs::cmd_args::StarlarkCommandLineValueUnpack;
use bz_build_api::interpreter::rule_defs::cmd_args::value_as::ValueAsCommandLineLike;
use bz_build_api::interpreter::rule_defs::command_executor_config::parse_custom_re_image;
use bz_build_api::interpreter::rule_defs::command_executor_config::parse_remote_execution_extra_params;
use bz_build_api::interpreter::rule_defs::context::AnalysisActions;
use bz_build_api::interpreter::rule_defs::provider::builtin::bazel::cc_info::BazelCcCompileAction;
use bz_build_api::interpreter::rule_defs::provider::builtin::bazel::cc_info::BazelCcCompileCommandLine;
use bz_build_api::interpreter::rule_defs::provider::builtin::bazel::java_info::BazelJavaRunAction;
use bz_build_api::interpreter::rule_defs::provider::builtin::default_info::BazelRunfiles;
use bz_build_api::interpreter::rule_defs::provider::builtin::default_info::bazel_files_to_run_executable;
use bz_build_api::interpreter::rule_defs::provider::builtin::default_info::bazel_files_to_run_runfiles;
use bz_build_api::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_artifact_entries;
use bz_build_api::interpreter::rule_defs::provider::builtin::run_info::RunInfo;
use bz_build_api::interpreter::rule_defs::provider::builtin::worker_info::WorkerInfo;
use bz_build_api::interpreter::rule_defs::provider::builtin::worker_run_info::WorkerRunInfo;
use bz_build_api::interpreter::rule_defs::provider::dependency::Dependency;
use bz_core::category::CategoryRef;
use bz_core::configuration::data::BazelBuildSettingValue;
use bz_core::deferred::base_deferred_key::BaseDeferredKey;
use bz_core::execution_types::executor_config::ReGangWorker;
use bz_core::execution_types::executor_config::RemoteExecutorDependency;
use bz_error::BuckErrorContext;
use bz_error::conversion::from_any_with_tag;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_hash::BuckIndexSet;
use bz_hash::StdBuckHashMap;
use bz_util::thin_box::ThinBoxSlice;
use dupe::Dupe;
use either::Either;
use host_sharing::WeightClass;
use host_sharing::WeightPercentage;
use starlark::collections::SmallSet;
use starlark::environment::MethodsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::StringValue;
use starlark::values::UnpackAndDiscard;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueOf;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueTyped;
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::dict::DictType;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::float::UnpackFloat;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list::UnpackList;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;
use starlark::values::typing::StarlarkCallable;
use starlark_map::small_map;
use starlark_map::small_map::SmallMap;

use crate::actions::impls::run::MetadataParameter;
use crate::actions::impls::run::StarlarkRunActionValues;
use crate::actions::impls::run::UnregisteredRunAction;
use crate::actions::impls::run::dep_files::RunActionDepFiles;
use crate::actions::impls::run::new_executor_preference;
use crate::actions::impls::run::precompute_bazel_local_action_cache_command_line_digest_for_cc_compile_command_line;
use crate::actions::impls::run::precompute_bazel_local_action_cache_command_line_digest_for_cmd_args;
use crate::actions::impls::run::precompute_bazel_spawn_action_key_for_bazel_run_action;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
pub(crate) enum RunActionError {
    #[error("expected at least one output artifact, did not get any")]
    NoOutputsSpecified,
    #[error("`weight` must be a positive integer, got `{0}`")]
    InvalidWeight(u32),
    #[error("`timeout_seconds` must be a positive integer, got `{0}`")]
    InvalidTimeout(u32),
    #[error("`weight` and `weight_percentage` cannot both be passed")]
    DuplicateWeightsSpecified,
    #[error("`dep_files` value with key `{}` has an invalid count of associated outputs. Expected 1, got {}.", .key, .count)]
    InvalidDepFileOutputs { key: String, count: usize },
    #[error("`dep_files` with keys `{}` and `{}` are using the same tag", .first, .second)]
    ConflictingDepFiles { first: String, second: String },
    #[error("Dep-files input `{}` is tagged with multiple tags relevant for dep-files: `{}` and `{}`", .input, .tags[0], .tags[1])]
    ConflictingDepFileInputTags {
        input: ArtifactGroup,
        tags: Vec<String>,
    },
    #[error(
        "missing `metadata_path` parameter which is required when `metadata_env_var` parameter is present"
    )]
    MetadataPathMissing,
    #[error(
        "missing `metadata_env_var` parameter which is required when `metadata_path` parameter is present"
    )]
    MetadataEnvVarMissing,
    #[error(
        "Recursion limit exceeded when visiting artifacts: do you have a cycle in your inputs or outputs?"
    )]
    ArtifactVisitRecursionLimitExceeded,
    #[error(
        "`{}` was marked to be materialized on failure but is not declared as an output of the action.", .path
    )]
    FailedActionArtifactNotDeclared { path: String },
    #[error(
        "Action is marked with `incremental_remote_outputs` but output `{}` is content-based, which is not allowed.", .path
    )]
    IncrementalRemoteOutputsWithContentBasedOutputs { path: String },
    #[error(
        "Action is marked with `incremental_remote_outputs` but not `no_outputs_cleanup`, which is not allowed."
    )]
    IncrementalRemoteOutputsWithoutNoOutputsCleanup,
    #[error(
        "Action is marked with `expect_eligible_for_dedupe` but output `{}` is not content-based", .path
    )]
    ExpectEligibleForDedupeWithNonContentBasedOutput { path: String },
    #[error(
        "Action is marked with `expect_eligible_for_dedupe` but input `{}` is not eligible for dedupe", .input
    )]
    ExpectEligibleForDedupeWithIneligibleInput { input: ArtifactGroup },
    #[error("missing `arguments` parameter for `ctx.actions.run`")]
    MissingArguments,
    #[error("missing `category` parameter for Buck-style `ctx.actions.run`")]
    MissingCategory,
}

fn bazel_run_outputs<'v>(
    outputs: impl IntoIterator<Item = ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>,
) -> BuckIndexSet<OutputArtifact<'v>> {
    outputs
        .into_iter()
        .map(|artifact| artifact.output_artifact())
        .collect()
}

fn bazel_run_identifier<'v>(
    outputs: &BuckIndexSet<OutputArtifact<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Option<StringValue<'v>> {
    outputs.iter().next().map(|output| {
        let identifier = output.get_path().with_full_path(|path| path.to_string());
        eval.heap().alloc_str(&identifier)
    })
}

fn bazel_run_add_hidden<'v>(
    args: &mut StarlarkCmdArgs<'v>,
    value: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<()> {
    args.add_bazel_hidden_value(value, eval.heap())
}

fn bazel_executable_values_match<'v>(
    candidate: Value<'v>,
    executable: Value<'v>,
) -> starlark::Result<bool> {
    if candidate.equals(executable)? {
        return Ok(true);
    }

    let Some(candidate) = ValueAsInputArtifactLike::unpack_value(candidate)? else {
        return Ok(false);
    };
    let Some(executable) = ValueAsInputArtifactLike::unpack_value(executable)? else {
        return Ok(false);
    };

    Ok(candidate.0.get_bound_artifact()? == executable.0.get_bound_artifact()?)
}

fn bazel_runfiles_artifact_entries_from_value<'v>(
    runfiles: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<Value<'v>>> {
    if runfiles.is_none() {
        return Ok(None);
    }
    let runfiles = BazelRunfiles::from_value(runfiles).ok_or_else(|| {
        bz_error::internal_error!("Bazel files_to_run runfiles should be runfiles")
    })?;
    Ok(Some(bazel_runfiles_artifact_entries(
        eval.heap(),
        runfiles,
    )?))
}

fn bazel_files_to_run_runfiles_entries<'v>(
    files_to_run: Value<'v>,
    executable: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<Value<'v>>> {
    if let Some(dep_executable) = bazel_files_to_run_executable(files_to_run)
        && bazel_executable_values_match(dep_executable, executable)?
        && let Some(runfiles) = bazel_files_to_run_runfiles(files_to_run)
    {
        return bazel_runfiles_artifact_entries_from_value(runfiles, eval);
    }
    Ok(None)
}

fn bazel_executable_runfiles_entries_from_value<'v>(
    value: Value<'v>,
    executable: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<Value<'v>>> {
    if value.is_none() {
        return Ok(None);
    }
    if let Some(runfiles) = bazel_files_to_run_runfiles_entries(value, executable, eval)? {
        return Ok(Some(runfiles));
    }
    if let Some(dep) = Dependency::from_value(value) {
        if let Some(dep_executable) = dep.files_to_run_executable()?
            && bazel_executable_values_match(dep_executable, executable)?
        {
            let runfiles = dep.default_runfiles()?;
            return Ok(Some(bazel_runfiles_artifact_entries(
                eval.heap(),
                runfiles,
            )?));
        }
        return Ok(None);
    }
    if let Some(list) = ListRef::from_value(value) {
        for item in list.iter() {
            if let Some(runfiles) =
                bazel_executable_runfiles_entries_from_value(item, executable, eval)?
            {
                return Ok(Some(runfiles));
            }
        }
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        for item in tuple.iter() {
            if let Some(runfiles) =
                bazel_executable_runfiles_entries_from_value(item, executable, eval)?
            {
                return Ok(Some(runfiles));
            }
        }
    }
    if let Some(dict) = DictRef::from_value(value) {
        for (key, value) in dict.iter() {
            if let Some(runfiles) =
                bazel_executable_runfiles_entries_from_value(key, executable, eval)?
            {
                return Ok(Some(runfiles));
            }
            if let Some(runfiles) =
                bazel_executable_runfiles_entries_from_value(value, executable, eval)?
            {
                return Ok(Some(runfiles));
            }
        }
    }
    if BazelDepset::from_value(value).is_some() {
        for item in bazel_depset_to_list(value)? {
            if let Some(runfiles) =
                bazel_executable_runfiles_entries_from_value(item, executable, eval)?
            {
                return Ok(Some(runfiles));
            }
        }
    }
    Ok(None)
}

fn bazel_executable_runfiles_entries<'v>(
    this: &AnalysisActions<'v>,
    executable: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<Value<'v>>> {
    let Some(attrs) = this.attributes.borrow().as_ref().copied() else {
        return Ok(None);
    };
    if let Some(attrs) = StructRef::from_value(attrs.get()) {
        for (_, value) in attrs.iter() {
            if let Some(runfiles) =
                bazel_executable_runfiles_entries_from_value(value, executable, eval)?
            {
                return Ok(Some(runfiles));
            }
        }
    }
    Ok(None)
}

fn bazel_executable_runfiles_entries_for_value<'v>(
    this: &AnalysisActions<'v>,
    executable_value: Value<'v>,
    executable: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<Value<'v>>> {
    if let Some(runfiles) = bazel_files_to_run_runfiles(executable_value)
        .map(|runfiles| bazel_runfiles_artifact_entries_from_value(runfiles, eval))
        .transpose()?
        .flatten()
    {
        return Ok(Some(runfiles));
    }
    bazel_executable_runfiles_entries(this, executable, eval)
}

fn bazel_push_tool_runfiles<'v>(
    executable: Value<'v>,
    runfiles: Value<'v>,
    seen_executables: &mut Vec<Value<'v>>,
    tool_runfiles: &mut Vec<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<()> {
    let Some(entries) = ListRef::from_value(runfiles) else {
        return Err(bz_error::internal_error!("Bazel tool runfiles should be a list").into());
    };
    if entries.is_empty() {
        return Ok(());
    }
    for seen in seen_executables.iter().copied() {
        if seen.equals(executable)? {
            return Ok(());
        }
    }
    seen_executables.push(executable);
    tool_runfiles.push(eval.heap().alloc(AllocStruct([
        ("executable", executable),
        ("runfiles", runfiles),
    ])));
    Ok(())
}

fn bazel_collect_tool_runfiles_from_value<'v>(
    this: &AnalysisActions<'v>,
    value: Value<'v>,
    seen_executables: &mut Vec<Value<'v>>,
    tool_runfiles: &mut Vec<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<()> {
    if value.is_none() {
        return Ok(());
    }
    if let Some(dep) = Dependency::from_value(value) {
        if let Some(executable) = dep.files_to_run_executable()? {
            let runfiles = bazel_runfiles_artifact_entries(eval.heap(), dep.default_runfiles()?)?;
            bazel_push_tool_runfiles(executable, runfiles, seen_executables, tool_runfiles, eval)?;
        }
        return Ok(());
    }
    if let Some(executable) = bazel_files_to_run_executable(value) {
        let runfiles = bazel_files_to_run_runfiles(value)
            .map(|runfiles| bazel_runfiles_artifact_entries_from_value(runfiles, eval))
            .transpose()?
            .flatten();
        let runfiles = match runfiles {
            Some(runfiles) => Some(runfiles),
            None => bazel_executable_runfiles_entries(this, executable, eval)?,
        };
        if let Some(runfiles) = runfiles {
            bazel_push_tool_runfiles(executable, runfiles, seen_executables, tool_runfiles, eval)?;
        }
        return Ok(());
    }
    if ValueAsInputArtifactLike::unpack_value(value)?.is_some() {
        if let Some(runfiles) = bazel_executable_runfiles_entries(this, value, eval)? {
            bazel_push_tool_runfiles(value, runfiles, seen_executables, tool_runfiles, eval)?;
        }
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        for item in list.iter() {
            bazel_collect_tool_runfiles_from_value(
                this,
                item,
                seen_executables,
                tool_runfiles,
                eval,
            )?;
        }
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        for item in tuple.iter() {
            bazel_collect_tool_runfiles_from_value(
                this,
                item,
                seen_executables,
                tool_runfiles,
                eval,
            )?;
        }
        return Ok(());
    }
    if BazelDepset::from_value(value).is_some() {
        for item in bazel_depset_to_list(value)? {
            bazel_collect_tool_runfiles_from_value(
                this,
                item,
                seen_executables,
                tool_runfiles,
                eval,
            )?;
        }
    }
    Ok(())
}

fn bazel_tool_runfiles<'v>(
    this: &AnalysisActions<'v>,
    tools: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<Value<'v>>> {
    let mut seen_executables = Vec::new();
    let mut tool_runfiles = Vec::new();
    bazel_collect_tool_runfiles_from_value(
        this,
        tools,
        &mut seen_executables,
        &mut tool_runfiles,
        eval,
    )?;
    if tool_runfiles.is_empty() {
        Ok(None)
    } else {
        Ok(Some(eval.heap().alloc(AllocList(tool_runfiles))))
    }
}

fn bazel_resolve_env<'v>(
    env: Option<
        ValueOf<'v, UnpackDictEntries<UnpackAndDiscard<&'v str>, ValueAsCommandLineLike<'v>>>,
    >,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Option<ValueOfUnchecked<'v, DictType<String, ValueAsCommandLineLike<'static>>>>>
{
    let Some(env) = env else {
        return Ok(None);
    };
    let dict = DictRef::from_value(env.value).ok_or_else(|| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "`env` should be a dict, got `{}`",
            env.value.get_type()
        )
    })?;
    let mut entries = Vec::with_capacity(dict.len());
    for (key, value) in dict.iter() {
        let Some(key) = key.unpack_str() else {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "`env` keys must be strings, got `{}`",
                key.get_type()
            )
            .into());
        };
        let Some(value) = value.unpack_str() else {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "`env` values must be strings, got `{}`",
                value.get_type()
            )
            .into());
        };
        entries.push((key.to_owned(), eval.heap().alloc_str(value).to_value()));
    }
    Ok(Some(ValueOfUnchecked::new(
        eval.heap().alloc(AllocDict(entries)),
    )))
}

fn bazel_static_env_entries<'v>(env: Value<'v>) -> Option<Vec<(&'v str, &'v str)>> {
    let dict = DictRef::from_value(env)?;
    dict.iter()
        .map(|(key, value)| Some((key.unpack_str()?, value.unpack_str()?)))
        .collect()
}

fn bazel_artifact_is_run_action_output<'v>(
    outputs: &BuckIndexSet<OutputArtifact<'v>>,
    artifact: &Artifact,
) -> bool {
    outputs
        .iter()
        .any(|output| output.get_path() == artifact.get_path())
}

fn bazel_artifact_group_is_run_action_output<'v>(
    outputs: &BuckIndexSet<OutputArtifact<'v>>,
    artifact_group: &ArtifactGroup,
) -> bool {
    match artifact_group {
        ArtifactGroup::Artifact(artifact) => bazel_artifact_is_run_action_output(outputs, artifact),
        ArtifactGroup::TransitiveSetProjection(_) | ArtifactGroup::Promise(_) => false,
    }
}

struct BazelRunOutputFilteringArtifactVisitor<'a, 'v> {
    outputs: &'a BuckIndexSet<OutputArtifact<'v>>,
    inner: &'a mut dyn CommandLineArtifactVisitor<'v>,
}

impl<'a, 'v> BazelRunOutputFilteringArtifactVisitor<'a, 'v> {
    fn new(
        outputs: &'a BuckIndexSet<OutputArtifact<'v>>,
        inner: &'a mut dyn CommandLineArtifactVisitor<'v>,
    ) -> Self {
        Self { outputs, inner }
    }
}

impl<'v> CommandLineArtifactVisitor<'v> for BazelRunOutputFilteringArtifactVisitor<'_, 'v> {
    fn visit_input(&mut self, input: ArtifactGroup, tags: Vec<&ArtifactTag>) {
        if !bazel_artifact_group_is_run_action_output(self.outputs, &input) {
            self.inner.visit_input(input, tags);
        }
    }

    fn visit_declared_output(&mut self, artifact: OutputArtifact<'v>, tags: Vec<&ArtifactTag>) {
        self.inner.visit_declared_output(artifact, tags);
    }

    fn visit_frozen_output(&mut self, artifact: Artifact, tags: Vec<&ArtifactTag>) {
        if !bazel_artifact_is_run_action_output(self.outputs, &artifact) {
            self.inner.visit_frozen_output(artifact, tags);
        }
    }

    fn visit_declared_artifact(
        &mut self,
        declared_artifact: DeclaredArtifact<'v>,
        tags: Vec<&ArtifactTag>,
    ) -> bz_error::Result<()> {
        let output = declared_artifact.as_output();
        if self.outputs.contains(&output) {
            self.inner.visit_declared_output(output, tags);
            return Ok(());
        }
        self.inner.visit_input(
            ArtifactGroup::Artifact(declared_artifact.ensure_bound()?.into_artifact()),
            tags,
        );
        Ok(())
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

    fn expand_transitive_set_inputs_for_bazel_shared_action(&self) -> bool {
        self.inner
            .expand_transitive_set_inputs_for_bazel_shared_action()
    }
}

fn bazel_visit_run_action_command_line_artifacts<'v>(
    outputs: &BuckIndexSet<OutputArtifact<'v>>,
    command_line: &dyn CommandLineArgLike<'v>,
    artifact_visitor: &mut dyn CommandLineArtifactVisitor<'v>,
) -> bz_error::Result<()> {
    let mut artifact_visitor =
        BazelRunOutputFilteringArtifactVisitor::new(outputs, artifact_visitor);
    command_line.visit_artifacts(&mut artifact_visitor)
}

struct BazelShareableActionInputVisitor<'v> {
    inputs: BuckIndexSet<ArtifactGroup>,
    _marker: std::marker::PhantomData<&'v ()>,
}

impl<'v> BazelShareableActionInputVisitor<'v> {
    fn new() -> Self {
        Self {
            inputs: BuckIndexSet::default(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'v> CommandLineArtifactVisitor<'v> for BazelShareableActionInputVisitor<'v> {
    fn visit_input(&mut self, input: ArtifactGroup, _tags: Vec<&ArtifactTag>) {
        self.inputs.insert(input);
    }

    fn visit_declared_output(&mut self, _artifact: OutputArtifact<'v>, _tags: Vec<&ArtifactTag>) {}

    fn visit_frozen_output(&mut self, artifact: Artifact, _tags: Vec<&ArtifactTag>) {
        self.inputs.insert(ArtifactGroup::Artifact(artifact));
    }

    fn expand_transitive_set_inputs_for_bazel_shared_action(&self) -> bool {
        true
    }
}

fn bazel_visit_runfiles_artifacts<'v>(
    runfiles: Value<'v>,
    artifact_visitor: &mut dyn CommandLineArtifactVisitor<'v>,
) -> starlark::Result<()> {
    let entries = ListRef::from_value(runfiles)
        .ok_or_else(|| bz_error::internal_error!("Bazel executable runfiles should be a list"))?;
    for entry in entries.iter() {
        let entry = StructRef::from_value(entry).ok_or_else(|| {
            bz_error::internal_error!("Bazel executable runfiles entry should be a struct")
        })?;
        let mut target_file = None;
        for (name, value) in entry.iter() {
            if name.as_str() == "target_file" {
                target_file = Some(value);
            }
        }
        let target_file = target_file.ok_or_else(|| {
            bz_error::internal_error!(
                "Bazel executable runfiles entry should have field `target_file`"
            )
        })?;
        let artifact = ValueAsInputArtifactLike::unpack_value(target_file)?.ok_or_else(|| {
            bz_error::internal_error!("Bazel executable runfiles target_file should be File")
        })?;
        artifact_visitor.visit_input(artifact.0.get_artifact_group()?, Vec::new());
    }
    Ok(())
}

fn bazel_visit_tool_runfiles_artifacts<'v>(
    tool_runfiles: Value<'v>,
    artifact_visitor: &mut dyn CommandLineArtifactVisitor<'v>,
) -> starlark::Result<()> {
    let tools = ListRef::from_value(tool_runfiles)
        .ok_or_else(|| bz_error::internal_error!("Bazel tool runfiles should be a list"))?;
    for tool in tools.iter() {
        let tool = StructRef::from_value(tool).ok_or_else(|| {
            bz_error::internal_error!("Bazel tool runfiles entry should be a struct")
        })?;
        let mut executable = None;
        let mut runfiles = None;
        for (name, value) in tool.iter() {
            match name.as_str() {
                "executable" => executable = Some(value),
                "runfiles" => runfiles = Some(value),
                _ => {}
            }
        }
        let executable = executable.ok_or_else(|| {
            bz_error::internal_error!("Bazel tool runfiles entry should have field `executable`")
        })?;
        let executable = ValueAsInputArtifactLike::unpack_value(executable)?.ok_or_else(|| {
            bz_error::internal_error!("Bazel tool runfiles executable should be File")
        })?;
        artifact_visitor.visit_input(executable.0.get_artifact_group()?, Vec::new());
        let runfiles = runfiles.ok_or_else(|| {
            bz_error::internal_error!("Bazel tool runfiles entry should have field `runfiles`")
        })?;
        bazel_visit_runfiles_artifacts(runfiles, artifact_visitor)?;
    }
    Ok(())
}

fn bazel_visit_worker_artifacts<'v>(
    worker: ValueTypedComplex<'v, WorkerInfo<'v>>,
    outputs: &BuckIndexSet<OutputArtifact<'v>>,
    artifact_visitor: &mut dyn CommandLineArtifactVisitor<'v>,
) -> starlark::Result<()> {
    match worker.unpack() {
        Either::Left(worker) => {
            bazel_visit_run_action_command_line_artifacts(
                outputs,
                worker.exe_command_line(),
                artifact_visitor,
            )?;
            for (_, value) in worker.env() {
                bazel_visit_run_action_command_line_artifacts(outputs, value, artifact_visitor)?;
            }
        }
        Either::Right(worker) => {
            bazel_visit_run_action_command_line_artifacts(
                outputs,
                worker.exe_command_line(),
                artifact_visitor,
            )?;
            for (_, value) in worker.env() {
                bazel_visit_run_action_command_line_artifacts(outputs, value, artifact_visitor)?;
            }
        }
    }
    Ok(())
}

fn bazel_run_mandatory_inputs<'v>(
    outputs: &BuckIndexSet<OutputArtifact<'v>>,
    exe: &StarlarkCmdArgs<'v>,
    args: &StarlarkCmdArgs<'v>,
    bazel_inputs: &StarlarkCmdArgs<'v>,
    env: Option<ValueOfUnchecked<'v, DictType<String, ValueAsCommandLineLike<'static>>>>,
    worker: Option<ValueTypedComplex<'v, WorkerInfo<'v>>>,
    remote_worker: Option<ValueTypedComplex<'v, WorkerInfo<'v>>>,
    bazel_executable_runfiles: Option<Value<'v>>,
    bazel_tool_runfiles: Option<Value<'v>>,
) -> starlark::Result<BuckIndexSet<ArtifactGroup>> {
    let mut artifact_visitor = BazelShareableActionInputVisitor::new();
    bazel_visit_run_action_command_line_artifacts(outputs, exe, &mut artifact_visitor)?;
    bazel_visit_run_action_command_line_artifacts(outputs, args, &mut artifact_visitor)?;
    bazel_visit_run_action_command_line_artifacts(outputs, bazel_inputs, &mut artifact_visitor)?;
    if let Some(env) = env {
        let dict = DictRef::from_value(env.get())
            .ok_or_else(|| bz_error::internal_error!("Bazel action env should be a dict"))?;
        for (_, value) in dict.iter() {
            bazel_visit_run_action_command_line_artifacts(
                outputs,
                ValueAsCommandLineLike::unpack_value_err(value)?.0,
                &mut artifact_visitor,
            )?;
        }
    }
    if let Some(worker) = worker {
        bazel_visit_worker_artifacts(worker, outputs, &mut artifact_visitor)?;
    }
    if let Some(remote_worker) = remote_worker {
        bazel_visit_worker_artifacts(remote_worker, outputs, &mut artifact_visitor)?;
    }
    if let Some(runfiles) = bazel_executable_runfiles {
        bazel_visit_runfiles_artifacts(runfiles, &mut artifact_visitor)?;
    }
    if let Some(tool_runfiles) = bazel_tool_runfiles {
        bazel_visit_tool_runfiles_artifacts(tool_runfiles, &mut artifact_visitor)?;
    }
    Ok(artifact_visitor.inputs)
}

const BAZEL_ACTION_ENV: &str = "//command_line_option:action_env";
const BAZEL_HOST_ACTION_ENV: &str = "//command_line_option:host_action_env";
const BAZEL_STRICT_ACTION_ENV: &str = "//command_line_option:incompatible_strict_action_env";

fn bazel_owner_action_env_values(owner: &BaseDeferredKey, key: &str) -> Vec<String> {
    let Some(label) = owner.configured_label() else {
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

fn bazel_owner_action_env_bool(owner: &BaseDeferredKey, key: &str, default: bool) -> bool {
    let Some(label) = owner.configured_label() else {
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

fn bazel_owner_uses_exec_configuration(owner: &BaseDeferredKey) -> bool {
    owner
        .configured_label()
        .and_then(|label| label.cfg().label().ok().map(str::to_owned))
        .is_some_and(|label| label.starts_with("bazeltr-"))
}

fn bazel_default_action_environment_for_owner(
    owner: &BaseDeferredKey,
) -> (Vec<(String, String)>, Vec<String>) {
    let strict_action_env = bazel_owner_action_env_bool(owner, BAZEL_STRICT_ACTION_ENV, true);
    let mut fixed_env = BTreeMap::new();
    let mut inherited_env = BTreeSet::new();
    if strict_action_env {
        if cfg!(windows) {
            fixed_env.insert("PATH".to_owned(), "c:/msys64/usr/bin".to_owned());
        } else {
            fixed_env.insert(
                "PATH".to_owned(),
                "/bin:/usr/bin:/sbin:/usr/sbin".to_owned(),
            );
        }
    }
    fixed_env.insert("LC_CTYPE".to_owned(), "C.UTF-8".to_owned());

    let key = if bazel_owner_uses_exec_configuration(owner) {
        BAZEL_HOST_ACTION_ENV
    } else {
        BAZEL_ACTION_ENV
    };
    for entry in bazel_owner_action_env_values(owner, key) {
        if let Some(name) = entry.strip_prefix('=') {
            fixed_env.remove(name);
            inherited_env.remove(name);
        } else if let Some((name, value)) = entry.split_once('=') {
            fixed_env.insert(name.to_owned(), value.to_owned());
            inherited_env.remove(name);
        } else {
            fixed_env.remove(entry.as_str());
            inherited_env.insert(entry);
        }
    }

    (
        fixed_env.into_iter().collect(),
        inherited_env.into_iter().collect(),
    )
}

fn bazel_resource_set_os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "osx"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    }
}

struct BazelResourceSetInputVisitor {
    inputs: BuckIndexSet<ArtifactGroup>,
}

impl BazelResourceSetInputVisitor {
    fn new() -> Self {
        Self {
            inputs: BuckIndexSet::default(),
        }
    }
}

impl<'v> CommandLineArtifactVisitor<'v> for BazelResourceSetInputVisitor {
    fn visit_input(&mut self, input: ArtifactGroup, _tags: Vec<&ArtifactTag>) {
        self.inputs.insert(input);
    }

    fn visit_declared_output(&mut self, _artifact: OutputArtifact<'v>, _tags: Vec<&ArtifactTag>) {}

    fn visit_frozen_output(&mut self, _artifact: Artifact, _tags: Vec<&ArtifactTag>) {}

    fn visit_declared_artifact(
        &mut self,
        declared_artifact: DeclaredArtifact<'v>,
        tags: Vec<&ArtifactTag>,
    ) -> bz_error::Result<()> {
        if let Ok(artifact) = declared_artifact.ensure_bound() {
            self.visit_input(ArtifactGroup::Artifact(artifact.into_artifact()), tags);
        }
        Ok(())
    }
}

fn bazel_run_input_count<'v>(
    exe: &StarlarkCmdArgs<'v>,
    args: &StarlarkCmdArgs<'v>,
    bazel_inputs: &StarlarkCmdArgs<'v>,
) -> starlark::Result<usize> {
    let mut visitor = BazelResourceSetInputVisitor::new();
    exe.visit_artifacts(&mut visitor)?;
    args.visit_artifacts(&mut visitor)?;
    bazel_inputs.visit_artifacts(&mut visitor)?;
    Ok(visitor.inputs.len())
}

fn bazel_resource_number<'v>(dict: &DictRef<'v>, key: &str, default: f64) -> starlark::Result<f64> {
    let Some(value) = dict.get_str(key) else {
        return Ok(default);
    };
    let Some(value) = UnpackFloat::unpack_value(value)? else {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "Illegal resource value type for key `{}`: got `{}`, want int or float",
            key,
            value.get_type()
        )
        .into());
    };
    Ok(value.0)
}

fn bazel_run_weight_from_resource_set<'v>(
    resource_set: Option<StarlarkCallable<'v>>,
    input_count: usize,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<WeightClass> {
    let Some(resource_set) = resource_set else {
        return Ok(WeightClass::Permits(1));
    };

    let input_count = i32::try_from(input_count).unwrap_or(i32::MAX);
    let os_name = eval
        .heap()
        .alloc_str(bazel_resource_set_os_name())
        .to_value();
    let input_count = eval.heap().alloc(input_count);
    let response = eval.eval_function(resource_set.0, &[os_name, input_count], &[])?;
    let dict = DictRef::from_value(response).ok_or_else(|| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "resource_set callback must return a dict, got `{}`",
            response.get_type()
        )
    })?;

    for (key, _) in dict.iter() {
        let Some(key) = key.unpack_str() else {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "resource_set keys must be strings, got `{}`",
                key.get_type()
            )
            .into());
        };
        if key != "cpu" && key != "memory" && key != "local_test" {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "Illegal resource key `{}`",
                key
            )
            .into());
        }
    }

    let cpu = bazel_resource_number(&dict, "cpu", 1.0)?;
    let _memory = bazel_resource_number(&dict, "memory", 250.0)?;
    let _local_test = bazel_resource_number(&dict, "local_test", 1.0)?;
    let permits = cpu.max(1.0).ceil() as u32;
    Ok(WeightClass::Permits(permits.try_into().map_err(|e| {
        bz_error::bz_error!(bz_error::ErrorTag::Input, "Invalid resource_set cpu: {e}")
    })?))
}

fn register_bazel_run_action<'v>(
    this: &AnalysisActions<'v>,
    exe: StarlarkCmdArgs<'v>,
    args: StarlarkCmdArgs<'v>,
    inputs: Value<'v>,
    tools: Value<'v>,
    bazel_inputs: Option<StarlarkCmdArgs<'v>>,
    bazel_executable: Option<Value<'v>>,
    bazel_executable_runfiles: Option<Value<'v>>,
    outputs: impl IntoIterator<Item = ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>,
    env: Option<ValueOfUnchecked<'v, DictType<String, ValueAsCommandLineLike<'static>>>>,
    worker: Option<ValueTypedComplex<'v, WorkerInfo<'v>>>,
    remote_worker: Option<ValueTypedComplex<'v, WorkerInfo<'v>>>,
    mnemonic: Option<StringValue<'v>>,
    execution_info: Vec<(String, String)>,
    use_default_shell_env: bool,
    resource_set: NoneOr<StarlarkCallable<'v>>,
    local_only: bool,
    supports_bazel_path_mapping: bool,
    shareable_action_owner_key: Option<BazelActionOwnerKey>,
    bazel_string_args: Option<Box<[String]>>,
    bazel_cc_command_line: Option<ValueTyped<'v, BazelCcCompileCommandLine<'v>>>,
    precomputed_local_action_cache_command_line_digest: Option<ExpandedCommandLineDigest>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let bazel_inputs = match bazel_inputs {
        Some(bazel_inputs) => bazel_inputs,
        None => {
            let mut bazel_inputs = StarlarkCmdArgs::default();
            bazel_run_add_hidden(&mut bazel_inputs, inputs, eval)?;
            bazel_run_add_hidden(&mut bazel_inputs, tools, eval)?;
            bazel_inputs
        }
    };

    let outputs = bazel_run_outputs(outputs);
    if outputs.is_empty() {
        return Err(bz_error::Error::from(RunActionError::NoOutputsSpecified).into());
    }
    let should_precompute_bazel_run_command_line = mnemonic.as_ref().is_some_and(|mnemonic| {
        matches!(mnemonic.as_str(), "CppCompile" | "Javac" | "JavacTurbine")
    });
    let static_env_entries = match env {
        Some(env) => bazel_static_env_entries(env.get()),
        None => Some(Vec::new()),
    };
    let precomputed_local_action_cache_command_line_digest =
        precomputed_local_action_cache_command_line_digest.or_else(|| {
            if !should_precompute_bazel_run_command_line {
                return None;
            }
            let static_env_entries = static_env_entries.as_deref()?;
            precompute_bazel_local_action_cache_command_line_digest_for_cmd_args(
                &exe,
                &args,
                static_env_entries,
                &outputs,
                this.digest_config,
            )
        });
    let identifier = bazel_run_identifier(&outputs, eval);
    let resource_set = resource_set.into_option();
    let weight = if resource_set.is_some() {
        let input_count = bazel_run_input_count(&exe, &args, &bazel_inputs)?;
        bazel_run_weight_from_resource_set(resource_set, input_count, eval)?
    } else if mnemonic
        .as_ref()
        .is_some_and(|mnemonic| mnemonic.as_str() == "GenProto")
    {
        // Large Bazel builds can schedule many protoc/plugin pairs at once. Bazel's resource
        // manager accounts for these as local resource consumers; Buck's host sharing only has a
        // single permit dimension here, so give GenProto actions a heavier default permit weight.
        WeightClass::Percentage(WeightPercentage::try_new(50u8).expect("50 is a valid percentage"))
    } else {
        WeightClass::Permits(1)
    };

    let executor_preference = new_executor_preference(local_only, false, false)?;
    let category = mnemonic.unwrap_or_else(|| eval.heap().alloc_str("BazelRun"));
    let effective_supports_bazel_path_mapping = supports_bazel_path_mapping && !local_only;
    let bazel_tool_runfiles = bazel_tool_runfiles(this, tools, eval)?;
    let (default_shell_env, inherited_env) = if use_default_shell_env {
        let state = this.state()?;
        bazel_default_action_environment_for_owner(state.analysis_value_storage.self_key.owner())
    } else {
        (Vec::new(), Vec::new())
    };
    let shareable_action_owner_key = if bazel_cc_command_line.is_none() {
        shareable_action_owner_key
    } else {
        None
    };
    let shareable_action_key = if bazel_cc_command_line.is_some() {
        None
    } else {
        static_env_entries
            .as_deref()
            .and_then(|static_env_entries| {
                let owner_key = shareable_action_owner_key.as_ref()?;
                precompute_bazel_spawn_action_key_for_bazel_run_action(
                    &exe,
                    &args,
                    static_env_entries,
                    &default_shell_env,
                    &inherited_env,
                    &outputs,
                    category.as_str(),
                    &execution_info,
                    effective_supports_bazel_path_mapping,
                    this.digest_config,
                    bazel_string_args.as_deref(),
                    owner_key,
                )
            })
    };
    if let Some(shareable_action_key) = &shareable_action_key {
        let mut state = this.state()?;
        if !state.should_register_bazel_shareable_action_for_outputs(&outputs, |state| {
            let mandatory_inputs = bazel_run_mandatory_inputs(
                &outputs,
                &exe,
                &args,
                &bazel_inputs,
                env,
                worker,
                remote_worker,
                bazel_executable_runfiles,
                bazel_tool_runfiles,
            )?;
            Ok(BazelShareableActionIdentity::spawn(
                category.as_str(),
                shareable_action_key.clone(),
                state.bazel_shareable_artifact_group_identities(mandatory_inputs.iter())?,
                state.bazel_shareable_output_identities(&outputs),
            ))
        })? {
            return Ok(NoneType);
        }
    }
    let starlark_values = eval.heap().alloc_complex(StarlarkRunActionValues {
        exe: eval.heap().alloc_typed(exe),
        args: eval.heap().alloc_typed(args),
        bazel_inputs: Some(eval.heap().alloc_typed(bazel_inputs)),
        bazel_executable,
        bazel_executable_runfiles,
        bazel_tool_runfiles,
        bazel_cc_command_line,
        env,
        worker,
        remote_worker,
        category,
        identifier,
        outputs_for_error_handler: Vec::new(),
    });

    let action = UnregisteredRunAction {
        executor_preference,
        always_print_stderr: false,
        eager_materialization_enabled: false,
        weight,
        low_pass_filter: true,
        dep_files: RunActionDepFiles::new(),
        metadata_param: None,
        no_outputs_cleanup: false,
        incremental_remote_outputs: false,
        allow_cache_upload: None,
        allow_dep_file_cache_upload: false,
        allow_offline_output_cache: false,
        force_full_hybrid_if_capable: false,
        unique_input_inodes: false,
        remote_execution_dependencies: ThinBoxSlice::empty(),
        re_gang_workers: ThinBoxSlice::empty(),
        remote_execution_custom_image: None,
        remote_execution_extra_params: parse_remote_execution_extra_params(None)?,
        expected_eligible_for_dedupe: None,
        timeout: None,
        bazel_use_default_shell_env: Some(use_default_shell_env),
        supports_bazel_path_mapping: effective_supports_bazel_path_mapping,
        bazel_string_args,
        bazel_shared_action_key: shareable_action_key
            .as_ref()
            .map(|action_key| action_key.as_str().to_owned()),
        precomputed_local_action_cache_command_line_digest,
    };

    this.state()?
        .register_action(outputs, action, Some(starlark_values), None)?;
    Ok(NoneType)
}

fn bazel_execution_info_entries(
    execution_requirements: Value<'_>,
) -> starlark::Result<Vec<(String, String)>> {
    let Some(requirements) = DictRef::from_value(execution_requirements) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::with_capacity(requirements.len());
    for (key, value) in requirements.iter() {
        let Some(key) = key.unpack_str() else {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "execution_requirements keys must be strings, got `{}`",
                key.get_type()
            )
            .into());
        };
        entries.push((key.to_owned(), value.unpack_str().unwrap_or("").to_owned()));
    }
    entries.sort();
    Ok(entries)
}

fn bazel_execution_info_contains(execution_info: &[(String, String)], key: &str) -> bool {
    execution_info
        .binary_search_by(|(entry, _)| entry.as_str().cmp(key))
        .is_ok()
}

fn supports_bazel_path_mapping(execution_info: &[(String, String)]) -> bool {
    bazel_execution_info_contains(execution_info, "supports-path-mapping")
        && !bazel_execution_info_contains(execution_info, "local")
        && !(bazel_execution_info_contains(execution_info, "no-sandbox")
            && bazel_execution_info_contains(execution_info, "no-remote"))
}

pub(crate) fn register_bazel_cc_compile_action<'v>(
    action: BazelCcCompileAction<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let heap = eval.heap();
    let executable_value = action.executable;
    let executable = bazel_files_to_run_executable(executable_value).unwrap_or(executable_value);
    let bazel_executable_runfiles = bazel_executable_runfiles_entries_for_value(
        action.actions.as_ref(),
        executable_value,
        executable,
        eval,
    )?;
    let precomputed_local_action_cache_command_line_digest = executable
        .unpack_str()
        .map(|executable| {
            precompute_bazel_local_action_cache_command_line_digest_for_cc_compile_command_line(
                executable,
                action.command_line.as_ref(),
                heap,
            )
        })
        .transpose()?;
    let exe = StarlarkCmdArgs::from_values([executable])?;
    let args = StarlarkCmdArgs::default();
    register_bazel_run_action(
        action.actions.as_ref(),
        exe,
        args,
        Value::new_none(),
        Value::new_none(),
        Some(action.inputs),
        Some(executable),
        bazel_executable_runfiles,
        action.outputs,
        None,
        None,
        None,
        Some(action.mnemonic),
        Vec::new(),
        true,
        NoneOr::None,
        false,
        false,
        None,
        None,
        Some(action.command_line),
        precomputed_local_action_cache_command_line_digest,
        eval,
    )
}

pub(crate) fn register_bazel_java_run_action<'v>(
    action: BazelJavaRunAction<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let heap = eval.heap();
    let inputs = heap.alloc(AllocList(action.inputs));
    let outputs = action
        .outputs
        .into_iter()
        .map(ValueTyped::<StarlarkDeclaredArtifact>::new_err)
        .collect::<starlark::Result<Vec<_>>>()?;

    let (exe, args, worker, remote_worker, bazel_executable, bazel_executable_runfiles) =
        if let Some(worker_executable) = action.worker_executable {
            let worker_run = ValueOf::<&WorkerRunInfo>::unpack_value_err(worker_executable)?;
            let worker = worker_run.typed.worker();
            let remote_worker = worker_run.typed.remote_worker();
            let worker_exe = worker_run.typed.exe();
            let exe = StarlarkCmdArgs::try_from_value(worker_exe.to_value())?;
            let args = StarlarkCmdArgs::from_values([action.arguments.to_value()])?;
            (exe, args, worker, remote_worker, None, None)
        } else {
            let executable_value = action.executable;
            let executable =
                bazel_files_to_run_executable(executable_value).unwrap_or(executable_value);
            let bazel_executable_runfiles = bazel_executable_runfiles_entries_for_value(
                action.actions.as_ref(),
                executable_value,
                executable,
                eval,
            )?;
            let exe = StarlarkCmdArgs::from_values([executable])?;
            let args = StarlarkCmdArgs::from_values([action.arguments.to_value()])?;
            (
                exe,
                args,
                None,
                None,
                Some(executable),
                bazel_executable_runfiles,
            )
        };

    register_bazel_run_action(
        action.actions.as_ref(),
        exe,
        args,
        inputs,
        Value::new_none(),
        None,
        bazel_executable,
        bazel_executable_runfiles,
        outputs,
        None,
        worker,
        remote_worker,
        Some(action.mnemonic),
        Vec::new(),
        true,
        NoneOr::None,
        false,
        false,
        action
            .actions
            .as_ref()
            .bazel_default_exec_group_action_owner_key()?,
        None,
        None,
        None,
        eval,
    )
}

#[starlark_module]
pub(crate) fn analysis_actions_methods_run(methods: &mut MethodsBuilder) {
    /// Bazel-compatible shell action.
    fn run_shell<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = named)] command: &str,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        arguments: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = NoneType)] inputs: Value<'v>,
        #[starlark(require = named, default = NoneType)] tools: Value<'v>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        outputs: UnpackListOrTuple<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>,
        #[starlark(require = named)] env: Option<
            ValueOf<'v, UnpackDictEntries<UnpackAndDiscard<&'v str>, ValueAsCommandLineLike<'v>>>,
        >,
        #[starlark(require = named)] mnemonic: Option<StringValue<'v>>,
        #[starlark(require = named, default = NoneType)] progress_message: Value<'v>,
        #[starlark(require = named, default = NoneType)] execution_requirements: Value<'v>,
        #[starlark(require = named, default = NoneType)] toolchain: Value<'v>,
        #[starlark(require = named, default = NoneType)] exec_group: Value<'v>,
        #[starlark(require = named, default = false)] use_default_shell_env: bool,
        #[starlark(require = named, default = NoneOr::None)] resource_set: NoneOr<
            StarlarkCallable<'v>,
        >,
        #[starlark(require = named, default = false)] local_only: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let execution_info = bazel_execution_info_entries(execution_requirements)?;
        let supports_bazel_path_mapping = supports_bazel_path_mapping(&execution_info);
        let shareable_action_owner_key = this.bazel_run_action_owner_key(toolchain, exec_group)?;
        let _unused = progress_message;
        let heap = eval.heap();
        let exe = StarlarkCmdArgs::from_values([heap.alloc_str("/bin/bash").to_value()])?;
        let mut shell_args = Vec::with_capacity(arguments.items.len() + 3);
        shell_args.push(heap.alloc_str("-c").to_value());
        shell_args.push(heap.alloc_str(command).to_value());
        shell_args.push(heap.alloc_str("bz_run_shell").to_value());
        shell_args.extend(arguments.items);
        let args = StarlarkCmdArgs::from_values(shell_args)?;
        let env = bazel_resolve_env(env, eval)?;
        register_bazel_run_action(
            this,
            exe,
            args,
            inputs,
            tools,
            None,
            None,
            None,
            outputs,
            env,
            None,
            None,
            mnemonic,
            execution_info,
            use_default_shell_env,
            resource_set,
            local_only,
            supports_bazel_path_mapping,
            shareable_action_owner_key,
            None,
            None,
            None,
            eval,
        )
    }

    /// Run a command to produce one or more artifacts.
    ///
    /// * `arguments`: must be of type `cmd_args`, or a type convertible to such (such as a list of
    ///   strings and artifacts). See below for detailed description of artifact arguments.
    /// * `env`: environment variables to set when the command is executed.
    /// * `category`: category and identifier - when used together, identify the action in bz's
    ///   event stream, and must be unique for a given target
    /// * `weight`: used to note how heavy the command is and will typically be set to a higher
    ///   value to indicate that less such commands should be run in parallel (if running locally)
    /// * `no_outputs_cleanup`: if this flag is set then bz won't clean the outputs of a previous
    ///   build that might be present on a disk; in which case, command from arguments should be
    ///   responsible for the cleanup (that is useful, for example, when an action is supporting
    ///   incremental mode and its outputs are based on result from a previous build)
    /// * `metadata_env_var` and `meadata_path` should be used together: both set or both unset
    ///     * `metadata_path`: defines a path relative to the result directory for a file with
    ///       action metadata, which will be created right before the command will be run.
    ///     * Metadata contains the path relative to the bz project root and hash digest for
    ///       every action input (this excludes symlinks as they could be resolved by a user script
    ///       if needed). The resolved path relative to the bz project for the metadata file will
    ///       be passed to command from arguments, via the environment variable, with its name set
    ///       by `metadata_env_var`
    ///     * Both `metadata_env_var` and `metadata_path` are useful when making actions behave in
    ///       an incremental manner (for details, see [Incremental
    ///       Actions](https://buck2.build/docs/rule_authors/incremental_actions/))
    /// * `dep_files`: a dictionary mapping labels to `ArtifactTag` instances for tracking actual
    ///   dependencies via dependency files (depfiles). This enables precise incremental builds by
    ///   allowing the build tool to report which inputs it actually used.
    ///     * Each entry maps a string label (e.g., `"headers"`) to an `ArtifactTag` created via
    ///       `ctx.actions.artifact_tag()`
    ///     * The tag should be used to mark both the potential inputs (via `tag.tag_artifacts()`)
    ///       and the depfile output that will list the actual inputs used
    ///     * After execution, bz reads the depfile and only tracks changes to inputs listed in it,
    ///       rather than all tagged inputs
    ///     * Depfiles must use Makefile syntax: `output: input1 input2 input3`
    ///     * For complete documentation and examples, see [`ctx.actions.artifact_tag()`](../AnalysisActions#analysisactionsartifact_tag)
    /// * `allow_offline_output_cache`: enables caching of this action's outputs for offline builds (default: `false`)
    ///     * When `true`, action outputs are cached during trace builds (via `bz debug trace-io`)
    ///       and restored during offline builds without re-executing the action
    ///     * Intended for actions that read from the network (e.g., downloads, remote artifact fetches)
    ///       which cannot execute in offline build environments where network access is restricted
    ///     * During trace builds: outputs are copied to `buck-out/offline-cache/` after successful execution
    ///     * During offline builds: if all outputs exist in offline cache, they are restored without
    ///       running the action; otherwise the action executes normally (graceful fallback)
    ///     * Requires `buck2.use_network_action_output_cache=true` config to take effect
    ///     * Example use case: caching network downloads in containerized offline build environments
    /// * The `prefer_local`, `prefer_remote` and `local_only` options allow selecting where the
    /// action should run if the executor selected for this target is a hybrid executor.
    ///     * All those options disable concurrent execution: the action will run on the preferred
    ///     platform first (concurrent execution only happens with a "full" hybrid executor).
    ///     * Execution may be retried on the "non-preferred" platform if it fails due to a
    ///     transient error, except for `local_only`, which does not allow this.
    ///     * If the executor selected is a remote-only executor and you use `local_only`, that's an
    ///     error. The other options will not raise errors.
    ///     * Setting more than one of those options is an error.
    ///     * Those flags behave the same way as the equivalent `--prefer-remote`, `--prefer-local`
    ///     and `--local-only` CLI flags. The CLI flags take precedence.
    ///     * The `force_full_hybrid_if_capable` option overrides the `use_limited_hybrid` hybrid.
    ///     The options listed above take precedence if set.
    /// * `remote_execution_dependencies`: list of dependencies which is passed to Remote Execution.
    ///   Each dependency is dictionary with the following keys:
    ///     * `smc_tier`: name of the SMC tier to call by RE Scheduler.
    ///     * `id`: name of the dependency.
    /// * `remote_execution_dynamic_image`: a custom Tupperware image which is passed to Remote Execution.
    ///   It takes a dictionary with the following keys:
    ///     * `identifier`: name of the SMC tier to call by RE Scheduler.
    ///         * `name`: name of the image.
    ///         * `uuid`: uuid of the image.
    ///     * `drop_host_mount_globs`: list of strings containing file
    ///     globs. Any mounts globs specified will not be bind mounted
    ///     from the host.
    /// * `timeout_seconds`: an optional timeout for the action, in seconds. If
    ///   the action takes longer than this, it will be cancelled and behave as if
    ///   it has failed. Must be a positive number. The default is no timeout.
    ///     * Use this to abort misbehaving actions (e.g. if an action sometimes
    ///     deadlock). In other words, use this when the build will fail either
    ///     way, and you want to just make it fail faster.
    ///     * Do NOT use  this to attempt to enforce e.g. a runtime policy on a
    ///     specific set of actions: action runtime is often variable so setting
    ///     a timeout to try and enforce a specific runtime goal will inevitably
    ///     result in flaky failures for end users running builds.
    ///  * `remote_execution_extra_params`: a dictionary to pass extra parameters to RE, can add more keys in the future:
    ///     * `remote_execution_policy`: refer to TExecutionPolicy.
    ///  * `error_handler`: an optional function that analyzes action failures and produces structured error information.
    ///     * Type signature: `def error_handler(ctx: ActionErrorCtx) -> list[ActionSubError]`
    ///     * The function receives an [`ActionErrorCtx`](../ActionErrorCtx) parameter and should return a list of [`ActionSubError`](../ActionSubError) objects
    ///     * Error handlers enable better error diagnostics and language-specific error categorization
    ///  * `outputs_for_error_handler`: Output files to be provided to the action error handler and read by
    /// [error handler](https://buck2.build/docs/api/build/ActionErrorCtx/#actionerrorctxoutput_artifacts) in the event of a failure..
    ///     * The output must also be declared as an output of the action
    ///     * The output artifact must be created if the action fails
    ///     * Nothing will be provided if left empty (Which is the default)
    ///
    /// When actions execute, they'll do so from the root of the repository. As they execute,
    /// actions have exclusive access to their output directory.
    ///
    /// Actions also get exclusive access to a "scratch" path that is exposed via the environment
    /// variable `BUCK_SCRATCH_PATH`. This path is expressed as a path relative to the working
    /// directory (i.e. relative to the project). This path is guaranteed to exist when the action
    /// executes.
    ///
    /// When actions run locally, the scratch path is also used as the `TMPDIR`.
    ///
    /// ### Input and output artifacts
    ///
    /// Run action consumes arbitrary number of input artifacts
    /// and produces at least one output artifact.
    ///
    /// Both input and output artifacts can be passed in:
    /// - positional `arguments` parameters
    /// - `env` dict
    ///
    /// Input artifacts must be already bound prior to this call,
    /// meaning these artifacts must be either:
    /// - source artifacts
    /// - coming from dependencies
    /// - declared locally and bound to another action (passed to `.as_output()`)
    ///   *before* this `run()` call
    /// - or created already bound with some simple action like `write()`
    ///
    /// Output artifacts must be declared locally (within the same analysis),
    /// and must not be already bound. Output artifacts become "bound" after this call.
    fn run<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(default = NoneOr::None)] arguments: NoneOr<StarlarkCommandLineValueUnpack<'v>>,
        #[starlark(require = named)] category: Option<StringValue<'v>>,
        #[starlark(require = named, default = NoneOr::None)] identifier: NoneOr<StringValue<'v>>,
        #[starlark(require = named)] env: Option<
            ValueOf<'v, UnpackDictEntries<UnpackAndDiscard<&'v str>, ValueAsCommandLineLike<'v>>>,
        >,
        #[starlark(require = named)] executable: Option<Value<'v>>,
        #[starlark(require = named, default = NoneType)] inputs: Value<'v>,
        #[starlark(require = named, default = NoneType)] tools: Value<'v>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        outputs: UnpackListOrTuple<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>,
        #[starlark(require = named)] mnemonic: Option<StringValue<'v>>,
        #[starlark(require = named, default = NoneType)] progress_message: Value<'v>,
        #[starlark(require = named, default = NoneType)] execution_requirements: Value<'v>,
        #[starlark(require = named, default = NoneType)] toolchain: Value<'v>,
        #[starlark(require = named, default = NoneType)] exec_group: Value<'v>,
        #[starlark(require = named, default = false)] use_default_shell_env: bool,
        #[starlark(require = named, default = NoneOr::None)] resource_set: NoneOr<
            StarlarkCallable<'v>,
        >,
        #[starlark(require = named, default = false)] local_only: bool,
        #[starlark(require = named, default = false)] prefer_local: bool,
        #[starlark(require = named, default = false)] prefer_remote: bool,
        #[starlark(require = named, default = true)] low_pass_filter: bool,
        #[starlark(require = named, default = false)] always_print_stderr: bool,
        #[starlark(require = named)] weight: Option<u32>,
        #[starlark(require = named)] weight_percentage: Option<u32>,
        #[starlark(require = named)] dep_files: Option<SmallMap<&'v str, &'v ArtifactTag>>,
        #[starlark(require = named)] metadata_env_var: Option<String>,
        #[starlark(require = named)] metadata_path: Option<String>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        incremental_metadata_ignore_tags: UnpackListOrTuple<&'v ArtifactTag>,
        // TODO(scottcao): Refactor `no_outputs_cleanup` to `outputs_cleanup`
        #[starlark(require = named, default = false)] no_outputs_cleanup: bool,
        #[starlark(require = named, default = false)] incremental_remote_outputs: bool,
        #[starlark(require = named, default = NoneOr::None)] allow_cache_upload: NoneOr<bool>,
        #[starlark(require = named, default = false)] allow_dep_file_cache_upload: bool,
        #[starlark(require = named, default = false)] allow_offline_output_cache: bool,
        #[starlark(require = named, default = false)] force_full_hybrid_if_capable: bool,
        #[starlark(require = named)] exe: Option<
            Either<ValueOf<'v, &'v WorkerRunInfo<'v>>, ValueOf<'v, &'v RunInfo<'v>>>,
        >,
        #[starlark(require = named, default = false)] unique_input_inodes: bool,
        #[starlark(require = named, default = NoneOr::None)] error_handler: NoneOr<
            StarlarkCallable<'v>,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
        #[starlark(require = named, default=UnpackList::default())]
        remote_execution_dependencies: UnpackList<SmallMap<&'v str, &'v str>>,
        #[starlark(require = named, default=UnpackList::default())] re_gang_workers: UnpackList<
            SmallMap<&'v str, &'v str>,
        >,
        #[starlark(default = NoneType, require = named)] remote_execution_dynamic_image: Value<'v>,
        #[starlark(require = named, default = NoneOr::None)] timeout_seconds: NoneOr<u32>,
        #[starlark(require = named, default = NoneOr::None)] remote_execution_extra_params: NoneOr<
            DictRef<'v>,
        >,
        // Note: Intentionally don't support frozen output artifacts
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        outputs_for_error_handler: UnpackListOrTuple<
            ValueTyped<'v, StarlarkOutputArtifact<'v>>,
        >,
        #[starlark(require = named, default = NoneOr::None)] expect_eligible_for_dedupe: NoneOr<
            bool,
        >,
        #[starlark(require = named, default = false)] eager_materialization_enabled: bool,
        // Bazel input-pruning hint: a file the action writes listing inputs it did
        // not use. bz does not perform input pruning, so this is accepted and
        // ignored (the action still runs and produces its real outputs).
        #[starlark(require = named, default = NoneType)] unused_inputs_list: Value<'v>,
    ) -> starlark::Result<NoneType> {
        let _ = unused_inputs_list;
        let arguments = arguments.into_option();
        if let Some(executable) = executable {
            let execution_info = bazel_execution_info_entries(execution_requirements)?;
            let supports_bazel_path_mapping = supports_bazel_path_mapping(&execution_info);
            let shareable_action_owner_key =
                this.bazel_run_action_owner_key(toolchain, exec_group)?;
            let _unused = progress_message;
            if let Ok(worker_run) = ValueOf::<&WorkerRunInfo>::unpack_value_err(executable) {
                let worker = worker_run.typed.worker();
                let remote_worker = worker_run.typed.remote_worker();
                let worker_exe = worker_run.typed.exe();
                let exe = StarlarkCmdArgs::try_from_value(worker_exe.to_value())?;
                let env = bazel_resolve_env(env, eval)?;
                let args = match arguments {
                    Some(arguments) => StarlarkCmdArgs::try_from_value_typed(arguments)?,
                    None => StarlarkCmdArgs::default(),
                };
                return register_bazel_run_action(
                    this,
                    exe,
                    args,
                    inputs,
                    tools,
                    None,
                    None,
                    None,
                    outputs,
                    env,
                    worker,
                    remote_worker,
                    mnemonic,
                    execution_info.clone(),
                    use_default_shell_env,
                    resource_set,
                    local_only,
                    supports_bazel_path_mapping,
                    shareable_action_owner_key,
                    None,
                    None,
                    None,
                    eval,
                );
            }
            let executable_value = executable;
            let executable =
                bazel_files_to_run_executable(executable_value).unwrap_or(executable_value);
            let bazel_executable_runfiles = bazel_executable_runfiles_entries_for_value(
                this,
                executable_value,
                executable,
                eval,
            )?;
            let env = bazel_resolve_env(env, eval)?;
            let exe = StarlarkCmdArgs::from_values([executable])?;
            let args = match arguments {
                Some(arguments) => StarlarkCmdArgs::try_from_value_typed(arguments)?,
                None => StarlarkCmdArgs::default(),
            };
            return register_bazel_run_action(
                this,
                exe,
                args,
                inputs,
                tools,
                None,
                Some(executable),
                bazel_executable_runfiles,
                outputs,
                env,
                None,
                None,
                mnemonic,
                execution_info,
                use_default_shell_env,
                resource_set,
                local_only,
                supports_bazel_path_mapping,
                shareable_action_owner_key,
                None,
                None,
                None,
                eval,
            );
        }
        let _unused_bazel_run_params = (
            inputs,
            tools,
            outputs,
            mnemonic,
            progress_message,
            execution_requirements,
            toolchain,
            exec_group,
            use_default_shell_env,
            resource_set,
        );
        let arguments =
            arguments.ok_or_else(|| bz_error::Error::from(RunActionError::MissingArguments))?;
        let category =
            category.ok_or_else(|| bz_error::Error::from(RunActionError::MissingCategory))?;
        if incremental_remote_outputs && !no_outputs_cleanup {
            // Precaution to make sure content-based paths are not involved.
            return Err(bz_error::Error::from(
                RunActionError::IncrementalRemoteOutputsWithoutNoOutputsCleanup,
            )
            .into());
        }

        struct RunCommandArtifactVisitor<'v> {
            inner: SimpleCommandLineArtifactVisitor<'v>,
            tagged_outputs: StdBuckHashMap<ArtifactTag, Vec<OutputArtifact<'v>>>,
            depth: u64,
            dep_file_artifact_tags: Option<SmallSet<&'v ArtifactTag>>,
            inputs_with_multiple_tags_for_dep_files: Vec<(ArtifactGroup, Vec<ArtifactTag>)>,
        }

        impl<'v> RunCommandArtifactVisitor<'v> {
            fn new(dep_files: &Option<SmallMap<&'v str, &'v ArtifactTag>>) -> Self {
                let dep_file_artifact_tags = if let Some(dep_files) = dep_files {
                    let mut tags = SmallSet::with_capacity(dep_files.len());
                    for (_key, tag) in dep_files {
                        tags.insert(tag.dupe());
                    }
                    Some(tags)
                } else {
                    None
                };
                Self {
                    inner: SimpleCommandLineArtifactVisitor::new(),
                    tagged_outputs: StdBuckHashMap::default(),
                    depth: 0,
                    dep_file_artifact_tags,
                    inputs_with_multiple_tags_for_dep_files: Vec::new(),
                }
            }
        }

        impl<'v> CommandLineArtifactVisitor<'v> for RunCommandArtifactVisitor<'v> {
            fn visit_input(&mut self, input: ArtifactGroup, tags: Vec<&ArtifactTag>) {
                if let Some(ref dep_file_artifact_tags) = self.dep_file_artifact_tags {
                    let dep_file_tags: Vec<&ArtifactTag> = tags
                        .iter()
                        .filter_map(|t| {
                            if dep_file_artifact_tags.contains(*t) {
                                Some(*t)
                            } else {
                                None
                            }
                        })
                        .collect();
                    if dep_file_tags.len() > 1 {
                        self.inputs_with_multiple_tags_for_dep_files.push((
                            input.dupe(),
                            dep_file_tags.into_iter().map(|t| t.dupe()).collect(),
                        ));
                    }
                }
                self.inner.visit_input(input, tags);
            }

            fn visit_declared_output(
                &mut self,
                artifact: OutputArtifact<'v>,
                tags: Vec<&ArtifactTag>,
            ) {
                for tag in tags.iter() {
                    self.tagged_outputs
                        .entry((*tag).dupe())
                        .or_default()
                        .push(artifact.dupe());
                }

                self.inner.visit_declared_output(artifact, tags);
            }

            fn visit_frozen_output(&mut self, artifact: Artifact, tags: Vec<&ArtifactTag>) {
                self.inner.visit_frozen_output(artifact, tags)
            }

            fn push_frame(&mut self) -> bz_error::Result<()> {
                self.depth += 1;
                if self.depth > 1000 {
                    return Err(RunActionError::ArtifactVisitRecursionLimitExceeded.into());
                }
                Ok(())
            }

            fn pop_frame(&mut self) {
                self.depth = self.depth.saturating_sub(1);
            }
        }

        let executor_preference = new_executor_preference(local_only, prefer_local, prefer_remote)?;

        let mut artifact_visitor = RunCommandArtifactVisitor::new(&dep_files);

        let starlark_args = StarlarkCmdArgs::try_from_value_typed(arguments)?;
        starlark_args.visit_artifacts(&mut artifact_visitor)?;

        // TODO(nga): we should not accept output artifacts in worker.
        let (starlark_exe, starlark_worker, starlark_remote_worker) = match exe {
            Some(Either::Left(worker_run)) => {
                let worker = worker_run.typed.worker();
                let remote_worker = worker_run.typed.remote_worker();
                let worker_exe = worker_run.typed.exe();
                worker_exe.as_ref().visit_artifacts(&mut artifact_visitor)?;
                let starlark_exe = StarlarkCmdArgs::try_from_value(worker_exe.to_value())?;
                starlark_exe.visit_artifacts(&mut artifact_visitor)?;
                (starlark_exe, worker, remote_worker)
            }
            Some(Either::Right(exe)) => {
                let starlark_exe = StarlarkCmdArgs::try_from_value(*exe)?;
                starlark_exe.visit_artifacts(&mut artifact_visitor)?;
                (starlark_exe, None, None)
            }
            None => (StarlarkCmdArgs::default(), None, None),
        };

        let weight = match (weight, weight_percentage) {
            (None, None) => WeightClass::Permits(1),
            (Some(v), None) => {
                if v == 0 {
                    return Err(bz_error::Error::from(RunActionError::InvalidWeight(v)).into());
                }
                WeightClass::Permits(v.try_into().unwrap()) // We don't support < 32 bit platforms.
            }
            (None, Some(v)) => WeightClass::Percentage(
                WeightPercentage::try_new(v)
                    .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Tier0))
                    .buck_error_context("Invalid `weight_percentage`")?,
            ),
            (Some(..), Some(..)) => {
                return Err(
                    bz_error::Error::from(RunActionError::DuplicateWeightsSpecified).into(),
                );
            }
        };

        let starlark_env = match &env {
            None => None,
            Some(env) => {
                for (_k, v) in &env.typed.entries {
                    v.0.visit_artifacts(&mut artifact_visitor)?;
                }
                Some(env.as_unchecked().cast())
            }
        };

        let RunCommandArtifactVisitor {
            inner: artifacts,
            tagged_outputs,
            inputs_with_multiple_tags_for_dep_files,
            ..
        } = artifact_visitor;

        if let Some(frozen) = { artifacts.frozen_outputs }.pop() {
            return Err(bz_error::Error::from(ArtifactErrors::DuplicateBind(frozen)).into());
        }

        let mut dep_files_configuration = RunActionDepFiles::new();

        if let Some(dep_files) = dep_files {
            for (key, tag) in dep_files {
                let tagged = tagged_outputs.get(tag);
                let count = tagged.map_or(0, |t| t.len());

                if count != 1 {
                    return Err(
                        bz_error::Error::from(RunActionError::InvalidDepFileOutputs {
                            key: (*key).to_owned(),
                            count,
                        })
                        .into(),
                    );
                }

                match dep_files_configuration.labels.entry(tag.dupe()) {
                    small_map::Entry::Vacant(v) => {
                        v.insert(Arc::from(key));
                    }
                    small_map::Entry::Occupied(o) => {
                        return Err(bz_error::Error::from(RunActionError::ConflictingDepFiles {
                            first: (**o.get()).to_owned(),
                            second: (*key).to_owned(),
                        })
                        .into());
                    }
                }
            }
        }

        if let Some((input, conflicting_tags)) = inputs_with_multiple_tags_for_dep_files.first() {
            return Err(
                bz_error::Error::from(RunActionError::ConflictingDepFileInputTags {
                    input: input.dupe(),
                    tags: conflicting_tags
                        .iter()
                        .map(|t| (**dep_files_configuration.labels.get(t).unwrap()).to_owned())
                        .collect(),
                })
                .into(),
            );
        }

        let metadata_param = match (metadata_env_var, metadata_path) {
            (Some(env_var), Some(path)) => {
                let path: ForwardRelativePathBuf = path.try_into()?;
                this.state()?.claim_output_path(eval, &path)?;
                bz_error::Ok(Some(Box::new(MetadataParameter {
                    env_var,
                    path,
                    ignore_tags: incremental_metadata_ignore_tags
                        .into_iter()
                        .map(|x| x.dupe())
                        .collect(),
                })))
            }
            (Some(_), None) => Err(RunActionError::MetadataPathMissing.into()),
            (None, Some(_)) => Err(RunActionError::MetadataEnvVarMissing.into()),
            (None, None) => Ok(None),
        }?;

        if artifacts.declared_outputs.is_empty() {
            return Err(bz_error::Error::from(RunActionError::NoOutputsSpecified).into());
        }
        let heap = eval.heap();

        for o in outputs_for_error_handler.items.iter() {
            let to_materialize = o.artifact();
            if !artifacts.declared_outputs.contains(&to_materialize) {
                return Err(bz_error::Error::from(
                    RunActionError::FailedActionArtifactNotDeclared {
                        path: o.to_string(),
                    },
                )
                .into());
            }
        }

        let starlark_values = heap.alloc_complex(StarlarkRunActionValues {
            exe: heap.alloc_typed(starlark_exe),
            args: heap.alloc_typed(starlark_args),
            bazel_inputs: None,
            bazel_executable: None,
            bazel_executable_runfiles: None,
            bazel_tool_runfiles: None,
            bazel_cc_command_line: None,
            env: starlark_env,
            worker: starlark_worker,
            remote_worker: starlark_remote_worker,
            category: {
                CategoryRef::new(category.as_str())?;
                category
            },
            identifier: identifier.into_option(),
            outputs_for_error_handler: outputs_for_error_handler.items,
        });

        let re_dependencies = remote_execution_dependencies
            .into_iter()
            .map(RemoteExecutorDependency::parse)
            .collect::<bz_error::Result<ThinBoxSlice<RemoteExecutorDependency>>>()?;

        let re_gang_workers = re_gang_workers
            .into_iter()
            .map(ReGangWorker::parse)
            .collect::<bz_error::Result<ThinBoxSlice<ReGangWorker>>>()?;

        let re_custom_image = parse_custom_re_image(
            "remote_execution_dynamic_image",
            remote_execution_dynamic_image,
        )?;

        let extra_params =
            parse_remote_execution_extra_params(remote_execution_extra_params.into_option())?;

        let timeout = match timeout_seconds.into_option() {
            Some(t) => {
                if t == 0 {
                    return Err(bz_error::Error::from(RunActionError::InvalidTimeout(t)).into());
                }
                Some(Duration::from_secs(t.into()))
            }
            None => None,
        };

        if incremental_remote_outputs {
            for o in artifacts.declared_outputs.iter() {
                if o.has_content_based_path() {
                    return Err(bz_error::Error::from(
                        RunActionError::IncrementalRemoteOutputsWithContentBasedOutputs {
                            path: o.get_path().to_string(),
                        },
                    )
                    .into());
                }
            }
        }

        let action = UnregisteredRunAction {
            executor_preference,
            always_print_stderr,
            eager_materialization_enabled,
            weight,
            low_pass_filter,
            dep_files: dep_files_configuration,
            metadata_param,
            no_outputs_cleanup,
            incremental_remote_outputs,
            allow_cache_upload: allow_cache_upload.into_option(),
            allow_dep_file_cache_upload,
            allow_offline_output_cache,
            force_full_hybrid_if_capable,
            unique_input_inodes,
            remote_execution_dependencies: re_dependencies,
            re_gang_workers,
            remote_execution_custom_image: re_custom_image,
            remote_execution_extra_params: extra_params,
            expected_eligible_for_dedupe: expect_eligible_for_dedupe.into_option(),
            timeout,
            bazel_use_default_shell_env: None,
            supports_bazel_path_mapping: false,
            bazel_string_args: None,
            bazel_shared_action_key: None,
            precomputed_local_action_cache_command_line_digest: None,
        };

        let expect_eligible_for_dedupe = expect_eligible_for_dedupe.into_option().unwrap_or(false);
        if expect_eligible_for_dedupe {
            let deferred_holder_key = &this.state()?.analysis_value_storage.self_key;
            let target_platform = if let BaseDeferredKey::TargetLabel(configured_label) =
                deferred_holder_key.owner()
            {
                Some(configured_label.cfg())
            } else {
                None
            };
            for o in artifacts.declared_outputs.iter() {
                if !o.has_content_based_path() {
                    return Err(bz_error::Error::from(
                        RunActionError::ExpectEligibleForDedupeWithNonContentBasedOutput {
                            path: o.get_path().to_string(),
                        },
                    )
                    .into());
                }
            }

            for i in artifacts.inputs.iter() {
                if i.is_eligible_for_dedupe(target_platform)
                    == bz_data::EligibleForDedupe::IneligibleInput
                {
                    return Err(bz_error::Error::from(
                        RunActionError::ExpectEligibleForDedupeWithIneligibleInput {
                            input: i.dupe(),
                        },
                    )
                    .into());
                }
            }
        }

        this.state()?.register_action(
            artifacts.declared_outputs,
            action,
            Some(starlark_values),
            error_handler.into_option(),
        )?;
        Ok(NoneType)
    }
}
