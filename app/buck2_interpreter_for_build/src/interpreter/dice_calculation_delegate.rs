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
use std::collections::BTreeSet;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use buck2_build_api::actions::execute::dice_data::DiceHasCommandExecutor;
use buck2_build_api::actions::execute::dice_data::HasFallbackExecutorConfig;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::dice::cycles::CycleGuard;
use buck2_common::dice::data::HasIoProvider;
use buck2_common::file_ops::dice::DiceFileComputations;
use buck2_common::file_ops::error::FileReadErrorContext;
use buck2_common::file_ops::metadata::RawPathMetadata;
use buck2_common::legacy_configs::dice::HasLegacyConfigs;
use buck2_common::legacy_configs::dice::OpaqueLegacyBuckConfigOnDice;
use buck2_common::package_boundary::HasPackageBoundaryExceptions;
use buck2_common::package_listing::PackageListingStrategy;
use buck2_common::package_listing::bazel_compat_package_listing_enabled;
use buck2_common::package_listing::dice::DicePackageListingResolver;
use buck2_common::package_listing::listing::PackageListing;
use buck2_core::build_file_path::BuildFilePath;
use buck2_core::bzl::ImportPath;
use buck2_core::cells::build_file_cell::BuildFileCell;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::cell_path_with_allowed_relative_dir::CellPathWithAllowedRelativeDir;
use buck2_core::cells::external::is_bzlmod_cell_name;
use buck2_core::execution_types::executor_config::Executor;
use buck2_core::execution_types::executor_config::RemoteEnabledExecutor;
use buck2_core::package::PackageLabel;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use buck2_events::dispatch::span;
use buck2_events::dispatch::span_async_simple;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_execute::execute::blocking::HasBlockingExecutor;
use buck2_execute::execute::command_executor::CommandExecutor;
use buck2_execute::materialize::materializer::HasMaterializer;
use buck2_interpreter::allow_relative_paths::HasAllowRelativePaths;
use buck2_interpreter::dice::starlark_provider::StarlarkEvalKind;
use buck2_interpreter::factory::StarlarkEvaluatorProvider;
use buck2_interpreter::file_loader::LoadedModule;
use buck2_interpreter::file_loader::ModuleDeps;
use buck2_interpreter::from_freeze::from_freeze_error;
use buck2_interpreter::import_paths::HasImportPaths;
use buck2_interpreter::import_paths::ImplicitImportPaths;
use buck2_interpreter::load_module::InterpreterCalculation;
use buck2_interpreter::package_imports::PackageImplicitImports;
use buck2_interpreter::paths::module::OwnedStarlarkModulePath;
use buck2_interpreter::paths::module::StarlarkModulePath;
use buck2_interpreter::paths::package::PackageFilePath;
use buck2_interpreter::paths::path::OwnedStarlarkPath;
use buck2_interpreter::paths::path::StarlarkPath;
use buck2_node::nodes::eval_result::EvaluationResult;
use buck2_node::super_package::SuperPackage;
use buck2_util::time_span::TimeSpan;
use derive_more::Display;
use dice::DiceComputations;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::FutureExt;
use pagable::Pagable;
use pagable::pagable_typetag;
use starlark::codemap::FileSpan;
use starlark::environment::Module;
use starlark::syntax::AstModule;
use starlark::values::FrozenHeapName;

use crate::bazel_repository::BazelRemoteRepositoryCommandExecutor;
use crate::bazel_repository::BazelRepositoryCommandExecutor;
use crate::bazel_repository::BazelRepositoryRuleEvaluation;
use crate::interpreter::bazel_glob::BazelPackageDataRequest;
use crate::interpreter::bazel_glob::compute_bazel_package_data;
use crate::interpreter::buckconfig::ConfigsOnDiceViewForStarlark;
use crate::interpreter::build_context::BazelRepositoryRuleInvocation;
use crate::interpreter::cell_info::InterpreterCellInfo;
use crate::interpreter::check_starlark_stack_size::check_starlark_stack_size;
use crate::interpreter::cycles::LoadCycleDescriptor;
use crate::interpreter::global_interpreter_state::HasGlobalInterpreterState;
use crate::interpreter::interpreter_for_dir::BuildFileEvalResult;
use crate::interpreter::interpreter_for_dir::InterpreterForDir;
use crate::interpreter::interpreter_for_dir::ParseData;
use crate::interpreter::interpreter_for_dir::ParseResult;
use crate::super_package::package_value::SuperPackageValuesImpl;

fn toml_value_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::Number(i.into()),
        toml::Value::Float(f) => match serde_json::Number::from_f64(f) {
            Some(n) => serde_json::Value::Number(n),
            None => serde_json::Value::Null,
        },
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .into_iter()
                .map(|(k, v)| (k, toml_value_to_json(v)))
                .collect(),
        ),
    }
}

#[async_trait]
pub trait HasCalculationDelegate<'c, 'd> {
    /// Get calculator for a file evaluation.
    ///
    /// This function only accepts cell names, but it is created
    /// per evaluated file (build file or `.bzl`).
    async fn get_interpreter_calculator(
        &'c mut self,
        path: OwnedStarlarkPath,
    ) -> buck2_error::Result<DiceCalculationDelegate<'c, 'd>>;
}

#[async_trait]
impl<'c, 'd> HasCalculationDelegate<'c, 'd> for DiceComputations<'d> {
    async fn get_interpreter_calculator(
        &'c mut self,
        path: OwnedStarlarkPath,
    ) -> buck2_error::Result<DiceCalculationDelegate<'c, 'd>> {
        #[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
        #[display("{}@{}", _0, _1)]
        #[pagable_typetag(dice::DiceKeyDyn)]
        struct InterpreterConfigForDirKey(CellPath, BuildFileCell);

        #[async_trait]
        impl Key for InterpreterConfigForDirKey {
            type Value = buck2_error::Result<Arc<InterpreterForDir>>;
            async fn compute(
                &self,
                ctx: &mut DiceComputations,
                _cancellation: &CancellationContext,
            ) -> Self::Value {
                let global_state = ctx.get_global_interpreter_state().await?;

                let cell_alias_resolver = ctx.get_cell_alias_resolver(self.0.cell()).await?;
                let is_bazel_external_cell = self.0.cell().as_str() == "bazel_tools"
                    || is_bzlmod_cell_name(self.0.cell().as_str());

                let implicit_import_paths = if is_bazel_external_cell {
                    Arc::new(ImplicitImportPaths {
                        root_import: None,
                        package_imports: PackageImplicitImports::new(
                            self.1,
                            cell_alias_resolver.dupe(),
                            None,
                        )?,
                    })
                } else {
                    ctx.import_paths_for_cell(self.1).await?
                };

                let dirs_allowing_relative_paths = if is_bazel_external_cell {
                    Arc::new(
                        CellPathWithAllowedRelativeDir::backwards_relative_not_supported(
                            self.0.clone(),
                        ),
                    )
                } else {
                    ctx.dirs_allowing_relative_paths(self.0.clone()).await?
                };

                let cell_info = InterpreterCellInfo::new(
                    self.1,
                    ctx.get_cell_resolver().await?,
                    cell_alias_resolver,
                )?;

                Ok(Arc::new(InterpreterForDir::new(
                    cell_info,
                    global_state.dupe(),
                    implicit_import_paths,
                    dirs_allowing_relative_paths,
                )?))
            }

            fn equality(x: &Self::Value, y: &Self::Value) -> bool {
                match (x, y) {
                    (Ok(x), Ok(y)) => x.equivalent(y),
                    _ => false,
                }
            }

            fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
                OkPagableValueSerialize::<Self::Value>::new()
            }
        }

        let build_file_cell = path.borrow().build_file_cell();
        let path_ref = path.borrow();
        let import_dir = match path_ref {
            StarlarkPath::LoadFile(path)
            | StarlarkPath::JsonFile(path)
            | StarlarkPath::TomlFile(path) => path.package_root().cloned(),
            StarlarkPath::BuildFile(_)
            | StarlarkPath::PackageFile(_)
            | StarlarkPath::BxlFile(_) => None,
        }
        .unwrap_or_else(|| {
            path_ref
                .path()
                .parent()
                .expect("starlark path to have parent")
                .to_owned()
        });
        let configs = self
            .compute(&InterpreterConfigForDirKey(import_dir, build_file_cell))
            .await??;

        Ok(DiceCalculationDelegate {
            build_file_cell,
            ctx: self,
            configs,
        })
    }
}

pub struct DiceCalculationDelegate<'c, 'd> {
    build_file_cell: BuildFileCell,
    ctx: &'c mut DiceComputations<'d>,
    configs: Arc<InterpreterForDir>,
}

impl<'c, 'd: 'c> DiceCalculationDelegate<'c, 'd> {
    async fn get_legacy_buck_config_for_starlark(
        &mut self,
    ) -> buck2_error::Result<OpaqueLegacyBuckConfigOnDice> {
        self.ctx
            .get_legacy_config_on_dice(self.build_file_cell.name())
            .await
    }

    async fn bazel_repository_command_executor(
        &mut self,
    ) -> buck2_error::Result<BazelRepositoryCommandExecutor> {
        let executor_config = self.ctx.get_fallback_executor_config().dupe();
        let remote_execution_enabled = match &executor_config.executor {
            Executor::RemoteEnabled(options) => matches!(
                options.executor,
                RemoteEnabledExecutor::Remote(_) | RemoteEnabledExecutor::Hybrid { .. }
            ),
            Executor::Local(_) | Executor::None => false,
        };
        if !remote_execution_enabled {
            return Ok(BazelRepositoryCommandExecutor::Local);
        }

        let artifact_fs = self.ctx.get_artifact_fs().await?;
        let digest_config = self.ctx.global_data().get_digest_config();
        let project_root = self
            .ctx
            .global_data()
            .get_io_provider()
            .project_root()
            .dupe();
        let blocking_executor = self.ctx.get_blocking_executor();
        let materializer = self.ctx.per_transaction_data().get_materializer();
        let response = self
            .ctx
            .get_command_executor_from_dice(&executor_config)
            .await?;
        let command_executor = CommandExecutor::new(
            response.executor,
            response.action_cache_checker,
            response.remote_dep_file_cache_checker,
            response.cache_uploader,
            artifact_fs.clone(),
            executor_config.options,
            response.platform,
        );
        Ok(BazelRepositoryCommandExecutor::Remote(Arc::new(
            BazelRemoteRepositoryCommandExecutor::new(
                command_executor,
                artifact_fs,
                project_root,
                blocking_executor,
                materializer,
                digest_config,
            ),
        )))
    }

    async fn parse_file(
        &mut self,
        starlark_path: StarlarkPath<'_>,
    ) -> buck2_error::Result<ParseResult> {
        let result =
            DiceFileComputations::read_file(self.ctx, starlark_path.path().as_ref().as_ref()).await;
        let content = match starlark_path {
            StarlarkPath::BuildFile(file) => {
                result.with_package_context_information(file.path().path().to_string())
            }
            // Should potentially add support for other file types as well
            _ => result.without_package_context_information(),
        }?;
        self.configs.parse(starlark_path, content)
    }

    async fn eval_deps(
        ctx: &mut DiceComputations<'_>,
        modules: &[(Option<FileSpan>, OwnedStarlarkModulePath)],
    ) -> buck2_error::Result<ModuleDeps> {
        Ok(ModuleDeps(
            ctx.try_compute_join(modules, |ctx, (span, import)| {
                async move {
                    ctx.get_loaded_module(import.borrow())
                        .await
                        .with_buck_error_context(|| {
                            format!(
                                "From load at {}",
                                span.as_ref()
                                    .map_or("implicit location".to_owned(), |file_span| file_span
                                        .resolve()
                                        .begin_file_line()
                                        .to_string())
                            )
                        })
                }
                .boxed()
            })
            .await?,
        ))
    }

    async fn eval_deps_with_cycle_guard(
        ctx: &mut DiceComputations<'_>,
        modules: &[(Option<FileSpan>, OwnedStarlarkModulePath)],
    ) -> buck2_error::Result<ModuleDeps> {
        let deps = CycleGuard::<LoadCycleDescriptor>::new(ctx)?
            .guard_this(Self::eval_deps(ctx, modules))
            .await
            .into_result(ctx)
            .await?;
        deps.map_err(buck2_error::Error::from)?
    }

    pub async fn prepare_eval(
        &mut self,
        starlark_file: StarlarkPath<'_>,
    ) -> buck2_error::Result<(AstModule, ModuleDeps)> {
        let (parse_data, deps) = self.prepare_eval_with_parse_data(starlark_file).await?;
        Ok((parse_data.ast, deps))
    }

    async fn prepare_eval_with_parse_data(
        &mut self,
        starlark_file: StarlarkPath<'_>,
    ) -> buck2_error::Result<(ParseData, ModuleDeps)> {
        let parse_data = self.parse_file(starlark_file).await??;
        let deps = Self::eval_deps_with_cycle_guard(self.ctx, &parse_data.imports).await?;
        Ok((parse_data, deps))
    }

    pub fn prepare_eval_with_content(
        &self,
        starlark_file: StarlarkPath<'_>,
        content: String,
    ) -> buck2_error::Result<ParseResult> {
        self.configs.parse(starlark_file, content)
    }

    pub async fn resolve_load(
        &self,
        starlark_file: StarlarkPath<'_>,
        load_string: &str,
    ) -> buck2_error::Result<OwnedStarlarkModulePath> {
        self.configs.resolve_path(starlark_file, load_string)
    }

    pub async fn eval_module_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<LoadedModule> {
        match starlark_file {
            StarlarkModulePath::JsonFile(_) => self.eval_json_module_uncached(starlark_file).await,
            StarlarkModulePath::TomlFile(_) => self.eval_toml_file_uncached(starlark_file).await,
            _ => {
                self.eval_starlark_module_uncached(starlark_file, cancellation)
                    .await
            }
        }
    }

    pub async fn eval_module_uncached_with_parse_data(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
        parse_data: Arc<ParseData>,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<LoadedModule> {
        match starlark_file {
            StarlarkModulePath::JsonFile(_) | StarlarkModulePath::TomlFile(_) => {
                Err(internal_error!(
                    "preparsed Starlark data supplied for non-Starlark module `{}`",
                    starlark_file
                ))
            }
            StarlarkModulePath::LoadFile(_) | StarlarkModulePath::BxlFile(_) => {
                let deps = Self::eval_deps_with_cycle_guard(self.ctx, &parse_data.imports).await?;
                self.eval_starlark_module_uncached_prepared(
                    starlark_file,
                    parse_data.ast.clone(),
                    deps,
                    cancellation,
                )
                .await
            }
        }
    }

    async fn eval_json_module_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
    ) -> buck2_error::Result<LoadedModule> {
        let path = starlark_file.path();
        let contents = DiceFileComputations::read_file(self.ctx, path.as_ref())
            .await
            .with_package_context_information(path.path().to_string())?;

        let value: serde_json::Value = serde_json::from_str(&contents)
            .with_buck_error_context(|| format!("Parsing {path}"))?;

        // patternlint-disable-next-line buck2-no-starlark-module: We expect these to be small + simple
        let frozen = Module::with_temp_heap(|module| {
            module.set("value", module.heap().alloc(value));
            module
                .freeze_named(FrozenHeapName::User(Box::new(StarlarkEvalKind::Load(
                    Arc::new(OwnedStarlarkModulePath::new(starlark_file)),
                ))))
                .map_err(from_freeze_error)
        })?;
        Ok(LoadedModule::new(
            OwnedStarlarkModulePath::new(starlark_file),
            Default::default(),
            frozen,
        ))
    }

    async fn eval_toml_file_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
    ) -> buck2_error::Result<LoadedModule> {
        let path = starlark_file.path();
        let contents = DiceFileComputations::read_file(self.ctx, path.as_ref())
            .await
            .with_package_context_information(path.path().to_string())?;

        let value: toml::Value =
            toml::from_str(&contents).with_buck_error_context(|| format!("Parsing {path}"))?;
        let json_value = toml_value_to_json(value);

        // patternlint-disable-next-line buck2-no-starlark-module: We expect these to be small + simple
        let frozen = Module::with_temp_heap(|module| {
            module.set("value", module.heap().alloc(json_value));
            module
                .freeze_named(FrozenHeapName::User(Box::new(StarlarkEvalKind::Load(
                    Arc::new(OwnedStarlarkModulePath::new(starlark_file)),
                ))))
                .map_err(from_freeze_error)
        })?;
        Ok(LoadedModule::new(
            OwnedStarlarkModulePath::new(starlark_file),
            Default::default(),
            frozen,
        ))
    }

    async fn eval_starlark_module_uncached(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<LoadedModule> {
        let (ast, deps) = self.prepare_eval(starlark_file.into()).await?;
        self.eval_starlark_module_uncached_prepared(starlark_file, ast, deps, cancellation)
            .await
    }

    async fn eval_starlark_module_uncached_prepared(
        &mut self,
        starlark_file: StarlarkModulePath<'_>,
        ast: AstModule,
        deps: ModuleDeps,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<LoadedModule> {
        let loaded_modules = deps.get_loaded_modules();
        let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
        let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;

        let configs = &self.configs;
        let ctx = &mut *self.ctx;

        let eval_kind = StarlarkEvalKind::Load(Arc::new(starlark_file.to_owned()));
        let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;

        let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(ctx, buckconfig, root_buckconfig);
        let evaluation = configs
            .eval_module(
                starlark_file,
                &mut buckconfigs,
                ast,
                loaded_modules.clone(),
                provider,
                cancellation,
            )
            .with_buck_error_context(|| format!("Error evaluating module: `{}`", starlark_file))?;

        Ok(LoadedModule::new(
            OwnedStarlarkModulePath::new(starlark_file),
            loaded_modules,
            evaluation,
        ))
    }

    pub async fn eval_bzlmod_module_extension(
        &mut self,
        extension_path: &ImportPath,
        extension_module: &LoadedModule,
        extension_name: &str,
        extension_usages_json: &str,
        module_ctx_working_dir: &str,
        repo_env: std::sync::Arc<std::collections::BTreeMap<String, String>>,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<crate::bazel_repository::BazelModuleExtensionEvaluation> {
        let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
        let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;
        let command_executor = self.bazel_repository_command_executor().await?;

        let configs = &self.configs;
        let ctx = &mut *self.ctx;
        let eval_kind = StarlarkEvalKind::Unknown(
            format!("bzlmod_module_extension/{extension_path}%{extension_name}").into(),
        );
        let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;
        let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(ctx, buckconfig, root_buckconfig);
        configs.eval_bzlmod_module_extension(
            extension_path,
            extension_module.env(),
            extension_name,
            extension_usages_json,
            module_ctx_working_dir,
            repo_env,
            command_executor,
            &mut buckconfigs,
            provider,
            cancellation,
        )
    }

    pub async fn eval_bzlmod_module_extension_usages_digest(
        &mut self,
        extension_path: &ImportPath,
        extension_usages_json: &str,
        extension_unique_name: &str,
        fallback_extension_bzl_file: &str,
        fallback_extension_name: &str,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<String> {
        let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
        let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;

        let configs = &self.configs;
        let ctx = &mut *self.ctx;
        let eval_kind = StarlarkEvalKind::Unknown(
            format!("bzlmod_module_extension_usages_digest/{extension_path}").into(),
        );
        let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;
        let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(ctx, buckconfig, root_buckconfig);
        configs.eval_bzlmod_module_extension_usages_digest(
            extension_path,
            extension_usages_json,
            extension_unique_name,
            fallback_extension_bzl_file,
            fallback_extension_name,
            &mut buckconfigs,
            provider,
            cancellation,
        )
    }

    pub(crate) async fn eval_bzlmod_repository_rule(
        &mut self,
        rule_path: &ImportPath,
        rule_module: &LoadedModule,
        invocation: &BazelRepositoryRuleInvocation,
        repository_ctx_working_dir: &str,
        repo_env: std::sync::Arc<std::collections::BTreeMap<String, String>>,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<BazelRepositoryRuleEvaluation> {
        let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
        let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;
        let command_executor = self.bazel_repository_command_executor().await?;

        let configs = &self.configs;
        let ctx = &mut *self.ctx;
        let eval_kind = StarlarkEvalKind::Unknown(
            format!("bzlmod_repository_rule/{}", invocation.rule_id).into(),
        );
        let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;
        let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(ctx, buckconfig, root_buckconfig);
        configs.eval_bzlmod_repository_rule(
            rule_path,
            rule_module.env(),
            invocation,
            repository_ctx_working_dir,
            repo_env,
            command_executor,
            &mut buckconfigs,
            provider,
            cancellation,
        )
    }

    /// Eval parent `PACKAGE` file for given package file.
    async fn eval_parent_package_file(
        &mut self,
        file: PackageLabel,
    ) -> buck2_error::Result<SuperPackage> {
        let cell_resolver = self.ctx.get_cell_resolver().await?;
        let proj_rel_path = cell_resolver.resolve_path(file.as_cell_path())?;
        match proj_rel_path.parent() {
            None => {
                // We are in the project root, there's no parent.
                Ok(SuperPackage::empty::<SuperPackageValuesImpl>()?)
            }
            Some(parent) => {
                let parent_cell = cell_resolver.get_cell_path(parent);
                self.eval_package_file(PackageLabel::from_cell_path(parent_cell.as_ref())?)
                    .await
            }
        }
    }

    /// Return `None` if there's no `PACKAGE` file in the directory.
    pub async fn prepare_package_file_eval(
        &mut self,
        package: PackageLabel,
    ) -> buck2_error::Result<Option<(PackageFilePath, AstModule, ModuleDeps)>> {
        // Note:
        /// To avoid paying the cost of read_dir when computing if any specific file has changed (e.g. PACKAGE),
        /// we depend on directory_sublisting_matching_any_case_key to invalidate all files that match (regardless of case).
        /// We need to do this to make sure to work with case-sensitive file paths.
        //   * `read_path_metadata` would not tell us if the file name is `PACKAGE`
        //     and not `package` on case-insensitive filesystems.
        //     We do case-sensitive comparison for `BUCK` files, so we do the same here.
        //   * we fail here if `PACKAGE` (but not `package`) exists, and it is not a file.

        // package file results capture starlark values and so cannot be checked for equality. This means we
        // can't get early cutoff for the consumers, and so we need to be careful to ensure our deps are precise.
        // Otherwise noop package value recomputations can lead to large recompute costs.
        //
        // Here we put the package file check behind an additional dice key so that we don't recompute on irrelevant
        // changes to the directory contents.
        #[derive(Debug, Display, Clone, Allocative, Eq, PartialEq, Hash, Pagable)]
        #[pagable_typetag(dice::DiceKeyDyn)]
        struct PackageFileLookupKey(PackageLabel);

        #[async_trait]
        impl Key for PackageFileLookupKey {
            type Value = buck2_error::Result<Option<Arc<PackageFilePath>>>;

            async fn compute(
                &self,
                ctx: &mut DiceComputations,
                _cancellation: &CancellationContext,
            ) -> Self::Value {
                for package_file_path in PackageFilePath::for_dir(self.0.as_cell_path()) {
                    if DiceFileComputations::exists_matching_exact_case(
                        ctx,
                        package_file_path.path().as_ref(),
                    )
                    .await?
                    {
                        return Ok(Some(Arc::new(package_file_path)));
                    }
                }
                Ok(None)
            }

            fn equality(x: &Self::Value, y: &Self::Value) -> bool {
                match (x, y) {
                    (Ok(x), Ok(y)) => x == y,
                    _ => false,
                }
            }

            fn validity(x: &Self::Value) -> bool {
                x.is_ok()
            }

            fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
                OkPagableValueSerialize::<Self::Value>::new()
            }
        }

        match self
            .ctx
            .compute(&PackageFileLookupKey(package.dupe()))
            .await??
        {
            Some(package_file_path) => {
                let (module, deps) = self
                    .prepare_eval(StarlarkPath::PackageFile(&package_file_path))
                    .await?;
                Ok(Some(((*package_file_path).clone(), module, deps)))
            }
            None => Ok(None),
        }
    }

    async fn eval_package_file_uncached(
        &mut self,
        path: PackageLabel,
        cancellation: &CancellationContext,
    ) -> buck2_error::Result<SuperPackage> {
        let parent = self.eval_parent_package_file(path.dupe()).await?;
        let ast_deps = self.prepare_package_file_eval(path.dupe()).await?;

        let (package_file_path, ast, deps) = match ast_deps {
            Some(x) => x,
            None => {
                // If there's no `PACKAGE` file, return parent.
                return Ok(parent);
            }
        };

        let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
        let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;

        let configs = &self.configs;
        let ctx = &mut *self.ctx;

        let eval_kind = StarlarkEvalKind::LoadPackageFile(path.dupe());
        let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;

        let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(ctx, buckconfig, root_buckconfig);

        configs
            .eval_package_file(
                &package_file_path,
                ast,
                parent,
                &mut buckconfigs,
                deps.get_loaded_modules(),
                provider,
                cancellation,
            )
            .with_buck_error_context(|| format!("evaluating Starlark PACKAGE file `{path}`"))
    }

    pub(crate) async fn eval_package_file(
        &mut self,
        path: PackageLabel,
    ) -> buck2_error::Result<SuperPackage> {
        #[derive(Debug, Display, Clone, Allocative, Eq, PartialEq, Hash, Pagable)]
        #[pagable_typetag(dice::DiceKeyDyn)]
        struct PackageFileKey(PackageLabel);

        #[async_trait]
        impl Key for PackageFileKey {
            type Value = buck2_error::Result<SuperPackage>;

            async fn compute(
                &self,
                ctx: &mut DiceComputations,
                cancellation: &CancellationContext,
            ) -> Self::Value {
                let mut interpreter = ctx
                    .get_interpreter_calculator(OwnedStarlarkPath::PackageFile(
                        PackageFilePath::package_file_for_dir(self.0.as_cell_path()),
                    ))
                    .await?;
                interpreter
                    .eval_package_file_uncached(self.0.dupe(), cancellation)
                    .await
            }

            fn equality(x: &Self::Value, y: &Self::Value) -> bool {
                match (x, y) {
                    (Ok(x), Ok(y)) => x == y,
                    _ => false,
                }
            }

            fn validity(x: &Self::Value) -> bool {
                x.is_ok()
            }

            fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
                OkPagableValueSerialize::<Self::Value>::new()
            }
        }

        self.ctx.compute(&PackageFileKey(path)).await?
    }

    /// Most directories do not contain a `PACKAGE` file, this function
    /// optimizes `eval_package_file` for this case by avoiding creation of DICE key.
    pub(crate) async fn eval_package_file_for_build_file(
        &mut self,
        package: PackageLabel,
        package_listing: &PackageListing,
    ) -> buck2_error::Result<SuperPackage> {
        for package_file_name in PackageFilePath::package_file_names() {
            if package_listing
                .get_file(package_file_name.as_ref())
                .is_some()
            {
                return self.eval_package_file(package).await;
            }
        }

        // Without this optimization, `cquery <that android target>` has 6% time regression.
        // With this optimization, check for `PACKAGE` files adds 2% to time.
        self.eval_parent_package_file(package).await
    }

    async fn resolve_package_listing(
        ctx: &mut DiceComputations<'_>,
        package: PackageLabel,
    ) -> buck2_error::Result<PackageListing> {
        span_async_simple(
            buck2_data::LoadPackageStart {
                path: package.as_cell_path().to_string(),
            },
            DicePackageListingResolver(ctx).resolve_package_listing(package.dupe()),
            buck2_data::LoadPackageEnd {
                path: package.as_cell_path().to_string(),
            },
        )
        .await
    }

    async fn resolve_package_listing_with_strategy(
        ctx: &mut DiceComputations<'_>,
        package: PackageLabel,
        strategy: PackageListingStrategy,
    ) -> buck2_error::Result<PackageListing> {
        span_async_simple(
            buck2_data::LoadPackageStart {
                path: package.as_cell_path().to_string(),
            },
            DicePackageListingResolver(ctx)
                .resolve_package_listing_with_strategy(package.dupe(), strategy),
            buck2_data::LoadPackageEnd {
                path: package.as_cell_path().to_string(),
            },
        )
        .await
    }

    async fn resolve_bazel_package_data(
        ctx: &mut DiceComputations<'_>,
        package: PackageLabel,
        requests: BTreeSet<BazelPackageDataRequest>,
    ) -> buck2_error::Result<BTreeMap<BazelPackageDataRequest, Arc<Vec<String>>>> {
        compute_bazel_package_data(ctx, package, requests).await
    }

    async fn resolve_bazel_build_file(
        ctx: &mut DiceComputations<'_>,
        package: PackageLabel,
    ) -> buck2_error::Result<BuildFilePath> {
        let buildfile_candidates =
            DiceFileComputations::buildfiles(ctx, package.cell_name()).await?;
        let package_root = package.as_cell_path().to_owned();
        for candidate in buildfile_candidates.iter() {
            let path = package_root.join(candidate);
            let metadata = DiceFileComputations::read_path_metadata_if_exists(ctx, path.as_ref())
                .await
                .with_buck_error_context(|| format!("Error checking Bazel build file `{path}`"))?;
            match metadata {
                Some(RawPathMetadata::File(_)) | Some(RawPathMetadata::Symlink { .. }) => {
                    return Ok(BuildFilePath::new(package.dupe(), candidate.to_owned()));
                }
                Some(RawPathMetadata::Directory) | None => {}
            }
        }

        let candidates = buildfile_candidates
            .iter()
            .map(|candidate| format!("`{candidate}`"))
            .collect::<Vec<_>>()
            .join(", ");
        Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "package `{}` has no build file; expected one of {candidates}",
            package.as_cell_path()
        ))
    }

    pub async fn eval_build_file(
        &mut self,
        package: PackageLabel,
        cancellation: &CancellationContext,
    ) -> (TimeSpan, buck2_error::Result<Arc<EvaluationResult>>) {
        let mut now = None;
        let eval_kind = StarlarkEvalKind::LoadBuildFile(package.dupe());
        let eval_result: buck2_error::Result<_> = try {
            let package_cell = package.cell_name();
            let ((), bazel_compat_listing) = self
                .ctx
                .try_compute2(
                    |ctx| check_starlark_stack_size(ctx).boxed(),
                    |ctx| {
                        async move { bazel_compat_package_listing_enabled(ctx, package_cell).await }
                            .boxed()
                    },
                )
                .await?;

            let (
                mut listing,
                build_file_path,
                ast,
                deps,
                mut listing_strategy,
                mut bazel_package_data,
            ) = if bazel_compat_listing {
                let build_file_path =
                    Self::resolve_bazel_build_file(self.ctx, package.dupe()).await?;
                let listing = PackageListing::empty(build_file_path.filename().to_owned());
                let parse_data = self
                    .parse_file(StarlarkPath::BuildFile(&build_file_path))
                    .await??;
                let package_data_requests = parse_data
                    .bazel_package_data_requests
                    .clone()
                    .unwrap_or_default();
                let (deps, package_data) = if package_data_requests.is_empty() {
                    let deps =
                        Self::eval_deps_with_cycle_guard(self.ctx, &parse_data.imports).await?;
                    (deps, BTreeMap::new())
                } else {
                    let imports = parse_data.imports.clone();
                    self.ctx
                        .try_compute2(
                            move |ctx| {
                                async move { Self::eval_deps_with_cycle_guard(ctx, &imports).await }
                                    .boxed()
                            },
                            {
                                let package = package.dupe();
                                move |ctx| {
                                    let requests = package_data_requests.clone();
                                    async move {
                                        Self::resolve_bazel_package_data(
                                            ctx,
                                            package.dupe(),
                                            requests,
                                        )
                                        .await
                                    }
                                    .boxed()
                                }
                            },
                        )
                        .await?
                };
                (
                    listing,
                    build_file_path,
                    parse_data.ast,
                    deps,
                    PackageListingStrategy::Shallow,
                    Some(package_data),
                )
            } else {
                let listing = Self::resolve_package_listing(self.ctx, package.dupe()).await?;
                let build_file_path =
                    BuildFilePath::new(package.dupe(), listing.buildfile().to_owned());
                let (ast, deps) = self
                    .prepare_eval(StarlarkPath::BuildFile(&build_file_path))
                    .await?;
                (
                    listing,
                    build_file_path,
                    ast,
                    deps,
                    PackageListingStrategy::Recursive,
                    None,
                )
            };
            let super_package = if bazel_compat_listing {
                self.eval_package_file(package.dupe()).await?
            } else {
                self.eval_package_file_for_build_file(package.dupe(), &listing)
                    .await?
            };

            let package_boundary_exception = self
                .ctx
                .get_package_boundary_exception(package.as_cell_path())
                .await?
                .is_some();
            let buckconfig = self.get_legacy_buck_config_for_starlark().await?;
            let root_buckconfig = self.ctx.get_legacy_root_config_on_dice().await?;
            let module_id = build_file_path.to_string();
            let cell_str = build_file_path.cell().as_str().to_owned();

            let configs = &self.configs;

            now = Some(TimeSpan::start_now());
            let (profile_data, eval_result) = loop {
                let ctx = &mut *self.ctx;
                let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind.dupe()).await?;
                let mut buckconfigs = ConfigsOnDiceViewForStarlark::new(
                    ctx,
                    buckconfig.dupe(),
                    root_buckconfig.dupe(),
                );
                let start_event = buck2_data::LoadBuildFileStart {
                    cell: cell_str.clone(),
                    module_id: module_id.clone(),
                };
                let span_module_id = module_id.clone();
                let span_cell_str = cell_str.clone();

                let eval_attempt = span(start_event, || {
                    let result_with_stats = configs
                        .eval_build_file(
                            &build_file_path,
                            &mut buckconfigs,
                            listing.dupe(),
                            listing_strategy.clone(),
                            bazel_package_data.clone(),
                            super_package.dupe(),
                            package_boundary_exception,
                            ast.clone(),
                            deps.get_loaded_modules(),
                            provider,
                            false,
                            cancellation,
                        )
                        .with_buck_error_context(|| {
                            format!("Error evaluating build file: `{}`", build_file_path)
                        });
                    let error = result_with_stats.as_ref().err().map(|e| format!("{e:#}"));
                    let (
                        starlark_peak_allocated_bytes,
                        cpu_instruction_count,
                        starlark_tick_count,
                        target_count,
                    ) = match &result_with_stats {
                        Ok(BuildFileEvalResult::Complete(_, rs)) => (
                            Some(rs.starlark_peak_allocated_bytes),
                            rs.cpu_instruction_count,
                            Some(rs.starlark_tick_count),
                            Some(rs.result.targets().len() as u64),
                        ),
                        Ok(BuildFileEvalResult::NeedsPackageListing(_)) => (None, None, None, None),
                        Ok(BuildFileEvalResult::NeedsBazelPackageData(_)) => {
                            (None, None, None, None)
                        }
                        Err(_) => (None, None, None, None),
                    };

                    (
                        result_with_stats,
                        buck2_data::LoadBuildFileEnd {
                            module_id: span_module_id,
                            cell: span_cell_str,
                            target_count,
                            starlark_peak_allocated_bytes,
                            cpu_instruction_count,
                            error,
                            starlark_tick_count,
                        },
                    )
                })?;
                drop(buckconfigs);

                match eval_attempt {
                    BuildFileEvalResult::Complete(profile_data, eval_result) => {
                        break (profile_data, eval_result);
                    }
                    BuildFileEvalResult::NeedsPackageListing(next_strategy) => {
                        if listing_strategy.covers(&next_strategy) {
                            return (
                                now.unwrap().end_now(),
                                Err(internal_error!(
                                    "package listing restart did not expand strategy from `{:?}` to `{:?}`",
                                    listing_strategy,
                                    next_strategy
                                )),
                            );
                        }
                        listing_strategy = next_strategy;
                        listing = Self::resolve_package_listing_with_strategy(
                            self.ctx,
                            package.dupe(),
                            listing_strategy.clone(),
                        )
                        .await?;
                    }
                    BuildFileEvalResult::NeedsBazelPackageData(requests) => {
                        let next_results =
                            Self::resolve_bazel_package_data(self.ctx, package.dupe(), requests)
                                .await?;
                        let data = bazel_package_data.get_or_insert_with(BTreeMap::new);
                        data.extend(next_results);
                    }
                }
            };

            let mut eval_result = eval_result.result;

            if eval_result.starlark_profile.is_some() {
                return (
                    now.unwrap().end_now(),
                    Err(internal_error!(
                        "starlark_profile field must not be set yet"
                    )),
                );
            }
            eval_result.starlark_profile = profile_data.map(|d| d as _);
            eval_result
        };

        (
            now.map_or(TimeSpan::empty_now(), |v| v.end_now()),
            eval_result.map(Arc::new),
        )
    }
}

mod keys {
    use allocative::Allocative;
    use buck2_interpreter::paths::module::OwnedStarlarkModulePath;
    use derive_more::Display;
    use pagable::Pagable;
    use pagable::pagable_typetag;

    #[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    pub struct EvalImportKey(pub OwnedStarlarkModulePath);
}

pub mod testing {
    // re-exports for testing
    pub use super::keys::EvalImportKey;
}
