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
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::target::name::TargetName;
use pagable::Pagable;

use crate::oncall::Oncall;

pub type PackageGroups = BTreeMap<TargetName, Vec<ParsedPattern<TargetPatternExtra>>>;

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
