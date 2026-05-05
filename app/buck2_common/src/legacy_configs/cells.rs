/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use allocative::Allocative;
use buck2_core::buck2_env;
use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::CellResolver;
use buck2_core::cells::alias::NonEmptyCellAlias;
use buck2_core::cells::cell_root_path::CellRootPath;
use buck2_core::cells::cell_root_path::CellRootPathBuf;
use buck2_core::cells::external::BZLMOD_BAZEL_COMPAT_VERSION;
use buck2_core::cells::external::BzlmodBazelFeaturesGlobalsSetup;
use buck2_core::cells::external::BzlmodBazelFeaturesVersionSetup;
use buck2_core::cells::external::BzlmodCcAutoconfSetup;
use buck2_core::cells::external::BzlmodCcAutoconfToolchainsSetup;
use buck2_core::cells::external::BzlmodCellSetup;
use buck2_core::cells::external::BzlmodGeneratedCellGenerator;
use buck2_core::cells::external::BzlmodGeneratedCellSetup;
use buck2_core::cells::external::BzlmodHostPlatformSetup;
use buck2_core::cells::external::BzlmodHttpArchiveSetup;
use buck2_core::cells::external::BzlmodJavaLocalJdkSetup;
use buck2_core::cells::external::BzlmodLocalConfigPlatformSetup;
use buck2_core::cells::external::BzlmodModuleExtensionRepoSetup;
use buck2_core::cells::external::BzlmodPatch;
use buck2_core::cells::external::BzlmodPythonHubSetup;
use buck2_core::cells::external::BzlmodRepositoryRuleInvocationSetup;
use buck2_core::cells::external::BzlmodRepositoryRuleSetup;
use buck2_core::cells::external::BzlmodShellConfigSetup;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::external::GitCellSetup;
use buck2_core::cells::external::GitObjectFormat;
use buck2_core::cells::external::bzlmod_cell_name;
use buck2_core::cells::external::register_bzlmod_cell_aliases;
use buck2_core::cells::external::register_external_cell_origin;
use buck2_core::cells::name::CellName;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_error::BuckErrorContext;
use buck2_error::buck2_error;
use buck2_error::conversion::from_any_with_tag;
use buck2_fs::paths::RelativePath;
use buck2_fs::paths::abs_path::AbsPath;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_hash::StdBuckHashSet;
use buck2_http::HttpClient;
use buck2_http::HttpClientBuilder;
use dice::DiceComputations;
use dupe::Dupe;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use pagable::Pagable;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::dice::cells::HasCellResolver;
use crate::dice::data::HasIoProvider;
use crate::external_cells::EXTERNAL_CELLS_IMPL;
use crate::legacy_configs::aggregator::CellsAggregator;
use crate::legacy_configs::args::ResolvedLegacyConfigArg;
use crate::legacy_configs::args::resolve_config_args;
use crate::legacy_configs::args::to_proto_config_args;
use crate::legacy_configs::configs::BazelCompatCellAlias;
use crate::legacy_configs::configs::BazelCompatExternalModule;
use crate::legacy_configs::configs::BazelCompatGeneratedModule;
use crate::legacy_configs::configs::BazelCompatRegistryModule;
use crate::legacy_configs::configs::LegacyBuckConfig;
use crate::legacy_configs::dice::HasInjectedLegacyConfigs;
use crate::legacy_configs::file_ops::ConfigDirEntry;
use crate::legacy_configs::file_ops::ConfigParserFileOps;
use crate::legacy_configs::file_ops::ConfigPath;
use crate::legacy_configs::file_ops::DefaultConfigParserFileOps;
use crate::legacy_configs::file_ops::DiceConfigFileOps;
use crate::legacy_configs::file_ops::push_all_files_from_a_directory;
use crate::legacy_configs::key::BuckconfigKeyRef;
use crate::legacy_configs::parser::LegacyConfigParser;
use crate::legacy_configs::path::DEFAULT_EXTERNAL_CONFIG_SOURCES;
use crate::legacy_configs::path::DEFAULT_PROJECT_CONFIG_SOURCES;
use crate::legacy_configs::path::DOT_BUCKCONFIG_LOCAL;
use crate::legacy_configs::path::ExternalConfigSource;
use crate::legacy_configs::path::ProjectConfigSource;

const PRIMARY_BUCKCONFIG: &str = ".buckconfig";
const BAZEL_PROJECT_ROOT_MARKERS: &[&str] = &["MODULE.bazel", "WORKSPACE.bazel", "WORKSPACE"];

/// Buckconfigs can partially be loaded from within dice. However, some parts of what makes up the
/// buckconfig comes from outside the buildgraph, and this type represents those parts.
#[derive(Clone, PartialEq, Eq, Allocative, Pagable)]
pub struct ExternalBuckconfigData {
    // The result of parsing the buckconfigs coming from either global (e.g. /etc/buckconfig.d) or
    // user (e.g. ~/.buckconfig.d or $home_dir/.buckconfig.local) files/dirs outside of the repo
    // The order matters here and reflects the same order these are processed in buck, see
    // https://fburl.com/code/8ue78p1j
    external_path_configs: Vec<ExternalPathBuckconfigData>,
    // The result of parsing the buckconfigs coming from command line args (e.g. --config or --config-file)
    args: Vec<ResolvedLegacyConfigArg>,

    bzlmod_module_extension_results_complete: bool,
    bzlmod_module_extension_results: Vec<BzlmodEvaluatedModuleExtension>,
    bzlmod_module_aliases: Option<Arc<BazelModuleCellAliases>>,
}

#[derive(PartialEq, Eq, Allocative, Clone, Pagable)]
pub struct ExternalPathBuckconfigData {
    pub(crate) parse_state: LegacyConfigParser,
    pub(crate) origin_path: ConfigPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative, Pagable)]
pub struct BzlmodModuleExtensionEvaluationRequest {
    pub parent_canonical_repo_name: Arc<str>,
    pub parent_is_root: bool,
    pub extension_bzl_file: Arc<str>,
    pub extension_bzl_cell: Arc<str>,
    pub extension_bzl_path: Arc<str>,
    pub extension_unique_name: Arc<str>,
    pub extension_name: Arc<str>,
    pub extension_usages_json: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative, Pagable)]
pub struct BzlmodEvaluatedModuleExtension {
    pub parent_canonical_repo_name: Arc<str>,
    pub parent_is_root: bool,
    pub extension_bzl_file: Arc<str>,
    pub extension_bzl_cell: Arc<str>,
    pub extension_bzl_path: Arc<str>,
    pub extension_unique_name: Arc<str>,
    pub extension_name: Arc<str>,
    pub repo_names: Vec<Arc<str>>,
    pub registered_toolchains: Vec<Arc<str>>,
    pub repository_rules: Vec<BzlmodEvaluatedRepositoryRule>,
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Allocative,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodEvaluatedRepositoryRule {
    pub repo_name: String,
    pub rule_bzl_cell: String,
    pub rule_bzl_path: String,
    pub rule_bzl_build_file_cell: String,
    pub rule_name: String,
    pub attrs: Vec<(String, String)>,
    #[serde(default)]
    pub label_deps: Vec<String>,
}

impl ExternalBuckconfigData {
    pub fn testing_default() -> Self {
        Self {
            external_path_configs: Vec::new(),
            args: Vec::new(),
            bzlmod_module_extension_results_complete: false,
            bzlmod_module_extension_results: Vec::new(),
            bzlmod_module_aliases: None,
        }
    }

    pub fn filter_values<F>(self, filter: F) -> Self
    where
        F: Fn(&BuckconfigKeyRef) -> bool,
    {
        Self {
            external_path_configs: self
                .external_path_configs
                .into_iter()
                .map(|o| ExternalPathBuckconfigData {
                    parse_state: o.parse_state.filter_values(&filter),
                    origin_path: o.origin_path,
                })
                .collect(),
            args: self
                .args
                .into_iter()
                .filter(|arg| match arg {
                    ResolvedLegacyConfigArg::Flag(flag) => {
                        flag.cell.is_some()
                            || filter(&BuckconfigKeyRef {
                                section: &flag.section,
                                property: &flag.key,
                            })
                    }
                    _ => true,
                })
                .collect(),
            bzlmod_module_extension_results_complete: self.bzlmod_module_extension_results_complete,
            bzlmod_module_extension_results: self.bzlmod_module_extension_results,
            bzlmod_module_aliases: self.bzlmod_module_aliases,
        }
    }

    async fn get_local_config_components(
        project_root: &ProjectRoot,
    ) -> Vec<buck2_data::BuckconfigComponent> {
        use buck2_data::buckconfig_component::Data::GlobalExternalConfigFile;
        let file_ops = &mut DefaultConfigParserFileOps {
            project_fs: project_root.dupe(),
        };
        let mut local_config_components = Vec::new();
        if let Ok(legacy_cells) =
            BuckConfigBasedCells::parse_with_config_args(project_root, &[]).await
        {
            let path = ForwardRelativePath::new(DOT_BUCKCONFIG_LOCAL).expect(
                "Internal error: .buckconfig.local should always be a valid forward relative path",
            );
            for (_cell, cell_instance) in legacy_cells.cell_resolver.cells() {
                let relative_path = cell_instance.path().as_project_relative_path().join(path);
                let origin_path = relative_path.to_string();
                let local_config = ConfigPath::Project(relative_path);

                let mut parser = LegacyConfigParser::new();
                if parser
                    .parse_file(&local_config, None, true, file_ops)
                    .await
                    .is_ok()
                {
                    let values = parser.to_proto_external_config_values(false);
                    if values.is_empty() {
                        // Don't create an empty component for cells with non-existing .buckconfig.local
                        continue;
                    }
                    local_config_components.push(buck2_data::BuckconfigComponent {
                        data: Some(GlobalExternalConfigFile(buck2_data::GlobalExternalConfig {
                            values,
                            origin_path,
                        })),
                    });
                }
            }
        }
        local_config_components
    }

    pub async fn get_buckconfig_components(
        &self,
        project_root: &ProjectRoot,
    ) -> Vec<buck2_data::BuckconfigComponent> {
        use buck2_data::buckconfig_component::Data::GlobalExternalConfigFile;
        let mut res: Vec<buck2_data::BuckconfigComponent> = self
            .external_path_configs
            .clone()
            .into_iter()
            .map(|o| {
                let external_file = buck2_data::GlobalExternalConfig {
                    values: o.parse_state.to_proto_external_config_values(false),
                    origin_path: o.origin_path.to_string(),
                };
                buck2_data::BuckconfigComponent {
                    data: Some(GlobalExternalConfigFile(external_file)),
                }
            })
            .collect();

        res.extend(Self::get_local_config_components(project_root).await);
        res.extend(to_proto_config_args(&self.args));
        res
    }
}

/// Used for creating a CellResolver in a buckv1-compatible way based on values
/// in .buckconfig in each cell.
///
/// We'll traverse the structure of the `[cells]` sections starting from
/// the root .buckconfig. All aliases found in the root config will also be
/// available in all other cells (v1 provides that same behavior).
///
/// We don't (currently) enforce that all aliases appear in the root config, but
/// unlike v1, our cells implementation works just fine if that isn't the case.
pub struct BuckConfigBasedCells {
    pub cell_resolver: CellResolver,
    pub root_config: LegacyBuckConfig,
    pub config_paths: StdBuckHashSet<ConfigPath>,
    pub external_data: ExternalBuckconfigData,
    pub bzlmod_module_extension_evaluation_requests: Vec<BzlmodModuleExtensionEvaluationRequest>,
}

impl BuckConfigBasedCells {
    /// In the client and one place in the daemon, we need access to the alias resolver for the cwd
    /// in some places where we don't have normal dice access
    ///
    /// This function reads buckconfigs to compute an appropriate cell alias resolver to make that
    /// possible.
    pub async fn get_cell_alias_resolver_for_cwd_fast(
        &self,
        project_fs: &ProjectRoot,
        cwd: &ProjectRelativePath,
    ) -> buck2_error::Result<CellAliasResolver> {
        self.get_cell_alias_resolver_for_cwd_fast_with_file_ops(
            &mut DefaultConfigParserFileOps {
                project_fs: project_fs.dupe(),
            },
            cwd,
        )
        .await
    }

    pub(crate) async fn get_cell_alias_resolver_for_cwd_fast_with_file_ops(
        &self,
        file_ops: &mut dyn ConfigParserFileOps,
        cwd: &ProjectRelativePath,
    ) -> buck2_error::Result<CellAliasResolver> {
        let cell_name = self.cell_resolver.find(cwd);
        let cell_path = self.cell_resolver.get(cell_name)?.path();

        let follow_includes = false;
        let is_bzlmod_cell = cell_name.as_str().starts_with("bzlmod_");

        let config_paths = if is_bzlmod_cell {
            Vec::new()
        } else {
            get_project_buckconfig_paths(cell_path, file_ops).await?
        };
        let config = LegacyBuckConfig::finish_parse(
            self.external_data.external_path_configs.clone(),
            &config_paths,
            cell_path,
            file_ops,
            &[],
            follow_includes,
        )
        .await?;
        let config = if is_bzlmod_cell
            || cell_name.as_str() == "bazel_tools"
            || should_apply_bazel_compat_defaults(cell_path, file_ops).await?
        {
            let module_aliases =
                get_bazel_module_resolution_for_external_data(file_ops, &self.external_data)
                    .await?;
            config.with_bazel_compat_defaults(
                module_aliases.aliases_for_cell(cell_name.as_str()),
                &module_aliases.external_modules,
                &module_aliases.registered_toolchains,
            )
        } else {
            config
        };

        CellAliasResolver::new_for_non_root_cell(
            cell_name,
            self.cell_resolver.root_cell_cell_alias_resolver(),
            BuckConfigBasedCells::get_cell_aliases_from_config(&config)?,
        )
    }

    pub async fn parse_with_config_args(
        project_fs: &ProjectRoot,
        config_args: &[buck2_cli_proto::ConfigOverride],
    ) -> buck2_error::Result<Self> {
        Self::parse_with_config_args_and_bzlmod_module_extension_results(
            project_fs,
            config_args,
            false,
            Vec::new(),
        )
        .await
    }

    pub async fn parse_with_config_args_and_bzlmod_module_extension_results(
        project_fs: &ProjectRoot,
        config_args: &[buck2_cli_proto::ConfigOverride],
        bzlmod_module_extension_results_complete: bool,
        bzlmod_module_extension_results: Vec<BzlmodEvaluatedModuleExtension>,
    ) -> buck2_error::Result<Self> {
        Self::parse_with_file_ops_and_options(
            &mut DefaultConfigParserFileOps {
                project_fs: project_fs.dupe(),
            },
            config_args,
            false, /* follow includes */
            bzlmod_module_extension_results_complete,
            bzlmod_module_extension_results,
        )
        .await
    }

    pub async fn testing_parse_with_file_ops(
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[buck2_cli_proto::ConfigOverride],
    ) -> buck2_error::Result<Self> {
        Self::parse_with_file_ops_and_options(
            file_ops,
            config_args,
            true, /* follow includes */
            false,
            Vec::new(),
        )
        .await
    }

    async fn parse_with_file_ops_and_options(
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[buck2_cli_proto::ConfigOverride],
        follow_includes: bool,
        bzlmod_module_extension_results_complete: bool,
        bzlmod_module_extension_results: Vec<BzlmodEvaluatedModuleExtension>,
    ) -> buck2_error::Result<Self> {
        Self::parse_with_file_ops_and_options_inner(
            file_ops,
            config_args,
            follow_includes,
            bzlmod_module_extension_results_complete,
            bzlmod_module_extension_results,
        )
        .await
        .buck_error_context("Parsing cells")
    }

    async fn parse_with_file_ops_and_options_inner(
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[buck2_cli_proto::ConfigOverride],
        follow_includes: bool,
        bzlmod_module_extension_results_complete: bool,
        bzlmod_module_extension_results: Vec<BzlmodEvaluatedModuleExtension>,
    ) -> buck2_error::Result<Self> {
        // Tracing file ops to record config file accesses on command invocation.
        struct TracingFileOps<'a> {
            inner: &'a mut dyn ConfigParserFileOps,
            trace: StdBuckHashSet<ConfigPath>,
        }

        #[async_trait::async_trait]
        impl ConfigParserFileOps for TracingFileOps<'_> {
            async fn read_file_lines_if_exists(
                &mut self,
                path: &ConfigPath,
            ) -> buck2_error::Result<Option<Vec<String>>> {
                let res = self.inner.read_file_lines_if_exists(path).await?;

                if res.is_some() {
                    self.trace.insert(path.clone());
                }

                Ok(res)
            }

            async fn read_dir(
                &mut self,
                path: &ConfigPath,
            ) -> buck2_error::Result<Vec<ConfigDirEntry>> {
                self.inner.read_dir(path).await
            }
        }

        let mut file_ops = TracingFileOps {
            inner: file_ops,
            trace: Default::default(),
        };

        // NOTE: This will _not_ perform IO unless it needs to.
        let processed_config_args = resolve_config_args(config_args, &mut file_ops).await?;

        let external_paths = get_external_buckconfig_paths(&mut file_ops).await?;
        let started_parse = LegacyBuckConfig::start_parse_for_external_files(
            &external_paths,
            &mut file_ops,
            follow_includes,
        )
        .await?;

        let root_path = CellRootPathBuf::new(ProjectRelativePath::empty().to_owned());

        let buckconfig_paths = get_project_buckconfig_paths(&root_path, &mut file_ops).await?;

        let root_config = LegacyBuckConfig::finish_parse(
            started_parse.clone(),
            buckconfig_paths.as_slice(),
            &root_path,
            &mut file_ops,
            &processed_config_args,
            follow_includes,
        )
        .await?;
        let mut bzlmod_module_extension_evaluation_requests = Vec::new();
        let mut bzlmod_module_aliases = None;
        let root_config = if should_apply_bazel_compat_defaults(&root_path, &mut file_ops).await? {
            let module_aliases = Arc::new(
                get_bazel_module_resolution(
                    &root_path,
                    &mut file_ops,
                    bzlmod_module_extension_results_complete,
                    &bzlmod_module_extension_results,
                )
                .await?,
            );
            bzlmod_module_extension_evaluation_requests =
                module_aliases.module_extension_evaluation_requests.clone();
            let root_config = root_config.with_bazel_compat_defaults(
                module_aliases.aliases_for_cell("root"),
                &module_aliases.external_modules,
                &module_aliases.registered_toolchains,
            );
            bzlmod_module_aliases = Some(module_aliases);
            root_config
        } else {
            root_config
        };

        let mut cell_definitions = Vec::new();

        // `cells` is preferred over `repositories` since it's more clear, however it's unlikely
        // that we'll ever remove `repositories` since that's probably unnecessary breakage in OSS.
        //
        // Note that `cells` is buck2-only
        let repositories = root_config
            .get_section("cells")
            .or_else(|| root_config.get_section("repositories"));
        if let Some(repositories) = repositories {
            for (alias, alias_path) in repositories.iter() {
                let alias_path = CellRootPathBuf::new(
                    root_path.as_project_relative_path()
                        .join_normalized(RelativePath::new(alias_path.as_str()))
                        .with_buck_error_context(|| {
                            format!(
                                "expected alias path to be a relative path, but found `{}` for `{}`",
                                alias_path.as_str(),
                                alias,
                            )
                        })?
                );
                let name = CellName::unchecked_new(alias)?;
                cell_definitions.push((name, alias_path));
            }
        }

        let root_aliases = Self::get_cell_aliases_from_config(&root_config)?.collect();

        let mut aggregator = CellsAggregator::new(cell_definitions, root_aliases)?;

        if let Some(external_cells) = root_config.get_section("external_cells") {
            for (alias, origin) in external_cells.iter() {
                if origin.as_str() == "disabled" {
                    // Ignore this entry, treat it as a normal cell
                    continue;
                }
                let alias = NonEmptyCellAlias::new(alias.to_owned())?;
                let name = aggregator.resolve_root_alias(alias)?;
                let origin = Self::parse_external_cell_origin(name, origin.as_str(), &root_config)?;
                register_external_cell_origin(name, origin.dupe());
                if let ExternalCellOrigin::Bundled(name) = origin {
                    // This code is executed both in the client and in the daemon. When in the
                    // client and using a client-only build, this late binding might not be bound,
                    // and so we can't check this. That doesn't matter though, as we'll get an error
                    // when this fails in the daemon anyway
                    if let Ok(imp) = EXTERNAL_CELLS_IMPL.get() {
                        imp.check_bundled_cell_exists(name)?;
                    }
                }
                aggregator.mark_external_cell(name, origin)?;
            }
        }

        let cell_resolver = aggregator.make_cell_resolver()?;

        Ok(Self {
            cell_resolver,
            root_config,
            config_paths: file_ops.trace,
            external_data: ExternalBuckconfigData {
                external_path_configs: started_parse,
                args: processed_config_args,
                bzlmod_module_extension_results_complete,
                bzlmod_module_extension_results,
                bzlmod_module_aliases,
            },
            bzlmod_module_extension_evaluation_requests,
        })
    }

    pub(crate) fn get_cell_aliases_from_config(
        config: &LegacyBuckConfig,
    ) -> buck2_error::Result<impl Iterator<Item = (NonEmptyCellAlias, NonEmptyCellAlias)> + use<>>
    {
        let mut aliases = Vec::new();
        if let Some(section) = config
            .get_section("cell_aliases")
            .or_else(|| config.get_section("repository_aliases"))
        {
            for (alias, destination) in section.iter() {
                let alias = NonEmptyCellAlias::new(alias.to_owned())?;
                let destination = NonEmptyCellAlias::new(destination.as_str().to_owned())?;
                aliases.push((alias, destination));
            }
        }
        Ok(aliases.into_iter())
    }

    pub(crate) async fn parse_single_cell_with_dice(
        ctx: &mut DiceComputations<'_>,
        cell_path: &CellRootPath,
    ) -> buck2_error::Result<LegacyBuckConfig> {
        let resolver = ctx.get_cell_resolver().await?;
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let external_data = ctx.get_injected_external_buckconfig_data().await?;
        let cell_name = resolver.find(cell_path.as_project_relative_path());

        let mut file_ops = DiceConfigFileOps::new(ctx, project_fs, &resolver);

        Self::parse_single_cell_with_file_ops_inner(
            &external_data,
            &mut file_ops,
            cell_name.as_str(),
            cell_path,
        )
        .await
    }

    pub async fn parse_single_cell(
        &self,
        cell: CellName,
        project_fs: &ProjectRoot,
    ) -> buck2_error::Result<LegacyBuckConfig> {
        self.parse_single_cell_with_file_ops(
            cell,
            &mut DefaultConfigParserFileOps {
                project_fs: project_fs.dupe(),
            },
        )
        .await
    }

    pub(crate) async fn parse_single_cell_with_file_ops(
        &self,
        cell: CellName,
        file_ops: &mut dyn ConfigParserFileOps,
    ) -> buck2_error::Result<LegacyBuckConfig> {
        Self::parse_single_cell_with_file_ops_inner(
            &self.external_data,
            file_ops,
            cell.as_str(),
            self.cell_resolver.get(cell)?.path(),
        )
        .await
    }

    async fn parse_single_cell_with_file_ops_inner(
        external_data: &ExternalBuckconfigData,
        file_ops: &mut dyn ConfigParserFileOps,
        cell_name: &str,
        cell_path: &CellRootPath,
    ) -> buck2_error::Result<LegacyBuckConfig> {
        let is_bzlmod_cell = cell_name.starts_with("bzlmod_");
        let config_paths = if is_bzlmod_cell {
            Vec::new()
        } else {
            get_project_buckconfig_paths(cell_path, file_ops).await?
        };
        let config = LegacyBuckConfig::finish_parse(
            external_data.external_path_configs.clone(),
            &config_paths,
            cell_path,
            file_ops,
            external_data.args.as_ref(),
            /* follow includes */ true,
        )
        .await?;

        if is_bzlmod_cell
            || cell_name == "bazel_tools"
            || should_apply_bazel_compat_defaults(cell_path, file_ops).await?
        {
            let module_aliases =
                get_bazel_module_resolution_for_external_data(file_ops, external_data).await?;
            Ok(config.with_bazel_compat_defaults(
                module_aliases.aliases_for_cell(cell_name),
                &module_aliases.external_modules,
                &module_aliases.registered_toolchains,
            ))
        } else {
            Ok(config)
        }
    }

    fn parse_external_cell_origin(
        cell: CellName,
        value: &str,
        config: &LegacyBuckConfig,
    ) -> buck2_error::Result<ExternalCellOrigin> {
        #[derive(buck2_error::Error, Debug)]
        #[buck2(tag = Input)]
        enum ExternalCellOriginParseError {
            #[error("Unknown external cell origin `{0}`")]
            Unknown(String),
            #[error("Missing buckconfig `{0}.{1}` for external cell configuration")]
            MissingConfiguration(String, String),
        }

        let get_config = |section: &str, property: &str| {
            config
                .get(crate::legacy_configs::key::BuckconfigKeyRef { section, property })
                .ok_or_else(|| {
                    ExternalCellOriginParseError::MissingConfiguration(
                        section.to_owned(),
                        property.to_owned(),
                    )
                })
        };

        if value == "bundled" {
            Ok(ExternalCellOrigin::Bundled(cell))
        } else if value == "git" {
            let section = &format!("external_cell_{}", cell.as_str());
            let commit = get_config(section, "commit_hash")?;
            let object_format = match get_config(section, "object_format") {
                Ok(s) => {
                    let object_format = GitObjectFormat::from_str(s)?;
                    object_format.check(commit)?;
                    Option::Some(GitObjectFormat::from_str(s)?)
                }
                Err(_) => {
                    // We pretend that the object format is SHA1 for this check only;
                    // We do not use it when interacting with Git.
                    GitObjectFormat::Sha1.check(commit)?;
                    Option::None
                }
            };
            Ok(ExternalCellOrigin::Git(GitCellSetup {
                git_origin: get_config(section, "git_origin")?.into(),
                commit: Arc::from(commit),
                object_format,
            }))
        } else if value == "bzlmod" {
            let section = &format!("external_cell_{}", cell.as_str());
            let patches: Vec<BzlmodPatchConfig> =
                serde_json::from_str(get_config(section, "patches")?)
                    .buck_error_context("Invalid bzlmod patch configuration")?;
            Ok(ExternalCellOrigin::Bzlmod(BzlmodCellSetup {
                module_name: get_config(section, "module_name")?.into(),
                version: get_config(section, "version")?.into(),
                canonical_repo_name: get_config(section, "canonical_repo_name")?.into(),
                url: get_config(section, "url")?.into(),
                integrity: get_config(section, "integrity")?.into(),
                strip_prefix: config
                    .get(crate::legacy_configs::key::BuckconfigKeyRef {
                        section,
                        property: "strip_prefix",
                    })
                    .map(Arc::from),
                archive_type: config
                    .get(crate::legacy_configs::key::BuckconfigKeyRef {
                        section,
                        property: "archive_type",
                    })
                    .map(Arc::from),
                patches: Arc::new(
                    patches
                        .into_iter()
                        .map(|patch| BzlmodPatch {
                            url: Arc::from(patch.url),
                            integrity: Arc::from(patch.integrity),
                        })
                        .collect(),
                ),
                patch_strip: get_config(section, "patch_strip")?.parse()?,
            }))
        } else if value == "bzlmod_generated" {
            let section = &format!("external_cell_{}", cell.as_str());
            let generator: BzlmodGeneratedRepoConfig =
                serde_json::from_str(get_config(section, "generator")?)
                    .buck_error_context("Invalid generated bzlmod repo configuration")?;
            let generator = match generator {
                BzlmodGeneratedRepoConfig::BazelFeaturesGlobals {
                    parent_canonical_repo_name,
                    bazel_version,
                } => BzlmodGeneratedCellGenerator::BazelFeaturesGlobals(
                    BzlmodBazelFeaturesGlobalsSetup {
                        parent_canonical_repo_name: Arc::from(parent_canonical_repo_name),
                        bazel_version: Arc::from(bazel_version),
                    },
                ),
                BzlmodGeneratedRepoConfig::BazelFeaturesVersion { bazel_version } => {
                    BzlmodGeneratedCellGenerator::BazelFeaturesVersion(
                        BzlmodBazelFeaturesVersionSetup {
                            bazel_version: Arc::from(bazel_version),
                        },
                    )
                }
                BzlmodGeneratedRepoConfig::HostPlatform {} => {
                    BzlmodGeneratedCellGenerator::HostPlatform(BzlmodHostPlatformSetup {})
                }
                BzlmodGeneratedRepoConfig::LocalConfigPlatform {} => {
                    BzlmodGeneratedCellGenerator::LocalConfigPlatform(
                        BzlmodLocalConfigPlatformSetup {},
                    )
                }
                BzlmodGeneratedRepoConfig::CcAutoconfToolchains {
                    parent_canonical_repo_name,
                } => BzlmodGeneratedCellGenerator::CcAutoconfToolchains(
                    BzlmodCcAutoconfToolchainsSetup {
                        parent_canonical_repo_name: Arc::from(parent_canonical_repo_name),
                    },
                ),
                BzlmodGeneratedRepoConfig::CcAutoconf {} => {
                    BzlmodGeneratedCellGenerator::CcAutoconf(BzlmodCcAutoconfSetup {})
                }
                BzlmodGeneratedRepoConfig::ShellConfig {} => {
                    BzlmodGeneratedCellGenerator::ShellConfig(BzlmodShellConfigSetup {})
                }
                BzlmodGeneratedRepoConfig::HttpArchive {
                    repo_name,
                    url,
                    sha256,
                    strip_prefix,
                    archive_type,
                } => BzlmodGeneratedCellGenerator::HttpArchive(BzlmodHttpArchiveSetup {
                    repo_name: Arc::from(repo_name),
                    url: Arc::from(url),
                    sha256: Arc::from(sha256),
                    strip_prefix: strip_prefix.map(Arc::from),
                    archive_type: archive_type.map(Arc::from),
                }),
                BzlmodGeneratedRepoConfig::JavaLocalJdk {} => {
                    BzlmodGeneratedCellGenerator::JavaLocalJdk(BzlmodJavaLocalJdkSetup {})
                }
                BzlmodGeneratedRepoConfig::PythonHub {} => {
                    BzlmodGeneratedCellGenerator::PythonHub(BzlmodPythonHubSetup {})
                }
                BzlmodGeneratedRepoConfig::RepositoryRule { files } => {
                    let files_json = serde_json::to_string(&files)
                        .buck_error_context("Error serializing repository_rule file manifest")?;
                    BzlmodGeneratedCellGenerator::RepositoryRule(BzlmodRepositoryRuleSetup {
                        files_json: Arc::from(files_json),
                        source_dir: None,
                    })
                }
                BzlmodGeneratedRepoConfig::RepositoryRuleInvocation {
                    repo_name,
                    rule_bzl_cell,
                    rule_bzl_path,
                    rule_bzl_build_file_cell,
                    rule_name,
                    attrs,
                    label_deps,
                } => BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(
                    BzlmodRepositoryRuleInvocationSetup {
                        repo_name: Arc::from(repo_name),
                        rule_bzl_cell: Arc::from(rule_bzl_cell),
                        rule_bzl_path: Arc::from(rule_bzl_path),
                        rule_bzl_build_file_cell: Arc::from(rule_bzl_build_file_cell),
                        rule_name: Arc::from(rule_name),
                        attrs: Arc::new(
                            attrs
                                .into_iter()
                                .map(|(key, value)| (Arc::from(key), Arc::from(value)))
                                .collect(),
                        ),
                        label_deps: Arc::new(label_deps.into_iter().map(Arc::from).collect()),
                    },
                ),
                BzlmodGeneratedRepoConfig::ModuleExtensionRepo {
                    parent_canonical_repo_name,
                    parent_is_root,
                    extension_bzl_file,
                    extension_name,
                    repo_name,
                    extension_usages_json,
                } => BzlmodGeneratedCellGenerator::ModuleExtensionRepo(
                    BzlmodModuleExtensionRepoSetup {
                        parent_canonical_repo_name: Arc::from(parent_canonical_repo_name),
                        parent_is_root,
                        extension_bzl_file: Arc::from(extension_bzl_file),
                        extension_name: Arc::from(extension_name),
                        repo_name: Arc::from(repo_name),
                        extension_usages_json: Arc::from(extension_usages_json),
                    },
                ),
            };
            Ok(ExternalCellOrigin::BzlmodGenerated(
                BzlmodGeneratedCellSetup {
                    canonical_repo_name: get_config(section, "canonical_repo_name")?.into(),
                    generator,
                },
            ))
        } else {
            Err(ExternalCellOriginParseError::Unknown(value.to_owned()).into())
        }
    }
}

async fn config_file_exists(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
    file: &str,
) -> buck2_error::Result<bool> {
    let file = ForwardRelativePath::new(file)?;
    let path = ConfigPath::Project(cell_path.as_project_relative_path().join(file));
    Ok(file_ops.read_file_lines_if_exists(&path).await?.is_some())
}

async fn should_apply_bazel_compat_defaults(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> buck2_error::Result<bool> {
    if config_file_exists(cell_path, file_ops, PRIMARY_BUCKCONFIG).await? {
        return Ok(false);
    }

    for marker in BAZEL_PROJECT_ROOT_MARKERS {
        if config_file_exists(cell_path, file_ops, marker).await? {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn get_bazel_module_resolution_for_external_data(
    file_ops: &mut dyn ConfigParserFileOps,
    external_data: &ExternalBuckconfigData,
) -> buck2_error::Result<Arc<BazelModuleCellAliases>> {
    if let Some(module_aliases) = &external_data.bzlmod_module_aliases {
        return Ok(module_aliases.clone());
    }

    let root_path = CellRootPathBuf::new(ProjectRelativePath::empty().to_owned());
    Ok(Arc::new(
        get_bazel_module_resolution(
            &root_path,
            file_ops,
            external_data.bzlmod_module_extension_results_complete,
            &external_data.bzlmod_module_extension_results,
        )
        .await?,
    ))
}

#[derive(Default, Clone, PartialEq, Eq, Allocative, Pagable)]
struct BazelModuleCellAliases {
    root_aliases: Vec<BazelCompatCellAlias>,
    cell_aliases: BTreeMap<String, Vec<BazelCompatCellAlias>>,
    external_modules: Vec<BazelCompatExternalModule>,
    registered_toolchains: Vec<String>,
    module_extension_evaluation_requests: Vec<BzlmodModuleExtensionEvaluationRequest>,
}

impl BazelModuleCellAliases {
    fn aliases_for_cell(&self, cell_name: &str) -> &[BazelCompatCellAlias] {
        if cell_name == "root" {
            &self.root_aliases
        } else {
            self.cell_aliases
                .get(cell_name)
                .map(Vec::as_slice)
                .unwrap_or(&[])
        }
    }

    fn normalize(&mut self) {
        self.root_aliases.sort();
        self.root_aliases.dedup();
        for aliases in self.cell_aliases.values_mut() {
            aliases.sort();
            aliases.dedup();
        }
        self.registered_toolchains.sort();
        self.registered_toolchains.dedup();
        self.external_modules
            .sort_by(|a, b| a.cell_name().cmp(b.cell_name()));
        self.external_modules
            .dedup_by(|a, b| a.cell_name() == b.cell_name());
        let mut seen_module_extension_requests = BTreeSet::new();
        self.module_extension_evaluation_requests.retain(|request| {
            seen_module_extension_requests.insert((
                request.extension_bzl_cell.to_string(),
                request.extension_bzl_path.to_string(),
                request.extension_name.to_string(),
            ))
        });
    }

    fn register_for_starlark_label_resolution(&self) {
        register_bzlmod_cell_aliases(
            "root",
            self.root_aliases
                .iter()
                .map(|alias| (alias.alias.clone(), alias.cell_name.clone())),
        );
        for (cell_name, aliases) in &self.cell_aliases {
            register_bzlmod_cell_aliases(
                cell_name,
                aliases
                    .iter()
                    .map(|alias| (alias.alias.clone(), alias.cell_name.clone())),
            );
        }
    }
}

#[derive(Clone, Debug)]
struct BazelDep {
    name: String,
    version: String,
    apparent_name: Option<String>,
}

#[derive(Clone, Debug)]
struct DiscoveredBcrModule {
    dep: BazelDep,
    source_json: BcrSourceJson,
    module_aliases: Vec<String>,
    use_repo_aliases: Vec<String>,
    extension_usages: Vec<BzlmodExtensionUsage>,
    use_repo_rule_invocations: Vec<BzlmodUseRepoRuleInvocation>,
    constants: Vec<(String, String)>,
    registered_toolchains: Vec<String>,
    deps: Vec<BazelDep>,
}

type DiscoveredBcrModules = BTreeMap<(String, String), DiscoveredBcrModule>;

static BCR_DISCOVERY_CACHE: LazyLock<Mutex<BTreeMap<BcrDiscoveryCacheKey, DiscoveredBcrModules>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct BcrDiscoveryCacheKey {
    root_deps: Vec<(String, String)>,
    archive_overrides: Vec<BcrDiscoveryArchiveOverrideKey>,
    single_version_overrides: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct BcrDiscoveryArchiveOverrideKey {
    module_name: String,
    url: String,
    integrity: String,
    strip_prefix: Option<String>,
    archive_type: Option<String>,
    patches: Vec<String>,
    patch_strip: Option<u32>,
}

fn bcr_discovery_cache_key(
    root_deps: &[BazelDep],
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, String>,
) -> BcrDiscoveryCacheKey {
    let mut root_deps = root_deps
        .iter()
        .map(|dep| (dep.name.clone(), dep.version.clone()))
        .collect::<Vec<_>>();
    root_deps.sort();
    root_deps.dedup();

    BcrDiscoveryCacheKey {
        root_deps,
        archive_overrides: archive_overrides
            .values()
            .map(|archive_override| BcrDiscoveryArchiveOverrideKey {
                module_name: archive_override.module_name.clone(),
                url: archive_override.url.clone(),
                integrity: archive_override.integrity.clone(),
                strip_prefix: archive_override.strip_prefix.clone(),
                archive_type: archive_override.archive_type.clone(),
                patches: archive_override.patches.clone(),
                patch_strip: archive_override.patch_strip,
            })
            .collect(),
        single_version_overrides: single_version_overrides
            .iter()
            .map(|(name, version)| (name.clone(), version.clone()))
            .collect(),
    }
}

#[derive(Clone, Debug)]
struct RootBzlmodModule {
    name: String,
    version: String,
    canonical_repo_name: String,
    constants: Vec<(String, String)>,
    extension_usages: Vec<BzlmodExtensionUsage>,
    use_repo_rule_invocations: Vec<BzlmodUseRepoRuleInvocation>,
}

#[derive(Clone, Debug)]
struct BzlmodUseRepoImport {
    alias: String,
    repo_name: String,
}

#[derive(Clone, Debug)]
struct BzlmodExtensionUsage {
    proxy_name: String,
    extension_bzl_file: String,
    extension_name: String,
    dev_dependency: bool,
    imports: Vec<BzlmodUseRepoImport>,
    tags: Vec<BzlmodExtensionTag>,
}

#[derive(Clone, Debug)]
struct BzlmodExtensionTag {
    tag_name: String,
    bindings: Vec<(String, String)>,
    kwargs: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
struct BzlmodUseRepoRuleInvocation {
    rule_bzl_file: String,
    rule_name: String,
    repo_name: String,
    attrs: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BzlmodExtensionId {
    bzl_cell_name: String,
    bzl_path: String,
    extension_name: String,
}

#[derive(Clone, Debug)]
struct BzlmodResolvedExtension {
    id: BzlmodExtensionId,
    unique_name: String,
}

struct BcrResolution {
    external_modules: Vec<BazelCompatExternalModule>,
    root_aliases: Vec<BazelCompatCellAlias>,
    cell_aliases: BTreeMap<String, Vec<BazelCompatCellAlias>>,
    registered_toolchains: Vec<String>,
    module_extension_evaluation_requests: Vec<BzlmodModuleExtensionEvaluationRequest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BzlmodPatchConfig {
    url: String,
    integrity: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BzlmodGeneratedRepoConfig {
    BazelFeaturesGlobals {
        parent_canonical_repo_name: String,
        bazel_version: String,
    },
    BazelFeaturesVersion {
        bazel_version: String,
    },
    HostPlatform {},
    LocalConfigPlatform {},
    CcAutoconfToolchains {
        parent_canonical_repo_name: String,
    },
    CcAutoconf {},
    ShellConfig {},
    HttpArchive {
        repo_name: String,
        url: String,
        sha256: String,
        strip_prefix: Option<String>,
        archive_type: Option<String>,
    },
    JavaLocalJdk {},
    PythonHub {},
    RepositoryRule {
        files: Vec<BzlmodRepositoryRuleFileConfig>,
    },
    RepositoryRuleInvocation {
        repo_name: String,
        rule_bzl_cell: String,
        rule_bzl_path: String,
        rule_bzl_build_file_cell: String,
        rule_name: String,
        attrs: Vec<(String, String)>,
        #[serde(default)]
        label_deps: Vec<String>,
    },
    ModuleExtensionRepo {
        parent_canonical_repo_name: String,
        #[serde(default)]
        parent_is_root: bool,
        extension_bzl_file: String,
        extension_name: String,
        repo_name: String,
        extension_usages_json: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BzlmodRepositoryRuleFileConfig {
    path: String,
    content: String,
    executable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BzlmodModuleExtensionEvaluationConfig {
    modules: Vec<BzlmodModuleExtensionModuleConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BzlmodModuleExtensionModuleConfig {
    name: String,
    version: String,
    canonical_repo_name: String,
    is_root: bool,
    constants: Vec<(String, String)>,
    tags: Vec<BzlmodModuleExtensionTagConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BzlmodModuleExtensionTagConfig {
    tag_name: String,
    dev_dependency: bool,
    bindings: Vec<(String, String)>,
    kwargs: Vec<(String, String)>,
}

#[derive(Clone, Debug, Deserialize)]
struct BcrSourceJson {
    url: String,
    integrity: String,
    strip_prefix: Option<String>,
    archive_type: Option<String>,
    patches: Option<BTreeMap<String, String>>,
    patch_strip: Option<u32>,
}

#[derive(Clone, Debug)]
struct BzlmodArchiveOverride {
    module_name: String,
    url: String,
    integrity: String,
    strip_prefix: Option<String>,
    archive_type: Option<String>,
    patches: Vec<String>,
    patch_strip: Option<u32>,
}

async fn get_bazel_module_resolution(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
    bzlmod_module_extension_results_complete: bool,
    bzlmod_module_extension_results: &[BzlmodEvaluatedModuleExtension],
) -> buck2_error::Result<BazelModuleCellAliases> {
    let mut aliases = BazelModuleCellAliases::default();
    let mut root_deps = Vec::new();
    let mut archive_overrides = BTreeMap::new();
    let mut root_module_lines = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stack = vec!["MODULE.bazel".to_owned()];

    while let Some(module_file) = stack.pop() {
        if !seen.insert(module_file.clone()) {
            continue;
        }

        let file = ForwardRelativePath::new(&module_file)?;
        let path = ConfigPath::Project(cell_path.as_project_relative_path().join(file));
        let Some(lines) = file_ops.read_file_lines_if_exists(&path).await? else {
            continue;
        };
        root_module_lines.extend(lines.iter().cloned());

        for call in collect_bzl_calls(&lines, "module(") {
            for arg in ["name", "repo_name"] {
                if bzl_arg_is_none(&call, arg) {
                    continue;
                }
                if let Some(alias) = bzl_string_arg(&call, arg) {
                    aliases.root_aliases.push(BazelCompatCellAlias {
                        alias,
                        cell_name: "root".to_owned(),
                    });
                }
            }
        }

        for call in collect_bzl_calls(&lines, "bazel_dep(") {
            let Some(name) = bzl_string_arg(&call, "name") else {
                continue;
            };
            let version = bzl_string_arg(&call, "version").unwrap_or_default();
            let apparent_name = bzl_repo_name_arg(&call, &name);
            root_deps.push(BazelDep {
                name,
                version,
                apparent_name,
            });
        }

        for call in collect_bzl_calls(&lines, "archive_override(") {
            let archive_override = bzlmod_archive_override_from_call(&call)?;
            archive_overrides.insert(archive_override.module_name.clone(), archive_override);
        }

        for call in collect_bzl_calls(&lines, "git_override(") {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "git_override is not implemented in Buck2 bzlmod resolution yet: {}",
                call
            ));
        }

        for call in collect_bzl_calls(&lines, "local_path_override(") {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "local_path_override is not implemented in Buck2 bzlmod resolution yet: {}",
                call
            ));
        }

        aliases
            .registered_toolchains
            .extend(bzlmod_registered_toolchains_from_lines(&lines, false));

        for call in collect_bzl_calls(&lines, "include(") {
            if let Some(label) = bzl_first_string_arg(&call) {
                if let Some(include_file) = module_include_to_path(&module_file, &label) {
                    stack.push(include_file);
                }
            }
        }
    }

    let single_version_overrides = bzlmod_single_version_overrides_from_lines(&root_module_lines);
    for dep in &mut root_deps {
        if archive_overrides.contains_key(&dep.name) {
            dep.version.clear();
        } else if let Some(version) = single_version_overrides.get(&dep.name) {
            dep.version = version.clone();
        }
    }

    let root_module = bzlmod_root_module_from_lines(&root_module_lines)?;
    let bcr_resolution = resolve_bcr_modules(
        root_deps,
        root_module,
        archive_overrides,
        single_version_overrides,
        bzlmod_module_extension_results_complete,
        bzlmod_module_extension_results,
    )
    .await?;
    aliases.external_modules = bcr_resolution.external_modules;
    aliases.root_aliases.extend(bcr_resolution.root_aliases);
    aliases.cell_aliases = bcr_resolution.cell_aliases;
    aliases
        .registered_toolchains
        .extend(bcr_resolution.registered_toolchains);
    aliases.module_extension_evaluation_requests =
        bcr_resolution.module_extension_evaluation_requests;
    aliases.normalize();
    aliases.register_for_starlark_label_resolution();
    Ok(aliases)
}

async fn bzlmod_http_client() -> buck2_error::Result<HttpClient> {
    let mut builder = HttpClientBuilder::oss().await?;
    builder
        .with_max_redirects(10)
        .with_connect_timeout(Some(Duration::from_secs(60)))
        .with_read_timeout(Some(Duration::from_secs(60)))
        .with_write_timeout(Some(Duration::from_secs(60)))
        .with_max_concurrent_requests(Some(8));
    Ok(builder.build())
}

async fn resolve_bcr_modules(
    root_deps: Vec<BazelDep>,
    root_module: RootBzlmodModule,
    archive_overrides: BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: BTreeMap<String, String>,
    bzlmod_module_extension_results_complete: bool,
    bzlmod_module_extension_results: &[BzlmodEvaluatedModuleExtension],
) -> buck2_error::Result<BcrResolution> {
    let bzlmod_module_extension_results = bzlmod_module_extension_results.to_owned();
    std::thread::Builder::new()
        .name("buck2-bzlmod-resolver".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .buck_error_context("Error creating Tokio runtime for bzlmod resolution")?;
            runtime.block_on(async move {
                let client = bzlmod_http_client().await?;
                resolve_bcr_modules_with_client(
                    root_deps,
                    root_module,
                    archive_overrides,
                    single_version_overrides,
                    &client,
                    bzlmod_module_extension_results_complete,
                    &bzlmod_module_extension_results,
                )
                .await
            })
        })
        .buck_error_context("Error spawning bzlmod resolver thread")?
        .join()
        .map_err(|_| {
            buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "bzlmod resolver thread panicked"
            )
        })?
}

async fn resolve_bcr_modules_with_client(
    root_deps: Vec<BazelDep>,
    root_module: RootBzlmodModule,
    archive_overrides: BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: BTreeMap<String, String>,
    client: &HttpClient,
    bzlmod_module_extension_results_complete: bool,
    bzlmod_module_extension_results: &[BzlmodEvaluatedModuleExtension],
) -> buck2_error::Result<BcrResolution> {
    let discovered = discover_bcr_modules_with_cache(
        &root_deps,
        &archive_overrides,
        &single_version_overrides,
        client,
    )
    .await?;
    resolve_bcr_modules_from_discovered(
        root_deps,
        root_module,
        discovered,
        bzlmod_module_extension_results_complete,
        bzlmod_module_extension_results,
    )
}

async fn discover_bcr_modules_with_cache(
    root_deps: &[BazelDep],
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, String>,
    client: &HttpClient,
) -> buck2_error::Result<DiscoveredBcrModules> {
    let cache_key = bcr_discovery_cache_key(root_deps, archive_overrides, single_version_overrides);
    if let Some(discovered) = BCR_DISCOVERY_CACHE
        .lock()
        .expect("BCR discovery cache lock poisoned")
        .get(&cache_key)
        .cloned()
    {
        return Ok(discovered);
    }

    let discovered = discover_bcr_modules_with_client(
        root_deps,
        archive_overrides,
        single_version_overrides,
        client,
    )
    .await?;
    BCR_DISCOVERY_CACHE
        .lock()
        .expect("BCR discovery cache lock poisoned")
        .insert(cache_key, discovered.clone());
    Ok(discovered)
}

async fn discover_bcr_modules_with_client(
    root_deps: &[BazelDep],
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, String>,
    client: &HttpClient,
) -> buck2_error::Result<DiscoveredBcrModules> {
    let registry = "https://bcr.bazel.build";
    let mut discovered = DiscoveredBcrModules::new();
    let mut scheduled = BTreeSet::<(String, String)>::new();
    let mut pending = FuturesUnordered::<BcrModuleFetch>::new();

    for dep in root_deps {
        schedule_bcr_module_fetch(
            registry,
            client,
            dep.clone(),
            &archive_overrides,
            &single_version_overrides,
            &mut scheduled,
            &mut pending,
        );
    }

    while let Some(module) = pending.next().await {
        let module = module?;
        let key = (module.dep.name.clone(), module.dep.version.clone());
        for child in &module.deps {
            schedule_bcr_module_fetch(
                registry,
                client,
                child.clone(),
                &archive_overrides,
                &single_version_overrides,
                &mut scheduled,
                &mut pending,
            );
        }

        discovered.insert(key, module);
    }
    Ok(discovered)
}

fn resolve_bcr_modules_from_discovered(
    root_deps: Vec<BazelDep>,
    root_module: RootBzlmodModule,
    discovered: DiscoveredBcrModules,
    bzlmod_module_extension_results_complete: bool,
    bzlmod_module_extension_results: &[BzlmodEvaluatedModuleExtension],
) -> buck2_error::Result<BcrResolution> {
    let registry = "https://bcr.bazel.build";
    let mut selected_versions = BTreeMap::<String, String>::new();
    for (name, version) in discovered.keys() {
        match selected_versions.get(name) {
            Some(existing)
                if bzlmod_version_cmp(version, existing).with_buck_error_context(|| {
                    format!("Invalid version for module `{name}`")
                })? != Ordering::Greater => {}
            _ => {
                selected_versions.insert(name.clone(), version.clone());
            }
        }
    }

    let mut selected_keys = BTreeSet::<(String, String)>::new();
    let mut visit = VecDeque::new();
    for dep in &root_deps {
        if let Some(version) = selected_versions.get(&dep.name) {
            visit.push_back((dep.name.clone(), version.clone()));
        }
    }
    while let Some(key) = visit.pop_front() {
        if !selected_keys.insert(key.clone()) {
            continue;
        }
        let Some(module) = discovered.get(&key) else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "selected bzlmod module `{}@{}` was not discovered",
                key.0,
                key.1
            ));
        };
        for dep in &module.deps {
            if let Some(version) = selected_versions.get(&dep.name) {
                visit.push_back((dep.name.clone(), version.clone()));
            }
        }
    }
    let selected_keys_in_dependency_order = bzlmod_selected_keys_dependency_first(
        &discovered,
        &root_deps,
        &selected_versions,
        &selected_keys,
    );
    let canonical_repo_names_by_key = bzlmod_canonical_repo_names_by_key(&selected_keys);

    let mut root_aliases_by_key = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let mut cell_aliases_by_cell = BTreeMap::<String, BTreeMap<String, String>>::new();
    for dep in &root_deps {
        add_bzlmod_dep_alias(dep, &selected_versions, &mut root_aliases_by_key);
        add_bzlmod_dep_cell_alias(
            "root",
            dep,
            &selected_versions,
            &canonical_repo_names_by_key,
            &mut cell_aliases_by_cell,
        )?;
    }
    for key in &selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            &canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let cell_name = bzlmod_cell_name(&canonical_repo_name);
        add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            &cell_name,
            &canonical_repo_name,
            &cell_name,
        );
        if module.dep.name == "platforms" {
            root_aliases_by_key
                .entry(key.clone())
                .or_default()
                .insert("platforms".to_owned());
            add_bzlmod_cell_alias(&mut cell_aliases_by_cell, "root", "platforms", &cell_name);
        }
        for alias in &module.module_aliases {
            add_bzlmod_cell_alias(&mut cell_aliases_by_cell, &cell_name, alias, &cell_name);
            add_bzlmod_cell_alias(&mut cell_aliases_by_cell, "bazel_tools", alias, &cell_name);
        }
        for dep in &module.deps {
            add_bzlmod_dep_cell_alias(
                &cell_name,
                dep,
                &selected_versions,
                &canonical_repo_names_by_key,
                &mut cell_aliases_by_cell,
            )?;
        }
    }

    let selected_keys_for_generated = selected_keys.clone();
    let mut canonical_repo_names_by_cell = BTreeMap::<String, String>::new();
    canonical_repo_names_by_cell.insert("bazel_tools".to_owned(), "bazel_tools".to_owned());
    canonical_repo_names_by_cell.insert("root".to_owned(), root_module.canonical_repo_name.clone());
    for key in &selected_keys_for_generated {
        let canonical_repo_name = canonical_repo_names_by_key
            .get(key)
            .expect("selected key should have canonical repo name")
            .clone();
        canonical_repo_names_by_cell
            .insert(bzlmod_cell_name(&canonical_repo_name), canonical_repo_name);
    }

    let mut resolved = BTreeMap::<String, BazelCompatExternalModule>::new();
    for key in selected_keys {
        let Some(module) = discovered.get(&key) else {
            continue;
        };
        let mut aliases = root_aliases_by_key
            .remove(&key)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        aliases.sort();
        aliases.dedup();

        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            &canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let patch_configs = bzlmod_patch_configs(registry, &module.dep, &module.source_json);
        let patches_json = serde_json::to_string(&patch_configs)
            .buck_error_context("Error serializing bzlmod patch configuration")?;
        let cell_name = bzlmod_cell_name(&canonical_repo_name);
        resolved.insert(
            cell_name.clone(),
            BazelCompatExternalModule::Registry(BazelCompatRegistryModule {
                cell_name,
                aliases,
                module_name: module.dep.name.clone(),
                version: module.dep.version.clone(),
                canonical_repo_name,
                url: module.source_json.url.clone(),
                integrity: module.source_json.integrity.clone(),
                strip_prefix: module.source_json.strip_prefix.clone(),
                archive_type: module.source_json.archive_type.clone(),
                patches_json,
                patch_strip: module.source_json.patch_strip.unwrap_or(0),
            }),
        );
    }

    let mut resolved = resolved.into_values().collect::<Vec<_>>();
    let generated_resolution = resolve_generated_bzlmod_repos(
        &root_module,
        &discovered,
        &selected_keys_for_generated,
        &selected_keys_in_dependency_order,
        &canonical_repo_names_by_key,
        &mut cell_aliases_by_cell,
        &canonical_repo_names_by_cell,
        bzlmod_module_extension_results_complete,
        bzlmod_module_extension_results,
    )?;
    resolved.extend(generated_resolution.external_modules);
    let registered_toolchains = resolve_bzlmod_registered_toolchains(
        &discovered,
        &selected_keys_for_generated,
        &canonical_repo_names_by_key,
        &cell_aliases_by_cell,
        bzlmod_module_extension_results,
    )?;
    Ok(BcrResolution {
        external_modules: resolved,
        root_aliases: cell_aliases_by_cell
            .remove("root")
            .map(bzlmod_cell_alias_map_to_vec)
            .unwrap_or_default(),
        cell_aliases: cell_aliases_by_cell
            .into_iter()
            .map(|(cell, aliases)| (cell, bzlmod_cell_alias_map_to_vec(aliases)))
            .collect(),
        registered_toolchains,
        module_extension_evaluation_requests: generated_resolution
            .module_extension_evaluation_requests,
    })
}

struct GeneratedBzlmodReposResolution {
    external_modules: Vec<BazelCompatExternalModule>,
    module_extension_evaluation_requests: Vec<BzlmodModuleExtensionEvaluationRequest>,
}

fn resolve_generated_bzlmod_repos(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    selected_keys_in_dependency_order: &[(String, String)],
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &mut BTreeMap<String, BTreeMap<String, String>>,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
    bzlmod_module_extension_results_complete: bool,
    bzlmod_module_extension_results: &[BzlmodEvaluatedModuleExtension],
) -> buck2_error::Result<GeneratedBzlmodReposResolution> {
    let mut generated = Vec::new();
    let mut module_extension_evaluation_requests = Vec::new();
    let mut generated_repo_declaring_cells = Vec::new();
    let mut extension_generated_repo_groups = BTreeMap::<String, Vec<(String, String)>>::new();
    let extension_unique_names = bzlmod_extension_unique_names(
        root_module,
        discovered,
        selected_keys,
        canonical_repo_names_by_key,
        cell_aliases_by_cell,
        canonical_repo_names_by_cell,
    )?;
    resolve_bzlmod_use_repo_rule_generated_repos(
        &root_module.use_repo_rule_invocations,
        &root_module.canonical_repo_name,
        "root",
        true,
        cell_aliases_by_cell,
        &mut generated,
        &mut generated_repo_declaring_cells,
    )?;
    let mut needs_local_config_platform = false;
    for key in selected_keys_in_dependency_order {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let parent_canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let parent_cell_name = bzlmod_cell_name(&parent_canonical_repo_name);
        resolve_bzlmod_use_repo_rule_generated_repos(
            &module.use_repo_rule_invocations,
            &parent_canonical_repo_name,
            &parent_cell_name,
            false,
            cell_aliases_by_cell,
            &mut generated,
            &mut generated_repo_declaring_cells,
        )?;
        if module.dep.name == "rules_cc" {
            for alias in &module.use_repo_aliases {
                let generator = match alias.as_str() {
                    "local_config_cc_toolchains" => {
                        needs_local_config_platform = true;
                        Some(BzlmodGeneratedRepoConfig::CcAutoconfToolchains {
                            parent_canonical_repo_name: parent_canonical_repo_name.clone(),
                        })
                    }
                    "local_config_cc" => Some(BzlmodGeneratedRepoConfig::CcAutoconf {}),
                    _ => None,
                };
                let Some(generator) = generator else {
                    continue;
                };
                let canonical_repo_name =
                    format!("{parent_canonical_repo_name}+cc_configure+{alias}");
                let generator_json = serde_json::to_string(&generator).buck_error_context(
                    "Error serializing generated rules_cc configure repo configuration",
                )?;
                add_generated_bzlmod_repo(
                    &mut generated,
                    &mut generated_repo_declaring_cells,
                    cell_aliases_by_cell,
                    &parent_cell_name,
                    alias,
                    &canonical_repo_name,
                    generator_json,
                );
            }
        }

        if module.dep.name == "rules_shell" {
            for alias in &module.use_repo_aliases {
                if alias != "local_config_shell" {
                    continue;
                }
                let canonical_repo_name =
                    format!("{parent_canonical_repo_name}+sh_configure+{alias}");
                let generator_json =
                    serde_json::to_string(&BzlmodGeneratedRepoConfig::ShellConfig {})
                        .buck_error_context(
                            "Error serializing generated rules_shell configure repo configuration",
                        )?;
                add_generated_bzlmod_repo(
                    &mut generated,
                    &mut generated_repo_declaring_cells,
                    cell_aliases_by_cell,
                    &parent_cell_name,
                    alias,
                    &canonical_repo_name,
                    generator_json,
                );
            }
        }

        if module.dep.name == "rules_java" {
            for alias in &module.use_repo_aliases {
                let generator = if alias == "local_jdk" {
                    needs_local_config_platform = true;
                    Some(BzlmodGeneratedRepoConfig::JavaLocalJdk {})
                } else {
                    rules_java_remote_tools_archive(alias).map(|(repo_name, url, sha256)| {
                        BzlmodGeneratedRepoConfig::HttpArchive {
                            repo_name: repo_name.to_owned(),
                            url: url.to_owned(),
                            sha256: sha256.to_owned(),
                            strip_prefix: None,
                            archive_type: Some("zip".to_owned()),
                        }
                    })
                };
                let Some(generator) = generator else {
                    continue;
                };
                let canonical_repo_name =
                    format!("{parent_canonical_repo_name}+toolchains+{alias}");
                let generator_json = serde_json::to_string(&generator).buck_error_context(
                    "Error serializing generated rules_java toolchains repo configuration",
                )?;
                add_generated_bzlmod_repo(
                    &mut generated,
                    &mut generated_repo_declaring_cells,
                    cell_aliases_by_cell,
                    &parent_cell_name,
                    alias,
                    &canonical_repo_name,
                    generator_json,
                );
            }
        }

        if module.dep.name == "platforms" {
            for import in
                bzlmod_extension_imports_from_usages(&module.extension_usages, "host_platform")
            {
                if import.repo_name != "host_platform" {
                    continue;
                }
                let canonical_repo_name = format!(
                    "{}+host_platform+{}",
                    parent_canonical_repo_name, import.repo_name
                );
                let generator_json =
                    serde_json::to_string(&BzlmodGeneratedRepoConfig::HostPlatform {})
                        .buck_error_context(
                            "Error serializing generated host_platform repo configuration",
                        )?;
                add_generated_bzlmod_repo(
                    &mut generated,
                    &mut generated_repo_declaring_cells,
                    cell_aliases_by_cell,
                    &parent_cell_name,
                    &import.alias,
                    &canonical_repo_name,
                    generator_json,
                );
            }
        }

        if module.dep.name == "bazel_features" {
            for import in
                bzlmod_extension_imports_from_usages(&module.extension_usages, "version_extension")
            {
                let generator = match import.repo_name.as_str() {
                    "bazel_features_globals" => {
                        Some(BzlmodGeneratedRepoConfig::BazelFeaturesGlobals {
                            parent_canonical_repo_name: parent_canonical_repo_name.clone(),
                            bazel_version: BZLMOD_BAZEL_COMPAT_VERSION.to_owned(),
                        })
                    }
                    "bazel_features_version" => {
                        Some(BzlmodGeneratedRepoConfig::BazelFeaturesVersion {
                            bazel_version: BZLMOD_BAZEL_COMPAT_VERSION.to_owned(),
                        })
                    }
                    _ => None,
                };
                let Some(generator) = generator else {
                    continue;
                };
                let canonical_repo_name = format!(
                    "{}+version_extension+{}",
                    parent_canonical_repo_name, import.repo_name
                );
                let generator_json = serde_json::to_string(&generator).buck_error_context(
                    "Error serializing generated bazel_features repo configuration",
                )?;
                add_generated_bzlmod_repo(
                    &mut generated,
                    &mut generated_repo_declaring_cells,
                    cell_aliases_by_cell,
                    &parent_cell_name,
                    &import.alias,
                    &canonical_repo_name,
                    generator_json,
                );
            }
        }

        for usage in &module.extension_usages {
            resolve_bzlmod_extension_usage_generated_repos(
                usage,
                &parent_canonical_repo_name,
                &parent_cell_name,
                false,
                root_module,
                discovered,
                selected_keys,
                canonical_repo_names_by_key,
                cell_aliases_by_cell,
                &extension_unique_names,
                bzlmod_module_extension_results_complete,
                bzlmod_module_extension_results,
                &mut generated,
                &mut module_extension_evaluation_requests,
                &mut generated_repo_declaring_cells,
                &mut extension_generated_repo_groups,
            )?;
        }
    }

    for usage in &root_module.extension_usages {
        resolve_bzlmod_extension_usage_generated_repos(
            usage,
            &root_module.canonical_repo_name,
            "root",
            true,
            root_module,
            discovered,
            selected_keys,
            canonical_repo_names_by_key,
            cell_aliases_by_cell,
            &extension_unique_names,
            bzlmod_module_extension_results_complete,
            bzlmod_module_extension_results,
            &mut generated,
            &mut module_extension_evaluation_requests,
            &mut generated_repo_declaring_cells,
            &mut extension_generated_repo_groups,
        )?;
    }

    if needs_local_config_platform {
        let canonical_repo_name = "local_config_platform".to_owned();
        let generator_json =
            serde_json::to_string(&BzlmodGeneratedRepoConfig::LocalConfigPlatform {})
                .buck_error_context(
                    "Error serializing generated local_config_platform repo configuration",
                )?;
        let cell_name = bzlmod_cell_name(&canonical_repo_name);
        let mut importing_cells = cell_aliases_by_cell.keys().cloned().collect::<Vec<_>>();
        importing_cells.push(cell_name.clone());
        importing_cells.sort();
        importing_cells.dedup();
        for parent_cell_name in &importing_cells {
            add_bzlmod_cell_alias(
                cell_aliases_by_cell,
                parent_cell_name,
                "local_config_platform",
                &cell_name,
            );
        }
        generated_repo_declaring_cells.push((cell_name.clone(), "root".to_owned()));
        generated.push(BazelCompatExternalModule::Generated(
            BazelCompatGeneratedModule {
                cell_name,
                aliases: Vec::new(),
                canonical_repo_name,
                generator_json,
            },
        ));
    }
    add_generated_bzlmod_repo_mappings(
        cell_aliases_by_cell,
        &generated_repo_declaring_cells,
        &extension_generated_repo_groups,
    );
    Ok(GeneratedBzlmodReposResolution {
        external_modules: generated,
        module_extension_evaluation_requests,
    })
}

fn resolve_bzlmod_extension_usage_generated_repos(
    usage: &BzlmodExtensionUsage,
    parent_canonical_repo_name: &str,
    parent_cell_name: &str,
    parent_is_root: bool,
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &mut BTreeMap<String, BTreeMap<String, String>>,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
    bzlmod_module_extension_results_complete: bool,
    bzlmod_module_extension_results: &[BzlmodEvaluatedModuleExtension],
    generated: &mut Vec<BazelCompatExternalModule>,
    module_extension_evaluation_requests: &mut Vec<BzlmodModuleExtensionEvaluationRequest>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    extension_generated_repo_groups: &mut BTreeMap<String, Vec<(String, String)>>,
) -> buck2_error::Result<()> {
    let resolved_extension = bzlmod_resolve_extension(
        parent_cell_name,
        usage,
        cell_aliases_by_cell,
        extension_unique_names,
    )?;
    let extension_usages_json = bzlmod_module_extension_evaluation_config_json(
        root_module,
        discovered,
        selected_keys,
        canonical_repo_names_by_key,
        cell_aliases_by_cell,
        &resolved_extension.id,
        extension_unique_names,
    )?;
    let extension_group_key = resolved_extension.unique_name.clone();
    let mut existing_generated_repos = extension_generated_repo_groups
        .get(&extension_group_key)
        .map(|generated_repos| generated_repos.iter().cloned().collect::<BTreeMap<_, _>>())
        .unwrap_or_default();

    let imports_needing_generic_repos = usage
        .imports
        .iter()
        .filter(|import| {
            bzlmod_cell_alias_target(cell_aliases_by_cell, parent_cell_name, &import.alias)
                .is_none()
        })
        .collect::<Vec<_>>();
    let mut static_repo_names = imports_needing_generic_repos
        .iter()
        .map(|import| import.repo_name.clone())
        .collect::<BTreeSet<_>>();
    static_repo_names.extend(bzlmod_extension_tag_repo_names(usage));

    let evaluated_extension = bzlmod_module_extension_results.iter().find(|result| {
        result.extension_bzl_cell.as_ref() == resolved_extension.id.bzl_cell_name.as_str()
            && result.extension_bzl_path.as_ref() == resolved_extension.id.bzl_path.as_str()
            && result.extension_name.as_ref() == usage.extension_name.as_str()
    });

    let mut generated_repo_names = if bzlmod_module_extension_results_complete {
        if static_repo_names.is_empty() && evaluated_extension.is_none() {
            return Ok(());
        }
        let Some(evaluated_extension) = evaluated_extension else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod module extension `{}`%`{}` for `{}` was not evaluated before cell graph finalization",
                usage.extension_bzl_file,
                usage.extension_name,
                parent_canonical_repo_name
            ));
        };
        evaluated_extension
            .repo_names
            .iter()
            .map(|repo_name| repo_name.to_string())
            .collect::<BTreeSet<_>>()
    } else {
        if static_repo_names.is_empty() {
            return Ok(());
        }
        if let Some(evaluated_extension) = evaluated_extension {
            static_repo_names.extend(
                evaluated_extension
                    .repo_names
                    .iter()
                    .map(|repo_name| repo_name.to_string()),
            );
        } else {
            module_extension_evaluation_requests.push(BzlmodModuleExtensionEvaluationRequest {
                parent_canonical_repo_name: Arc::from(parent_canonical_repo_name),
                parent_is_root,
                extension_bzl_file: Arc::from(usage.extension_bzl_file.clone()),
                extension_bzl_cell: Arc::from(resolved_extension.id.bzl_cell_name.clone()),
                extension_bzl_path: Arc::from(resolved_extension.id.bzl_path.clone()),
                extension_unique_name: Arc::from(resolved_extension.unique_name.clone()),
                extension_name: Arc::from(usage.extension_name.clone()),
                extension_usages_json: Arc::from(extension_usages_json.clone()),
            });
        }
        static_repo_names
    };

    for import in imports_needing_generic_repos {
        if bzlmod_module_extension_results_complete
            && !generated_repo_names.contains(&import.repo_name)
        {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod module extension `{}`%`{}` for `{}` did not emit imported repository `{}`",
                usage.extension_bzl_file,
                usage.extension_name,
                parent_canonical_repo_name,
                import.repo_name
            ));
        }

        if let Some(generated_cell_name) = existing_generated_repos.get(&import.repo_name) {
            add_bzlmod_cell_alias(
                cell_aliases_by_cell,
                parent_cell_name,
                &import.alias,
                generated_cell_name,
            );
            generated_repo_names.remove(&import.repo_name);
            continue;
        }

        let canonical_repo_name =
            bzlmod_extension_repo_canonical_repo_name(&resolved_extension, &import.repo_name);
        let generator_json = serde_json::to_string(&bzlmod_module_extension_repo_config(
            bzlmod_module_extension_results_complete,
            evaluated_extension,
            parent_canonical_repo_name,
            parent_is_root,
            usage,
            &import.repo_name,
            &extension_usages_json,
        )?)
        .buck_error_context("Error serializing generated module extension repo configuration")?;
        let generated_cell_name = add_generated_bzlmod_repo(
            generated,
            generated_repo_declaring_cells,
            cell_aliases_by_cell,
            parent_cell_name,
            &import.alias,
            &canonical_repo_name,
            generator_json,
        );
        extension_generated_repo_groups
            .entry(extension_group_key.clone())
            .or_default()
            .push((import.repo_name.clone(), generated_cell_name.clone()));
        existing_generated_repos.insert(import.repo_name.clone(), generated_cell_name);
        generated_repo_names.remove(&import.repo_name);
    }

    for repo_name in generated_repo_names {
        if existing_generated_repos.contains_key(&repo_name) {
            continue;
        }
        let canonical_repo_name =
            bzlmod_extension_repo_canonical_repo_name(&resolved_extension, &repo_name);
        let generator_json = serde_json::to_string(&bzlmod_module_extension_repo_config(
            bzlmod_module_extension_results_complete,
            evaluated_extension,
            parent_canonical_repo_name,
            parent_is_root,
            usage,
            &repo_name,
            &extension_usages_json,
        )?)
        .buck_error_context("Error serializing generated module extension repo configuration")?;
        let generated_cell_name = add_unimported_generated_bzlmod_repo(
            generated,
            generated_repo_declaring_cells,
            parent_cell_name,
            &canonical_repo_name,
            generator_json,
        );
        extension_generated_repo_groups
            .entry(extension_group_key.clone())
            .or_default()
            .push((repo_name.clone(), generated_cell_name.clone()));
        existing_generated_repos.insert(repo_name, generated_cell_name);
    }

    Ok(())
}

fn bzlmod_module_extension_evaluation_config_json(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
    extension_id: &BzlmodExtensionId,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
) -> buck2_error::Result<String> {
    let mut modules = Vec::new();
    let mut root_has_usage = false;
    let mut root_tags = Vec::new();
    for usage in &root_module.extension_usages {
        let resolved_extension =
            bzlmod_resolve_extension("root", usage, cell_aliases_by_cell, extension_unique_names)?;
        if &resolved_extension.id != extension_id {
            continue;
        }
        root_has_usage = true;
        root_tags.extend(usage.tags.iter().map(|tag| BzlmodModuleExtensionTagConfig {
            tag_name: tag.tag_name.clone(),
            dev_dependency: usage.dev_dependency,
            bindings: tag.bindings.clone(),
            kwargs: tag.kwargs.clone(),
        }));
    }
    if root_has_usage {
        modules.push(BzlmodModuleExtensionModuleConfig {
            name: root_module.name.clone(),
            version: root_module.version.clone(),
            canonical_repo_name: root_module.canonical_repo_name.clone(),
            is_root: true,
            constants: root_module.constants.clone(),
            tags: root_tags,
        });
    }

    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let module_cell_name = bzlmod_cell_name(&canonical_repo_name);
        let mut has_usage = false;
        let mut tags = Vec::new();
        for usage in &module.extension_usages {
            let resolved_extension = bzlmod_resolve_extension(
                &module_cell_name,
                usage,
                cell_aliases_by_cell,
                extension_unique_names,
            )?;
            if &resolved_extension.id != extension_id {
                continue;
            }
            has_usage = true;
            tags.extend(usage.tags.iter().map(|tag| BzlmodModuleExtensionTagConfig {
                tag_name: tag.tag_name.clone(),
                dev_dependency: usage.dev_dependency,
                bindings: tag.bindings.clone(),
                kwargs: tag.kwargs.clone(),
            }));
        }
        if !has_usage {
            continue;
        }
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        modules.push(BzlmodModuleExtensionModuleConfig {
            name: module.dep.name.clone(),
            version: module.dep.version.clone(),
            canonical_repo_name,
            is_root: false,
            constants: module.constants.clone(),
            tags,
        });
    }

    serde_json::to_string(&BzlmodModuleExtensionEvaluationConfig { modules })
        .buck_error_context("Error serializing module extension evaluation configuration")
}

fn bzlmod_module_extension_repo_config(
    bzlmod_module_extension_results_complete: bool,
    evaluated_extension: Option<&BzlmodEvaluatedModuleExtension>,
    parent_canonical_repo_name: &str,
    parent_is_root: bool,
    usage: &BzlmodExtensionUsage,
    repo_name: &str,
    extension_usages_json: &str,
) -> buck2_error::Result<BzlmodGeneratedRepoConfig> {
    if let Some(evaluated_extension) = evaluated_extension {
        if let Some(invocation) = evaluated_extension
            .repository_rules
            .iter()
            .find(|invocation| invocation.repo_name == repo_name)
        {
            return Ok(BzlmodGeneratedRepoConfig::RepositoryRuleInvocation {
                repo_name: invocation.repo_name.clone(),
                rule_bzl_cell: invocation.rule_bzl_cell.clone(),
                rule_bzl_path: invocation.rule_bzl_path.clone(),
                rule_bzl_build_file_cell: invocation.rule_bzl_build_file_cell.clone(),
                rule_name: invocation.rule_name.clone(),
                attrs: invocation.attrs.clone(),
                label_deps: invocation.label_deps.clone(),
            });
        }

        if bzlmod_module_extension_results_complete {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod module extension `{}`%`{}` for `{}` did not retain repository_rule invocation for emitted repository `{}`",
                usage.extension_bzl_file,
                usage.extension_name,
                parent_canonical_repo_name,
                repo_name
            ));
        }
    } else if bzlmod_module_extension_results_complete {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "bzlmod module extension `{}`%`{}` for `{}` was not evaluated before cell graph finalization",
            usage.extension_bzl_file,
            usage.extension_name,
            parent_canonical_repo_name
        ));
    }

    Ok(BzlmodGeneratedRepoConfig::ModuleExtensionRepo {
        parent_canonical_repo_name: parent_canonical_repo_name.to_owned(),
        parent_is_root,
        extension_bzl_file: usage.extension_bzl_file.clone(),
        extension_name: usage.extension_name.clone(),
        repo_name: repo_name.to_owned(),
        extension_usages_json: extension_usages_json.to_owned(),
    })
}

fn add_generated_bzlmod_repo(
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    cell_aliases_by_cell: &mut BTreeMap<String, BTreeMap<String, String>>,
    declaring_cell_name: &str,
    alias: &str,
    canonical_repo_name: &str,
    generator_json: String,
) -> String {
    let cell_name = bzlmod_cell_name(canonical_repo_name);
    add_bzlmod_cell_alias(cell_aliases_by_cell, declaring_cell_name, alias, &cell_name);
    add_unimported_generated_bzlmod_repo(
        generated,
        generated_repo_declaring_cells,
        declaring_cell_name,
        canonical_repo_name,
        generator_json,
    )
}

fn add_unimported_generated_bzlmod_repo(
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    declaring_cell_name: &str,
    canonical_repo_name: &str,
    generator_json: String,
) -> String {
    let cell_name = bzlmod_cell_name(canonical_repo_name);
    generated_repo_declaring_cells.push((cell_name.clone(), declaring_cell_name.to_owned()));
    generated.push(BazelCompatExternalModule::Generated(
        BazelCompatGeneratedModule {
            cell_name: cell_name.clone(),
            aliases: Vec::new(),
            canonical_repo_name: canonical_repo_name.to_owned(),
            generator_json,
        },
    ));
    cell_name
}

fn add_generated_bzlmod_repo_mappings(
    cell_aliases_by_cell: &mut BTreeMap<String, BTreeMap<String, String>>,
    generated_repo_declaring_cells: &[(String, String)],
    extension_generated_repo_groups: &BTreeMap<String, Vec<(String, String)>>,
) {
    let mut mapped_declaring_cells = BTreeSet::new();
    for (generated_cell_name, declaring_cell_name) in generated_repo_declaring_cells {
        if !mapped_declaring_cells
            .insert((generated_cell_name.clone(), declaring_cell_name.clone()))
        {
            continue;
        }
        let Some(declaring_aliases) = cell_aliases_by_cell.get(declaring_cell_name).cloned() else {
            continue;
        };
        cell_aliases_by_cell
            .entry(generated_cell_name.clone())
            .or_default()
            .extend(declaring_aliases);
    }

    for generated_repos in extension_generated_repo_groups.values() {
        let generated_repos = generated_repos.iter().cloned().collect::<BTreeMap<_, _>>();
        for generated_cell_name in generated_repos.values() {
            for (repo_name, target_cell_name) in &generated_repos {
                add_bzlmod_cell_alias(
                    cell_aliases_by_cell,
                    generated_cell_name,
                    repo_name,
                    target_cell_name,
                );
            }
        }
    }
}

fn resolve_bzlmod_use_repo_rule_generated_repos(
    invocations: &[BzlmodUseRepoRuleInvocation],
    parent_canonical_repo_name: &str,
    parent_cell_name: &str,
    parent_is_root: bool,
    cell_aliases_by_cell: &mut BTreeMap<String, BTreeMap<String, String>>,
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
) -> buck2_error::Result<()> {
    for invocation in invocations {
        let (rule_bzl_cell, rule_bzl_path) = bzlmod_resolve_extension_bzl_label(
            parent_cell_name,
            &invocation.rule_bzl_file,
            cell_aliases_by_cell,
        )?;
        let canonical_repo_name = bzlmod_use_repo_rule_canonical_repo_name(
            parent_canonical_repo_name,
            parent_is_root,
            &invocation.rule_name,
            &invocation.repo_name,
        );
        let label_deps = bzlmod_repository_rule_invocation_label_deps(
            parent_cell_name,
            &invocation.attrs,
            cell_aliases_by_cell,
        );
        let generator_json =
            serde_json::to_string(&BzlmodGeneratedRepoConfig::RepositoryRuleInvocation {
                repo_name: invocation.repo_name.clone(),
                rule_bzl_cell,
                rule_bzl_path,
                rule_bzl_build_file_cell: parent_cell_name.to_owned(),
                rule_name: invocation.rule_name.clone(),
                attrs: invocation.attrs.clone(),
                label_deps,
            })
            .buck_error_context("Error serializing use_repo_rule repository configuration")?;
        add_generated_bzlmod_repo(
            generated,
            generated_repo_declaring_cells,
            cell_aliases_by_cell,
            parent_cell_name,
            &invocation.repo_name,
            &canonical_repo_name,
            generator_json,
        );
    }
    Ok(())
}

fn bzlmod_use_repo_rule_canonical_repo_name(
    parent_canonical_repo_name: &str,
    parent_is_root: bool,
    rule_name: &str,
    repo_name: &str,
) -> String {
    let extension_unique_name = if parent_is_root {
        format!("+{rule_name}")
    } else {
        format!("{parent_canonical_repo_name}+{rule_name}")
    };
    bzlmod_extension_unique_repo_canonical_repo_name(&extension_unique_name, repo_name)
}

fn bzlmod_repository_rule_invocation_label_deps(
    current_cell_name: &str,
    attrs: &[(String, String)],
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
) -> Vec<String> {
    let mut label_deps = BTreeSet::new();
    for (_, expression) in attrs {
        let Some(label) = bzl_label_expression_value(expression) else {
            continue;
        };
        let Some(cell_name) =
            bzlmod_label_dep_cell_name(current_cell_name, &label, cell_aliases_by_cell)
        else {
            continue;
        };
        label_deps.insert(cell_name);
    }
    label_deps.into_iter().collect()
}

fn bzl_label_expression_value(expression: &str) -> Option<String> {
    let expression = expression.trim();
    let args = expression
        .strip_prefix("Label(")
        .and_then(|value| value.strip_suffix(')'))?;
    bzl_string_literal_value(args.trim())
}

fn bzlmod_label_dep_cell_name(
    current_cell_name: &str,
    label: &str,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
) -> Option<String> {
    if let Some(rest) = label.strip_prefix("@@") {
        let (canonical_repo_name, _) = rest.split_once("//")?;
        return Some(if canonical_repo_name == "bazel_tools" {
            "bazel_tools".to_owned()
        } else {
            bzlmod_cell_name(canonical_repo_name)
        });
    }
    if let Some(rest) = label.strip_prefix('@') {
        let (alias, _) = rest.split_once("//")?;
        if alias == "bazel_tools" {
            return Some("bazel_tools".to_owned());
        }
        return bzlmod_cell_alias_target(cell_aliases_by_cell, current_cell_name, alias)
            .map(str::to_owned);
    }
    if label.starts_with("//") || label.starts_with(':') {
        return Some(current_cell_name.to_owned());
    }
    None
}

fn bzlmod_extension_tag_repo_names(usage: &BzlmodExtensionUsage) -> Vec<String> {
    let mut repo_names = usage
        .tags
        .iter()
        .flat_map(|tag| tag.kwargs.iter())
        .filter_map(|(name, value)| {
            if name == "name" || name == "repo_name" {
                bzl_string_value(value.trim())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    repo_names.sort();
    repo_names.dedup();
    repo_names
}

fn bzlmod_extension_unique_names(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
) -> buck2_error::Result<BTreeMap<BzlmodExtensionId, String>> {
    let mut extension_ids = BTreeSet::new();
    for usage in &root_module.extension_usages {
        extension_ids.insert(bzlmod_resolve_extension_id(
            "root",
            usage,
            cell_aliases_by_cell,
        )?);
    }
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let module_cell_name = bzlmod_cell_name(&canonical_repo_name);
        for usage in &module.extension_usages {
            extension_ids.insert(bzlmod_resolve_extension_id(
                &module_cell_name,
                usage,
                cell_aliases_by_cell,
            )?);
        }
    }

    let mut used_names = BTreeSet::new();
    let mut unique_names = BTreeMap::new();
    for extension_id in extension_ids {
        let Some(extension_repo_name) =
            canonical_repo_names_by_cell.get(&extension_id.bzl_cell_name)
        else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod module extension `{}//{}%{}` resolves to unknown cell `{}`",
                extension_id.bzl_cell_name,
                extension_id.bzl_path,
                extension_id.extension_name,
                extension_id.bzl_cell_name
            ));
        };
        let mut attempt = 1;
        loop {
            let disambiguator = if attempt == 1 {
                String::new()
            } else {
                attempt.to_string()
            };
            let candidate = format!(
                "{}+{}{}",
                extension_repo_name, extension_id.extension_name, disambiguator
            );
            if used_names.insert(candidate.clone()) {
                unique_names.insert(extension_id, candidate);
                break;
            }
            attempt += 1;
        }
    }
    Ok(unique_names)
}

fn bzlmod_resolve_extension(
    current_cell_name: &str,
    usage: &BzlmodExtensionUsage,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
) -> buck2_error::Result<BzlmodResolvedExtension> {
    let id = bzlmod_resolve_extension_id(current_cell_name, usage, cell_aliases_by_cell)?;
    let Some(unique_name) = extension_unique_names.get(&id) else {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "bzlmod module extension `{}`%`{}` in cell `{}` was not assigned a unique name",
            usage.extension_bzl_file,
            usage.extension_name,
            current_cell_name
        ));
    };
    Ok(BzlmodResolvedExtension {
        id,
        unique_name: unique_name.clone(),
    })
}

fn bzlmod_resolve_extension_id(
    current_cell_name: &str,
    usage: &BzlmodExtensionUsage,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
) -> buck2_error::Result<BzlmodExtensionId> {
    let (bzl_cell_name, bzl_path) = bzlmod_resolve_extension_bzl_label(
        current_cell_name,
        &usage.extension_bzl_file,
        cell_aliases_by_cell,
    )?;
    Ok(BzlmodExtensionId {
        bzl_cell_name,
        bzl_path,
        extension_name: usage.extension_name.clone(),
    })
}

fn bzlmod_resolve_extension_bzl_label(
    current_cell_name: &str,
    label: &str,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
) -> buck2_error::Result<(String, String)> {
    if let Some(rest) = label.strip_prefix("@@") {
        let Some((canonical_repo_name, package_and_target)) = rest.split_once("//") else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod module extension label `{}` is not an absolute label",
                label
            ));
        };
        let cell_name = if canonical_repo_name == "bazel_tools" {
            "bazel_tools".to_owned()
        } else {
            bzlmod_cell_name(canonical_repo_name)
        };
        return Ok((
            cell_name,
            bzlmod_label_package_target_to_path(label, package_and_target)?,
        ));
    }
    if let Some(rest) = label.strip_prefix('@') {
        let Some((alias, package_and_target)) = rest.split_once("//") else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod module extension label `{}` is not an absolute label",
                label
            ));
        };
        let cell_name = if alias == "bazel_tools" {
            "bazel_tools"
        } else {
            bzlmod_cell_alias_target(cell_aliases_by_cell, current_cell_name, alias).ok_or_else(
                || {
                    buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "bzlmod module extension label `{}` in cell `{}` references unknown repo `{}`",
                        label,
                        current_cell_name,
                        alias
                    )
                },
            )?
        };
        return Ok((
            cell_name.to_owned(),
            bzlmod_label_package_target_to_path(label, package_and_target)?,
        ));
    }
    if let Some(package_and_target) = label.strip_prefix("//") {
        return Ok((
            current_cell_name.to_owned(),
            bzlmod_label_package_target_to_path(label, package_and_target)?,
        ));
    }
    if let Some(target) = label.strip_prefix(':') {
        return Ok((
            current_cell_name.to_owned(),
            bzlmod_label_package_target_to_path(label, &format!(":{target}"))?,
        ));
    }
    Err(buck2_error!(
        buck2_error::ErrorTag::Input,
        "bzlmod module extension label `{}` is not an absolute or module-root-relative label",
        label
    ))
}

fn bzlmod_label_package_target_to_path(
    label: &str,
    package_and_target: &str,
) -> buck2_error::Result<String> {
    let (package, target) = match package_and_target.split_once(':') {
        Some((package, target)) => (package, target),
        None => {
            let target = package_and_target
                .rsplit('/')
                .next()
                .unwrap_or(package_and_target);
            (package_and_target, target)
        }
    };
    if target.is_empty() {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "bzlmod module extension label `{}` has an empty target name",
            label
        ));
    }
    if package.is_empty() {
        Ok(target.to_owned())
    } else {
        Ok(format!("{package}/{target}"))
    }
}

fn bzlmod_extension_repo_canonical_repo_name(
    extension: &BzlmodResolvedExtension,
    repo_name: &str,
) -> String {
    bzlmod_extension_unique_repo_canonical_repo_name(&extension.unique_name, repo_name)
}

fn bzlmod_extension_unique_repo_canonical_repo_name(
    extension_unique_name: &str,
    repo_name: &str,
) -> String {
    format!("{extension_unique_name}+{repo_name}")
}

fn resolve_bzlmod_registered_toolchains(
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
    bzlmod_module_extension_results: &[BzlmodEvaluatedModuleExtension],
) -> buck2_error::Result<Vec<String>> {
    let mut registered_toolchains = Vec::new();
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let cell_name = bzlmod_cell_name(&canonical_repo_name);
        for pattern in &module.registered_toolchains {
            registered_toolchains.push(qualify_bzlmod_registered_toolchain(
                pattern,
                &cell_name,
                cell_aliases_by_cell,
            )?);
        }
    }
    for result in bzlmod_module_extension_results {
        for pattern in &result.registered_toolchains {
            registered_toolchains.push(qualify_bzlmod_extension_registered_toolchain(
                pattern,
                result,
                cell_aliases_by_cell,
            )?);
        }
    }
    registered_toolchains.sort();
    registered_toolchains.dedup();
    Ok(registered_toolchains)
}

fn qualify_bzlmod_extension_registered_toolchain(
    pattern: &str,
    result: &BzlmodEvaluatedModuleExtension,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
) -> buck2_error::Result<String> {
    let parent_cell_name = if result.parent_is_root {
        "root".to_owned()
    } else {
        bzlmod_cell_name(&result.parent_canonical_repo_name)
    };
    if let Some(repo_relative) = pattern.strip_prefix('@') {
        let Some((repo_name, package_and_target)) = repo_relative.split_once("//") else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod module extension registered toolchain pattern `{}` is not an absolute target pattern",
                pattern
            ));
        };
        if !result
            .repo_names
            .iter()
            .any(|emitted_repo_name| emitted_repo_name.as_ref() == repo_name)
        {
            return qualify_bzlmod_registered_toolchain(
                pattern,
                &parent_cell_name,
                cell_aliases_by_cell,
            );
        }
        let canonical_repo_name = bzlmod_extension_unique_repo_canonical_repo_name(
            &result.extension_unique_name,
            repo_name,
        );
        return Ok(format!(
            "{}//{}",
            bzlmod_cell_name(&canonical_repo_name),
            package_and_target
        ));
    }

    if let Some(package_and_target) = pattern.strip_prefix("//") {
        return Ok(format!("{parent_cell_name}//{package_and_target}"));
    }

    Err(buck2_error!(
        buck2_error::ErrorTag::Input,
        "bzlmod module extension registered toolchain pattern `{}` is not an absolute target pattern",
        pattern
    ))
}

fn qualify_bzlmod_registered_toolchain(
    pattern: &str,
    module_cell_name: &str,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
) -> buck2_error::Result<String> {
    let pattern = pattern.trim();
    if let Some(rest) = pattern.strip_prefix("//") {
        return Ok(format!("{module_cell_name}//{rest}"));
    }
    if pattern.starts_with("@@") {
        return Ok(pattern.to_owned());
    }
    if let Some(rest) = pattern.strip_prefix('@') {
        let Some((alias, package_and_target)) = rest.split_once("//") else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod registered toolchain pattern `{}` in cell `{}` is not an absolute target pattern",
                pattern,
                module_cell_name
            ));
        };
        if alias.is_empty() {
            return Ok(format!("{module_cell_name}//{package_and_target}"));
        }
        if alias == "bazel_tools" {
            return Ok(format!("bazel_tools//{package_and_target}"));
        }
        let Some(target_cell_name) =
            bzlmod_cell_alias_target(cell_aliases_by_cell, module_cell_name, alias)
        else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod registered toolchain pattern `{}` in cell `{}` references unknown repo `{}`",
                pattern,
                module_cell_name,
                alias
            ));
        };
        return Ok(format!("{target_cell_name}//{package_and_target}"));
    }
    if pattern.contains("//") {
        Ok(pattern.to_owned())
    } else {
        Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "bzlmod registered toolchain pattern `{}` in cell `{}` is not an absolute target pattern",
            pattern,
            module_cell_name
        ))
    }
}

fn add_bzlmod_dep_alias(
    dep: &BazelDep,
    selected_versions: &BTreeMap<String, String>,
    aliases_by_key: &mut BTreeMap<(String, String), BTreeSet<String>>,
) {
    let Some(alias) = dep.apparent_name.as_ref() else {
        return;
    };
    let Some(version) = selected_versions.get(&dep.name) else {
        return;
    };
    aliases_by_key
        .entry((dep.name.clone(), version.clone()))
        .or_default()
        .insert(alias.clone());
}

fn add_bzlmod_dep_cell_alias(
    current_cell_name: &str,
    dep: &BazelDep,
    selected_versions: &BTreeMap<String, String>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    aliases_by_cell: &mut BTreeMap<String, BTreeMap<String, String>>,
) -> buck2_error::Result<()> {
    let Some(alias) = dep.apparent_name.as_ref() else {
        return Ok(());
    };
    let Some(version) = selected_versions.get(&dep.name) else {
        return Ok(());
    };
    let canonical_repo_name =
        bzlmod_selected_canonical_repo_name(canonical_repo_names_by_key, &dep.name, version)?;
    let cell_name = bzlmod_cell_name(&canonical_repo_name);
    add_bzlmod_cell_alias(aliases_by_cell, current_cell_name, alias, &cell_name);
    Ok(())
}

fn add_bzlmod_cell_alias(
    aliases_by_cell: &mut BTreeMap<String, BTreeMap<String, String>>,
    current_cell_name: &str,
    alias: &str,
    target_cell_name: &str,
) {
    aliases_by_cell
        .entry(current_cell_name.to_owned())
        .or_default()
        .insert(alias.to_owned(), target_cell_name.to_owned());
}

fn bzlmod_cell_alias_target<'a>(
    aliases_by_cell: &'a BTreeMap<String, BTreeMap<String, String>>,
    current_cell_name: &str,
    alias: &str,
) -> Option<&'a str> {
    aliases_by_cell
        .get(current_cell_name)
        .and_then(|aliases| aliases.get(alias))
        .map(String::as_str)
}

fn bzlmod_cell_alias_map_to_vec(aliases: BTreeMap<String, String>) -> Vec<BazelCompatCellAlias> {
    aliases
        .into_iter()
        .map(|(alias, cell_name)| BazelCompatCellAlias { alias, cell_name })
        .collect()
}

type BcrModuleFetch = BoxFuture<'static, buck2_error::Result<DiscoveredBcrModule>>;

fn schedule_bcr_module_fetch(
    registry: &'static str,
    client: &HttpClient,
    mut dep: BazelDep,
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, String>,
    scheduled: &mut BTreeSet<(String, String)>,
    pending: &mut FuturesUnordered<BcrModuleFetch>,
) {
    if archive_overrides.contains_key(&dep.name) {
        dep.version.clear();
    } else if let Some(version) = single_version_overrides.get(&dep.name) {
        dep.version = version.clone();
    }
    let key = (dep.name.clone(), dep.version.clone());
    if scheduled.insert(key) {
        let archive_override = archive_overrides.get(&dep.name).cloned();
        pending.push(fetch_bcr_module(registry, client.dupe(), dep, archive_override).boxed());
    }
}

async fn fetch_bcr_module(
    registry: &'static str,
    client: HttpClient,
    dep: BazelDep,
    archive_override: Option<BzlmodArchiveOverride>,
) -> buck2_error::Result<DiscoveredBcrModule> {
    let (source_json, module_text) = if let Some(archive_override) = archive_override.as_ref() {
        let source_json = BcrSourceJson {
            url: archive_override.url.clone(),
            integrity: archive_override.integrity.clone(),
            strip_prefix: archive_override.strip_prefix.clone(),
            archive_type: archive_override.archive_type.clone(),
            patches: None,
            patch_strip: archive_override.patch_strip,
        };
        let module_text = fetch_archive_override_module_file(&client, archive_override)
            .await
            .with_buck_error_context(|| {
                format!(
                    "Error reading MODULE.bazel from archive_override for module `{}`",
                    dep.name
                )
            })?;
        (source_json, module_text)
    } else {
        let source_url = format!(
            "{registry}/modules/{}/{}/source.json",
            dep.name, dep.version
        );
        let module_url = format!(
            "{registry}/modules/{}/{}/MODULE.bazel",
            dep.name, dep.version
        );
        let source_json: BcrSourceJson =
            serde_json::from_str(&http_get_text(&client, &source_url).await?)
                .with_buck_error_context(|| {
                    format!("Invalid BCR source metadata at `{source_url}`")
                })?;
        let module_text = http_get_text(&client, &module_url).await?;
        (source_json, module_text)
    };
    let module_lines = module_text.lines().map(str::to_owned).collect::<Vec<_>>();
    let constants = bzlmod_module_constants_from_lines(&module_lines);
    let extension_usages = bzlmod_extension_usages_from_lines(&module_lines, &constants, true);
    let use_repo_rule_invocations =
        bzlmod_use_repo_rule_invocations_from_lines(&module_lines, &constants, true)?;

    Ok(DiscoveredBcrModule {
        dep,
        source_json,
        module_aliases: bzlmod_module_aliases(&module_lines),
        use_repo_aliases: bzlmod_use_repo_aliases_from_usages(&extension_usages),
        extension_usages,
        use_repo_rule_invocations,
        constants,
        registered_toolchains: bzlmod_registered_toolchains_from_lines(&module_lines, true),
        deps: bzlmod_deps_from_lines(&module_lines, true),
    })
}

async fn http_get_text(client: &HttpClient, url: &str) -> buck2_error::Result<String> {
    let bytes = http_get_bytes(client, url).await?;
    String::from_utf8(bytes)
        .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Input))
        .with_buck_error_context(|| format!("Invalid UTF-8 response from `{url}`"))
}

async fn http_get_bytes(client: &HttpClient, url: &str) -> buck2_error::Result<Vec<u8>> {
    let response = client
        .get(url)
        .await
        .with_buck_error_context(|| format!("Error fetching `{url}`"))?;
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Tier0))?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn fetch_archive_override_module_file(
    client: &HttpClient,
    archive_override: &BzlmodArchiveOverride,
) -> buck2_error::Result<String> {
    let bytes = http_get_bytes(client, &archive_override.url).await?;
    verify_bzlmod_archive_integrity(&archive_override.url, &archive_override.integrity, &bytes)?;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Tier0))?
        .as_nanos();
    let temp = std::env::temp_dir().join(format!(
        "buck2-bzlmod-archive-{}-{unique}",
        sanitize_bzlmod_temp_name(&archive_override.module_name)
    ));
    let archive = temp.join("source.archive");
    let extract_dir = temp.join("extract");
    fs::create_dir_all(&extract_dir)
        .with_buck_error_context(|| format!("Error creating `{}`", extract_dir.display()))?;
    fs::write(&archive, bytes)
        .with_buck_error_context(|| format!("Error writing `{}`", archive.display()))?;
    extract_bzlmod_archive_override(archive_override, &archive, &extract_dir)?;

    if !archive_override.patches.is_empty() {
        // Local patch labels are applied during final external-cell materialization. For module
        // discovery, the unpatched MODULE.bazel is sufficient for the current override set.
    }

    let module_file = archive_override
        .strip_prefix
        .as_ref()
        .map(|strip_prefix| extract_dir.join(strip_prefix).join("MODULE.bazel"))
        .unwrap_or_else(|| extract_dir.join("MODULE.bazel"));
    let module_text = fs::read_to_string(&module_file)
        .with_buck_error_context(|| format!("Error reading `{}`", module_file.display()))?;
    let _ = fs::remove_dir_all(&temp);
    Ok(module_text)
}

fn extract_bzlmod_archive_override(
    archive_override: &BzlmodArchiveOverride,
    archive: &Path,
    extract_dir: &Path,
) -> buck2_error::Result<()> {
    let archive_type = archive_override
        .archive_type
        .as_deref()
        .or_else(|| archive.extension().and_then(|ext| ext.to_str()))
        .unwrap_or("");
    let mut command = if archive_type == "zip" || archive_override.url.ends_with(".zip") {
        let mut command = Command::new("unzip");
        command.arg("-q").arg(archive).arg("-d").arg(extract_dir);
        command
    } else if matches!(
        archive_type,
        "tar" | "gz" | "tgz" | "tar.gz" | "tar.xz" | "tar.bz2"
    ) || archive_override.url.ends_with(".tar.gz")
        || archive_override.url.ends_with(".tgz")
        || archive_override.url.ends_with(".tar.xz")
        || archive_override.url.ends_with(".tar.bz2")
        || archive_override.url.ends_with(".tar")
    {
        let mut command = Command::new("tar");
        command.arg("-xf").arg(archive).arg("-C").arg(extract_dir);
        command
    } else {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "unsupported archive_override archive type for `{}`",
            archive_override.url
        ));
    };

    let output = command
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .buck_error_context("Could not run archive_override extractor")?;
    if !output.status.success() {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "archive_override extraction failed for `{}` with exit code {:?}: {}",
            archive_override.url,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn verify_bzlmod_archive_integrity(
    url: &str,
    integrity: &str,
    bytes: &[u8],
) -> buck2_error::Result<()> {
    let Some(expected) = bzlmod_integrity_sha256_bytes(integrity)? else {
        return Ok(());
    };
    let got = Sha256::digest(bytes);
    if got.as_slice() != expected.as_slice() {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "archive_override integrity mismatch for `{}`: expected {}, got {}",
            url,
            hex::encode(expected),
            hex::encode(got)
        ));
    }
    Ok(())
}

fn bzlmod_integrity_sha256_bytes(integrity: &str) -> buck2_error::Result<Option<Vec<u8>>> {
    if integrity.is_empty() {
        return Ok(None);
    }
    let Some(encoded) = integrity.strip_prefix("sha256-") else {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "unsupported bzlmod archive integrity `{}`",
            integrity
        ));
    };
    let bytes = bzlmod_base64_decode(encoded)?;
    if bytes.len() != 32 {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "invalid bzlmod sha256 integrity `{}`",
            integrity
        ));
    }
    Ok(Some(bytes))
}

fn bzlmod_base64_decode(encoded: &str) -> buck2_error::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for ch in encoded.chars() {
        if ch == '=' {
            break;
        }
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "invalid base64 character `{}` in bzlmod integrity",
                    ch
                ));
            }
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn sanitize_bzlmod_temp_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn bzlmod_deps_from_lines(lines: &[String], ignore_dev_deps: bool) -> Vec<BazelDep> {
    let mut deps = Vec::new();
    for call in collect_bzl_calls(lines, "bazel_dep(") {
        if ignore_dev_deps && bzl_bool_arg(&call, "dev_dependency") {
            continue;
        }
        let Some(name) = bzl_string_arg(&call, "name") else {
            continue;
        };
        let version = bzl_string_arg(&call, "version").unwrap_or_default();
        let apparent_name = bzl_repo_name_arg(&call, &name);
        deps.push(BazelDep {
            name,
            version,
            apparent_name,
        });
    }
    deps
}

fn bzlmod_module_aliases(lines: &[String]) -> Vec<String> {
    let mut aliases = Vec::new();
    for call in collect_bzl_calls(lines, "module(") {
        for arg in ["name", "repo_name"] {
            if bzl_arg_is_none(&call, arg) {
                continue;
            }
            if let Some(alias) = bzl_string_arg(&call, arg) {
                aliases.push(alias);
            }
        }
    }
    aliases
}

fn bzlmod_archive_override_from_call(call: &str) -> buck2_error::Result<BzlmodArchiveOverride> {
    let module_name = bzl_string_arg(call, "module_name").ok_or_else(|| {
        buck2_error!(
            buck2_error::ErrorTag::Input,
            "archive_override must have a literal string `module_name`: {}",
            call
        )
    })?;
    let urls = bzl_call_named_arg_value(call, "urls")
        .as_deref()
        .and_then(|value| bzl_string_sequence_expression_raw_values(value, &[]))
        .unwrap_or_default();
    let url = if let Some(url) = urls.first() {
        url.clone()
    } else if let Some(url) = bzl_string_arg(call, "url") {
        url
    } else {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "archive_override for module `{}` must have a literal `url` or non-empty `urls`",
            module_name
        ));
    };
    let integrity = bzl_string_arg(call, "integrity").unwrap_or_default();
    let strip_prefix = bzl_string_arg(call, "strip_prefix");
    let archive_type =
        bzl_string_arg(call, "type").or_else(|| bzl_string_arg(call, "archive_type"));
    let patches = bzl_call_named_arg_value(call, "patches")
        .as_deref()
        .and_then(|value| bzl_string_sequence_expression_raw_values(value, &[]))
        .unwrap_or_default();
    let patch_strip = bzl_call_named_arg_value(call, "patch_strip")
        .as_deref()
        .and_then(bzl_integer_expression_value)
        .map(|value| {
            u32::try_from(value).map_err(|_| {
                buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "archive_override for module `{}` has negative patch_strip `{}`",
                    module_name,
                    value
                )
            })
        })
        .transpose()?;
    Ok(BzlmodArchiveOverride {
        module_name,
        url,
        integrity,
        strip_prefix,
        archive_type,
        patches,
        patch_strip,
    })
}

fn bzlmod_single_version_overrides_from_lines(lines: &[String]) -> BTreeMap<String, String> {
    collect_bzl_calls(lines, "single_version_override(")
        .into_iter()
        .filter_map(|call| {
            let module_name = bzl_string_arg(&call, "module_name")?;
            let version = bzl_string_arg(&call, "version")?;
            Some((module_name, version))
        })
        .collect()
}

fn bzlmod_root_module_from_lines(lines: &[String]) -> buck2_error::Result<RootBzlmodModule> {
    let mut name = "root".to_owned();
    let mut version = String::new();
    let mut canonical_repo_name = "root".to_owned();
    for call in collect_bzl_calls(lines, "module(") {
        if let Some(module_name) = bzl_string_arg(&call, "name") {
            if !module_name.is_empty() {
                name = module_name;
            }
        }
        if let Some(module_version) = bzl_string_arg(&call, "version") {
            version = module_version;
        }
        if let Some(repo_name) = bzl_repo_name_arg(&call, &name) {
            if !repo_name.is_empty() {
                canonical_repo_name = repo_name;
            }
        }
    }
    let constants = bzlmod_module_constants_from_lines(lines);
    let extension_usages = bzlmod_extension_usages_from_lines(lines, &constants, false);
    let use_repo_rule_invocations =
        bzlmod_use_repo_rule_invocations_from_lines(lines, &constants, false)?;
    Ok(RootBzlmodModule {
        name,
        version,
        canonical_repo_name,
        constants,
        extension_usages,
        use_repo_rule_invocations,
    })
}

fn bzlmod_use_repo_aliases_from_usages(usages: &[BzlmodExtensionUsage]) -> Vec<String> {
    usages
        .iter()
        .flat_map(|usage| usage.imports.iter())
        .map(|import| import.alias.clone())
        .collect()
}

fn bzlmod_registered_toolchains_from_lines(
    lines: &[String],
    ignore_dev_dependency: bool,
) -> Vec<String> {
    let mut toolchains = collect_bzl_calls(lines, "register_toolchains(")
        .into_iter()
        .filter(|call| !(ignore_dev_dependency && bzl_bool_arg(call, "dev_dependency")))
        .flat_map(|call| {
            bzl_call_args(&call)
                .into_iter()
                .filter_map(|arg| bzl_string_literal_value(arg.trim()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    toolchains.sort();
    toolchains.dedup();
    toolchains
}

fn bzlmod_extension_imports_from_usages(
    usages: &[BzlmodExtensionUsage],
    extension: &str,
) -> Vec<BzlmodUseRepoImport> {
    usages
        .iter()
        .filter(|usage| usage.extension_name == extension)
        .flat_map(|usage| usage.imports.iter().cloned())
        .collect()
}

fn bzlmod_extension_usages_from_lines(
    lines: &[String],
    constants: &[(String, String)],
    ignore_dev_dependency: bool,
) -> Vec<BzlmodExtensionUsage> {
    let mut usages = bzl_top_level_assignments(lines)
        .into_iter()
        .filter_map(|(name, value)| {
            let name = name.trim();
            if !is_bzl_identifier(name) || !value.trim_start().starts_with("use_extension(") {
                return None;
            }
            let dev_dependency = bzl_bool_arg(&value, "dev_dependency");
            if ignore_dev_dependency && dev_dependency {
                return None;
            }
            let args = bzl_call_args(&value);
            let extension_bzl_file = args.first().and_then(|arg| bzl_string_value(arg.trim()))?;
            let extension_name = args.get(1).and_then(|arg| bzl_string_value(arg.trim()))?;
            Some(BzlmodExtensionUsage {
                proxy_name: name.to_owned(),
                extension_bzl_file,
                extension_name,
                dev_dependency,
                imports: bzlmod_extension_imports(lines, name, constants),
                tags: bzlmod_extension_tags(lines, name, constants),
            })
        })
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    usages.retain(|usage| {
        seen.insert((
            usage.proxy_name.clone(),
            usage.extension_bzl_file.clone(),
            usage.extension_name.clone(),
        ))
    });
    usages
}

fn bzlmod_use_repo_rule_invocations_from_lines(
    lines: &[String],
    constants: &[(String, String)],
    ignore_dev_dependency: bool,
) -> buck2_error::Result<Vec<BzlmodUseRepoRuleInvocation>> {
    let repo_rule_bindings = bzl_top_level_assignments(lines)
        .into_iter()
        .filter_map(|(name, value)| {
            let name = name.trim();
            if !is_bzl_identifier(name) || !value.trim_start().starts_with("use_repo_rule(") {
                return None;
            }
            let args = bzl_call_args(&value);
            let rule_bzl_file = args.first().and_then(|arg| bzl_string_value(arg.trim()))?;
            let rule_name = args.get(1).and_then(|arg| bzl_string_value(arg.trim()))?;
            Some((name.to_owned(), rule_bzl_file, rule_name))
        })
        .collect::<Vec<_>>();

    let mut invocations = Vec::new();
    for (proxy_name, rule_bzl_file, rule_name) in repo_rule_bindings {
        for call in collect_bzl_calls(lines, &format!("{proxy_name}(")) {
            if ignore_dev_dependency && bzl_bool_arg(&call, "dev_dependency") {
                continue;
            }
            let repo_name = bzl_string_arg(&call, "name").ok_or_else(|| {
                buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "use_repo_rule invocation must have a literal string `name`: {}",
                    call
                )
            })?;
            let mut attrs = Vec::new();
            for arg in bzl_call_args(&call) {
                let Some((name, value)) = bzl_top_level_assignment(&arg) else {
                    continue;
                };
                let name = name.trim();
                if !is_bzl_identifier(name)
                    || name == "name"
                    || name == "dev_dependency"
                    || name == "visibility"
                {
                    continue;
                }
                attrs.push((
                    name.to_owned(),
                    bzlmod_repository_rule_attr_expression(value.trim(), constants)?,
                ));
            }
            attrs.sort_by(|left, right| left.0.cmp(&right.0));
            invocations.push(BzlmodUseRepoRuleInvocation {
                rule_bzl_file: rule_bzl_file.clone(),
                rule_name: rule_name.clone(),
                repo_name,
                attrs,
            });
        }
    }
    invocations.sort_by(|left, right| {
        (
            &left.rule_bzl_file,
            &left.rule_name,
            &left.repo_name,
            &left.attrs,
        )
            .cmp(&(
                &right.rule_bzl_file,
                &right.rule_name,
                &right.repo_name,
                &right.attrs,
            ))
    });
    invocations.dedup_by(|left, right| {
        left.rule_bzl_file == right.rule_bzl_file
            && left.rule_name == right.rule_name
            && left.repo_name == right.repo_name
            && left.attrs == right.attrs
    });
    Ok(invocations)
}

fn bzlmod_repository_rule_attr_expression(
    value: &str,
    constants: &[(String, String)],
) -> buck2_error::Result<String> {
    let value = value.trim();
    if let Some(string) = bzl_string_expression_value(value, constants) {
        return bzlmod_repository_rule_string_attr_expression(&string);
    }
    if let Some(values) = bzlmod_repository_rule_string_list_attr_expression(value, constants)? {
        return Ok(format!("[{}]", values.join(", ")));
    }
    Ok(value.to_owned())
}

fn bzlmod_repository_rule_string_attr_expression(value: &str) -> buck2_error::Result<String> {
    let serialized = serde_json::to_string(value)
        .buck_error_context("Error serializing use_repo_rule string repository-rule attribute")?;
    if bzlmod_string_looks_like_label(value) {
        Ok(format!("Label({serialized})"))
    } else {
        Ok(serialized)
    }
}

fn bzlmod_repository_rule_string_list_attr_expression(
    value: &str,
    constants: &[(String, String)],
) -> buck2_error::Result<Option<Vec<String>>> {
    let value = if is_bzl_identifier(value) {
        constants
            .iter()
            .find_map(|(name, constant_value)| (name == value).then_some(constant_value.as_str()))
            .unwrap_or(value)
    } else {
        value
    };
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .or_else(|| {
            value
                .strip_prefix('(')
                .and_then(|value| value.strip_suffix(')'))
        })
    else {
        return Ok(None);
    };
    let values = bzl_split_top_level(inner, ',')
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .map(|item| {
            let string = bzl_string_expression_value(item.trim(), constants)?;
            bzlmod_repository_rule_string_attr_expression(&string).ok()
        })
        .collect::<Option<Vec<_>>>();
    Ok(values)
}

fn bzlmod_string_looks_like_label(value: &str) -> bool {
    value.starts_with('@') || value.starts_with("//") || value.starts_with(':')
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct BzlmodCollectedTagCall {
    call: String,
    bindings: Vec<(String, String)>,
}

fn bzlmod_extension_tags(
    lines: &[String],
    proxy_name: &str,
    constants: &[(String, String)],
) -> Vec<BzlmodExtensionTag> {
    let call_prefix = format!("{proxy_name}.");
    let comprehension_calls = collect_bzl_list_comprehension_calls(lines, &call_prefix, constants);
    let comprehension_call_strings = comprehension_calls
        .iter()
        .map(|call| call.call.clone())
        .collect::<BTreeSet<_>>();
    let normal_calls = collect_bzl_calls(lines, &call_prefix)
        .into_iter()
        .filter(|call| !comprehension_call_strings.contains(call))
        .map(|call| BzlmodCollectedTagCall {
            call,
            bindings: Vec::new(),
        });
    let mut tags = normal_calls
        .chain(comprehension_calls)
        .into_iter()
        .filter_map(|collected_call| {
            let call = collected_call.call;
            let rest = call.strip_prefix(&call_prefix)?;
            let (tag_name, _) = rest.split_once('(')?;
            if !is_bzl_identifier(tag_name) {
                return None;
            }
            let mut kwargs = bzl_call_args(&call)
                .into_iter()
                .filter_map(|arg| {
                    let (name, value) = bzl_top_level_assignment(&arg)?;
                    let name = name.trim();
                    if is_bzl_identifier(name) {
                        Some((name.to_owned(), value.trim().to_owned()))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            kwargs.sort_by(|left, right| left.0.cmp(&right.0));
            Some(BzlmodExtensionTag {
                tag_name: tag_name.to_owned(),
                bindings: collected_call.bindings,
                kwargs,
            })
        })
        .collect::<Vec<_>>();
    tags.sort_by(|left, right| {
        (&left.tag_name, &left.bindings, &left.kwargs).cmp(&(
            &right.tag_name,
            &right.bindings,
            &right.kwargs,
        ))
    });
    tags
}

fn collect_bzl_list_comprehension_calls(
    lines: &[String],
    function: &str,
    constants: &[(String, String)],
) -> Vec<BzlmodCollectedTagCall> {
    let mut calls = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = strip_bzl_comment(&lines[index]);
        if !line.trim_start().starts_with('[') {
            index += 1;
            continue;
        }

        let mut block = vec![line.clone()];
        let mut depth = delimiter_delta(&line);
        index += 1;
        while depth > 0 && index < lines.len() {
            let line = strip_bzl_comment(&lines[index]);
            depth += delimiter_delta(&line);
            block.push(line);
            index += 1;
        }

        let Some(binding_groups) = bzl_list_comprehension_binding_groups(&block, constants) else {
            continue;
        };
        for call in collect_bzl_calls(&block, function) {
            for bindings in &binding_groups {
                calls.push(BzlmodCollectedTagCall {
                    call: call.clone(),
                    bindings: bindings.clone(),
                });
            }
        }
    }
    calls.sort();
    calls.dedup();
    calls
}

struct BzlmodListComprehensionForClause {
    names: Vec<String>,
    expression: String,
}

fn bzl_list_comprehension_binding_groups(
    block: &[String],
    constants: &[(String, String)],
) -> Option<Vec<Vec<(String, String)>>> {
    let clauses = bzl_list_comprehension_for_clauses(block)?;
    let mut groups = vec![Vec::new()];
    for clause in clauses {
        let options =
            bzl_list_comprehension_clause_bindings(&clause.names, &clause.expression, constants)?;
        let mut next_groups = Vec::new();
        for group in &groups {
            for option in &options {
                let mut next_group = group.clone();
                next_group.extend(option.iter().cloned());
                next_groups.push(next_group);
            }
        }
        groups = next_groups;
    }
    Some(groups)
}

fn bzl_list_comprehension_for_clauses(
    block: &[String],
) -> Option<Vec<BzlmodListComprehensionForClause>> {
    let mut clauses = Vec::new();
    let mut depth = 0i32;
    let mut index = 0usize;
    while index < block.len() {
        let line = strip_bzl_comment(&block[index]);
        let trimmed = line.trim();
        if depth != 1 {
            depth += delimiter_delta(&line);
            index += 1;
            continue;
        }

        let Some(rest) = trimmed.strip_prefix("for ") else {
            depth += delimiter_delta(&line);
            index += 1;
            continue;
        };
        let (name, expression) = rest.split_once(" in ")?;
        let names = bzl_list_comprehension_binding_names(name)?;
        let mut expression_lines = vec![expression.trim().to_owned()];
        let mut expression_depth = delimiter_delta(expression);
        index += 1;
        while expression_depth > 0 && index < block.len() {
            let line = strip_bzl_comment(&block[index]);
            expression_depth += delimiter_delta(&line);
            expression_lines.push(line.trim().to_owned());
            index += 1;
        }
        let expression = expression_lines.join("\n");
        let expression = expression.trim().trim_end_matches(',').trim().to_owned();
        clauses.push(BzlmodListComprehensionForClause { names, expression });
    }
    (!clauses.is_empty()).then_some(clauses)
}

fn bzl_list_comprehension_binding_names(binding: &str) -> Option<Vec<String>> {
    let binding = binding.trim();
    let binding = binding
        .strip_prefix('(')
        .and_then(|binding| binding.strip_suffix(')'))
        .unwrap_or(binding);
    let names = bzl_split_top_level(binding, ',')
        .into_iter()
        .map(|name| name.trim().to_owned())
        .collect::<Vec<_>>();
    if names.is_empty() || names.iter().any(|name| !is_bzl_identifier(name)) {
        return None;
    }
    Some(names)
}

fn bzl_list_comprehension_clause_bindings(
    names: &[String],
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<Vec<(String, String)>>> {
    if names.len() == 1 {
        let values = bzl_string_sequence_expression_values(expression, constants)?;
        return Some(
            values
                .into_iter()
                .map(|value| vec![(names[0].clone(), value)])
                .collect(),
        );
    }

    let values = bzl_dict_items_expression_values(expression, constants)?;
    values
        .into_iter()
        .map(|value| {
            if value.len() != names.len() {
                return None;
            }
            Some(names.iter().cloned().zip(value).collect::<Vec<_>>())
        })
        .collect()
}

fn bzl_dict_items_expression_values(
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<Vec<String>>> {
    let receiver = expression.trim().strip_suffix(".items()")?.trim();
    let dict = if is_bzl_identifier(receiver) {
        constants
            .iter()
            .find_map(|(name, value)| (name == receiver).then_some(value.as_str()))?
    } else {
        receiver
    };
    bzl_string_dict_literal_items(dict)?
        .into_iter()
        .map(|(key, value)| {
            Some(vec![
                serde_json::to_string(&key).ok()?,
                bzl_supported_module_literal_expression(value.trim())?,
            ])
        })
        .collect()
}

fn bzl_string_dict_literal_items(value: &str) -> Option<Vec<(String, String)>> {
    let inner = value
        .trim()
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))?;
    bzl_split_top_level(inner, ',')
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .map(|item| {
            let parts = bzl_split_top_level(&item, ':');
            if parts.len() != 2 {
                return None;
            }
            let key = bzl_string_literal_value(parts[0].trim())?;
            Some((key, parts[1].trim().to_owned()))
        })
        .collect()
}

fn bzl_supported_module_literal_expression(value: &str) -> Option<String> {
    let value = value.trim();
    if bzl_string_literal_value(value).is_some()
        || bzl_string_sequence_literal_raw_values(value).is_some()
        || bzl_module_constant_expression_is_supported(value)
    {
        return Some(value.to_owned());
    }
    None
}

fn bzl_string_sequence_expression_values(
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<String>> {
    if let Some(values) = bzl_string_sequence_literal_raw_values(expression) {
        return values
            .into_iter()
            .map(|value| serde_json::to_string(&value).ok())
            .collect();
    }
    if is_bzl_identifier(expression) {
        let (_, value) = constants.iter().find(|(name, _)| name == expression)?;
        return bzl_string_sequence_literal_values(value);
    }
    None
}

fn bzl_string_sequence_literal_values(value: &str) -> Option<Vec<String>> {
    bzl_string_sequence_literal_raw_values(value)?
        .into_iter()
        .map(|value| serde_json::to_string(&value).ok())
        .collect()
}

fn bzl_string_sequence_literal_raw_values(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    let inner = if let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        inner
    } else if let Some(inner) = value
        .strip_prefix('(')
        .and_then(|value| value.strip_suffix(')'))
    {
        inner
    } else {
        return None;
    };
    bzl_split_top_level(inner, ',')
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .map(|item| bzl_string_literal_value(item.trim()))
        .collect()
}

fn bzl_string_sequence_expression_raw_values(
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<String>> {
    if let Some(values) = bzl_string_sequence_literal_raw_values(expression) {
        return Some(values);
    }
    if is_bzl_identifier(expression) {
        let (_, value) = constants.iter().find(|(name, _)| name == expression)?;
        return bzl_string_sequence_literal_raw_values(value);
    }
    None
}

fn bzlmod_module_constants_from_lines(lines: &[String]) -> Vec<(String, String)> {
    bzl_top_level_assignments(lines)
        .into_iter()
        .filter_map(|(name, value)| {
            let name = name.trim();
            let value = value.trim();
            if !is_bzl_identifier(name) {
                return None;
            }
            if !bzl_module_constant_expression_is_supported(value) {
                return None;
            }
            Some((name.to_owned(), value.to_owned()))
        })
        .collect()
}

fn bzl_top_level_assignments(lines: &[String]) -> Vec<(String, String)> {
    let mut assignments = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_value = String::new();
    let mut depth = 0i32;

    for line in lines {
        let line = strip_bzl_comment(line);
        if current_name.is_none() {
            if line.chars().next().is_some_and(|ch| ch.is_whitespace()) {
                continue;
            }
            let Some((name, value)) = bzl_top_level_assignment(&line) else {
                continue;
            };
            current_name = Some(name.trim().to_owned());
            current_value = value.trim().to_owned();
            depth = delimiter_delta(value);
        } else {
            if !current_value.is_empty() {
                current_value.push('\n');
            }
            current_value.push_str(&line);
            depth += delimiter_delta(&line);
        }

        if depth <= 0 {
            let name = current_name.take().expect("assignment name should be set");
            assignments.push((name, std::mem::take(&mut current_value)));
            depth = 0;
        }
    }

    assignments
}

fn bzl_module_constant_expression_is_supported(value: &str) -> bool {
    let trimmed = value.trim_start();
    if trimmed.starts_with("use_extension(") || trimmed.starts_with("use_repo_rule(") {
        return false;
    }
    value.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                '_' | ' '
                    | '\n'
                    | '\t'
                    | '"'
                    | '\''
                    | '\\'
                    | '.'
                    | ','
                    | ':'
                    | '/'
                    | '-'
                    | '+'
                    | '%'
                    | '='
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
            )
    })
}

fn bzlmod_extension_imports(
    lines: &[String],
    proxy_name: &str,
    constants: &[(String, String)],
) -> Vec<BzlmodUseRepoImport> {
    collect_bzl_calls(lines, "use_repo(")
        .into_iter()
        .filter(|call| {
            bzl_call_args(call)
                .first()
                .is_some_and(|arg| arg.trim() == proxy_name)
        })
        .flat_map(|call| bzl_use_repo_imports(&call, constants))
        .collect()
}

fn bzlmod_patch_configs(
    registry: &str,
    dep: &BazelDep,
    source_json: &BcrSourceJson,
) -> Vec<BzlmodPatchConfig> {
    source_json
        .patches
        .as_ref()
        .into_iter()
        .flat_map(|patches| patches.iter())
        .map(|(file, integrity)| BzlmodPatchConfig {
            url: format!(
                "{registry}/modules/{}/{}/patches/{}",
                dep.name, dep.version, file
            ),
            integrity: integrity.clone(),
        })
        .collect()
}

fn bzlmod_selected_keys_dependency_first(
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    root_deps: &[BazelDep],
    selected_versions: &BTreeMap<String, String>,
    selected_keys: &BTreeSet<(String, String)>,
) -> Vec<(String, String)> {
    fn visit(
        key: &(String, String),
        discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
        selected_versions: &BTreeMap<String, String>,
        selected_keys: &BTreeSet<(String, String)>,
        seen: &mut BTreeSet<(String, String)>,
        ordered: &mut Vec<(String, String)>,
    ) {
        if !selected_keys.contains(key) || !seen.insert(key.clone()) {
            return;
        }
        if let Some(module) = discovered.get(key) {
            for dep in &module.deps {
                let Some(version) = selected_versions.get(&dep.name) else {
                    continue;
                };
                visit(
                    &(dep.name.clone(), version.clone()),
                    discovered,
                    selected_versions,
                    selected_keys,
                    seen,
                    ordered,
                );
            }
        }
        ordered.push(key.clone());
    }

    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    for dep in root_deps {
        let Some(version) = selected_versions.get(&dep.name) else {
            continue;
        };
        visit(
            &(dep.name.clone(), version.clone()),
            discovered,
            selected_versions,
            selected_keys,
            &mut seen,
            &mut ordered,
        );
    }
    for key in selected_keys {
        visit(
            key,
            discovered,
            selected_versions,
            selected_keys,
            &mut seen,
            &mut ordered,
        );
    }
    ordered
}

fn bzlmod_canonical_repo_names_by_key(
    selected_keys: &BTreeSet<(String, String)>,
) -> BTreeMap<(String, String), String> {
    let mut selected_versions_by_name = BTreeMap::<&str, BTreeSet<&str>>::new();
    for (module_name, version) in selected_keys {
        selected_versions_by_name
            .entry(module_name.as_str())
            .or_default()
            .insert(version.as_str());
    }

    selected_keys
        .iter()
        .map(|(module_name, version)| {
            let multiple_versions = selected_versions_by_name
                .get(module_name.as_str())
                .map_or(false, |versions| versions.len() > 1);
            (
                (module_name.clone(), version.clone()),
                bzlmod_canonical_repo_name(module_name, version, multiple_versions),
            )
        })
        .collect()
}

fn bzlmod_selected_canonical_repo_name(
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    module_name: &str,
    version: &str,
) -> buck2_error::Result<String> {
    canonical_repo_names_by_key
        .get(&(module_name.to_owned(), version.to_owned()))
        .cloned()
        .ok_or_else(|| {
            buck2_error!(
                buck2_error::ErrorTag::Input,
                "selected bzlmod module `{}@{}` does not have a canonical repository name",
                module_name,
                version
            )
        })
}

fn bzlmod_canonical_repo_name(module_name: &str, version: &str, multiple_versions: bool) -> String {
    match module_name {
        "bazel_tools" => "bazel_tools".to_owned(),
        "platforms" => "platforms".to_owned(),
        _ if multiple_versions => format!("{module_name}+{version}"),
        _ => format!("{module_name}+"),
    }
}

fn rules_java_remote_tools_archive(
    alias: &str,
) -> Option<(&'static str, &'static str, &'static str)> {
    match alias {
        "remote_java_tools" => Some((
            "remote_java_tools",
            "https://mirror.bazel.build/bazel_java_tools/releases/java/v13.9/java_tools-v13.9.zip",
            "3b92e0c1884ac0e9683e87c3c49e1098cff91faeacdb76cc90d92efb0df861cf",
        )),
        "remote_java_tools_linux" => Some((
            "remote_java_tools_linux",
            "https://mirror.bazel.build/bazel_java_tools/releases/java/v13.9/java_tools_linux-v13.9.zip",
            "7a3d7b1cd080efdf49ab2a3818177799416734acf2bd23040aa9037141287548",
        )),
        "remote_java_tools_windows" => Some((
            "remote_java_tools_windows",
            "https://mirror.bazel.build/bazel_java_tools/releases/java/v13.9/java_tools_windows-v13.9.zip",
            "6a17ac1921d60af5dca780f4200fd0f9963441bd7afff53b9efad6e7156c699d",
        )),
        "remote_java_tools_darwin_x86_64" => Some((
            "remote_java_tools_darwin_x86_64",
            "https://mirror.bazel.build/bazel_java_tools/releases/java/v13.9/java_tools_darwin_x86_64-v13.9.zip",
            "802bfb5085cec0ac5745a637ae2e7a7152c54230ba542d093a10bd48ba29ba6f",
        )),
        "remote_java_tools_darwin_arm64" => Some((
            "remote_java_tools_darwin_arm64",
            "https://mirror.bazel.build/bazel_java_tools/releases/java/v13.9/java_tools_darwin_arm64-v13.9.zip",
            "9fa400a43153b048ae5a785e3ee533d675ed6a994ab3c763f50bd15a28544c10",
        )),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BzlmodVersionIdentifier {
    Digits { number: u64, raw: String },
    Text(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BzlmodVersion {
    release: Vec<BzlmodVersionIdentifier>,
    prerelease: Vec<BzlmodVersionIdentifier>,
}

fn bzlmod_version_cmp(a: &str, b: &str) -> buck2_error::Result<Ordering> {
    let a = parse_bzlmod_version(a)?;
    let b = parse_bzlmod_version(b)?;

    match (a.release.is_empty(), b.release.is_empty()) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Greater),
        (false, true) => return Ok(Ordering::Less),
        (false, false) => {}
    }

    let release = bzlmod_identifier_lex_cmp(&a.release, &b.release);
    if release != Ordering::Equal {
        return Ok(release);
    }

    match (a.prerelease.is_empty(), b.prerelease.is_empty()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(Ordering::Greater),
        (false, true) => Ok(Ordering::Less),
        (false, false) => Ok(bzlmod_identifier_lex_cmp(&a.prerelease, &b.prerelease)),
    }
}

fn parse_bzlmod_version(version: &str) -> buck2_error::Result<BzlmodVersion> {
    if version.is_empty() {
        return Ok(BzlmodVersion {
            release: Vec::new(),
            prerelease: Vec::new(),
        });
    }

    let (version, build) = version.split_once('+').unwrap_or((version, ""));
    if !build.is_empty()
        && !build
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
    {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "invalid bzlmod version build metadata `{}`",
            build
        ));
    }

    let (release, prerelease) = version.split_once('-').unwrap_or((version, ""));
    if release.is_empty()
        || !release
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.')
    {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "invalid bzlmod version release `{}`",
            release
        ));
    }
    if !prerelease.is_empty()
        && !prerelease
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
    {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "invalid bzlmod version prerelease `{}`",
            prerelease
        ));
    }

    Ok(BzlmodVersion {
        release: parse_bzlmod_version_identifiers(release)?,
        prerelease: if prerelease.is_empty() {
            Vec::new()
        } else {
            parse_bzlmod_version_identifiers(prerelease)?
        },
    })
}

fn parse_bzlmod_version_identifiers(
    value: &str,
) -> buck2_error::Result<Vec<BzlmodVersionIdentifier>> {
    value
        .split('.')
        .map(|identifier| {
            if identifier.is_empty() {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "empty bzlmod version identifier in `{}`",
                    value
                ));
            }
            if identifier.chars().all(|ch| ch.is_ascii_digit()) {
                let number = identifier.parse::<u64>().map_err(|e| {
                    from_any_with_tag(e, buck2_error::ErrorTag::Input).context(format!(
                        "numeric bzlmod version identifier `{identifier}` is too large"
                    ))
                })?;
                Ok(BzlmodVersionIdentifier::Digits {
                    number,
                    raw: identifier.to_owned(),
                })
            } else {
                Ok(BzlmodVersionIdentifier::Text(identifier.to_owned()))
            }
        })
        .collect()
}

fn bzlmod_identifier_lex_cmp(
    a: &[BzlmodVersionIdentifier],
    b: &[BzlmodVersionIdentifier],
) -> Ordering {
    for (a, b) in a.iter().zip(b) {
        let cmp = bzlmod_identifier_cmp(a, b);
        if cmp != Ordering::Equal {
            return cmp;
        }
    }
    a.len().cmp(&b.len())
}

fn bzlmod_identifier_cmp(a: &BzlmodVersionIdentifier, b: &BzlmodVersionIdentifier) -> Ordering {
    match (a, b) {
        (
            BzlmodVersionIdentifier::Digits {
                number: a_number,
                raw: a_raw,
            },
            BzlmodVersionIdentifier::Digits {
                number: b_number,
                raw: b_raw,
            },
        ) => a_number.cmp(b_number).then_with(|| a_raw.cmp(b_raw)),
        (BzlmodVersionIdentifier::Digits { .. }, BzlmodVersionIdentifier::Text(_)) => {
            Ordering::Less
        }
        (BzlmodVersionIdentifier::Text(_), BzlmodVersionIdentifier::Digits { .. }) => {
            Ordering::Greater
        }
        (BzlmodVersionIdentifier::Text(a), BzlmodVersionIdentifier::Text(b)) => a.cmp(b),
    }
}

fn collect_bzl_calls(lines: &[String], function: &str) -> Vec<String> {
    let mut calls = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;

    for line in lines {
        let line = strip_bzl_comment(line);
        if current.is_empty() {
            let rest = line.trim_start();
            if !rest.starts_with(function) {
                continue;
            };
            depth = paren_delta(rest);
            current.push_str(rest);
        } else {
            current.push('\n');
            current.push_str(&line);
            depth += paren_delta(&line);
        }

        if depth <= 0 {
            calls.push(std::mem::take(&mut current));
            depth = 0;
        }
    }

    calls
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

fn strip_bzl_comment(line: &str) -> String {
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;

    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            continue;
        }
        if ch == '#' {
            return line[..idx].to_owned();
        }
    }

    line.to_owned()
}

fn paren_delta(s: &str) -> i32 {
    s.chars()
        .map(|ch| match ch {
            '(' => 1,
            ')' => -1,
            _ => 0,
        })
        .sum()
}

fn delimiter_delta(s: &str) -> i32 {
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut delta = 0i32;

    for ch in s.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            continue;
        }
        match ch {
            '(' | '[' | '{' => delta += 1,
            ')' | ']' | '}' => delta -= 1,
            _ => {}
        }
    }

    delta
}

fn bzl_string_arg(call: &str, arg: &str) -> Option<String> {
    let value = bzl_arg_value(call, arg)?;
    bzl_string_value(value)
}

fn bzl_bool_arg(call: &str, arg: &str) -> bool {
    bzl_arg_value(call, arg).is_some_and(|value| {
        let value = value.trim_start();
        value
            .strip_prefix("True")
            .is_some_and(|rest| rest.chars().next().is_none_or(|ch| !is_bzl_ident(ch)))
    })
}

fn bzl_repo_name_arg(call: &str, module_name: &str) -> Option<String> {
    if bzl_arg_is_none(call, "repo_name") {
        None
    } else {
        match bzl_string_arg(call, "repo_name") {
            Some(repo_name) if repo_name.is_empty() => Some(module_name.to_owned()),
            Some(repo_name) => Some(repo_name),
            None => Some(module_name.to_owned()),
        }
    }
}

fn bzl_first_string_arg(call: &str) -> Option<String> {
    let (_, args) = call.split_once('(')?;
    bzl_string_value(args.trim_start())
}

fn bzl_use_repo_imports(call: &str, constants: &[(String, String)]) -> Vec<BzlmodUseRepoImport> {
    let args = bzl_call_args(call);
    let mut imports = Vec::new();
    for arg in args.into_iter().skip(1) {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        if let Some((alias, actual)) = bzl_top_level_assignment(arg) {
            let alias = alias.trim();
            if alias != "dev_dependency" && is_bzl_identifier(alias) {
                if let Some(repo_name) = bzl_string_expression_value(actual.trim(), constants) {
                    imports.push(BzlmodUseRepoImport {
                        alias: alias.to_owned(),
                        repo_name,
                    });
                }
            }
        } else if let Some(alias) = bzl_string_expression_value(arg, constants) {
            imports.push(BzlmodUseRepoImport {
                alias: alias.clone(),
                repo_name: alias,
            });
        }
    }
    imports
}

fn bzl_call_args(call: &str) -> Vec<String> {
    let Some((_, args)) = call.split_once('(') else {
        return Vec::new();
    };
    let args = args.trim();
    let args = args.strip_suffix(')').unwrap_or(args);
    bzl_split_top_level(args, ',')
}

fn bzl_top_level_assignment(arg: &str) -> Option<(&str, &str)> {
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut depth = 0i32;

    for (idx, ch) in arg.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '=' if depth == 0 => return Some((&arg[..idx], &arg[idx + 1..])),
            _ => {}
        }
    }
    None
}

fn bzl_split_top_level(s: &str, delimiter: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut depth = 0i32;

    for (idx, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ if ch == delimiter && depth == 0 => {
                parts.push(s[start..idx].to_owned());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    parts.push(s[start..].to_owned());
    parts
}

fn bzl_string_value(value: &str) -> Option<String> {
    let mut chars = value.chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    let mut result = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            result.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some(result);
        } else {
            result.push(ch);
        }
    }

    None
}

fn bzl_string_expression_value(value: &str, constants: &[(String, String)]) -> Option<String> {
    let value = value.trim();
    if let Some(literal) = bzl_string_literal_value(value) {
        return Some(literal);
    }
    if let Some((receiver, args)) = bzl_top_level_method_call(value, "format") {
        let template = bzl_string_expression_value(receiver, constants)?;
        let mut values = Vec::new();
        let mut named_values = BTreeMap::new();
        for arg in bzl_split_top_level(args, ',') {
            let arg = arg.trim();
            if arg.is_empty() {
                continue;
            }
            if let Some((name, value)) = bzl_top_level_assignment(arg) {
                let name = name.trim();
                if is_bzl_identifier(name) {
                    named_values.insert(
                        name.to_owned(),
                        bzl_string_expression_value(value.trim(), constants)?,
                    );
                }
            } else {
                values.push(bzl_string_expression_value(arg, constants)?);
            }
        }
        return bzl_format_string(&template, &values, &named_values);
    }
    if let Some((receiver, args)) = bzl_top_level_method_call(value, "replace") {
        let receiver = bzl_string_expression_value(receiver, constants)?;
        let args = bzl_split_top_level(args, ',');
        if args.len() != 2 {
            return None;
        }
        let from = bzl_string_expression_value(args[0].trim(), constants)?;
        let to = bzl_string_expression_value(args[1].trim(), constants)?;
        return Some(receiver.replace(&from, &to));
    }
    if let Some((receiver, index)) = bzl_top_level_index(value) {
        let values = bzl_string_sequence_expression_raw_values(receiver, constants)?;
        let index = bzl_integer_expression_value(index.trim())?;
        let len = values.len() as i32;
        let index = if index < 0 { len + index } else { index };
        if index < 0 || index >= len {
            return None;
        }
        return values.get(index as usize).cloned();
    }
    if is_bzl_identifier(value) {
        let (_, constant_value) = constants.iter().find(|(name, _)| name == value)?;
        return bzl_string_expression_value(constant_value, constants);
    }
    None
}

fn bzl_format_string(
    template: &str,
    values: &[String],
    named_values: &BTreeMap<String, String>,
) -> Option<String> {
    let mut result = String::new();
    let mut rest = template;
    let mut next_value = 0usize;
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('}') else {
            return None;
        };
        let placeholder = &after_open[..close];
        if placeholder.is_empty() {
            let value = values.get(next_value)?;
            next_value += 1;
            result.push_str(value);
        } else {
            result.push_str(named_values.get(placeholder)?);
        }
        rest = &after_open[close + 1..];
    }
    result.push_str(rest);
    Some(result)
}

fn bzl_top_level_method_call<'a>(value: &'a str, method: &str) -> Option<(&'a str, &'a str)> {
    let value = value.trim();
    let suffix_prefix = format!(".{method}(");
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut depth = 0i32;

    for (idx, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            continue;
        }
        if depth == 0 && value[idx..].starts_with(&suffix_prefix) {
            let args_start = idx + suffix_prefix.len();
            let args = value[args_start..].strip_suffix(')')?;
            if delimiter_delta(args) == 0 {
                return Some((value[..idx].trim(), args));
            }
            return None;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }

    None
}

fn bzl_top_level_index(value: &str) -> Option<(&str, &str)> {
    let value = value.trim();
    if !value.ends_with(']') {
        return None;
    }
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut depth = 0i32;

    for (idx, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            continue;
        }
        if ch == '[' && depth == 0 {
            let index = &value[idx + 1..value.len() - 1];
            if delimiter_delta(index) == 0 {
                return Some((value[..idx].trim(), index));
            }
            return None;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }

    None
}

fn bzl_integer_expression_value(value: &str) -> Option<i32> {
    value.trim().parse::<i32>().ok()
}

fn bzl_string_literal_value(value: &str) -> Option<String> {
    let mut chars = value.trim().chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    let mut result = String::new();
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if escaped {
            result.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            if chars.as_str().trim().is_empty() {
                return Some(result);
            }
            return None;
        } else {
            result.push(ch);
        }
    }

    None
}

fn bzl_arg_is_none(call: &str, arg: &str) -> bool {
    bzl_arg_value(call, arg).is_some_and(|value| value.starts_with("None"))
}

fn bzl_call_named_arg_value(call: &str, arg: &str) -> Option<String> {
    for call_arg in bzl_call_args(call) {
        let Some((name, value)) = bzl_top_level_assignment(&call_arg) else {
            continue;
        };
        if name.trim() == arg {
            return Some(value.trim().to_owned());
        }
    }
    None
}

fn bzl_arg_value<'a>(call: &'a str, arg: &str) -> Option<&'a str> {
    let mut search_start = 0;
    while let Some(pos) = call[search_start..].find(arg) {
        let pos = search_start + pos;
        let before = call[..pos].chars().next_back();
        let after = call[pos + arg.len()..].chars().next();
        if before.is_none_or(|ch| !is_bzl_ident(ch)) && after.is_none_or(|ch| !is_bzl_ident(ch)) {
            let rest = call[pos + arg.len()..].trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return Some(rest.trim_start());
            }
        }
        search_start = pos + arg.len();
    }

    None
}

fn is_bzl_ident(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn is_bzl_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(is_bzl_ident)
}

async fn get_external_buckconfig_paths(
    file_ops: &mut dyn ConfigParserFileOps,
) -> buck2_error::Result<Vec<ConfigPath>> {
    let skip_default_external_config = buck2_env!(
        "BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG",
        bool,
        applicability = testing
    )?;

    let mut buckconfig_paths: Vec<ConfigPath> = Vec::new();

    if !skip_default_external_config {
        for buckconfig in DEFAULT_EXTERNAL_CONFIG_SOURCES {
            match buckconfig {
                ExternalConfigSource::UserFile(file) => {
                    let home_dir = dirs::home_dir();
                    if let Some(home_dir_path) = home_dir {
                        let buckconfig_path = ForwardRelativePath::new(file)?;
                        buckconfig_paths.push(ConfigPath::Global(
                            AbsPath::new(&home_dir_path)?.join(buckconfig_path.as_str()),
                        ));
                    }
                }
                ExternalConfigSource::UserFolder(folder) => {
                    let home_dir = dirs::home_dir();
                    if let Some(home_dir_path) = home_dir {
                        let buckconfig_path = ForwardRelativePath::new(folder)?;
                        let buckconfig_folder_abs_path =
                            AbsPath::new(&home_dir_path)?.join(buckconfig_path.as_str());
                        push_all_files_from_a_directory(
                            &mut buckconfig_paths,
                            &ConfigPath::Global(buckconfig_folder_abs_path),
                            file_ops,
                        )
                        .await?;
                    }
                }
                ExternalConfigSource::GlobalFile(file) => {
                    buckconfig_paths.push(ConfigPath::Global(AbsPath::new(*file)?.to_owned()));
                }
                ExternalConfigSource::GlobalFolder(folder) => {
                    let buckconfig_folder_abs_path = AbsPath::new(*folder)?.to_owned();
                    push_all_files_from_a_directory(
                        &mut buckconfig_paths,
                        &ConfigPath::Global(buckconfig_folder_abs_path),
                        file_ops,
                    )
                    .await?;
                }
            }
        }
    }

    let extra_external_config =
        buck2_env!("BUCK2_TEST_EXTRA_EXTERNAL_CONFIG", applicability = testing)?;

    if let Some(f) = extra_external_config {
        buckconfig_paths.push(ConfigPath::Global(AbsPath::new(f)?.to_owned()));
    }

    Ok(buckconfig_paths)
}

async fn get_project_buckconfig_paths(
    path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> buck2_error::Result<Vec<ConfigPath>> {
    let mut buckconfig_paths: Vec<ConfigPath> = Vec::new();

    for buckconfig in DEFAULT_PROJECT_CONFIG_SOURCES {
        match buckconfig {
            ProjectConfigSource::CellRelativeFile(file) => {
                let buckconfig_path = ForwardRelativePath::new(file)?;
                buckconfig_paths.push(ConfigPath::Project(
                    path.as_project_relative_path().join(buckconfig_path),
                ));
            }
            ProjectConfigSource::CellRelativeFolder(folder) => {
                let buckconfig_folder_path = ForwardRelativePath::new(folder)?;
                let buckconfig_folder_path =
                    path.as_project_relative_path().join(buckconfig_folder_path);
                push_all_files_from_a_directory(
                    &mut buckconfig_paths,
                    &ConfigPath::Project(buckconfig_folder_path),
                    file_ops,
                )
                .await?;
            }
        }
    }

    Ok(buckconfig_paths)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use buck2_cli_proto::ConfigOverride;
    use buck2_core::cells::cell_root_path::CellRootPath;
    use buck2_core::cells::cell_root_path::CellRootPathBuf;
    use buck2_core::cells::external::ExternalCellOrigin;
    use buck2_core::cells::external::GitCellSetup;
    use buck2_core::cells::name::CellName;
    use buck2_core::fs::project_rel_path::ProjectRelativePath;
    use dice::DiceComputations;
    use indoc::indoc;

    use crate::external_cells::EXTERNAL_CELLS_IMPL;
    use crate::external_cells::ExternalCellsImpl;
    use crate::file_ops::delegate::FileOpsDelegate;
    use crate::legacy_configs::cells::BuckConfigBasedCells;
    use crate::legacy_configs::configs::testing::TestConfigParserFileOps;
    use crate::legacy_configs::configs::tests::assert_config_value;
    use crate::legacy_configs::key::BuckconfigKeyRef;

    #[tokio::test]
    async fn test_cells() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = .
                                other = other/
                                other_alias = other/
                                third_party = third_party/
                        "#
                ),
            ),
            (
                "other/.buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = ..
                                other = .
                                third_party = ../third_party/
                        "#
                ),
            ),
            (
                "third_party/.buckconfig",
                indoc!(
                    r#"
                            [cells]
                                third_party = .
                        "#
                ),
            ),
        ])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;

        let resolver = &cells.cell_resolver;

        let root_instance = resolver.get(CellName::testing_new("root"))?;
        let other_instance = resolver.get(CellName::testing_new("other"))?;
        let tp_instance = resolver.get(CellName::testing_new("third_party"))?;

        assert_eq!("", root_instance.path().as_str());
        assert_eq!("other", other_instance.path().as_str());
        assert_eq!("third_party", tp_instance.path().as_str());

        assert_eq!(
            "other",
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("other_alias")?
                .as_str()
        );

        let tp_resolver = cells
            .get_cell_alias_resolver_for_cwd_fast_with_file_ops(
                &mut file_ops,
                tp_instance.path().as_project_relative_path(),
            )
            .await?;

        assert_eq!("other", tp_resolver.resolve("other_alias")?.as_str());

        Ok(())
    }

    #[tokio::test]
    async fn test_bazel_compat_defaults_without_buckconfig() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                "MODULE.bazel",
                indoc!(
                    r#"
                    module(name = "hello", repo_name = "hello_root")
                    bazel_dep(name = "rules_go", version = "0.57.0", repo_name = "io_bazel_rules_go")
                    bazel_dep(name = "hidden", version = "1.0.0", repo_name = None)
                    include("//deps:extra.MODULE.bazel")
                "#
                ),
            ),
            (
                "deps/extra.MODULE.bazel",
                indoc!(
                    r#"
                    bazel_dep(name = "rules_shell", version = "0.3.0")
                "#
                ),
            ),
            (
                "buck-out/v2/external_cells/bzlmod/rules_go+0.57.0/MODULE.bazel",
                "module(name = \"rules_go\")\n",
            ),
        ])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;
        let resolver = &cells.cell_resolver;

        assert_eq!(
            "",
            resolver.get(CellName::testing_new("root"))?.path().as_str()
        );
        assert_eq!(
            "bzlmod_rules_go_0_57_0",
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("rules_go")?
                .as_str()
        );
        assert_eq!(
            "bzlmod_rules_go_0_57_0",
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("io_bazel_rules_go")?
                .as_str()
        );
        assert_eq!(
            "bzlmod_platforms",
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("platforms")?
                .as_str()
        );
        let rules_go_resolver = cells
            .get_cell_alias_resolver_for_cwd_fast_with_file_ops(
                &mut file_ops,
                ProjectRelativePath::new("buck-out/v2/external_cells/bzlmod/rules_go+0.57.0")?,
            )
            .await?;
        assert_eq!(
            "bzlmod_platforms",
            rules_go_resolver.resolve("platforms")?.as_str()
        );
        assert_eq!(
            "bzlmod_rules_shell_0_3_0",
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("rules_shell")?
                .as_str()
        );
        assert_eq!(
            "root",
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("hello")?
                .as_str()
        );
        assert_eq!(
            "root",
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("hello_root")?
                .as_str()
        );
        assert!(
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("hidden")
                .is_err()
        );
        assert_eq!(
            resolver.get(CellName::testing_new("prelude"))?.external(),
            Some(&ExternalCellOrigin::Bundled(CellName::testing_new(
                "prelude"
            ))),
        );

        let root_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("root"), &mut file_ops)
            .await?;
        assert_eq!(
            root_config.get(BuckconfigKeyRef {
                section: "buildfile",
                property: "name_v2",
            }),
            Some("BUILD.bazel,BUILD"),
        );
        assert_eq!(
            root_config.get(BuckconfigKeyRef {
                section: "buildfile",
                property: "includes",
            }),
            Some("prelude//bazel/prelude.bzl"),
        );

        Ok(())
    }

    #[test]
    fn test_bzlmod_extension_usages_are_parsed_without_extension_name_special_cases() {
        let lines = indoc!(
            r#"
            sdk = use_extension("//go:extensions.bzl", "go_sdk")
            _GO_MOD = "//:go.mod"
            SUPPORTED_PYTHON_VERSIONS = [
                "3.11",
                "3.12",
            ]
            sdk.from_file(name = "go_default_sdk", go_mod = "//:go.mod")
            [sdk.from_file(name = name, go_mod = "//:go.mod") for name in ("ignored",)]
            use_repo(
                sdk,
                "go_toolchains",
                alias_name = "actual_repo",
                system_python = "python_{}".format(SUPPORTED_PYTHON_VERSIONS[-1].replace(".", "_")),
            )

            features = use_extension("@bazel_features//:extensions.bzl", "version_extension")
            use_repo(features, "bazel_features_globals")

            rules_kotlin_extensions = use_extension(
                "//src/main/starlark/core/repositories:bzlmod_setup.bzl",
                "rules_kotlin_extensions",
            )
            use_repo(
                rules_kotlin_extensions,
                "com_github_jetbrains_kotlin",
            )
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        assert_eq!(
            super::bzlmod_module_constants_from_lines(&lines),
            vec![
                ("_GO_MOD".to_owned(), "\"//:go.mod\"".to_owned()),
                (
                    "SUPPORTED_PYTHON_VERSIONS".to_owned(),
                    "[\n    \"3.11\",\n    \"3.12\",\n]".to_owned()
                )
            ]
        );
        let constants = super::bzlmod_module_constants_from_lines(&lines);
        let usages = super::bzlmod_extension_usages_from_lines(&lines, &constants, false);
        assert_eq!(usages.len(), 3);
        assert_eq!(usages[0].proxy_name, "sdk");
        assert_eq!(usages[0].extension_bzl_file, "//go:extensions.bzl");
        assert_eq!(usages[0].extension_name, "go_sdk");
        assert_eq!(usages[0].tags.len(), 1);
        assert_eq!(usages[0].tags[0].tag_name, "from_file");
        assert_eq!(
            usages[0].tags[0].kwargs,
            vec![
                ("go_mod".to_owned(), "\"//:go.mod\"".to_owned()),
                ("name".to_owned(), "\"go_default_sdk\"".to_owned()),
            ]
        );
        assert_eq!(usages[0].imports.len(), 3);
        assert_eq!(usages[0].imports[0].alias, "go_toolchains");
        assert_eq!(usages[0].imports[0].repo_name, "go_toolchains");
        assert_eq!(usages[0].imports[1].alias, "alias_name");
        assert_eq!(usages[0].imports[1].repo_name, "actual_repo");
        assert_eq!(usages[0].imports[2].alias, "system_python");
        assert_eq!(usages[0].imports[2].repo_name, "python_3_12");
        assert_eq!(
            super::bzlmod_extension_tag_repo_names(&usages[0]),
            vec!["go_default_sdk".to_owned()]
        );

        assert_eq!(usages[1].proxy_name, "features");
        assert_eq!(
            usages[1].extension_bzl_file,
            "@bazel_features//:extensions.bzl"
        );
        assert_eq!(usages[1].extension_name, "version_extension");
        assert_eq!(usages[1].imports.len(), 1);
        assert_eq!(usages[1].imports[0].alias, "bazel_features_globals");
        assert_eq!(usages[1].imports[0].repo_name, "bazel_features_globals");

        assert_eq!(usages[2].proxy_name, "rules_kotlin_extensions");
        assert_eq!(
            usages[2].extension_bzl_file,
            "//src/main/starlark/core/repositories:bzlmod_setup.bzl"
        );
        assert_eq!(usages[2].extension_name, "rules_kotlin_extensions");
        assert_eq!(usages[2].imports.len(), 1);
        assert_eq!(usages[2].imports[0].alias, "com_github_jetbrains_kotlin");
        assert_eq!(
            usages[2].imports[0].repo_name,
            "com_github_jetbrains_kotlin"
        );
    }

    #[test]
    fn test_bzlmod_extension_tags_expand_simple_list_comprehensions() {
        let lines = indoc!(
            r#"
            SUPPORTED_PYTHON_VERSIONS = [
                "3.11",
                "3.12",
            ]

            python = use_extension("@rules_python//python/extensions:python.bzl", "python")

            [
                python.toolchain(
                    is_default = python_version == SUPPORTED_PYTHON_VERSIONS[-1],
                    python_version = python_version,
                )
                for python_version in SUPPORTED_PYTHON_VERSIONS
            ]
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let constants = super::bzlmod_module_constants_from_lines(&lines);
        assert_eq!(
            constants,
            vec![(
                "SUPPORTED_PYTHON_VERSIONS".to_owned(),
                "[\n    \"3.11\",\n    \"3.12\",\n]".to_owned()
            )]
        );
        let usages = super::bzlmod_extension_usages_from_lines(&lines, &constants, false);
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].tags.len(), 2);
        assert_eq!(
            usages[0].tags[0].bindings,
            vec![("python_version".to_owned(), "\"3.11\"".to_owned())]
        );
        assert_eq!(
            usages[0].tags[1].bindings,
            vec![("python_version".to_owned(), "\"3.12\"".to_owned())]
        );
    }

    #[test]
    fn test_bzlmod_use_repo_rule_attrs_resolve_module_constants() {
        let lines = indoc!(
            r#"
            http_file = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_file")

            _VERSION = "v1.2.3"
            FILE_NAME = ("tool_" + _VERSION).replace(".", "_")
            URL = "https://example.com/{version}/tool".format(version = _VERSION)
            SHA256 = "abc123"

            http_file(
                name = "tool",
                sha256 = SHA256,
                urls = [URL],
            )
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let constants = super::bzlmod_module_constants_from_lines(&lines);
        let invocations =
            super::bzlmod_use_repo_rule_invocations_from_lines(&lines, &constants, false).unwrap();
        assert_eq!(invocations.len(), 1);
        assert_eq!(
            invocations[0].attrs,
            vec![
                ("sha256".to_owned(), "\"abc123\"".to_owned()),
                (
                    "urls".to_owned(),
                    "[\"https://example.com/v1.2.3/tool\"]".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn test_bzlmod_extension_usages_ignore_dev_dependency_when_requested() {
        let lines = indoc!(
            r#"
            dev_ext = use_extension(
                "@dev_repo//:extensions.bzl",
                "dev",
                dev_dependency = True,
            )
            use_repo(dev_ext, "dev_repo")

            prod_ext = use_extension("//:extensions.bzl", "prod")
            use_repo(prod_ext, "prod_repo")
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let constants = super::bzlmod_module_constants_from_lines(&lines);
        let usages = super::bzlmod_extension_usages_from_lines(&lines, &constants, true);
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].proxy_name, "prod_ext");
        assert_eq!(usages[0].imports[0].repo_name, "prod_repo");
    }

    #[test]
    fn test_bzlmod_registered_toolchains_resolve_declaring_repo_mapping() -> buck2_error::Result<()>
    {
        let mut cell_aliases_by_cell = std::collections::BTreeMap::new();
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "bzlmod_rules_go_0_57_0",
            "go_toolchains",
            "bzlmod_rules_go_0_57_0_go_sdk_go_toolchains",
        );

        assert_eq!(
            super::qualify_bzlmod_registered_toolchain(
                "@go_toolchains//:all",
                "bzlmod_rules_go_0_57_0",
                &cell_aliases_by_cell,
            )?,
            "bzlmod_rules_go_0_57_0_go_sdk_go_toolchains//:all"
        );
        assert_eq!(
            super::qualify_bzlmod_registered_toolchain(
                "//:all",
                "bzlmod_rules_go_0_57_0",
                &cell_aliases_by_cell,
            )?,
            "bzlmod_rules_go_0_57_0//:all"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_buckconfig_wins_over_bazel_compat_defaults() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".buckconfig",
                indoc!(
                    r#"
                        [cells]
                            root = .
                    "#
                ),
            ),
            (
                "MODULE.bazel",
                indoc!(
                    r#"
                        module(name = "ignored")
                    "#
                ),
            ),
        ])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;
        assert!(
            cells
                .cell_resolver
                .get(CellName::testing_new("prelude"))
                .is_err()
        );

        let root_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("root"), &mut file_ops)
            .await?;
        assert_eq!(
            root_config.get(BuckconfigKeyRef {
                section: "buildfile",
                property: "name_v2",
            }),
            None,
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_multi_cell_with_config_file() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = .
                                other = other/
                                other_alias = other/
                                third_party = third_party/
                        "#
                ),
            ),
            (
                "other/.buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = ..
                                other = .
                                third_party = ../third_party/
                            [buildfile]
                                name = TARGETS
                        "#
                ),
            ),
            (
                "third_party/.buckconfig",
                indoc!(
                    r#"
                            [cells]
                                third_party = .
                            [buildfile]
                                name_v2 = OKAY
                                name = OKAY_v1
                        "#
                ),
            ),
            (
                "other/cli-conf",
                indoc!(
                    r#"
                            [foo]
                                bar = blah
                        "#
                ),
            ),
        ])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(
            &mut file_ops,
            &[ConfigOverride::file(
                "cli-conf",
                Some(CellRootPathBuf::testing_new("other")),
            )],
        )
        .await?;

        let root_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("root"), &mut file_ops)
            .await?;
        let other_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("other"), &mut file_ops)
            .await?;
        let tp_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("third_party"), &mut file_ops)
            .await?;

        assert_eq!(
            root_config.get(BuckconfigKeyRef {
                section: "foo",
                property: "bar"
            }),
            Some("blah")
        );
        assert_eq!(
            other_config.get(BuckconfigKeyRef {
                section: "foo",
                property: "bar"
            }),
            Some("blah")
        );
        assert_eq!(
            tp_config.get(BuckconfigKeyRef {
                section: "foo",
                property: "bar"
            }),
            Some("blah")
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_multi_cell_no_repositories_in_non_root_cell() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = .
                                other = other/
                        "#
                ),
            ),
            (
                "other/.buckconfig",
                indoc!(
                    r#"
                            [foo]
                                bar = baz
                        "#
                ),
            ),
        ])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;

        let other_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("other"), &mut file_ops)
            .await?;

        assert_eq!(
            other_config.get(BuckconfigKeyRef {
                section: "foo",
                property: "bar"
            }),
            Some("baz")
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_multi_cell_with_cell_relative() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = .
                                other = other/
                        "#
                ),
            ),
            (
                "global-conf",
                indoc!(
                    r#"
                            [apple]
                                test_tool = xctool
                        "#
                ),
            ),
            (
                "other/.buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = ..
                                other = .
                            [buildfile]
                                name = TARGETS
                        "#
                ),
            ),
            (
                "other/app-conf",
                indoc!(
                    r#"
                            [apple]
                                ide = Xcode
                        "#
                ),
            ),
        ])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(
            &mut file_ops,
            &[
                ConfigOverride::file("app-conf", Some(CellRootPathBuf::testing_new("other"))),
                ConfigOverride::file("global-conf", Some(CellRootPathBuf::testing_new(""))),
            ],
        )
        .await?;

        let other_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("other"), &mut file_ops)
            .await?;

        assert_eq!(
            other_config.get(BuckconfigKeyRef {
                section: "apple",
                property: "ide"
            }),
            Some("Xcode")
        );
        assert_eq!(
            other_config.get(BuckconfigKeyRef {
                section: "apple",
                property: "test_tool"
            }),
            Some("xctool")
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_local_config_file_overwrite_config_file() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = .
                            [apple]
                                key = value1
                                key2 = value2
                        "#
                ),
            ),
            (
                ".buckconfig.local",
                indoc!(
                    r#"
                            [orange]
                                key = value3
                            [apple]
                                key2 = value5
                                key3 = value4
                        "#
                ),
            ),
        ])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;

        let config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("root"), &mut file_ops)
            .await?;
        // No local override
        assert_config_value(&config, "apple", "key", "value1");
        // local override to new value
        assert_config_value(&config, "apple", "key2", "value5");
        // local override new field
        assert_config_value(&config, "apple", "key3", "value4");
        // local override new section
        assert_config_value(&config, "orange", "key", "value3");

        Ok(())
    }

    #[tokio::test]
    async fn test_multi_cell_local_config_file_overwrite_config_file() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = .
                                other = other/
                            [apple]
                                key = value1
                                key2 = value2
                        "#
                ),
            ),
            (
                ".buckconfig.local",
                indoc!(
                    r#"
                            [orange]
                                key = value3
                            [apple]
                                key2 = value5
                                key3 = value4
                        "#
                ),
            ),
            (
                "other/.buckconfig",
                indoc!(
                    r#"
                            [cells]
                                root = ..
                                other = .
                            [apple]
                                key = othervalue1
                                key2 = othervalue2
                        "#
                ),
            ),
            (
                "other/.buckconfig.local",
                indoc!(
                    r#"
                            [orange]
                                key = othervalue3
                            [apple]
                                key2 = othervalue5
                                key3 = othervalue4
                        "#
                ),
            ),
        ])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;

        let root_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("root"), &mut file_ops)
            .await?;
        let other_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("other"), &mut file_ops)
            .await?;

        // No local override
        assert_config_value(&root_config, "apple", "key", "value1");
        // local override to new value
        assert_config_value(&root_config, "apple", "key2", "value5");
        // local override new field
        assert_config_value(&root_config, "apple", "key3", "value4");
        // local override new section
        assert_config_value(&root_config, "orange", "key", "value3");

        // No local override
        assert_config_value(&other_config, "apple", "key", "othervalue1");
        // local override to new value
        assert_config_value(&other_config, "apple", "key2", "othervalue5");
        // local override new field
        assert_config_value(&other_config, "apple", "key3", "othervalue4");
        // local override new section
        assert_config_value(&other_config, "orange", "key", "othervalue3");

        Ok(())
    }

    #[tokio::test]
    async fn test_config_arg_with_no_buckconfig() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                        [repositories]
                            root = .
                            other = other
                    "#
            ),
        )])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(
            &mut file_ops,
            &[ConfigOverride::flag_no_cell("some_section.key=value1")],
        )
        .await?;
        let config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("other"), &mut file_ops)
            .await?;

        assert_config_value(&config, "some_section", "key", "value1");

        Ok(())
    }

    #[tokio::test]
    async fn test_cell_config_section_name() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                            [repositories]
                                root = .
                                other = other/
                            [repository_aliases]
                                other_alias = other
                        "#
            ),
        )])?;

        let resolver = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[])
            .await?
            .cell_resolver;

        assert_eq!(
            "other",
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("other_alias")?
                .as_str(),
        );

        Ok(())
    }

    fn initialize_external_cells_impl() {
        struct TestExternalCellsImpl;

        #[async_trait::async_trait]
        impl ExternalCellsImpl for TestExternalCellsImpl {
            async fn get_file_ops_delegate(
                &self,
                _ctx: &mut DiceComputations<'_>,
                _cell_name: CellName,
                _origin: ExternalCellOrigin,
            ) -> buck2_error::Result<Arc<dyn FileOpsDelegate>> {
                // Not used in these tests
                unreachable!()
            }

            fn check_bundled_cell_exists(&self, cell_name: CellName) -> buck2_error::Result<()> {
                if cell_name.as_str() == "test_bundled_cell"
                    || cell_name.as_str() == "prelude"
                    || cell_name.as_str() == "bazel_tools"
                {
                    Ok(())
                } else {
                    Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "No bundled cell with name `{}`",
                        cell_name
                    ))
                }
            }

            async fn expand(
                &self,
                _ctx: &mut DiceComputations<'_>,
                _cell_name: CellName,
                _origin: ExternalCellOrigin,
                _path: &CellRootPath,
            ) -> buck2_error::Result<()> {
                // Not used in these tests
                unreachable!()
            }
        }

        static INIT: std::sync::Once = std::sync::Once::new();

        // Sometimes multiple unittests are run in the same process
        INIT.call_once(|| {
            EXTERNAL_CELLS_IMPL.init(&TestExternalCellsImpl);
        });
    }

    #[tokio::test]
    async fn test_external_cell_configs() -> buck2_error::Result<()> {
        initialize_external_cells_impl();

        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                    [cells]
                        root = .
                        test_bundled_cell = other1/
                        other2 = other2/
                    [cell_aliases]
                        other_alias = test_bundled_cell
                    [external_cells]
                        other_alias = bundled
                "#
            ),
        )])?;

        let resolver = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[])
            .await?
            .cell_resolver;

        let other1 = resolver
            .root_cell_cell_alias_resolver()
            .resolve("other_alias")
            .unwrap();
        let other2 = resolver
            .root_cell_cell_alias_resolver()
            .resolve("other2")
            .unwrap();

        assert_eq!(
            resolver.get(other1).unwrap().external(),
            Some(&ExternalCellOrigin::Bundled(CellName::testing_new(
                "test_bundled_cell"
            ))),
        );
        assert_eq!(resolver.get(other2).unwrap().external(), None,);
        assert_eq!(
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("other_alias")
                .unwrap()
                .as_str(),
            "test_bundled_cell",
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_nested_external_cell_configs() -> buck2_error::Result<()> {
        initialize_external_cells_impl();

        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                    [cells]
                        root = .
                        test_bundled_cell = foo/
                        bar = foo/bar/
                    [external_cells]
                        test_bundled_cell = bundled
                "#
            ),
        )])?;

        BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[])
            .await
            .err()
            .unwrap();

        Ok(())
    }

    #[tokio::test]
    async fn test_missing_bundled_cell() -> buck2_error::Result<()> {
        initialize_external_cells_impl();

        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                    [cells]
                        root = .
                        foo = foo/
                        bar = foo/bar/
                    [external_cells]
                        foo = bundled
                "#
            ),
        )])?;

        let e = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[])
            .await
            .err()
            .unwrap();

        let e = format!("{e:?}");
        assert!(e.contains("No bundled cell"), "error: {e}");

        Ok(())
    }

    #[tokio::test]
    async fn test_git_external_cell() -> buck2_error::Result<()> {
        initialize_external_cells_impl();

        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                    [cells]
                        root = .
                        libfoo = foo/
                    [external_cells]
                        libfoo = git
                    [external_cell_libfoo]
                        git_origin = https://github.com/jeff/libfoo.git
                        commit_hash = aaaaaaaabbbbbbbbccccccccddddddddeeeeeeee
                "#
            ),
        )])?;

        let resolver = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[])
            .await?
            .cell_resolver;

        let instance = resolver.get(CellName::testing_new("libfoo")).unwrap();

        assert_eq!(
            instance.external(),
            Some(&ExternalCellOrigin::Git(GitCellSetup {
                git_origin: "https://github.com/jeff/libfoo.git".into(),
                commit: "aaaaaaaabbbbbbbbccccccccddddddddeeeeeeee".into(),
                object_format: None,
            })),
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_git_external_cell_invalid_sha1() -> buck2_error::Result<()> {
        initialize_external_cells_impl();

        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                    [cells]
                        root = .
                        libfoo = foo/
                    [external_cells]
                        libfoo = git
                    [external_cell_libfoo]
                        git_origin = https://github.com/jeff/libfoo.git
                        commit_hash = abcde
                "#
            ),
        )])?;

        let e = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[])
            .await
            .err()
            .unwrap();

        let e = format!("{e:?}");
        assert!(e.contains("not a valid SHA1 digest"), "error: {e}");

        Ok(())
    }
}
