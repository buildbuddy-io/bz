/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cell::RefCell;
use std::cell::RefMut;
use std::convert::Infallible;
use std::fmt;
use std::fmt::Formatter;
use std::sync::OnceLock;

use allocative::Allocative;
use buck2_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use buck2_core::cells::external::bzlmod_cell_aliases_for_cell;
use buck2_core::configuration::data::BazelBuildSettingValue;
use buck2_core::fs::buck_out_path::BazelOutputRoot;
use buck2_core::fs::buck_out_path::BuckOutPathKind;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersName;
use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
use buck2_core::target::label::label::TargetLabel;
use buck2_error::BuckErrorContext;
use buck2_error::conversion::from_any_with_tag;
use buck2_error::internal_error;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::execute::request::OutputType;
use buck2_interpreter::late_binding_ty::AnalysisContextReprLate;
use buck2_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use buck2_interpreter::types::target_label::StarlarkTargetLabel;
use buck2_util::late_binding::LateBinding;
use derive_more::Display;
use dice::DiceComputations;
use dupe::Dupe;
use futures::FutureExt;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::typing::Ty;
use starlark::values::AllocValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::ValueOf;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueTyped;
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::dict::FrozenDictRef;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::structs::StructRef;
use starlark::values::tuple::AllocTuple;
use starlark::values::tuple::TupleRef;
use starlark::values::type_repr::StarlarkTypeRepr;

use crate::actions::impls::workspace_status::UnregisteredWorkspaceStatusAction;
use crate::actions::impls::workspace_status::WorkspaceStatusKind;
use crate::analysis::anon_promises_dyn::RunAnonPromisesAccessor;
use crate::analysis::registry::AnalysisRegistry;
use crate::deferred::calculation::GET_PROMISED_ARTIFACT;
use crate::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use crate::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::StarlarkArtifactLike;
use crate::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use crate::interpreter::rule_defs::depset::BazelDepset;
use crate::interpreter::rule_defs::depset::bazel_depset_from_values;
use crate::interpreter::rule_defs::depset::bazel_depset_to_list;
use crate::interpreter::rule_defs::plugins::AnalysisPlugins;
use crate::interpreter::rule_defs::provider::builtin::constraint_value_info::ConstraintValueInfo;
use crate::interpreter::rule_defs::provider::builtin::default_info::BazelRunfiles;
use crate::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_from_files;
use crate::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_from_runfiles;
use crate::interpreter::rule_defs::provider::builtin::template_variable_info::FrozenTemplateVariableInfo;
use crate::interpreter::rule_defs::provider::dependency::Dependency;
use buck2_hash::BuckIndexSet;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum AnalysisContextError {
    #[error("attempting to access `build_setting_value` of non-build setting {0}")]
    NonBuildSetting(String),
    #[error("{0}")]
    MakeVariableExpansion(String),
    #[error("{message} while tokenizing '{option}'")]
    Tokenization { message: String, option: String },
}

/// Whether `declare_output` defaults `has_content_based_path` to `true`.
/// Controlled by `[buck2] declare_output_has_content_based_path_default` buckconfig.
pub static DECLARE_OUTPUT_HAS_CONTENT_BASED_PATH_DEFAULT: OnceLock<bool> = OnceLock::new();

pub fn init_declare_output_has_content_based_path_default(
    value: Option<bool>,
) -> buck2_error::Result<()> {
    let value = value.unwrap_or(false);
    DECLARE_OUTPUT_HAS_CONTENT_BASED_PATH_DEFAULT
        .set(value)
        .map_err(|_| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "DECLARE_OUTPUT_HAS_CONTENT_BASED_PATH_DEFAULT is already initialized"
            )
        })?;
    Ok(())
}

/// Whether artifact-creating actions default `has_content_based_path` to `true`
/// when a string name is passed as the output (i.e., the action implicitly
/// declares the output).
/// Controlled by `[buck2] action_has_content_based_path_default` buckconfig.
pub static ACTION_HAS_CONTENT_BASED_PATH_DEFAULT: OnceLock<bool> = OnceLock::new();

pub fn init_action_has_content_based_path_default(value: Option<bool>) -> buck2_error::Result<()> {
    let value = value.unwrap_or(false);
    ACTION_HAS_CONTENT_BASED_PATH_DEFAULT
        .set(value)
        .map_err(|_| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "ACTION_HAS_CONTENT_BASED_PATH_DEFAULT is already initialized"
            )
        })?;
    Ok(())
}

/// Functions to allow users to interact with the Actions registry.
///
/// Accessed via `ctx.actions.<function>`
#[derive(ProvidesStaticType, Debug, Display, Trace, NoSerialize, Allocative)]
#[display("<ctx.actions>")]
pub struct AnalysisActions<'v> {
    /// Use a RefCell/Option so when we are done with it, without obtaining exclusive access,
    /// we can take the internal state without having to clone it.
    pub state: RefCell<Option<AnalysisRegistry<'v>>>,
    pub label: Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>,
    pub toolchains: ValueTyped<'v, AnalysisToolchains<'v>>,
    pub bazel_cpp_options: BazelCppOptions,
    #[trace(unsafe_ignore)]
    pub bazel_output_root: BazelOutputRoot,
    /// Copies from the ctx, so we can capture them for `dynamic`.
    pub attributes: RefCell<Option<ValueOfUnchecked<'v, StructRef<'static>>>>,
    pub bazel_context_override: RefCell<Option<BazelActionsContextOverride<'v>>>,
    pub plugins: Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>,
    pub build_file_path: Option<String>,
    pub rule_kind_name: Option<String>,
    /// Digest configuration to use when interpreting digests passed in analysis.
    pub digest_config: DigestConfig,
}

#[derive(Clone, Debug, Trace, Allocative)]
pub struct BazelActionsContextOverride<'v> {
    pub label: Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>,
    pub build_file_path: Option<String>,
    pub rule_kind_name: Option<String>,
    pub toolchains: Option<ValueTyped<'v, AnalysisToolchains<'v>>>,
}

#[derive(ProvidesStaticType, Debug, Trace, NoSerialize, Allocative)]
pub struct AnalysisToolchains<'v> {
    toolchains: Vec<String>,
    resolved: SmallMap<String, Value<'v>>,
}

#[derive(Clone, Debug, Default, Trace, Allocative)]
pub struct BazelCppOptions {
    pub copt: Vec<String>,
    pub conlyopt: Vec<String>,
    pub cxxopt: Vec<String>,
    pub host_copt: Vec<String>,
    pub host_conlyopt: Vec<String>,
    pub host_cxxopt: Vec<String>,
    pub per_file_copt: Vec<String>,
    pub macos_minimum_os: Vec<String>,
    pub host_macos_minimum_os: Vec<String>,
}

impl BazelCppOptions {
    fn opts_for<'a>(
        &'a self,
        is_exec: bool,
        target: &'a [String],
        host: &'a [String],
    ) -> &'a [String] {
        if is_exec { host } else { target }
    }

    fn copt(&self, is_exec: bool) -> &[String] {
        self.opts_for(is_exec, &self.copt, &self.host_copt)
    }

    fn conlyopt(&self, is_exec: bool) -> &[String] {
        self.opts_for(is_exec, &self.conlyopt, &self.host_conlyopt)
    }

    fn cxxopt(&self, is_exec: bool) -> &[String] {
        self.opts_for(is_exec, &self.cxxopt, &self.host_cxxopt)
    }

    fn macos_minimum_os(&self, is_exec: bool) -> Option<&str> {
        self.opts_for(is_exec, &self.macos_minimum_os, &self.host_macos_minimum_os)
            .last()
            .map(String::as_str)
    }
}

impl Display for AnalysisToolchains<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "<ctx.toolchains>")
    }
}

impl<'v> AnalysisToolchains<'v> {
    fn new(toolchains: Vec<String>, resolved: SmallMap<String, Value<'v>>) -> Self {
        let toolchains = toolchains
            .into_iter()
            .map(|toolchain| Self::normalize_key(&toolchain))
            .collect();
        Self {
            toolchains,
            resolved,
        }
    }

    pub fn empty(heap: Heap<'v>) -> ValueTyped<'v, AnalysisToolchains<'v>> {
        heap.alloc_typed(Self::new(Vec::new(), SmallMap::new()))
    }

    fn normalize_key(key: &str) -> String {
        key.trim_start_matches('@').to_owned()
    }

    fn keys_match(requested: &str, declared: &str) -> bool {
        requested == declared
            || requested
                .split_once("//")
                .zip(declared.split_once("//"))
                .is_some_and(|((_, requested_rest), (_, declared_rest))| {
                    requested_rest == declared_rest
                })
    }

    fn declared_key_for(&self, key: &str) -> Option<&str> {
        self.toolchains
            .iter()
            .find(|declared| Self::keys_match(key, declared))
            .map(String::as_str)
    }

    fn key_from_value(value: Value<'_>) -> String {
        if let Some(label) = StarlarkProvidersLabel::from_value(value) {
            return Self::normalize_key(&label.to_string());
        }
        if let Some(label) = StarlarkTargetLabel::from_value(value) {
            return Self::normalize_key(&label.to_string());
        }
        if let Some(key) = value.unpack_str() {
            return Self::normalize_key(key);
        }
        value.to_repr()
    }

    pub fn with_declared_values(
        &self,
        heap: Heap<'v>,
        toolchains: impl IntoIterator<Item = Value<'v>>,
    ) -> ValueTyped<'v, AnalysisToolchains<'v>> {
        let toolchains = toolchains.into_iter().map(Self::key_from_value).collect();
        heap.alloc_typed(Self::new(toolchains, self.resolved.clone()))
    }

    fn contains_value(&self, value: Value<'_>) -> bool {
        let key = Self::key_from_value(value);
        self.declared_key_for(&key).is_some()
    }
}

impl<'v> AllocValue<'v> for AnalysisToolchains<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex_no_freeze(self)
    }
}

#[starlark_value(type = "ToolchainContext")]
impl<'v> StarlarkValue<'v> for AnalysisToolchains<'v> {
    fn at(&self, index: Value<'v>, _heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let key = Self::key_from_value(index);
        if let Some(declared_key) = self.declared_key_for(&key) {
            Ok(self
                .resolved
                .get(declared_key)
                .copied()
                .unwrap_or_else(Value::new_none))
        } else {
            Err(internal_error!(
                "toolchain `{}` was not declared by this rule",
                index.to_repr()
            )
            .into())
        }
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(self.contains_value(other))
    }
}

impl<'v> AnalysisActions<'v> {
    pub fn state(&self) -> buck2_error::Result<RefMut<'_, AnalysisRegistry<'v>>> {
        let state = self
            .state
            .try_borrow_mut()
            .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Tier0))
            .buck_error_context("AnalysisActions.state is already borrowed")?;
        RefMut::filter_map(state, |x| x.as_mut())
            .ok()
            .ok_or_else(|| internal_error!("state to be present during execution"))
    }

    pub async fn run_promises<'a, 'e: 'a>(
        &self,
        accessor: &mut dyn RunAnonPromisesAccessor<'v, 'a, 'e>,
    ) -> buck2_error::Result<bool>
    where
        'v: 'a,
    {
        // We need to loop here because running the promises evaluates promise.map, which might produce more promises.
        // We keep going until there are no promises left.
        let mut resolved_any = false;
        loop {
            let promises = self.state()?.take_promises();
            if let Some(promises) = promises {
                resolved_any = true;
                promises.run_promises(accessor).await?;
            } else {
                break;
            }
        }

        accessor
            .with_dice(|dice| self.assert_short_paths_and_resolve(dice).boxed_local())
            .await?;

        Ok(resolved_any)
    }

    // Called after `run_promises()` to assert short paths and resolve consumer's promise artifacts.
    pub async fn assert_short_paths_and_resolve(
        &self,
        dice: &mut DiceComputations<'_>,
    ) -> buck2_error::Result<()> {
        let (short_path_assertions, content_based_path_assertions, consumer_analysis_artifacts) = {
            let state = self.state()?;
            (
                state.short_path_assertions.clone(),
                state.content_based_path_assertions.clone(),
                state.consumer_analysis_artifacts(),
            )
        };

        for consumer_artifact in consumer_analysis_artifacts {
            let artifact = (GET_PROMISED_ARTIFACT.get()?)(&consumer_artifact, dice).await?;
            let id = consumer_artifact.id();
            let short_path = short_path_assertions.get(id).cloned();
            consumer_artifact.resolve(
                artifact.clone(),
                &short_path,
                content_based_path_assertions.contains(id),
            )?;
        }
        Ok(())
    }

    pub fn bazel_label(&self) -> Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>> {
        self.bazel_context_override
            .borrow()
            .as_ref()
            .and_then(|context| context.label)
            .or(self.label)
    }

    pub fn bazel_owner(&self) -> Option<ConfiguredTargetLabel> {
        self.bazel_label()
            .map(|label| label.as_ref().label().target().dupe())
    }

    pub fn bazel_build_file_path(&self) -> String {
        self.bazel_context_override
            .borrow()
            .as_ref()
            .and_then(|context| context.build_file_path.clone())
            .or_else(|| self.build_file_path.clone())
            .unwrap_or_else(|| bazel_build_file_path_from_label(self.bazel_label()))
    }

    pub fn bazel_rule_kind_name(&self) -> String {
        self.bazel_context_override
            .borrow()
            .as_ref()
            .and_then(|context| context.rule_kind_name.clone())
            .or_else(|| self.rule_kind_name.clone())
            .unwrap_or_default()
    }

    pub fn bazel_toolchains(&self) -> ValueTyped<'v, AnalysisToolchains<'v>> {
        self.bazel_context_override
            .borrow()
            .as_ref()
            .and_then(|context| context.toolchains)
            .unwrap_or(self.toolchains)
    }

    pub fn replace_bazel_context_override(
        &self,
        context: Option<BazelActionsContextOverride<'v>>,
    ) -> Option<BazelActionsContextOverride<'v>> {
        self.bazel_context_override.replace(context)
    }
}

#[starlark_value(type = "AnalysisActions", StarlarkTypeRepr, UnpackValue)]
impl<'v> StarlarkValue<'v> for AnalysisActions<'v> {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(|builder| {
            analysis_actions_methods_context(builder);
            (ANALYSIS_ACTIONS_METHODS_ACTIONS.get().unwrap())(builder);
            (ANALYSIS_ACTIONS_METHODS_ANON_TARGET.get().unwrap())(builder);
        })
    }
}

impl<'v> AllocValue<'v> for AnalysisActions<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex_no_freeze(self)
    }
}

#[allow(dead_code)] // field `0` is never read
struct RefAnalysisAction<'v>(&'v AnalysisActions<'v>);

impl<'v> StarlarkTypeRepr for RefAnalysisAction<'v> {
    type Canonical = <AnalysisActions<'v> as StarlarkTypeRepr>::Canonical;

    fn starlark_type_repr() -> Ty {
        AnalysisActions::starlark_type_repr()
    }
}

impl<'v> UnpackValue<'v> for RefAnalysisAction<'v> {
    type Error = Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        let Some(analysis_actions) = value.downcast_ref::<AnalysisActions>() else {
            return Ok(None);
        };
        Ok(Some(RefAnalysisAction(analysis_actions)))
    }
}

fn bazel_build_file_path_from_label(
    label: Option<ValueTyped<'_, StarlarkConfiguredProvidersLabel>>,
) -> String {
    let Some(label) = label else {
        return "BUILD.bazel".to_owned();
    };
    let package = label.label().target().pkg();
    let package = package.cell_relative_path().as_str();
    if package.is_empty() {
        "BUILD.bazel".to_owned()
    } else {
        format!("{package}/BUILD.bazel")
    }
}

pub fn bazel_workspace_name_for_cell(cell: &str) -> String {
    if cell == "root" {
        "_main".to_owned()
    } else {
        bzlmod_canonical_repo_name_for_cell(cell).unwrap_or_else(|| cell.to_owned())
    }
}

pub fn bazel_workspace_name_for_label(
    label: Option<ValueTyped<'_, StarlarkConfiguredProvidersLabel>>,
) -> String {
    let Some(label) = label else {
        return "_main".to_owned();
    };
    let cell = label.label().target().pkg().cell_name();
    bazel_workspace_name_for_cell(cell.as_str())
}

#[starlark_module]
fn analysis_actions_methods_context(builder: &mut MethodsBuilder) {
    /// Bazel-internal context recovery support for `cc_internal.actions2ctx_cheat`.
    #[starlark(attribute)]
    fn attr<'v>(this: &AnalysisActions<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(this
            .attributes
            .borrow()
            .as_ref()
            .map(|attrs| attrs.get())
            .unwrap_or_else(|| heap.alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new()))))
    }

    /// Alias for `attr` for Buck naming compatibility.
    #[starlark(attribute)]
    fn attrs<'v>(this: &AnalysisActions<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(this
            .attributes
            .borrow()
            .as_ref()
            .map(|attrs| attrs.get())
            .unwrap_or_else(|| heap.alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new()))))
    }

    #[starlark(attribute)]
    fn bin_dir<'v>(this: &AnalysisActions<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(bazel_file_root_for_label(
            heap,
            "buck-out/bin",
            this.bazel_label(),
        ))
    }

    #[starlark(attribute)]
    fn build_file_path<'v>(this: &AnalysisActions<'v>) -> starlark::Result<String> {
        Ok(this.bazel_build_file_path())
    }

    #[starlark(attribute)]
    fn configuration<'v>(
        this: &AnalysisActions<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(analysis_configuration(this.bazel_label(), heap).get())
    }

    #[starlark(attribute)]
    fn disabled_features<'v>(
        this: &AnalysisActions<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList::EMPTY))
    }

    #[starlark(attribute)]
    fn exec_groups<'v>(this: &AnalysisActions<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocDict::EMPTY))
    }

    #[starlark(attribute)]
    fn features<'v>(this: &AnalysisActions<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList::EMPTY))
    }

    #[starlark(attribute)]
    fn fragments<'v>(this: &AnalysisActions<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(bazel_fragments(
            heap,
            this.bazel_label(),
            this.bazel_cpp_options.clone(),
        ))
    }

    #[starlark(attribute)]
    fn genfiles_dir<'v>(this: &AnalysisActions<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(bazel_file_root_for_label(
            heap,
            "buck-out/genfiles",
            this.bazel_label(),
        ))
    }

    #[starlark(attribute)]
    fn label<'v>(
        this: &AnalysisActions<'v>,
    ) -> starlark::Result<NoneOr<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>> {
        Ok(NoneOr::from_option(this.bazel_label()))
    }

    #[starlark(attribute)]
    fn toolchains<'v>(
        this: &AnalysisActions<'v>,
    ) -> starlark::Result<ValueTyped<'v, AnalysisToolchains<'v>>> {
        Ok(this.bazel_toolchains())
    }

    #[starlark(attribute)]
    fn workspace_name<'v>(this: &AnalysisActions<'v>) -> starlark::Result<String> {
        Ok(bazel_workspace_name_for_label(this.bazel_label()))
    }

    #[starlark(attribute)]
    fn info_file<'v>(this: &AnalysisActions<'v>) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(Value::new_none())
    }

    #[starlark(attribute)]
    fn version_file<'v>(this: &AnalysisActions<'v>) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(Value::new_none())
    }
}

#[derive(ProvidesStaticType, Debug, Trace, NoSerialize, Allocative)]
pub struct AnalysisContext<'v> {
    attrs: RefCell<Option<ValueOfUnchecked<'v, StructRef<'static>>>>,
    split_attrs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
    outputs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
    pub actions: ValueTyped<'v, AnalysisActions<'v>>,
    /// Only `None` when running a `dynamic_output` action from Bxl.
    label: Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>,
    plugins: Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>,
    toolchains: ValueTyped<'v, AnalysisToolchains<'v>>,
    bazel_cpp_options: BazelCppOptions,
    is_bazel_build_setting: bool,
    build_file_path: Option<String>,
    rule_kind_name: Option<String>,
    bazel_info_file: RefCell<Option<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>>,
    bazel_version_file: RefCell<Option<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>>,
    bazel_file_structs: RefCell<Option<BazelFileStructs<'v>>>,
}

#[derive(Clone, Debug, Trace, Allocative)]
struct BazelFileStructs<'v> {
    file: ValueOfUnchecked<'v, StructRef<'static>>,
    files: ValueOfUnchecked<'v, StructRef<'static>>,
    executable: ValueOfUnchecked<'v, StructRef<'static>>,
}

impl<'v> Display for AnalysisContext<'v> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "<ctx")?;
        if let Some(label) = &self.label {
            write!(f, " label=\"{label}\"")?;
        }
        write!(f, " attrs=...")?;
        write!(f, " actions=...")?;
        write!(f, ">")?;
        Ok(())
    }
}

impl<'v> AnalysisContext<'v> {
    /// The context that is provided to users' UDR implementation functions. Comprised of things like attribute values, actions, etc
    fn new(
        heap: Heap<'v>,
        attrs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
        split_attrs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
        outputs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
        label: Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>,
        plugins: Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>,
        toolchains: Vec<String>,
        resolved_toolchains: SmallMap<String, Value<'v>>,
        bazel_cpp_options: BazelCppOptions,
        bazel_output_root: BazelOutputRoot,
        is_bazel_build_setting: bool,
        build_file_path: Option<String>,
        rule_kind_name: Option<String>,
        registry: AnalysisRegistry<'v>,
        digest_config: DigestConfig,
    ) -> Self {
        let toolchains = heap.alloc_typed(AnalysisToolchains::new(toolchains, resolved_toolchains));
        let actions = heap.alloc_typed(AnalysisActions {
            state: RefCell::new(Some(registry)),
            label,
            toolchains,
            bazel_cpp_options: bazel_cpp_options.clone(),
            bazel_output_root,
            attributes: RefCell::new(attrs),
            bazel_context_override: RefCell::new(None),
            plugins,
            build_file_path: build_file_path.clone(),
            rule_kind_name: rule_kind_name.clone(),
            digest_config,
        });
        Self {
            attrs: RefCell::new(attrs),
            split_attrs,
            outputs,
            actions,
            label,
            plugins,
            toolchains,
            bazel_cpp_options,
            is_bazel_build_setting,
            build_file_path,
            rule_kind_name,
            bazel_info_file: RefCell::new(None),
            bazel_version_file: RefCell::new(None),
            bazel_file_structs: RefCell::new(None),
        }
    }

    pub fn prepare(
        heap: Heap<'v>,
        attrs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
        split_attrs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
        outputs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
        label: Option<ConfiguredTargetLabel>,
        plugins: Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>,
        toolchains: Vec<String>,
        resolved_toolchains: SmallMap<String, Value<'v>>,
        bazel_cpp_options: BazelCppOptions,
        bazel_output_root: BazelOutputRoot,
        is_bazel_build_setting: bool,
        build_file_path: Option<String>,
        rule_kind_name: Option<String>,
        registry: AnalysisRegistry<'v>,
        digest_config: DigestConfig,
    ) -> ValueTyped<'v, AnalysisContext<'v>> {
        let label = label.map(|label| {
            heap.alloc_typed(StarlarkConfiguredProvidersLabel::new(
                ConfiguredProvidersLabel::new(label, ProvidersName::Default),
            ))
        });
        let analysis_context = Self::new(
            heap,
            attrs,
            split_attrs,
            outputs,
            label,
            plugins,
            toolchains,
            resolved_toolchains,
            bazel_cpp_options,
            bazel_output_root,
            is_bazel_build_setting,
            build_file_path,
            rule_kind_name,
            registry,
            digest_config,
        );
        heap.alloc_typed(analysis_context)
    }

    pub fn assert_no_promises(&self) -> buck2_error::Result<()> {
        self.actions.state()?.assert_no_promises()
    }

    pub fn set_attrs(&self, attrs: ValueOfUnchecked<'v, StructRef<'static>>) {
        *self.attrs.borrow_mut() = Some(attrs);
        *self.actions.attributes.borrow_mut() = Some(attrs);
        *self.bazel_file_structs.borrow_mut() = None;
    }

    /// Must take an `AnalysisContext` which has never had `take_state` called on it before.
    pub fn take_state(&self) -> AnalysisRegistry<'v> {
        self.actions
            .state
            .borrow_mut()
            .take()
            .expect("nothing to have stolen state yet")
    }
}

fn bazel_build_setting_value_to_starlark<'v>(
    value: &BazelBuildSettingValue,
    heap: Heap<'v>,
) -> Value<'v> {
    match value {
        BazelBuildSettingValue::Bool(value) => heap.alloc(*value).to_value(),
        BazelBuildSettingValue::Int(value) => heap.alloc(*value).to_value(),
        BazelBuildSettingValue::Label(value) => {
            heap.alloc(StarlarkProvidersLabel::new(value.clone()))
        }
        BazelBuildSettingValue::LabelList(values) => heap.alloc(
            values
                .iter()
                .map(|value| StarlarkProvidersLabel::new(value.clone()))
                .collect::<Vec<_>>(),
        ),
        BazelBuildSettingValue::String(value) => heap.alloc(value.as_str()).to_value(),
        BazelBuildSettingValue::StringList(values) => {
            let values = values.iter().map(String::as_str).collect::<Vec<_>>();
            heap.alloc(values).to_value()
        }
    }
}

fn struct_field<'v>(
    value: ValueOfUnchecked<'v, StructRef<'static>>,
    field: &str,
) -> Option<Value<'v>> {
    StructRef::from_value(value.get())?
        .iter()
        .find_map(|(name, value)| (name.as_str() == field).then_some(value))
}

fn analysis_context_attrs<'v>(
    ctx: &AnalysisContext<'v>,
) -> buck2_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    ctx.attrs
        .borrow()
        .as_ref()
        .copied()
        .ok_or_else(|| internal_error!("`attrs` is not available for `dynamic_output` or BXL"))
}

fn analysis_context_split_attrs<'v>(
    ctx: &AnalysisContext<'v>,
) -> buck2_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    ctx.split_attrs
        .ok_or_else(|| internal_error!("`split_attr` is not available for `dynamic_output` or BXL"))
}

fn analysis_context_outputs<'v>(
    ctx: &AnalysisContext<'v>,
) -> buck2_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    ctx.outputs
        .ok_or_else(|| internal_error!("`outputs` is not available for `dynamic_output` or BXL"))
}

fn analysis_context_rule<'v>(
    ctx: &AnalysisContext<'v>,
    heap: Heap<'v>,
) -> buck2_error::Result<Value<'v>> {
    let attrs = analysis_context_attrs(ctx)?.get();
    let kind = ctx.rule_kind_name.as_deref().unwrap_or("");
    Ok(heap.alloc(AllocStruct([
        ("attr", attrs),
        ("kind", heap.alloc_str(kind).to_value()),
    ])))
}

fn bazel_file_root<'v>(heap: Heap<'v>, path: &str) -> Value<'v> {
    heap.alloc(AllocStruct([("path", heap.alloc_str(path).to_value())]))
}

fn bazel_configuration_exec_path(label: &ConfiguredTargetLabel) -> String {
    let mut path = label.cfg().output_hash().as_str().to_owned();
    if let Some(exec_cfg) = label.exec_cfg() {
        path.push('-');
        path.push_str(exec_cfg.output_hash().as_str());
    }
    path
}

fn bazel_output_root_for_configured_label(root: &str, label: &ConfiguredTargetLabel) -> String {
    let mut path = root.to_owned();
    path.push('/');
    path.push_str(&bazel_configuration_exec_path(label));
    path
}

fn bazel_output_root_for_label(
    root: &str,
    label: Option<ValueTyped<'_, StarlarkConfiguredProvidersLabel>>,
) -> String {
    label.map_or_else(
        || root.to_owned(),
        |label| bazel_output_root_for_configured_label(root, label.label().target()),
    )
}

fn bazel_file_root_for_label<'v>(
    heap: Heap<'v>,
    root: &str,
    label: Option<ValueTyped<'_, StarlarkConfiguredProvidersLabel>>,
) -> Value<'v> {
    let path = bazel_output_root_for_label(root, label);
    bazel_file_root(heap, &path)
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelConfigurationBoolMethod {
    name: &'static str,
    value: bool,
}

impl fmt::Display for BazelConfigurationBoolMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<configuration method {}>", self.name)
    }
}

starlark::starlark_simple_value!(BazelConfigurationBoolMethod);

#[starlark_value(type = "function")]
impl<'v> StarlarkValue<'v> for BazelConfigurationBoolMethod {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        Ok(Value::new_bool(self.value))
    }
}

fn bazel_configuration_bool_method<'v>(
    heap: Heap<'v>,
    name: &'static str,
    value: bool,
) -> Value<'v> {
    heap.alloc(BazelConfigurationBoolMethod { name, value })
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelTokenizeFunction;

impl fmt::Display for BazelTokenizeFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<ctx.tokenize>")
    }
}

starlark::starlark_simple_value!(BazelTokenizeFunction);

#[starlark_value(type = "function")]
impl<'v> StarlarkValue<'v> for BazelTokenizeFunction {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_named_args()?;
        let positions = args.positions(eval.heap())?.collect::<Vec<_>>();
        let [option] = positions.as_slice() else {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "ctx.tokenize() expects exactly one positional argument"
            )
            .into());
        };
        let Some(option) = option.unpack_str() else {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "ctx.tokenize() expected str, got `{}`",
                option.get_type()
            )
            .into());
        };
        let tokens = bazel_shell_tokenize(option)?;
        Ok(bazel_string_list(eval.heap(), &tokens))
    }
}

fn bazel_tokenize_function<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(BazelTokenizeFunction)
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCoverageInstrumentedFunction;

impl fmt::Display for BazelCoverageInstrumentedFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<ctx.coverage_instrumented>")
    }
}

starlark::starlark_simple_value!(BazelCoverageInstrumentedFunction);

#[starlark_value(type = "function")]
impl<'v> StarlarkValue<'v> for BazelCoverageInstrumentedFunction {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_named_args()?;
        let positions = args.positions(eval.heap())?.collect::<Vec<_>>();
        if positions.len() > 1 {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "ctx.coverage_instrumented() expects at most one positional argument"
            )
            .into());
        }
        Ok(Value::new_bool(false))
    }
}

fn bazel_coverage_instrumented_function<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(BazelCoverageInstrumentedFunction)
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCppConfiguration {
    options: BazelCppOptions,
    is_exec: bool,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelAppleConfiguration {
    options: BazelCppOptions,
    is_exec: bool,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelJavaConfiguration;

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelPlatformConfiguration {
    platform: String,
    host_platform: String,
}

impl fmt::Display for BazelCppConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<cpp fragment>")
    }
}

impl fmt::Display for BazelAppleConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<apple fragment>")
    }
}

impl fmt::Display for BazelJavaConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<java fragment>")
    }
}

impl fmt::Display for BazelPlatformConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<platform fragment>")
    }
}

starlark::starlark_simple_value!(BazelCppConfiguration);
starlark::starlark_simple_value!(BazelAppleConfiguration);
starlark::starlark_simple_value!(BazelJavaConfiguration);
starlark::starlark_simple_value!(BazelPlatformConfiguration);

#[starlark_value(type = "cpp")]
impl<'v> StarlarkValue<'v> for BazelCppConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cpp_configuration_methods)
    }
}

#[starlark_value(type = "apple")]
impl<'v> StarlarkValue<'v> for BazelAppleConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_apple_configuration_methods)
    }
}

#[starlark_value(type = "java")]
impl<'v> StarlarkValue<'v> for BazelJavaConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_java_configuration_methods)
    }
}

#[starlark_value(type = "platform")]
impl<'v> StarlarkValue<'v> for BazelPlatformConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_platform_configuration_methods)
    }
}

fn bazel_empty_list<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocList::EMPTY)
}

fn bazel_string_list<'v>(heap: Heap<'v>, values: &[String]) -> Value<'v> {
    heap.alloc(AllocList(
        values.iter().map(|value| heap.alloc_str(value).to_value()),
    ))
}

pub(crate) fn bazel_shell_tokenize(option_string: &str) -> buck2_error::Result<Vec<String>> {
    let mut options = Vec::new();
    let mut token = String::new();
    let mut force_token = false;
    let mut quotation = None;
    let mut chars = option_string.chars();

    while let Some(c) = chars.next() {
        if let Some(quote) = quotation {
            if c == quote {
                quotation = None;
            } else if c == '\\' && quote == '"' {
                let Some(next) = chars.next() else {
                    return Err(buck2_error::Error::from(
                        AnalysisContextError::Tokenization {
                            message: "backslash at end of string".to_owned(),
                            option: option_string.to_owned(),
                        },
                    ));
                };
                if next != '\\' && next != '"' {
                    token.push('\\');
                }
                token.push(next);
            } else {
                token.push(c);
            }
        } else if c == '\'' || c == '"' {
            quotation = Some(c);
            force_token = true;
        } else if c == ' ' || c == '\t' {
            if force_token || !token.is_empty() {
                options.push(std::mem::take(&mut token));
                force_token = false;
            }
        } else if c == '\\' {
            let Some(next) = chars.next() else {
                return Err(buck2_error::Error::from(
                    AnalysisContextError::Tokenization {
                        message: "backslash at end of string".to_owned(),
                        option: option_string.to_owned(),
                    },
                ));
            };
            token.push(next);
        } else {
            token.push(c);
        }
    }

    if quotation.is_some() {
        return Err(buck2_error::Error::from(
            AnalysisContextError::Tokenization {
                message: "unterminated quotation".to_owned(),
                option: option_string.to_owned(),
            },
        ));
    }

    if force_token || !token.is_empty() {
        options.push(token);
    }

    Ok(options)
}

fn bazel_apple_platform<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("name", heap.alloc_str("macos").to_value()),
        ("platform_type", heap.alloc_str("macos").to_value()),
        ("is_device", Value::new_bool(true)),
        ("name_in_plist", heap.alloc_str("MacOSX").to_value()),
    ]))
}

fn bazel_apple_minimum_os<'v>(
    heap: Heap<'v>,
    options: &BazelCppOptions,
    is_exec: bool,
) -> Value<'v> {
    options
        .macos_minimum_os(is_exec)
        .map(|value| heap.alloc_str(value).to_value())
        .unwrap_or_else(Value::new_none)
}

#[starlark_module]
fn bazel_java_configuration_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn default_javac_flags<'v>(
        #[starlark(this)] this: &BazelJavaConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_empty_list(heap))
    }

    #[starlark(attribute)]
    fn default_javac_flags_depset<'v>(
        #[starlark(this)] this: &BazelJavaConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_depset_from_values(heap, Vec::<Value>::new())?)
    }

    #[starlark(attribute)]
    fn strict_java_deps(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<&'static str> {
        let _ = this;
        Ok("default")
    }

    fn use_header_compilation(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn generate_java_deps(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn reduce_java_classpath(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<&'static str> {
        let _ = this;
        Ok("BAZEL")
    }

    #[starlark(attribute)]
    fn default_jvm_opts<'v>(
        #[starlark(this)] this: &BazelJavaConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_empty_list(heap))
    }

    #[starlark(attribute)]
    fn one_version_enforcement_level(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<&'static str> {
        let _ = this;
        Ok("OFF")
    }

    #[starlark(attribute)]
    fn one_version_enforcement_on_java_tests(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(true)
    }

    #[starlark(attribute)]
    fn add_test_support_to_compile_deps(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(true)
    }

    #[starlark(attribute)]
    fn run_android_lint(#[starlark(this)] this: &BazelJavaConfiguration) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn enforce_explicit_java_test_deps(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }

    #[starlark(attribute)]
    fn multi_release_deploy_jars(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(true)
    }

    #[starlark(attribute)]
    fn plugins<'v>(
        #[starlark(this)] this: &BazelJavaConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_empty_list(heap))
    }

    fn use_ijars(#[starlark(this)] this: &BazelJavaConfiguration) -> starlark::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn use_header_compilation_direct_deps(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn disallow_java_import_exports(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }

    #[starlark(attribute)]
    fn bytecode_optimizer_mnemonic(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<&'static str> {
        let _ = this;
        Ok("Proguard")
    }

    #[starlark(attribute)]
    fn split_bytecode_optimization_pass(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }

    #[starlark(attribute)]
    fn bytecode_optimization_pass_actions(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<i32> {
        let _ = this;
        Ok(1)
    }

    #[starlark(attribute)]
    fn enforce_proguard_file_extension(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn auto_create_java_test_deploy_jars(
        #[starlark(this)] this: &BazelJavaConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }
}

#[starlark_module]
fn bazel_platform_configuration_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn platform<'v>(
        #[starlark(this)] this: &BazelPlatformConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(heap.alloc_str(&this.platform).to_value())
    }

    #[starlark(attribute)]
    fn host_platform<'v>(
        #[starlark(this)] this: &BazelPlatformConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(heap.alloc_str(&this.host_platform).to_value())
    }

    #[starlark(attribute)]
    fn platforms<'v>(
        #[starlark(this)] this: &BazelPlatformConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(heap.alloc(AllocList([heap.alloc_str(&this.platform).to_value()])))
    }
}

#[starlark_module]
fn bazel_apple_configuration_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn single_arch_platform<'v>(
        #[starlark(this)] this: &BazelAppleConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_apple_platform(heap))
    }

    #[starlark(attribute)]
    fn ios_minimum_os_flag<'v>(
        #[starlark(this)] this: &BazelAppleConfiguration,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(Value::new_none())
    }

    #[starlark(attribute)]
    fn macos_minimum_os_flag<'v>(
        #[starlark(this)] this: &BazelAppleConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(bazel_apple_minimum_os(heap, &this.options, this.is_exec))
    }

    #[starlark(attribute)]
    fn tvos_minimum_os_flag<'v>(
        #[starlark(this)] this: &BazelAppleConfiguration,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(Value::new_none())
    }

    #[starlark(attribute)]
    fn watchos_minimum_os_flag<'v>(
        #[starlark(this)] this: &BazelAppleConfiguration,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(Value::new_none())
    }
}

#[starlark_module]
fn bazel_cpp_configuration_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn _dont_enable_host_nonhost(
        #[starlark(this)] this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }

    #[starlark(attribute)]
    fn _fdo_prefetch_hints_label<'v>(
        #[starlark(this)] this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(Value::new_none())
    }

    #[starlark(attribute)]
    fn apple_generate_dsym(
        #[starlark(this)] this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }

    #[starlark(attribute)]
    fn conlyopts<'v>(
        #[starlark(this)] this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(bazel_string_list(heap, this.options.conlyopt(this.is_exec)))
    }

    #[starlark(attribute)]
    fn copts<'v>(
        #[starlark(this)] this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(bazel_string_list(heap, this.options.copt(this.is_exec)))
    }

    #[starlark(attribute)]
    fn custom_malloc<'v>(
        #[starlark(this)] this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(Value::new_none())
    }

    #[starlark(attribute)]
    fn cxxopts<'v>(
        #[starlark(this)] this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(bazel_string_list(heap, this.options.cxxopt(this.is_exec)))
    }

    #[starlark(attribute)]
    fn do_not_use_macos_set_install_name(
        #[starlark(this)] this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(true)
    }

    #[starlark(attribute)]
    fn linkopts<'v>(
        #[starlark(this)] this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_empty_list(heap))
    }

    #[starlark(attribute)]
    fn lto_backend_options<'v>(
        #[starlark(this)] this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_empty_list(heap))
    }

    #[starlark(attribute)]
    fn objc_generate_linkmap(
        #[starlark(this)] this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }

    #[starlark(attribute)]
    fn objc_should_strip_binary(
        #[starlark(this)] this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        let _ = this;
        Ok(false)
    }

    #[starlark(attribute)]
    fn objccopts<'v>(
        #[starlark(this)] this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_empty_list(heap))
    }

    fn build_test_dwp(#[starlark(this)] _this: &BazelCppConfiguration) -> starlark::Result<bool> {
        Ok(false)
    }

    fn compilation_mode(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<&'static str> {
        Ok("fastbuild")
    }

    fn cs_fdo_instrument<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn cs_fdo_path<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn disable_nocopts(#[starlark(this)] _this: &BazelCppConfiguration) -> starlark::Result<bool> {
        Ok(true)
    }

    fn dynamic_mode(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<&'static str> {
        Ok("DEFAULT")
    }

    fn experimental_cc_implementation_deps(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn experimental_cpp_modules(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn experimental_link_static_libraries_once(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn extra_allowlisted_feature_layering_check_macros<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(bazel_empty_list(heap))
    }

    fn fdo_instrument<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn fdo_path<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn fission_active_for_current_compilation_mode(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn force_layering_check_features(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn force_pic(#[starlark(this)] _this: &BazelCppConfiguration) -> starlark::Result<bool> {
        Ok(false)
    }

    fn generate_llvm_lcov(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn grte_top<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn incompatible_remove_legacy_whole_archive(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn incompatible_use_specific_tool_files(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn interface_shared_objects(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn legacy_whole_archive(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn lto_index_options<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(bazel_empty_list(heap))
    }

    fn minimum_os_version<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn objc_should_generate_dotd_files(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn process_headers_in_dependencies(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn propeller_optimize_absolute_cc_profile<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn propeller_optimize_absolute_ld_profile<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn proto_profile(#[starlark(this)] _this: &BazelCppConfiguration) -> starlark::Result<bool> {
        Ok(true)
    }

    fn save_feature_state(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn save_temps(#[starlark(this)] _this: &BazelCppConfiguration) -> starlark::Result<bool> {
        Ok(false)
    }

    fn share_native_deps(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn should_generate_dotd_files(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn should_strip_binaries(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn start_end_lib(#[starlark(this)] _this: &BazelCppConfiguration) -> starlark::Result<bool> {
        Ok(true)
    }

    fn strip_opts<'v>(
        #[starlark(this)] _this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(bazel_empty_list(heap))
    }

    fn use_llvm_coverage_map_format(
        #[starlark(this)] _this: &BazelCppConfiguration,
    ) -> starlark::Result<bool> {
        Ok(false)
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelProtoConfiguration;

impl fmt::Display for BazelProtoConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<proto fragment>")
    }
}

starlark::starlark_simple_value!(BazelProtoConfiguration);

#[starlark_value(type = "proto")]
impl<'v> StarlarkValue<'v> for BazelProtoConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_proto_configuration_methods)
    }
}

#[starlark_module]
fn bazel_proto_configuration_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn experimental_protoc_opts<'v>(
        #[starlark(this)] this: &BazelProtoConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_empty_list(heap))
    }

    #[starlark(attribute)]
    fn cc_proto_library_header_suffixes<'v>(
        #[starlark(this)] this: &BazelProtoConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList([heap.alloc_str(".pb.h").to_value()])))
    }

    #[starlark(attribute)]
    fn cc_proto_library_source_suffixes<'v>(
        #[starlark(this)] this: &BazelProtoConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList([heap.alloc_str(".pb.cc").to_value()])))
    }

    fn strict_proto_deps(
        #[starlark(this)] this: &BazelProtoConfiguration,
    ) -> starlark::Result<&'static str> {
        let _ = this;
        Ok("ERROR")
    }

    fn strict_public_imports(
        #[starlark(this)] this: &BazelProtoConfiguration,
    ) -> starlark::Result<&'static str> {
        let _ = this;
        Ok("OFF")
    }
}

fn bazel_is_exec_configuration(
    label: Option<ValueTyped<'_, StarlarkConfiguredProvidersLabel>>,
) -> bool {
    label.is_some_and(|label| label.label().target().cfg().is_marked_as_exec_platform())
}

fn bazel_platform_label(label: Option<ValueTyped<'_, StarlarkConfiguredProvidersLabel>>) -> String {
    label
        .and_then(|label| label.label().target().cfg().label().ok().map(str::to_owned))
        .unwrap_or_else(|| "@@platforms//host:host".to_owned())
}

fn bazel_fragments<'v>(
    heap: Heap<'v>,
    label: Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>,
    bazel_cpp_options: BazelCppOptions,
) -> Value<'v> {
    let is_exec = bazel_is_exec_configuration(label);
    let platform = bazel_platform_label(label);
    heap.alloc(AllocStruct([
        (
            "apple",
            heap.alloc(BazelAppleConfiguration {
                options: bazel_cpp_options.clone(),
                is_exec,
            }),
        ),
        (
            "cpp",
            heap.alloc(BazelCppConfiguration {
                options: bazel_cpp_options,
                is_exec,
            }),
        ),
        ("java", heap.alloc(BazelJavaConfiguration)),
        (
            "platform",
            heap.alloc(BazelPlatformConfiguration {
                platform,
                host_platform: "@@platforms//host:host".to_owned(),
            }),
        ),
        ("proto", heap.alloc(BazelProtoConfiguration)),
    ]))
}

fn analysis_configuration<'v>(
    label: Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>,
    heap: Heap<'v>,
) -> ValueOfUnchecked<'v, StructRef<'static>> {
    let host_path_separator = if cfg!(windows) { ";" } else { ":" };
    let bin_dir = bazel_file_root_for_label(heap, "buck-out/bin", label);
    let genfiles_dir = bazel_file_root_for_label(heap, "buck-out/genfiles", label);
    let is_tool_configuration = bazel_is_exec_configuration(label);
    ValueOfUnchecked::new(heap.alloc(AllocStruct([
        ("bin_dir", bin_dir),
        ("genfiles_dir", genfiles_dir),
        (
            "host_path_separator",
            heap.alloc_str(host_path_separator).to_value(),
        ),
        ("default_shell_env", heap.alloc(AllocDict::EMPTY)),
        ("test_env", heap.alloc(AllocDict::EMPTY)),
        ("coverage_enabled", Value::new_bool(false)),
        ("short_id", heap.alloc_str("buck2").to_value()),
        (
            "has_separate_genfiles_directory",
            bazel_configuration_bool_method(heap, "has_separate_genfiles_directory", false),
        ),
        (
            "is_sibling_repository_layout",
            bazel_configuration_bool_method(heap, "is_sibling_repository_layout", false),
        ),
        (
            "is_tool_configuration",
            bazel_configuration_bool_method(heap, "is_tool_configuration", is_tool_configuration),
        ),
        (
            "runfiles_enabled",
            bazel_configuration_bool_method(heap, "runfiles_enabled", !cfg!(windows)),
        ),
        (
            "stamp_binaries",
            bazel_configuration_bool_method(heap, "stamp_binaries", false),
        ),
    ])))
}

fn analysis_context_configuration<'v>(
    ctx: &AnalysisContext<'v>,
    heap: Heap<'v>,
) -> ValueOfUnchecked<'v, StructRef<'static>> {
    analysis_configuration(ctx.label, heap)
}

pub fn analysis_actions_to_bazel_ctx<'v>(
    actions: ValueTyped<'v, AnalysisActions<'v>>,
    heap: Heap<'v>,
) -> Value<'v> {
    let this = actions.as_ref();
    let empty_struct = heap.alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new()));
    let attr = this
        .attributes
        .borrow()
        .as_ref()
        .map(|attrs| attrs.get())
        .unwrap_or(empty_struct);
    analysis_actions_to_bazel_ctx_with_overrides(
        actions,
        heap,
        attr,
        attr,
        this.bazel_label(),
        this.bazel_build_file_path(),
        this.bazel_rule_kind_name(),
    )
}

pub fn analysis_actions_to_bazel_ctx_with_overrides<'v>(
    actions: ValueTyped<'v, AnalysisActions<'v>>,
    heap: Heap<'v>,
    attr: Value<'v>,
    rule_attr: Value<'v>,
    label: Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>,
    build_file_path: String,
    rule_kind: String,
) -> Value<'v> {
    let this = actions.as_ref();
    let empty_struct = heap.alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new()));
    let label_value = label
        .map(|label| label.to_value())
        .unwrap_or_else(Value::new_none);
    heap.alloc(AllocStruct([
        ("actions", actions.to_value()),
        ("attr", attr),
        ("attrs", attr),
        (
            "bin_dir",
            bazel_file_root_for_label(heap, "buck-out/bin", label),
        ),
        (
            "build_file_path",
            heap.alloc_str(build_file_path.as_str()).to_value(),
        ),
        ("configuration", analysis_configuration(label, heap).get()),
        (
            "coverage_instrumented",
            bazel_coverage_instrumented_function(heap),
        ),
        ("disabled_features", heap.alloc(AllocList::EMPTY)),
        ("exec_groups", heap.alloc(AllocDict::EMPTY)),
        ("executable", empty_struct),
        ("features", heap.alloc(AllocList::EMPTY)),
        ("file", empty_struct),
        ("files", empty_struct),
        (
            "fragments",
            bazel_fragments(heap, label, this.bazel_cpp_options.clone()),
        ),
        (
            "genfiles_dir",
            bazel_file_root_for_label(heap, "buck-out/genfiles", label),
        ),
        ("info_file", Value::new_none()),
        ("label", label_value),
        ("outputs", empty_struct),
        (
            "rule",
            heap.alloc(AllocStruct([
                ("attr", rule_attr),
                ("kind", heap.alloc_str(&rule_kind).to_value()),
            ])),
        ),
        ("rule_class", heap.alloc_str(&rule_kind).to_value()),
        ("tokenize", bazel_tokenize_function(heap)),
        ("toolchains", this.bazel_toolchains().to_value()),
        ("version_file", Value::new_none()),
        (
            "workspace_name",
            heap.alloc_str(&bazel_workspace_name_for_label(label))
                .to_value(),
        ),
    ]))
}

fn collect_bazel_files_from_value<'v>(
    value: Value<'v>,
    files: &mut Vec<Value<'v>>,
) -> buck2_error::Result<()> {
    if value.is_none() {
        return Ok(());
    }
    if let Some(dep) = value.downcast_ref::<Dependency<'v>>() {
        files.extend(dep.default_output_values()?);
        return Ok(());
    }
    if value.downcast_ref::<StarlarkArtifact>().is_some() {
        files.push(value);
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        for item in list.iter() {
            collect_bazel_files_from_value(item, files)?;
        }
    }
    Ok(())
}

fn bazel_files_from_attr_value<'v>(value: Value<'v>) -> buck2_error::Result<Vec<Value<'v>>> {
    let mut files = Vec::new();
    collect_bazel_files_from_value(value, &mut files)?;
    Ok(files)
}

fn bazel_executable_from_attr_value<'v>(
    value: Value<'v>,
) -> buck2_error::Result<Option<Value<'v>>> {
    if value.is_none() {
        return Ok(None);
    }
    if let Some(dep) = Dependency::from_value(value) {
        return dep.files_to_run_executable();
    }
    if value.downcast_ref::<StarlarkArtifact>().is_some() {
        return Ok(Some(value));
    }
    Ok(None)
}

fn analysis_context_bazel_file_structs_from_attrs<'v>(
    heap: Heap<'v>,
    attrs: ValueOfUnchecked<'v, StructRef<'static>>,
) -> buck2_error::Result<(
    ValueOfUnchecked<'v, StructRef<'static>>,
    ValueOfUnchecked<'v, StructRef<'static>>,
    ValueOfUnchecked<'v, StructRef<'static>>,
)> {
    let mut file_fields = Vec::new();
    let mut files_fields = Vec::new();
    let mut executable_fields = Vec::new();
    if let Some(attrs) = StructRef::from_value(attrs.get()) {
        for (name, value) in attrs.iter() {
            let name = name.as_str().to_owned();
            let files = bazel_files_from_attr_value(value)?;
            files_fields.push((name.clone(), heap.alloc(files.clone())));
            let executable = bazel_executable_from_attr_value(value)?;
            let single_file = match files.as_slice() {
                [file] => *file,
                [] => Value::new_none(),
                _ => {
                    if let Some(executable) = executable {
                        executable_fields.push((name, executable));
                    }
                    continue;
                }
            };
            file_fields.push((name.clone(), single_file));
            executable_fields.push((name, executable.unwrap_or(single_file)));
        }
    }
    Ok((
        ValueOfUnchecked::new(heap.alloc(AllocStruct(file_fields))),
        ValueOfUnchecked::new(heap.alloc(AllocStruct(files_fields))),
        ValueOfUnchecked::new(heap.alloc(AllocStruct(executable_fields))),
    ))
}

fn analysis_context_bazel_file_structs<'v>(
    ctx: &AnalysisContext<'v>,
    heap: Heap<'v>,
) -> buck2_error::Result<BazelFileStructs<'v>> {
    if let Some(structs) = ctx.bazel_file_structs.borrow().as_ref() {
        return Ok(structs.clone());
    }

    let attrs = analysis_context_attrs(ctx)?;
    let (file, files, executable) = analysis_context_bazel_file_structs_from_attrs(heap, attrs)?;
    let structs = BazelFileStructs {
        file,
        files,
        executable,
    };
    *ctx.bazel_file_structs.borrow_mut() = Some(structs.clone());
    Ok(structs)
}

fn bazel_collect_runfiles_from_attr_value<'v>(
    value: Value<'v>,
    collect_data: bool,
    collect_default: bool,
    runfiles: &mut Vec<&'v BazelRunfiles<'v>>,
) -> starlark::Result<()> {
    if value.is_none() {
        return Ok(());
    }
    if let Some(dep) = Dependency::from_value(value) {
        if collect_data {
            runfiles.push(dep.data_runfiles()?);
        }
        if collect_default {
            runfiles.push(dep.default_runfiles()?);
        }
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        for item in list.iter() {
            bazel_collect_runfiles_from_attr_value(item, collect_data, collect_default, runfiles)?;
        }
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        for item in tuple.iter() {
            bazel_collect_runfiles_from_attr_value(item, collect_data, collect_default, runfiles)?;
        }
        return Ok(());
    }
    if let Some(dict) = DictRef::from_value(value) {
        for (key, value) in dict.iter() {
            bazel_collect_runfiles_from_attr_value(key, collect_data, collect_default, runfiles)?;
            bazel_collect_runfiles_from_attr_value(value, collect_data, collect_default, runfiles)?;
        }
    }
    Ok(())
}

fn bazel_collect_runfiles_from_attrs<'v>(
    attrs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
    collect_data: bool,
    collect_default: bool,
    runfiles: &mut Vec<&'v BazelRunfiles<'v>>,
) -> starlark::Result<()> {
    let Some(attrs) = attrs else {
        return Ok(());
    };
    let Some(attrs) = StructRef::from_value(attrs.get()) else {
        return Ok(());
    };
    for (name, value) in attrs.iter() {
        if matches!(name.as_str(), "srcs" | "data" | "deps") {
            bazel_collect_runfiles_from_attr_value(value, collect_data, collect_default, runfiles)?;
        }
    }
    Ok(())
}

#[derive(Clone)]
struct BazelLocationTarget {
    exec_paths: Vec<String>,
    rlocation_paths: Vec<String>,
}

fn bazel_location_label_keys_for_target<'v>(
    ctx: &AnalysisContext<'v>,
    target: &TargetLabel,
) -> Vec<String> {
    let package = target.pkg();
    let package_path = package.cell_relative_path().as_str();
    let cell = package.cell_name().as_str();
    let name = target.name().as_str();

    let mut keys = Vec::new();
    let full = target.to_string();
    keys.push(full.clone());
    if let Some(root_relative) = full.strip_prefix("root") {
        if root_relative.starts_with("//") {
            keys.push(root_relative.to_owned());
        }
    }
    if let Some(current) = ctx.label
        && current.label().target().pkg().cell_name().as_str() == cell
    {
        bazel_push_repo_local_location_label_key(&mut keys, package_path, name);
    }
    if cell != "root" {
        bazel_push_external_location_label_key(
            &mut keys,
            &bazel_workspace_name_for_cell(cell),
            package_path,
            name,
        );
    }

    if let Some(current) = ctx.label {
        let current_cell = current.label().target().pkg().cell_name();
        for (alias, destination) in bzlmod_cell_aliases_for_cell(current_cell.as_str()) {
            if destination == cell {
                bazel_push_external_location_label_key(&mut keys, &alias, package_path, name);
            }
        }
    }

    if ctx.label.is_some_and(|label| {
        let package = label.label().target().pkg();
        package.cell_name().as_str() == cell
            && package.cell_relative_path().as_str() == package_path
    }) {
        keys.push(format!(":{name}"));
        keys.push(name.to_owned());
    }

    keys
}

fn bazel_push_repo_local_location_label_key(
    keys: &mut Vec<String>,
    package_path: &str,
    name: &str,
) {
    if package_path.is_empty() {
        keys.push(format!("//:{name}"));
    } else {
        keys.push(format!("//{package_path}:{name}"));
        if package_path.rsplit('/').next() == Some(name) {
            keys.push(format!("//{package_path}"));
        }
    }
}

fn bazel_push_external_location_label_key(
    keys: &mut Vec<String>,
    repo: &str,
    package_path: &str,
    name: &str,
) {
    if package_path.is_empty() {
        keys.push(format!("@{repo}//:{name}"));
    } else {
        keys.push(format!("@{repo}//{package_path}:{name}"));
    }
}

fn bazel_rlocation_path_for_artifact<'v>(
    artifact: &'v dyn StarlarkArtifactLike<'v>,
    short_paths: bool,
    heap: Heap<'v>,
) -> buck2_error::Result<String> {
    if short_paths {
        return Ok(artifact
            .with_bazel_short_path(&|path| heap.alloc_str(path))?
            .as_str()
            .to_owned());
    }
    let path = artifact
        .with_bazel_path(&|path| heap.alloc_str(path))?
        .as_str()
        .to_owned();
    if let Some(external_path) = path.strip_prefix("external/") {
        Ok(external_path.to_owned())
    } else {
        Ok(format!("{}/{}", bazel_workspace_name_for_label(None), path))
    }
}

fn bazel_location_target_for_artifact<'v>(
    artifact: &'v dyn StarlarkArtifactLike<'v>,
    short_paths: bool,
    heap: Heap<'v>,
) -> buck2_error::Result<BazelLocationTarget> {
    let exec_path = if short_paths {
        artifact
            .with_bazel_short_path(&|path| heap.alloc_str(path))?
            .as_str()
            .to_owned()
    } else {
        artifact
            .with_bazel_path(&|path| heap.alloc_str(path))?
            .as_str()
            .to_owned()
    };
    Ok(BazelLocationTarget {
        exec_paths: vec![exec_path],
        rlocation_paths: vec![bazel_rlocation_path_for_artifact(
            artifact,
            short_paths,
            heap,
        )?],
    })
}

fn bazel_location_target_for_dep<'v>(
    dep: &Dependency<'v>,
    short_paths: bool,
    prefer_executable: bool,
    heap: Heap<'v>,
) -> starlark::Result<BazelLocationTarget> {
    if prefer_executable
        && let Some(executable) = dep.files_to_run_executable()?
        && let Some(artifact) = <&dyn StarlarkArtifactLike<'v>>::unpack_value(executable)?
    {
        return Ok(bazel_location_target_for_artifact(
            artifact,
            short_paths,
            heap,
        )?);
    }

    let mut exec_paths = Vec::new();
    let mut rlocation_paths = Vec::new();
    for output in dep.default_output_values()? {
        let Some(artifact) = <&dyn StarlarkArtifactLike<'v>>::unpack_value(output)? else {
            continue;
        };
        let target = bazel_location_target_for_artifact(artifact, short_paths, heap)?;
        exec_paths.extend(target.exec_paths);
        rlocation_paths.extend(target.rlocation_paths);
    }
    Ok(BazelLocationTarget {
        exec_paths,
        rlocation_paths,
    })
}

fn bazel_collect_location_targets<'v>(
    ctx: &AnalysisContext<'v>,
    value: Value<'v>,
    short_paths: bool,
    prefer_executable: bool,
    heap: Heap<'v>,
    targets: &mut SmallMap<String, BazelLocationTarget>,
) -> starlark::Result<()> {
    if value.is_none() {
        return Ok(());
    }
    if let Some(dep) = Dependency::from_value(value) {
        let target = bazel_location_target_for_dep(dep, short_paths, prefer_executable, heap)?;
        for key in
            bazel_location_label_keys_for_target(ctx, dep.label().inner().target().unconfigured())
        {
            if !targets.contains_key(&key) {
                targets.insert(key, target.clone());
            }
        }
        return Ok(());
    }
    if let Some(artifact) = <&dyn StarlarkArtifactLike<'v>>::unpack_value(value)? {
        let target = bazel_location_target_for_artifact(artifact, short_paths, heap)?;
        if let Some(owner) = artifact.source_owner()? {
            for key in bazel_location_label_keys_for_target(ctx, owner.target()) {
                if !targets.contains_key(&key) {
                    targets.insert(key, target.clone());
                }
            }
        } else if let Some(owner) = artifact.owner()? {
            if let Some(owner) = owner.configured_label() {
                for key in bazel_location_label_keys_for_target(ctx, owner.unconfigured()) {
                    if !targets.contains_key(&key) {
                        targets.insert(key, target.clone());
                    }
                }
            }
        }
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        for item in list.iter() {
            bazel_collect_location_targets(
                ctx,
                item,
                short_paths,
                prefer_executable,
                heap,
                targets,
            )?;
        }
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        for item in tuple.iter() {
            bazel_collect_location_targets(
                ctx,
                item,
                short_paths,
                prefer_executable,
                heap,
                targets,
            )?;
        }
        return Ok(());
    }
    if let Some(dict) = DictRef::from_value(value) {
        for (key, value) in dict.iter() {
            bazel_collect_location_targets(
                ctx,
                key,
                short_paths,
                prefer_executable,
                heap,
                targets,
            )?;
            bazel_collect_location_targets(
                ctx,
                value,
                short_paths,
                prefer_executable,
                heap,
                targets,
            )?;
        }
    }
    Ok(())
}

fn bazel_collect_location_targets_from_attrs<'v>(
    ctx: &AnalysisContext<'v>,
    short_paths: bool,
    heap: Heap<'v>,
    targets: &mut SmallMap<String, BazelLocationTarget>,
) -> starlark::Result<()> {
    let Some(attrs) = ctx.attrs.borrow().as_ref().copied() else {
        return Ok(());
    };
    let Some(attrs) = StructRef::from_value(attrs.get()) else {
        return Ok(());
    };
    for (name, value) in attrs.iter() {
        let prefer_executable = matches!(
            name.as_str(),
            "deps" | "implementation_deps" | "data" | "tools" | "exec_tools" | "toolchains"
        );
        bazel_collect_location_targets(ctx, value, short_paths, prefer_executable, heap, targets)?;
    }
    Ok(())
}

fn bazel_expand_location_macro(
    body: &str,
    targets: &SmallMap<String, BazelLocationTarget>,
) -> starlark::Result<Option<String>> {
    let body = body.trim();
    let mut parts = body.splitn(2, char::is_whitespace);
    let Some(function) = parts.next() else {
        return Ok(None);
    };
    let (plural, use_rlocation) = match function {
        "location" | "execpath" | "rootpath" => (false, false),
        "locations" | "execpaths" | "rootpaths" => (true, false),
        "rlocationpath" => (false, true),
        "rlocationpaths" => (true, true),
        _ => return Ok(None),
    };
    let label = parts.next().map(str::trim).unwrap_or("");
    if label.is_empty() {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "`$({function})` requires a label"
        )
        .into());
    }
    let Some(target) = targets.get(label) else {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "label `{label}` in `$({function})` was not listed in ctx.expand_location targets"
        )
        .into());
    };
    let paths = if use_rlocation {
        &target.rlocation_paths
    } else {
        &target.exec_paths
    };
    if plural {
        return Ok(Some(paths.join(" ")));
    }
    match paths.as_slice() {
        [path] => Ok(Some(path.clone())),
        _ => Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "`$({function} {label})` expected exactly one file, got {}",
            paths.len()
        )
        .into()),
    }
}

fn bazel_expand_location(
    input: &str,
    targets: &SmallMap<String, BazelLocationTarget>,
) -> starlark::Result<String> {
    let mut result = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative_start) = input[cursor..].find("$(") {
        let start = cursor + relative_start;
        result.push_str(&input[cursor..start]);
        let body_start = start + 2;
        let Some(relative_end) = input[body_start..].find(')') else {
            result.push_str(&input[start..]);
            return Ok(result);
        };
        let end = body_start + relative_end;
        let body = &input[body_start..end];
        if let Some(expanded) = bazel_expand_location_macro(body, targets)? {
            result.push_str(&expanded);
        } else {
            result.push_str(&input[start..=end]);
        }
        cursor = end + 1;
    }
    result.push_str(&input[cursor..]);
    Ok(result)
}

fn bazel_target_label_from_label_value(value: Value<'_>) -> Option<&TargetLabel> {
    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
        return Some(label.label().target());
    }
    if let Some(label) = StarlarkConfiguredProvidersLabel::from_value(value) {
        return Some(label.label().target().unconfigured());
    }
    if let Some(label) = StarlarkTargetLabel::from_value(value) {
        return Some(label.label());
    }
    None
}

fn bazel_file_values_from_value<'v>(value: Value<'v>) -> starlark::Result<Vec<Value<'v>>> {
    if let Some(list) = ListRef::from_value(value) {
        return Ok(list.iter().collect());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return Ok(tuple.iter().collect());
    }
    if BazelDepset::from_value(value).is_some() {
        return bazel_depset_to_list(value);
    }
    Ok(vec![value])
}

fn bazel_location_target_for_file_values<'v>(
    files: Vec<Value<'v>>,
    short_paths: bool,
    heap: Heap<'v>,
) -> starlark::Result<BazelLocationTarget> {
    let mut exec_paths = Vec::new();
    let mut rlocation_paths = Vec::new();
    for file in files {
        let Some(artifact) = <&dyn StarlarkArtifactLike<'v>>::unpack_value(file)? else {
            continue;
        };
        let target = bazel_location_target_for_artifact(artifact, short_paths, heap)?;
        exec_paths.extend(target.exec_paths);
        rlocation_paths.extend(target.rlocation_paths);
    }
    Ok(BazelLocationTarget {
        exec_paths,
        rlocation_paths,
    })
}

fn bazel_collect_location_targets_from_label_dict<'v>(
    ctx: &AnalysisContext<'v>,
    label_dict: DictRef<'v>,
    short_paths: bool,
    heap: Heap<'v>,
    targets: &mut SmallMap<String, BazelLocationTarget>,
) -> starlark::Result<()> {
    for (label, files) in label_dict.iter() {
        let Some(label) = bazel_target_label_from_label_value(label) else {
            continue;
        };
        let target = bazel_location_target_for_file_values(
            bazel_file_values_from_value(files)?,
            short_paths,
            heap,
        )?;
        for key in bazel_location_label_keys_for_target(ctx, label) {
            if !targets.contains_key(&key) {
                targets.insert(key, target.clone());
            }
        }
    }
    Ok(())
}

fn bazel_collect_resolved_command_inputs<'v>(
    value: Value<'v>,
    inputs: &mut Vec<Value<'v>>,
) -> starlark::Result<()> {
    if value.is_none() {
        return Ok(());
    }
    if let Some(dep) = Dependency::from_value(value) {
        if let Some(executable) = dep.files_to_run_executable()? {
            inputs.push(executable);
        }
        inputs.extend(dep.default_output_values()?);
        return Ok(());
    }
    if <&dyn StarlarkArtifactLike<'v>>::unpack_value(value)?.is_some() {
        inputs.push(value);
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        for item in list.iter() {
            bazel_collect_resolved_command_inputs(item, inputs)?;
        }
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        for item in tuple.iter() {
            bazel_collect_resolved_command_inputs(item, inputs)?;
        }
        return Ok(());
    }
    if BazelDepset::from_value(value).is_some() {
        for item in bazel_depset_to_list(value)? {
            bazel_collect_resolved_command_inputs(item, inputs)?;
        }
        return Ok(());
    }
    if let Some(dict) = DictRef::from_value(value) {
        for (key, value) in dict.iter() {
            bazel_collect_resolved_command_inputs(key, inputs)?;
            bazel_collect_resolved_command_inputs(value, inputs)?;
        }
    }
    Ok(())
}

fn analysis_context_workspace_status_file<'v>(
    this: &AnalysisContext<'v>,
    kind: WorkspaceStatusKind,
    heap: Heap<'v>,
) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
    let slot = match kind {
        WorkspaceStatusKind::Stable => &this.bazel_info_file,
        WorkspaceStatusKind::Volatile => &this.bazel_version_file,
    };
    if let Some(value) = slot.borrow().as_ref().copied() {
        return Ok(value);
    }

    let mut state = this.actions.state()?;
    let declared = state.declare_output(
        None,
        kind.output_path(),
        OutputType::File,
        None,
        BuckOutPathKind::Configuration,
        heap,
    )?;
    let artifact = heap.alloc_typed(StarlarkDeclaredArtifact::new(
        None,
        declared,
        AssociatedArtifacts::new(),
    ));
    let outputs = BuckIndexSet::from_iter([artifact.output_artifact()]);
    state.register_action(
        outputs,
        UnregisteredWorkspaceStatusAction::new(kind),
        None,
        None,
    )?;
    *slot.borrow_mut() = Some(artifact);
    Ok(artifact)
}

const BAZEL_DEFAULT_MAKE_VARIABLE_ATTRIBUTES: &[&str] = &[
    "toolchains",
    ":cc_toolchain",
    "$toolchains",
    "$cc_toolchain",
];

fn make_variable_expansion_error(message: impl Into<String>) -> buck2_error::Error {
    buck2_error::Error::from(AnalysisContextError::MakeVariableExpansion(message.into()))
}

fn bazel_target_cpu(this: &AnalysisContext<'_>) -> Option<String> {
    let label = this.label?;
    let data = label.label().target().cfg().data().ok()?;
    data.constraints.iter().find_map(|(key, value)| {
        let key = key.to_string();
        if key.ends_with("//cpu:cpu") || key.contains("//cpu:cpu ") {
            let value = value.to_string();
            Some(
                value
                    .rsplit(':')
                    .next()
                    .unwrap_or(value.as_str())
                    .to_owned(),
            )
        } else {
            None
        }
    })
}

fn bazel_global_make_variables(this: &AnalysisContext<'_>) -> Vec<(String, String)> {
    let bin_dir = this.label.map_or_else(
        || "buck-out/bin".to_owned(),
        |label| bazel_output_root_for_configured_label("buck-out/bin", label.label().target()),
    );
    let gen_dir = this.label.map_or_else(
        || "buck-out/genfiles".to_owned(),
        |label| bazel_output_root_for_configured_label("buck-out/genfiles", label.label().target()),
    );
    vec![
        (
            "TARGET_CPU".to_owned(),
            bazel_target_cpu(this).unwrap_or_else(|| {
                if cfg!(target_arch = "aarch64") {
                    "arm64".to_owned()
                } else {
                    "k8".to_owned()
                }
            }),
        ),
        ("COMPILATION_MODE".to_owned(), "fastbuild".to_owned()),
        ("BINDIR".to_owned(), bin_dir),
        ("GENDIR".to_owned(), gen_dir),
    ]
}

fn collect_template_variables_from_info(
    info: &FrozenTemplateVariableInfo,
    variables: &mut Vec<(String, String)>,
) -> buck2_error::Result<()> {
    let dict = FrozenDictRef::from_frozen_value(info.variables_raw()).ok_or_else(|| {
        buck2_error::internal_error!("TemplateVariableInfo.variables is not a dict")
    })?;
    for (key, value) in dict.iter() {
        let key = key.to_value().unpack_str().ok_or_else(|| {
            buck2_error::internal_error!("TemplateVariableInfo variable key is not a string")
        })?;
        let value = value.to_value().unpack_str().ok_or_else(|| {
            buck2_error::internal_error!("TemplateVariableInfo variable value is not a string")
        })?;
        variables.push((key.to_owned(), value.to_owned()));
    }
    Ok(())
}

fn collect_template_variables_from_value<'v>(
    value: Value<'v>,
    variables: &mut Vec<(String, String)>,
) -> buck2_error::Result<()> {
    if value.is_none() {
        return Ok(());
    }
    if let Some(dep) = value.downcast_ref::<Dependency<'v>>() {
        if let Some(info) = dep.template_variable_info() {
            collect_template_variables_from_info(info.as_ref(), variables)?;
        }
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        for item in list.iter() {
            collect_template_variables_from_value(item, variables)?;
        }
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        for item in tuple.iter() {
            collect_template_variables_from_value(item, variables)?;
        }
    }
    Ok(())
}

fn analysis_context_template_make_variables<'v>(
    this: &AnalysisContext<'v>,
) -> buck2_error::Result<Vec<(String, String)>> {
    let mut variables = Vec::new();
    let Some(attrs) = this.attrs.borrow().as_ref().copied() else {
        return Ok(variables);
    };
    for attr in BAZEL_DEFAULT_MAKE_VARIABLE_ATTRIBUTES {
        if let Some(value) = struct_field(attrs, attr) {
            collect_template_variables_from_value(value, &mut variables)?;
        }
    }
    Ok(variables)
}

fn analysis_context_make_variable_entries<'v>(
    this: &AnalysisContext<'v>,
) -> buck2_error::Result<Vec<(String, String)>> {
    let mut variables = bazel_global_make_variables(this);
    variables.extend(analysis_context_template_make_variables(this)?);
    Ok(variables)
}

fn is_java_identifier_part(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}

fn scan_bazel_make_variable(chars: &[char], offset: &mut usize) -> buck2_error::Result<String> {
    let c = chars[*offset];
    match c {
        '(' => {
            *offset += 1;
            let start = *offset;
            while *offset < chars.len() && chars[*offset] != ')' {
                *offset += 1;
            }
            if *offset >= chars.len() {
                return Err(make_variable_expansion_error(
                    "unterminated variable reference",
                ));
            }
            let variable = chars[start..*offset].iter().collect();
            *offset += 1;
            Ok(variable)
        }
        '{' => {
            *offset += 1;
            let start = *offset;
            while *offset < chars.len() && chars[*offset] != '}' {
                *offset += 1;
            }
            if *offset >= chars.len() {
                return Err(make_variable_expansion_error(
                    "unterminated variable reference",
                ));
            }
            let expression: String = chars[start..*offset].iter().collect();
            Err(make_variable_expansion_error(format!(
                "'${{{expression}}}' syntax is not supported; use '$({expression})' instead for \
                 \"Make\" variables, or escape the '$' as '$$' if you intended this for the shell"
            )))
        }
        '@' | '<' | '^' => {
            *offset += 1;
            Ok(c.to_string())
        }
        _ => {
            let start = *offset;
            while *offset + 1 < chars.len() && is_java_identifier_part(chars[*offset + 1]) {
                *offset += 1;
            }
            let expression: String = chars[start..=*offset].iter().collect();
            *offset += 1;
            Err(make_variable_expansion_error(format!(
                "'${expression}' syntax is not supported; use '$({expression})' instead for \
                 \"Make\" variables, or escape the '$' as '$$' if you intended this for the shell"
            )))
        }
    }
}

fn expand_bazel_make_variables_with_lookup<F>(
    expression: &str,
    lookup: &F,
    depth: usize,
) -> buck2_error::Result<String>
where
    F: Fn(&str) -> Option<String>,
{
    if !expression.contains('$') {
        return Ok(expression.to_owned());
    }
    if depth > 10 {
        return Err(make_variable_expansion_error(format!(
            "potentially unbounded recursion during expansion of '{expression}'"
        )));
    }

    let chars = expression.chars().collect::<Vec<_>>();
    let mut result = String::new();
    let mut offset = 0;
    while offset < chars.len() {
        let c = chars[offset];
        if c != '$' {
            result.push(c);
            offset += 1;
            continue;
        }

        offset += 1;
        if offset >= chars.len() {
            return Err(make_variable_expansion_error("unterminated $"));
        }
        if chars[offset] == '$' {
            result.push('$');
            offset += 1;
            continue;
        }

        let variable = scan_bazel_make_variable(&chars, &mut offset)?;
        let Some(mut value) = lookup(&variable) else {
            let name = variable
                .split_once(' ')
                .map_or(variable.as_str(), |(name, _)| name);
            return Err(make_variable_expansion_error(format!(
                "$({name}) not defined"
            )));
        };
        if value != variable {
            value = expand_bazel_make_variables_with_lookup(&value, lookup, depth + 1)?;
        }
        result.push_str(&value);
    }
    Ok(result)
}

fn expand_bazel_make_variables<'v>(
    command: &str,
    additional_substitutions: &UnpackDictEntries<&'v str, &'v str>,
    variables: &[(String, String)],
) -> buck2_error::Result<String> {
    expand_bazel_make_variables_with_lookup(
        command,
        &|name| {
            additional_substitutions
                .entries
                .iter()
                .find_map(|(key, value)| (*key == name).then(|| (*value).to_owned()))
                .or_else(|| {
                    variables
                        .iter()
                        .rev()
                        .find_map(|(key, value)| (key == name).then(|| value.clone()))
                })
        },
        0,
    )
}

#[starlark_value(type = "AnalysisContext")]
impl<'v> StarlarkValue<'v> for AnalysisContext<'v> {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(analysis_context_methods)
    }
}

impl<'v> AllocValue<'v> for AnalysisContext<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex_no_freeze(self)
    }
}

pub fn bazel_analysis_context_declare_file<'v>(
    ctx: Value<'v>,
    name: &str,
    heap: Heap<'v>,
) -> starlark::Result<Value<'v>> {
    let ctx = ctx.downcast_ref::<AnalysisContext>().ok_or_else(|| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "expected AnalysisContext, got `{}`",
            ctx.to_string_for_type_error()
        )
    })?;
    let mut state = ctx.actions.state()?;
    let declared = state.declare_output_with_bazel_owner_and_output_root(
        None,
        name,
        OutputType::File,
        None,
        BuckOutPathKind::Configuration,
        ctx.actions.bazel_owner(),
        ctx.actions.bazel_output_root,
        heap,
    )?;
    Ok(heap
        .alloc_typed(StarlarkDeclaredArtifact::new(
            None,
            declared,
            AssociatedArtifacts::new(),
        ))
        .to_value())
}

struct RefAnalysisContext<'v>(&'v AnalysisContext<'v>);

impl<'v> StarlarkTypeRepr for RefAnalysisContext<'v> {
    type Canonical = <AnalysisContext<'v> as StarlarkTypeRepr>::Canonical;

    fn starlark_type_repr() -> Ty {
        AnalysisContext::starlark_type_repr()
    }
}

impl<'v> UnpackValue<'v> for RefAnalysisContext<'v> {
    type Error = Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        let Some(analysis_context) = value.downcast_ref::<AnalysisContext>() else {
            return Ok(None);
        };
        Ok(Some(RefAnalysisContext(analysis_context)))
    }
}

/// The type used for defining rules, usually bound as `ctx`.
/// Usually the sole argument to the `impl` argument of the `rule` function.
///
/// ```python
/// def _impl_my_rule(ctx: AnalysisContext) -> ["provider"]:
///     return [DefaultInfo()]
/// my_rule = rule(impl = _impl_my_rule, attrs = {})
/// ```
#[starlark_module]
fn analysis_context_methods(builder: &mut MethodsBuilder) {
    /// Returns the attributes of the target as a Starlark struct with a field for each attribute, which varies per rule.
    /// As an example, given a rule with the `attrs` argument of `{"foo": attrs.string()}`, this field will be
    /// a `struct` containing a field `foo` of type string.
    #[starlark(attribute)]
    fn attrs<'v>(
        this: RefAnalysisContext<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        Ok(analysis_context_attrs(this.0)?)
    }

    /// Bazel spelling for the target attribute struct.
    #[starlark(attribute)]
    fn attr<'v>(
        this: RefAnalysisContext<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        Ok(analysis_context_attrs(this.0)?)
    }

    /// Bazel view of split-transition attributes.
    #[starlark(attribute)]
    fn split_attr<'v>(
        this: RefAnalysisContext<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        Ok(analysis_context_split_attrs(this.0)?)
    }

    /// Bazel rule metadata visible to aspect implementations.
    #[starlark(attribute)]
    fn rule<'v>(this: RefAnalysisContext<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(analysis_context_rule(this.0, heap)?)
    }

    /// Bazel single-file view of label attributes marked with `allow_single_file`.
    #[starlark(attribute)]
    fn file<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        Ok(analysis_context_bazel_file_structs(this.0, heap)?.file)
    }

    /// Bazel files-to-build view of label attributes.
    #[starlark(attribute)]
    fn files<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        Ok(analysis_context_bazel_file_structs(this.0, heap)?.files)
    }

    /// Bazel executable view of executable label attributes.
    #[starlark(attribute)]
    fn executable<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        Ok(analysis_context_bazel_file_structs(this.0, heap)?.executable)
    }

    /// The current target's Bazel-compatible build configuration view.
    #[starlark(attribute)]
    fn configuration<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        Ok(analysis_context_configuration(this.0, heap))
    }

    /// Bazel root object for generated binary outputs.
    #[starlark(attribute)]
    fn bin_dir<'v>(this: RefAnalysisContext<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(bazel_file_root_for_label(
            heap,
            "buck-out/bin",
            this.0.label,
        ))
    }

    /// Deprecated Bazel path to the BUILD file for this rule, relative to the repository root.
    #[starlark(attribute)]
    fn build_file_path<'v>(this: RefAnalysisContext<'v>) -> starlark::Result<String> {
        Ok(this
            .0
            .build_file_path
            .clone()
            .unwrap_or_else(|| bazel_build_file_path_from_label(this.0.label)))
    }

    /// Bazel root object for generated files.
    #[starlark(attribute)]
    fn genfiles_dir<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(bazel_file_root_for_label(
            heap,
            "buck-out/genfiles",
            this.0.label,
        ))
    }

    /// Bazel workspace/runfiles prefix.
    #[starlark(attribute)]
    fn workspace_name<'v>(this: RefAnalysisContext<'v>) -> starlark::Result<String> {
        Ok(bazel_workspace_name_for_label(this.0.label))
    }

    /// Enabled Bazel features for this rule.
    #[starlark(attribute)]
    fn features<'v>(this: RefAnalysisContext<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList::EMPTY))
    }

    /// Disabled Bazel features for this rule.
    #[starlark(attribute)]
    fn disabled_features<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList::EMPTY))
    }

    /// Bazel configuration fragments available to Starlark rules.
    #[starlark(attribute)]
    fn fragments<'v>(this: RefAnalysisContext<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(bazel_fragments(
            heap,
            this.0.label,
            this.0.bazel_cpp_options.clone(),
        ))
    }

    /// Bazel stable workspace status file.
    #[starlark(attribute)]
    fn info_file<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        analysis_context_workspace_status_file(this.0, WorkspaceStatusKind::Stable, heap)
    }

    /// Bazel volatile workspace status file.
    #[starlark(attribute)]
    fn version_file<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        analysis_context_workspace_status_file(this.0, WorkspaceStatusKind::Volatile, heap)
    }

    /// Returns whether coverage instrumentation should be generated for this rule.
    fn coverage_instrumented<'v>(
        this: RefAnalysisContext<'v>,
        #[starlark(default = NoneOr::None)] target: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        let _ = (this, target);
        Ok(false)
    }

    /// Splits a shell command into a list of tokens.
    fn tokenize<'v>(
        this: RefAnalysisContext<'v>,
        #[starlark(require = pos)] option: &str,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        let tokens = bazel_shell_tokenize(option)?;
        Ok(bazel_string_list(heap, &tokens))
    }

    /// Returns true if the given constraint value is part of the current target platform.
    fn target_platform_has_constraint<'v>(
        this: RefAnalysisContext<'v>,
        #[starlark(require = pos)] constraint_value: ValueOf<'v, &'v ConstraintValueInfo<'v>>,
    ) -> starlark::Result<bool> {
        let Some(label) = this.0.label else {
            return Ok(false);
        };
        let cfg = label.label().target().cfg();
        if !cfg.is_bound() {
            return Ok(false);
        }
        let (constraint_key, expected_constraint_value) =
            constraint_value.typed.to_constraint_key_value();
        Ok(cfg
            .get_constraint_value(&constraint_key)?
            .is_some_and(|actual_constraint_value| {
                actual_constraint_value == &expected_constraint_value
            }))
    }

    /// Returns a Bazel runfiles object.
    fn runfiles<'v>(
        this: RefAnalysisContext<'v>,
        #[starlark(default = NoneOr::None)] files: NoneOr<UnpackListOrTuple<Value<'v>>>,
        #[starlark(require = named, default = NoneOr::None)] transitive_files: NoneOr<Value<'v>>,
        #[starlark(require = named, default = false)] collect_data: bool,
        #[starlark(require = named, default = false)] collect_default: bool,
        #[starlark(require = named, default = NoneOr::None)] symlinks: NoneOr<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] root_symlinks: NoneOr<Value<'v>>,
        #[starlark(require = named, default = false)] skip_conflict_checking: bool,
        heap: Heap<'v>,
    ) -> starlark::Result<BazelRunfiles<'v>> {
        let _ = skip_conflict_checking;
        let explicit = bazel_runfiles_from_files(
            heap,
            files.into_option().unwrap_or_default().items,
            transitive_files.into_option(),
            symlinks.into_option(),
            root_symlinks.into_option(),
        )?;
        if !collect_data && !collect_default {
            return Ok(explicit);
        }

        let mut collected = Vec::new();
        bazel_collect_runfiles_from_attrs(
            this.0.attrs.borrow().as_ref().copied(),
            collect_data,
            collect_default,
            &mut collected,
        )?;
        if collected.is_empty() {
            return Ok(explicit);
        }

        let mut runfiles = Vec::with_capacity(collected.len() + 1);
        runfiles.push(&explicit);
        runfiles.extend(collected);
        bazel_runfiles_from_runfiles(heap, runfiles)
    }

    /// Returns the Bazel predeclared output artifacts for this rule.
    #[starlark(attribute)]
    fn outputs<'v>(
        this: RefAnalysisContext<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        Ok(analysis_context_outputs(this.0)?)
    }

    /// Returns an `actions` value containing functions to define actual actions that are run.
    /// See the `actions` type for the operations that are available.
    #[starlark(attribute)]
    fn actions<'v>(
        this: RefAnalysisContext<'v>,
    ) -> starlark::Result<ValueTyped<'v, AnalysisActions<'v>>> {
        Ok(this.0.actions)
    }

    /// Returns a `label` representing the target, or `None` if being invoked from a
    /// `dynamic_output` in Bxl.
    #[starlark(attribute)]
    fn label<'v>(
        this: RefAnalysisContext<'v>,
    ) -> starlark::Result<NoneOr<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>> {
        Ok(NoneOr::from_option(this.0.label))
    }

    /// An opaque value that can be indexed with a plugin kind to get a list of the available plugin
    /// deps of that kind. The rule must set an appropriate value on `uses_plugins` in its
    /// declaration.
    #[starlark(attribute)]
    fn plugins<'v>(
        this: RefAnalysisContext<'v>,
    ) -> starlark::Result<ValueTypedComplex<'v, AnalysisPlugins<'v>>> {
        Ok(this.0.plugins.ok_or_else(|| {
            internal_error!("`plugins` is not available for `dynamic_output` or BXL")
        })?)
    }

    /// Returns the Bazel toolchain context for this rule.
    #[starlark(attribute)]
    fn toolchains<'v>(
        this: RefAnalysisContext<'v>,
    ) -> starlark::Result<ValueTyped<'v, AnalysisToolchains<'v>>> {
        Ok(this.0.toolchains)
    }

    /// Returns the configured value of this Bazel build-setting target.
    #[starlark(attribute)]
    fn build_setting_value<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let Some(label) = this.0.label else {
            return Err(
                internal_error!("`build_setting_value` is not available without a label").into(),
            );
        };
        if !this.0.is_bazel_build_setting {
            return Err(
                buck2_error::Error::from(AnalysisContextError::NonBuildSetting(label.to_string()))
                    .into(),
            );
        }

        let target = label.label().target();
        let build_setting_key = target.unconfigured().to_string();
        if let Some(value) = target.cfg().data()?.build_settings.get(&build_setting_key) {
            return Ok(bazel_build_setting_value_to_starlark(value, heap));
        }

        let attrs = this.0.attrs.borrow().as_ref().copied().ok_or_else(|| {
            internal_error!("`build_setting_value` is not available without attrs")
        })?;
        struct_field(attrs, "build_setting_default").ok_or_else(|| {
            internal_error!(
                "Bazel build setting `{}` has no `build_setting_default` attr",
                target.unconfigured()
            )
            .into()
        })
    }

    /// Expands Bazel "Make" variable references in a string.
    fn expand_make_variables<'v>(
        this: RefAnalysisContext<'v>,
        #[starlark(require = pos)] attribute_name: &str,
        #[starlark(require = pos)] command: &str,
        #[starlark(require = pos)] additional_substitutions: UnpackDictEntries<&'v str, &'v str>,
    ) -> starlark::Result<String> {
        let _ = attribute_name;
        let variables = analysis_context_make_variable_entries(this.0)?;
        Ok(expand_bazel_make_variables(
            command,
            &additional_substitutions,
            &variables,
        )?)
    }

    /// Expands Bazel location macro references in a string.
    fn expand_location<'v>(
        this: RefAnalysisContext<'v>,
        #[starlark(require = pos)] input: &str,
        #[starlark(require = pos, default = UnpackListOrTuple::default())]
        targets: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = false)] short_paths: bool,
        heap: Heap<'v>,
    ) -> starlark::Result<String> {
        let mut target_map = SmallMap::new();
        bazel_collect_location_targets_from_attrs(this.0, short_paths, heap, &mut target_map)?;
        for target in targets.items {
            bazel_collect_location_targets(
                this.0,
                target,
                short_paths,
                false,
                heap,
                &mut target_map,
            )?;
        }
        bazel_expand_location(input, &target_map)
    }

    /// Resolves a Bazel shell command into action inputs and argv.
    fn resolve_command<'v>(
        this: RefAnalysisContext<'v>,
        #[starlark(require = named, default = "")] command: &str,
        #[starlark(require = named, default = NoneOr::None)] attribute: NoneOr<&str>,
        #[starlark(require = named, default = false)] expand_locations: bool,
        #[starlark(require = named, default = NoneOr::None)] make_variables: NoneOr<
            UnpackDictEntries<&'v str, Value<'v>>,
        >,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        tools: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] label_dict: NoneOr<DictRef<'v>>,
        #[starlark(require = named, default = NoneOr::None)] execution_requirements: NoneOr<
            DictRef<'v>,
        >,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = (attribute, execution_requirements);
        let mut command = command.to_owned();

        if expand_locations {
            let mut target_map = SmallMap::new();
            bazel_collect_location_targets_from_attrs(this.0, false, heap, &mut target_map)?;
            if let Some(label_dict) = label_dict.into_option() {
                bazel_collect_location_targets_from_label_dict(
                    this.0,
                    label_dict,
                    false,
                    heap,
                    &mut target_map,
                )?;
            }
            command = bazel_expand_location(&command, &target_map)?;
        }

        if let Some(make_variables) = make_variables.into_option() {
            let substitutions = make_variables
                .entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_str()))
                .collect::<Vec<_>>();
            let variables = analysis_context_make_variable_entries(this.0)?;
            command = expand_bazel_make_variables_with_lookup(
                &command,
                &|name| {
                    substitutions
                        .iter()
                        .find_map(|(key, value)| (key == name).then(|| value.clone()))
                        .or_else(|| {
                            variables
                                .iter()
                                .rev()
                                .find_map(|(key, value)| (key == name).then(|| value.clone()))
                        })
                },
                0,
            )?;
        }

        let mut inputs = Vec::new();
        for tool in tools.items {
            bazel_collect_resolved_command_inputs(tool, &mut inputs)?;
        }

        let argv = [
            heap.alloc_str("/bin/bash").to_value(),
            heap.alloc_str("-c").to_value(),
            heap.alloc_str(&command).to_value(),
        ];
        Ok(heap.alloc(AllocTuple([
            heap.alloc(AllocList(inputs)).to_value(),
            heap.alloc(AllocList(argv)).to_value(),
            heap.alloc(AllocList::EMPTY).to_value(),
        ])))
    }

    /// Bazel make-variable map for this rule.
    #[starlark(attribute)]
    fn var<'v>(this: RefAnalysisContext<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let mut variables = SmallMap::new();
        for (key, value) in analysis_context_make_variable_entries(this.0)? {
            variables.insert(key, value);
        }
        Ok(heap.alloc(AllocDict(variables)))
    }
}

#[starlark_module]
#[starlark_types(
    AnalysisContext<'_> as AnalysisContext,
    AnalysisActions<'_> as AnalysisActions,
    AnalysisToolchains<'_> as AnalysisToolchains
)]
pub(crate) fn register_analysis_context(builder: &mut GlobalsBuilder) {}

pub static ANALYSIS_ACTIONS_METHODS_ACTIONS: LateBinding<fn(&mut MethodsBuilder)> =
    LateBinding::new("ANALYSIS_ACTIONS_METHODS_ACTIONS");
pub static ANALYSIS_ACTIONS_METHODS_ANON_TARGET: LateBinding<fn(&mut MethodsBuilder)> =
    LateBinding::new("ANALYSIS_ACTIONS_METHODS_ANON_TARGET");

pub(crate) fn init_analysis_context_ty() {
    AnalysisContextReprLate::init(AnalysisContext::starlark_type_repr());
}
