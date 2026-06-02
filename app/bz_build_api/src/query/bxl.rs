/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use bz_core::cells::CellResolver;
use bz_core::cells::name::CellName;
use bz_core::configuration::compatibility::MaybeCompatible;
use bz_core::fs::project::ProjectRoot;
use bz_core::global_cfg_options::GlobalCfgOptions;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_core::target::configured_target_label::ConfiguredTargetLabel;
use bz_node::nodes::configured::ConfiguredTargetNode;
use bz_node::nodes::unconfigured::TargetNode;
use bz_query::query::syntax::simple::eval::file_set::FileSet;
use bz_query::query::syntax::simple::eval::set::TargetSet;
use bz_query::query::syntax::simple::eval::values::QueryValueDepth;
use bz_query::query::syntax::simple::functions::helpers::CapturedExpr;
use bz_util::late_binding::LateBinding;
use dice::DiceComputations;

use crate::actions::query::ActionQueryNode;

#[async_trait]
pub trait BxlCqueryFunctions: Send {
    async fn allpaths(
        &self,
        dice: &mut DiceComputations<'_>,
        from: &TargetSet<ConfiguredTargetNode>,
        to: &TargetSet<ConfiguredTargetNode>,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<ConfiguredTargetNode>>;
    async fn somepath(
        &self,
        dice: &mut DiceComputations<'_>,
        from: &TargetSet<ConfiguredTargetNode>,
        to: &TargetSet<ConfiguredTargetNode>,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<ConfiguredTargetNode>>;
    async fn owner(
        &self,
        dice: &mut DiceComputations<'_>,
        file_set: &FileSet,
        target_universe: Option<&TargetSet<ConfiguredTargetNode>>,
    ) -> bz_error::Result<TargetSet<ConfiguredTargetNode>>;
    async fn deps(
        &self,
        dice: &mut DiceComputations<'_>,
        targets: &TargetSet<ConfiguredTargetNode>,
        depth: QueryValueDepth,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<ConfiguredTargetNode>>;
    async fn rdeps(
        &self,
        dice: &mut DiceComputations<'_>,
        universe: &TargetSet<ConfiguredTargetNode>,
        targets: &TargetSet<ConfiguredTargetNode>,
        depth: QueryValueDepth,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<ConfiguredTargetNode>>;
    async fn testsof(
        &self,
        dice: &mut DiceComputations<'_>,
        targets: &TargetSet<ConfiguredTargetNode>,
    ) -> bz_error::Result<TargetSet<ConfiguredTargetNode>>;
    async fn testsof_with_default_target_platform(
        &self,
        dice: &mut DiceComputations<'_>,
        targets: &TargetSet<ConfiguredTargetNode>,
    ) -> bz_error::Result<Vec<MaybeCompatible<ConfiguredTargetNode>>>;
}

#[async_trait]
pub trait BxlUqueryFunctions: Send {
    async fn allpaths(
        &self,
        dice: &mut DiceComputations<'_>,
        from: &TargetSet<TargetNode>,
        to: &TargetSet<TargetNode>,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<TargetNode>>;
    async fn somepath(
        &self,
        dice: &mut DiceComputations<'_>,
        from: &TargetSet<TargetNode>,
        to: &TargetSet<TargetNode>,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<TargetNode>>;
    async fn deps(
        &self,
        dice: &mut DiceComputations<'_>,
        targets: &TargetSet<TargetNode>,
        depth: QueryValueDepth,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<TargetNode>>;
    async fn rdeps(
        &self,
        dice: &mut DiceComputations<'_>,
        universe: &TargetSet<TargetNode>,
        targets: &TargetSet<TargetNode>,
        depth: QueryValueDepth,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<TargetNode>>;
    async fn testsof(
        &self,
        dice: &mut DiceComputations<'_>,
        targets: &TargetSet<TargetNode>,
    ) -> bz_error::Result<TargetSet<TargetNode>>;
    async fn owner(
        &self,
        dice: &mut DiceComputations<'_>,
        file_set: &FileSet,
    ) -> bz_error::Result<TargetSet<TargetNode>>;
    async fn targets_in_buildfile(
        &self,
        dice: &mut DiceComputations<'_>,
        file_set: &FileSet,
    ) -> bz_error::Result<TargetSet<TargetNode>>;
}

#[async_trait]
pub trait BxlAqueryFunctions: Send {
    async fn allpaths(
        &self,
        dice: &mut DiceComputations<'_>,
        from: &TargetSet<ActionQueryNode>,
        to: &TargetSet<ActionQueryNode>,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<ActionQueryNode>>;
    async fn somepath(
        &self,
        dice: &mut DiceComputations<'_>,
        from: &TargetSet<ActionQueryNode>,
        to: &TargetSet<ActionQueryNode>,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<ActionQueryNode>>;
    async fn deps(
        &self,
        dice: &mut DiceComputations<'_>,
        targets: &TargetSet<ActionQueryNode>,
        depth: QueryValueDepth,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<ActionQueryNode>>;
    async fn rdeps(
        &self,
        dice: &mut DiceComputations<'_>,
        universe: &TargetSet<ActionQueryNode>,
        targets: &TargetSet<ActionQueryNode>,
        depth: QueryValueDepth,
        captured_expr: Option<&CapturedExpr>,
    ) -> bz_error::Result<TargetSet<ActionQueryNode>>;
    async fn testsof(
        &self,
        dice: &mut DiceComputations<'_>,
        targets: &TargetSet<ActionQueryNode>,
    ) -> bz_error::Result<TargetSet<ActionQueryNode>>;
    async fn owner(
        &self,
        dice: &mut DiceComputations<'_>,
        file_set: &FileSet,
    ) -> bz_error::Result<TargetSet<ActionQueryNode>>;
    async fn get_target_set(
        &self,
        dice: &mut DiceComputations<'_>,
        configured_labels: Vec<ConfiguredProvidersLabel>,
    ) -> bz_error::Result<(Vec<ConfiguredTargetLabel>, TargetSet<ActionQueryNode>)>;
    async fn all_outputs(
        &self,
        dice: &mut DiceComputations<'_>,
        targets: &TargetSet<ActionQueryNode>,
    ) -> bz_error::Result<TargetSet<ActionQueryNode>>;
    async fn all_actions(
        &self,
        dice: &mut DiceComputations<'_>,
        targets: &TargetSet<ActionQueryNode>,
    ) -> bz_error::Result<TargetSet<ActionQueryNode>>;
}

pub static NEW_BXL_CQUERY_FUNCTIONS: LateBinding<
    fn(
        // Target configuration info (target platform + cli modifiers)
        GlobalCfgOptions,
        ProjectRoot,
        CellName,
        CellResolver,
    ) -> Pin<Box<dyn Future<Output = bz_error::Result<Box<dyn BxlCqueryFunctions>>>>>,
> = LateBinding::new("NEW_BXL_CQUERY_FUNCTIONS");

pub static NEW_BXL_UQUERY_FUNCTIONS: LateBinding<
    fn(
        ProjectRoot,
        CellName,
        CellResolver,
    )
        -> Pin<Box<dyn Future<Output = bz_error::Result<Box<dyn BxlUqueryFunctions>>> + Send>>,
> = LateBinding::new("NEW_BXL_UQUERY_FUNCTIONS");

pub static NEW_BXL_AQUERY_FUNCTIONS: LateBinding<
    fn(
        // Target configuration info (target platform + cli modifiers)
        GlobalCfgOptions,
        ProjectRoot,
        CellName,
        CellResolver,
    ) -> Pin<Box<dyn Future<Output = bz_error::Result<Box<dyn BxlAqueryFunctions>>>>>,
> = LateBinding::new("NEW_BXL_AQUERY_FUNCTIONS");
