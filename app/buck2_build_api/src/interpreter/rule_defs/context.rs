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
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersName;
use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
use buck2_error::BuckErrorContext;
use buck2_error::conversion::from_any_with_tag;
use buck2_error::internal_error;
use buck2_execute::digest_config::DigestConfig;
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
use starlark::typing::Ty;
use starlark::values::AllocValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueTyped;
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::structs::StructRef;
use starlark::values::type_repr::StarlarkTypeRepr;

use crate::analysis::anon_promises_dyn::RunAnonPromisesAccessor;
use crate::analysis::registry::AnalysisRegistry;
use crate::deferred::calculation::GET_PROMISED_ARTIFACT;
use crate::interpreter::rule_defs::plugins::AnalysisPlugins;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum AnalysisContextError {
    #[error("attempting to access `build_setting_value` of non-build setting {0}")]
    NonBuildSetting(String),
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
        key.strip_prefix('@').unwrap_or(key).to_owned()
    }

    fn key_from_value(value: Value<'_>) -> String {
        if let Some(label) = StarlarkProvidersLabel::from_value(value) {
            return label.to_string();
        }
        if let Some(label) = StarlarkTargetLabel::from_value(value) {
            return label.to_string();
        }
        if let Some(key) = value.unpack_str() {
            return Self::normalize_key(key);
        }
        value.to_repr()
    }

    fn contains_value(&self, value: Value<'_>) -> bool {
        let key = Self::key_from_value(value);
        self.toolchains.iter().any(|candidate| candidate == &key)
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
        if self.toolchains.iter().any(|candidate| candidate == &key) {
            Ok(self
                .resolved
                .get(&key)
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
