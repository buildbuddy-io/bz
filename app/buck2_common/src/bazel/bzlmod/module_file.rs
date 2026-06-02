use super::*;

#[derive(Clone, Debug)]
pub(super) struct BzlmodCompiledModuleFile {
    pub(super) module_file: String,
    pub(super) module_text: String,
    pub(super) ast: AstModule,
    pub(super) includes: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct BzlmodEvaluatedModuleFile {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) repo_name: String,
    pub(super) aliases: Vec<String>,
    pub(super) deps: Vec<BazelDep>,
    pub(super) archive_overrides: BTreeMap<String, BzlmodArchiveOverride>,
    pub(super) single_version_overrides: BTreeMap<String, BzlmodSingleVersionOverride>,
    pub(super) local_path_overrides: BTreeMap<String, BzlmodLocalPathOverride>,
    pub(super) extension_usages: Vec<BzlmodExtensionUsage>,
    pub(super) use_repo_rule_invocations: Vec<BzlmodUseRepoRuleInvocation>,
    pub(super) registered_toolchains: Vec<String>,
}

#[derive(Debug)]
pub(super) struct BzlmodModuleEvalOptions {
    pub(super) is_root: bool,
    pub(super) allow_include: bool,
    pub(super) ignore_dev_dependency: bool,
    pub(super) default_name: String,
    pub(super) default_version: String,
    pub(super) default_repo_name: String,
    pub(super) cell_project_path: Option<ProjectRelativePathBuf>,
    pub(super) included_modules: BTreeMap<String, Arc<BzlmodCompiledModuleFile>>,
}

#[derive(Debug, ProvidesStaticType)]
struct BzlmodModuleEvalContext {
    current_module_file: RefCell<String>,
    is_root: bool,
    allow_include: bool,
    ignore_dev_dependency: bool,
    cell_project_path: Option<ProjectRelativePathBuf>,
    included_modules: BTreeMap<String, Arc<BzlmodCompiledModuleFile>>,
    module_called: Cell<bool>,
    non_module_called: Cell<bool>,
    name: RefCell<String>,
    version: RefCell<String>,
    repo_name: RefCell<String>,
    aliases: RefCell<Vec<String>>,
    deps: RefCell<Vec<BazelDep>>,
    archive_overrides: RefCell<BTreeMap<String, BzlmodArchiveOverride>>,
    single_version_overrides: RefCell<BTreeMap<String, BzlmodSingleVersionOverride>>,
    local_path_overrides: RefCell<BTreeMap<String, BzlmodLocalPathOverride>>,
    extension_usages: RefCell<Vec<BzlmodExtensionUsage>>,
    use_repo_rule_invocations: RefCell<Vec<BzlmodUseRepoRuleInvocation>>,
    registered_toolchains: RefCell<Vec<String>>,
}

impl BzlmodModuleEvalContext {
    fn new(module_file: String, options: BzlmodModuleEvalOptions) -> Self {
        Self {
            current_module_file: RefCell::new(module_file),
            is_root: options.is_root,
            allow_include: options.allow_include,
            ignore_dev_dependency: options.ignore_dev_dependency,
            cell_project_path: options.cell_project_path,
            included_modules: options.included_modules,
            module_called: Cell::new(false),
            non_module_called: Cell::new(false),
            name: RefCell::new(options.default_name),
            version: RefCell::new(options.default_version),
            repo_name: RefCell::new(options.default_repo_name),
            aliases: RefCell::new(Vec::new()),
            deps: RefCell::new(Vec::new()),
            archive_overrides: RefCell::new(BTreeMap::new()),
            single_version_overrides: RefCell::new(BTreeMap::new()),
            local_path_overrides: RefCell::new(BTreeMap::new()),
            extension_usages: RefCell::new(Vec::new()),
            use_repo_rule_invocations: RefCell::new(Vec::new()),
            registered_toolchains: RefCell::new(Vec::new()),
        }
    }

    fn set_non_module_called(&self) {
        self.non_module_called.set(true);
    }

    fn current_module_file(&self) -> String {
        self.current_module_file.borrow().clone()
    }

    fn should_ignore_dev_dependency(&self, dev_dependency: bool) -> bool {
        self.ignore_dev_dependency && dev_dependency
    }

    fn set_extension_proxy_name(
        &self,
        usage_id: Option<usize>,
        proxy_name: &str,
    ) -> starlark::Result<()> {
        let Some(usage_id) = usage_id else {
            return Ok(());
        };
        let mut usages = self.extension_usages.borrow_mut();
        let Some(usage) = usages.get_mut(usage_id) else {
            return Err(bzlmod_starlark_error(format!(
                "internal error: unknown bzlmod extension usage id `{usage_id}`"
            )));
        };
        usage.proxy_name = proxy_name.to_owned();
        Ok(())
    }

    fn include(&self, include_label: &str) -> starlark::Result<()> {
        if !self.allow_include {
            return Err(bzlmod_starlark_error(
                "trying to call `include()` from a registry module",
            ));
        }
        let Some(compiled) = self.included_modules.get(include_label) else {
            return Err(bzlmod_starlark_error(format!(
                "internal error: included file `{include_label}` was not precompiled"
            )));
        };
        let previous_module_file = self
            .current_module_file
            .replace(compiled.module_file.clone());
        let result = Module::with_temp_heap(|module| {
            let globals = bzlmod_module_globals();
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(self);
            eval.eval_module(compiled.ast.clone(), &globals)?;
            starlark::Result::Ok(())
        });
        self.current_module_file.replace(previous_module_file);
        result
    }

    fn into_result(self) -> BzlmodEvaluatedModuleFile {
        BzlmodEvaluatedModuleFile {
            name: self.name.into_inner(),
            version: self.version.into_inner(),
            repo_name: self.repo_name.into_inner(),
            aliases: self.aliases.into_inner(),
            deps: self.deps.into_inner(),
            archive_overrides: self.archive_overrides.into_inner(),
            single_version_overrides: self.single_version_overrides.into_inner(),
            local_path_overrides: self.local_path_overrides.into_inner(),
            extension_usages: self.extension_usages.into_inner(),
            use_repo_rule_invocations: self.use_repo_rule_invocations.into_inner(),
            registered_toolchains: self.registered_toolchains.into_inner(),
        }
    }
}

fn bzlmod_module_dialect() -> Dialect {
    Dialect {
        enable_def: false,
        enable_lambda: false,
        enable_load: false,
        enable_keyword_only_arguments: true,
        enable_types: DialectTypes::Disable,
        enable_load_reexport: false,
        enable_top_level_stmt: false,
        enable_f_strings: buck2_core::is_open_source(),
        ..Dialect::Standard
    }
}

pub(super) fn compile_bzlmod_module_file(
    module_file: String,
    module_text: String,
) -> buck2_error::Result<BzlmodCompiledModuleFile> {
    let ast = AstModule::parse(&module_file, module_text.clone(), &bzlmod_module_dialect())
        .map_err(|error| buck2_error!(buck2_error::ErrorTag::Input, "{}", error))
        .with_buck_error_context(|| format!("Error parsing `{module_file}` as MODULE.bazel"))?;
    let includes = bzlmod_module_includes_from_ast(&module_file, &ast)?;
    Ok(BzlmodCompiledModuleFile {
        module_file,
        module_text,
        ast,
        includes,
    })
}

pub(super) fn eval_bzlmod_module_file(
    compiled: &BzlmodCompiledModuleFile,
    options: BzlmodModuleEvalOptions,
) -> buck2_error::Result<BzlmodEvaluatedModuleFile> {
    let context = BzlmodModuleEvalContext::new(compiled.module_file.clone(), options);
    Module::with_temp_heap(|module| {
        let globals = bzlmod_module_globals();
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&context);
        eval.eval_module(compiled.ast.clone(), &globals)?;
        starlark::Result::Ok(())
    })
    .map_err(|error| buck2_error!(buck2_error::ErrorTag::Input, "{}", error))
    .with_buck_error_context(|| {
        format!(
            "Error evaluating `{}` as MODULE.bazel",
            compiled.module_file
        )
    })?;
    Ok(context.into_result())
}

fn bzlmod_module_includes_from_ast(
    module_file: &str,
    ast: &AstModule,
) -> buck2_error::Result<Vec<String>> {
    let mut includes = Vec::new();
    let mut include_was_assigned = false;
    bzlmod_validate_stmt_for_includes(
        module_file,
        ast.statement(),
        &mut include_was_assigned,
        &mut includes,
    )?;
    Ok(includes)
}

fn bzlmod_validate_stmt_for_includes(
    module_file: &str,
    stmt: &AstStmt,
    include_was_assigned: &mut bool,
    includes: &mut Vec<String>,
) -> buck2_error::Result<()> {
    match &stmt.node {
        StmtP::Statements(stmts) => {
            for stmt in stmts {
                bzlmod_validate_stmt_for_includes(
                    module_file,
                    stmt,
                    include_was_assigned,
                    includes,
                )?;
            }
        }
        StmtP::Expression(expr) => {
            if !*include_was_assigned
                && let ExprP::Call(function, args) = &expr.node
                && bzlmod_expr_is_identifier(function, "include")
            {
                includes.push(bzlmod_include_arg(module_file, args)?);
                return Ok(());
            }
            bzlmod_validate_expr_for_module_file(module_file, expr, *include_was_assigned)?;
        }
        StmtP::Assign(assign) => {
            bzlmod_validate_expr_for_module_file(module_file, &assign.rhs, *include_was_assigned)?;
            if !*include_was_assigned && bzlmod_assign_target_is_identifier(&assign.lhs, "include")
            {
                *include_was_assigned = true;
            } else {
                bzlmod_validate_assign_target_for_include(
                    module_file,
                    &assign.lhs,
                    *include_was_assigned,
                )?;
            }
        }
        StmtP::AssignModify(target, _op, expr) => {
            bzlmod_validate_assign_target_for_include(module_file, target, *include_was_assigned)?;
            bzlmod_validate_expr_for_module_file(module_file, expr, *include_was_assigned)?;
        }
        StmtP::If(..) | StmtP::IfElse(..) => {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid MODULE.bazel syntax in `{}`: `if` statements are not allowed",
                module_file
            ));
        }
        StmtP::For(..) => {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid MODULE.bazel syntax in `{}`: `for` statements are not allowed",
                module_file
            ));
        }
        StmtP::Def(..) => {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid MODULE.bazel syntax in `{}`: functions may not be defined",
                module_file
            ));
        }
        StmtP::Return(..) => {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid MODULE.bazel syntax in `{}`: return statements are not allowed",
                module_file
            ));
        }
        StmtP::Load(_) => {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid MODULE.bazel syntax in `{}`: `load` statements may not be used",
                module_file
            ));
        }
        StmtP::Break | StmtP::Continue | StmtP::Pass => {}
    }
    Ok(())
}

fn bzlmod_validate_expr_for_module_file(
    module_file: &str,
    expr: &AstExpr,
    include_was_assigned: bool,
) -> buck2_error::Result<()> {
    match &expr.node {
        ExprP::Identifier(ident) if !include_was_assigned && ident.ident == "include" => {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid MODULE.bazel syntax in `{}`: the `include` directive must be called directly at the top level",
                module_file
            ));
        }
        ExprP::Call(function, args) => {
            bzlmod_validate_call_args(module_file, args)?;
            bzlmod_validate_expr_for_module_file(module_file, function, include_was_assigned)?;
            for arg in &args.args {
                bzlmod_validate_expr_for_module_file(
                    module_file,
                    arg.node.expr(),
                    include_was_assigned,
                )?;
            }
        }
        ExprP::Tuple(items) | ExprP::List(items) => {
            for item in items {
                bzlmod_validate_expr_for_module_file(module_file, item, include_was_assigned)?;
            }
        }
        ExprP::Dot(base, _)
        | ExprP::Not(base)
        | ExprP::Minus(base)
        | ExprP::Plus(base)
        | ExprP::BitNot(base) => {
            bzlmod_validate_expr_for_module_file(module_file, base, include_was_assigned)?;
        }
        ExprP::FString(fstring) => {
            for expr in &fstring.node.expressions {
                bzlmod_validate_expr_for_module_file(module_file, expr, include_was_assigned)?;
            }
        }
        ExprP::Index(index) => {
            bzlmod_validate_expr_for_module_file(module_file, &index.0, include_was_assigned)?;
            bzlmod_validate_expr_for_module_file(module_file, &index.1, include_was_assigned)?;
        }
        ExprP::Index2(index) => {
            bzlmod_validate_expr_for_module_file(module_file, &index.0, include_was_assigned)?;
            bzlmod_validate_expr_for_module_file(module_file, &index.1, include_was_assigned)?;
            bzlmod_validate_expr_for_module_file(module_file, &index.2, include_was_assigned)?;
        }
        ExprP::Slice(base, start, stop, stride) => {
            bzlmod_validate_expr_for_module_file(module_file, base, include_was_assigned)?;
            for expr in [start, stop, stride].into_iter().flatten() {
                bzlmod_validate_expr_for_module_file(module_file, expr, include_was_assigned)?;
            }
        }
        ExprP::Op(left, _op, right) => {
            bzlmod_validate_expr_for_module_file(module_file, left, include_was_assigned)?;
            bzlmod_validate_expr_for_module_file(module_file, right, include_was_assigned)?;
        }
        ExprP::If(exprs) => {
            bzlmod_validate_expr_for_module_file(module_file, &exprs.0, include_was_assigned)?;
            bzlmod_validate_expr_for_module_file(module_file, &exprs.1, include_was_assigned)?;
            bzlmod_validate_expr_for_module_file(module_file, &exprs.2, include_was_assigned)?;
        }
        ExprP::Dict(items) => {
            for (key, value) in items {
                bzlmod_validate_expr_for_module_file(module_file, key, include_was_assigned)?;
                bzlmod_validate_expr_for_module_file(module_file, value, include_was_assigned)?;
            }
        }
        ExprP::ListComprehension(body, clause, clauses) => {
            bzlmod_validate_expr_for_module_file(module_file, body, include_was_assigned)?;
            bzlmod_validate_for_clause_for_module_file(module_file, clause, include_was_assigned)?;
            for clause in clauses {
                bzlmod_validate_clause_for_module_file(module_file, clause, include_was_assigned)?;
            }
        }
        ExprP::DictComprehension(body, clause, clauses) => {
            bzlmod_validate_expr_for_module_file(module_file, &body.0, include_was_assigned)?;
            bzlmod_validate_expr_for_module_file(module_file, &body.1, include_was_assigned)?;
            bzlmod_validate_for_clause_for_module_file(module_file, clause, include_was_assigned)?;
            for clause in clauses {
                bzlmod_validate_clause_for_module_file(module_file, clause, include_was_assigned)?;
            }
        }
        ExprP::Lambda(_) => {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid MODULE.bazel syntax in `{}`: functions may not be defined",
                module_file
            ));
        }
        ExprP::Identifier(_) | ExprP::Literal(_) => {}
    }
    Ok(())
}

fn bzlmod_validate_for_clause_for_module_file(
    module_file: &str,
    clause: &ForClauseP<AstNoPayload>,
    include_was_assigned: bool,
) -> buck2_error::Result<()> {
    bzlmod_validate_assign_target_for_include(module_file, &clause.var, include_was_assigned)?;
    bzlmod_validate_expr_for_module_file(module_file, &clause.over, include_was_assigned)
}

fn bzlmod_validate_clause_for_module_file(
    module_file: &str,
    clause: &ClauseP<AstNoPayload>,
    include_was_assigned: bool,
) -> buck2_error::Result<()> {
    match clause {
        ClauseP::For(clause) => {
            bzlmod_validate_for_clause_for_module_file(module_file, clause, include_was_assigned)
        }
        ClauseP::If(expr) => {
            bzlmod_validate_expr_for_module_file(module_file, expr, include_was_assigned)
        }
    }
}

fn bzlmod_validate_assign_target_for_include(
    module_file: &str,
    target: &AstAssignTarget,
    include_was_assigned: bool,
) -> buck2_error::Result<()> {
    if include_was_assigned {
        return Ok(());
    }
    match &target.node {
        AssignTargetP::Tuple(items) => {
            for item in items {
                bzlmod_validate_assign_target_for_include(module_file, item, include_was_assigned)?;
            }
        }
        AssignTargetP::Index(index) => {
            bzlmod_validate_expr_for_module_file(module_file, &index.0, include_was_assigned)?;
            bzlmod_validate_expr_for_module_file(module_file, &index.1, include_was_assigned)?;
        }
        AssignTargetP::Dot(base, _) => {
            bzlmod_validate_expr_for_module_file(module_file, base, include_was_assigned)?;
        }
        AssignTargetP::Identifier(ident) if ident.ident == "include" => {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid MODULE.bazel syntax in `{}`: the `include` directive must be called directly at the top level",
                module_file
            ));
        }
        AssignTargetP::Identifier(_) => {}
    }
    Ok(())
}

fn bzlmod_validate_call_args(
    module_file: &str,
    args: &CallArgsP<AstNoPayload>,
) -> buck2_error::Result<()> {
    for arg in &args.args {
        match &arg.node {
            ArgumentP::Args(_) => {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Invalid MODULE.bazel syntax in `{}`: *args arguments are not allowed",
                    module_file
                ));
            }
            ArgumentP::KwArgs(value) if !matches!(value.node, ExprP::Dict(_)) => {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Invalid MODULE.bazel syntax in `{}`: **kwargs arguments must be a literal dict",
                    module_file
                ));
            }
            ArgumentP::KwArgs(_) | ArgumentP::Named(_, _) | ArgumentP::Positional(_) => {}
        }
    }
    Ok(())
}

fn bzlmod_include_arg(
    module_file: &str,
    args: &CallArgsP<AstNoPayload>,
) -> buck2_error::Result<String> {
    let [arg] = args.args.as_slice() else {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "Invalid MODULE.bazel syntax in `{}`: include() must be called with exactly one positional string literal",
            module_file
        ));
    };
    let ArgumentP::Positional(expr) = &arg.node else {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "Invalid MODULE.bazel syntax in `{}`: include() must be called with exactly one positional string literal",
            module_file
        ));
    };
    let ExprP::Literal(AstLiteral::String(label)) = &expr.node else {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "Invalid MODULE.bazel syntax in `{}`: include() must be called with exactly one positional string literal",
            module_file
        ));
    };
    bzlmod_include_label_to_path(module_file, &label.node)?;
    Ok(label.node.clone())
}

fn bzlmod_expr_is_identifier(expr: &AstExpr, name: &str) -> bool {
    matches!(&expr.node, ExprP::Identifier(ident) if ident.ident == name)
}

fn bzlmod_assign_target_is_identifier(target: &AstAssignTarget, name: &str) -> bool {
    matches!(&target.node, AssignTargetP::Identifier(ident) if ident.ident == name)
}

pub(super) fn bzlmod_include_label_to_path(
    module_file: &str,
    label: &str,
) -> buck2_error::Result<String> {
    if !label.starts_with("//") {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "bad include label `{}` in `{}`: include() must be called with repo-relative labels starting with `//`",
            label,
            module_file
        ));
    }
    let path = module_include_to_path(module_file, label).ok_or_else(|| {
        buck2_error!(
            buck2_error::ErrorTag::Input,
            "bad include label `{}` in `{}`: invalid repo-relative label",
            label,
            module_file
        )
    })?;
    let basename = path.rsplit('/').next().unwrap_or(path.as_str());
    if !basename.ends_with(".MODULE.bazel") {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "bad include label `{}` in `{}`: the file to be included must have a name ending in `.MODULE.bazel`",
            label,
            module_file
        ));
    }
    if basename.starts_with('.') {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "bad include label `{}` in `{}`: the name of the file to be included must not start with `.`",
            label,
            module_file
        ));
    }
    Ok(path)
}

fn module_include_to_path(current_module_file: &str, label: &str) -> Option<String> {
    if label.starts_with('@') {
        return None;
    }

    if let Some(rest) = label.strip_prefix("//") {
        let (package, name) = rest.split_once(':')?;
        return Some(if package.is_empty() {
            name.to_owned()
        } else {
            format!("{package}/{name}")
        });
    }

    if let Some(name) = label.strip_prefix(':') {
        let base = current_module_file.rsplit_once('/').map(|(base, _)| base);
        return Some(match base {
            Some(base) => format!("{base}/{name}"),
            None => name.to_owned(),
        });
    }

    None
}

fn bzlmod_module_globals() -> Globals {
    GlobalsBuilder::extended_by(&[LibraryExtension::Print])
        .with(bzlmod_module_globals_builder)
        .build()
}

fn bzlmod_starlark_error(message: impl fmt::Display) -> starlark::Error {
    buck2_error!(buck2_error::ErrorTag::Input, "{}", message).into()
}

fn bzlmod_eval_context<'v, 'a, 'e>(
    eval: &Evaluator<'v, 'a, 'e>,
) -> starlark::Result<&'a BzlmodModuleEvalContext> {
    eval.extra
        .ok_or_else(|| bzlmod_starlark_error("internal error: missing bzlmod evaluation context"))?
        .downcast_ref::<BzlmodModuleEvalContext>()
        .ok_or_else(|| {
            bzlmod_starlark_error("internal error: wrong bzlmod evaluation context type")
        })
}

fn bzlmod_value_to_string(value: Value<'_>, what: &str) -> starlark::Result<String> {
    value.unpack_str().map(str::to_owned).ok_or_else(|| {
        bzlmod_starlark_error(format!(
            "{what} must be a string, got `{}` of type `{}`",
            value.to_repr(),
            value.get_type()
        ))
    })
}

fn bzlmod_value_to_bool(value: Value<'_>, what: &str) -> starlark::Result<bool> {
    value.unpack_bool().ok_or_else(|| {
        bzlmod_starlark_error(format!(
            "{what} must be a bool, got `{}` of type `{}`",
            value.to_repr(),
            value.get_type()
        ))
    })
}

fn bzlmod_value_to_i32(value: Value<'_>, what: &str) -> starlark::Result<i32> {
    value.unpack_i32().ok_or_else(|| {
        bzlmod_starlark_error(format!(
            "{what} must be an int, got `{}` of type `{}`",
            value.to_repr(),
            value.get_type()
        ))
    })
}

fn bzlmod_value_to_string_list(value: Value<'_>, what: &str) -> starlark::Result<Vec<String>> {
    if value.is_none() {
        return Ok(Vec::new());
    }
    if let Some(list) = ListRef::from_value(value) {
        return list
            .iter()
            .map(|value| bzlmod_value_to_string(value, what))
            .collect();
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return tuple
            .iter()
            .map(|value| bzlmod_value_to_string(value, what))
            .collect();
    }
    Err(bzlmod_starlark_error(format!(
        "{what} must be a list or tuple of strings, got `{}` of type `{}`",
        value.to_repr(),
        value.get_type()
    )))
}

fn bzlmod_kwarg<'v>(kwargs: &SmallMap<String, Value<'v>>, name: &str) -> Option<Value<'v>> {
    kwargs
        .iter()
        .find_map(|(key, value)| (key == name).then_some(*value))
}

fn bzlmod_kwarg_string(
    kwargs: &SmallMap<String, Value<'_>>,
    name: &str,
    what: &str,
) -> starlark::Result<Option<String>> {
    bzlmod_kwarg(kwargs, name)
        .map(|value| bzlmod_value_to_string(value, what))
        .transpose()
}

fn bzlmod_kwarg_string_list(
    kwargs: &SmallMap<String, Value<'_>>,
    name: &str,
    what: &str,
) -> starlark::Result<Vec<String>> {
    bzlmod_kwarg(kwargs, name)
        .map(|value| bzlmod_value_to_string_list(value, what))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn bzlmod_kwarg_u32(
    kwargs: &SmallMap<String, Value<'_>>,
    name: &str,
    what: &str,
) -> starlark::Result<Option<u32>> {
    bzlmod_kwarg(kwargs, name)
        .map(|value| {
            let value = bzlmod_value_to_i32(value, what)?;
            u32::try_from(value).map_err(|_| {
                bzlmod_starlark_error(format!("{what} must be non-negative, got `{value}`"))
            })
        })
        .transpose()
}

fn bzlmod_override_patch_paths_from_value(
    current_module_file: &str,
    value: Value<'_>,
    what: &str,
) -> starlark::Result<Vec<BzlmodRootPatch>> {
    bzlmod_value_to_string_list(value, what)?
        .into_iter()
        .map(|label| {
            let path = module_include_to_path(current_module_file, &label).ok_or_else(|| {
                bzlmod_starlark_error(format!(
                    "{what} patch must be a root-module label, got `{label}`"
                ))
            })?;
            Ok(BzlmodRootPatch {
                path,
                content: Arc::from(""),
            })
        })
        .collect()
}

fn bzlmod_patch_paths_from_kwargs(
    current_module_file: &str,
    kwargs: &SmallMap<String, Value<'_>>,
    what: &str,
) -> starlark::Result<Vec<BzlmodRootPatch>> {
    bzlmod_kwarg(kwargs, "patches")
        .map(|value| bzlmod_override_patch_paths_from_value(current_module_file, value, what))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn bzlmod_no_positional_args<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<()> {
    args.no_positional_args(eval.heap())
}

fn bzlmod_repository_rule_attr_expression_from_value(value: Value<'_>) -> starlark::Result<String> {
    if let Some(string) = value.unpack_str() {
        return bzlmod_repository_rule_string_attr_expression(string).map_err(Into::into);
    }
    if let Some(list) = ListRef::from_value(value) {
        let values = list
            .iter()
            .map(|value| {
                let string = bzlmod_value_to_string(value, "use_repo_rule string-list attribute")?;
                bzlmod_repository_rule_string_attr_expression(&string).map_err(Into::into)
            })
            .collect::<starlark::Result<Vec<_>>>()?;
        return Ok(format!("[{}]", values.join(", ")));
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        let values = tuple
            .iter()
            .map(|value| {
                let string = bzlmod_value_to_string(value, "use_repo_rule string-list attribute")?;
                bzlmod_repository_rule_string_attr_expression(&string).map_err(Into::into)
            })
            .collect::<starlark::Result<Vec<_>>>()?;
        return Ok(format!("[{}]", values.join(", ")));
    }
    Ok(value.to_repr())
}

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
struct BzlmodExtensionProxy {
    usage_id: Option<usize>,
}

impl fmt::Display for BzlmodExtensionProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<module_extension_proxy>")
    }
}

starlark_simple_value!(BzlmodExtensionProxy);

#[starlark_value(type = "module_extension_proxy")]
impl<'v> StarlarkValue<'v> for BzlmodExtensionProxy {
    fn export_as(
        &self,
        variable_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<()> {
        bzlmod_eval_context(eval)?.set_extension_proxy_name(self.usage_id, variable_name)
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(BzlmodExtensionTagCallable {
            usage_id: self.usage_id,
            tag_name: attribute.to_owned(),
        }))
    }
}

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
struct BzlmodExtensionTagCallable {
    usage_id: Option<usize>,
    tag_name: String,
}

impl fmt::Display for BzlmodExtensionTagCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_extension_tag {}>", self.tag_name)
    }
}

starlark_simple_value!(BzlmodExtensionTagCallable);

#[starlark_value(type = "module_extension_tag")]
impl<'v> StarlarkValue<'v> for BzlmodExtensionTagCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        bzlmod_no_positional_args(args, eval)?;
        let Some(usage_id) = self.usage_id else {
            return Ok(Value::new_none());
        };
        let kwargs = args.names_map()?;
        let context = bzlmod_eval_context(eval)?;
        let mut usages = context.extension_usages.borrow_mut();
        let Some(usage) = usages.get_mut(usage_id) else {
            return Err(bzlmod_starlark_error(format!(
                "internal error: unknown bzlmod extension usage id `{usage_id}`"
            )));
        };
        usage.tags.push(BzlmodExtensionTag {
            tag_name: self.tag_name.clone(),
            bindings: Vec::new(),
            kwargs: kwargs
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.to_repr()))
                .collect(),
        });
        Ok(Value::new_none())
    }
}

#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative)]
struct BzlmodUseRepoRuleCallable {
    rule_bzl_file: String,
    rule_name: String,
}

impl fmt::Display for BzlmodUseRepoRuleCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repo_rule {}%{}>", self.rule_bzl_file, self.rule_name)
    }
}

starlark_simple_value!(BzlmodUseRepoRuleCallable);

#[starlark_value(type = "repo_rule")]
impl<'v> StarlarkValue<'v> for BzlmodUseRepoRuleCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        bzlmod_no_positional_args(args, eval)?;
        let kwargs = args.names_map()?;
        let dev_dependency = kwargs
            .iter()
            .find_map(|(name, value)| {
                (name.as_str() == "dev_dependency")
                    .then_some(bzlmod_value_to_bool(*value, "use_repo_rule dev_dependency"))
            })
            .transpose()?
            .unwrap_or(false);
        let context = bzlmod_eval_context(eval)?;
        if context.should_ignore_dev_dependency(dev_dependency) {
            return Ok(Value::new_none());
        }
        let repo_name = kwargs
            .iter()
            .find_map(|(name, value)| {
                (name.as_str() == "name")
                    .then_some(bzlmod_value_to_string(*value, "use_repo_rule name"))
            })
            .transpose()?
            .ok_or_else(|| {
                bzlmod_starlark_error("use_repo_rule invocation must have a string `name`")
            })?;
        let mut attrs = kwargs
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "name" | "dev_dependency" | "visibility"))
            .map(|(name, value)| {
                Ok((
                    name.as_str().to_owned(),
                    bzlmod_repository_rule_attr_expression_from_value(*value)?,
                ))
            })
            .collect::<starlark::Result<Vec<_>>>()?;
        attrs.sort_by(|left, right| left.0.cmp(&right.0));
        context
            .use_repo_rule_invocations
            .borrow_mut()
            .push(BzlmodUseRepoRuleInvocation {
                rule_bzl_file: self.rule_bzl_file.clone(),
                rule_name: self.rule_name.clone(),
                repo_name,
                attrs,
            });
        Ok(Value::new_none())
    }
}

#[starlark_module]
fn bzlmod_module_globals_builder(builder: &mut GlobalsBuilder) {
    fn module<'v>(
        #[starlark(require = named, default = String::new())] name: String,
        #[starlark(require = named, default = String::new())] version: String,
        #[starlark(require = named, default = -1)] compatibility_level: i32,
        #[starlark(require = named, default = String::new())] repo_name: String,
        #[starlark(require = named, default = NoneOr::None)] bazel_compatibility: NoneOr<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        if context.module_called.get() {
            return Err(bzlmod_starlark_error(
                "the module() directive can only be called once",
            ));
        }
        if context.non_module_called.get() {
            return Err(bzlmod_starlark_error(
                "if module() is called, it must be called before any other functions",
            ));
        }
        if compatibility_level != -1 {
            // Bazel warns that this is a no-op. Buck2 does not currently surface this warning path.
        }
        if let NoneOr::Other(bazel_compatibility) = bazel_compatibility {
            let _ = bzlmod_value_to_string_list(bazel_compatibility, "module bazel_compatibility")?;
        }
        context.module_called.set(true);
        let module_name = if name.is_empty() {
            context.name.borrow().clone()
        } else {
            if !is_valid_bzlmod_module_name(&name) {
                return Err(bzlmod_starlark_error(format!(
                    "invalid module name `{name}` in module()"
                )));
            }
            name
        };
        parse_bzlmod_version(&version)
            .map_err(Into::<starlark::Error>::into)
            .map_err(|error| {
                bzlmod_starlark_error(format!("Invalid version in module(): {error}"))
            })?;
        let module_repo_name = if repo_name.is_empty() {
            module_name.clone()
        } else {
            repo_name
        };
        *context.name.borrow_mut() = module_name.clone();
        *context.version.borrow_mut() = version;
        *context.repo_name.borrow_mut() = module_repo_name.clone();
        let mut aliases = context.aliases.borrow_mut();
        aliases.push(module_name);
        aliases.push(module_repo_name);
        Ok(NoneType)
    }

    fn bazel_dep(
        #[starlark(require = named)] name: String,
        #[starlark(require = named, default = String::new())] version: String,
        #[starlark(require = named, default = -1)] max_compatibility_level: i32,
        #[starlark(require = named, default = NoneOr::Other(String::new()))] repo_name: NoneOr<
            String,
        >,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        if !is_valid_bzlmod_module_name(&name) {
            return Err(bzlmod_starlark_error(format!(
                "invalid module name `{name}` in bazel_dep()"
            )));
        }
        parse_bzlmod_version(&version)
            .map_err(Into::<starlark::Error>::into)
            .map_err(|error| {
                bzlmod_starlark_error(format!("Invalid version in bazel_dep(): {error}"))
            })?;
        if max_compatibility_level != -1 {
            // Bazel warns that this is a no-op. Buck2 does not currently surface this warning path.
        }
        if context.should_ignore_dev_dependency(dev_dependency) {
            return Ok(NoneType);
        }
        let apparent_name = match repo_name {
            NoneOr::None => None,
            NoneOr::Other(repo_name) if repo_name.is_empty() => Some(name.clone()),
            NoneOr::Other(repo_name) => Some(repo_name),
        };
        context.deps.borrow_mut().push(BazelDep {
            name,
            version,
            apparent_name,
        });
        Ok(NoneType)
    }

    fn register_toolchains(
        #[starlark(args)] args: UnpackTuple<String>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        if !context.should_ignore_dev_dependency(dev_dependency) {
            context
                .registered_toolchains
                .borrow_mut()
                .extend(args.items);
        }
        Ok(NoneType)
    }

    fn register_execution_platforms(
        #[starlark(args)] _args: UnpackTuple<String>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        let _ = dev_dependency;
        Ok(NoneType)
    }

    fn use_extension(
        extension_bzl_file: String,
        extension_name: String,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        #[starlark(require = named, default = false)] isolate: bool,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<BzlmodExtensionProxy> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        let _ = isolate;
        if context.should_ignore_dev_dependency(dev_dependency) {
            return Ok(BzlmodExtensionProxy { usage_id: None });
        }
        let mut usages = context.extension_usages.borrow_mut();
        let usage_id = usages.len();
        usages.push(BzlmodExtensionUsage {
            proxy_name: String::new(),
            extension_bzl_file,
            extension_name,
            dev_dependency,
            imports: Vec::new(),
            repo_overrides: Vec::new(),
            tags: Vec::new(),
        });
        Ok(BzlmodExtensionProxy {
            usage_id: Some(usage_id),
        })
    }

    fn use_repo<'v>(
        extension_proxy: Value<'v>,
        #[starlark(args)] args: UnpackTuple<String>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        let Some(proxy) = BzlmodExtensionProxy::from_value(extension_proxy) else {
            return Err(bzlmod_starlark_error(format!(
                "use_repo() expected a module extension proxy, got `{}` of type `{}`",
                extension_proxy.to_repr(),
                extension_proxy.get_type()
            )));
        };
        let Some(usage_id) = proxy.usage_id else {
            return Ok(NoneType);
        };
        let module_name = context.name.borrow().clone();
        let module_version = context.version.borrow().clone();
        let mut imports = args
            .items
            .into_iter()
            .map(|repo_name| BzlmodUseRepoImport {
                alias: repo_name.clone(),
                repo_name,
            })
            .collect::<Vec<_>>();
        for (alias, value) in kwargs.iter() {
            imports.push(BzlmodUseRepoImport {
                alias: alias.clone(),
                repo_name: bzlmod_value_to_string(*value, "use_repo repo name")?
                    .replace("{name}", &module_name)
                    .replace("{version}", &module_version),
            });
        }
        let mut usages = context.extension_usages.borrow_mut();
        let Some(usage) = usages.get_mut(usage_id) else {
            return Err(bzlmod_starlark_error(format!(
                "internal error: unknown bzlmod extension usage id `{usage_id}`"
            )));
        };
        usage.imports.extend(imports);
        Ok(NoneType)
    }

    fn override_repo<'v>(
        extension_proxy: Value<'v>,
        #[starlark(args)] args: UnpackTuple<String>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        bzlmod_extension_repo_overrides_from_eval(extension_proxy, args, kwargs, true, eval)
    }

    fn inject_repo<'v>(
        extension_proxy: Value<'v>,
        #[starlark(args)] args: UnpackTuple<String>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        bzlmod_extension_repo_overrides_from_eval(extension_proxy, args, kwargs, false, eval)
    }

    fn use_repo_rule(
        rule_bzl_file: String,
        rule_name: String,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<BzlmodUseRepoRuleCallable> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        Ok(BzlmodUseRepoRuleCallable {
            rule_bzl_file,
            rule_name,
        })
    }

    fn include(label: String, eval: &mut Evaluator<'_, '_, '_>) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        context.include(&label)?;
        Ok(NoneType)
    }

    fn single_version_override<'v>(
        #[starlark(require = named)] module_name: String,
        #[starlark(require = named, default = String::new())] version: String,
        #[starlark(require = named, default = String::new())] registry: String,
        #[starlark(require = named, default = NoneOr::None)] patches: NoneOr<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] patch_cmds: NoneOr<Value<'v>>,
        #[starlark(require = named, default = -1)] patch_strip: i32,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        if !context.is_root {
            return Ok(NoneType);
        }
        if !registry.is_empty() {
            return Err(bzlmod_starlark_error(format!(
                "single_version_override for module `{module_name}` uses unsupported `registry`. Bazel uses `registry` to fetch the module from a non-default registry."
            )));
        }
        let patch_cmds = match patch_cmds {
            NoneOr::None => Vec::new(),
            NoneOr::Other(patch_cmds) => {
                bzlmod_value_to_string_list(patch_cmds, "single_version_override patch_cmds")?
            }
        };
        if !patch_cmds.is_empty() {
            return Err(bzlmod_starlark_error(format!(
                "single_version_override for module `{module_name}` uses unsupported `patch_cmds`. Bazel runs `patch_cmds` after applying patch files."
            )));
        }
        let current_module_file = context.current_module_file();
        let patches = match patches {
            NoneOr::None => Vec::new(),
            NoneOr::Other(patches) => bzlmod_override_patch_paths_from_value(
                &current_module_file,
                patches,
                "single_version_override",
            )?,
        };
        let patch_strip = match patch_strip {
            -1 => None,
            value => Some(u32::try_from(value).map_err(|_| {
                bzlmod_starlark_error(format!(
                    "single_version_override for module `{module_name}` has negative patch_strip `{value}`"
                ))
            })?),
        };
        context.single_version_overrides.borrow_mut().insert(
            module_name,
            BzlmodSingleVersionOverride {
                version: (!version.is_empty()).then_some(version),
                patches,
                patch_strip,
            },
        );
        Ok(NoneType)
    }

    fn multiple_version_override<'v>(
        #[starlark(require = named)] module_name: String,
        #[starlark(require = named, default = NoneOr::None)] versions: NoneOr<Value<'v>>,
        #[starlark(require = named, default = String::new())] registry: String,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        let versions = match versions {
            NoneOr::None => Vec::new(),
            NoneOr::Other(versions) => {
                bzlmod_value_to_string_list(versions, "multiple_version_override versions")?
            }
        };
        let _ = registry;
        Err(bzlmod_starlark_error(format!(
            "multiple_version_override is not implemented in Buck2 bzlmod resolution yet. Bazel allows multiple selected versions of the same module; refusing to silently collapse that graph: module `{}` versions {:?}",
            module_name, versions
        )))
    }

    fn archive_override<'v>(
        #[starlark(require = named)] module_name: String,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        if !context.is_root {
            return Ok(NoneType);
        }
        let mut urls = Vec::new();
        if let Some(url) = bzlmod_kwarg_string(&kwargs, "url", "archive_override url")? {
            urls.push(url);
        }
        urls.extend(bzlmod_kwarg_string_list(
            &kwargs,
            "urls",
            "archive_override urls",
        )?);
        if urls.is_empty() {
            return Err(bzlmod_starlark_error(format!(
                "archive_override for module `{module_name}` must have `url` or non-empty `urls`"
            )));
        }
        let current_module_file = context.current_module_file();
        context.archive_overrides.borrow_mut().insert(
            module_name.clone(),
            BzlmodArchiveOverride {
                module_name,
                urls,
                integrity: bzlmod_kwarg_string(&kwargs, "integrity", "archive_override integrity")?
                    .unwrap_or_default(),
                strip_prefix: bzlmod_kwarg_string(
                    &kwargs,
                    "strip_prefix",
                    "archive_override strip_prefix",
                )?,
                archive_type: bzlmod_kwarg_string(&kwargs, "type", "archive_override type")?.or(
                    bzlmod_kwarg_string(&kwargs, "archive_type", "archive_override archive_type")?,
                ),
                patches: bzlmod_patch_paths_from_kwargs(
                    &current_module_file,
                    &kwargs,
                    "archive_override",
                )?,
                patch_strip: bzlmod_kwarg_u32(
                    &kwargs,
                    "patch_strip",
                    "archive_override patch_strip",
                )?,
            },
        );
        Ok(NoneType)
    }

    fn git_override<'v>(
        #[starlark(require = named)] module_name: String,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        let _ = kwargs;
        Err(bzlmod_starlark_error(format!(
            "git_override is not implemented in Buck2 bzlmod resolution yet: module `{module_name}`"
        )))
    }

    fn local_path_override(
        #[starlark(require = named)] module_name: String,
        #[starlark(require = named)] path: String,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        if !context.is_root {
            return Ok(NoneType);
        }
        let cell_project_path = context.cell_project_path.as_ref().ok_or_else(|| {
            bzlmod_starlark_error("internal error: local_path_override missing root cell path")
        })?;
        let path = cell_project_path
            .join_normalized(RelativePath::new(&path))
            .map_err(Into::<starlark::Error>::into)?;
        context.local_path_overrides.borrow_mut().insert(
            module_name.clone(),
            BzlmodLocalPathOverride {
                module_name,
                path: path.as_str().to_owned(),
                module_text: String::new(),
                included_module_texts: BTreeMap::new(),
            },
        );
        Ok(NoneType)
    }

    fn flag_alias(
        #[starlark(require = named)] name: String,
        #[starlark(require = named)] starlark_flag: String,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let context = bzlmod_eval_context(eval)?;
        context.set_non_module_called();
        let _ = (name, starlark_flag);
        Ok(NoneType)
    }
}

fn bzlmod_extension_repo_overrides_from_eval<'v>(
    extension_proxy: Value<'v>,
    args: UnpackTuple<String>,
    kwargs: SmallMap<String, Value<'v>>,
    must_exist: bool,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let context = bzlmod_eval_context(eval)?;
    context.set_non_module_called();
    let Some(proxy) = BzlmodExtensionProxy::from_value(extension_proxy) else {
        return Err(bzlmod_starlark_error(format!(
            "repo override expected a module extension proxy, got `{}` of type `{}`",
            extension_proxy.to_repr(),
            extension_proxy.get_type()
        )));
    };
    let Some(usage_id) = proxy.usage_id else {
        return Ok(NoneType);
    };
    let mut overrides = args
        .items
        .into_iter()
        .map(|repo_name| BzlmodRepoOverride {
            overriding_repo_name: repo_name.clone(),
            repo_name,
            must_exist,
        })
        .collect::<Vec<_>>();
    for (repo_name, overriding_repo_name) in kwargs.iter() {
        overrides.push(BzlmodRepoOverride {
            repo_name: repo_name.clone(),
            overriding_repo_name: bzlmod_value_to_string(*overriding_repo_name, "repo override")?,
            must_exist,
        });
    }
    let mut usages = context.extension_usages.borrow_mut();
    let Some(usage) = usages.get_mut(usage_id) else {
        return Err(bzlmod_starlark_error(format!(
            "internal error: unknown bzlmod extension usage id `{usage_id}`"
        )));
    };
    usage.repo_overrides.extend(overrides);
    usage.repo_overrides.sort_by(|left, right| {
        (
            &left.repo_name,
            &left.overriding_repo_name,
            &left.must_exist,
        )
            .cmp(&(
                &right.repo_name,
                &right.overriding_repo_name,
                &right.must_exist,
            ))
    });
    usage.repo_overrides.dedup_by(|left, right| {
        left.repo_name == right.repo_name
            && left.overriding_repo_name == right.overriding_repo_name
            && left.must_exist == right.must_exist
    });
    Ok(NoneType)
}
