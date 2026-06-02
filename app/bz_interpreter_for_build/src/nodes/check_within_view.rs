/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_core::package::PackageLabel;
use bz_core::package::source_path::SourcePathRef;
use bz_core::provider::label::ProvidersLabel;
use bz_core::target::label::label::TargetLabel;
use bz_node::attrs::attr_type::AttrType;
use bz_node::attrs::attr_type::configuration_dep::ConfigurationDepKind;
use bz_node::attrs::coerced_attr::CoercedAttr;
use bz_node::attrs::traversal::CoercedAttrTraversal;
use bz_node::visibility::VisibilityPattern;
use bz_node::visibility::VisibilityPatternList;
use bz_node::visibility::WithinViewSpecification;
use dupe::Dupe;
use starlark::collections::SmallSet;

fn indented_within_view(spec: &WithinViewSpecification) -> String {
    match &spec.0 {
        VisibilityPatternList::Public => format!("  {}\n", VisibilityPattern::PUBLIC),
        VisibilityPatternList::List(items) => {
            let mut s = String::new();
            for item in items {
                s.push_str(&format!("  {item}\n"));
            }
            s
        }
    }
}

#[derive(Debug, bz_error::Error)]
#[buck2(input)]
enum CheckWithinViewError {
    #[error(
        "Target's `within_view` attribute does not allow dependency `{}`. Allowed dependencies:\n{}",
        _0,
        indented_within_view(_1)
    )]
    #[buck2(tag = Visibility)]
    DepNotWithinView(TargetLabel, WithinViewSpecification),
}

/// Check that dependencies in attribute do not violate `within_view`.
pub(crate) fn check_within_view(
    attr: &CoercedAttr,
    pkg: PackageLabel,
    attr_type: &AttrType,
    within_view: &WithinViewSpecification,
    default_deps: Option<&SmallSet<TargetLabel>>,
) -> bz_error::Result<()> {
    if within_view == &WithinViewSpecification::PUBLIC {
        // Shortcut.
        return Ok(());
    }

    struct WithinViewCheckTraversal<'x> {
        pkg: PackageLabel,
        within_view: &'x WithinViewSpecification,
        default_deps: &'x SmallSet<TargetLabel>,
    }

    impl<'x> WithinViewCheckTraversal<'x> {
        fn check_dep_within_view(&self, dep: &TargetLabel) -> bz_error::Result<()> {
            if self.pkg == dep.pkg()
                || self.default_deps.contains(dep)
                || self.within_view.0.matches_target(dep)
            {
                Ok(())
            } else {
                Err(
                    CheckWithinViewError::DepNotWithinView(dep.dupe(), self.within_view.dupe())
                        .into(),
                )
            }
        }
    }

    impl<'a, 'x> CoercedAttrTraversal<'a> for WithinViewCheckTraversal<'x> {
        fn dep(&mut self, dep: &ProvidersLabel) -> bz_error::Result<()> {
            self.check_dep_within_view(dep.target())
        }

        fn configuration_dep(
            &mut self,
            dep: &ProvidersLabel,
            t: ConfigurationDepKind,
        ) -> bz_error::Result<()> {
            match t {
                // Skip some configuration deps
                ConfigurationDepKind::CompatibilityAttribute => (),
                ConfigurationDepKind::SelectKey => (),
                ConfigurationDepKind::DefaultTargetPlatform => (),
                ConfigurationDepKind::ConfiguredDepPlatform | ConfigurationDepKind::Transition => {
                    self.check_dep_within_view(dep.target())?
                }
            }
            Ok(())
        }

        fn input(&mut self, _input: SourcePathRef) -> bz_error::Result<()> {
            Ok(())
        }
    }

    attr.traverse(
        attr_type,
        Some(pkg),
        &mut WithinViewCheckTraversal {
            pkg,
            within_view,
            default_deps: default_deps.unwrap_or(&SmallSet::new()),
        },
    )
}
