/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Calculations relating to 'TargetNode's that runs on Dice

use std::collections::BTreeSet;
use std::hash::Hash;
use std::hash::Hasher;
use std::iter;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_build_api::analysis::calculation::RuleAnalysisCalculation;
use buck2_build_api::interpreter::rule_defs::provider::builtin::dep_only_incompatible_info::DepOnlyIncompatibleCustomSoftErrors;
use buck2_build_api::interpreter::rule_defs::provider::builtin::dep_only_incompatible_info::FrozenDepOnlyIncompatibleInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::platform_info::FrozenPlatformInfo;
use buck2_build_api::transition::TRANSITION_ATTRS_PROVIDER;
use buck2_build_api::transition::TRANSITION_CALCULATION;
use buck2_build_api::transition::TransitionAttrs;
use buck2_build_signals::node_key::BuildSignalsNodeKey;
use buck2_build_signals::node_key::BuildSignalsNodeKeyImpl;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::dice::cycles::CycleGuard;
use buck2_common::legacy_configs::cells::get_bazel_module_registered_toolchains_on_dice;
use buck2_common::legacy_configs::dice::HasLegacyConfigs;
use buck2_common::legacy_configs::key::BuckconfigKeyRef;
use buck2_common::legacy_configs::view::LegacyBuckConfigView;
use buck2_common::pattern::resolve::ResolveTargetPatterns;
use buck2_core::configuration::compatibility::IncompatiblePlatformReason;
use buck2_core::configuration::compatibility::IncompatiblePlatformReasonCause;
use buck2_core::configuration::compatibility::MaybeCompatible;
use buck2_core::configuration::compatibility::ResultMaybeCompatible;
use buck2_core::configuration::data::BazelBuildSettingValue;
use buck2_core::configuration::data::ConfigurationData;
use buck2_core::configuration::pair::Configuration;
use buck2_core::configuration::pair::ConfigurationNoExec;
use buck2_core::configuration::pair::ConfigurationWithExec;
use buck2_core::configuration::transition::applied::TransitionApplied;
use buck2_core::configuration::transition::id::TransitionId;
use buck2_core::execution_types::execution::ExecutionPlatformResolution;
use buck2_core::execution_types::execution::ExecutionPlatformResolutionPartial;
use buck2_core::package::PackageLabel;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern::TargetParsingRel;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::plugins::PluginKind;
use buck2_core::plugins::PluginKindSet;
use buck2_core::plugins::PluginListElemKind;
use buck2_core::plugins::PluginLists;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::provider::label::ProvidersName;
use buck2_core::soft_error;
use buck2_core::target::configured_or_unconfigured::ConfiguredOrUnconfiguredTargetLabel;
use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
use buck2_core::target::label::label::TargetLabel;
use buck2_core::target::target_configured_target_label::TargetConfiguredTargetLabel;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use buck2_hash::BuckHasher;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::configuration_context::AttrConfigurationContext;
use buck2_node::attrs::configuration_context::AttrConfigurationContextImpl;
use buck2_node::attrs::configuration_context::PlatformConfigurationError;
use buck2_node::attrs::configured_attr::ConfiguredAttr;
use buck2_node::attrs::configured_traversal::ConfiguredAttrTraversal;
use buck2_node::attrs::display::AttrDisplayWithContextExt;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::attrs::spec::AttributeId;
use buck2_node::attrs::spec::internal::EXEC_COMPATIBLE_WITH_ATTRIBUTE;
use buck2_node::attrs::spec::internal::INCOMING_TRANSITION_ATTRIBUTE;
use buck2_node::attrs::spec::internal::LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE;
use buck2_node::attrs::spec::internal::TARGET_COMPATIBLE_WITH_ATTRIBUTE;
use buck2_node::configuration::calculation::CellNameForConfigurationResolution;
use buck2_node::configuration::resolved::ConfigurationSettingKey;
use buck2_node::configuration::resolved::MatchedConfigurationSettingKeys;
use buck2_node::configuration::resolved::MatchedConfigurationSettingKeysWithCfg;
use buck2_node::nodes::configured::BazelResolvedToolchain;
use buck2_node::nodes::configured::ConfiguredTargetNode;
use buck2_node::nodes::configured_frontend::CONFIGURED_TARGET_NODE_CALCULATION;
use buck2_node::nodes::configured_frontend::ConfiguredTargetNodeCalculation;
use buck2_node::nodes::configured_frontend::ConfiguredTargetNodeCalculationImpl;
use buck2_node::nodes::frontend::TargetGraphCalculation;
use buck2_node::nodes::unconfigured::TargetNode;
use buck2_node::nodes::unconfigured::TargetNodeRef;
use buck2_node::rule::RuleIncomingTransition;
use buck2_node::rule_type::RuleType;
use buck2_node::visibility::VisibilityError;
use buck2_node::visibility::VisibilityPatternList;
use buck2_util::arc_str::ArcStr;
use derive_more::Display;
use dice::Demand;
use dice::DiceComputations;
use dice::Key;
use dice::NoValueSerialize;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use dupe::OptionDupedExt;
use futures::FutureExt;
use futures::future::BoxFuture;
use itertools::Itertools;
use pagable::Pagable;
use pagable::StaticStr;
use pagable::pagable_typetag;
use starlark_map::ordered_map::OrderedMap;
use starlark_map::small_map::SmallMap;
use starlark_map::small_set::SmallSet;

use crate::configuration::compute_platform_cfgs;
use crate::configuration::get_matched_cfg_keys;
use crate::configuration::get_matched_cfg_keys_for_node;
use crate::cycle::ConfiguredGraphCycleDescriptor;
use crate::execution::configure_exec_dep_with_modifiers;
use crate::execution::find_execution_platform_by_configuration;
use crate::execution::resolve_execution_platform;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum NodeCalculationError {
    #[error("expected `{0}` attribute to be a list but got `{1}`")]
    TargetCompatibleNotList(String, String),
    #[error(
        "`{0}` had both `{}` and `{}` attributes. It should only have one.",
        TARGET_COMPATIBLE_WITH_ATTRIBUTE.name,
        LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE.name
    )]
    BothTargetCompatibleWith(String),
    #[error(
        "Target {0} configuration transitioned\n\
        old: {1}\n\
        new: {2}\n\
        but attribute: {3}\n\
        resolved with old configuration to: {4}\n\
        resolved with new configuration to: {5}"
    )]
    TransitionAttrIncompatibleChange(
        TargetLabel,
        ConfigurationData,
        ConfigurationData,
        String,
        String,
        String,
    ),

    #[error(
        "Target {0} configuration transition is not idempotent
         in initial configuration  `{1}`
         first transitioned to cfg `{2}`
         then transitions to cfg   `{3}`
         Use `buck2 audit configurations {1} {2} {3}` to see the configurations."
    )]
    TransitionNotIdempotent(
        TargetLabel,
        ConfigurationData,
        ConfigurationData,
        ConfigurationData,
    ),
    #[error("unsupported value for Bazel build setting `//command_line_option:platforms`: `{0}`")]
    UnsupportedBazelPlatformsValue(String),
    #[error("Expected `{0}` to be a `platform()` target, but it had no `PlatformInfo` provider.")]
    MissingPlatformInfo(TargetLabel),
}

enum CompatibilityConstraints {
    Any(ConfiguredAttr),
    All(ConfiguredAttr),
}

#[derive(Debug, buck2_error::Error)]
#[buck2(input)]
enum ToolchainDepError {
    #[error("Target `{0}` was used as a toolchain_dep, but is not a toolchain rule")]
    NonToolchainRuleUsedAsToolchainDep(TargetLabel),
    #[error("Target `{0}` was used not as a toolchain_dep, but is a toolchain rule")]
    ToolchainRuleUsedAsNormalDep(TargetLabel),
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum PluginDepError {
    #[error("Plugin dep `{0}` is a toolchain rule")]
    PluginDepIsToolchainRule(TargetLabel),
}

fn unpack_target_compatible_with_attr(
    target_label: &ConfiguredTargetLabel,
    target_node: TargetNodeRef,
    resolved_cfg: &MatchedConfigurationSettingKeysWithCfg,
    attr_id: AttributeId,
) -> buck2_error::Result<Option<ConfiguredAttr>> {
    let attr = target_node.known_attr_or_none(attr_id, AttrInspectOptions::All);
    let attr = match attr {
        Some(attr) => attr,
        None => return Ok(None),
    };

    struct AttrConfigurationContextToResolveCompatibleWith<'c> {
        target_label: &'c ConfiguredTargetLabel,
        resolved_cfg: &'c MatchedConfigurationSettingKeysWithCfg,
        label: TargetLabel,
    }

    impl AttrConfigurationContext for AttrConfigurationContextToResolveCompatibleWith<'_> {
        fn matched_cfg_keys(&self) -> &MatchedConfigurationSettingKeys {
            self.resolved_cfg.settings()
        }

        fn cfg(&self) -> ConfigurationNoExec {
            self.resolved_cfg.cfg().dupe()
        }

        fn target_label(&self) -> Option<&TargetLabel> {
            Some(&self.label)
        }

        fn base_exec_cfg(&self) -> buck2_error::Result<ConfigurationNoExec> {
            Err(internal_error!(
                "exec_cfg() is not needed to resolve `{}` or `{}`",
                TARGET_COMPATIBLE_WITH_ATTRIBUTE.name,
                LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE.name
            ))
        }

        fn toolchain_cfg(&self) -> ConfigurationWithExec {
            unreachable!()
        }

        fn platform_cfg(&self, _label: &TargetLabel) -> buck2_error::Result<ConfigurationData> {
            unreachable!(
                "platform_cfg() is not needed to resolve `{}` or `{}`",
                TARGET_COMPATIBLE_WITH_ATTRIBUTE.name, LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE.name
            )
        }

        fn resolved_transitions(
            &self,
        ) -> buck2_error::Result<&OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>>> {
            Err(internal_error!(
                "resolved_transitions() is not needed to resolve `{}` or `{}`",
                TARGET_COMPATIBLE_WITH_ATTRIBUTE.name,
                LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE.name
            ))
        }

        fn incompatible_platform_reason(
            &self,
            cause: IncompatiblePlatformReasonCause,
        ) -> Arc<IncompatiblePlatformReason> {
            Arc::new(IncompatiblePlatformReason {
                target: self.target_label.dupe(),
                cause,
            })
        }
    }

    let attr = attr
        .configure(&AttrConfigurationContextToResolveCompatibleWith {
            resolved_cfg,
            label: target_node.label().dupe(),
            target_label,
        })
        .with_buck_error_context(|| format!("Error configuring attribute `{}`", attr.name))
        .require_compatible()?;

    match attr.value.unpack_list() {
        Some(values) => {
            if !values.is_empty() {
                Ok(Some(attr.value))
            } else {
                Ok(None)
            }
        }
        None => Err(NodeCalculationError::TargetCompatibleNotList(
            attr.name.to_owned(),
            attr.value.as_display_no_ctx().to_string(),
        )
        .into()),
    }
}

fn check_compatible(
    target_label: &ConfiguredTargetLabel,
    target_node: TargetNodeRef,
    resolved_cfg: &MatchedConfigurationSettingKeysWithCfg,
) -> buck2_error::Result<MaybeCompatible<()>> {
    let target_compatible_with = unpack_target_compatible_with_attr(
        target_label,
        target_node,
        resolved_cfg,
        TARGET_COMPATIBLE_WITH_ATTRIBUTE.id,
    )?;
    let legacy_compatible_with = unpack_target_compatible_with_attr(
        target_label,
        target_node,
        resolved_cfg,
        LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE.id,
    )?;

    let compatibility_constraints = match (target_compatible_with, legacy_compatible_with) {
        (None, None) => return Ok(MaybeCompatible::Compatible(())),
        (Some(..), Some(..)) => {
            return Err(
                NodeCalculationError::BothTargetCompatibleWith(target_label.to_string()).into(),
            );
        }
        (Some(target_compatible_with), None) => {
            CompatibilityConstraints::All(target_compatible_with)
        }
        (None, Some(legacy_compatible_with)) => {
            CompatibilityConstraints::Any(legacy_compatible_with)
        }
    };

    // We are compatible if the list of target expressions is empty,
    // OR if we match ANY expression in the list of attributes.
    let check_compatibility = |attr| -> buck2_error::Result<(Vec<_>, Vec<_>)> {
        let mut left = Vec::new();
        let mut right = Vec::new();
        for label in ConfiguredTargetNode::attr_as_target_compatible_with(attr) {
            let label = label?;
            match resolved_cfg.settings().setting_matches(&label) {
                Some(_) => left.push(label),
                None => right.push(label),
            }
        }

        Ok((left, right))
    };

    // We only record the first incompatibility, for either ANY or ALL.
    // TODO(cjhopman): Should we report _all_ the things that are incompatible?
    let incompatible_target = match compatibility_constraints {
        CompatibilityConstraints::Any(attr) => {
            let (compatible, incompatible) =
                check_compatibility(attr).with_buck_error_context(|| {
                    format!(
                        "attribute `{}`",
                        LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE.name
                    )
                })?;
            let incompatible = incompatible.into_iter().next();
            match (compatible.is_empty(), incompatible.into_iter().next()) {
                (false, _) | (true, None) => {
                    return Ok(MaybeCompatible::Compatible(()));
                }
                (true, Some(v)) => v,
            }
        }
        CompatibilityConstraints::All(attr) => {
            let (_compatible, incompatible) =
                check_compatibility(attr).with_buck_error_context(|| {
                    format!("attribute `{}`", TARGET_COMPATIBLE_WITH_ATTRIBUTE.name)
                })?;
            match incompatible.into_iter().next() {
                Some(label) => label,
                None => {
                    return Ok(MaybeCompatible::Compatible(()));
                }
            }
        }
    };
    Ok(MaybeCompatible::Incompatible(Arc::new(
        IncompatiblePlatformReason {
            target: target_label.dupe(),
            cause: IncompatiblePlatformReasonCause::UnsatisfiedConfig(incompatible_target.0.dupe()),
        },
    )))
}

/// Ideally, we would check this much earlier. However, that turns out to be a bit tricky to
/// implement. Naively implementing this check on unconfigured nodes doesn't work because it results
/// in dice cycles when there are cycles in the unconfigured graph.
async fn check_plugin_deps(
    ctx: &mut DiceComputations<'_>,
    target_label: &ConfiguredTargetLabel,
    plugin_deps: &PluginLists,
) -> buck2_error::Result<()> {
    for (_, dep_label, elem_kind) in plugin_deps.iter() {
        if *elem_kind == PluginListElemKind::Direct {
            let dep_node = ctx
                .get_target_node(dep_label)
                .await
                .with_buck_error_context(|| {
                    format!("looking up unconfigured target node `{dep_label}`")
                })?;
            if dep_node.is_toolchain_rule() {
                return Err(PluginDepError::PluginDepIsToolchainRule(dep_label.dupe()).into());
            }
            if !target_node_is_visible_to(ctx, &dep_node, target_label.unconfigured()).await? {
                return Err(VisibilityError::NotVisibleTo(
                    dep_label.dupe(),
                    target_label.unconfigured().dupe(),
                )
                .into());
            }
        }
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum CheckVisibility {
    Yes,
    OrPackages(Vec<PackageLabel>),
    No,
}

impl CheckVisibility {
    fn for_bazel_attr(
        target_node: TargetNodeRef<'_>,
        attr_name: &str,
    ) -> buck2_error::Result<Self> {
        Ok(
            match target_node.bazel_implicit_attr_visibility_package(attr_name)? {
                Some(package) => CheckVisibility::OrPackages(vec![package]),
                None => CheckVisibility::Yes,
            },
        )
    }

    fn merge(&mut self, other: &CheckVisibility) {
        match (&mut *self, other) {
            (CheckVisibility::Yes, _) | (_, CheckVisibility::Yes) => {
                *self = CheckVisibility::Yes;
            }
            (CheckVisibility::No, check) => {
                *self = check.clone();
            }
            (CheckVisibility::OrPackages(packages), CheckVisibility::OrPackages(other)) => {
                for package in other {
                    if !packages.contains(package) {
                        packages.push(package.dupe());
                    }
                }
            }
            (_, CheckVisibility::No) => {}
        }
    }
}

impl Default for CheckVisibility {
    fn default() -> Self {
        CheckVisibility::Yes
    }
}

#[derive(Default)]
pub(crate) struct ErrorsAndIncompatibilities {
    errs: Vec<buck2_error::Error>,
    incompats: Vec<Arc<IncompatiblePlatformReason>>,
}

impl ErrorsAndIncompatibilities {
    fn unpack_dep_no_visibility_into(
        &mut self,
        target_label: &TargetConfiguredTargetLabel,
        result: ResultMaybeCompatible<ConfiguredTargetNode>,
        list: &mut Vec<ConfiguredTargetNode>,
    ) {
        if let Some(dep) = self.unpack_dep_no_visibility(target_label, result) {
            list.push(dep);
        }
    }

    fn unpack_dep_no_visibility(
        &mut self,
        target_label: &TargetConfiguredTargetLabel,
        result: ResultMaybeCompatible<ConfiguredTargetNode>,
    ) -> Option<ConfiguredTargetNode> {
        match result {
            ResultMaybeCompatible::Err(e) => {
                self.errs.push(e);
            }
            ResultMaybeCompatible::Incompatible(reason) => {
                self.incompats.push(Arc::new(IncompatiblePlatformReason {
                    target: target_label.inner().dupe(),
                    cause: IncompatiblePlatformReasonCause::Dependency(reason.dupe()),
                }));
            }
            ResultMaybeCompatible::Compatible(dep) => return Some(dep),
        }
        None
    }

    fn unpack_dep<'a>(
        &'a mut self,
        ctx: &'a mut DiceComputations<'_>,
        target_label: &'a TargetConfiguredTargetLabel,
        result: ResultMaybeCompatible<ConfiguredTargetNode>,
        check_visibility: &'a CheckVisibility,
    ) -> BoxFuture<'a, Option<ConfiguredTargetNode>> {
        async move {
            match result {
                ResultMaybeCompatible::Err(e) => {
                    self.errs.push(e);
                }
                ResultMaybeCompatible::Incompatible(reason) => {
                    self.incompats.push(Arc::new(IncompatiblePlatformReason {
                        target: target_label.inner().dupe(),
                        cause: IncompatiblePlatformReasonCause::Dependency(reason.dupe()),
                    }));
                }
                ResultMaybeCompatible::Compatible(dep) => {
                    if CheckVisibility::No == *check_visibility {
                        return Some(dep);
                    }
                    match dependency_is_visible(ctx, &dep, target_label, check_visibility).await {
                        Ok(true) => {
                            return Some(dep);
                        }
                        Ok(false) => {
                            self.errs.push(
                                VisibilityError::NotVisibleTo(
                                    dep.label().unconfigured().dupe(),
                                    target_label.unconfigured().dupe(),
                                )
                                .into(),
                            );
                        }
                        Err(e) => {
                            self.errs.push(e);
                        }
                    }
                }
            }
            None
        }
        .boxed()
    }

    /// Returns an error/incompatibility to return, if any, and `None` otherwise
    pub(crate) fn finalize(mut self) -> ResultMaybeCompatible<()> {
        // FIXME(JakobDegen): Report all incompatibilities
        if let Some(incompat) = self.incompats.pop() {
            return ResultMaybeCompatible::Incompatible(incompat);
        }
        if let Some(err) = self.errs.pop() {
            return ResultMaybeCompatible::Err(err);
        }
        ResultMaybeCompatible::Compatible(())
    }
}

async fn dependency_is_visible(
    ctx: &mut DiceComputations<'_>,
    dep: &ConfiguredTargetNode,
    target_label: &TargetConfiguredTargetLabel,
    check_visibility: &CheckVisibility,
) -> buck2_error::Result<bool> {
    target_node_dependency_is_visible(ctx, dep.target_node(), target_label, check_visibility).await
}

async fn target_node_dependency_is_visible(
    ctx: &mut DiceComputations<'_>,
    dep: &TargetNode,
    target_label: &TargetConfiguredTargetLabel,
    check_visibility: &CheckVisibility,
) -> buck2_error::Result<bool> {
    if dep.bazel_package_group().is_some() {
        return Ok(true);
    }
    if dep.is_visible_to(target_label.unconfigured())? {
        return Ok(true);
    }
    if target_node_visibility_matches_bazel_package_groups_for_target(
        ctx,
        dep,
        target_label.unconfigured(),
    )
    .await?
    {
        return Ok(true);
    }

    match check_visibility {
        CheckVisibility::No | CheckVisibility::Yes => Ok(false),
        CheckVisibility::OrPackages(packages) => {
            for package in packages {
                if dep.is_visible_to_package(package)? {
                    return Ok(true);
                }
                if target_node_visibility_matches_bazel_package_groups_for_package(
                    ctx, dep, package,
                )
                .await?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

async fn precheck_exec_dep_visibility(
    ctx: &mut DiceComputations<'_>,
    target_label: &TargetConfiguredTargetLabel,
    exec_deps: SmallMap<ConfiguredProvidersLabel, CheckVisibility>,
    errors_and_incompats: &mut ErrorsAndIncompatibilities,
) -> buck2_error::Result<SmallMap<ConfiguredProvidersLabel, CheckVisibility>> {
    let mut checked = SmallMap::new();
    for (dep, check_visibility) in exec_deps {
        if check_visibility == CheckVisibility::No {
            checked.insert(dep, check_visibility);
            continue;
        }
        let dep_node = ctx
            .get_target_node(dep.target().unconfigured())
            .await
            .with_buck_error_context(|| {
                format!(
                    "looking up unconfigured target node `{}`",
                    dep.target().unconfigured()
                )
            })?;
        match target_node_dependency_is_visible(ctx, &dep_node, target_label, &check_visibility)
            .await
        {
            Ok(true) => {
                checked.insert(dep, CheckVisibility::No);
            }
            Ok(false) => {
                errors_and_incompats.errs.push(
                    VisibilityError::NotVisibleTo(
                        dep.target().unconfigured().dupe(),
                        target_label.unconfigured().dupe(),
                    )
                    .into(),
                );
            }
            Err(e) => errors_and_incompats.errs.push(e),
        }
    }
    Ok(checked)
}

async fn precheck_toolchain_dep_visibility(
    ctx: &mut DiceComputations<'_>,
    target_label: &TargetConfiguredTargetLabel,
    toolchain_deps: SmallSet<TargetConfiguredTargetLabel>,
    errors_and_incompats: &mut ErrorsAndIncompatibilities,
) -> buck2_error::Result<SmallSet<TargetConfiguredTargetLabel>> {
    let mut checked = SmallSet::new();
    for dep in toolchain_deps {
        let dep_node = ctx
            .get_target_node(dep.unconfigured())
            .await
            .with_buck_error_context(|| {
                format!(
                    "looking up unconfigured target node `{}`",
                    dep.unconfigured()
                )
            })?;
        match target_node_dependency_is_visible(ctx, &dep_node, target_label, &CheckVisibility::Yes)
            .await
        {
            Ok(true) => {
                checked.insert(dep);
            }
            Ok(false) => {
                errors_and_incompats.errs.push(
                    VisibilityError::NotVisibleTo(
                        dep.unconfigured().dupe(),
                        target_label.unconfigured().dupe(),
                    )
                    .into(),
                );
            }
            Err(e) => errors_and_incompats.errs.push(e),
        }
    }
    Ok(checked)
}

async fn target_node_is_visible_to(
    ctx: &mut DiceComputations<'_>,
    dep: &TargetNode,
    target: &TargetLabel,
) -> buck2_error::Result<bool> {
    if dep.is_visible_to(target)? {
        return Ok(true);
    }
    target_node_visibility_matches_bazel_package_groups_for_target(ctx, dep, target).await
}

async fn target_node_visibility_matches_bazel_package_groups_for_target(
    ctx: &mut DiceComputations<'_>,
    dep: &TargetNode,
    target: &TargetLabel,
) -> buck2_error::Result<bool> {
    let group_labels = bazel_package_group_visibility_labels(dep)?;
    bazel_package_groups_allow_target(ctx, group_labels, target).await
}

async fn target_node_visibility_matches_bazel_package_groups_for_package(
    ctx: &mut DiceComputations<'_>,
    dep: &TargetNode,
    package: &PackageLabel,
) -> buck2_error::Result<bool> {
    let group_labels = bazel_package_group_visibility_labels(dep)?;
    bazel_package_groups_allow_package(ctx, group_labels, package).await
}

fn bazel_package_group_visibility_labels(
    dep: &TargetNode,
) -> buck2_error::Result<Vec<TargetLabel>> {
    let mut labels = Vec::new();
    if let VisibilityPatternList::List(patterns) = &dep.visibility()?.0 {
        for pattern in patterns {
            if let ParsedPattern::Target(package, name, TargetPatternExtra) = &pattern.0 {
                labels.push(TargetLabel::new(package.dupe(), name.as_ref()));
            }
        }
    }
    Ok(labels)
}

async fn bazel_package_groups_allow_target(
    ctx: &mut DiceComputations<'_>,
    group_labels: Vec<TargetLabel>,
    target: &TargetLabel,
) -> buck2_error::Result<bool> {
    let mut seen = BTreeSet::new();
    let mut stack = group_labels;
    while let Some(group_label) = stack.pop() {
        if !seen.insert(group_label.dupe()) {
            continue;
        }
        let group_node = ctx
            .get_target_node(&group_label)
            .await
            .with_buck_error_context(|| format!("looking up package_group `{group_label}`"))?;
        if group_node
            .bazel_package_group_contains_target(target)
            .unwrap_or(false)
        {
            return Ok(true);
        }
        if let Some(group) = group_node.bazel_package_group() {
            stack.extend(group.includes.iter().cloned());
        }
    }
    Ok(false)
}

async fn bazel_package_groups_allow_package(
    ctx: &mut DiceComputations<'_>,
    group_labels: Vec<TargetLabel>,
    package: &PackageLabel,
) -> buck2_error::Result<bool> {
    let mut seen = BTreeSet::new();
    let mut stack = group_labels;
    while let Some(group_label) = stack.pop() {
        if !seen.insert(group_label.dupe()) {
            continue;
        }
        let group_node = ctx
            .get_target_node(&group_label)
            .await
            .with_buck_error_context(|| format!("looking up package_group `{group_label}`"))?;
        if group_node
            .bazel_package_group_contains_package(package)
            .unwrap_or(false)
        {
            return Ok(true);
        }
        if let Some(group) = group_node.bazel_package_group() {
            stack.extend(group.includes.iter().cloned());
        }
    }
    Ok(false)
}

#[derive(Default)]
pub(crate) struct GatheredDeps {
    pub(crate) deps: Vec<ConfiguredTargetNode>,
    pub(crate) exec_deps: SmallMap<ConfiguredProvidersLabel, CheckVisibility>,
    pub(crate) toolchain_deps: SmallSet<TargetConfiguredTargetLabel>,
    pub(crate) plugin_lists: PluginLists,
}

fn is_bazel_default_make_variable_attribute(name: &str) -> bool {
    matches!(
        name,
        "toolchains" | ":cc_toolchain" | "$toolchains" | "$cc_toolchain"
    )
}

pub(crate) async fn gather_deps(
    target_label: &TargetConfiguredTargetLabel,
    target_node: TargetNodeRef<'_>,
    attr_cfg_ctx: &(dyn AttrConfigurationContext + Sync),
    ctx: &mut DiceComputations<'_>,
) -> ResultMaybeCompatible<(GatheredDeps, ErrorsAndIncompatibilities)> {
    #[derive(Default)]
    struct Traversal {
        deps: OrderedMap<ConfiguredProvidersLabel, (SmallSet<PluginKindSet>, CheckVisibility)>,
        exec_deps: SmallMap<ConfiguredProvidersLabel, CheckVisibility>,
        toolchain_deps: SmallSet<TargetConfiguredTargetLabel>,
        plugin_lists: PluginLists,
        current_visibility_check: CheckVisibility,
    }

    impl Traversal {
        fn insert_dep_visibility(&mut self, dep: &ConfiguredProvidersLabel) {
            self.deps
                .entry(dep.dupe())
                .or_insert_with(|| (SmallSet::new(), self.current_visibility_check.clone()))
                .1
                .merge(&self.current_visibility_check);
        }

        fn insert_exec_dep_visibility(&mut self, dep: &ConfiguredProvidersLabel) {
            self.exec_deps
                .entry(dep.dupe())
                .or_insert_with(|| self.current_visibility_check.clone())
                .merge(&self.current_visibility_check);
        }

        fn insert_bazel_make_variable_label_deps(
            &mut self,
            attr: &ConfiguredAttr,
        ) -> buck2_error::Result<()> {
            match attr {
                ConfiguredAttr::Label(label) => self.dep(label),
                ConfiguredAttr::List(list) => {
                    for item in list.iter() {
                        self.insert_bazel_make_variable_label_deps(item)?;
                    }
                    Ok(())
                }
                ConfiguredAttr::Tuple(tuple) => {
                    for item in tuple.iter() {
                        self.insert_bazel_make_variable_label_deps(item)?;
                    }
                    Ok(())
                }
                ConfiguredAttr::Dict(dict) => {
                    for (key, value) in dict.iter() {
                        self.insert_bazel_make_variable_label_deps(key)?;
                        self.insert_bazel_make_variable_label_deps(value)?;
                    }
                    Ok(())
                }
                ConfiguredAttr::OneOf(attr, _) => self.insert_bazel_make_variable_label_deps(attr),
                _ => Ok(()),
            }
        }
    }

    impl ConfiguredAttrTraversal for Traversal {
        fn dep(&mut self, dep: &ConfiguredProvidersLabel) -> buck2_error::Result<()> {
            self.insert_dep_visibility(dep);
            Ok(())
        }

        fn dep_with_plugins(
            &mut self,
            dep: &ConfiguredProvidersLabel,
            plugin_kinds: &PluginKindSet,
        ) -> buck2_error::Result<()> {
            self.insert_dep_visibility(dep);
            self.deps
                .entry(dep.dupe())
                .or_insert_with(|| (SmallSet::new(), self.current_visibility_check.clone()))
                .0
                .insert(plugin_kinds.dupe());
            Ok(())
        }

        fn exec_dep(&mut self, dep: &ConfiguredProvidersLabel) -> buck2_error::Result<()> {
            self.insert_exec_dep_visibility(dep);
            Ok(())
        }

        fn toolchain_dep(&mut self, dep: &ConfiguredProvidersLabel) -> buck2_error::Result<()> {
            self.toolchain_deps
                .insert(TargetConfiguredTargetLabel::new_without_exec_cfg(
                    dep.target().dupe(),
                ));
            Ok(())
        }

        fn plugin_dep(&mut self, dep: &TargetLabel, kind: &PluginKind) -> buck2_error::Result<()> {
            self.plugin_lists
                .insert(kind.dupe(), dep.dupe(), PluginListElemKind::Direct);
            Ok(())
        }
    }

    let mut traversal = Traversal::default();
    for a in target_node.attrs(AttrInspectOptions::All) {
        traversal.current_visibility_check = CheckVisibility::for_bazel_attr(target_node, a.name)?;
        let configured_attr = a.configure(attr_cfg_ctx)?;
        configured_attr
            .traverse(target_node.label().pkg(), &mut traversal)
            .with_buck_error_context(|| format!("traversing attribute `{}`", a.name))?;
        if target_node.is_bazel_rule() && is_bazel_default_make_variable_attribute(a.name) {
            traversal
                .insert_bazel_make_variable_label_deps(&configured_attr.value)
                .with_buck_error_context(|| {
                    format!("traversing Bazel make variable attribute `{}`", a.name)
                })?;
        }
    }

    let dep_results = ctx
        .compute_join(traversal.deps.iter(), |ctx, v| {
            async move { get_configured_dep_node(ctx, v.0.target()).await }.boxed()
        })
        .await;

    let mut plugin_lists = traversal.plugin_lists;
    let mut deps = Vec::new();
    let mut errors_and_incompats = ErrorsAndIncompatibilities::default();
    for (res, (_, (plugin_kind_sets, check_visibility))) in
        dep_results.into_iter().zip(traversal.deps)
    {
        let Some(dep) = errors_and_incompats
            .unpack_dep(ctx, target_label, res, &check_visibility)
            .await
        else {
            continue;
        };

        if !plugin_kind_sets.is_empty() {
            for (kind, plugins) in dep.plugin_lists().iter_by_kind() {
                let Some(should_propagate) = plugin_kind_sets
                    .iter()
                    .filter_map(|set| set.get(kind))
                    .reduce(std::ops::BitOr::bitor)
                else {
                    continue;
                };
                let should_propagate = if should_propagate {
                    PluginListElemKind::Propagate
                } else {
                    PluginListElemKind::NoPropagate
                };
                for (target, elem_kind) in plugins {
                    if *elem_kind != PluginListElemKind::NoPropagate {
                        plugin_lists.insert(kind.dupe(), target.dupe(), should_propagate);
                    }
                }
            }
        }

        deps.push(dep);
    }

    let mut exec_deps = traversal.exec_deps;
    for kind in target_node.uses_plugins() {
        for plugin_label in plugin_lists.iter_for_kind(kind).map(|(target, _)| {
            attr_cfg_ctx.configure_exec_target(&ProvidersLabel::default_for(target.dupe()))
        }) {
            exec_deps
                .entry(plugin_label?)
                .or_insert(CheckVisibility::No);
        }
    }

    let exec_deps =
        precheck_exec_dep_visibility(ctx, target_label, exec_deps, &mut errors_and_incompats)
            .await?;
    let toolchain_deps = precheck_toolchain_dep_visibility(
        ctx,
        target_label,
        traversal.toolchain_deps,
        &mut errors_and_incompats,
    )
    .await?;

    ResultMaybeCompatible::Compatible((
        GatheredDeps {
            deps,
            exec_deps,
            toolchain_deps,
            plugin_lists,
        },
        errors_and_incompats,
    ))
}

fn new_bazel_input_file_configured_target_node(
    target_label: &ConfiguredTargetLabel,
    target_node: TargetNode,
) -> buck2_error::Result<ConfiguredTargetNode> {
    Ok(ConfiguredTargetNode::new(
        target_label.dupe(),
        target_node,
        MatchedConfigurationSettingKeysWithCfg::new(
            ConfigurationNoExec::new(target_label.cfg().dupe()),
            MatchedConfigurationSettingKeys::empty(),
        ),
        OrderedMap::new(),
        ExecutionPlatformResolution::unspecified(),
        Vec::new(),
        Vec::new(),
        OrderedMap::new(),
        Vec::new(),
        PluginLists::new(),
    ))
}

async fn get_configured_dep_node(
    ctx: &mut DiceComputations<'_>,
    target_label: &ConfiguredTargetLabel,
) -> ResultMaybeCompatible<ConfiguredTargetNode> {
    let target_node = ctx
        .get_target_node(target_label.unconfigured())
        .await
        .with_buck_error_context(|| {
            format!(
                "looking up unconfigured target node `{}`",
                target_label.unconfigured()
            )
        })?;

    if matches!(target_node.rule_type(), RuleType::BazelInputFile) {
        return ResultMaybeCompatible::Compatible(new_bazel_input_file_configured_target_node(
            target_label,
            target_node,
        )?);
    }

    ctx.get_internal_configured_target_node(target_label).await
}

struct ResolvedTransitionInputAttrs<'a> {
    attrs: OrderedMap<&'a str, Arc<ConfiguredAttr>>,
    requires_post_transition_attr_check: bool,
}

/// Resolves configured attributes of target node needed to compute transitions.
async fn resolve_transition_input_attrs<'a>(
    target_label: &ConfiguredTargetLabel,
    transitions: impl Iterator<Item = &TransitionId>,
    target_node: &'a TargetNode,
    matched_cfg_keys: &MatchedConfigurationSettingKeysWithCfg,
    platform_cfgs: &OrderedMap<TargetLabel, ConfigurationData>,
    ctx: &mut DiceComputations<'_>,
) -> ResultMaybeCompatible<ResolvedTransitionInputAttrs<'a>> {
    struct AttrConfigurationContextToResolveTransitionAttrs<'c> {
        target_label: &'c ConfiguredTargetLabel,
        matched_cfg_keys: &'c MatchedConfigurationSettingKeysWithCfg,
        toolchain_cfg: ConfigurationWithExec,
        platform_cfgs: &'c OrderedMap<TargetLabel, ConfigurationData>,
        label: TargetLabel,
    }

    impl AttrConfigurationContext for AttrConfigurationContextToResolveTransitionAttrs<'_> {
        fn matched_cfg_keys(&self) -> &MatchedConfigurationSettingKeys {
            self.matched_cfg_keys.settings()
        }

        fn cfg(&self) -> ConfigurationNoExec {
            self.matched_cfg_keys.cfg().dupe()
        }

        fn target_label(&self) -> Option<&TargetLabel> {
            Some(&self.label)
        }

        fn base_exec_cfg(&self) -> buck2_error::Result<ConfigurationNoExec> {
            // Bazel transition implementations receive attrs in the pre-rule
            // configuration, and label attrs are exposed as unconfigured Label
            // values. If an attr itself has `cfg = "exec"`, we still need to
            // produce an intermediate ConfiguredAttr before converting it back
            // into that Label-shaped Starlark value.
            Ok(self.matched_cfg_keys.cfg().dupe())
        }

        fn toolchain_cfg(&self) -> ConfigurationWithExec {
            self.toolchain_cfg.dupe()
        }

        fn platform_cfg(&self, label: &TargetLabel) -> buck2_error::Result<ConfigurationData> {
            match self.platform_cfgs.get(label) {
                Some(configuration) => Ok(configuration.dupe()),
                None => Err(PlatformConfigurationError::UnknownPlatformTarget(label.dupe()).into()),
            }
        }

        fn resolved_transitions(
            &self,
        ) -> buck2_error::Result<&OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>>> {
            // TODO(cjhopman): Why is this an internal error? Doesn't it indicate an error in the
            // rule or the target definition? Do we enforce it somewhere else as an input error?
            Err(internal_error!(
                "resolved_transitions() can't be used before transition execution."
            ))
        }

        fn configure_transition_target(
            &self,
            label: &ProvidersLabel,
            _tr: &TransitionId,
        ) -> buck2_error::Result<ConfiguredProvidersLabel> {
            // Transition implementations receive attr label values as labels in the
            // pre-transition configuration. The outgoing attr transition is applied
            // later when configured deps are gathered.
            Ok(label.configure_pair(self.matched_cfg_keys.cfg().cfg_pair().dupe()))
        }

        fn incompatible_platform_reason(
            &self,
            cause: IncompatiblePlatformReasonCause,
        ) -> Arc<IncompatiblePlatformReason> {
            Arc::new(IncompatiblePlatformReason {
                target: self.target_label.dupe(),
                cause,
            })
        }
    }

    let cfg_ctx = AttrConfigurationContextToResolveTransitionAttrs {
        target_label,
        matched_cfg_keys,
        platform_cfgs,
        toolchain_cfg: matched_cfg_keys
            .cfg()
            .make_toolchain(&ConfigurationNoExec::unbound_exec()),
        label: target_node.label().dupe(),
    };
    let mut result = OrderedMap::default();
    let mut requires_post_transition_attr_check = false;
    for tr in transitions {
        let attrs = TRANSITION_ATTRS_PROVIDER
            .get()?
            .transition_attrs(ctx, tr)
            .await?;
        requires_post_transition_attr_check |= attrs.requires_post_transition_attr_check();
        match attrs {
            TransitionAttrs::None => {}
            TransitionAttrs::Listed(attrs) => {
                for attr in attrs.as_ref() {
                    // Multiple outgoing transitions may refer the same attribute.
                    if result.contains_key(attr.as_str()) {
                        continue;
                    }

                    if let Some(coerced_attr) = target_node.attr(attr, AttrInspectOptions::All)? {
                        let configured_attr = coerced_attr.configure(&cfg_ctx)?;
                        if let Some(old_val) =
                            result.insert(configured_attr.name, Arc::new(configured_attr.value))
                        {
                            return internal_error!(
                                "Found duplicated value `{}` for attr `{}` on target `{}`",
                                &old_val.as_display_no_ctx(),
                                attr,
                                target_node.label()
                            )
                            .into();
                        }
                    }
                }
            }
            TransitionAttrs::All => {
                for coerced_attr in target_node.attrs(AttrInspectOptions::All) {
                    if !coerced_attr_can_be_transition_attr(coerced_attr.value) {
                        continue;
                    }

                    // Multiple outgoing transitions may refer the same attribute.
                    if result.contains_key(coerced_attr.name) {
                        continue;
                    }

                    let configured_attr = coerced_attr.configure(&cfg_ctx)?;
                    if let Some(old_val) =
                        result.insert(configured_attr.name, Arc::new(configured_attr.value))
                    {
                        return internal_error!(
                            "Found duplicated value `{}` for attr `{}` on target `{}`",
                            &old_val.as_display_no_ctx(),
                            coerced_attr.name,
                            target_node.label()
                        )
                        .into();
                    }
                }
            }
            TransitionAttrs::BazelAll => {
                for coerced_attr in target_node.attrs(AttrInspectOptions::All) {
                    if !coerced_attr_can_be_bazel_transition_attr(coerced_attr.value) {
                        continue;
                    }

                    // Multiple outgoing transitions may refer the same attribute.
                    if result.contains_key(coerced_attr.name) {
                        continue;
                    }

                    let configured_attr = coerced_attr.configure(&cfg_ctx)?;
                    if let Some(old_val) =
                        result.insert(configured_attr.name, Arc::new(configured_attr.value))
                    {
                        return internal_error!(
                            "Found duplicated value `{}` for attr `{}` on target `{}`",
                            &old_val.as_display_no_ctx(),
                            coerced_attr.name,
                            target_node.label()
                        )
                        .into();
                    }
                }
            }
        }
    }
    ResultMaybeCompatible::Compatible(ResolvedTransitionInputAttrs {
        attrs: result,
        requires_post_transition_attr_check,
    })
}

fn coerced_attr_can_be_transition_attr(value: &CoercedAttr) -> bool {
    match value {
        CoercedAttr::ExplicitConfiguredDep(_)
        | CoercedAttr::TransitionDep(_)
        | CoercedAttr::SplitTransitionDep(_)
        | CoercedAttr::ConfiguredDepForForwardNode(_)
        | CoercedAttr::ConfigurationDep(_)
        | CoercedAttr::PluginDep(_)
        | CoercedAttr::Dep(_)
        | CoercedAttr::SourceLabel(_)
        | CoercedAttr::SourceFile(_) => false,
        CoercedAttr::OneOf(value, _) => coerced_attr_can_be_transition_attr(value),
        CoercedAttr::Selector(select) => select
            .all_entries()
            .all(|(_, value)| coerced_attr_can_be_transition_attr(value)),
        CoercedAttr::Concat(values) => values.iter().all(coerced_attr_can_be_transition_attr),
        CoercedAttr::List(values) => values.iter().all(coerced_attr_can_be_transition_attr),
        CoercedAttr::Tuple(values) => values.iter().all(coerced_attr_can_be_transition_attr),
        CoercedAttr::Dict(values) => values.iter().all(|(key, value)| {
            coerced_attr_can_be_transition_attr(key) && coerced_attr_can_be_transition_attr(value)
        }),
        CoercedAttr::SelectFail(_)
        | CoercedAttr::SelectIncompatible(_)
        | CoercedAttr::Bool(_)
        | CoercedAttr::Int(_)
        | CoercedAttr::String(_)
        | CoercedAttr::EnumVariant(_)
        | CoercedAttr::None
        | CoercedAttr::Visibility(_)
        | CoercedAttr::WithinView(_)
        | CoercedAttr::Label(_)
        | CoercedAttr::Arg(_)
        | CoercedAttr::Query(_)
        | CoercedAttr::Metadata(_)
        | CoercedAttr::TargetModifiers(_) => true,
    }
}

fn coerced_attr_can_be_bazel_transition_attr(value: &CoercedAttr) -> bool {
    match value {
        CoercedAttr::TransitionDep(_)
        | CoercedAttr::SplitTransitionDep(_)
        | CoercedAttr::ConfiguredDepForForwardNode(_)
        | CoercedAttr::PluginDep(_)
        | CoercedAttr::SourceFile(_) => false,
        CoercedAttr::OneOf(value, _) => coerced_attr_can_be_bazel_transition_attr(value),
        CoercedAttr::Selector(select) => select
            .all_entries()
            .all(|(_, value)| coerced_attr_can_be_bazel_transition_attr(value)),
        CoercedAttr::Concat(values) => values.iter().all(coerced_attr_can_be_bazel_transition_attr),
        CoercedAttr::List(values) => values.iter().all(coerced_attr_can_be_bazel_transition_attr),
        CoercedAttr::Tuple(values) => values.iter().all(coerced_attr_can_be_bazel_transition_attr),
        CoercedAttr::Dict(values) => values.iter().all(|(key, value)| {
            coerced_attr_can_be_bazel_transition_attr(key)
                && coerced_attr_can_be_bazel_transition_attr(value)
        }),
        CoercedAttr::ExplicitConfiguredDep(_)
        | CoercedAttr::ConfigurationDep(_)
        | CoercedAttr::Dep(_)
        | CoercedAttr::SourceLabel(_)
        | CoercedAttr::SelectFail(_)
        | CoercedAttr::SelectIncompatible(_)
        | CoercedAttr::Bool(_)
        | CoercedAttr::Int(_)
        | CoercedAttr::String(_)
        | CoercedAttr::EnumVariant(_)
        | CoercedAttr::None
        | CoercedAttr::Visibility(_)
        | CoercedAttr::WithinView(_)
        | CoercedAttr::Label(_)
        | CoercedAttr::Arg(_)
        | CoercedAttr::Query(_)
        | CoercedAttr::Metadata(_)
        | CoercedAttr::TargetModifiers(_) => true,
    }
}

/// Verifies if configured node's attributes are equal to the same attributes configured with pre-transition configuration.
/// Only check attributes used in transition.
fn verify_transitioned_attrs(
    // Attributes resolved with pre-transition configuration
    pre_transition_attrs: &OrderedMap<&str, Arc<ConfiguredAttr>>,
    pre_transition_config: &ConfigurationData,
    node: &ConfiguredTargetNode,
) -> buck2_error::Result<()> {
    for (attr, attr_value) in pre_transition_attrs {
        let transition_configured_attr =
            node.get(attr, AttrInspectOptions::All).ok_or_else(|| {
                internal_error!(
                    "Attr {} was not found in transition for target {} ({})",
                    attr,
                    node.label(),
                    node.attrs(AttrInspectOptions::All)
                        .format_with(", ", |v, f| f(&format_args!("{v:?}")))
                )
            })?;
        if &transition_configured_attr.value != attr_value.as_ref() {
            return Err(NodeCalculationError::TransitionAttrIncompatibleChange(
                node.label().unconfigured().dupe(),
                pre_transition_config.dupe(),
                node.label().cfg().dupe(),
                attr.to_string(),
                attr_value.as_display_no_ctx().to_string(),
                transition_configured_attr
                    .value
                    .as_display_no_ctx()
                    .to_string(),
            )
            .into());
        }
    }
    Ok(())
}

fn normalize_bazel_toolchain_key(key: &str) -> String {
    key.trim_start_matches('@').to_owned()
}

fn bazel_toolchain_keys_match(declared: &str, candidate: &str) -> bool {
    declared == candidate
        || declared
            .split_once("//")
            .zip(candidate.split_once("//"))
            .is_some_and(|((_, declared_rest), (_, candidate_rest))| {
                declared_rest == candidate_rest
            })
}

fn bazel_rule_kind_is(node: &TargetNode, kind: &str) -> bool {
    let rule_type = node.rule_type().name();
    rule_type == kind
        || rule_type
            .rsplit_once(':')
            .is_some_and(|(_, name)| name == kind)
}

fn attr_target_label(attr: &CoercedAttr) -> Option<&TargetLabel> {
    match attr {
        CoercedAttr::Label(label)
        | CoercedAttr::Dep(label)
        | CoercedAttr::ConfigurationDep(label)
        | CoercedAttr::SourceLabel(label) => Some(label.target()),
        CoercedAttr::OneOf(inner, _) => attr_target_label(inner),
        _ => None,
    }
}

fn attr_target_labels(attr: &CoercedAttr) -> Vec<TargetLabel> {
    match attr {
        CoercedAttr::List(list) => list
            .iter()
            .filter_map(attr_target_label)
            .map(|label| label.dupe())
            .collect(),
        CoercedAttr::OneOf(inner, _) => attr_target_labels(inner),
        _ => attr_target_label(attr)
            .map(|label| label.dupe())
            .into_iter()
            .collect(),
    }
}

fn attr_configuration_setting_keys(attr: &CoercedAttr) -> Vec<ConfigurationSettingKey> {
    attr_target_labels(attr)
        .into_iter()
        .map(|label| ConfigurationSettingKey(ProvidersLabel::default_for(label)))
        .collect()
}

fn attr_bool(attr: &CoercedAttr) -> Option<bool> {
    match attr {
        CoercedAttr::Bool(value) => Some(value.0),
        CoercedAttr::OneOf(inner, _) => attr_bool(inner),
        _ => None,
    }
}

fn is_bazel_relative_target_shorthand(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['@', ':', '/', '.'])
        && !value.contains('/')
        && !value.contains(':')
        && !value.contains('[')
        && !value.contains(']')
}

async fn bazel_toolchain_implementation_label(
    ctx: &mut DiceComputations<'_>,
    toolchain_node: &TargetNode,
    attr: &CoercedAttr,
) -> buck2_error::Result<Option<TargetLabel>> {
    match attr {
        CoercedAttr::String(value) => {
            let cell_resolver = ctx.get_cell_resolver().await?;
            let cell_alias_resolver = ctx
                .get_cell_alias_resolver(toolchain_node.label().pkg().cell_name())
                .await?;
            Ok(Some(parse_bazel_nodep_label(
                value.as_str(),
                toolchain_node,
                &cell_resolver,
                &cell_alias_resolver,
            )?))
        }
        _ => Ok(attr_target_label(attr).map(|label| label.dupe())),
    }
}

fn parse_bazel_nodep_label(
    value: &str,
    owning_node: &TargetNode,
    cell_resolver: &buck2_core::cells::CellResolver,
    cell_alias_resolver: &buck2_core::cells::CellAliasResolver,
) -> buck2_error::Result<TargetLabel> {
    let package = owning_node.label().pkg();
    let parse = |value: &str| {
        ParsedPattern::<TargetPatternExtra>::parse_not_relaxed(
            value,
            TargetParsingRel::AllowLimitedRelative(package.as_cell_path()),
            cell_resolver,
            cell_alias_resolver,
        )?
        .as_target_label(value)
    };

    match parse(value) {
        Ok(label) => Ok(label),
        Err(_) if is_bazel_relative_target_shorthand(value) => parse(&format!(":{value}")),
        Err(e) => Err(e),
    }
}

const BAZEL_PLATFORMS_OPTION: &str = "//command_line_option:platforms";

fn bazel_transitioned_label(
    data: &buck2_core::configuration::data::ConfigurationDataData,
    is_marked_as_exec_platform: bool,
) -> String {
    let mut hasher = BuckHasher::default();
    "bazel_transition".hash(&mut hasher);
    data.hash(&mut hasher);
    is_marked_as_exec_platform.hash(&mut hasher);
    format!("bazeltr-{:016x}", hasher.finish())
}

async fn parse_bazel_platform_target(
    ctx: &mut DiceComputations<'_>,
    label: &str,
) -> buck2_error::Result<TargetLabel> {
    let cell_resolver = ctx.get_cell_resolver().await?;
    let cell_alias_resolver = ctx
        .get_cell_alias_resolver(cell_resolver.root_cell())
        .await?;
    TargetLabel::parse(
        label,
        cell_resolver.root_cell(),
        &cell_resolver,
        &cell_alias_resolver,
    )
}

async fn bazel_platform_targets_from_setting(
    ctx: &mut DiceComputations<'_>,
    value: &BazelBuildSettingValue,
) -> buck2_error::Result<Vec<TargetLabel>> {
    match value {
        BazelBuildSettingValue::Label(label) => Ok(vec![label.target().dupe()]),
        BazelBuildSettingValue::LabelList(labels) => {
            Ok(labels.iter().map(|label| label.target().dupe()).collect())
        }
        BazelBuildSettingValue::String(label) => Ok(vec![
            parse_bazel_platform_target(ctx, label)
                .await
                .with_buck_error_context(|| format!("Parsing Bazel platform label `{label}`"))?,
        ]),
        BazelBuildSettingValue::StringList(labels) => {
            let mut targets = Vec::with_capacity(labels.len());
            for label in labels {
                targets.push(
                    parse_bazel_platform_target(ctx, label)
                        .await
                        .with_buck_error_context(|| {
                            format!("Parsing Bazel platform label `{label}`")
                        })?,
                );
            }
            Ok(targets)
        }
        BazelBuildSettingValue::Bool(_) | BazelBuildSettingValue::Int(_) => Err(
            NodeCalculationError::UnsupportedBazelPlatformsValue(value.as_config_setting_value())
                .into(),
        ),
    }
}

async fn bazel_host_platform_target(
    ctx: &mut DiceComputations<'_>,
) -> buck2_error::Result<TargetLabel> {
    parse_bazel_platform_target(ctx, "platforms//host:host").await
}

async fn bazel_platform_configuration(
    ctx: &mut DiceComputations<'_>,
    target: &TargetLabel,
    is_marked_as_exec_platform: bool,
) -> buck2_error::Result<ConfigurationData> {
    ctx.get_configuration_analysis_result(&ProvidersLabel::default_for(target.dupe()))
        .await?
        .provider_collection()
        .builtin_provider::<FrozenPlatformInfo>()
        .ok_or_else(|| NodeCalculationError::MissingPlatformInfo(target.dupe()))?
        .to_configuration(is_marked_as_exec_platform)
}

async fn apply_bazel_platform_cfg_to_bazel_rule(
    ctx: &mut DiceComputations<'_>,
    cfg: &ConfigurationData,
) -> buck2_error::Result<ConfigurationData> {
    if !cfg.is_bound() {
        return Ok(cfg.dupe());
    }

    let data = cfg.data()?.clone();
    let mut platform_targets = match data.build_settings.get(BAZEL_PLATFORMS_OPTION) {
        Some(platforms) => bazel_platform_targets_from_setting(ctx, platforms).await?,
        None => Vec::new(),
    };
    if platform_targets.is_empty() {
        platform_targets.push(bazel_host_platform_target(ctx).await?);
    }
    platform_targets.truncate(1);

    let platform_cfg =
        bazel_platform_configuration(ctx, &platform_targets[0], cfg.is_marked_as_exec_platform())
            .await?;

    let mut new_data = platform_cfg.data()?.clone();
    for (key, value) in data.build_settings {
        new_data.build_settings.insert(key, value);
    }
    new_data.build_settings.insert(
        BAZEL_PLATFORMS_OPTION.to_owned(),
        BazelBuildSettingValue::LabelList(
            platform_targets
                .into_iter()
                .map(ProvidersLabel::default_for)
                .collect(),
        ),
    );

    if cfg.data()? == &new_data {
        return Ok(cfg.dupe());
    }

    ConfigurationData::from_platform(
        bazel_transitioned_label(&new_data, cfg.is_marked_as_exec_platform()),
        new_data,
        cfg.is_marked_as_exec_platform(),
    )
}

async fn configuration_settings_match(
    ctx: &mut DiceComputations<'_>,
    cfg: &ConfigurationData,
    target_cell: CellNameForConfigurationResolution,
    keys: &[ConfigurationSettingKey],
) -> buck2_error::Result<bool> {
    if keys.is_empty() {
        return Ok(true);
    }
    if !cfg.is_bound() {
        return Ok(false);
    }

    let matched = get_matched_cfg_keys(ctx, cfg, target_cell, keys.iter()).await?;
    for key in keys {
        if matched.settings().setting_matches(key).is_none() {
            return Ok(false);
        }
    }

    Ok(true)
}

fn platform_constraints_contain_label(
    platform_constraints: &std::collections::BTreeMap<
        buck2_core::configuration::constraints::ConstraintKey,
        buck2_core::configuration::constraints::ConstraintValue,
    >,
    constraint_value: &TargetLabel,
) -> bool {
    let expected = ProvidersLabel::default_for(constraint_value.dupe());
    platform_constraints
        .values()
        .any(|actual| actual.0 == expected)
}

async fn resolve_bazel_alias(
    ctx: &mut DiceComputations<'_>,
    label: &TargetLabel,
) -> buck2_error::Result<TargetLabel> {
    let mut current = label.dupe();
    for _ in 0..16 {
        let node = ctx.get_target_node(&current).await?;
        if !bazel_rule_kind_is(&node, "alias") {
            return Ok(current);
        }
        let Some(actual) = node
            .attr_or_none("actual", AttrInspectOptions::All)
            .and_then(|attr| attr_target_label(&attr.value).map(|label| label.dupe()))
        else {
            return Ok(current);
        };
        if actual == current {
            return Ok(current);
        }
        current = actual;
    }

    Ok(current)
}

async fn resolve_constraint_value_alias(
    ctx: &mut DiceComputations<'_>,
    constraint_value: &TargetLabel,
) -> buck2_error::Result<TargetLabel> {
    resolve_bazel_alias(ctx, constraint_value).await
}

async fn platform_contains_constraint_values(
    ctx: &mut DiceComputations<'_>,
    platform_cfg: &ConfigurationData,
    constraint_values: &[TargetLabel],
) -> buck2_error::Result<bool> {
    if constraint_values.is_empty() {
        return Ok(true);
    }
    if !platform_cfg.is_bound() {
        return Ok(false);
    }

    let platform_constraints = &platform_cfg.data()?.constraints;
    for constraint_value in constraint_values {
        if platform_constraints_contain_label(platform_constraints, constraint_value) {
            continue;
        }

        let resolved = resolve_constraint_value_alias(ctx, constraint_value).await?;
        if !platform_constraints_contain_label(platform_constraints, &resolved) {
            return Ok(false);
        }
    }

    Ok(true)
}

fn platform_contains_platform_constraints(
    platform_cfg: &ConfigurationData,
    required_cfg: &ConfigurationData,
) -> buck2_error::Result<bool> {
    if !platform_cfg.is_bound() || !required_cfg.is_bound() {
        return Ok(false);
    }

    let platform_constraints = &platform_cfg.data()?.constraints;
    for (constraint_key, required_value) in &required_cfg.data()?.constraints {
        match platform_constraints.get(constraint_key) {
            Some(actual_value) if actual_value == required_value => {}
            _ => return Ok(false),
        }
    }

    Ok(true)
}

async fn toolchain_constraints_match(
    ctx: &mut DiceComputations<'_>,
    target_node: &TargetNode,
    target_cfg: &ConfigurationData,
    exec_cfg: &ConfigurationData,
) -> buck2_error::Result<bool> {
    let toolchain_cell = CellNameForConfigurationResolution(target_node.label().pkg().cell_name());

    let target_settings = target_node
        .attr_or_none("target_settings", AttrInspectOptions::All)
        .map(|attr| attr_configuration_setting_keys(&attr.value))
        .unwrap_or_default();
    if !configuration_settings_match(ctx, target_cfg, toolchain_cell, &target_settings).await? {
        return Ok(false);
    }

    let use_target_platform_constraints = target_node
        .attr_or_none("use_target_platform_constraints", AttrInspectOptions::All)
        .and_then(|attr| attr_bool(&attr.value))
        .unwrap_or(false);
    if use_target_platform_constraints {
        return platform_contains_platform_constraints(exec_cfg, target_cfg);
    }

    let target_compatible_with = target_node
        .known_attr_or_none(TARGET_COMPATIBLE_WITH_ATTRIBUTE.id, AttrInspectOptions::All)
        .map(|attr| attr_target_labels(&attr.value))
        .unwrap_or_default();
    if !platform_contains_constraint_values(ctx, target_cfg, &target_compatible_with).await? {
        return Ok(false);
    }

    let exec_compatible_with = target_node
        .known_attr_or_none(EXEC_COMPATIBLE_WITH_ATTRIBUTE.id, AttrInspectOptions::All)
        .map(|attr| attr_target_labels(&attr.value))
        .unwrap_or_default();
    if !platform_contains_constraint_values(ctx, exec_cfg, &exec_compatible_with).await? {
        return Ok(false);
    }

    Ok(true)
}

#[derive(Clone, Debug, Allocative, Eq, PartialEq)]
struct BazelRegisteredToolchain {
    toolchain_type: String,
    node: TargetNode,
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REGISTERED_TOOLCHAINS")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct RegisteredBazelToolchainNodesKey;

async fn compute_registered_bazel_toolchain_nodes(
    ctx: &mut DiceComputations<'_>,
) -> buck2_error::Result<Arc<Vec<BazelRegisteredToolchain>>> {
    let root_conf = ctx.get_legacy_root_config_on_dice().await?;
    let registered: Vec<String> = root_conf
        .view(ctx)
        .parse_list(BuckconfigKeyRef {
            section: "bazel",
            property: "registered_toolchains",
        })?
        .unwrap_or_default();
    let mut registered = registered;
    registered.reverse();
    registered.extend(get_bazel_module_registered_toolchains_on_dice(ctx).await?);
    let mut seen = BTreeSet::new();
    registered.retain(|pattern| seen.insert(pattern.clone()));
    if registered.is_empty() {
        return Ok(Arc::new(Vec::new()));
    }

    let cell_resolver = ctx.get_cell_resolver().await?;
    let root_cell = cell_resolver.root_cell();
    let alias_resolver = ctx.get_cell_alias_resolver(root_cell).await?;

    let mut parsed_patterns = Vec::new();
    for pattern in registered {
        let pattern = ParsedPattern::<TargetPatternExtra>::parse_precise(
            pattern.trim(),
            root_cell,
            &cell_resolver,
            &alias_resolver,
        )?;
        if let ParsedPattern::Target(package, target_name, _) = &pattern {
            if target_name.as_ref().as_str() == "all" {
                parsed_patterns.push(ParsedPattern::Package(package.dupe()));
                continue;
            }
        }
        parsed_patterns.push(pattern);
    }

    let resolved = ResolveTargetPatterns::resolve(ctx, &parsed_patterns).await?;
    let loaded_toolchain_packages = ctx
        .try_compute_join(
            resolved.specs.into_iter().collect::<Vec<_>>(),
            |ctx, (package_with_modifiers, spec)| {
                async move {
                    let result = ctx
                        .get_interpreter_results(package_with_modifiers.package)
                        .await?;
                    buck2_error::Ok((result, spec))
                }
                .boxed()
            },
        )
        .await?;

    let mut toolchains = Vec::new();
    for (result, spec) in loaded_toolchain_packages {
        let (targets, missing) = result.apply_spec(spec);
        if let Some(missing) = missing {
            return Err(missing.into_first_error().into());
        }
        for node in targets.into_values() {
            if !bazel_rule_kind_is(&node, "toolchain") {
                continue;
            }
            let Some(toolchain_type) = node
                .attr_or_none("toolchain_type", AttrInspectOptions::All)
                .and_then(|attr| attr_target_label(&attr.value).map(|label| label.dupe()))
            else {
                continue;
            };
            let toolchain_type = resolve_bazel_alias(ctx, &toolchain_type).await?;
            toolchains.push(BazelRegisteredToolchain {
                toolchain_type: normalize_bazel_toolchain_key(&toolchain_type.to_string()),
                node,
            });
        }
    }
    Ok(Arc::new(toolchains))
}

#[async_trait]
impl Key for RegisteredBazelToolchainNodesKey {
    type Value = buck2_error::Result<Arc<Vec<BazelRegisteredToolchain>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        compute_registered_bazel_toolchain_nodes(ctx).await
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn registered_bazel_toolchain_nodes(
    ctx: &mut DiceComputations<'_>,
) -> buck2_error::Result<Arc<Vec<BazelRegisteredToolchain>>> {
    ctx.compute(&RegisteredBazelToolchainNodesKey).await?
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display(
    "SINGLE_BAZEL_TOOLCHAIN_RESOLUTION({}, target: {}, exec: {})",
    toolchain_type,
    target_cfg,
    exec_cfg
)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BazelSingleToolchainResolutionKey {
    toolchain_type: String,
    target_cfg: ConfigurationData,
    exec_cfg: ConfigurationData,
}

async fn compute_bazel_single_toolchain_resolution(
    ctx: &mut DiceComputations<'_>,
    key: &BazelSingleToolchainResolutionKey,
) -> buck2_error::Result<Option<TargetLabel>> {
    let registered = registered_bazel_toolchain_nodes(ctx).await?;
    if registered.is_empty() {
        return Ok(None);
    }

    for candidate in registered.iter() {
        if !bazel_toolchain_keys_match(&key.toolchain_type, &candidate.toolchain_type) {
            continue;
        }
        if !toolchain_constraints_match(ctx, &candidate.node, &key.target_cfg, &key.exec_cfg)
            .await?
        {
            continue;
        }

        let Some(toolchain_attr) = candidate
            .node
            .attr_or_none("toolchain", AttrInspectOptions::All)
        else {
            continue;
        };
        return bazel_toolchain_implementation_label(ctx, &candidate.node, toolchain_attr.value)
            .await;
    }

    Ok(None)
}

#[async_trait]
impl Key for BazelSingleToolchainResolutionKey {
    type Value = buck2_error::Result<Option<TargetLabel>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        compute_bazel_single_toolchain_resolution(ctx, self).await
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn resolve_bazel_single_toolchain(
    ctx: &mut DiceComputations<'_>,
    toolchain_type: String,
    target_cfg: ConfigurationData,
    exec_cfg: ConfigurationData,
) -> buck2_error::Result<Option<TargetLabel>> {
    ctx.compute(&BazelSingleToolchainResolutionKey {
        toolchain_type,
        target_cfg,
        exec_cfg,
    })
    .await?
}

async fn resolve_bazel_toolchain_type_alias(
    ctx: &mut DiceComputations<'_>,
    toolchain_type: &str,
) -> buck2_error::Result<String> {
    let cell_resolver = ctx.get_cell_resolver().await?;
    let root_cell = cell_resolver.root_cell();
    let alias_resolver = ctx.get_cell_alias_resolver(root_cell).await?;
    let Ok(label) = TargetLabel::parse(toolchain_type, root_cell, &cell_resolver, &alias_resolver)
    else {
        return Ok(toolchain_type.to_owned());
    };
    let resolved = resolve_bazel_alias(ctx, &label).await?;
    Ok(normalize_bazel_toolchain_key(&resolved.to_string()))
}

async fn resolve_bazel_toolchain_deps(
    ctx: &mut DiceComputations<'_>,
    target_label: &ConfiguredTargetLabel,
    target_node: &TargetNode,
    execution_platform_cfg: &ConfigurationNoExec,
) -> buck2_error::Result<(Vec<ConfiguredTargetNode>, Vec<BazelResolvedToolchain>)> {
    if !target_label.cfg().is_bound()
        || (target_node.bazel_toolchains().is_empty()
            && target_node.bazel_aspect_toolchains().is_empty())
    {
        return Ok((Vec::new(), Vec::new()));
    }

    let all_declared_toolchains = target_node
        .bazel_toolchains()
        .iter()
        .chain(target_node.bazel_aspect_toolchains());
    let mut declared_toolchains = SmallMap::new();
    for declared in all_declared_toolchains {
        let declared_key = normalize_bazel_toolchain_key(&declared.toolchain_type);
        let resolution_key = resolve_bazel_toolchain_type_alias(ctx, &declared_key).await?;
        declared_toolchains
            .entry(declared_key)
            .and_modify(|(_, mandatory)| *mandatory |= declared.mandatory)
            .or_insert((resolution_key, declared.mandatory));
    }

    let selected_toolchain_impls: Vec<(String, bool, Option<TargetLabel>)> = ctx
        .try_compute_join(
            declared_toolchains.iter(),
            |ctx, (declared, (resolution_key, mandatory))| {
                async move {
                    let toolchain_impl = resolve_bazel_single_toolchain(
                        ctx,
                        resolution_key.clone(),
                        target_label.cfg().dupe(),
                        execution_platform_cfg.cfg().dupe(),
                    )
                    .await?;
                    buck2_error::Ok((declared.clone(), *mandatory, toolchain_impl))
                }
                .boxed()
            },
        )
        .await?;

    let mut resolved_toolchain_impls = Vec::new();
    for (declared, mandatory, toolchain_impl) in selected_toolchain_impls {
        let Some(toolchain_impl) = toolchain_impl else {
            if mandatory {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "mandatory toolchain type `{}` was not resolved for `{}`",
                    declared,
                    target_label
                ));
            }
            continue;
        };
        let configured = toolchain_impl.configure_with_exec(
            target_label.cfg().dupe(),
            execution_platform_cfg.cfg().dupe(),
        );
        let provider_label =
            ConfiguredProvidersLabel::new(configured.dupe(), ProvidersName::Default);
        resolved_toolchain_impls.push((declared, configured, provider_label));
    }

    let mut deps = Vec::with_capacity(resolved_toolchain_impls.len());
    let mut resolved = Vec::with_capacity(resolved_toolchain_impls.len());
    for (declared, configured, provider_label) in resolved_toolchain_impls {
        let dep = ctx
            .get_internal_configured_target_node(&configured)
            .await
            .require_compatible()?;
        deps.push(dep);
        resolved.push(BazelResolvedToolchain {
            toolchain_type: declared,
            toolchain: provider_label,
        });
    }

    Ok((deps, resolved))
}

/// Compute configured target node ignoring transition for this node.
async fn compute_configured_target_node_no_transition(
    target_label: &ConfiguredTargetLabel,
    target_node: TargetNode,
    ctx: &mut DiceComputations<'_>,
) -> ResultMaybeCompatible<ConfiguredTargetNode> {
    let partial_target_label =
        &TargetConfiguredTargetLabel::new_without_exec_cfg(target_label.dupe());
    let target_cfg = target_label.cfg();
    let target_cell = target_node.label().pkg().cell_name();
    let resolved_configuration = get_matched_cfg_keys_for_node(
        ctx,
        target_cfg,
        CellNameForConfigurationResolution(target_cell),
        target_node.as_ref(),
    )
    .await
    .with_buck_error_context(|| {
        format!("Error resolving configuration deps of `{target_label}`")
    })?;

    // Must check for compatibility before evaluating non-compatibility attributes.
    if let MaybeCompatible::Incompatible(reason) =
        check_compatible(target_label, target_node.as_ref(), &resolved_configuration)?
    {
        return ResultMaybeCompatible::Incompatible(reason);
    }

    let platform_cfgs = compute_platform_cfgs(ctx, target_node.as_ref()).await?;

    let mut resolved_transitions = OrderedMap::new();
    let attrs = resolve_transition_input_attrs(
        target_label,
        target_node.transition_deps().map(|(_, tr)| tr.as_ref()),
        &target_node,
        &resolved_configuration,
        &platform_cfgs,
        ctx,
    )
    .boxed()
    .await?;
    for (_dep, tr) in target_node.transition_deps() {
        let resolved_cfg = TRANSITION_CALCULATION
            .get()?
            .apply_transition(ctx, &attrs.attrs, target_cfg, tr)
            .await?;
        resolved_transitions.insert(tr.dupe(), resolved_cfg);
    }
    drop(attrs);

    // We need to collect deps and to ensure that all attrs can be successfully
    // configured so that we don't need to support propagate configuration errors on attr access.
    let unspecified_resolution = ExecutionPlatformResolution::unspecified();
    let attr_cfg_ctx = AttrConfigurationContextImpl::new(
        target_label.dupe(),
        &resolved_configuration,
        // We have not yet done exec platform resolution so for now we just use `unspecified`
        // here. We only use this when collecting exec deps and toolchain deps. In both of those
        // cases, we replace the exec cfg later on in this function with the "proper" exec cfg.
        &unspecified_resolution,
        &resolved_transitions,
        &platform_cfgs,
        Some(target_label.unconfigured().dupe()),
    );

    let (gathered_deps, mut errors_and_incompats) = gather_deps(
        partial_target_label,
        target_node.as_ref(),
        &attr_cfg_ctx,
        ctx,
    )
    .boxed()
    .await?;

    check_plugin_deps(ctx, target_label, &gathered_deps.plugin_lists)
        .boxed()
        .await?;

    let execution_platform_partial = if target_cfg.is_unbound() {
        // The unbound configuration is used when evaluation configuration nodes.
        // That evaluation is
        // (1) part of execution platform resolution and
        // (2) isn't allowed to do execution
        // And so we use an "unspecified" execution platform to avoid cycles and cause any attempts at execution to fail.
        None
    } else if let Some(exec_cfg) = target_label.exec_cfg() {
        // The label was produced by a toolchain_dep, so we use the execution platform of our parent
        // We need to convert that to an execution platform, so just find the one with the same configuration.
        Some(ExecutionPlatformResolutionPartial::new(
            Some(
                find_execution_platform_by_configuration(
                    ctx,
                    exec_cfg,
                    resolved_configuration.cfg().cfg(),
                    target_node.is_bazel_rule(),
                )
                .await?,
            ),
            Vec::new(),
        ))
    } else {
        Some(
            resolve_execution_platform(
                ctx,
                target_node.as_ref(),
                &resolved_configuration,
                &gathered_deps,
                &attr_cfg_ctx,
            )
            .boxed()
            .await?,
        )
    };

    // Get the execution platform configuration - either from partial or use unspecified
    let execution_platform_cfg = match &execution_platform_partial {
        Some(partial) => partial.cfg(),
        None => ConfigurationNoExec::unspecified_exec().dupe(),
    };

    // We now need to replace the dummy exec config we used above with the real one

    let execution_platform_cfg = &execution_platform_cfg;
    let toolchain_deps = &gathered_deps.toolchain_deps;
    let exec_deps = &gathered_deps.exec_deps;
    let bazel_target_label = target_label.dupe();
    let bazel_target_node = target_node.dupe();
    let get_toolchain_deps = DiceComputations::declare_closure(move |ctx| {
        async move {
            let toolchain_dep_results = ctx
                .compute_join(
                    toolchain_deps,
                    |ctx, target: &TargetConfiguredTargetLabel| {
                        async move {
                            ctx.get_internal_configured_target_node(
                                &target.with_exec_cfg(execution_platform_cfg.cfg().dupe()),
                            )
                            .await
                        }
                        .boxed()
                    },
                )
                .await;
            let bazel_toolchain_result = resolve_bazel_toolchain_deps(
                ctx,
                &bazel_target_label,
                &bazel_target_node,
                execution_platform_cfg,
            )
            .await;
            (toolchain_dep_results, bazel_toolchain_result)
        }
        .boxed()
    });

    let get_exec_deps = DiceComputations::declare_closure(|ctx| {
        async move {
            ctx.compute_join(exec_deps, |ctx, (target, check_visibility)| {
                async move {
                    // Apply modifiers to exec_dep before configuring
                    let result = configure_exec_dep_with_modifiers(
                        ctx,
                        target.target().unconfigured(),
                        execution_platform_cfg.cfg(),
                    )
                    .await;

                    (result, check_visibility.clone())
                }
                .boxed()
            })
            .await
        }
        .boxed()
    });

    let ((toolchain_dep_results, bazel_toolchain_result), exec_dep_results): ((Vec<_>, _), Vec<_>) =
        ctx.compute2(get_toolchain_deps, get_exec_deps).await;

    let mut deps = gathered_deps.deps;
    let mut exec_deps = Vec::with_capacity(gathered_deps.exec_deps.len());

    for dep in toolchain_dep_results {
        errors_and_incompats.unpack_dep_no_visibility_into(partial_target_label, dep, &mut deps);
    }
    for (dep, _check_visibility) in exec_dep_results {
        errors_and_incompats.unpack_dep_no_visibility_into(
            partial_target_label,
            dep,
            &mut exec_deps,
        );
    }
    let (bazel_toolchain_deps, bazel_resolved_toolchains) = bazel_toolchain_result?;
    deps.extend(bazel_toolchain_deps);

    // Build the exec_dep_cfgs mapping from exec_dep target labels to their actual cfgs.
    // This is needed because modifiers may change the cfg of exec_deps, and we need to
    // use the actual cfg when configuring exec_dep attributes during analysis.
    let mut exec_dep_cfgs = OrderedMap::new();
    for exec_dep in &exec_deps {
        exec_dep_cfgs.insert(
            exec_dep.label().unconfigured().dupe(),
            exec_dep.label().cfg().dupe(),
        );
    }

    // Finalize the execution platform resolution with exec_dep_cfgs
    let execution_platform_resolution = match execution_platform_partial {
        Some(partial) => partial.finalize(exec_dep_cfgs),
        None => ExecutionPlatformResolution::unspecified(),
    };

    errors_and_incompats.finalize()?;

    ResultMaybeCompatible::Compatible(ConfiguredTargetNode::new(
        target_label.dupe(),
        target_node.dupe(),
        resolved_configuration,
        resolved_transitions,
        execution_platform_resolution,
        deps,
        exec_deps,
        platform_cfgs,
        bazel_resolved_toolchains,
        gathered_deps.plugin_lists,
    ))
}

async fn compute_configured_target_node(
    key: &ConfiguredTargetNodeKey,
    ctx: &mut DiceComputations<'_>,
) -> ResultMaybeCompatible<ConfiguredTargetNode> {
    let target_node = ctx
        .get_target_node(key.0.unconfigured())
        .await
        .with_buck_error_context(|| {
            format!(
                "looking up unconfigured target node `{}`",
                key.0.unconfigured()
            )
        })?;

    match key.0.exec_cfg() {
        None if target_node.is_toolchain_rule() => {
            return ResultMaybeCompatible::Err(
                ToolchainDepError::ToolchainRuleUsedAsNormalDep(key.0.unconfigured().dupe()).into(),
            );
        }
        Some(_) if !target_node.is_toolchain_rule() && !target_node.is_bazel_rule() => {
            return ResultMaybeCompatible::Err(
                ToolchainDepError::NonToolchainRuleUsedAsToolchainDep(key.0.unconfigured().dupe())
                    .into(),
            );
        }
        _ => {}
    }

    if matches!(target_node.rule_type(), RuleType::BazelInputFile) {
        return ResultMaybeCompatible::Compatible(new_bazel_input_file_configured_target_node(
            &key.0,
            target_node,
        )?);
    }

    let transition_id = match &target_node.rule.cfg {
        RuleIncomingTransition::None => None,
        RuleIncomingTransition::Fixed(transition_id) => Some(transition_id.dupe()),
        RuleIncomingTransition::FromAttribute => target_node
            .attr_or_none(INCOMING_TRANSITION_ATTRIBUTE.name, AttrInspectOptions::All)
            .and_then(|v| match v.value {
                CoercedAttr::None => None,
                CoercedAttr::ConfigurationDep(l) => Some(Arc::new(TransitionId::Target(l.dupe()))),
                _ => unreachable!("Verified by attr coercer"),
            }),
    };

    if let Some(transition_id) = transition_id {
        compute_configured_forward_target_node(key, &target_node, &transition_id, ctx).await
    } else {
        if target_node.is_bazel_rule() {
            let cfg = apply_bazel_platform_cfg_to_bazel_rule(ctx, key.0.cfg()).await?;
            if &cfg != key.0.cfg() {
                let target_label_after_transition = key
                    .0
                    .unconfigured()
                    .configure_pair(Configuration::new(cfg, key.0.exec_cfg().duped()));
                let transitioned_node = ctx
                    .get_internal_configured_target_node(&target_label_after_transition)
                    .await?;
                return ResultMaybeCompatible::Compatible(ConfiguredTargetNode::new_forward(
                    key.0.dupe(),
                    transitioned_node,
                )?);
            }
        }

        // We are not caching `ConfiguredTransitionedNodeKey` because this is cheap,
        // and no need to fetch `target_node` again.
        compute_configured_target_node_no_transition(&key.0.dupe(), target_node, ctx).await
    }
}

async fn compute_configured_forward_target_node(
    key: &ConfiguredTargetNodeKey,
    target_node: &TargetNode,
    transition_id: &TransitionId,
    ctx: &mut DiceComputations<'_>,
) -> ResultMaybeCompatible<ConfiguredTargetNode> {
    let target_label_before_transition = &key.0;
    let platform_cfgs = compute_platform_cfgs(ctx, target_node.as_ref())
        .boxed()
        .await?;
    let matched_cfg_keys = get_matched_cfg_keys_for_node(
        ctx,
        target_label_before_transition.cfg(),
        CellNameForConfigurationResolution(target_node.label().pkg().cell_name()),
        target_node.as_ref(),
    )
    .await
    .with_buck_error_context(|| {
        format!("Error resolving configuration deps of `{target_label_before_transition}`")
    })?;

    let attrs = resolve_transition_input_attrs(
        target_label_before_transition,
        iter::once(transition_id),
        target_node,
        &matched_cfg_keys,
        &platform_cfgs,
        ctx,
    )
    .boxed()
    .await?;

    let cfg = TRANSITION_CALCULATION
        .get()?
        .apply_transition(
            ctx,
            &attrs.attrs,
            target_label_before_transition.cfg(),
            transition_id,
        )
        .await?;
    let target_label_after_transition = target_label_before_transition
        .unconfigured()
        .configure(cfg.single()?.dupe());

    if &target_label_after_transition == target_label_before_transition {
        // Transitioned to identical configured target, no need to create a forward node.
        compute_configured_target_node_no_transition(
            target_label_before_transition,
            target_node.dupe(),
            ctx,
        )
        .boxed()
        .await
    } else {
        // This must call through dice to get the configured target node so that it is the correct
        // instance (because ConfiguredTargetNode uses reference equality on its deps).
        // This also helps further verify idempotence (as we will get the real result with the any transition applied again).
        let transitioned_node = ctx
            .get_internal_configured_target_node(&target_label_after_transition)
            .await?;

        // In apply_transition() above we've checked that the transition is idempotent when applied again with the same attrs (but the
        // transitioned cfg) we don't know if it causes an attr change (and then a subsequent change in the transition
        // result). We verify that here. If we're in a case where it is changing the attr in a way that causes the transition
        // to introduce a cycle, we depend on the dice cycle detection to identify it. Alternatively we could directly recompute
        // the node and check the attrs, but we'd still need to request the real node from dice and it doesn't seem worth
        // that extra cost just for a slightly improved error message.

        // Buck transitions require attrs used by the transition to stay stable under the
        // transitioned configuration. Bazel rule transitions receive attrs configured in the
        // pre-rule configuration and do not enforce this equality invariant.
        if attrs.requires_post_transition_attr_check {
            verify_transitioned_attrs(
                &attrs.attrs,
                matched_cfg_keys.cfg().cfg(),
                &transitioned_node,
            )?;
        }

        if let Some(forward) = transitioned_node.forward_target() {
            return ResultMaybeCompatible::Err(NodeCalculationError::TransitionNotIdempotent(
                    target_label_before_transition.unconfigured().dupe(),
                    target_label_before_transition.cfg().dupe(),
                    target_label_after_transition.cfg().dupe(),
                    forward.label().cfg().dupe(),
                ).into())
                .internal_error("idempotence should have been enforced by transition idempotence and attr change checks");
        }

        let configured_target_node = ConfiguredTargetNode::new_forward(
            target_label_before_transition.dupe(),
            transitioned_node,
        )?;

        ResultMaybeCompatible::Compatible(configured_target_node)
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("CONFIGURED_TARGET({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ConfiguredTargetNodeKey(pub ConfiguredTargetLabel);

struct ConfiguredTargetNodeCalculationInstance;

pub(crate) fn init_configured_target_node_calculation() {
    CONFIGURED_TARGET_NODE_CALCULATION.init(&ConfiguredTargetNodeCalculationInstance);
}

#[derive(Debug, Allocative, Eq, PartialEq)]
pub(crate) struct LookingUpConfiguredNodeContext {
    target: ConfiguredTargetLabel,
    len: usize,
    rest: Option<Arc<Self>>,
}

impl buck2_error::TypedContext for LookingUpConfiguredNodeContext {
    fn eq(&self, other: &dyn buck2_error::TypedContext) -> bool {
        match (other as &dyn std::any::Any).downcast_ref::<Self>() {
            Some(v) => self == v,
            None => false,
        }
    }

    fn display(&self) -> Option<String> {
        Some(format!("{}", self))
    }
}

impl LookingUpConfiguredNodeContext {
    pub(crate) fn new(target: ConfiguredTargetLabel, parent: Option<Arc<Self>>) -> Self {
        let (len, rest) = match parent {
            Some(v) => (v.len + 1, Some(v.clone())),
            None => (1, None),
        };
        Self { target, len, rest }
    }

    pub(crate) fn add_context<T>(
        res: buck2_error::Result<T>,
        target: ConfiguredTargetLabel,
    ) -> buck2_error::Result<T> {
        res.compute_context(
            |parent_ctx: Arc<Self>| Self::new(target.dupe(), Some(parent_ctx)),
            || Self::new(target.dupe(), None),
        )
    }

    pub(crate) fn add_context_rmc<T>(
        res: ResultMaybeCompatible<T>,
        target: ConfiguredTargetLabel,
    ) -> ResultMaybeCompatible<T> {
        res.compute_context(
            |parent_ctx: Arc<Self>| Self::new(target.dupe(), Some(parent_ctx)),
            || Self::new(target.dupe(), None),
        )
    }
}

impl std::fmt::Display for LookingUpConfiguredNodeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.len == 1 {
            write!(f, "Error looking up configured node {}", &self.target)?;
        } else {
            writeln!(
                f,
                "Error in configured node dependency, dependency chain follows (-> indicates depends on, ^ indicates same configuration as previous):"
            )?;

            let mut curr = self;
            let mut prev_cfg = None;
            let mut is_first = true;

            loop {
                f.write_str("    ")?;
                if is_first {
                    f.write_str("   ")?;
                } else {
                    f.write_str("-> ")?;
                }

                write!(f, "{}", curr.target.unconfigured())?;
                let cfg = Some(curr.target.cfg());
                f.write_str(" (")?;
                if cfg == prev_cfg {
                    f.write_str("^")?;
                } else {
                    std::fmt::Display::fmt(curr.target.cfg(), f)?;
                }
                f.write_str(")\n")?;
                is_first = false;
                prev_cfg = Some(curr.target.cfg());
                match &curr.rest {
                    Some(v) => curr = &**v,
                    None => break,
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Key for ConfiguredTargetNodeKey {
    type Value = ResultMaybeCompatible<ConfiguredTargetNode>;
    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let res = CycleGuard::<ConfiguredGraphCycleDescriptor>::new(ctx)?
            .guard_this(compute_configured_target_node(self, ctx))
            .await
            .into_result(ctx)
            .await??;
        LookingUpConfiguredNodeContext::add_context_rmc(res, self.0.dupe())
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (ResultMaybeCompatible::Compatible(x), ResultMaybeCompatible::Compatible(y)) => x == y,
            (ResultMaybeCompatible::Incompatible(x), ResultMaybeCompatible::Incompatible(y)) => {
                x == y
            }
            _ => false,
        }
    }

    fn provide<'a>(&'a self, demand: &mut Demand<'a>) {
        demand.provide_value_with(|| BuildSignalsNodeKey::new(self.dupe()))
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

impl BuildSignalsNodeKeyImpl for ConfiguredTargetNodeKey {
    fn kind(&self) -> &'static str {
        "configure_target"
    }
}

#[async_trait]
impl ConfiguredTargetNodeCalculationImpl for ConfiguredTargetNodeCalculationInstance {
    async fn get_configured_target_node(
        &self,
        ctx: &mut DiceComputations<'_>,
        target: &ConfiguredTargetLabel,
        check_dependency_incompatibility: bool,
    ) -> ResultMaybeCompatible<ConfiguredTargetNode> {
        let maybe_compatible_result = ctx.compute(&ConfiguredTargetNodeKey(target.dupe())).await?;
        if check_dependency_incompatibility {
            if let ResultMaybeCompatible::Incompatible(reason) = &maybe_compatible_result {
                if matches!(
                    &reason.cause,
                    &IncompatiblePlatformReasonCause::Dependency(_)
                ) {
                    if check_error_on_incompatible_dep(ctx, target.unconfigured_label()).await? {
                        return ResultMaybeCompatible::Err(reason.to_err());
                    }
                    soft_error!(
                        "dep_only_incompatible_version_two", reason.to_soft_err(),
                        quiet: false,
                        error_on_oss: true,
                        // Log at least one sample per unique package.
                        low_cardinality_key_for_additional_logview_samples: Some(Box::new(target.unconfigured().pkg())),
                    )?;
                    if let Some(custom_soft_errors) = get_dep_only_incompatible_custom_soft_error(
                        ctx,
                        target.unconfigured_label(),
                    )
                    .await?
                    {
                        for custom_soft_error in custom_soft_errors {
                            soft_error!(
                                &custom_soft_error,
                                reason.to_soft_err(),
                                quiet: true,
                                task: false,
                                error_on_oss: true,
                            )?;
                        }
                    }
                }
            }
        }
        maybe_compatible_result
    }
}

pagable::static_str!(SECTION_BUCK2 = "buck2");
pagable::static_str!(PROPERTY_ERROR_ON_DEP_ONLY_INCOMPATIBLE = "error_on_dep_only_incompatible");
pagable::static_str!(
    PROPERTY_ERROR_ON_DEP_ONLY_INCOMPATIBLE_EXCLUDED = "error_on_dep_only_incompatible_excluded"
);

async fn check_error_on_incompatible_dep(
    ctx: &mut DiceComputations<'_>,
    target_label: &TargetLabel,
) -> buck2_error::Result<bool> {
    if check_target_enabled_for_config(
        ctx,
        target_label,
        SECTION_BUCK2,
        PROPERTY_ERROR_ON_DEP_ONLY_INCOMPATIBLE_EXCLUDED,
    )
    .await?
    {
        return Ok(false);
    }
    check_target_enabled_for_config(
        ctx,
        target_label,
        SECTION_BUCK2,
        PROPERTY_ERROR_ON_DEP_ONLY_INCOMPATIBLE,
    )
    .await
}

async fn check_target_enabled_for_config(
    ctx: &mut DiceComputations<'_>,
    target_label: &TargetLabel,
    section: StaticStr,
    property: StaticStr,
) -> buck2_error::Result<bool> {
    #[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
    #[display("ConfigPatternCalculation({section}, {property})")]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct ConfigPatternCalculation {
        section: StaticStr,
        property: StaticStr,
    }

    #[async_trait]
    impl Key for ConfigPatternCalculation {
        type Value = buck2_error::Result<Arc<Vec<ParsedPattern<TargetPatternExtra>>>>;

        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            _cancellation: &CancellationContext,
        ) -> Self::Value {
            let cell_resolver = ctx.get_cell_resolver().await?;
            let root_cell = cell_resolver.root_cell();
            let alias_resolver = ctx.get_cell_alias_resolver(root_cell).await?;
            let root_conf = ctx.get_legacy_root_config_on_dice().await?;
            let patterns: Vec<String> = root_conf
                .view(ctx)
                .parse_list(BuckconfigKeyRef {
                    section: &self.section,
                    property: &self.property,
                })?
                .unwrap_or_default();

            let mut result = Vec::new();
            for pattern in patterns {
                result.push(ParsedPattern::parse_precise(
                    pattern.trim(),
                    root_cell,
                    &cell_resolver,
                    &alias_resolver,
                )?);
            }
            Ok(result.into())
        }

        fn equality(x: &Self::Value, y: &Self::Value) -> bool {
            match (x, y) {
                (Ok(x), Ok(y)) => x == y,
                _ => false,
            }
        }

        fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
            OkPagableValueSerialize::<Self::Value>::new()
        }
    }

    let patterns = ctx
        .compute(&ConfigPatternCalculation {
            section: section.into(),
            property: property.into(),
        })
        .await??;
    for pattern in patterns.iter() {
        if pattern.matches(target_label) {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn get_dep_only_incompatible_custom_soft_error(
    ctx: &mut DiceComputations<'_>,
    target_label: &TargetLabel,
) -> buck2_error::Result<Option<Vec<ArcStr>>> {
    #[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct GetDepOnlyIncompatibleInfo;

    #[async_trait]
    impl Key for GetDepOnlyIncompatibleInfo {
        type Value = buck2_error::Result<Option<Arc<DepOnlyIncompatibleCustomSoftErrors>>>;

        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            _cancellation: &CancellationContext,
        ) -> Self::Value {
            let cell_resolver = ctx.get_cell_resolver().await?;
            let root_cell = cell_resolver.root_cell();
            let alias_resolver = ctx.get_cell_alias_resolver(root_cell).await?;
            let root_conf = ctx.get_legacy_root_config_on_dice().await?;
            let Some(target) = root_conf.view(ctx).parse::<String>(BuckconfigKeyRef {
                section: "buck2",
                property: "dep_only_incompatible_info",
            })?
            else {
                return Ok(None);
            };
            let target =
                ProvidersLabel::parse(&target, root_cell.dupe(), &cell_resolver, &alias_resolver)?;
            let providers = ctx.get_configuration_analysis_result(&target).await?;
            let dep_only_incompatible_info = providers
                .provider_collection()
                .builtin_provider::<FrozenDepOnlyIncompatibleInfo>()
                .unwrap();
            let result = dep_only_incompatible_info.custom_soft_errors(
                root_cell,
                &cell_resolver,
                &alias_resolver,
            )?;
            Ok(Some(Arc::new(result)))
        }

        fn equality(x: &Self::Value, y: &Self::Value) -> bool {
            match (x, y) {
                (Ok(x), Ok(y)) => x == y,
                _ => false,
            }
        }

        fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
            OkPagableValueSerialize::<Self::Value>::new()
        }
    }

    let Some(custom_soft_errors) = ctx.compute(&GetDepOnlyIncompatibleInfo).await?? else {
        return Ok(None);
    };
    let soft_error_categories: Vec<_> = custom_soft_errors
        .iter()
        .filter_map(|(soft_error_category, rollout_patterns)| {
            if rollout_patterns.matches(target_label) {
                Some(soft_error_category.dupe())
            } else {
                None
            }
        })
        .collect();
    Ok(Some(soft_error_categories))
}

#[allow(unused)]
fn _assert_compute_configured_target_node_no_transition_size() {
    const fn sz<F, T1, T2, T3, R>(_: &F) -> usize
    where
        F: FnOnce(T1, T2, T3) -> R,
    {
        std::mem::size_of::<R>()
    }

    const _: () = assert!(
        sz(&compute_configured_target_node_no_transition) <= 700,
        "compute_configured_target_node_no_transition size is larger than 700 bytes",
    );
}

#[allow(unused)]
fn _assert_compute_configured_forward_target_node_size() {
    const fn sz<F, T1, T2, T3, T4, R>(_: &F) -> usize
    where
        F: FnOnce(T1, T2, T3, T4) -> R,
    {
        std::mem::size_of::<R>()
    }

    const _: () = assert!(
        sz(&compute_configured_forward_target_node) <= 700,
        "compute_configured_forward_target_node size is larger than 700 bytes",
    );
}
