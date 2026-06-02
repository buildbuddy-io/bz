/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_build_api::interpreter::rule_defs::provider::collection::FrozenProviderCollection;
use bz_build_api::interpreter::rule_defs::provider::dependency::Dependency;
use bz_core::execution_types::execution::ExecutionPlatformResolution;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_node::attrs::attr_type::configured_dep::ConfiguredExplicitConfiguredDep;
use bz_node::attrs::attr_type::configured_dep::ExplicitConfiguredDepAttrType;
use bz_node::attrs::attr_type::dep::DepAttr;
use bz_node::attrs::attr_type::dep::DepAttrTransition;
use bz_node::attrs::attr_type::dep::DepAttrType;
use bz_node::attrs::attr_type::transition_dep::ConfiguredTransitionDep;
use bz_node::attrs::attr_type::transition_dep::TransitionDepAttrType;
use bz_node::provider_id_set::ProviderIdSet;
use starlark::environment::Module;
use starlark::values::FrozenValueTyped;
use starlark::values::Value;

use crate::attrs::resolve::ctx::AttrResolutionContext;

#[derive(bz_error::Error, Debug)]
#[buck2(tag = Input)]
enum ResolutionError {
    #[error(
        "Attribute requires a dep that provides `{0}`, but it was not found on `{1}`. Found these providers: {}",
        .2.join(", "),
)]
    MissingRequiredProvider(String, ConfiguredProvidersLabel, Vec<String>),
}

fn required_providers_description(required_providers: &ProviderIdSet) -> String {
    required_providers
        .provider_groups()
        .iter()
        .map(|group| {
            group
                .iter()
                .map(|provider_id| provider_id.name())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

pub trait DepAttrTypeExt {
    fn check_providers(
        required_providers: &ProviderIdSet,
        providers: &FrozenProviderCollection,
        target: &ConfiguredProvidersLabel,
    ) -> bz_error::Result<()>;

    fn alloc_dependency<'v>(
        env: &Module<'v>,
        target: &ConfiguredProvidersLabel,
        v: FrozenValueTyped<'v, FrozenProviderCollection>,
        execution_platform_resolution: Option<&ExecutionPlatformResolution>,
    ) -> Value<'v>;

    fn resolve_single_impl<'v>(
        ctx: &mut dyn AttrResolutionContext<'v>,
        target: &ConfiguredProvidersLabel,
        required_providers: &ProviderIdSet,
        is_exec: bool,
    ) -> bz_error::Result<Value<'v>>;

    fn resolve_single<'v>(
        ctx: &mut dyn AttrResolutionContext<'v>,
        dep_attr: &DepAttr<ConfiguredProvidersLabel>,
    ) -> bz_error::Result<Value<'v>>;
}

impl DepAttrTypeExt for DepAttrType {
    fn check_providers(
        required_providers: &ProviderIdSet,
        providers: &FrozenProviderCollection,
        target: &ConfiguredProvidersLabel,
    ) -> bz_error::Result<()> {
        if required_providers.is_empty()
            || required_providers
                .provider_groups()
                .iter()
                .any(|group| group.iter().all(|id| providers.contains_provider(id)))
        {
            return Ok(());
        }
        Err(ResolutionError::MissingRequiredProvider(
            required_providers_description(required_providers),
            target.clone(),
            providers.provider_names(),
        )
        .into())
    }

    fn alloc_dependency<'v>(
        env: &Module<'v>,
        target: &ConfiguredProvidersLabel,
        v: FrozenValueTyped<'v, FrozenProviderCollection>,
        execution_platform_resolution: Option<&ExecutionPlatformResolution>,
    ) -> Value<'v> {
        env.heap().alloc(Dependency::new(
            env.heap(),
            target.clone(),
            v,
            execution_platform_resolution,
        ))
    }

    fn resolve_single_impl<'v>(
        ctx: &mut dyn AttrResolutionContext<'v>,
        target: &ConfiguredProvidersLabel,
        required_providers: &ProviderIdSet,
        is_exec_dep: bool,
    ) -> bz_error::Result<Value<'v>> {
        let provider_collection = ctx.get_dep(target)?;
        Self::check_providers(required_providers, provider_collection.as_ref(), target)?;
        let execution_platform_resolution = if is_exec_dep {
            Some(ctx.execution_platform_resolution())
        } else {
            None
        };

        Ok(Self::alloc_dependency(
            ctx.starlark_module(),
            target,
            provider_collection,
            execution_platform_resolution,
        ))
    }

    fn resolve_single<'v>(
        ctx: &mut dyn AttrResolutionContext<'v>,
        dep_attr: &DepAttr<ConfiguredProvidersLabel>,
    ) -> bz_error::Result<Value<'v>> {
        let is_exec = dep_attr.attr_type.transition == DepAttrTransition::Exec;
        Self::resolve_single_impl(
            ctx,
            &dep_attr.label,
            &dep_attr.attr_type.required_providers,
            is_exec,
        )
    }
}

pub(crate) trait ExplicitConfiguredDepAttrTypeExt {
    fn resolve_single<'v>(
        ctx: &mut dyn AttrResolutionContext<'v>,
        dep_attr: &ConfiguredExplicitConfiguredDep,
    ) -> bz_error::Result<Value<'v>> {
        DepAttrType::resolve_single_impl(
            ctx,
            &dep_attr.label,
            &dep_attr.attr_type.required_providers,
            false,
        )
    }
}

impl ExplicitConfiguredDepAttrTypeExt for ExplicitConfiguredDepAttrType {}

pub(crate) trait TransitionDepAttrTypeExt {
    fn resolve_single<'v>(
        ctx: &mut dyn AttrResolutionContext<'v>,
        dep_attr: &ConfiguredTransitionDep,
    ) -> bz_error::Result<Value<'v>> {
        DepAttrType::resolve_single_impl(ctx, &dep_attr.dep, &dep_attr.required_providers, false)
    }
}

impl TransitionDepAttrTypeExt for TransitionDepAttrType {}
