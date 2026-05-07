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
use buck2_core::configuration::data::BazelBuildSettingValue;
use buck2_core::fs::buck_out_path::BuckOutPathKind;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersName;
use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
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
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::structs::StructRef;
use starlark::values::type_repr::StarlarkTypeRepr;

use crate::actions::impls::workspace_status::UnregisteredWorkspaceStatusAction;
use crate::actions::impls::workspace_status::WorkspaceStatusKind;
use crate::analysis::anon_promises_dyn::RunAnonPromisesAccessor;
use crate::analysis::registry::AnalysisRegistry;
use crate::deferred::calculation::GET_PROMISED_ARTIFACT;
use crate::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use crate::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use crate::interpreter::rule_defs::plugins::AnalysisPlugins;
use crate::interpreter::rule_defs::provider::builtin::constraint_value_info::ConstraintValueInfo;
use crate::interpreter::rule_defs::provider::builtin::default_info::BazelRunfiles;
use crate::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_from_files;
use crate::interpreter::rule_defs::provider::dependency::Dependency;
use buck2_hash::BuckIndexSet;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum AnalysisContextError {
    #[error("attempting to access `build_setting_value` of non-build setting {0}")]
    NonBuildSetting(String),
    #[error("ctx.runfiles argument `{0}` is not supported yet")]
    UnsupportedRunfilesArgument(&'static str),
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
    /// Copies from the ctx, so we can capture them for `dynamic`.
    pub attributes: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
    pub plugins: Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>,
    /// Digest configuration to use when interpreting digests passed in analysis.
    pub digest_config: DigestConfig,
}

#[derive(ProvidesStaticType, Debug, Trace, NoSerialize, Allocative)]
pub struct AnalysisToolchains<'v> {
    toolchains: Vec<String>,
    resolved: SmallMap<String, Value<'v>>,
}

impl Display for AnalysisToolchains<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "<ctx.toolchains>")
    }
}

impl<'v> AnalysisToolchains<'v> {
    fn new(toolchains: Vec<String>, resolved: SmallMap<String, Value<'v>>) -> Self {
        Self {
            toolchains,
            resolved,
        }
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
}

#[starlark_value(type = "AnalysisActions", StarlarkTypeRepr, UnpackValue)]
impl<'v> StarlarkValue<'v> for AnalysisActions<'v> {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(|builder| {
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

#[derive(ProvidesStaticType, Debug, Trace, NoSerialize, Allocative)]
pub struct AnalysisContext<'v> {
    attrs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
    outputs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
    pub actions: ValueTyped<'v, AnalysisActions<'v>>,
    /// Only `None` when running a `dynamic_output` action from Bxl.
    label: Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>,
    plugins: Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>,
    toolchains: ValueTyped<'v, AnalysisToolchains<'v>>,
    is_bazel_build_setting: bool,
    bazel_info_file: RefCell<Option<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>>,
    bazel_version_file: RefCell<Option<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>>,
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
        outputs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
        label: Option<ValueTyped<'v, StarlarkConfiguredProvidersLabel>>,
        plugins: Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>,
        toolchains: Vec<String>,
        resolved_toolchains: SmallMap<String, Value<'v>>,
        is_bazel_build_setting: bool,
        registry: AnalysisRegistry<'v>,
        digest_config: DigestConfig,
    ) -> Self {
        Self {
            attrs,
            outputs,
            actions: heap.alloc_typed(AnalysisActions {
                state: RefCell::new(Some(registry)),
                attributes: attrs,
                plugins,
                digest_config,
            }),
            label,
            plugins,
            toolchains: heap.alloc_typed(AnalysisToolchains::new(toolchains, resolved_toolchains)),
            is_bazel_build_setting,
            bazel_info_file: RefCell::new(None),
            bazel_version_file: RefCell::new(None),
        }
    }

    pub fn prepare(
        heap: Heap<'v>,
        attrs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
        outputs: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
        label: Option<ConfiguredTargetLabel>,
        plugins: Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>,
        toolchains: Vec<String>,
        resolved_toolchains: SmallMap<String, Value<'v>>,
        is_bazel_build_setting: bool,
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
            outputs,
            label,
            plugins,
            toolchains,
            resolved_toolchains,
            is_bazel_build_setting,
            registry,
            digest_config,
        );
        heap.alloc_typed(analysis_context)
    }

    pub fn assert_no_promises(&self) -> buck2_error::Result<()> {
        self.actions.state()?.assert_no_promises()
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
        .ok_or_else(|| internal_error!("`attrs` is not available for `dynamic_output` or BXL"))
}

fn analysis_context_outputs<'v>(
    ctx: &AnalysisContext<'v>,
) -> buck2_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    ctx.outputs
        .ok_or_else(|| internal_error!("`outputs` is not available for `dynamic_output` or BXL"))
}

fn bazel_file_root<'v>(heap: Heap<'v>, path: &str) -> Value<'v> {
    heap.alloc(AllocStruct([("path", heap.alloc_str(path).to_value())]))
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
struct BazelCppConfiguration;

impl fmt::Display for BazelCppConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<cpp fragment>")
    }
}

starlark::starlark_simple_value!(BazelCppConfiguration);

#[starlark_value(type = "cpp")]
impl<'v> StarlarkValue<'v> for BazelCppConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cpp_configuration_methods)
    }
}

fn bazel_empty_list<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocList::EMPTY)
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
        let _ = this;
        Ok(bazel_empty_list(heap))
    }

    #[starlark(attribute)]
    fn copts<'v>(
        #[starlark(this)] this: &BazelCppConfiguration,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_empty_list(heap))
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
        let _ = this;
        Ok(bazel_empty_list(heap))
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

fn bazel_fragments<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocStruct([("cpp", heap.alloc(BazelCppConfiguration))]))
}

fn analysis_context_configuration<'v>(
    ctx: &AnalysisContext<'v>,
    heap: Heap<'v>,
) -> ValueOfUnchecked<'v, StructRef<'static>> {
    let host_path_separator = if cfg!(windows) { ";" } else { ":" };
    let bin_dir = bazel_file_root(heap, "buck-out/bin");
    let genfiles_dir = bazel_file_root(heap, "buck-out/genfiles");
    let is_tool_configuration = ctx
        .label
        .is_some_and(|label| label.label().target().cfg().is_marked_as_exec_platform());
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

fn analysis_context_bazel_file_structs<'v>(
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
            let single_file = match files.as_slice() {
                [file] => *file,
                [] => Value::new_none(),
                _ => continue,
            };
            file_fields.push((name.clone(), single_file));
            executable_fields.push((name, single_file));
        }
    }
    Ok((
        ValueOfUnchecked::new(heap.alloc(AllocStruct(file_fields))),
        ValueOfUnchecked::new(heap.alloc(AllocStruct(files_fields))),
        ValueOfUnchecked::new(heap.alloc(AllocStruct(executable_fields))),
    ))
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

    /// Bazel single-file view of label attributes marked with `allow_single_file`.
    #[starlark(attribute)]
    fn file<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        let attrs = analysis_context_attrs(this.0)?;
        let (file, _, _) = analysis_context_bazel_file_structs(heap, attrs)?;
        Ok(file)
    }

    /// Bazel files-to-build view of label attributes.
    #[starlark(attribute)]
    fn files<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        let attrs = analysis_context_attrs(this.0)?;
        let (_, files, _) = analysis_context_bazel_file_structs(heap, attrs)?;
        Ok(files)
    }

    /// Bazel executable view of executable label attributes.
    #[starlark(attribute)]
    fn executable<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
        let attrs = analysis_context_attrs(this.0)?;
        let (_, _, executable) = analysis_context_bazel_file_structs(heap, attrs)?;
        Ok(executable)
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
        let _ = this;
        Ok(bazel_file_root(heap, "buck-out/bin"))
    }

    /// Bazel root object for generated files.
    #[starlark(attribute)]
    fn genfiles_dir<'v>(
        this: RefAnalysisContext<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_file_root(heap, "buck-out/genfiles"))
    }

    /// Bazel workspace/runfiles prefix. Under bzlmod, Bazel exposes this as `_main`.
    #[starlark(attribute)]
    fn workspace_name<'v>(this: RefAnalysisContext<'v>) -> starlark::Result<&'static str> {
        let _ = this;
        Ok("_main")
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
        let _ = this;
        Ok(bazel_fragments(heap))
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
        let _ctx = this;
        if collect_data {
            return Err(buck2_error::Error::from(
                AnalysisContextError::UnsupportedRunfilesArgument("collect_data"),
            )
            .into());
        }
        if collect_default {
            return Err(buck2_error::Error::from(
                AnalysisContextError::UnsupportedRunfilesArgument("collect_default"),
            )
            .into());
        }
        if symlinks.into_option().is_some() {
            return Err(buck2_error::Error::from(
                AnalysisContextError::UnsupportedRunfilesArgument("symlinks"),
            )
            .into());
        }
        if root_symlinks.into_option().is_some() {
            return Err(buck2_error::Error::from(
                AnalysisContextError::UnsupportedRunfilesArgument("root_symlinks"),
            )
            .into());
        }
        if skip_conflict_checking {
            return Err(buck2_error::Error::from(
                AnalysisContextError::UnsupportedRunfilesArgument("skip_conflict_checking"),
            )
            .into());
        }
        bazel_runfiles_from_files(
            heap,
            files.into_option().unwrap_or_default().items,
            transitive_files.into_option(),
        )
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

        let attrs = this.0.attrs.ok_or_else(|| {
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

    /// Bazel make-variable map for this rule.
    #[starlark(attribute)]
    fn var<'v>(this: RefAnalysisContext<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let _unused = this;
        Ok(heap.alloc(AllocDict::EMPTY))
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
