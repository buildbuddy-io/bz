use super::*;

#[derive(Debug, Clone, Allocative)]
pub(super) struct BazelRepositoryAttrValues {
    pub(super) attrs: SmallMap<String, CoercedAttr>,
    pub(super) name: String,
}

impl BazelRepositoryAttrValues {
    pub(super) fn alloc<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        let mut attrs = Vec::with_capacity(self.attrs.len() + 1);
        for (name, value) in &self.attrs {
            attrs.push((
                name.as_str(),
                alloc_coerced_attr_value_on_heap(value, heap)
                    .expect("repository rule attributes were already coerced"),
            ));
        }
        attrs.push((NAME_ATTRIBUTE_FIELD, heap.alloc_str(&self.name).to_value()));
        heap.alloc(AllocStruct(attrs))
    }
}

pub(super) fn repository_ctx_workspace_root(working_dir: &str) -> String {
    if let Some((workspace_root, _)) = working_dir.split_once("/buck-out/")
        && !workspace_root.is_empty()
    {
        return workspace_root.to_owned();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut components = Path::new(working_dir).components();
    if let (Some(std::path::Component::Normal(first)), Some(std::path::Component::Normal(second))) =
        (components.next(), components.next())
    {
        let isolation_root = Path::new(first).join(second);
        if cwd.ends_with(&isolation_root)
            && let Some(root) = cwd.parent().and_then(|path| path.parent())
        {
            return root.to_string_lossy().into_owned();
        }
    }
    cwd.to_string_lossy().into_owned()
}

#[derive(Debug, Allocative)]
pub(super) struct BazelAttributeSpec {
    attributes: SmallMap<String, Attribute>,
}

impl BazelAttributeSpec {
    fn from_entries<'v>(
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>>,
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
    pub(super) fn new(
        implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>>,
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
    pub(super) attributes: BazelAttributeSpec,
    #[allow(dead_code)]
    pub(super) local: bool,
    #[allow(dead_code)]
    configure: bool,
    #[allow(dead_code)]
    pub(super) remotable: bool,
    #[allow(dead_code)]
    pub(super) environ: Vec<String>,
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

    pub(crate) fn invoke_implementation<'v>(
        &self,
        repository_ctx: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        eval.eval_function(self.implementation.to_value(), &[repository_ctx], &[])
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
    pub(super) fn new(
        attrs: Option<UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>>,
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
    pub(super) fn new(
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
        let result = eval.eval_function(self.implementation.0, &[module_ctx], &[])?;
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
    pub(super) environ: Vec<String>,
    #[allow(dead_code)]
    pub(super) os_dependent: bool,
    #[allow(dead_code)]
    pub(super) arch_dependent: bool,
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
        let result = eval.eval_function(self.implementation.to_value(), &[module_ctx], &[])?;
        validate_module_extension_return(id, result)
    }
}

starlark_simple_value!(FrozenStarlarkModuleExtension);

#[starlark_value(type = "module_extension")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkModuleExtension {
    type Canonical = StarlarkModuleExtension<'v>;
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkRepositoryOs {
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    repo_env: Arc<BTreeMap<String, String>>,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>>,
}

impl StarlarkRepositoryOs {
    pub(super) fn new(
        repo_env: Arc<BTreeMap<String, String>>,
        recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>>,
    ) -> Self {
        Self {
            repo_env,
            recorded_inputs,
        }
    }
}

impl Display for StarlarkRepositoryOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository_os>")
    }
}

impl<'v> AllocValue<'v> for StarlarkRepositoryOs {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "repository_os")]
impl<'v> StarlarkValue<'v> for StarlarkRepositoryOs {
    fn dir_attr(&self) -> Vec<String> {
        vec!["arch".to_owned(), "environ".to_owned(), "name".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "arch" => Some(heap.alloc(repository_os_arch(&self.repo_env, &self.recorded_inputs))),
            "environ" => Some(host_environ(heap, &self.repo_env, &self.recorded_inputs)),
            "name" => Some(heap.alloc(repository_os_name(&self.repo_env, &self.recorded_inputs))),
            _ => None,
        }
    }
}

impl Freeze for StarlarkRepositoryOs {
    type Frozen = FrozenStarlarkRepositoryOs;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkRepositoryOs {
            repo_env: self.repo_env,
            recorded_inputs: self.recorded_inputs,
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkRepositoryOs {
    #[allocative(skip)]
    repo_env: Arc<BTreeMap<String, String>>,
    #[allocative(skip)]
    recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>>,
}

impl FrozenStarlarkRepositoryOs {
    pub(super) fn new(
        repo_env: Arc<BTreeMap<String, String>>,
        recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>>,
    ) -> Self {
        Self {
            repo_env,
            recorded_inputs,
        }
    }
}

impl Display for FrozenStarlarkRepositoryOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository_os>")
    }
}

starlark_simple_value!(FrozenStarlarkRepositoryOs);

#[starlark_value(type = "repository_os")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkRepositoryOs {
    type Canonical = StarlarkRepositoryOs;

    fn dir_attr(&self) -> Vec<String> {
        vec!["arch".to_owned(), "environ".to_owned(), "name".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "arch" => Some(heap.alloc(repository_os_arch(&self.repo_env, &self.recorded_inputs))),
            "environ" => Some(host_environ(heap, &self.repo_env, &self.recorded_inputs)),
            "name" => Some(heap.alloc(repository_os_name(&self.repo_env, &self.recorded_inputs))),
            _ => None,
        }
    }
}

#[derive(Debug, Display, ProvidesStaticType, NoSerialize, Allocative)]
#[display("<repo_metadata>")]
pub(crate) struct StarlarkRepositoryMetadata {
    #[allow(dead_code)]
    pub(super) reproducible: bool,
}

impl StarlarkRepositoryMetadata {
    pub(crate) fn reproducible(&self) -> bool {
        self.reproducible
    }
}

starlark_simple_value!(StarlarkRepositoryMetadata);

#[starlark_value(type = "repo_metadata")]
impl<'v> StarlarkValue<'v> for StarlarkRepositoryMetadata {}

#[derive(Debug, Display, ProvidesStaticType, NoSerialize, Allocative)]
#[display("<extension_metadata>")]
pub(crate) struct StarlarkModuleExtensionMetadata {
    #[allow(dead_code)]
    pub(super) reproducible: bool,
}

impl StarlarkModuleExtensionMetadata {
    pub(crate) fn reproducible(&self) -> bool {
        self.reproducible
    }
}

starlark_simple_value!(StarlarkModuleExtensionMetadata);

#[starlark_value(type = "extension_metadata")]
impl<'v> StarlarkValue<'v> for StarlarkModuleExtensionMetadata {}
