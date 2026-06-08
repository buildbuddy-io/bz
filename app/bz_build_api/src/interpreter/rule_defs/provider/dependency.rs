/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::fmt::Display;
use std::hash::Hash;
use std::mem;

use allocative::Allocative;
use bz_core::execution_types::execution::ExecutionPlatformResolution;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_core::provider::label::ProviderName;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use bz_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use dupe::Dupe;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::typing::Ty;
use starlark::values::Freeze;
use starlark::values::FrozenValue;
use starlark::values::FrozenValueTyped;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark_map::StarlarkHasher;

use crate::interpreter::rule_defs::provider::DefaultInfo;
use crate::interpreter::rule_defs::provider::FrozenDefaultInfo;
use crate::interpreter::rule_defs::provider::builtin::bazel::template_variable_info::FrozenTemplateVariableInfo;
use crate::interpreter::rule_defs::provider::builtin::default_info::BazelRunfiles;
use crate::interpreter::rule_defs::provider::builtin::default_info::bazel_files_to_run_executable;
use crate::interpreter::rule_defs::provider::collection::FrozenProviderCollection;
use crate::interpreter::rule_defs::provider::collection::ProviderCollection;
use crate::interpreter::rule_defs::provider::collection::empty_provider_collection_value;
use crate::interpreter::rule_defs::provider::execution_platform::StarlarkExecutionPlatformResolution;
use crate::interpreter::rule_defs::provider::ty::abstract_provider::AbstractProvider;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum DependencyError {
    #[error("Unknown subtarget, could not find `{0}`")]
    UnknownSubtarget(String),
}

/// Wraps a dependency's `ProvidersLabel` and the result of analysis together for users' rule implementation functions
///
/// From Starlark, the label is accessible with `.label`, and providers from the underlying
/// `ProviderCollection` are available via `[]` (`get()`)
#[derive(
    Debug,
    Trace,
    Coerce,
    Freeze,
    ProvidesStaticType,
    NoSerialize,
    Allocative
)]
#[repr(C)]
pub struct DependencyGen<V: ValueLifetimeless> {
    label: ValueOfUncheckedGeneric<V, StarlarkConfiguredProvidersLabel>,
    provider_collection: FrozenValueTyped<'static, FrozenProviderCollection>,
    extra_provider_collection: V,
    // This could be `Option<...>`, but that breaks `Coerce`.
    execution_platform: ValueOfUncheckedGeneric<V, NoneOr<StarlarkExecutionPlatformResolution>>,
}

starlark_complex_value!(pub Dependency);

impl<V: ValueLifetimeless> Display for DependencyGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<dependency ")?;
        Display::fmt(&self.label, f)?;
        write!(f, ">")
    }
}

impl<'v, V: ValueLike<'v>> DependencyGen<V> {
    pub fn label(&self) -> &'v StarlarkConfiguredProvidersLabel {
        StarlarkConfiguredProvidersLabel::from_value(self.label.get().to_value()).unwrap()
    }

    pub fn label_value(&self) -> Value<'v> {
        self.label.get().to_value()
    }

    pub fn configured_providers_label(&self) -> ConfiguredProvidersLabel {
        self.label().inner().dupe()
    }

    pub fn provider_collection_value(&self) -> Value<'v> {
        let extra = self.extra_provider_collection.to_value();
        if extra.is_none() {
            self.provider_collection.to_value()
        } else {
            extra
        }
    }
}

impl<'v> Dependency<'v> {
    pub fn new(
        heap: Heap<'v>,
        label: ConfiguredProvidersLabel,
        provider_collection: FrozenValueTyped<'v, FrozenProviderCollection>,
        execution_platform: Option<&ExecutionPlatformResolution>,
    ) -> Self {
        let execution_platform: ValueOfUnchecked<NoneOr<StarlarkExecutionPlatformResolution>> =
            match execution_platform {
                Some(e) => ValueOfUnchecked::new(
                    heap.alloc(StarlarkExecutionPlatformResolution(e.clone())),
                ),
                None => ValueOfUnchecked::new(Value::new_none()),
            };
        Dependency {
            label: heap.alloc_typed_unchecked(StarlarkConfiguredProvidersLabel::new(label)),
            provider_collection: unsafe {
                mem::transmute::<
                    FrozenValueTyped<'_, FrozenProviderCollection>,
                    FrozenValueTyped<'_, FrozenProviderCollection>,
                >(provider_collection)
            },
            extra_provider_collection: Value::new_none(),
            execution_platform,
        }
    }

    pub fn new_with_provider_collection(
        heap: Heap<'v>,
        label: ConfiguredProvidersLabel,
        base_provider_collection: FrozenValueTyped<'v, FrozenProviderCollection>,
        provider_collection: ProviderCollection<'v>,
        execution_platform: Option<&ExecutionPlatformResolution>,
    ) -> Self {
        let execution_platform: ValueOfUnchecked<NoneOr<StarlarkExecutionPlatformResolution>> =
            match execution_platform {
                Some(e) => ValueOfUnchecked::new(
                    heap.alloc(StarlarkExecutionPlatformResolution(e.clone())),
                ),
                None => ValueOfUnchecked::new(Value::new_none()),
            };
        Dependency {
            label: heap.alloc_typed_unchecked(StarlarkConfiguredProvidersLabel::new(label)),
            provider_collection: unsafe {
                mem::transmute::<
                    FrozenValueTyped<'_, FrozenProviderCollection>,
                    FrozenValueTyped<'_, FrozenProviderCollection>,
                >(base_provider_collection)
            },
            extra_provider_collection: heap.alloc(provider_collection),
            execution_platform,
        }
    }

    pub fn new_with_runtime_provider_collection(
        heap: Heap<'v>,
        label: ConfiguredProvidersLabel,
        provider_collection: ProviderCollection<'v>,
        execution_platform: Option<&ExecutionPlatformResolution>,
    ) -> Self {
        let execution_platform: ValueOfUnchecked<NoneOr<StarlarkExecutionPlatformResolution>> =
            match execution_platform {
                Some(e) => ValueOfUnchecked::new(
                    heap.alloc(StarlarkExecutionPlatformResolution(e.clone())),
                ),
                None => ValueOfUnchecked::new(Value::new_none()),
            };
        Dependency {
            label: heap.alloc_typed_unchecked(StarlarkConfiguredProvidersLabel::new(label)),
            provider_collection: empty_provider_collection_value(),
            extra_provider_collection: heap.alloc(provider_collection),
            execution_platform,
        }
    }

    pub fn base_provider_collection(&self) -> FrozenValueTyped<'v, FrozenProviderCollection> {
        unsafe {
            mem::transmute::<
                FrozenValueTyped<'_, FrozenProviderCollection>,
                FrozenValueTyped<'_, FrozenProviderCollection>,
            >(self.provider_collection)
        }
    }

    pub fn provider_collection_shallow_clone(&self) -> ProviderCollection<'v> {
        ProviderCollection::from_value(self.provider_collection_value())
            .expect("Dependency provider collection should be a provider collection")
            .shallow_clone()
    }

    fn map_default_info<T>(
        &self,
        f: impl FnOnce(&DefaultInfo<'v>) -> bz_error::Result<T>,
        frozen_f: impl FnOnce(&FrozenDefaultInfo) -> bz_error::Result<T>,
    ) -> bz_error::Result<T> {
        let collection = ProviderCollection::from_value(self.provider_collection_value())
            .expect("Dependency provider collection should be a provider collection");
        let default_info = collection.default_info_value()?;
        if let Some(default_info) = default_info.downcast_ref::<DefaultInfo<'v>>() {
            return f(default_info);
        }
        if let Some(default_info) = default_info
            .unpack_frozen()
            .and_then(|value| value.downcast_ref::<FrozenDefaultInfo>())
        {
            return frozen_f(default_info);
        }
        Err(internal_error!(
            "DefaultInfo provider should have the expected provider type"
        ))
    }

    pub fn execution_platform(&self) -> bz_error::Result<Option<&ExecutionPlatformResolution>> {
        let execution_platform: ValueOfUnchecked<NoneOr<&StarlarkExecutionPlatformResolution>> =
            self.execution_platform.cast();
        match execution_platform.unpack()? {
            NoneOr::None => Ok(None),
            NoneOr::Other(e) => Ok(Some(&e.0)),
        }
    }

    pub fn default_output_values(&self) -> bz_error::Result<Vec<Value<'v>>> {
        self.map_default_info(
            |info| info.default_output_values_for_dependency(),
            |info| info.default_output_values(),
        )
    }

    pub fn files_to_run_executable(&self) -> bz_error::Result<Option<Value<'v>>> {
        let files_to_run = self.map_default_info(
            |info| Ok(info.files_to_run_raw_for_dependency()),
            |info| Ok(info.files_to_run_raw().to_value()),
        )?;
        Ok(bazel_files_to_run_executable(files_to_run))
    }

    pub fn default_runfiles_value(&self) -> bz_error::Result<Value<'v>> {
        self.map_default_info(
            |info| Ok(info.default_runfiles_raw_for_dependency()),
            |info| Ok(info.default_runfiles_raw().to_value()),
        )
    }

    pub fn template_variable_info(
        &self,
    ) -> Option<FrozenValueTyped<'_, FrozenTemplateVariableInfo>> {
        self.provider_collection.builtin_provider()
    }

    pub fn data_runfiles(&'v self) -> bz_error::Result<&'v BazelRunfiles<'v>> {
        let value = self.map_default_info(
            |info| Ok(info.data_runfiles_raw_for_dependency()),
            |info| Ok(info.data_runfiles_raw().to_value()),
        )?;
        BazelRunfiles::from_value(value).ok_or_else(|| {
            bz_error::internal_error!("DefaultInfo.data_runfiles should be a runfiles object")
        })
    }

    pub fn default_runfiles(&'v self) -> bz_error::Result<&'v BazelRunfiles<'v>> {
        let value = self.map_default_info(
            |info| Ok(info.default_runfiles_raw_for_dependency()),
            |info| Ok(info.default_runfiles_raw().to_value()),
        )?;
        BazelRunfiles::from_value(value).ok_or_else(|| {
            bz_error::internal_error!("DefaultInfo.default_runfiles should be a runfiles object")
        })
    }
}

#[starlark_value(type = "Dependency")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for DependencyGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_type_starlark_repr() -> Ty {
        Ty::starlark_value::<DependencyGen<Value<'v>>>()
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(dependency_methods)
    }

    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        self.provider_collection_value()
            .at(index, heap)
            .with_buck_error_context(|| format!("Error accessing dependencies of `{}`", self.label))
            .map_err(Into::into)
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        self.provider_collection_value().is_in(other)
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        let other = match other.downcast_ref::<Dependency<'v>>() {
            Some(other) => other.label(),
            None => match other.downcast_ref::<FrozenDependency>() {
                Some(other) => other.label(),
                None => return Ok(false),
            },
        };
        Ok(self.label().inner() == other.inner())
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.label().inner().hash(hasher);
        Ok(())
    }
}

/// Represents a dependency in a build rule. When you declare a dependency attribute using
/// `attrs.dep()` in your rule definition, accessing that attribute gives you a Dependency object
/// that provides access to the dependency's providers and metadata.
///
/// Key operations:
/// - Index with `dep[ProviderType]` to access a provider (errors if absent)
/// - Use `dep.get(ProviderType)` to optionally access a provider (returns None if absent)
/// - Access the dependency's label with `dep.label`
/// - Get subtargets with `dep.sub_target("name")`
///
/// Example usage in a rule:
/// ```python
/// my_library = rule(
///     impl = my_library_impl,
///     attrs = {
///         "deps": attrs.list(attrs.dep()),
///     },
/// )
///
/// def my_library_impl(ctx):
///     # Iterate over dependencies
///     for dep in ctx.attrs.deps:
///         # Access providers
///         if dep.get(CxxLibraryInfo):
///             libs = dep[CxxLibraryInfo].libraries
///
///         # Access outputs
///         outputs = dep[DefaultInfo].default_outputs
///
///         # Get the label
///         dep_target = dep.label.raw_target()
/// ```
#[starlark_module]
fn dependency_methods(builder: &mut MethodsBuilder) {
    /// The label of this dependency.
    #[starlark(attribute)]
    fn label<'v>(this: &Dependency<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(heap.alloc(StarlarkProvidersLabel::new(
            this.label().inner().unconfigured(),
        )))
    }

    /// Bazel target-style shortcut for `dep[DefaultInfo].files`.
    #[starlark(attribute)]
    fn files<'v>(this: &Dependency<'v>) -> starlark::Result<Value<'v>> {
        Ok(this.map_default_info(
            |info| Ok(info.files_raw_for_dependency()),
            |info| Ok(info.files_raw().to_value()),
        )?)
    }

    /// Bazel target-style shortcut for `dep[DefaultInfo].files_to_run`.
    #[starlark(attribute)]
    fn files_to_run<'v>(this: &Dependency<'v>) -> starlark::Result<Value<'v>> {
        Ok(this.map_default_info(
            |info| Ok(info.files_to_run_raw_for_dependency()),
            |info| Ok(info.files_to_run_raw().to_value()),
        )?)
    }

    /// Bazel target-style shortcut for `dep[DefaultInfo].data_runfiles`.
    #[starlark(attribute)]
    fn data_runfiles<'v>(this: &Dependency<'v>) -> starlark::Result<Value<'v>> {
        Ok(this.map_default_info(
            |info| Ok(info.data_runfiles_raw_for_dependency()),
            |info| Ok(info.data_runfiles_raw().to_value()),
        )?)
    }

    /// Bazel target-style shortcut for `dep[DefaultInfo].default_runfiles`.
    #[starlark(attribute)]
    fn default_runfiles<'v>(this: &Dependency<'v>) -> starlark::Result<Value<'v>> {
        Ok(this.map_default_info(
            |info| Ok(info.default_runfiles_raw_for_dependency()),
            |info| Ok(info.default_runfiles_raw().to_value()),
        )?)
    }

    /// Returns a list of all providers available from this dependency.
    // TODO(nga): should return provider collection.
    #[starlark(attribute)]
    fn providers<'v>(this: &Dependency<'v>) -> starlark::Result<Vec<Value<'v>>> {
        Ok(this
            .provider_collection_shallow_clone()
            .providers
            .values()
            .copied()
            .collect())
    }

    /// Returns a `Dependency` object of the subtarget of this target.
    ///
    /// In most cases, you can also use `dep[DefaultInfo].sub_targets["foo"]` to access subtarget
    /// providers directly. This method is useful when you need a real `Dependency` object, such
    /// as when passing to `ctx.actions.anon_target()`.
    ///
    /// Example:
    /// ```python
    /// def _impl(ctx):
    ///     for dep in ctx.attrs.deps:
    ///         # Get the dependency for a subtarget named "shared"
    ///         shared_dep = dep.sub_target("shared")
    ///         # Now shared_dep is a Dependency you can pass to other APIs
    ///         # that require a Dependency object
    ///         ctx.actions.anon_target(my_rule, {"dep": shared_dep})
    /// ```
    fn sub_target<'v>(
        this: &Dependency<'v>,
        #[starlark(require = pos)] subtarget: &str,
        heap: Heap<'v>,
    ) -> starlark::Result<Dependency<'v>> {
        let di = this.provider_collection.default_info()?;
        let providers = di.get_sub_target_providers(subtarget).ok_or_else(|| {
            bz_error::Error::from(DependencyError::UnknownSubtarget(subtarget.to_owned()))
        })?;
        let lbl = StarlarkConfiguredProvidersLabel::from_value(this.label.get())
            .unwrap()
            .inner();
        let lbl = ConfiguredProvidersLabel::new(
            lbl.target().clone(),
            lbl.name().push(ProviderName::new(subtarget.to_owned())?),
        );
        Ok(Dependency::new(heap, lbl, providers, None))
    }

    /// Gets a specific provider from this dependency by provider type. Returns None if the
    /// provider is not present. This is the same as using indexing syntax `dep[ProviderType]`,
    /// but returns None instead of raising an error when the provider is absent.
    ///
    /// Example:
    /// ```python
    /// FooInfo = provider(fields=["bar"])
    ///
    /// def _impl(ctx):
    ///     for dep in ctx.attrs.deps:
    ///         # Try to get FooInfo provider, returns None if absent
    ///         foo_info = dep.get(FooInfo)
    ///         if foo_info:
    ///             # Provider exists, use it
    ///             value = foo_info.bar
    ///         else:
    ///             # Provider not available from this dependency
    ///             pass
    ///
    ///         # Compare with indexing (raises error if absent):
    ///         # foo_info = dep[FooInfo]  # Errors if FooInfo not provided
    /// ```
    fn get<'v>(
        this: &Dependency<'v>,
        index: Value<'v>,
    ) -> starlark::Result<NoneOr<ValueOfUnchecked<'v, AbstractProvider>>> {
        Ok(this
            .provider_collection_shallow_clone()
            .get(index)
            .with_buck_error_context(|| {
                format!("Error accessing dependencies of `{}`", this.label)
            })?)
    }
}

#[starlark_module]
#[starlark_types(
    DependencyGen<FrozenValue> as Dependency
)]
pub(crate) fn register_dependency(globals: &mut GlobalsBuilder) {}
