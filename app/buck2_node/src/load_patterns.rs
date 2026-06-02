/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_common::bazel::skyframe::BazelSkyframeFunction;
use buck2_common::bazel::skyframe::mark_bazel_skyframe_key;
use buck2_common::file_ops::trait_::DiceFileOps;
use buck2_common::file_ops::trait_::FileOps;
use buck2_common::pattern::package_roots::collect_package_roots;
use buck2_common::pattern::resolve::ResolvedPattern;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::package::PackageLabel;
use buck2_core::package::PackageLabelWithModifiers;
use buck2_core::pattern::pattern::Modifiers;
use buck2_core::pattern::pattern::PackageSpec;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern::ParsedPatternWithModifiers;
use buck2_core::pattern::pattern_type::ConfiguredProvidersPatternExtra;
use buck2_core::pattern::pattern_type::ConfiguredTargetPatternExtra;
use buck2_core::pattern::pattern_type::PatternType;
use buck2_core::pattern::pattern_type::ProvidersPatternExtra;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::target::name::TargetName;
use buck2_events::dispatch::console_message;
use dice::DiceComputations;
use dice::Key;
use dice::NoValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::FutureExt;
use itertools::Itertools;
use pagable::Pagable;
use pagable::pagable_typetag;
use pagable::typetag::PagableTagged;

use crate::nodes::eval_result::EvaluationResult;
use crate::nodes::frontend::TargetGraphCalculation;
use crate::nodes::unconfigured::TargetNode;
use crate::nodes::unconfigured::TargetNodeRef;
use crate::super_package::SuperPackage;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct IgnoredSubdirectoriesKey(CellPath);

impl fmt::Display for IgnoredSubdirectoriesKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IGNORED_SUBDIRECTORIES({})", self.0)
    }
}

#[async_trait]
impl Key for IgnoredSubdirectoriesKey {
    type Value = buck2_error::Result<bool>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let root = self.0.clone();
        ctx.with_linear_recompute(|ctx| async move {
            Ok(DiceFileOps(&ctx)
                .is_ignored(root.as_ref())
                .await?
                .is_ignored())
        })
        .await
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct RecursivePkgKey(CellPath);

impl fmt::Display for RecursivePkgKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RECURSIVE_PKG({})", self.0)
    }
}

#[async_trait]
impl Key for RecursivePkgKey {
    type Value = buck2_error::Result<Arc<Vec<PackageLabel>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        if ctx
            .compute(&IgnoredSubdirectoriesKey(self.0.clone()))
            .await??
        {
            return Ok(Arc::new(Vec::new()));
        }

        let root = self.0.clone();
        ctx.with_linear_recompute(|ctx| async move {
            let mut packages = Vec::new();
            collect_package_roots(&DiceFileOps(&ctx), vec![root], |package| {
                packages.push(package?);
                buck2_error::Ok(())
            })
            .await?;
            packages.sort();
            packages.dedup();
            Ok(Arc::new(packages))
        })
        .await
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct PrepareDepsOfTargetsUnderDirectoryKey(CellPath);

impl fmt::Display for PrepareDepsOfTargetsUnderDirectoryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PREPARE_DEPS_OF_TARGETS_UNDER_DIRECTORY({})", self.0)
    }
}

#[async_trait]
impl Key for PrepareDepsOfTargetsUnderDirectoryKey {
    type Value = buck2_error::Result<Arc<Vec<PackageLabel>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        ctx.compute(&RecursivePkgKey(self.0.clone())).await?
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct CollectPackagesUnderDirectoryKey(CellPath);

impl fmt::Display for CollectPackagesUnderDirectoryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "COLLECT_PACKAGES_UNDER_DIRECTORY({})", self.0)
    }
}

#[async_trait]
impl Key for CollectPackagesUnderDirectoryKey {
    type Value = buck2_error::Result<Arc<Vec<PackageLabel>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        ctx.compute(&RecursivePkgKey(self.0.clone())).await?
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
struct PrepareDepsOfPatternKey<T: PatternType> {
    pattern: ParsedPatternWithModifiers<T>,
}

impl<T: PatternType> PagableTagged for PrepareDepsOfPatternKey<T> {
    fn pagable_type_tag(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

impl<T: PatternType> fmt::Display for PrepareDepsOfPatternKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PREPARE_DEPS_OF_PATTERN({})",
            self.pattern.parsed_pattern
        )
    }
}

#[async_trait]
impl<T: PatternType> Key for PrepareDepsOfPatternKey<T> {
    type Value = buck2_error::Result<Arc<Vec<PackageLabel>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        match &self.pattern.parsed_pattern {
            ParsedPattern::Target(package, ..) | ParsedPattern::Package(package) => {
                Ok(Arc::new(vec![package.dupe()]))
            }
            ParsedPattern::Recursive(cell_path) => {
                ctx.compute(&PrepareDepsOfTargetsUnderDirectoryKey(cell_path.clone()))
                    .await??;
                ctx.compute(&CollectPackagesUnderDirectoryKey(cell_path.clone()))
                    .await?
            }
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
struct PrepareDepsOfPatternsKey<T: PatternType> {
    patterns: Vec<ParsedPatternWithModifiers<T>>,
}

impl<T: PatternType> PagableTagged for PrepareDepsOfPatternsKey<T> {
    fn pagable_type_tag(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

impl<T: PatternType> fmt::Display for PrepareDepsOfPatternsKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PREPARE_DEPS_OF_PATTERNS({} patterns)",
            self.patterns.len()
        )
    }
}

#[async_trait]
impl<T: PatternType> Key for PrepareDepsOfPatternsKey<T> {
    type Value = buck2_error::Result<Arc<Vec<PackageLabel>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let prepared = ctx
            .compute_join(self.patterns.iter(), |ctx, pattern| {
                async move {
                    let key = PrepareDepsOfPatternKey {
                        pattern: pattern.clone(),
                    };
                    ctx.compute(&key).await?
                }
                .boxed()
            })
            .await;

        let mut packages = BTreeSet::new();
        for result in prepared {
            packages.extend(result?.iter().copied());
        }
        Ok(Arc::new(packages.into_iter().collect()))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
struct TargetPatternKey<T: PatternType> {
    pattern: ParsedPatternWithModifiers<T>,
}

impl<T: PatternType> PagableTagged for TargetPatternKey<T> {
    fn pagable_type_tag(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

impl<T: PatternType> fmt::Display for TargetPatternKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TARGET_PATTERN({})", self.pattern.parsed_pattern)
    }
}

#[async_trait]
impl<T: PatternType> Key for TargetPatternKey<T> {
    type Value = buck2_error::Result<Arc<ResolvedPattern<T>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let mut resolved = ResolvedPattern::new();
        let modifiers = self.pattern.modifiers.dupe();
        match &self.pattern.parsed_pattern {
            ParsedPattern::Target(package, target_name, extra) => {
                resolved.add_target(
                    package.dupe(),
                    target_name.clone(),
                    extra.clone(),
                    modifiers,
                );
            }
            ParsedPattern::Package(package) => {
                resolved.add_package(package.dupe(), modifiers);
            }
            ParsedPattern::Recursive(_) => {
                let packages = ctx
                    .compute(&PrepareDepsOfPatternKey {
                        pattern: self.pattern.clone(),
                    })
                    .await??;
                for package in packages.iter() {
                    resolved.add_package(package.dupe(), modifiers.dupe());
                }
            }
        }
        Ok(Arc::new(resolved))
    }

    fn equality(_: &Self::Value, _: &Self::Value) -> bool {
        false
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct TargetPatternErrorKey {
    message: String,
}

impl fmt::Display for TargetPatternErrorKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TARGET_PATTERN_ERROR({})", self.message)
    }
}

#[async_trait]
impl Key for TargetPatternErrorKey {
    type Value = buck2_error::Result<()>;

    async fn compute(
        &self,
        _ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "{}",
            self.message
        ))
    }

    fn equality(_: &Self::Value, _: &Self::Value) -> bool {
        false
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
struct CollectTargetsInPackageKey<T: PatternType> {
    package: PackageLabelWithModifiers,
    spec: PackageSpec<T>,
    skip_missing_targets: MissingTargetBehavior,
}

impl<T: PatternType> PagableTagged for CollectTargetsInPackageKey<T> {
    fn pagable_type_tag(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

impl<T: PatternType> fmt::Display for CollectTargetsInPackageKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "COLLECT_TARGETS_IN_PACKAGE({})", self.package.package)
    }
}

#[async_trait]
impl<T: PatternType> Key for CollectTargetsInPackageKey<T> {
    type Value = buck2_error::Result<Arc<PackageLoadedPatterns<T>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        let package_result = ctx
            .get_interpreter_results(self.package.package.dupe())
            .await?;

        Ok(Arc::new(collect_targets_in_package(
            self.package.dupe(),
            &package_result,
            self.spec.clone(),
            self.skip_missing_targets,
        )?))
    }

    fn equality(_: &Self::Value, _: &Self::Value) -> bool {
        false
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
struct TargetPatternPhaseKey<T: PatternType> {
    patterns: Vec<ParsedPatternWithModifiers<T>>,
    skip_missing_targets: MissingTargetBehavior,
}

impl<T: PatternType> PagableTagged for TargetPatternPhaseKey<T> {
    fn pagable_type_tag(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

pagable::register_typetag!(PrepareDepsOfPatternKey<TargetPatternExtra> as dyn dice::DiceKeyDyn);
pagable::register_typetag!(PrepareDepsOfPatternKey<ProvidersPatternExtra> as dyn dice::DiceKeyDyn);
pagable::register_typetag!(
    PrepareDepsOfPatternKey<ConfiguredTargetPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(
    PrepareDepsOfPatternKey<ConfiguredProvidersPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(PrepareDepsOfPatternsKey<TargetPatternExtra> as dyn dice::DiceKeyDyn);
pagable::register_typetag!(PrepareDepsOfPatternsKey<ProvidersPatternExtra> as dyn dice::DiceKeyDyn);
pagable::register_typetag!(
    PrepareDepsOfPatternsKey<ConfiguredTargetPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(
    PrepareDepsOfPatternsKey<ConfiguredProvidersPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(TargetPatternKey<TargetPatternExtra> as dyn dice::DiceKeyDyn);
pagable::register_typetag!(TargetPatternKey<ProvidersPatternExtra> as dyn dice::DiceKeyDyn);
pagable::register_typetag!(
    TargetPatternKey<ConfiguredTargetPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(
    TargetPatternKey<ConfiguredProvidersPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(CollectTargetsInPackageKey<TargetPatternExtra> as dyn dice::DiceKeyDyn);
pagable::register_typetag!(
    CollectTargetsInPackageKey<ProvidersPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(
    CollectTargetsInPackageKey<ConfiguredTargetPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(
    CollectTargetsInPackageKey<ConfiguredProvidersPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(TargetPatternPhaseKey<TargetPatternExtra> as dyn dice::DiceKeyDyn);
pagable::register_typetag!(TargetPatternPhaseKey<ProvidersPatternExtra> as dyn dice::DiceKeyDyn);
pagable::register_typetag!(
    TargetPatternPhaseKey<ConfiguredTargetPatternExtra> as dyn dice::DiceKeyDyn
);
pagable::register_typetag!(
    TargetPatternPhaseKey<ConfiguredProvidersPatternExtra> as dyn dice::DiceKeyDyn
);

impl<T: PatternType> fmt::Display for TargetPatternPhaseKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TARGET_PATTERN_PHASE({} patterns)", self.patterns.len())
    }
}

#[async_trait]
impl<T: PatternType> Key for TargetPatternPhaseKey<T> {
    type Value = buck2_error::Result<Arc<LoadedPatterns<T>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        mark_bazel_skyframe_key(ctx, BazelSkyframeFunction::PrepareAnalysisPhase).await?;

        let _prepared_packages = ctx
            .compute(&PrepareDepsOfPatternsKey {
                patterns: self.patterns.clone(),
            })
            .await??;

        let resolved_patterns = ctx
            .compute_join(self.patterns.iter(), |ctx, pattern| {
                async move {
                    let key = TargetPatternKey {
                        pattern: pattern.clone(),
                    };
                    ctx.compute(&key).await?
                }
                .boxed()
            })
            .await;

        let mut spec = ResolvedPattern::new();
        for resolved in resolved_patterns {
            merge_resolved_pattern(&mut spec, (*resolved?).clone());
        }

        let collect_results = ctx
            .compute_join(spec.specs.iter(), |ctx, (package, package_spec)| {
                async move {
                    let key = CollectTargetsInPackageKey {
                        package: package.dupe(),
                        spec: package_spec.clone(),
                        skip_missing_targets: self.skip_missing_targets,
                    };
                    let result = ctx.compute(&key).await?;
                    Ok::<_, buck2_error::Error>((package.dupe(), result))
                }
                .boxed()
            })
            .await;

        let mut results = BTreeMap::new();
        for result in collect_results {
            let (package, result) = result?;
            results.insert(package, result.map(|v| (*v).clone()));
        }

        Ok(Arc::new(LoadedPatterns { results }))
    }

    fn equality(_: &Self::Value, _: &Self::Value) -> bool {
        false
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

fn merge_resolved_pattern<T: PatternType>(
    destination: &mut ResolvedPattern<T>,
    source: ResolvedPattern<T>,
) {
    for (package_with_modifiers, spec) in source.specs {
        match spec {
            PackageSpec::Targets(targets) => {
                for (target_name, extra) in targets {
                    destination.add_target(
                        package_with_modifiers.package.dupe(),
                        target_name,
                        extra,
                        package_with_modifiers.modifiers.dupe(),
                    );
                }
            }
            PackageSpec::All() => {
                destination.add_package(
                    package_with_modifiers.package,
                    package_with_modifiers.modifiers,
                );
            }
        }
    }
}

#[derive(Clone, Allocative)]
pub struct LoadedPatterns<T: PatternType> {
    results: BTreeMap<PackageLabelWithModifiers, buck2_error::Result<PackageLoadedPatterns<T>>>,
}

#[derive(Clone, Allocative)]
pub struct PackageLoadedPatterns<T: PatternType> {
    targets: BTreeMap<(TargetName, T), TargetNode>,
    super_package: SuperPackage,
}

impl<T: PatternType> PackageLoadedPatterns<T> {
    pub fn iter(&self) -> impl Iterator<Item = (&(TargetName, T), TargetNodeRef<'_>)> {
        self.targets.iter().map(|(k, v)| (k, v.as_ref()))
    }

    pub fn keys(&self) -> impl Iterator<Item = &(TargetName, T)> {
        self.targets.keys()
    }

    pub fn values(&self) -> impl Iterator<Item = TargetNodeRef<'_>> {
        self.targets.values().map(|v| v.as_ref())
    }

    pub fn into_values(self) -> impl Iterator<Item = TargetNode> {
        self.targets.into_values()
    }

    pub fn super_package(&self) -> &SuperPackage {
        &self.super_package
    }
}

impl<T: PatternType> IntoIterator for PackageLoadedPatterns<T> {
    type Item = ((TargetName, T), TargetNode);
    type IntoIter = std::collections::btree_map::IntoIter<(TargetName, T), TargetNode>;

    fn into_iter(self) -> Self::IntoIter {
        self.targets.into_iter()
    }
}

impl<T: PatternType> LoadedPatterns<T> {
    pub fn iter(
        &self,
    ) -> impl Iterator<
        Item = (
            PackageLabelWithModifiers,
            &buck2_error::Result<PackageLoadedPatterns<T>>,
        ),
    > {
        self.results.iter().map(|(k, v)| (k.dupe(), v))
    }

    // Implementing IntoIterator requires explicitly specifying the iterator type, which seems higher cost than the value of doing it.
    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(
        self,
    ) -> impl Iterator<
        Item = (
            PackageLabelWithModifiers,
            buck2_error::Result<PackageLoadedPatterns<T>>,
        ),
    > {
        self.results.into_iter()
    }

    pub fn iter_loaded_targets(
        &self,
    ) -> impl Iterator<Item = buck2_error::Result<TargetNodeRef<'_>>> {
        self.results
            .values()
            .map(|result| match result {
                Ok(pkg) => Ok(pkg.targets.values().map(|t| t.as_ref())),
                Err(e) => Err(e.dupe()),
            })
            .flatten_ok()
    }

    pub fn iter_loaded_targets_by_package(
        &self,
    ) -> impl Iterator<
        Item = (
            PackageLabelWithModifiers,
            buck2_error::Result<Vec<TargetNode>>,
        ),
    > + '_ {
        self.results.iter().map(|(package, result)| {
            let targets = result
                .as_ref()
                .map(|pkg| pkg.targets.values().map(|t| t.dupe()).collect::<Vec<_>>())
                .map_err(|e| e.dupe());
            (package.dupe(), targets)
        })
    }
}

/// Option to skip missing targets instead of failing.
/// This is not a good option to use long term, but we need it now to deal with our legacy setup.
#[derive(Clone, Dupe, Copy, Eq, PartialEq, Hash, Debug, Allocative, Pagable)]
pub enum MissingTargetBehavior {
    /// Skip missing targets (but error on missing packages or evaluation errors).
    /// When skipping, we emit a warning to the console.
    Warn,
    Fail,
}

impl MissingTargetBehavior {
    pub fn from_skip(skip: bool) -> MissingTargetBehavior {
        if skip {
            MissingTargetBehavior::Warn
        } else {
            MissingTargetBehavior::Fail
        }
    }
}

/// Finds the requested targets in one package.
fn collect_targets_in_package<T: PatternType>(
    _package_with_modifiers: PackageLabelWithModifiers,
    res: &EvaluationResult,
    pkg_spec: PackageSpec<T>,
    skip_missing_targets: MissingTargetBehavior,
) -> buck2_error::Result<PackageLoadedPatterns<T>> {
    let (label_to_node, missing) = res.apply_spec(pkg_spec);
    if let Some(missing) = missing {
        match skip_missing_targets {
            MissingTargetBehavior::Fail => {
                return Err(missing.into_first_error().into());
            }
            MissingTargetBehavior::Warn => console_message(missing.missing_targets_warning()),
        }
    };

    Ok(PackageLoadedPatterns {
        targets: label_to_node,
        super_package: res.super_package().dupe(),
    })
}

pub async fn load_patterns<T: PatternType>(
    ctx: &mut DiceComputations<'_>,
    parsed_patterns: Vec<ParsedPattern<T>>,
    skip_missing_targets: MissingTargetBehavior,
) -> buck2_error::Result<LoadedPatterns<T>> {
    let patterns = parsed_patterns
        .into_iter()
        .map(|parsed_pattern| ParsedPatternWithModifiers {
            parsed_pattern,
            modifiers: Modifiers::new(None),
        })
        .collect();
    load_patterns_with_modifiers(ctx, patterns, skip_missing_targets).await
}

pub async fn load_patterns_with_modifiers<T: PatternType>(
    ctx: &mut DiceComputations<'_>,
    parsed_patterns: Vec<ParsedPatternWithModifiers<T>>,
    skip_missing_targets: MissingTargetBehavior,
) -> buck2_error::Result<LoadedPatterns<T>> {
    let result = ctx
        .compute(&TargetPatternPhaseKey {
            patterns: parsed_patterns,
            skip_missing_targets,
        })
        .await??;
    Ok((*result).clone())
}
