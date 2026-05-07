/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cell::Ref;
use std::cell::RefCell;
use std::cell::RefMut;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Debug;
use std::mem;
use std::sync::Arc;

use buck2_common::package_listing::listing::PackageListing;
use buck2_core::build_file_path::BuildFilePath;
use buck2_core::bzl::ImportPath;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::paths::CellRelativePathBuf;
use buck2_core::package::PackageLabel;
use buck2_core::package::package_relative_path::PackageRelativePath;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::provider::label::ProvidersName;
use buck2_core::target::name::TargetName;
use buck2_core::target::name::TargetNameRef;
use buck2_events::dispatch::console_message;
use buck2_interpreter::package_imports::ImplicitImport;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::attrs::traversal::CoercedAttrTraversal;
use buck2_node::nodes::eval_result::EvaluationResult;
use buck2_node::nodes::targets_map::TargetsMap;
use buck2_node::nodes::targets_map::TargetsMapRecordError;
use buck2_node::nodes::unconfigured::TargetNode;
use buck2_node::nodes::unconfigured::TargetNodeRef;
use buck2_node::oncall::Oncall;
use buck2_node::package::Package;
use buck2_node::super_package::SuperPackage;
use buck2_node::visibility::VisibilitySpecification;
use dupe::Dupe;
use starlark::environment::FrozenModule;
use starlark::values::OwnedFrozenValue;

use crate::attrs::coerce::ctx::BuildAttrCoercionContext;
use crate::interpreter::globspec::GlobSpec;
use crate::nodes::unconfigured::bazel_input_file_target;

impl From<ModuleInternals> for EvaluationResult {
    // TODO(cjhopman): Let's make this an `into_evaluation_result()` on ModuleInternals instead.
    fn from(internals: ModuleInternals) -> Self {
        let ModuleInternals {
            state,
            imports,
            buildfile_path,
            super_package,
            package_listing,
            ..
        } = internals;
        let (recorder, package) = match state.into_inner() {
            State::BeforeTargets(_) => (TargetsRecorder::new(), None),
            State::RecordingTargets(RecordingTargets { recorder, package }) => {
                populate_bazel_package_groups(&package, &recorder);
                (recorder, Some(package))
            }
        };
        let super_package = super_package.into_inner();
        let mut targets = recorder.take();
        if let Some(package) = package {
            populate_bazel_input_file_targets(
                &package,
                &buildfile_path,
                &package_listing,
                &super_package,
                &mut targets,
            );
        }
        EvaluationResult::new(buildfile_path, imports, super_package, targets)
    }
}

fn populate_bazel_package_groups(package: &Arc<Package>, recorder: &TargetsRecorder) {
    let mut package_groups = BTreeMap::new();
    for (_, node) in recorder.targets.iter() {
        if node.rule_type().name() != "bazel_package_group" {
            continue;
        }
        if let Ok(patterns) = collect_bazel_package_group_patterns(package, node) {
            package_groups.insert(node.label().name().to_owned(), patterns);
        }
    }
    let _ = package.package_groups.set(package_groups);
}

fn is_bazel_compat_build_file(buildfile_path: &BuildFilePath) -> bool {
    let cell = buildfile_path.cell();
    let cell = cell.as_str();
    let filename = buildfile_path.filename().as_str();
    (cell == "root" || cell == "bazel_tools" || cell.starts_with("bzlmod_"))
        && (filename == "BUILD" || filename == "BUILD.bazel")
}

fn populate_bazel_input_file_targets(
    package: &Arc<Package>,
    buildfile_path: &BuildFilePath,
    package_listing: &PackageListing,
    super_package: &SuperPackage,
    targets: &mut TargetsMap,
) {
    if !is_bazel_compat_build_file(buildfile_path) {
        return;
    }

    let mut collector = BazelInputFileLabelCollector {
        package: buildfile_path.package(),
        labels: BTreeSet::new(),
    };
    for target in targets.values() {
        for attr in target.attrs(AttrInspectOptions::All) {
            let _ = attr.traverse(buildfile_path.package(), &mut collector);
        }
    }

    for name in collector.labels {
        let name = name.as_ref();
        if targets.contains_key(name) {
            continue;
        }
        let Ok(path) = PackageRelativePath::new(name.as_str()) else {
            continue;
        };
        // Shallow Bazel package listings include top-level files only. After
        // all rule targets have been registered, a remaining slashy label is
        // eligible to be a nested source file label.
        if package_listing.get_file(path).is_none()
            && package_listing.get_dir(path).is_none()
            && !name.as_str().contains('/')
        {
            continue;
        }
        let target = bazel_input_file_target(package.dupe(), name, buildfile_path, super_package)
            .expect("constructing Bazel input-file target should be infallible");
        targets
            .record(target)
            .expect("Bazel input-file target was checked to be absent");
    }
}

struct BazelInputFileLabelCollector {
    package: PackageLabel,
    labels: BTreeSet<TargetName>,
}

impl<'a> CoercedAttrTraversal<'a> for BazelInputFileLabelCollector {
    fn dep(&mut self, dep: &ProvidersLabel) -> buck2_error::Result<()> {
        self.collect(dep);
        Ok(())
    }

    fn configuration_dep(
        &mut self,
        _dep: &ProvidersLabel,
        _kind: buck2_node::attrs::attr_type::configuration_dep::ConfigurationDepKind,
    ) -> buck2_error::Result<()> {
        Ok(())
    }

    fn plugin_dep(
        &mut self,
        _dep: &'a buck2_core::target::label::label::TargetLabel,
        _kind: &buck2_core::plugins::PluginKind,
    ) -> buck2_error::Result<()> {
        Ok(())
    }

    fn input(
        &mut self,
        input: buck2_core::package::source_path::SourcePathRef,
    ) -> buck2_error::Result<()> {
        if input.package() == self.package
            && let Ok(name) = TargetName::new(input.path().as_str())
        {
            self.labels.insert(name.to_owned());
        }
        Ok(())
    }

    fn label(&mut self, label: &'a ProvidersLabel) -> buck2_error::Result<()> {
        self.collect(label);
        Ok(())
    }
}

impl BazelInputFileLabelCollector {
    fn collect(&mut self, label: &ProvidersLabel) {
        if label.target().pkg() == self.package && matches!(label.name(), ProvidersName::Default) {
            self.labels.insert(label.target().name().to_owned());
        }
    }
}

fn collect_bazel_package_group_patterns(
    package: &Package,
    node: TargetNodeRef<'_>,
) -> buck2_error::Result<Vec<ParsedPattern<TargetPatternExtra>>> {
    let mut patterns = Vec::new();
    for package_spec in string_list_attr(node, "packages")? {
        if let Some(pattern) = parse_bazel_package_group_spec(package, &package_spec)? {
            patterns.push(pattern);
        }
    }
    Ok(patterns)
}

fn string_list_attr(node: TargetNodeRef<'_>, attr: &str) -> buck2_error::Result<Vec<String>> {
    let Some(attr) = node.attr_or_none(attr, AttrInspectOptions::All) else {
        return Ok(Vec::new());
    };
    match attr.value {
        CoercedAttr::List(values) => values
            .iter()
            .map(|value| match value {
                CoercedAttr::String(value) => Ok(value.0.to_string()),
                value => Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Expected package_group `{}` item to be a string, got `{:?}`",
                    attr.name,
                    value
                )),
            })
            .collect(),
        value => Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "Expected package_group `{}` to be a string list, got `{:?}`",
            attr.name,
            value
        )),
    }
}

fn parse_bazel_package_group_spec(
    package: &Package,
    spec: &str,
) -> buck2_error::Result<Option<ParsedPattern<TargetPatternExtra>>> {
    if spec.starts_with('-') {
        return Ok(None);
    }

    let Some(package_path) = spec.strip_prefix("//") else {
        return Ok(None);
    };
    let cell = package.buildfile_path.cell();

    if package_path == "..." {
        return Ok(Some(ParsedPattern::Recursive(CellPath::new(
            cell,
            CellRelativePathBuf::try_from(String::new())?,
        ))));
    }

    if let Some(package_path) = package_path.strip_suffix("/...") {
        return Ok(Some(ParsedPattern::Recursive(CellPath::new(
            cell,
            CellRelativePathBuf::try_from(package_path.to_owned())?,
        ))));
    }

    Ok(Some(ParsedPattern::Package(PackageLabel::new(
        cell,
        CellRelativePathBuf::try_from(package_path.to_owned())?.as_ref(),
    )?)))
}

#[derive(Debug, Default)]
struct BeforeTargets {
    oncall: Option<Oncall>,
    has_read_oncall: bool,
}

#[derive(Debug)]
struct RecordingTargets {
    package: Arc<Package>,
    recorder: TargetsRecorder,
}

#[derive(Debug)]
enum State {
    /// No targets recorded yet, `oncall` call is allowed unless it was already called.
    BeforeTargets(BeforeTargets),
    /// First target seen.
    RecordingTargets(RecordingTargets),
}

/// ModuleInternals contains the module/package-specific information for
/// evaluating build files. Built-in functions that need access to
/// package-specific information or objects can get them by acquiring the
/// ModuleInternals.
#[derive(Debug)]
pub struct ModuleInternals {
    attr_coercion_context: BuildAttrCoercionContext,
    buildfile_path: Arc<BuildFilePath>,
    /// Have you seen an oncall annotation yet
    state: RefCell<State>,
    /// Directly imported modules.
    imports: Vec<ImportPath>,
    package_implicits: Option<PackageImplicits>,
    record_target_call_stacks: bool,
    skip_targets_with_duplicate_names: bool,
    /// The files owned by this directory. Is `None` for .bzl files.
    package_listing: PackageListing,
    super_package: RefCell<SuperPackage>,
    bazel_package_declared: RefCell<bool>,
}

#[derive(Debug)]
pub(crate) struct PackageImplicits {
    import_spec: Arc<ImplicitImport>,
    env: FrozenModule,
}

impl PackageImplicits {
    pub(crate) fn new(import_spec: Arc<ImplicitImport>, env: FrozenModule) -> Self {
        Self { import_spec, env }
    }

    fn lookup(&self, name: &str) -> Option<OwnedFrozenValue> {
        self.env
            .get_option(self.import_spec.lookup_alias(name))
            .ok()
            .flatten()
    }
}

#[derive(Debug, buck2_error::Error)]
#[buck2(input)]
enum OncallErrors {
    #[error("Called `oncall` after one or more targets were declared, `oncall` must be first.")]
    OncallAfterTargets,
    #[error("Called `oncall` more than once in the file.")]
    DuplicateOncall,
    #[error("Called `oncall` after calling `read_oncall`, `oncall` must be first.")]
    AfterReadOncall,
}

#[derive(Debug, buck2_error::Error)]
#[buck2(input)]
enum BazelPackageError {
    #[error("'package' can only be used once per BUILD file")]
    AtMostOnce,
    #[error("package() must be called before targets are declared")]
    AfterTargets,
}

impl ModuleInternals {
    pub(crate) fn new(
        attr_coercion_context: BuildAttrCoercionContext,
        buildfile_path: Arc<BuildFilePath>,
        imports: Vec<ImportPath>,
        package_implicits: Option<PackageImplicits>,
        record_target_call_stacks: bool,
        skip_targets_with_duplicate_names: bool,
        package_listing: PackageListing,
        super_package: SuperPackage,
    ) -> Self {
        Self {
            attr_coercion_context,
            buildfile_path,
            state: RefCell::new(State::BeforeTargets(BeforeTargets::default())),
            imports,
            package_implicits,
            record_target_call_stacks,
            skip_targets_with_duplicate_names,
            package_listing,
            super_package: RefCell::new(super_package),
            bazel_package_declared: RefCell::new(false),
        }
    }

    pub(crate) fn attr_coercion_context(&self) -> &BuildAttrCoercionContext {
        &self.attr_coercion_context
    }

    pub fn record(&self, target_node: TargetNode) -> buck2_error::Result<()> {
        match self.recording_targets().recorder.record(target_node) {
            Ok(()) => Ok(()),
            Err(e @ TargetsMapRecordError::RegisteredTargetTwice { .. }) => {
                if self.skip_targets_with_duplicate_names {
                    console_message(e.to_string());
                    Ok(())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    pub(crate) fn set_oncall(&self, name: &str) -> buck2_error::Result<()> {
        match &mut *self.state.borrow_mut() {
            State::BeforeTargets(x) => match x.oncall {
                _ if x.has_read_oncall => Err(OncallErrors::AfterReadOncall.into()),
                Some(_) => Err(OncallErrors::DuplicateOncall.into()),
                None => {
                    x.oncall = Some(Oncall::new(name));
                    Ok(())
                }
            },
            State::RecordingTargets(..) => {
                // We require oncall to be first both so users can find it,
                // and so we can propagate it to all targets more easily.
                Err(OncallErrors::OncallAfterTargets.into())
            }
        }
    }

    pub(crate) fn get_oncall(&self) -> Option<Oncall> {
        match &mut *self.state.borrow_mut() {
            State::BeforeTargets(x) => {
                x.has_read_oncall = true;
                x.oncall.dupe()
            }
            State::RecordingTargets(t) => t.package.oncall.dupe(),
        }
    }

    fn recording_targets(&self) -> RefMut<'_, RecordingTargets> {
        RefMut::map(self.state.borrow_mut(), |state| {
            loop {
                match state {
                    State::BeforeTargets(BeforeTargets { oncall, .. }) => {
                        let oncall = mem::take(oncall);
                        *state = State::RecordingTargets(RecordingTargets {
                            package: Arc::new(Package {
                                buildfile_path: self.buildfile_path.dupe(),
                                oncall,
                                package_groups: Arc::default(),
                            }),
                            recorder: TargetsRecorder::new(),
                        });
                    }
                    State::RecordingTargets(r) => return r,
                }
            }
        })
    }

    pub(crate) fn target_exists(&self, name: &str) -> bool {
        self.recording_targets()
            .recorder
            .targets
            .contains_key(TargetNameRef::unchecked_new(name))
    }

    pub fn buildfile_path(&self) -> &Arc<BuildFilePath> {
        &self.buildfile_path
    }

    pub(crate) fn super_package(&self) -> Ref<'_, SuperPackage> {
        self.super_package.borrow()
    }

    pub(crate) fn set_bazel_package_default_visibility(
        &self,
        visibility: VisibilitySpecification,
    ) -> buck2_error::Result<()> {
        let mut declared = self.bazel_package_declared.borrow_mut();
        if *declared {
            return Err(BazelPackageError::AtMostOnce.into());
        }
        *declared = true;

        if matches!(&*self.state.borrow(), State::RecordingTargets(_)) {
            return Err(BazelPackageError::AfterTargets.into());
        }

        let current = self.super_package.borrow();
        let next = SuperPackage::new(
            current.package_values().clone(),
            visibility,
            current.within_view().to_owned(),
            current.cfg_constructor().cloned(),
            current.test_config_unification_rollout(),
        )?;
        drop(current);
        *self.super_package.borrow_mut() = next;
        Ok(())
    }

    pub fn package(&self) -> Arc<Package> {
        self.recording_targets().package.dupe()
    }

    pub(crate) fn get_package_implicit(&self, name: &str) -> Option<OwnedFrozenValue> {
        self.package_implicits
            .as_ref()
            .and_then(|implicits| implicits.lookup(name))
    }

    pub fn record_target_call_stacks(&self) -> bool {
        self.record_target_call_stacks
    }

    pub(crate) fn resolve_glob<'a>(
        &'a self,
        spec: &'a GlobSpec,
        include_directories: bool,
    ) -> Vec<&'a PackageRelativePath> {
        let mut matches = spec
            .resolve_glob(self.package_listing.files())
            .collect::<Vec<_>>();
        if include_directories {
            matches.extend(
                self.package_listing
                    .dirs()
                    .filter(|path| spec.matches(path.as_str())),
            );
            matches.sort();
        }
        matches
    }

    pub(crate) fn sub_packages(&self) -> impl Iterator<Item = &PackageRelativePath> {
        self.package_listing
            .subpackages_within(PackageRelativePath::empty())
    }
}

// Records the targets declared when evaluating a build file.
struct TargetsRecorder {
    targets: TargetsMap,
}

impl Debug for TargetsRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetsRecorder").finish_non_exhaustive()
    }
}

impl TargetsRecorder {
    fn new() -> Self {
        Self {
            targets: TargetsMap::new(),
        }
    }

    fn record(&mut self, target_node: TargetNode) -> Result<(), TargetsMapRecordError> {
        self.targets.record(target_node)
    }

    fn take(self) -> TargetsMap {
        self.targets
    }
}
