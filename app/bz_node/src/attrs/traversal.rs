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

use bz_core::configuration::transition::id::TransitionId;
use bz_core::package::source_path::SourcePathRef;
use bz_core::plugins::PluginKind;
use bz_core::provider::label::ProvidersLabel;
use bz_core::target::label::label::TargetLabel;
use dupe::Dupe;

use crate::attrs::attr_type::configuration_dep::ConfigurationDepKind;

pub trait CoercedAttrTraversal<'a> {
    fn dep(&mut self, dep: &ProvidersLabel) -> bz_error::Result<()>;
    fn exec_dep(&mut self, dep: &'a ProvidersLabel) -> bz_error::Result<()> {
        self.dep(dep)
    }

    fn toolchain_dep(&mut self, dep: &'a ProvidersLabel) -> bz_error::Result<()> {
        self.dep(dep)
    }

    fn transition_dep(
        &mut self,
        dep: &'a ProvidersLabel,
        _tr: &Arc<TransitionId>,
    ) -> bz_error::Result<()> {
        self.dep(dep)
    }

    fn split_transition_dep(
        &mut self,
        dep: &'a ProvidersLabel,
        _tr: &Arc<TransitionId>,
    ) -> bz_error::Result<()> {
        self.dep(dep)
    }

    fn configuration_dep(
        &mut self,
        dep: &ProvidersLabel,
        _kind: ConfigurationDepKind,
    ) -> bz_error::Result<()> {
        self.dep(dep)
    }

    fn plugin_dep(&mut self, dep: &'a TargetLabel, _kind: &PluginKind) -> bz_error::Result<()> {
        let p = ProvidersLabel::default_for(dep.dupe());
        self.dep(&p)
    }

    fn input(&mut self, input: SourcePathRef) -> bz_error::Result<()>;

    fn inputs_require_package(&self) -> bool {
        true
    }

    fn label(&mut self, _label: &'a ProvidersLabel) -> bz_error::Result<()> {
        Ok(())
    }
}
