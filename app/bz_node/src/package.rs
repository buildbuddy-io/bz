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
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::OnceLock;

use allocative::Allocative;
use bz_core::build_file_path::BuildFilePath;
use bz_core::package::PackageLabel;
use bz_core::pattern::pattern::ParsedPattern;
use bz_core::pattern::pattern_type::TargetPatternExtra;
use bz_core::target::label::label::TargetLabel;
use bz_core::target::name::TargetName;
use pagable::Pagable;

use crate::oncall::Oncall;

pub type PackageGroups = BTreeMap<TargetName, PackageGroup>;

#[derive(Debug, Allocative, Pagable)]
pub struct PackageGroup {
    pub packages: PackageGroupContents,
    pub includes: Vec<TargetLabel>,
}

impl PackageGroup {
    pub fn contains_target(
        &self,
        target: &TargetLabel,
        group_package: &PackageLabel,
        package_groups: &PackageGroups,
    ) -> bool {
        self.contains_package(&target.pkg(), group_package, package_groups)
    }

    pub fn contains_package(
        &self,
        package: &PackageLabel,
        group_package: &PackageLabel,
        package_groups: &PackageGroups,
    ) -> bool {
        self.contains_package_impl(package, group_package, package_groups, &mut BTreeSet::new())
    }

    fn contains_package_impl(
        &self,
        package: &PackageLabel,
        group_package: &PackageLabel,
        package_groups: &PackageGroups,
        visited: &mut BTreeSet<TargetName>,
    ) -> bool {
        if self.packages.contains_package(package) {
            return true;
        }

        for include in &self.includes {
            if include.pkg() != *group_package {
                continue;
            }
            let include_name = include.name().to_owned();
            if visited.insert(include_name.clone())
                && let Some(group) = package_groups.get(&include_name)
                && group.contains_package_impl(package, group_package, package_groups, visited)
            {
                return true;
            }
        }

        false
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

#[cfg(test)]
mod tests {
    use bz_core::target::label::label::TargetLabel;

    use super::*;

    fn package_spec(package: &str) -> PackageGroupSpec {
        PackageGroupSpec::Pattern(ParsedPattern::Package(PackageLabel::testing_parse(package)))
    }

    fn package_group(packages: Vec<PackageGroupSpec>, includes: Vec<TargetLabel>) -> PackageGroup {
        PackageGroup {
            packages: PackageGroupContents {
                positives: packages,
                negatives: Vec::new(),
            },
            includes,
        }
    }

    #[test]
    fn package_group_contains_packages_from_includes() {
        let group_package = PackageLabel::testing_parse("root//vis");
        let allowed_package = PackageLabel::testing_parse("root//app");
        let mut groups = PackageGroups::new();
        groups.insert(
            TargetName::testing_new("base"),
            package_group(vec![package_spec("root//app")], Vec::new()),
        );

        let derived = package_group(
            Vec::new(),
            vec![TargetLabel::testing_parse("root//vis:base")],
        );

        assert!(derived.contains_package(&allowed_package, &group_package, &groups));
        assert!(derived.contains_target(
            &TargetLabel::testing_parse("root//app:lib"),
            &group_package,
            &groups
        ));
    }

    #[test]
    fn package_group_includes_do_not_cross_package_boundaries() {
        let group_package = PackageLabel::testing_parse("root//vis");
        let allowed_package = PackageLabel::testing_parse("root//app");
        let mut groups = PackageGroups::new();
        groups.insert(
            TargetName::testing_new("base"),
            package_group(vec![package_spec("root//app")], Vec::new()),
        );

        let derived = package_group(
            Vec::new(),
            vec![TargetLabel::testing_parse("root//other:base")],
        );

        assert!(!derived.contains_package(&allowed_package, &group_package, &groups));
    }

    #[test]
    fn package_group_include_cycles_terminate() {
        let group_package = PackageLabel::testing_parse("root//vis");
        let allowed_package = PackageLabel::testing_parse("root//app");
        let mut groups = PackageGroups::new();
        groups.insert(
            TargetName::testing_new("a"),
            package_group(Vec::new(), vec![TargetLabel::testing_parse("root//vis:b")]),
        );
        groups.insert(
            TargetName::testing_new("b"),
            package_group(Vec::new(), vec![TargetLabel::testing_parse("root//vis:a")]),
        );

        assert!(
            !groups
                .get(&TargetName::testing_new("a"))
                .unwrap()
                .contains_package(&allowed_package, &group_package, &groups)
        );
    }
}
