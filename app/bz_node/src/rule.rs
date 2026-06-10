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

use allocative::Allocative;
use bz_core::configuration::transition::id::TransitionId;
use bz_core::plugins::PluginKind;
#[allow(unused_imports)]
use bz_hash::BuckHasher;
use bz_util::arc_str::ArcStr;
use pagable::Pagable;
use static_interner::interner;

use crate::attrs::spec::AttributeSpec;
use crate::nodes::unconfigured::RuleKind;
use crate::rule_type::RuleType;

pub const BAZEL_OUTPUT_FILE_GENERATING_RULE_ATTR: &str = "generating_rule";
pub const BAZEL_OUTPUT_FILE_OUTPUT_ATTR: &str = "output";

#[derive(Debug, Eq, PartialEq, Hash, Pagable, Allocative, Clone, dupe::Dupe)]
pub enum RuleIncomingTransition {
    None,
    Fixed(Arc<TransitionId>),
    /// This rule has an `incoming_transition` attribute
    FromAttribute,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, dupe::Dupe, Pagable, Allocative)]
pub enum BazelOutputAttrKind {
    Output,
    OutputList,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, dupe::Dupe, Pagable, Allocative)]
pub struct BazelOutputAttr {
    pub name: ArcStr,
    pub kind: BazelOutputAttrKind,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, dupe::Dupe, Pagable, Allocative)]
pub struct BazelImplicitOutput {
    pub name: ArcStr,
    pub template: ArcStr,
}

/// Bazel toolchain type declared by `rule(toolchains = ...)`.
#[derive(Debug, Eq, PartialEq, Hash, Clone, Pagable, Allocative)]
pub struct BazelToolchainRequirement {
    pub toolchain_type: String,
    pub mandatory: bool,
}

/// Common rule data needed in `TargetNode`.
#[derive(Debug, Eq, PartialEq, Hash, Pagable, Allocative)]
pub struct Rule {
    /// The attribute spec. This holds the attribute name -> index mapping and the default values
    /// (for those attributes without explicit values).
    pub attributes: AttributeSpec,
    /// The 'type', used to find the implementation function from the graph
    pub rule_type: RuleType,
    /// The kind of rule, e.g. configuration or otherwise.
    pub rule_kind: RuleKind,
    /// Transition to apply to the target.
    pub cfg: RuleIncomingTransition,
    /// The plugin kinds that are used by the target
    pub uses_plugins: Vec<PluginKind>,
    /// Bazel toolchain types declared by `rule(toolchains = ...)`.
    pub bazel_toolchains: Vec<BazelToolchainRequirement>,
    /// Bazel toolchain types declared by aspects attached to this rule's attrs.
    pub bazel_aspect_toolchains: Vec<BazelToolchainRequirement>,
    /// Bazel explicit output attrs declared with `attr.output()` or `attr.output_list()`.
    pub bazel_output_attrs: Vec<BazelOutputAttr>,
    /// Bazel implicit outputs declared with `rule(outputs = {...})`.
    pub bazel_implicit_outputs: Vec<BazelImplicitOutput>,
    /// Whether Bazel output artifacts from this rule are declared under genfiles instead of bin.
    pub bazel_output_to_genfiles: bool,
    /// Whether the rule was declared through Bazel's `rule(implementation = ...)` API.
    pub is_bazel_rule: bool,
    /// Whether the rule was declared through Bazel's `rule(test = True)` API.
    pub is_bazel_test_rule: bool,
    /// Whether the rule was declared through Bazel's `rule(executable = True)` API.
    pub is_bazel_executable_rule: bool,
    /// Whether the rule was declared with Bazel's `build_setting = ...`.
    pub is_bazel_build_setting: bool,
}

interner!(INTERNER, BuckHasher, Rule);
