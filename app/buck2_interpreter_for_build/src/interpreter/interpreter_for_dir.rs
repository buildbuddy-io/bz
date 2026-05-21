/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Implements the core skylark interpreter. This encodes the primitive
//! operations of converting file content to ASTs and evaluating import and
//! build files.

use std::cell::OnceCell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;

use allocative::Allocative;
use buck2_common::legacy_configs::configs::LegacyBuckConfig;
use buck2_common::legacy_configs::key::BuckconfigKeyRef;
use buck2_common::package_listing::PackageListingStrategy;
use buck2_common::package_listing::listing::PackageListing;
use buck2_core::build_file_path::BuildFilePath;
use buck2_core::bxl::BxlFilePath;
use buck2_core::bzl::ImportPath;
use buck2_core::cells::build_file_cell::BuildFileCell;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::cell_path_with_allowed_relative_dir::CellPathWithAllowedRelativeDir;
use buck2_core::cells::paths::CellRelativePathBuf;
use buck2_core::package::package_relative_path::PackageRelativePathBuf;
use buck2_error::BuckErrorContext;
use buck2_error::conversion::from_any_with_tag;
use buck2_error::internal_error;
use buck2_event_observer::humanized::HumanizedBytes;
use buck2_events::dispatch::get_dispatcher;
use buck2_interpreter::factory::BuckStarlarkModule;
use buck2_interpreter::factory::FinishedStarlarkEvaluation;
use buck2_interpreter::factory::StarlarkEvaluatorProvider;
use buck2_interpreter::file_loader::InterpreterFileLoader;
use buck2_interpreter::file_loader::LoadResolver;
use buck2_interpreter::file_loader::LoadedModules;
use buck2_interpreter::file_type::StarlarkFileType;
use buck2_interpreter::import_paths::ImplicitImportPaths;
use buck2_interpreter::package_imports::ImplicitImport;
use buck2_interpreter::parse_import::ParseImportOptions;
use buck2_interpreter::parse_import::RelativeImports;
use buck2_interpreter::parse_import::parse_import_with_config_and_package_root;
use buck2_interpreter::paths::module::OwnedStarlarkModulePath;
use buck2_interpreter::paths::module::StarlarkModulePath;
use buck2_interpreter::paths::package::PackageFilePath;
use buck2_interpreter::paths::path::OwnedStarlarkPath;
use buck2_interpreter::paths::path::StarlarkPath;
use buck2_interpreter::prelude_path::PreludePath;
use buck2_interpreter::print_handler::EventDispatcherPrintHandler;
use buck2_interpreter::soft_error::Buck2StarlarkSoftErrorHandler;
use buck2_interpreter::starlark_profiler::data::StarlarkProfileDataAndStats;
use buck2_node::nodes::eval_result::EvaluationResult;
use buck2_node::nodes::eval_result::EvaluationResultWithStats;
use buck2_node::super_package::SuperPackage;
use buck2_util::per_thread_instruction_counter::PerThreadInstructionCounter;
use dice::CancellationContext;
use dupe::Dupe;
use gazebo::prelude::*;
use pagable::PagablePanic;
use starlark::codemap::FileSpan;
use starlark::environment::FrozenModule;
use starlark::syntax::AstModule;
use starlark::syntax::ast::Argument;
use starlark::syntax::ast::AstExpr;
use starlark::syntax::ast::AstLiteral;
use starlark::syntax::ast::Expr;
use starlark::values::ValueLike;
use starlark::values::any_complex::StarlarkAnyComplex;

use crate::interpreter::bazel_glob::BazelGlobRequest;
use crate::interpreter::bazel_glob::BazelPackageDataRequest;
use crate::interpreter::buckconfig::BuckConfigsViewForStarlark;
use crate::interpreter::build_context::BazelRepositoryContextForStarlark;
use crate::interpreter::build_context::BazelRepositoryRecordedInput;
use crate::interpreter::build_context::BazelRepositoryRuleInvocation;
use crate::interpreter::build_context::BazelRepositoryRuleRecorder;
use crate::interpreter::build_context::BuildContext;
use crate::interpreter::build_context::PerFileTypeContext;
use crate::interpreter::bzl_eval_ctx::BzlEvalCtx;
use crate::interpreter::cell_info::InterpreterCellInfo;
use crate::interpreter::extra_value::InterpreterExtraValue;
use crate::interpreter::global_interpreter_state::GlobalInterpreterState;
use crate::interpreter::module_internals::ModuleInternals;
use crate::interpreter::package_file_extra::FrozenPackageFileExtra;
use crate::super_package::eval_ctx::PackageFileEvalCtx;

const DEFAULT_STARLARK_MEMORY_USAGE_LIMIT: u64 = 2 * (1 << 30);

#[derive(Debug, buck2_error::Error)]
#[error("Tabs are not allowed in Buck files: `{0}`")]
#[buck2(input)]
struct StarlarkTabsError(OwnedStarlarkPath);

#[derive(Debug, buck2_error::Error)]
enum StarlarkPeakMemoryError {
    #[error(
        "Starlark peak memory usage for {0} is {1} which exceeds the limit {2}! Please reduce memory usage to prevent OOMs. See {3} for debugging tips."
    )]
    #[buck2(input)]
    ExceedsThreshold(BuildFilePath, HumanizedBytes, HumanizedBytes, String),
}

/// A ParseData includes the parsed AST and a list of the imported files.
///
/// The imports are under a separate Arc so that that can be shared with
/// the evaluation result (which needs the imports but no longer needs the AST).
#[derive(Allocative)]
pub struct ParseData {
    #[allocative(skip)]
    pub ast: AstModule,
    pub imports: Arc<Vec<(Option<FileSpan>, OwnedStarlarkModulePath)>>,
    pub(crate) bazel_package_data_requests: Option<BTreeSet<BazelPackageDataRequest>>,
}

pub type ParseResult = Result<ParseData, buck2_error::Error>;

impl ParseData {
    fn new(
        ast: AstModule,
        implicit_imports: Vec<OwnedStarlarkModulePath>,
        resolver: &dyn LoadResolver,
        is_build_file: bool,
    ) -> buck2_error::Result<Self> {
        let mut loads = implicit_imports.into_map(|x| (None, x));
        for x in ast.loads() {
            let path = resolver
                .resolve_load(x.module_id, Some(&x.span))
                .with_buck_error_context(|| {
                    format!(
                        "Error loading `load` of `{}` from `{}`",
                        x.module_id, x.span
                    )
                })?;
            loads.push((Some(x.span), path));
        }
        let bazel_package_data_requests =
            is_build_file.then(|| bazel_package_data_requests_from_ast(&ast));
        Ok(Self {
            ast,
            imports: Arc::new(loads),
            bazel_package_data_requests,
        })
    }

    pub fn ast(&self) -> &AstModule {
        &self.ast
    }

    pub fn imports(&self) -> &Arc<Vec<(Option<FileSpan>, OwnedStarlarkModulePath)>> {
        &self.imports
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strategy(patterns: &[&str]) -> PackageListingStrategy {
        package_listing_strategy_from_glob_patterns(
            &patterns
                .iter()
                .map(|pattern| pattern.to_string())
                .collect::<Vec<_>>(),
        )
    }

    fn prefix(path: &str) -> PackageRelativePathBuf {
        PackageRelativePathBuf::try_from(path.to_owned()).unwrap()
    }

    #[test]
    fn test_bazel_package_listing_strategy_shallow_glob() {
        assert_eq!(strategy(&["*.cc"]), PackageListingStrategy::Shallow);
    }

    #[test]
    fn test_bazel_package_listing_strategy_selective_glob_prefix() {
        assert_eq!(
            strategy(&["src/**/*.cc"]),
            PackageListingStrategy::Selective(vec![prefix("src")])
        );
    }

    #[test]
    fn test_bazel_package_listing_strategy_recursive_glob() {
        assert_eq!(strategy(&["**/*.cc"]), PackageListingStrategy::Recursive);
    }
}

fn bazel_package_data_requests_from_ast(ast: &AstModule) -> BTreeSet<BazelPackageDataRequest> {
    struct Visitor {
        requests: BTreeSet<BazelPackageDataRequest>,
    }

    impl Visitor {
        fn visit_expr(&mut self, node: &AstExpr) {
            match &node.node {
                Expr::Call(callee, arguments) => {
                    match package_data_callee_name(callee) {
                        Some("glob") => {
                            if let Some(request) = literal_glob_request(arguments.args.as_slice()) {
                                self.requests.insert(BazelPackageDataRequest::Glob(request));
                            }
                        }
                        Some("sub_packages") => {
                            self.requests.insert(BazelPackageDataRequest::Subpackages);
                        }
                        _ => {}
                    }
                    for argument in &arguments.args {
                        visit_argument_exprs(argument, |expr| self.visit_expr(expr));
                    }
                }
                _ => node.visit_expr(|expr| self.visit_expr(expr)),
            }
        }
    }

    let mut visitor = Visitor {
        requests: BTreeSet::new(),
    };
    ast.statement().visit_expr(|expr| visitor.visit_expr(expr));
    visitor.requests
}

pub(crate) fn package_listing_strategy_from_glob_patterns(
    patterns: &[String],
) -> PackageListingStrategy {
    let mut prefixes = Vec::new();
    for pattern in patterns {
        match glob_listing_prefix(pattern) {
            GlobListingPrefix::Shallow => {}
            GlobListingPrefix::Prefix(prefix) => prefixes.push(prefix),
            GlobListingPrefix::Recursive => return PackageListingStrategy::Recursive,
        }
    }
    PackageListingStrategy::selective(prefixes)
}

fn package_data_callee_name(callee: &AstExpr) -> Option<&str> {
    match &callee.node {
        Expr::Identifier(ident) => Some(ident.node.ident.as_str()),
        Expr::Dot(_, field) => Some(field.node.as_str()),
        _ => None,
    }
}

fn visit_argument_exprs(
    argument: &starlark::syntax::ast::AstArgument,
    mut f: impl FnMut(&AstExpr),
) {
    match &argument.node {
        Argument::Positional(expr)
        | Argument::Named(_, expr)
        | Argument::Args(expr)
        | Argument::KwArgs(expr) => f(expr),
    }
}

fn literal_glob_request(
    arguments: &[starlark::syntax::ast::AstArgument],
) -> Option<BazelGlobRequest> {
    let include = literal_glob_list_arg(arguments, "include", true)??;
    let exclude = literal_glob_list_arg(arguments, "exclude", false)?.unwrap_or_default();
    if has_named_arg(arguments, "exclude_directories") {
        return None;
    }
    Some(BazelGlobRequest {
        include,
        exclude,
        include_directories: false,
    })
}

fn literal_glob_list_arg(
    arguments: &[starlark::syntax::ast::AstArgument],
    name: &str,
    allow_positional: bool,
) -> Option<Option<Vec<String>>> {
    if allow_positional {
        let mut positional = arguments
            .iter()
            .filter_map(|argument| match &argument.node {
                Argument::Positional(expr) => Some(expr),
                _ => None,
            });
        if let Some(expr) = positional.next() {
            return Some(Some(literal_string_list(expr)?));
        }
    }

    for argument in arguments {
        match &argument.node {
            Argument::Named(arg_name, expr) if arg_name.node == name => {
                return Some(Some(literal_string_list(expr)?));
            }
            Argument::Args(_) | Argument::KwArgs(_) => return None,
            _ => {}
        }
    }

    Some(None)
}

fn literal_string_list(expr: &AstExpr) -> Option<Vec<String>> {
    match &expr.node {
        Expr::List(items) | Expr::Tuple(items) => items
            .iter()
            .map(|item| match &item.node {
                Expr::Literal(AstLiteral::String(value)) => Some(value.node.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn has_named_arg(arguments: &[starlark::syntax::ast::AstArgument], name: &str) -> bool {
    arguments.iter().any(|argument| match &argument.node {
        Argument::Named(arg_name, _) => arg_name.node == name,
        _ => false,
    })
}

enum GlobListingPrefix {
    Shallow,
    Prefix(PackageRelativePathBuf),
    Recursive,
}

fn glob_listing_prefix(pattern: &str) -> GlobListingPrefix {
    let Some(wildcard) = pattern.find(['*', '[', '?']) else {
        return match pattern.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => parse_glob_prefix(parent),
            _ => GlobListingPrefix::Shallow,
        };
    };

    let wildcard_suffix = &pattern[wildcard..];
    let before_wildcard = &pattern[..wildcard];
    let literal_prefix = before_wildcard.trim_end_matches('/');
    if before_wildcard.ends_with('/') && !literal_prefix.is_empty() {
        return parse_glob_prefix(literal_prefix);
    }
    let Some((parent, _)) = literal_prefix.rsplit_once('/') else {
        return if wildcard_suffix.contains('/') {
            GlobListingPrefix::Recursive
        } else {
            GlobListingPrefix::Shallow
        };
    };
    if parent.is_empty() {
        GlobListingPrefix::Shallow
    } else {
        parse_glob_prefix(parent)
    }
}

fn parse_glob_prefix(prefix: &str) -> GlobListingPrefix {
    match PackageRelativePathBuf::try_from(prefix.to_owned()) {
        Ok(prefix) => GlobListingPrefix::Prefix(prefix),
        Err(_) => GlobListingPrefix::Recursive,
    }
}

pub fn get_starlark_warning_link() -> &'static str {
    if buck2_core::is_open_source() {
        "https://buck2.build/docs/users/faq/starlark_peak_mem"
    } else {
        "https://fburl.com/starlark_peak_mem_warning"
    }
}
/// Interpreter for build files.
///
/// The Interpreter is responsible for parsing files to an AST and then
/// evaluating that AST. The Interpreter doesn't maintain state or cache results
/// of parsing or loading imports.
#[derive(Allocative, PagablePanic)]
pub(crate) struct InterpreterForDir {
    /// Non-cell-specific information.
    global_state: Arc<GlobalInterpreterState>,
    /// Cell-specific alias resolver.
    cell_info: InterpreterCellInfo,
    /// Log GC.
    verbose_gc: bool,
    /// When true, rule function creates a node with no attributes.
    /// (Which won't work correctly, but useful for profiling of starlark).
    ignore_attrs_for_profiling: bool,
    /// Implicit imports. These are only used for build files (e.g. `BUCK`),
    /// not for `bzl` or other files, because we only have implicit imports for build files.
    implicit_import_paths: Arc<ImplicitImportPaths>,
    /// Enable relative imports for the current dir
    current_dir_with_allowed_relative_dirs: Arc<CellPathWithAllowedRelativeDir>,
}

struct InterpreterLoadResolver {
    config: Arc<InterpreterForDir>,
    loader_file_type: StarlarkFileType,
    build_file_cell: BuildFileCell,
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum LoadResolutionError {
    #[error(
        "Cannot load `{0}`. Bxl loads are not allowed from within this context. bxl files can only be loaded from other bxl files."
    )]
    BxlLoadNotAllowed(CellPath),
    #[error("The `load` at {location} of `{got}` should use the canonical name `{wanted}`")]
    WrongCell {
        got: CellPath,
        wanted: CellPath,
        location: String,
    },
}

impl LoadResolver for InterpreterLoadResolver {
    fn resolve_load(
        &self,
        path: &str,
        location: Option<&FileSpan>,
    ) -> buck2_error::Result<OwnedStarlarkModulePath> {
        let relative_import_option = RelativeImports::Allow {
            current_dir_with_allowed_relative: &self.config.current_dir_with_allowed_relative_dirs,
        };
        let opts = ParseImportOptions {
            allow_missing_at_symbol: false,
            relative_import_option,
        };
        let parsed_import = parse_import_with_config_and_package_root(
            self.config.cell_info.cell_alias_resolver(),
            path,
            &opts,
        )?;
        let package_root = parsed_import.package_root;
        let path = parsed_import.path;

        // check for bxl files first before checking for prelude.
        // All bxl imports are parsed the same regardless of prelude or not.
        if path.path().extension() == Some("bxl") {
            match self.loader_file_type {
                StarlarkFileType::Bzl
                | StarlarkFileType::Buck
                | StarlarkFileType::Package
                | StarlarkFileType::Json
                | StarlarkFileType::Toml => {
                    return Err(LoadResolutionError::BxlLoadNotAllowed(path).into());
                }
                StarlarkFileType::Bxl => {
                    return Ok(OwnedStarlarkModulePath::BxlFile(BxlFilePath::new(path)?));
                }
            }
        }

        // Bazel module repos carry their own repository mappings. A .bzl file loaded from a
        // bzlmod repo must therefore resolve its own label literals and transitive loads in that
        // repo's context, not in the context of the BUILD or .bzl file that imported it.
        if path.cell().as_str().starts_with("bzlmod_") || path.cell().as_str() == "bazel_tools" {
            let import_path = ImportPath::new_same_cell_with_package_root(path, package_root)?;
            return Ok(match import_path.path().path().extension() {
                Some("json") => OwnedStarlarkModulePath::JsonFile(import_path),
                Some("toml") => OwnedStarlarkModulePath::TomlFile(import_path),
                _ => OwnedStarlarkModulePath::LoadFile(import_path),
            });
        }

        // If you load the same .bzl file twice via different aliases (e.g. fbcode//buck2/prelude/foo.bzl and prelude.bzl)
        // then anything doing pointer equality (t-sets, provider identities) will go wrong.
        let project_path = self
            .config
            .global_state
            .cell_resolver
            .resolve_path(path.as_ref())?;
        let reformed_path = self
            .config
            .global_state
            .cell_resolver
            .get_cell_path(&project_path);
        if reformed_path.cell() != path.cell() && !self.config.bazel_compat_prelude_enabled() {
            // We actually call resolve_load twice for each loadable - once with all load's up front,
            // then again on each one when we are loading. The second time we don't have a location,
            // so just omit the soft_error that time. Once it is a real error, we should real error on either.
            if let Some(location) = location {
                return Err(LoadResolutionError::WrongCell {
                    got: path,
                    wanted: reformed_path,
                    location: location.to_string(),
                }
                .into());
            }
        }

        // If importing from the prelude, then do not let that inherit the configuration. This
        // ensures that if you define a UDR outside of the prelude's cell, it gets the same prelude
        // as using the exported rules from the prelude would. This matters notably for identity
        // checks in t-sets, which would fail if we had > 1 copy of the prelude.
        if let Some(prelude_import) = self.config.global_state.configuror.prelude_import() {
            if prelude_import.is_prelude_path(&path) {
                if path.path().extension() == Some("json") {
                    return Ok(OwnedStarlarkModulePath::JsonFile(
                        ImportPath::new_same_cell_with_package_root(path, package_root)?,
                    ));
                } else {
                    return Ok(OwnedStarlarkModulePath::LoadFile(
                        ImportPath::new_same_cell_with_package_root(path, package_root)?,
                    ));
                }
            }
        }

        let import_path = ImportPath::new_with_build_file_cells_and_package_root(
            path,
            self.build_file_cell,
            package_root,
        )?;
        Ok(match import_path.path().path().extension() {
            Some("json") => OwnedStarlarkModulePath::JsonFile(import_path),
            Some("toml") => OwnedStarlarkModulePath::TomlFile(import_path),
            _ => OwnedStarlarkModulePath::LoadFile(import_path),
        })
    }
}

struct EvalResult {
    additional: PerFileTypeContext,
    starlark_peak_allocated_byte_limit: OnceCell<Option<u64>>,
    is_profiling_enabled: bool,
    cpu_instruction_count: Option<u64>,
    starlark_tick_count: u64,
}

pub(crate) enum BuildFileEvalResult {
    Complete(
        Option<Arc<StarlarkProfileDataAndStats>>,
        EvaluationResultWithStats,
    ),
    NeedsPackageListing(PackageListingStrategy),
    NeedsBazelPackageData(BTreeSet<BazelPackageDataRequest>),
}

enum BuildFileEvalControl {
    Error(buck2_error::Error),
    NeedsPackageListing(PackageListingStrategy),
    NeedsBazelPackageData(BTreeSet<BazelPackageDataRequest>),
}

impl From<buck2_error::Error> for BuildFileEvalControl {
    fn from(error: buck2_error::Error) -> Self {
        Self::Error(error)
    }
}

impl InterpreterForDir {
    pub(crate) fn equivalent(&self, other: &Self) -> bool {
        self.global_state.equivalent(&other.global_state)
            && self.cell_info == other.cell_info
            && self.verbose_gc == other.verbose_gc
            && self.ignore_attrs_for_profiling == other.ignore_attrs_for_profiling
            && self.implicit_import_paths == other.implicit_import_paths
            && self.current_dir_with_allowed_relative_dirs
                == other.current_dir_with_allowed_relative_dirs
    }

    fn verbose_gc() -> buck2_error::Result<bool> {
        match std::env::var_os("BUCK2_STARLARK_VERBOSE_GC") {
            Some(val) => Ok(!val.is_empty()),
            None => Ok(false),
        }
    }

    fn is_ignore_attrs_for_profiling() -> buck2_error::Result<bool> {
        // If unsure, feel free to break this code or just delete it.
        // It is intended only for profiling of very specific use cases.
        let ignore_attrs_for_profiling = match std::env::var_os("BUCK2_IGNORE_ATTRS_FOR_PROFILING")
        {
            Some(val) => !val.is_empty(),
            None => false,
        };
        if ignore_attrs_for_profiling {
            // This messages is printed in each run once per cell.
            // Somewhat inconvenient, but it is safe.
            eprintln!("Ignoring rule attributes");
        }
        Ok(ignore_attrs_for_profiling)
    }

    //, configuror: Arc<dyn InterpreterConfigurer>
    pub(crate) fn new(
        cell_info: InterpreterCellInfo,
        global_state: Arc<GlobalInterpreterState>,
        implicit_import_paths: Arc<ImplicitImportPaths>,
        current_dir_with_allowed_relative_dirs: Arc<CellPathWithAllowedRelativeDir>,
    ) -> buck2_error::Result<Self> {
        Ok(Self {
            global_state,
            cell_info,
            verbose_gc: Self::verbose_gc()?,
            ignore_attrs_for_profiling: Self::is_ignore_attrs_for_profiling()?,
            implicit_import_paths,
            current_dir_with_allowed_relative_dirs,
        })
    }

    fn bazel_compat_prelude_enabled(&self) -> bool {
        match self.global_state.configuror.prelude_import() {
            Some(prelude_import) => {
                prelude_import.import_path().path().path().as_str() == "bazel/prelude.bzl"
            }
            None => false,
        }
    }

    fn is_bazel_compat_path(&self, import: StarlarkPath<'_>) -> bool {
        let import_cell = import.cell();
        let import_cell_name = import_cell.as_str();
        if import_cell_name == "bazel_tools" || import_cell_name.starts_with("bzlmod_") {
            return true;
        }
        if import_cell_name == "root" && self.bazel_compat_prelude_enabled() {
            return true;
        }
        match self.global_state.configuror.prelude_import() {
            Some(prelude_import) if prelude_import.prelude_cell() == import_cell => {
                if self.bazel_compat_prelude_enabled() {
                    return true;
                }

                import.path().path().as_str().starts_with("bazel/")
            }
            _ => false,
        }
    }

    fn create_env<'v>(
        &self,
        env: BuckStarlarkModule<'v>,
        starlark_path: StarlarkPath<'_>,
        loaded_modules: &LoadedModules,
    ) -> buck2_error::Result<BuckStarlarkModule<'v>> {
        if let Some(prelude_import) = self.prelude_import(starlark_path)? {
            let prelude_env = loaded_modules
                .map
                .get(&StarlarkModulePath::LoadFile(&prelude_import))
                .ok_or_else(|| {
                    internal_error!(
                        "Should've had an env for the prelude import `{prelude_import}`"
                    )
                })?;
            env.import_public_symbols(prelude_env.env());
            if let Ok(Some(native)) = prelude_env.env().get_option("native") {
                // Keep `native` from the prelude as an explicit build-file binding.
                // Safe because `import_public_symbols` above retained the prelude module heap.
                let native = unsafe { native.unchecked_frozen_value() };
                env.set("native", native.to_value());
            }
            if let Ok(Some(bazel_native_rules)) =
                prelude_env.env().get_option("buck2_bazel_native_rules")
            {
                // NativeRuleCallable resolves its backing through the caller's public module
                // bindings, while implicit prelude imports are private.
                // Safe because `import_public_symbols` above retained the prelude module heap.
                let bazel_native_rules = unsafe { bazel_native_rules.unchecked_frozen_value() };
                env.set("buck2_bazel_native_rules", bazel_native_rules.to_value());
            }
            if let StarlarkPath::BuildFile(_) = starlark_path {
                for (name, value) in prelude_env.extra_globals_from_prelude_for_buck_files()? {
                    env.set(name, value.to_value());
                }
            }
        }

        env.set_extra_value_no_overwrite(env.heap().alloc_complex(StarlarkAnyComplex {
            value: InterpreterExtraValue::default(),
        }))
        .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Interpreter))?;

        Ok(env)
    }

    // The environment for evaluating a build file contains additional information
    // to support the extra things available in that context. For example, rule
    // functions can be invoked when evaluating a build file, the package (cell
    // + path) is available. It also includes the implicit root include and
    // implicit package include.
    fn create_build_env<'v>(
        &self,
        env: BuckStarlarkModule<'v>,
        build_file: &BuildFilePath,
        package_listing: &PackageListing,
        package_listing_strategy: PackageListingStrategy,
        package_listing_restart: Arc<RefCell<Option<PackageListingStrategy>>>,
        bazel_package_data: Option<BTreeMap<BazelPackageDataRequest, Arc<Vec<String>>>>,
        bazel_package_data_restart: Arc<RefCell<BTreeSet<BazelPackageDataRequest>>>,
        super_package: SuperPackage,
        package_boundary_exception: bool,
        loaded_modules: &LoadedModules,
    ) -> buck2_error::Result<(BuckStarlarkModule<'v>, ModuleInternals)> {
        let internals = self.global_state.configuror.new_extra_context(
            &self.cell_info,
            build_file.clone(),
            package_listing.dupe(),
            package_listing_strategy,
            package_listing_restart,
            bazel_package_data,
            bazel_package_data_restart,
            super_package,
            package_boundary_exception,
            loaded_modules,
            self.package_import(build_file),
            self.current_dir_with_allowed_relative_dirs
                .as_ref()
                .to_owned(),
        )?;
        let env = self.create_env(env, StarlarkPath::BuildFile(build_file), loaded_modules)?;

        if let Some(root_import) = self.root_import() {
            let root_env = loaded_modules
                .map
                .get(&StarlarkModulePath::LoadFile(&root_import))
                .ok_or_else(|| {
                    internal_error!("Should've had an env for the root import `{root_import}`")
                })?
                .env();
            env.import_public_symbols(root_env);
        }

        Ok((env, internals))
    }

    fn load_resolver(
        self: &Arc<Self>,
        current_file_path: StarlarkPath<'_>,
    ) -> InterpreterLoadResolver {
        InterpreterLoadResolver {
            config: self.dupe(),
            loader_file_type: current_file_path.file_type(),
            build_file_cell: current_file_path.build_file_cell(),
        }
    }

    fn package_import(&self, build_file_import: &BuildFilePath) -> Option<&Arc<ImplicitImport>> {
        self.implicit_import_paths
            .package_imports
            .get(build_file_import.package())
    }

    fn root_import(&self) -> Option<ImportPath> {
        self.implicit_import_paths.root_import.clone()
    }

    fn cell_default_prelude_import(
        prelude_import: &PreludePath,
    ) -> buck2_error::Result<ImportPath> {
        let prelude_file = CellRelativePathBuf::unchecked_new("prelude.bzl".to_owned());
        ImportPath::new_same_cell(CellPath::new(prelude_import.prelude_cell(), prelude_file))
    }

    fn prelude_import(&self, import: StarlarkPath) -> buck2_error::Result<Option<ImportPath>> {
        let prelude_import = self.global_state.configuror.prelude_import();
        if let Some(prelude_import) = prelude_import {
            let import_path = import.path();

            match import {
                StarlarkPath::BuildFile(_)
                | StarlarkPath::PackageFile(_)
                | StarlarkPath::BxlFile(_) => {
                    if import_path.cell() == prelude_import.prelude_cell() {
                        return Ok(Some(Self::cell_default_prelude_import(prelude_import)?));
                    }
                    return Ok(Some(prelude_import.import_path().clone()));
                }
                StarlarkPath::LoadFile(_) => {
                    if !prelude_import.is_prelude_path(&import_path) {
                        return Ok(Some(prelude_import.import_path().clone()));
                    }
                }
                StarlarkPath::JsonFile(_) | StarlarkPath::TomlFile(_) => return Ok(None),
            }
        }

        Ok(None)
    }

    /// Parses skylark code to an AST.
    pub(crate) fn parse(
        self: &Arc<Self>,
        import: StarlarkPath,
        content: String,
    ) -> buck2_error::Result<ParseResult> {
        // Indentation with tabs is prohibited by starlark spec and configured starlark dialect.
        // This check also prohibits tabs even where spaces are not significant,
        // for example inside parentheses in function call arguments,
        // which restricts what the spec allows.
        let is_bazel_compat_path = self.is_bazel_compat_path(import);
        if content.contains('\t') && !is_bazel_compat_path {
            return Err(StarlarkTabsError(OwnedStarlarkPath::new(import)).into());
        }

        let project_relative_path = self
            .global_state
            .cell_resolver
            .resolve_path(import.path().as_ref().as_ref())?;

        let disable_starlark_types =
            self.global_state.disable_starlark_types || is_bazel_compat_path;
        let mut dialect = import.file_type().dialect(disable_starlark_types);
        if is_bazel_compat_path {
            dialect.enable_tabs_as_whitespace = true;
        }
        let ast = match AstModule::parse(project_relative_path.as_str(), content, &dialect) {
            Ok(ast) => ast,
            Err(e) => {
                return Ok(Err(buck2_error::Error::from(e).context(format!(
                    "Error parsing: `{}`",
                    OwnedStarlarkPath::new(import)
                ))));
            }
        };
        let mut implicit_imports = Vec::new();
        if let Some(i) = self.prelude_import(import)? {
            implicit_imports.push(OwnedStarlarkModulePath::LoadFile(i));
        }
        if let StarlarkPath::BuildFile(build_file) = import {
            if let Some(i) = self.package_import(build_file) {
                implicit_imports.push(OwnedStarlarkModulePath::LoadFile(i.import().clone()));
            }
            if let Some(i) = self.root_import() {
                implicit_imports.push(OwnedStarlarkModulePath::LoadFile(i));
            }
        }
        ParseData::new(
            ast,
            implicit_imports,
            &self.load_resolver(import),
            matches!(import, StarlarkPath::BuildFile(_)),
        )
        .map(Ok)
    }

    pub(crate) fn resolve_path(
        self: &Arc<Self>,
        import: StarlarkPath<'_>,
        import_string: &str,
    ) -> buck2_error::Result<OwnedStarlarkModulePath> {
        self.load_resolver(import).resolve_load(import_string, None)
    }

    fn eval(
        self: &Arc<Self>,
        env: &BuckStarlarkModule,
        ast: AstModule,
        buckconfigs: &mut dyn BuckConfigsViewForStarlark,
        loaded_modules: LoadedModules,
        extra_context: PerFileTypeContext,
        eval_provider: StarlarkEvaluatorProvider,
        unstable_typecheck: bool,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<(FinishedStarlarkEvaluation, EvalResult)> {
        let import = extra_context.starlark_path();
        let globals = self.global_state.globals();
        let file_loader =
            InterpreterFileLoader::new(loaded_modules, Arc::new(self.load_resolver(import)));
        let host_info = self.global_state.configuror.host_info();
        let extra = BuildContext::new(
            &self.cell_info,
            buckconfigs,
            host_info,
            extra_context,
            self.ignore_attrs_for_profiling,
        );

        let print = EventDispatcherPrintHandler(get_dispatcher());
        let (finished_eval, (cpu_instruction_count, starlark_tick_count, is_profiling_enabled)) =
            eval_provider.with_evaluator(
                env,
                cancellation.into(),
                |eval, is_profiling_enabled_by_provider| {
                    eval.enable_static_typechecking(unstable_typecheck);
                    eval.set_print_handler(&print);
                    eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);
                    eval.set_loader(&file_loader);
                    eval.extra = Some(&extra);
                    if self.verbose_gc {
                        eval.verbose_gc();
                    }

                    // Ignore error if failed to initialize instruction counter.
                    let instruction_counter: Option<PerThreadInstructionCounter> =
                        PerThreadInstructionCounter::init().ok().unwrap_or_default();

                    match eval.eval_module(ast, globals) {
                        Ok(_) => {
                            let cpu_instruction_count =
                                instruction_counter.and_then(|c| c.collect().ok());
                            let starlark_tick_count = eval.get_total_tick_count();
                            Ok((
                                cpu_instruction_count,
                                starlark_tick_count,
                                is_profiling_enabled_by_provider,
                            ))
                        }
                        Err(p) => Err(p.into()),
                    }
                },
            )?;
        Ok((
            finished_eval,
            EvalResult {
                additional: extra.additional,
                is_profiling_enabled,
                starlark_peak_allocated_byte_limit: extra.starlark_peak_allocated_byte_limit,
                cpu_instruction_count,
                starlark_tick_count,
            },
        ))
    }

    /// Evaluates the AST for a parsed module. Loaded modules must contain the loaded
    /// environment for all (transitive) required imports.
    /// Returns the FrozenModule for the module.
    pub(crate) fn eval_module(
        self: &Arc<Self>,
        starlark_path: StarlarkModulePath<'_>,
        buckconfigs: &mut dyn BuckConfigsViewForStarlark,
        ast: AstModule,
        loaded_modules: LoadedModules,
        eval_provider: StarlarkEvaluatorProvider,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<FrozenModule> {
        BuckStarlarkModule::with_profiling(|env| {
            let env = self.create_env(env, starlark_path.into(), &loaded_modules)?;
            let extra_context = match starlark_path {
                StarlarkModulePath::LoadFile(bzl) => {
                    PerFileTypeContext::Bzl(BzlEvalCtx::new(bzl.clone()))
                }
                StarlarkModulePath::BxlFile(bxl) => PerFileTypeContext::Bxl(bxl.clone()),
                StarlarkModulePath::JsonFile(j) => PerFileTypeContext::Json(j.clone()),
                StarlarkModulePath::TomlFile(t) => PerFileTypeContext::Toml(t.clone()),
            };
            let typecheck = !self.is_bazel_compat_path(starlark_path.starlark_path())
                && (self.global_state.unstable_typecheck
                    || matches!(starlark_path, StarlarkModulePath::BxlFile(..))
                    || match self.global_state.configuror.prelude_import() {
                        Some(prelude_import) => {
                            prelude_import.prelude_cell()
                                == self.cell_info.cell_alias_resolver().resolve_self()
                        }
                        None => false,
                    });
            let (finished_eval, _) = self.eval(
                &env,
                ast,
                buckconfigs,
                loaded_modules,
                extra_context,
                eval_provider,
                typecheck,
                cancellation,
            )?;
            let (token, frozen, _) = finished_eval.freeze_and_finish(env)?;

            Ok((token, frozen))
        })
    }

    pub(crate) fn eval_bzlmod_module_extension(
        self: &Arc<Self>,
        extension_path: &ImportPath,
        extension_module: &FrozenModule,
        extension_name: &str,
        extension_usages_json: &str,
        module_ctx_working_dir: &str,
        repo_env: std::sync::Arc<std::collections::BTreeMap<String, String>>,
        buckconfigs: &mut dyn BuckConfigsViewForStarlark,
        eval_provider: StarlarkEvaluatorProvider,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<crate::bazel_repository::BazelModuleExtensionEvaluation> {
        BuckStarlarkModule::with_profiling(|env| {
            let extension_value = extension_module
                .get_option(extension_name)
                .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Input))?
                .ok_or_else(|| {
                    buck2_error::Error::from(
                        crate::bazel_repository::BazelRepositoryError::ModuleExtensionSymbolMissing {
                            path: extension_path.to_string(),
                            extension: extension_name.to_owned(),
                        },
                    )
                })?;
            let extension = crate::bazel_repository::module_extension_from_loaded_module(
                extension_path,
                extension_name,
                extension_value,
            )?;
            let recorder = BazelRepositoryRuleRecorder::default();
            let recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>> =
                Arc::new(Mutex::new(Vec::new()));
            let extra_context = PerFileTypeContext::Bzl(BzlEvalCtx::new(extension_path.clone()));
            let extra = BuildContext::new_with_bazel_repository_rule_recorder(
                &self.cell_info,
                buckconfigs,
                self.global_state.configuror.host_info(),
                extra_context,
                self.ignore_attrs_for_profiling,
                &recorder,
                BazelRepositoryContextForStarlark {
                    recorded_inputs: recorded_inputs.clone(),
                    working_dir: module_ctx_working_dir.to_owned(),
                },
            );

            let print = EventDispatcherPrintHandler(get_dispatcher());
            let globals = self.global_state.globals();
            let (finished_eval, evaluation) =
                eval_provider.with_evaluator(&env, cancellation.into(), |eval, _| {
                    eval.set_print_handler(&print);
                    eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);
                    eval.extra = Some(&extra);

                    let module_ctx =
                        crate::bazel_repository::alloc_bzlmod_module_extension_context(
                            &extension,
                            extension_usages_json,
                            module_ctx_working_dir,
                            repo_env.clone(),
                            recorded_inputs.clone(),
                            globals,
                            eval,
                        )?;
                    match extension.invoke_implementation(module_ctx, eval) {
                        Ok(value) => {
                            let mut result = recorder.take_result();
                            result.recorded_inputs =
                                crate::bazel_repository::take_module_ctx_recorded_inputs(
                                    module_ctx,
                                )?;
                            result.reproducible = value
                                .downcast_ref::<crate::bazel_repository::StarlarkModuleExtensionMetadata>(
                                )
                                .is_some_and(|metadata| metadata.reproducible());
                            Ok(crate::bazel_repository::BazelModuleExtensionEvaluation::Success(
                                result,
                            ))
                        }
                        Err(error) => {
                            let label_deps =
                                crate::bazel_repository::take_module_ctx_path_label_deps(
                                    module_ctx,
                                )?;
                            if label_deps.is_empty() {
                                Err(error.into())
                            } else {
                                Ok(crate::bazel_repository::BazelModuleExtensionEvaluation::NeedsPathLabelDeps {
                                    label_deps,
                                    error: error.to_string(),
                                })
                            }
                        }
                    }
                })?;
            let (token, _) = finished_eval.finish()?;
            Ok((token, evaluation))
        })
    }

    pub(crate) fn eval_bzlmod_repository_rule(
        self: &Arc<Self>,
        rule_path: &ImportPath,
        rule_module: &FrozenModule,
        invocation: &BazelRepositoryRuleInvocation,
        repository_ctx_working_dir: &str,
        repo_env: std::sync::Arc<std::collections::BTreeMap<String, String>>,
        buckconfigs: &mut dyn BuckConfigsViewForStarlark,
        eval_provider: StarlarkEvaluatorProvider,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<crate::bazel_repository::BazelRepositoryRuleEvaluation> {
        BuckStarlarkModule::with_profiling(|env| {
            let rule_value = rule_module
                .get_any_visibility(&invocation.rule_id.name)
                .map(|(value, _)| value)
                .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Input))
                .or_else(|_| {
                    Err(buck2_error::Error::from(
                        crate::bazel_repository::BazelRepositoryError::RepositoryRuleSymbolMissing {
                            path: rule_path.to_string(),
                            rule: invocation.rule_id.name.clone(),
                        },
                    ))
                })?;
            let repository_rule = crate::bazel_repository::repository_rule_from_loaded_module(
                rule_path,
                &invocation.rule_id.name,
                rule_value,
            )?;
            let recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>> =
                Arc::new(Mutex::new(Vec::new()));
            let extra_context = PerFileTypeContext::Bzl(BzlEvalCtx::new(rule_path.clone()));
            let extra = BuildContext::new_with_bazel_repository_context(
                &self.cell_info,
                buckconfigs,
                self.global_state.configuror.host_info(),
                extra_context,
                self.ignore_attrs_for_profiling,
                BazelRepositoryContextForStarlark {
                    recorded_inputs: recorded_inputs.clone(),
                    working_dir: repository_ctx_working_dir.to_owned(),
                },
            );

            let print = EventDispatcherPrintHandler(get_dispatcher());
            let globals = self.global_state.globals();
            let (finished_eval, evaluation) =
                eval_provider.with_evaluator(&env, cancellation.into(), |eval, _| {
                    eval.set_print_handler(&print);
                    eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);
                    eval.extra = Some(&extra);

                    let repository_ctx = crate::bazel_repository::alloc_bzlmod_repository_context(
                        repository_rule.as_ref(),
                        invocation,
                        repository_ctx_working_dir,
                        repo_env.clone(),
                        recorded_inputs.clone(),
                        globals,
                        eval,
                    )?;
                    match repository_rule
                        .as_ref()
                        .invoke_implementation(repository_ctx, eval)
                    {
                        Ok(_) => Ok(crate::bazel_repository::BazelRepositoryRuleEvaluation::Success(
                            crate::bazel_repository::BazelRepositoryRuleEvaluationResult {
                                files: crate::bazel_repository::take_repository_ctx_files(
                                    repository_ctx,
                                )?,
                                recorded_inputs:
                                    crate::bazel_repository::take_repository_ctx_recorded_inputs(
                                        repository_ctx,
                                    )?,
                            },
                        )),
                        Err(error) => {
                            let label_deps =
                                crate::bazel_repository::take_repository_ctx_path_label_deps(
                                    repository_ctx,
                                )?;
                            if label_deps.is_empty() {
                                Err(error.into())
                            } else {
                                Ok(crate::bazel_repository::BazelRepositoryRuleEvaluation::NeedsPathLabelDeps {
                                    label_deps,
                                    error: error.to_string(),
                                })
                            }
                        }
                    }
                })?;
            let (token, _) = finished_eval.finish()?;
            Ok((token, evaluation))
        })
    }

    pub(crate) fn eval_package_file(
        self: &Arc<Self>,
        package_file_path: &PackageFilePath,
        ast: AstModule,
        parent: SuperPackage,
        buckconfigs: &mut dyn BuckConfigsViewForStarlark,
        loaded_modules: LoadedModules,
        eval_provider: StarlarkEvaluatorProvider,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<SuperPackage> {
        BuckStarlarkModule::with_profiling(|env| {
            let env = self.create_env(
                env,
                StarlarkPath::PackageFile(package_file_path),
                &loaded_modules,
            )?;

            let extra_context = PerFileTypeContext::Package(PackageFileEvalCtx {
                path: package_file_path.clone(),
                parent,
                visibility: RefCell::new(None),
                test_config_unification_rollout: RefCell::new(None),
            });

            let (finished_eval, eval_result) = self.eval(
                &env,
                ast,
                buckconfigs,
                loaded_modules,
                extra_context,
                eval_provider,
                false,
                cancellation,
            )?;

            let per_file_context = eval_result.additional;

            let (token, extra) = if InterpreterExtraValue::get(&env)?
                .package_extra
                .get()
                .is_some()
            {
                // Only freeze if there's something to freeze, otherwise we will needlessly freeze
                // globals. TODO(nga): add API to only freeze extra.
                let (token, frozen, _) = finished_eval.freeze_and_finish(env)?;
                (token, FrozenPackageFileExtra::get(&frozen)?)
            } else {
                let (token, _) = finished_eval.finish()?;
                (token, None)
            };

            let package_file_eval_ctx = per_file_context.into_package_file()?;

            Ok((token, package_file_eval_ctx.build_super_package(extra)?))
        })
    }

    /// Evaluates the AST for a parsed build file. Loaded modules must contain the
    /// loaded environment for all (transitive) required imports.
    /// Returns the result of evaluation.
    pub(crate) fn eval_build_file(
        self: &Arc<Self>,
        build_file: &BuildFilePath,
        buckconfigs: &mut dyn BuckConfigsViewForStarlark,
        listing: PackageListing,
        listing_strategy: PackageListingStrategy,
        bazel_package_data: Option<BTreeMap<BazelPackageDataRequest, Arc<Vec<String>>>>,
        super_package: SuperPackage,
        package_boundary_exception: bool,
        ast: AstModule,
        loaded_modules: LoadedModules,
        eval_provider: StarlarkEvaluatorProvider,
        unstable_typecheck: bool,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<BuildFileEvalResult> {
        match BuckStarlarkModule::with_profiling(|env| {
            let package_listing_restart = Arc::new(RefCell::new(None));
            let bazel_package_data_restart = Arc::new(RefCell::new(BTreeSet::new()));
            let (env, internals) = self.create_build_env(
                env,
                build_file,
                &listing,
                listing_strategy,
                package_listing_restart.dupe(),
                bazel_package_data,
                bazel_package_data_restart.dupe(),
                super_package,
                package_boundary_exception,
                &loaded_modules,
            )?;
            let buckconfig_key = BuckconfigKeyRef {
                section: "buck2",
                property: "check_starlark_peak_memory",
            };
            let starlark_peak_mem_config_enabled = LegacyBuckConfig::parse_value(
                buckconfig_key,
                buckconfigs
                    .read_root_cell_config(buckconfig_key)?
                    .as_deref(),
            )?
            .unwrap_or(false);

            let (finished_eval, eval_result) = match self.eval(
                &env,
                ast,
                buckconfigs,
                loaded_modules,
                PerFileTypeContext::Build(internals),
                eval_provider,
                unstable_typecheck,
                cancellation,
            ) {
                Ok(result) => result,
                Err(e) => {
                    if let Some(requests) =
                        take_bazel_package_data_restart(&bazel_package_data_restart)
                    {
                        return Err(BuildFileEvalControl::NeedsBazelPackageData(requests));
                    }
                    if let Some(strategy) = package_listing_restart.borrow_mut().take() {
                        return Err(BuildFileEvalControl::NeedsPackageListing(strategy));
                    }
                    return Err(e.into());
                }
            };

            let internals = eval_result.additional.into_build()?;
            if let Some(requests) = internals.take_bazel_package_data_restart() {
                let (token, _profile_data) = finished_eval.finish()?;
                return Ok((token, BuildFileEvalResult::NeedsBazelPackageData(requests)));
            }
            if let Some(strategy) = internals.take_package_listing_restart() {
                let (token, _profile_data) = finished_eval.finish()?;
                return Ok((token, BuildFileEvalResult::NeedsPackageListing(strategy)));
            }
            let starlark_peak_allocated_bytes = env.heap().peak_allocated_bytes() as u64;
            let starlark_peak_mem_check_enabled =
                !eval_result.is_profiling_enabled && starlark_peak_mem_config_enabled;
            let starlark_mem_limit = eval_result
                .starlark_peak_allocated_byte_limit
                .get()
                .and_then(|limit| *limit)
                .unwrap_or(DEFAULT_STARLARK_MEMORY_USAGE_LIMIT);

            if starlark_peak_mem_check_enabled && starlark_peak_allocated_bytes > starlark_mem_limit
            {
                Err(
                    buck2_error::Error::from(StarlarkPeakMemoryError::ExceedsThreshold(
                        build_file.to_owned(),
                        HumanizedBytes::fixed_width(starlark_peak_allocated_bytes),
                        HumanizedBytes::fixed_width(starlark_mem_limit),
                        get_starlark_warning_link().to_owned(),
                    ))
                    .into(),
                )
            } else {
                let (token, profile_data) = finished_eval.finish()?;

                Ok((
                    token,
                    BuildFileEvalResult::Complete(
                        profile_data,
                        EvaluationResultWithStats {
                            result: EvaluationResult::from(internals),
                            starlark_peak_allocated_bytes,
                            cpu_instruction_count: eval_result.cpu_instruction_count,
                            starlark_tick_count: eval_result.starlark_tick_count,
                        },
                    ),
                ))
            }
        }) {
            Ok(result) => Ok(result),
            Err(BuildFileEvalControl::Error(error)) => Err(error),
            Err(BuildFileEvalControl::NeedsPackageListing(strategy)) => {
                Ok(BuildFileEvalResult::NeedsPackageListing(strategy))
            }
            Err(BuildFileEvalControl::NeedsBazelPackageData(requests)) => {
                Ok(BuildFileEvalResult::NeedsBazelPackageData(requests))
            }
        }
    }
}

fn take_bazel_package_data_restart(
    restart: &Arc<RefCell<BTreeSet<BazelPackageDataRequest>>>,
) -> Option<BTreeSet<BazelPackageDataRequest>> {
    let mut restart = restart.borrow_mut();
    if restart.is_empty() {
        None
    } else {
        Some(std::mem::take(&mut *restart))
    }
}
