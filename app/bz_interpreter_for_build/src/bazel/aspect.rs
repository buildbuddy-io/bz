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
use starlark::values::ValueLike;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::typing::StarlarkCallable;

use crate::attrs::starlark_attribute::StarlarkAttribute;
use bz_interpreter::types::rule::FrozenBazelAspectInfo;
use bz_interpreter::types::rule::bazel_aspect_hidden_attr_name;
use bz_node::attrs::attr::Attribute;

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkAspect<'v> {
    id: RefCell<Option<String>>,
    implementation: StarlarkCallable<'v, (Value<'v>, Value<'v>), Value<'v>>,
    attr_aspects: Vec<String>,
    toolchains_aspects: Value<'v>,
    required_providers: Vec<Value<'v>>,
    required_aspect_providers: Vec<Value<'v>>,
    requires: Vec<Value<'v>>,
    toolchains: Vec<Value<'v>>,
    attrs: Vec<(String, Attribute)>,
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
            attr_aspects: self.attr_aspects,
            toolchains_aspects: self.toolchains_aspects.freeze(freezer)?,
            required_providers: self
                .required_providers
                .into_iter()
                .map(|provider| provider.freeze(freezer))
                .collect::<FreezeResult<Vec<_>>>()?,
            required_aspect_providers: self
                .required_aspect_providers
                .into_iter()
                .map(|provider| provider.freeze(freezer))
                .collect::<FreezeResult<Vec<_>>>()?,
            requires: self
                .requires
                .into_iter()
                .map(|aspect| aspect.freeze(freezer))
                .collect::<FreezeResult<Vec<_>>>()?,
            toolchains: self
                .toolchains
                .into_iter()
                .map(|toolchain| toolchain.freeze(freezer))
                .collect::<FreezeResult<Vec<_>>>()?,
            attrs: self.attrs,
            doc: self.doc,
            apply_to_generating_rules: self.apply_to_generating_rules,
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkAspect {
    id: Option<String>,
    pub(crate) implementation: FrozenValue,
    attr_aspects: Vec<String>,
    #[allow(dead_code)]
    toolchains_aspects: FrozenValue,
    required_providers: Vec<FrozenValue>,
    required_aspect_providers: Vec<FrozenValue>,
    requires: Vec<FrozenValue>,
    toolchains: Vec<FrozenValue>,
    attrs: Vec<(String, Attribute)>,
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

pub(crate) fn frozen_aspect_implementation(aspect: FrozenValue) -> Option<FrozenValue> {
    let aspect = aspect.downcast_ref::<FrozenStarlarkAspect>()?;
    Some(aspect.implementation)
}

pub(crate) fn frozen_aspect_info(
    aspect: FrozenValue,
) -> bz_error::Result<FrozenBazelAspectInfo> {
    let aspect = aspect
        .downcast_ref::<FrozenStarlarkAspect>()
        .ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "expected Bazel aspect, got `{}`",
                aspect
            )
        })?;
    Ok(FrozenBazelAspectInfo {
        implementation: aspect.implementation,
        attr_aspects: aspect.attr_aspects.clone(),
        required_providers: aspect.required_providers.clone(),
        required_aspect_providers: aspect.required_aspect_providers.clone(),
        requires: aspect.requires.clone(),
        attrs: aspect.attrs.iter().map(|(name, _)| name.clone()).collect(),
        toolchains: aspect.toolchains.clone(),
    })
}

fn collect_bazel_aspect_hidden_attributes_impl<'v>(
    rule_attr: &str,
    aspect_path: &str,
    aspect: Value<'v>,
    output: &mut Vec<(String, Attribute)>,
) {
    if let Some(aspect) = aspect.downcast_ref::<StarlarkAspect>() {
        for (name, attr) in &aspect.attrs {
            output.push((
                bazel_aspect_hidden_attr_name(rule_attr, aspect_path, name),
                attr.clone(),
            ));
        }
        for (idx, required) in aspect.requires.iter().enumerate() {
            let required_path = format!("{aspect_path}r{idx}");
            collect_bazel_aspect_hidden_attributes_impl(
                rule_attr,
                &required_path,
                *required,
                output,
            );
        }
        return;
    }

    let Some(aspect) = aspect
        .unpack_frozen()
        .and_then(|aspect| aspect.downcast_ref::<FrozenStarlarkAspect>())
    else {
        return;
    };
    for (name, attr) in &aspect.attrs {
        output.push((
            bazel_aspect_hidden_attr_name(rule_attr, aspect_path, name),
            attr.clone(),
        ));
    }
    for (idx, required) in aspect.requires.iter().enumerate() {
        let required_path = format!("{aspect_path}r{idx}");
        collect_bazel_aspect_hidden_attributes_impl(
            rule_attr,
            &required_path,
            required.to_value(),
            output,
        );
    }
}

pub(crate) fn collect_bazel_aspect_hidden_attributes<'v>(
    rule_attr: &str,
    aspects: &[Value<'v>],
    output: &mut Vec<(String, Attribute)>,
) {
    for (idx, aspect) in aspects.iter().enumerate() {
        collect_bazel_aspect_hidden_attributes_impl(rule_attr, &idx.to_string(), *aspect, output);
    }
}

fn collect_bazel_aspect_toolchains_impl<'v>(aspect: Value<'v>, output: &mut Vec<Value<'v>>) {
    if let Some(aspect) = aspect.downcast_ref::<StarlarkAspect>() {
        output.extend(aspect.toolchains.iter().copied());
        for required in &aspect.requires {
            collect_bazel_aspect_toolchains_impl(*required, output);
        }
        return;
    }

    let Some(aspect) = aspect
        .unpack_frozen()
        .and_then(|aspect| aspect.downcast_ref::<FrozenStarlarkAspect>())
    else {
        return;
    };
    output.extend(
        aspect
            .toolchains
            .iter()
            .map(|toolchain| toolchain.to_value()),
    );
    for required in &aspect.requires {
        collect_bazel_aspect_toolchains_impl(required.to_value(), output);
    }
}

pub(crate) fn collect_bazel_aspect_toolchains<'v>(
    aspects: &[Value<'v>],
    output: &mut Vec<Value<'v>>,
) {
    for aspect in aspects {
        collect_bazel_aspect_toolchains_impl(*aspect, output);
    }
}

fn doc_string(doc: NoneOr<&str>) -> Option<String> {
    doc.into_option().map(|doc| doc.trim().to_owned())
}

#[starlark_module]
pub(crate) fn register_bazel_aspect(builder: &mut GlobalsBuilder) {
    fn aspect<'v>(
        implementation: StarlarkCallable<'v, (Value<'v>, Value<'v>), Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        attr_aspects: UnpackListOrTuple<String>,
        #[starlark(require = named, default = AllocList::EMPTY)] toolchains_aspects: Value<'v>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        attrs: UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
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
            provides,
            propagation_predicate,
            fragments,
            host_fragments,
            exec_compatible_with,
            exec_groups,
            subrules,
        );
        Ok(StarlarkAspect {
            id: RefCell::new(None),
            implementation,
            attr_aspects: attr_aspects.items,
            toolchains_aspects,
            required_providers: required_providers.items,
            required_aspect_providers: required_aspect_providers.items,
            requires: requires.items,
            toolchains: toolchains.items,
            attrs: attrs
                .entries
                .into_iter()
                .map(|(name, attr)| (name.to_owned(), attr.clone_attribute()))
                .collect(),
            doc: doc_string(doc),
            apply_to_generating_rules,
        })
    }
}
