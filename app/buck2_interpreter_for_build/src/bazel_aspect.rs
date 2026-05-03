/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cell::RefCell;
use std::fmt;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::AllocValue;
use starlark::values::Freeze;
use starlark::values::FreezeResult;
use starlark::values::Freezer;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::typing::StarlarkCallable;

use crate::attrs::starlark_attribute::StarlarkAttribute;

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkAspect<'v> {
    id: RefCell<Option<String>>,
    implementation: StarlarkCallable<'v, (Value<'v>, Value<'v>), Value<'v>>,
    attr_aspects: Value<'v>,
    toolchains_aspects: Value<'v>,
    attrs: Vec<String>,
    doc: Option<String>,
    apply_to_generating_rules: bool,
}

impl<'v> fmt::Display for StarlarkAspect<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.id.borrow() {
            Some(id) => write!(f, "<aspect {id}>"),
            None => write!(f, "<anonymous aspect>"),
        }
    }
}

impl<'v> AllocValue<'v> for StarlarkAspect<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "aspect")]
impl<'v> StarlarkValue<'v> for StarlarkAspect<'v> {
    fn export_as(
        &self,
        variable_name: &str,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<()> {
        *self.id.borrow_mut() = Some(variable_name.to_owned());
        Ok(())
    }
}

impl<'v> Freeze for StarlarkAspect<'v> {
    type Frozen = FrozenStarlarkAspect;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkAspect {
            id: self.id.into_inner(),
            implementation: self.implementation.0.freeze(freezer)?,
            attr_aspects: self.attr_aspects.freeze(freezer)?,
            toolchains_aspects: self.toolchains_aspects.freeze(freezer)?,
            attrs: self.attrs,
            doc: self.doc,
            apply_to_generating_rules: self.apply_to_generating_rules,
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkAspect {
    id: Option<String>,
    #[allow(dead_code)]
    implementation: FrozenValue,
    #[allow(dead_code)]
    attr_aspects: FrozenValue,
    #[allow(dead_code)]
    toolchains_aspects: FrozenValue,
    #[allow(dead_code)]
    attrs: Vec<String>,
    #[allow(dead_code)]
    doc: Option<String>,
    #[allow(dead_code)]
    apply_to_generating_rules: bool,
}

impl fmt::Display for FrozenStarlarkAspect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.id {
            Some(id) => write!(f, "<aspect {id}>"),
            None => write!(f, "<anonymous aspect>"),
        }
    }
}

starlark_simple_value!(FrozenStarlarkAspect);

#[starlark_value(type = "aspect")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkAspect {
    type Canonical = StarlarkAspect<'v>;
}

fn doc_string(doc: NoneOr<&str>) -> Option<String> {
    doc.into_option().map(|doc| doc.trim().to_owned())
}

#[starlark_module]
pub(crate) fn register_bazel_aspect(builder: &mut GlobalsBuilder) {
    fn aspect<'v>(
        #[starlark(require = named)] implementation: StarlarkCallable<
            'v,
            (Value<'v>, Value<'v>),
            Value<'v>,
        >,
        #[starlark(require = named, default = AllocList::EMPTY)] attr_aspects: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] toolchains_aspects: Value<'v>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        attrs: UnpackDictEntries<&'v str, &'v StarlarkAttribute>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        required_providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        required_aspect_providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        provides: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        requires: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] propagation_predicate: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        fragments: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        host_fragments: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        toolchains: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named, default = false)] apply_to_generating_rules: bool,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        exec_compatible_with: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] exec_groups: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        subrules: UnpackListOrTuple<Value<'v>>,
    ) -> starlark::Result<StarlarkAspect<'v>> {
        let _unused = (
            required_providers,
            required_aspect_providers,
            provides,
            requires,
            propagation_predicate,
            fragments,
            host_fragments,
            toolchains,
            exec_compatible_with,
            exec_groups,
            subrules,
        );
        Ok(StarlarkAspect {
            id: RefCell::new(None),
            implementation,
            attr_aspects,
            toolchains_aspects,
            attrs: attrs
                .entries
                .into_iter()
                .map(|(name, _)| name.to_owned())
                .collect(),
            doc: doc_string(doc),
            apply_to_generating_rules,
        })
    }
}
