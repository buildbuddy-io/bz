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
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::OnceLock;

use allocative::Allocative;
use buck2_core::build_file_path::BuildFilePath;
use buck2_core::package::PackageLabel;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::target::label::label::TargetLabel;
use buck2_core::target::name::TargetName;
use pagable::Pagable;

use crate::oncall::Oncall;

pub type PackageGroups = BTreeMap<TargetName, PackageGroup>;

#[derive(Debug, Allocative, Pagable)]
pub struct PackageGroup {
    pub packages: PackageGroupContents,
    pub includes: Vec<TargetLabel>,
}

impl PackageGroup {
    pub fn contains_target(&self, target: &TargetLabel) -> bool {
        self.packages.contains_target(target)
    }

    pub fn contains_package(&self, package: &PackageLabel) -> bool {
        self.packages.contains_package(package)
    }
}

#[derive(Debug, Default, Allocative, Pagable)]
pub struct PackageGroupContents {
    pub positives: Vec<PackageGroupSpec>,
    pub negatives: Vec<PackageGroupSpec>,
}

impl PackageGroupContents {
    pub fn contains_target(&self, target: &TargetLabel) -> bool {
        self.contains_package(&target.pkg())
    }

    pub fn contains_package(&self, package: &PackageLabel) -> bool {
        self.positives
            .iter()
            .any(|spec| spec.matches_package(package))
            && !self
                .negatives
                .iter()
                .any(|spec| spec.matches_package(package))
    }
}

#[derive(Debug, Allocative, Pagable)]
pub enum PackageGroupSpec {
    AllPackages,
    Pattern(ParsedPattern<TargetPatternExtra>),
}

impl PackageGroupSpec {
    fn matches_package(&self, package: &PackageLabel) -> bool {
        match self {
            PackageGroupSpec::AllPackages => true,
            PackageGroupSpec::Pattern(pattern) => package_pattern_matches(pattern, package),
        }
    }
}

fn package_pattern_matches(
    pattern: &ParsedPattern<TargetPatternExtra>,
    package: &PackageLabel,
) -> bool {
    match pattern {
        ParsedPattern::Target(..) => false,
        ParsedPattern::Package(pkg) => pkg == package,
        ParsedPattern::Recursive(cell_path) => {
            package.as_cell_path().starts_with(cell_path.as_ref())
        }
    }
}

/// Package-specific data for `TargetNode`.
///
/// (Note this has nothing to do with `PACKAGE` files which are not implemented
/// at the moment of writing.)
#[derive(Debug, Allocative, Pagable)]
pub struct Package {
    /// The build file which defined this target, e.g. `fbcode//foo/bar/TARGETS`
    pub buildfile_path: Arc<BuildFilePath>,
    /// The oncall attribute, if set
    pub oncall: Option<Oncall>,
    #[allocative(skip)]
    #[pagable(discard = "Default::default()")]
    pub package_groups: Arc<OnceLock<PackageGroups>>,
}

impl PartialEq for Package {
    fn eq(&self, other: &Self) -> bool {
        self.buildfile_path == other.buildfile_path && self.oncall == other.oncall
    }
}

impl Eq for Package {}

impl Hash for Package {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.buildfile_path.hash(state);
        self.oncall.hash(state);
    }
}
