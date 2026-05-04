/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use allocative::Allocative;
use buck2_node::attrs::attr::Attribute;
use buck2_node::bzl_or_bxl_path::BzlOrBxlPath;
use buck2_node::rule_type::StarlarkRuleType;
use derive_more::Display;
use itertools::Itertools;
use starlark::any::ProvidesStaticType;
use starlark::docs::DocFunction;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::DocStringKind;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::eval::ParametersSpec;
use starlark::eval::ParametersSpecParam;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::typing::ParamSpec;
use starlark::typing::Ty;
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
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::typing::StarlarkCallable;
use starlark_map::small_map::SmallMap;

use crate::attrs::starlark_attribute::StarlarkAttribute;
use crate::interpreter::build_context::BazelRepositoryRuleInvocation;
use crate::interpreter::build_context::BuildContext;
use crate::interpreter::build_context::PerFileTypeContext;
use crate::rule::NAME_ATTRIBUTE_FIELD;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelRepositoryError {
    #[error("`{0}` is not a valid repository rule attribute name")]
    InvalidRepositoryRuleAttributeName(String),
    #[error("`repository_rule` requires an implementation function")]
    MissingRepositoryRuleImplementation,
    #[error("`{0}` can only be declared in bzl files")]
    NotInBzl(&'static str),
    #[error(
        "repository rules can only be called from within module extension implementation functions"
    )]
    RepositoryRuleCalledOutsideModuleExtension,
    #[error("repository rule calls require a `name` argument")]
    RepositoryRuleMissingName,
    #[error("repository rule `name` argument must be a string, got `{0}`")]
    RepositoryRuleNameMustBeString(String),
    #[error("attempting to instantiate a non-exported repository rule")]
    RepositoryRuleNotExported,
    #[error("attempting to instantiate a non-exported module extension")]
    ModuleExtensionNotExported,
    #[error("expected module extension `{0}` to return None or extension_metadata, got `{1}`")]
    InvalidModuleExtensionReturn(String, String),
    #[error("`tag_classes[{0}]` must be a tag_class object, got `{1}`")]
    InvalidTagClass(String, String),
}

fn current_bzl_path<'v>(
    eval: &Evaluator<'v, '_, '_>,
    symbol: &'static str,
) -> buck2_error::Result<BzlOrBxlPath> {
    let build_context = BuildContext::from_context(eval)?;
    match &build_context.additional {
        PerFileTypeContext::Bzl(bzl_path) => Ok(BzlOrBxlPath::Bzl(bzl_path.bzl_path.clone())),
        _ => Err(BazelRepositoryError::NotInBzl(symbol).into()),
    }
}

fn doc_string(doc: NoneOr<&str>) -> Option<String> {
    doc.into_option().map(|doc| doc.trim().to_owned())
}

fn record_repository_rule_invocation<'v>(
    rule_id: &StarlarkRuleType,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let build_context = BuildContext::from_context(eval)?;
    let recorder = build_context.bazel_repository_rule_recorder.ok_or_else(|| {
        buck2_error::Error::from(BazelRepositoryError::RepositoryRuleCalledOutsideModuleExtension)
    })?;

    args.no_positional_args(eval.heap())?;

    let mut name = None;
    let mut attrs = Vec::new();
    for (attr_name, attr_value) in args.names_map()? {
        let attr_name = attr_name.as_str();
        if attr_name == NAME_ATTRIBUTE_FIELD {
            let Some(name_value) = attr_value.unpack_str() else {
                return Err(buck2_error::Error::from(
                    BazelRepositoryError::RepositoryRuleNameMustBeString(
                        attr_value.get_type().to_owned(),
                    ),
                )
                .into());
            };
            name = Some(name_value.to_owned());
        } else {
            attrs.push((attr_name.to_owned(), attr_value.to_repr()));
        }
    }
    let name = name.ok_or_else(|| {
        buck2_error::Error::from(BazelRepositoryError::RepositoryRuleMissingName)
    })?;
    attrs.sort_by(|(left, _), (right, _)| left.cmp(right));

    recorder.record(BazelRepositoryRuleInvocation {
        rule_id: rule_id.clone(),
        name,
        attrs,
    });

    Ok(Value::new_none())
}

fn empty_dict_value<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocDict(Vec::<(Value<'v>, Value<'v>)>::new()))
}

fn validate_module_extension_return<'v>(
    extension_id: &StarlarkRuleType,
    value: Value<'v>,
) -> starlark::Result<Value<'v>> {
    if value.is_none() || value.downcast_ref::<StarlarkModuleExtensionMetadata>().is_some() {
        return Ok(value);
    }
    Err(buck2_error::Error::from(BazelRepositoryError::InvalidModuleExtensionReturn(
        extension_id.to_string(),
        value.get_type().to_owned(),
    ))
    .into())
}

#[derive(Debug, Allocative)]
struct BazelAttributeSpec {
    attributes: SmallMap<String, Attribute>,
}

impl BazelAttributeSpec {
    fn from_entries<'v>(
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute>>,
        allow_name: bool,
    ) -> buck2_error::Result<Self> {
        let attrs = attrs.unwrap_or_default();
        let attributes = attrs
            .entries
            .into_iter()
            .sorted_by(|(k1, _), (k2, _)| Ord::cmp(k1, k2))
            .map(|(name, value)| {
                if !allow_name && name == NAME_ATTRIBUTE_FIELD {
                    Err(BazelRepositoryError::InvalidRepositoryRuleAttributeName(
                        NAME_ATTRIBUTE_FIELD.to_owned(),
                    )
                    .into())
                } else {
                    Ok((name.to_owned(), value.clone_attribute()))
                }
            })
            .collect::<buck2_error::Result<SmallMap<_, _>>>()?;
        Ok(Self { attributes })
    }

    fn documentation(&self, name: &str, docs: Option<&str>, ret: Ty) -> DocItem {
        let parameters_spec = ParametersSpec::new_named_only(
            name,
            self.attributes.iter().map(|(name, attribute)| {
                (
                    name.as_str(),
                    match attribute.default() {
                        Some(_) => ParametersSpecParam::<FrozenValue>::Optional,
                        None => ParametersSpecParam::<FrozenValue>::Required,
                    },
                )
            }),
        );
        let params = parameters_spec.documentation_with_default_value_formatter(
            vec![Ty::any(); self.attributes.len()],
            HashMap::new(),
            |_v| "<default>".to_owned(),
        );

        DocItem::Member(DocMember::Function(DocFunction::from_docstring(
            DocStringKind::Starlark,
            params,
            ret,
            docs,
        )))
    }

    #[allow(dead_code)]
    pub(crate) fn attributes(&self) -> &SmallMap<String, Attribute> {
        &self.attributes
    }
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkRepositoryRule<'v> {
    rule_path: BzlOrBxlPath,
    id: RefCell<Option<StarlarkRuleType>>,
    implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
    #[trace(unsafe_ignore)]
    attributes: BazelAttributeSpec,
    local: bool,
    configure: bool,
    remotable: bool,
    environ: Vec<String>,
    docs: Option<String>,
    ty: Ty,
}

impl<'v> StarlarkRepositoryRule<'v> {
    fn new(
        implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute>>,
        local: bool,
        configure: bool,
        remotable: bool,
        environ: UnpackListOrTuple<String>,
        doc: NoneOr<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<Self> {
        let attributes = BazelAttributeSpec::from_entries(attrs, false)?;
        let ty = Ty::function(ParamSpec::kwargs(Ty::any()), Ty::none());
        Ok(Self {
            rule_path: current_bzl_path(eval, "repository_rule")?,
            id: RefCell::new(None),
            implementation,
            attributes,
            local,
            configure,
            remotable,
            environ: environ.items,
            docs: doc_string(doc),
            ty,
        })
    }

    fn name_for_docs(&self) -> String {
        self.id
            .borrow()
            .as_ref()
            .map_or_else(|| "repository_rule".to_owned(), |id| id.name.clone())
    }
}

impl<'v> Display for StarlarkRepositoryRule<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.id.borrow() {
            Some(id) => write!(f, "<starlark repository rule {}>", id.name),
            None => write!(f, "<anonymous starlark repository rule>"),
        }
    }
}

impl<'v> AllocValue<'v> for StarlarkRepositoryRule<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "repository_rule")]
impl<'v> StarlarkValue<'v> for StarlarkRepositoryRule<'v> {
    fn export_as(
        &self,
        variable_name: &str,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<()> {
        *self.id.borrow_mut() = Some(StarlarkRuleType {
            path: self.rule_path.clone(),
            name: variable_name.to_owned(),
        });
        Ok(())
    }

    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let id = self.id.borrow();
        let Some(id) = id.as_ref() else {
            return Err(
                buck2_error::Error::from(BazelRepositoryError::RepositoryRuleNotExported).into(),
            );
        };
        record_repository_rule_invocation(id, args, eval)
    }

    fn documentation(&self) -> DocItem {
        self.attributes
            .documentation(&self.name_for_docs(), self.docs.as_deref(), Ty::none())
    }

    fn typechecker_ty(&self) -> Option<Ty> {
        Some(self.ty.clone())
    }

    fn get_type_starlark_repr() -> Ty {
        Ty::function(ParamSpec::kwargs(Ty::any()), Ty::none())
    }
}

impl<'v> Freeze for StarlarkRepositoryRule<'v> {
    type Frozen = FrozenStarlarkRepositoryRule;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkRepositoryRule {
            id: self.id.into_inner().map(Arc::new),
            implementation: self.implementation.0.freeze(freezer)?,
            attributes: self.attributes,
            local: self.local,
            configure: self.configure,
            remotable: self.remotable,
            environ: self.environ,
            docs: self.docs,
            ty: self.ty,
        })
    }
}

#[derive(Debug, Display, ProvidesStaticType, NoSerialize, Allocative)]
#[display("{}", self.display())]
pub(crate) struct FrozenStarlarkRepositoryRule {
    id: Option<Arc<StarlarkRuleType>>,
    #[allow(dead_code)]
    implementation: FrozenValue,
    attributes: BazelAttributeSpec,
    #[allow(dead_code)]
    local: bool,
    #[allow(dead_code)]
    configure: bool,
    #[allow(dead_code)]
    remotable: bool,
    #[allow(dead_code)]
    environ: Vec<String>,
    docs: Option<String>,
    ty: Ty,
}

impl FrozenStarlarkRepositoryRule {
    fn display(&self) -> String {
        match &self.id {
            Some(id) => format!("<starlark repository rule {}>", id.name),
            None => "<anonymous starlark repository rule>".to_owned(),
        }
    }

    fn name_for_docs(&self) -> String {
        self.id
            .as_ref()
            .map_or_else(|| "repository_rule".to_owned(), |id| id.name.clone())
    }
}

starlark_simple_value!(FrozenStarlarkRepositoryRule);

#[starlark_value(type = "repository_rule")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkRepositoryRule {
    type Canonical = StarlarkRepositoryRule<'v>;

    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let Some(id) = &self.id else {
            return Err(
                buck2_error::Error::from(BazelRepositoryError::RepositoryRuleNotExported).into(),
            );
        };
        record_repository_rule_invocation(id, args, eval)
    }

    fn documentation(&self) -> DocItem {
        self.attributes
            .documentation(&self.name_for_docs(), self.docs.as_deref(), Ty::none())
    }

    fn typechecker_ty(&self) -> Option<Ty> {
        Some(self.ty.clone())
    }

    fn get_type_starlark_repr() -> Ty {
        StarlarkRepositoryRule::get_type_starlark_repr()
    }
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkTagClass {
    #[trace(unsafe_ignore)]
    attributes: BazelAttributeSpec,
    docs: Option<String>,
}

impl<'v> StarlarkTagClass {
    fn new(
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute>>,
        doc: NoneOr<&str>,
    ) -> buck2_error::Result<Self> {
        Ok(Self {
            attributes: BazelAttributeSpec::from_entries(attrs, true)?,
            docs: doc_string(doc),
        })
    }
}

impl Display for StarlarkTagClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<tag_class>")
    }
}

impl<'v> AllocValue<'v> for StarlarkTagClass {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "tag_class")]
impl<'v> StarlarkValue<'v> for StarlarkTagClass {
    fn documentation(&self) -> DocItem {
        self.attributes
            .documentation("tag_class", self.docs.as_deref(), Ty::any())
    }
}

impl Freeze for StarlarkTagClass {
    type Frozen = FrozenStarlarkTagClass;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkTagClass {
            attributes: self.attributes,
            docs: self.docs,
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkTagClass {
    #[allow(dead_code)]
    attributes: BazelAttributeSpec,
    #[allow(dead_code)]
    docs: Option<String>,
}

impl FrozenStarlarkTagClass {
    #[allow(dead_code)]
    pub(crate) fn attributes(&self) -> &SmallMap<String, Attribute> {
        self.attributes.attributes()
    }
}

impl Display for FrozenStarlarkTagClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<tag_class>")
    }
}

starlark_simple_value!(FrozenStarlarkTagClass);

#[starlark_value(type = "tag_class")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkTagClass {
    type Canonical = StarlarkTagClass;
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkModuleExtension<'v> {
    extension_path: BzlOrBxlPath,
    id: RefCell<Option<StarlarkRuleType>>,
    implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
    tag_classes: SmallMap<String, Value<'v>>,
    docs: Option<String>,
    environ: Vec<String>,
    os_dependent: bool,
    arch_dependent: bool,
}

impl<'v> StarlarkModuleExtension<'v> {
    fn new(
        implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
        tag_classes: SmallMap<String, Value<'v>>,
        doc: NoneOr<&str>,
        environ: UnpackListOrTuple<String>,
        os_dependent: bool,
        arch_dependent: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<Self> {
        for (name, value) in &tag_classes {
            if ValueTypedComplex::<StarlarkTagClass>::new(*value).is_none() {
                return Err(BazelRepositoryError::InvalidTagClass(
                    name.to_owned(),
                    value.get_type().to_owned(),
                )
                .into());
            }
        }
        Ok(Self {
            extension_path: current_bzl_path(eval, "module_extension")?,
            id: RefCell::new(None),
            implementation,
            tag_classes,
            docs: doc_string(doc),
            environ: environ.items,
            os_dependent,
            arch_dependent,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn invoke_implementation(
        &self,
        module_ctx: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let id = self.id.borrow();
        let Some(id) = id.as_ref() else {
            return Err(
                buck2_error::Error::from(BazelRepositoryError::ModuleExtensionNotExported).into(),
            );
        };
        let positional = [module_ctx];
        let args = Arguments::new_positional(&positional);
        let result = self.implementation.0.invoke(&args, eval)?;
        validate_module_extension_return(id, result)
    }
}

impl<'v> Display for StarlarkModuleExtension<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.id.borrow() {
            Some(id) => write!(f, "<module_extension {}>", id.name),
            None => write!(f, "<anonymous module_extension>"),
        }
    }
}

impl<'v> AllocValue<'v> for StarlarkModuleExtension<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "module_extension")]
impl<'v> StarlarkValue<'v> for StarlarkModuleExtension<'v> {
    fn export_as(
        &self,
        variable_name: &str,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<()> {
        *self.id.borrow_mut() = Some(StarlarkRuleType {
            path: self.extension_path.clone(),
            name: variable_name.to_owned(),
        });
        Ok(())
    }
}

impl<'v> Freeze for StarlarkModuleExtension<'v> {
    type Frozen = FrozenStarlarkModuleExtension;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let tag_classes = self
            .tag_classes
            .into_iter()
            .map(|(name, value)| Ok((name, value.freeze(freezer)?)))
            .collect::<FreezeResult<SmallMap<String, FrozenValue>>>()?;
        Ok(FrozenStarlarkModuleExtension {
            id: self.id.into_inner().map(Arc::new),
            implementation: self.implementation.0.freeze(freezer)?,
            tag_classes,
            docs: self.docs,
            environ: self.environ,
            os_dependent: self.os_dependent,
            arch_dependent: self.arch_dependent,
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkModuleExtension {
    #[allow(dead_code)]
    id: Option<Arc<StarlarkRuleType>>,
    #[allow(dead_code)]
    implementation: FrozenValue,
    #[allow(dead_code)]
    tag_classes: SmallMap<String, FrozenValue>,
    #[allow(dead_code)]
    docs: Option<String>,
    #[allow(dead_code)]
    environ: Vec<String>,
    #[allow(dead_code)]
    os_dependent: bool,
    #[allow(dead_code)]
    arch_dependent: bool,
}

impl Display for FrozenStarlarkModuleExtension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.id {
            Some(id) => write!(f, "<module_extension {}>", id.name),
            None => write!(f, "<anonymous module_extension>"),
        }
    }
}

impl FrozenStarlarkModuleExtension {
    #[allow(dead_code)]
    pub(crate) fn id(&self) -> buck2_error::Result<&StarlarkRuleType> {
        self.id
            .as_deref()
            .ok_or_else(|| BazelRepositoryError::ModuleExtensionNotExported.into())
    }

    #[allow(dead_code)]
    pub(crate) fn tag_classes(&self) -> &SmallMap<String, FrozenValue> {
        &self.tag_classes
    }

    #[allow(dead_code)]
    pub(crate) fn invoke_implementation<'v>(
        &self,
        module_ctx: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let id = self.id()?;
        let positional = [module_ctx];
        let args = Arguments::new_positional(&positional);
        let result = self.implementation.to_value().invoke(&args, eval)?;
        validate_module_extension_return(id, result)
    }
}

starlark_simple_value!(FrozenStarlarkModuleExtension);

#[starlark_value(type = "module_extension")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkModuleExtension {
    type Canonical = StarlarkModuleExtension<'v>;
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkModuleExtensionContext<'v> {
    modules: Vec<Value<'v>>,
}

#[allow(dead_code)]
impl<'v> StarlarkModuleExtensionContext<'v> {
    pub(crate) fn new(modules: Vec<Value<'v>>) -> Self {
        Self { modules }
    }
}

impl<'v> Display for StarlarkModuleExtensionContext<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_ctx>")
    }
}

impl<'v> AllocValue<'v> for StarlarkModuleExtensionContext<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "module_ctx")]
impl<'v> StarlarkValue<'v> for StarlarkModuleExtensionContext<'v> {
    fn dir_attr(&self) -> Vec<String> {
        vec!["facts".to_owned(), "modules".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "facts" => Some(empty_dict_value(heap)),
            "modules" => Some(heap.alloc(AllocList(self.modules.iter().copied()))),
            _ => None,
        }
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(module_extension_context_methods)
    }
}

impl<'v> Freeze for StarlarkModuleExtensionContext<'v> {
    type Frozen = FrozenStarlarkModuleExtensionContext;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let modules = self
            .modules
            .into_iter()
            .map(|module| module.freeze(freezer))
            .collect::<FreezeResult<Vec<_>>>()?;
        Ok(FrozenStarlarkModuleExtensionContext { modules })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkModuleExtensionContext {
    modules: Vec<FrozenValue>,
}

impl Display for FrozenStarlarkModuleExtensionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_ctx>")
    }
}

starlark_simple_value!(FrozenStarlarkModuleExtensionContext);

#[starlark_value(type = "module_ctx")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkModuleExtensionContext {
    type Canonical = StarlarkModuleExtensionContext<'v>;

    fn dir_attr(&self) -> Vec<String> {
        vec!["facts".to_owned(), "modules".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "facts" => Some(empty_dict_value(heap)),
            "modules" => Some(heap.alloc(AllocList(
                self.modules.iter().map(|module| module.to_value()),
            ))),
            _ => None,
        }
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(module_extension_context_methods)
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkBazelModule<'v> {
    name: String,
    version: String,
    tags: Value<'v>,
    is_root: bool,
}

#[allow(dead_code)]
impl<'v> StarlarkBazelModule<'v> {
    pub(crate) fn new(name: String, version: String, tags: Value<'v>, is_root: bool) -> Self {
        Self {
            name,
            version,
            tags,
            is_root,
        }
    }
}

impl<'v> Display for StarlarkBazelModule<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<bazel_module {}@{}>", self.name, self.version)
    }
}

impl<'v> AllocValue<'v> for StarlarkBazelModule<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "bazel_module")]
impl<'v> StarlarkValue<'v> for StarlarkBazelModule<'v> {
    fn dir_attr(&self) -> Vec<String> {
        vec![
            "is_root".to_owned(),
            "name".to_owned(),
            "tags".to_owned(),
            "version".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "is_root" => Some(Value::new_bool(self.is_root)),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "tags" => Some(self.tags),
            "version" => Some(heap.alloc_str(&self.version).to_value()),
            _ => None,
        }
    }
}

impl<'v> Freeze for StarlarkBazelModule<'v> {
    type Frozen = FrozenStarlarkBazelModule;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkBazelModule {
            name: self.name,
            version: self.version,
            tags: self.tags.freeze(freezer)?,
            is_root: self.is_root,
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkBazelModule {
    name: String,
    version: String,
    tags: FrozenValue,
    is_root: bool,
}

impl Display for FrozenStarlarkBazelModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<bazel_module {}@{}>", self.name, self.version)
    }
}

starlark_simple_value!(FrozenStarlarkBazelModule);

#[starlark_value(type = "bazel_module")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkBazelModule {
    type Canonical = StarlarkBazelModule<'v>;

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "is_root".to_owned(),
            "name".to_owned(),
            "tags".to_owned(),
            "version".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "is_root" => Some(Value::new_bool(self.is_root)),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "tags" => Some(self.tags.to_value()),
            "version" => Some(heap.alloc_str(&self.version).to_value()),
            _ => None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkBazelModuleTags<'v> {
    tags: SmallMap<String, Vec<Value<'v>>>,
}

#[allow(dead_code)]
impl<'v> StarlarkBazelModuleTags<'v> {
    pub(crate) fn new(tags: SmallMap<String, Vec<Value<'v>>>) -> Self {
        Self { tags }
    }
}

impl<'v> Display for StarlarkBazelModuleTags<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<bazel_module_tags>")
    }
}

impl<'v> AllocValue<'v> for StarlarkBazelModuleTags<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "bazel_module_tags")]
impl<'v> StarlarkValue<'v> for StarlarkBazelModuleTags<'v> {
    fn dir_attr(&self) -> Vec<String> {
        self.tags.keys().cloned().collect()
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        self.tags
            .get(attribute)
            .map(|tags| heap.alloc(AllocList(tags.iter().copied())))
    }
}

impl<'v> Freeze for StarlarkBazelModuleTags<'v> {
    type Frozen = FrozenStarlarkBazelModuleTags;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let tags = self
            .tags
            .into_iter()
            .map(|(name, values)| {
                let values = values
                    .into_iter()
                    .map(|value| value.freeze(freezer))
                    .collect::<FreezeResult<Vec<_>>>()?;
                Ok((name, values))
            })
            .collect::<FreezeResult<SmallMap<_, _>>>()?;
        Ok(FrozenStarlarkBazelModuleTags { tags })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkBazelModuleTags {
    tags: SmallMap<String, Vec<FrozenValue>>,
}

impl Display for FrozenStarlarkBazelModuleTags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<bazel_module_tags>")
    }
}

starlark_simple_value!(FrozenStarlarkBazelModuleTags);

#[starlark_value(type = "bazel_module_tags")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkBazelModuleTags {
    type Canonical = StarlarkBazelModuleTags<'v>;

    fn dir_attr(&self) -> Vec<String> {
        self.tags.keys().cloned().collect()
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        self.tags.get(attribute).map(|tags| {
            heap.alloc(AllocList(tags.iter().map(|tag| tag.to_value())))
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkBazelModuleTag<'v> {
    tag_name: String,
    dev_dependency: bool,
    sort_key: i32,
    attrs: SmallMap<String, Value<'v>>,
}

#[allow(dead_code)]
impl<'v> StarlarkBazelModuleTag<'v> {
    pub(crate) fn new(
        tag_name: String,
        dev_dependency: bool,
        sort_key: i32,
        attrs: SmallMap<String, Value<'v>>,
    ) -> Self {
        Self {
            tag_name,
            dev_dependency,
            sort_key,
            attrs,
        }
    }
}

impl<'v> Display for StarlarkBazelModuleTag<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_extension_tag {}>", self.tag_name)
    }
}

impl<'v> AllocValue<'v> for StarlarkBazelModuleTag<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "module_extension_tag")]
impl<'v> StarlarkValue<'v> for StarlarkBazelModuleTag<'v> {
    fn dir_attr(&self) -> Vec<String> {
        self.attrs.keys().cloned().collect()
    }

    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.attrs.get(attribute).copied()
    }
}

impl<'v> Freeze for StarlarkBazelModuleTag<'v> {
    type Frozen = FrozenStarlarkBazelModuleTag;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let attrs = self
            .attrs
            .into_iter()
            .map(|(name, value)| Ok((name, value.freeze(freezer)?)))
            .collect::<FreezeResult<SmallMap<_, _>>>()?;
        Ok(FrozenStarlarkBazelModuleTag {
            tag_name: self.tag_name,
            dev_dependency: self.dev_dependency,
            sort_key: self.sort_key,
            attrs,
        })
    }
}

#[allow(dead_code)]
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkBazelModuleTag {
    tag_name: String,
    dev_dependency: bool,
    sort_key: i32,
    attrs: SmallMap<String, FrozenValue>,
}

impl Display for FrozenStarlarkBazelModuleTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_extension_tag {}>", self.tag_name)
    }
}

starlark_simple_value!(FrozenStarlarkBazelModuleTag);

#[starlark_value(type = "module_extension_tag")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkBazelModuleTag {
    type Canonical = StarlarkBazelModuleTag<'v>;

    fn dir_attr(&self) -> Vec<String> {
        self.attrs.keys().cloned().collect()
    }

    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.attrs.get(attribute).map(|value| value.to_value())
    }
}

#[derive(Debug, Display, ProvidesStaticType, NoSerialize, Allocative)]
#[display("<extension_metadata>")]
pub(crate) struct StarlarkModuleExtensionMetadata {
    #[allow(dead_code)]
    reproducible: bool,
}

starlark_simple_value!(StarlarkModuleExtensionMetadata);

#[starlark_value(type = "extension_metadata")]
impl<'v> StarlarkValue<'v> for StarlarkModuleExtensionMetadata {}

fn bazel_module_tag_dev_dependency<'v>(tag: Value<'v>) -> starlark::Result<bool> {
    if let Some(tag) = tag.downcast_ref::<StarlarkBazelModuleTag>() {
        return Ok(tag.dev_dependency);
    }
    if let Some(tag) = tag.downcast_ref::<FrozenStarlarkBazelModuleTag>() {
        return Ok(tag.dev_dependency);
    }
    Err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::Input,
        "expected module extension tag, got `{}`",
        tag.get_type()
    )
    .into())
}

fn bazel_module_tag_sort_key<'v>(tag: Value<'v>) -> starlark::Result<i32> {
    if let Some(tag) = tag.downcast_ref::<StarlarkBazelModuleTag>() {
        return Ok(tag.sort_key);
    }
    if let Some(tag) = tag.downcast_ref::<FrozenStarlarkBazelModuleTag>() {
        return Ok(tag.sort_key);
    }
    Err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::Input,
        "expected module extension tag, got `{}`",
        tag.get_type()
    )
    .into())
}

#[starlark_module]
fn module_extension_context_methods(builder: &mut MethodsBuilder) {
    fn is_dev_dependency<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        tag: Value<'v>,
    ) -> starlark::Result<bool> {
        let _unused = this;
        bazel_module_tag_dev_dependency(tag)
    }

    fn tag_sort_key<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        tag: Value<'v>,
    ) -> starlark::Result<i32> {
        let _unused = this;
        bazel_module_tag_sort_key(tag)
    }

    fn extension_metadata<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = named, default = false)] reproducible: bool,
        #[starlark(require = named, default = NoneOr::None)] _root_module_direct_deps: NoneOr<
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] _root_module_direct_dev_deps: NoneOr<
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] _facts: NoneOr<Value<'v>>,
    ) -> starlark::Result<StarlarkModuleExtensionMetadata> {
        let _unused = this;
        Ok(StarlarkModuleExtensionMetadata { reproducible })
    }
}

#[starlark_module]
#[starlark_types(
    StarlarkRepositoryRule<'_> as RepositoryRule,
    StarlarkTagClass as TagClass,
    StarlarkModuleExtension<'_> as ModuleExtension
)]
pub(crate) fn register_bazel_repository_globals(builder: &mut GlobalsBuilder) {
    fn repository_rule<'v>(
        implementation: Option<StarlarkCallable<'v, (Value<'v>,), Value<'v>>>,
        #[starlark(require = named)] attrs: Option<
            UnpackDictEntries<&'v str, &'v StarlarkAttribute>,
        >,
        #[starlark(require = named, default = false)] local: bool,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        environ: UnpackListOrTuple<String>,
        #[starlark(require = named, default = false)] configure: bool,
        #[starlark(require = named, default = false)] remotable: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkRepositoryRule<'v>> {
        let implementation = implementation.ok_or_else(|| {
            buck2_error::Error::from(BazelRepositoryError::MissingRepositoryRuleImplementation)
        })?;
        Ok(StarlarkRepositoryRule::new(
            implementation,
            attrs,
            local,
            configure,
            remotable,
            environ,
            doc,
            eval,
        )?)
    }

    fn tag_class<'v>(
        #[starlark(require = named)] attrs: Option<
            UnpackDictEntries<&'v str, &'v StarlarkAttribute>,
        >,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
    ) -> starlark::Result<StarlarkTagClass> {
        Ok(StarlarkTagClass::new(attrs, doc)?)
    }

    fn module_extension<'v>(
        #[starlark(require = named)] implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
        #[starlark(require = named, default = SmallMap::new())] tag_classes: SmallMap<
            String,
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        environ: UnpackListOrTuple<String>,
        #[starlark(require = named, default = false)] os_dependent: bool,
        #[starlark(require = named, default = false)] arch_dependent: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkModuleExtension<'v>> {
        Ok(StarlarkModuleExtension::new(
            implementation,
            tag_classes,
            doc,
            environ,
            os_dependent,
            arch_dependent,
            eval,
        )?)
    }
}
