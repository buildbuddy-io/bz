/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::CellResolver;
use buck2_core::cells::name::CellName;
use buck2_core::package::PackageLabel;
use starlark::any::ProvidesStaticType;

#[derive(Debug, ProvidesStaticType)]
pub struct StarlarkLabelResolutionContext {
    pub cell_name: CellName,
    pub cell_resolver: CellResolver,
    pub cell_alias_resolver: CellAliasResolver,
    pub package: Option<PackageLabel>,
}

impl StarlarkLabelResolutionContext {
    pub fn new(
        cell_name: CellName,
        cell_resolver: CellResolver,
        cell_alias_resolver: CellAliasResolver,
        package: Option<PackageLabel>,
    ) -> Self {
        Self {
            cell_name,
            cell_resolver,
            cell_alias_resolver,
            package,
        }
    }
}
