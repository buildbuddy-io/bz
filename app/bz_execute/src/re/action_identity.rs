/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_events::dispatch::get_dispatcher;
use bz_wrapper_common::invocation_id::TraceId;

use crate::execute::request::CommandExecutionPaths;
use crate::execute::target::CommandExecutionTarget;

fn configured_target_label_metadata(
    label: &bz_data::ConfiguredTargetLabel,
) -> (Option<String>, Option<String>) {
    let target_id = label
        .label
        .as_ref()
        .map(|label| format!("{}:{}", label.package, label.name));
    let configuration_id = label
        .configuration
        .as_ref()
        .map(|configuration| configuration.full_name.clone());
    (target_id, configuration_id)
}

fn anon_target_metadata(anon_target: &bz_data::AnonTarget) -> (Option<String>, Option<String>) {
    let target_id = anon_target
        .name
        .as_ref()
        .map(|label| format!("{}:{}@{}", label.package, label.name, anon_target.hash));
    let configuration_id = anon_target
        .execution_configuration
        .as_ref()
        .map(|configuration| configuration.full_name.clone());
    (target_id, configuration_id)
}

fn action_key_metadata(action_key: bz_data::ActionKey) -> (Option<String>, Option<String>) {
    match action_key.owner {
        Some(bz_data::action_key::Owner::TargetLabel(label))
        | Some(bz_data::action_key::Owner::TestTargetLabel(label))
        | Some(bz_data::action_key::Owner::LocalResourceSetup(label)) => {
            configured_target_label_metadata(&label)
        }
        Some(bz_data::action_key::Owner::AnonTarget(anon_target)) => {
            anon_target_metadata(&anon_target)
        }
        Some(bz_data::action_key::Owner::BxlKey(bxl_key)) => {
            let target_id = bxl_key
                .label
                .map(|label| format!("{}:{}", label.bxl_path, label.name));
            (target_id, None)
        }
        None => (None, None),
    }
}

pub struct ReActionIdentity<'a> {
    /// This is currently unused, but historically it has been useful to add logging in the RE
    /// client, so it's worth keeping around.
    _target: &'a dyn CommandExecutionTarget,

    /// Actions with the same action key share e.g. memory requirements learnt by RE.
    pub action_key: String,

    /// Actions with the same affinity key get scheduled on similar hosts.
    pub affinity_key: String,

    /// A stable identifier tying REAPI requests for this action together.
    pub action_id: String,

    /// REAPI RequestMetadata.action_mnemonic.
    pub action_mnemonic: String,

    /// REAPI RequestMetadata.target_id.
    pub target_id: String,

    /// REAPI RequestMetadata.configuration_id.
    pub configuration_id: String,

    /// Details about the action collected while uploading
    pub paths: &'a CommandExecutionPaths,

    //// Trace ID which started the execution of this action, to be added on the RE side
    pub trace_id: TraceId,
}

impl<'a> ReActionIdentity<'a> {
    pub fn new(
        target: &'a dyn CommandExecutionTarget,
        executor_action_key: Option<&str>,
        paths: &'a CommandExecutionPaths,
    ) -> Self {
        let mut action_key = target.re_action_key();
        if let Some(executor_action_key) = executor_action_key {
            action_key = format!("{executor_action_key} {action_key}");
        }

        let action_name = target.as_proto_action_name();
        let (target_id, configuration_id) = action_key_metadata(target.as_proto_action_key());
        let affinity_key = target.re_affinity_key();
        let trace_id = get_dispatcher().trace_id().to_owned();

        Self {
            _target: target,
            action_id: action_key.clone(),
            action_key,
            affinity_key: affinity_key.clone(),
            action_mnemonic: action_name.category,
            target_id: target_id.unwrap_or(affinity_key),
            configuration_id: configuration_id.unwrap_or_default(),
            paths,
            trace_id,
        }
    }
}
