/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;

use bz_core::configuration::compatibility::IncompatiblePlatformReason;
use bz_core::configuration::compatibility::IncompatiblePlatformReasonCause;
use bz_core::configuration::data::ConfigurationData;
use bz_core::configuration::pair::ConfigurationNoExec;
use bz_core::configuration::pair::ConfigurationWithExec;
use bz_core::configuration::transition::applied::TransitionApplied;
use bz_core::configuration::transition::id::TransitionId;
use bz_core::execution_types::execution::ExecutionPlatformResolution;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_core::provider::label::ProvidersLabel;
use bz_core::target::configured_target_label::ConfiguredTargetLabel;
use bz_core::target::label::label::TargetLabel;
use bz_error::internal_error;
use dupe::Dupe;
use starlark_map::ordered_map::OrderedMap;
use starlark_map::sorted_map::SortedMap;

use crate::configuration::resolved::MatchedConfigurationSettingKeys;
use crate::configuration::resolved::MatchedConfigurationSettingKeysWithCfg;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Tier0)]
pub enum PlatformConfigurationError {
    #[error("Could not find configuration for platform target `{0}`")]
    UnknownPlatformTarget(TargetLabel),
}

/// The context for attribute configuration. Contains information about the
/// configuration.
pub trait AttrConfigurationContext {
    fn matched_cfg_keys(&self) -> &MatchedConfigurationSettingKeys;

    fn cfg(&self) -> ConfigurationNoExec;

    /// The target label whose attributes are being configured.
    /// Used for diagnostics (select_first_match_differs soft error message).
    // TODO(nero): clean this when we migrate to the first select match
    fn target_label(&self) -> Option<&TargetLabel> {
        None
    }

    fn base_exec_cfg(&self) -> bz_error::Result<ConfigurationNoExec>;

    /// Must be equal to `(cfg, Some(exec_cfg))`.
    fn toolchain_cfg(&self) -> ConfigurationWithExec;

    fn platform_cfg(&self, label: &TargetLabel) -> bz_error::Result<ConfigurationData>;

    /// Map of transition ids resolved to configurations
    /// using current node configuration as input.
    fn resolved_transitions(
        &self,
    ) -> bz_error::Result<&OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>>>;

    /// Constructs an `IncompatiblePlatformReason` for this target from the given cause.
    fn incompatible_platform_reason(
        &self,
        cause: IncompatiblePlatformReasonCause,
    ) -> Arc<IncompatiblePlatformReason>;

    fn configure_target(&self, label: &ProvidersLabel) -> ConfiguredProvidersLabel {
        label.configure_pair(self.cfg().cfg_pair().dupe())
    }

    fn configure_exec_target(
        &self,
        label: &ProvidersLabel,
    ) -> bz_error::Result<ConfiguredProvidersLabel> {
        Ok(label.configure_pair(self.base_exec_cfg()?.cfg_pair().dupe()))
    }

    fn configure_toolchain_target(&self, label: &ProvidersLabel) -> ConfiguredProvidersLabel {
        // The toolchain dependency itself is always configured in the target configuration,
        // but its exec_deps are considered when picking an execution platform, and MUST
        // use the execution dependency of its parent.
        label.configure_pair(self.toolchain_cfg().cfg_pair().dupe())
    }

    /// Configure a transition target.
    fn configure_transition_target(
        &self,
        label: &ProvidersLabel,
        tr: &TransitionId,
    ) -> bz_error::Result<ConfiguredProvidersLabel> {
        let cfg = self
            .resolved_transitions()?
            .get(tr)
            .ok_or_else(|| internal_error!("internal error: no resolved transition"))?;
        Ok(label.configure(cfg.single()?.dupe()))
    }

    fn configure_split_transition_target(
        &self,
        label: &ProvidersLabel,
        tr: &TransitionId,
    ) -> bz_error::Result<SortedMap<String, ConfiguredProvidersLabel>> {
        let cfg = self
            .resolved_transitions()?
            .get(tr)
            .ok_or_else(|| internal_error!("internal error: no resolved transition"))?;
        let split = cfg.split()?;
        Ok(split
            .iter()
            .map(|(k, v)| (k.to_owned(), label.configure(v.dupe())))
            .collect())
    }
}

pub struct AttrConfigurationContextImpl<'b> {
    configured_target_label: ConfiguredTargetLabel,
    resolved_cfg: &'b MatchedConfigurationSettingKeysWithCfg,
    /// Must be equal to `(cfg, Some(exec_cfg))`.
    toolchain_cfg: ConfigurationWithExec,
    resolved_transitions: &'b OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>>,
    platform_cfgs: &'b OrderedMap<TargetLabel, ConfigurationData>,
    /// The execution platform resolution, which contains per-exec_dep configurations
    /// when in the Resolved state.
    execution_platform_resolution: &'b ExecutionPlatformResolution,
    label: Option<TargetLabel>,
}

impl<'b> AttrConfigurationContextImpl<'b> {
    pub fn new(
        configured_target_label: ConfiguredTargetLabel,
        resolved_cfg: &'b MatchedConfigurationSettingKeysWithCfg,
        execution_platform_resolution: &'b ExecutionPlatformResolution,
        resolved_transitions: &'b OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>>,
        platform_cfgs: &'b OrderedMap<TargetLabel, ConfigurationData>,
        label: Option<TargetLabel>,
    ) -> AttrConfigurationContextImpl<'b> {
        let exec_cfg = execution_platform_resolution.base_cfg();
        AttrConfigurationContextImpl {
            configured_target_label,
            resolved_cfg,
            toolchain_cfg: resolved_cfg.cfg().make_toolchain(&exec_cfg),
            resolved_transitions,
            platform_cfgs,
            execution_platform_resolution,
            label,
        }
    }
}

impl AttrConfigurationContext for AttrConfigurationContextImpl<'_> {
    fn matched_cfg_keys(&self) -> &MatchedConfigurationSettingKeys {
        self.resolved_cfg.settings()
    }

    fn cfg(&self) -> ConfigurationNoExec {
        self.resolved_cfg.cfg().dupe()
    }

    fn target_label(&self) -> Option<&TargetLabel> {
        self.label.as_ref()
    }

    fn incompatible_platform_reason(
        &self,
        cause: IncompatiblePlatformReasonCause,
    ) -> Arc<IncompatiblePlatformReason> {
        Arc::new(IncompatiblePlatformReason {
            target: self.configured_target_label.dupe(),
            cause,
        })
    }

    fn base_exec_cfg(&self) -> bz_error::Result<ConfigurationNoExec> {
        Ok(self.execution_platform_resolution.base_cfg())
    }

    fn configure_exec_target(
        &self,
        label: &ProvidersLabel,
    ) -> bz_error::Result<ConfiguredProvidersLabel> {
        let cfg = self
            .execution_platform_resolution
            .cfg_for_exec_dep(label.target())?;
        Ok(label.configure(cfg))
    }

    fn toolchain_cfg(&self) -> ConfigurationWithExec {
        self.toolchain_cfg.dupe()
    }

    fn platform_cfg(&self, label: &TargetLabel) -> bz_error::Result<ConfigurationData> {
        match self.platform_cfgs.get(label) {
            Some(configuration) => Ok(configuration.dupe()),
            None => Err(PlatformConfigurationError::UnknownPlatformTarget(label.dupe()).into()),
        }
    }

    fn resolved_transitions(
        &self,
    ) -> bz_error::Result<&OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>>> {
        Ok(self.resolved_transitions)
    }
}
