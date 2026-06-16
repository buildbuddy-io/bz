/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;

use allocative::Allocative;
use bz_core::bz_env;
use bz_core::cells::CellAliasResolver;
use bz_core::cells::CellResolver;
use bz_core::cells::alias::NonEmptyCellAlias;
use bz_core::cells::cell_root_path::CellRootPath;
use bz_core::cells::cell_root_path::CellRootPathBuf;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::external::GitCellSetup;
use bz_core::cells::external::GitObjectFormat;
use bz_core::cells::external::bzlmod_cell_aliases_for_cell;
use bz_core::cells::external::is_bzlmod_cell_name;
use bz_core::cells::external::register_bzlmod_cell_aliases;
use bz_core::cells::external::register_external_cell_origin;
use bz_core::cells::name::CellName;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_error::BuckErrorContext;
use bz_error::bz_error;
use bz_fs::paths::RelativePath;
use bz_fs::paths::abs_path::AbsPath;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use bz_hash::StdBuckHashMap;
use bz_hash::StdBuckHashSet;
use dice::DiceComputations;
use dupe::Dupe;
use futures::FutureExt;
use futures::future::BoxFuture;
use pagable::Pagable;

use crate::bazel::bzlmod::BAZEL_HOST_PLATFORM_CONSTRAINTS;
use crate::bazel::bzlmod::BAZEL_MODULE_FILE;
use crate::bazel::bzlmod::BazelModuleCellAliases;
use crate::bazel::bzlmod::parse_bzlmod_external_cell_origin;
use crate::dice::cells::HasCellResolver;
use crate::dice::data::HasIoProvider;
use crate::dice::progress::dice_state_update_stage;
use crate::external_cells::EXTERNAL_CELLS_IMPL;
use crate::legacy_configs::aggregator::CellsAggregator;
use crate::legacy_configs::args::ResolvedLegacyConfigArg;
use crate::legacy_configs::args::resolve_config_args;
use crate::legacy_configs::args::to_proto_config_args;
use crate::legacy_configs::configs::BazelCompatBazelrcOptions;
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
const BAZEL_PROJECT_ROOT_MARKERS: &[&str] = &["WORKSPACE.bazel", "WORKSPACE"];

/// Buckconfigs can partially be loaded from within dice. However, some parts of what makes up the
/// buckconfig comes from outside the buildgraph, and this type represents those parts.
#[derive(Clone, PartialEq, Eq, Allocative, Pagable)]
pub struct ExternalBuckconfigData {
    // The result of parsing the buckconfigs coming from either global (e.g. /etc/buckconfig.d) or
    // user (e.g. ~/.buckconfig.d or $home_dir/.buckconfig.local) files/dirs outside of the repo
    // The order matters here and reflects the same order these are processed in buck.
    external_path_configs: Vec<ExternalPathBuckconfigData>,
    // The result of parsing the buckconfigs coming from command line args (e.g. --config or --config-file)
    args: Vec<ResolvedLegacyConfigArg>,

    bzlmod_module_aliases: Option<Arc<BazelModuleCellAliases>>,
}

#[derive(PartialEq, Eq, Allocative, Clone, Pagable)]
pub struct ExternalPathBuckconfigData {
    pub(crate) parse_state: LegacyConfigParser,
    pub(crate) origin_path: ConfigPath,
}

impl ExternalBuckconfigData {
    pub fn dice_config_equal(&self, other: &Self) -> bool {
        self.external_path_configs == other.external_path_configs
            && self.args == other.args
            && match (&self.bzlmod_module_aliases, &other.bzlmod_module_aliases) {
                (Some(x), Some(y)) => x.dice_config_equal(y),
                (None, None) => true,
                _ => false,
            }
    }

    pub fn testing_default() -> Self {
        Self {
            external_path_configs: Vec::new(),
            args: Vec::new(),
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
            bzlmod_module_aliases: self.bzlmod_module_aliases,
        }
    }

    async fn get_local_config_components(
        project_root: &ProjectRoot,
    ) -> Vec<bz_data::BuckconfigComponent> {
        use bz_data::buckconfig_component::Data::GlobalExternalConfigFile;
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
                    local_config_components.push(bz_data::BuckconfigComponent {
                        data: Some(GlobalExternalConfigFile(bz_data::GlobalExternalConfig {
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
    ) -> Vec<bz_data::BuckconfigComponent> {
        use bz_data::buckconfig_component::Data::GlobalExternalConfigFile;
        let mut res: Vec<bz_data::BuckconfigComponent> = self
            .external_path_configs
            .clone()
            .into_iter()
            .map(|o| {
                let external_file = bz_data::GlobalExternalConfig {
                    values: o.parse_state.to_proto_external_config_values(false),
                    origin_path: o.origin_path.to_string(),
                };
                bz_data::BuckconfigComponent {
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
#[derive(Clone)]
pub struct BuckConfigBasedCells {
    pub cell_resolver: CellResolver,
    pub root_config: LegacyBuckConfig,
    pub config_paths: StdBuckHashSet<ConfigPath>,
    pub external_data: ExternalBuckconfigData,
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
    ) -> bz_error::Result<CellAliasResolver> {
        self.get_cell_alias_resolver_for_cwd_fast_with_file_ops(
            &mut DefaultConfigParserFileOps {
                project_fs: project_fs.dupe(),
            },
            cwd,
            true, /* apply bazel compatibility defaults */
        )
        .await
    }

    pub async fn get_cell_alias_resolver_for_cwd_immediate_config(
        &self,
        project_fs: &ProjectRoot,
        cwd: &ProjectRelativePath,
    ) -> bz_error::Result<CellAliasResolver> {
        // Immediate config runs in the client before connecting to the daemon. It must not resolve
        // Bazel modules; the daemon command update owns that after file-watcher sync.
        self.get_cell_alias_resolver_for_cwd_fast_with_file_ops(
            &mut DefaultConfigParserFileOps {
                project_fs: project_fs.dupe(),
            },
            cwd,
            false, /* apply bazel compatibility defaults */
        )
        .await
    }

    pub(crate) async fn get_cell_alias_resolver_for_cwd_fast_with_file_ops(
        &self,
        file_ops: &mut dyn ConfigParserFileOps,
        cwd: &ProjectRelativePath,
        apply_bazel_compat_defaults: bool,
    ) -> bz_error::Result<CellAliasResolver> {
        let cell_name = self.cell_resolver.find(cwd);
        let cell_path = self.cell_resolver.get(cell_name)?.path();

        let follow_includes = false;
        let is_bzlmod_cell = is_bzlmod_cell_name(cell_name.as_str());

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
        let apply_bazel_project_defaults = apply_bazel_compat_defaults
            && !is_bzlmod_cell
            && cell_name.as_str() != "bazel_tools"
            && should_apply_bazel_compat_defaults(cell_path, file_ops).await?;
        let config = if apply_bazel_compat_defaults
            && (is_bzlmod_cell
                || cell_name.as_str() == "bazel_tools"
                || apply_bazel_project_defaults)
        {
            let bazelrc_options = if apply_bazel_project_defaults {
                get_bazelrc_options(cell_path, file_ops, &[]).await?
            } else {
                BazelCompatBazelrcOptions::default()
            };
            config.with_bazel_compat_cell_defaults(&[], &[], &bazelrc_options)
        } else {
            config
        };

        if apply_bazel_compat_defaults && (is_bzlmod_cell || cell_name.as_str() == "bazel_tools") {
            return Self::get_bazel_cell_alias_resolver_from_config(
                cell_name,
                &self.cell_resolver,
                &config,
            );
        }

        CellAliasResolver::new_for_non_root_cell(
            cell_name,
            self.cell_resolver.root_cell_cell_alias_resolver(),
            BuckConfigBasedCells::get_cell_aliases_from_config(&config)?,
        )
    }

    pub async fn parse_with_config_args(
        project_fs: &ProjectRoot,
        config_args: &[bz_cli_proto::ConfigOverride],
    ) -> bz_error::Result<Self> {
        Self::parse_with_file_ops_and_options(
            &mut DefaultConfigParserFileOps {
                project_fs: project_fs.dupe(),
            },
            config_args,
            false, /* follow includes */
            true,  /* apply bazel compatibility defaults */
            Some(project_fs),
        )
        .await
    }

    pub async fn parse_for_immediate_config(project_fs: &ProjectRoot) -> bz_error::Result<Self> {
        // Keep the client-side startup parse to real Buck config plus a minimal Bazel root cell.
        // Full MODULE.bazel resolution happens in the daemon command update.
        Self::parse_with_file_ops_and_options(
            &mut DefaultConfigParserFileOps {
                project_fs: project_fs.dupe(),
            },
            &[],
            false, /* follow includes */
            false, /* apply bazel compatibility defaults */
            None,
        )
        .await
    }

    pub fn root_config_with_bazel_compat_startup_defaults(&self) -> LegacyBuckConfig {
        self.root_config.with_bazel_compat_startup_defaults()
    }

    pub async fn testing_parse_with_file_ops(
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[bz_cli_proto::ConfigOverride],
    ) -> bz_error::Result<Self> {
        Self::parse_with_file_ops_and_options(
            file_ops,
            config_args,
            true, /* follow includes */
            true, /* apply bazel compatibility defaults */
            None,
        )
        .await
    }

    async fn parse_with_file_ops_and_options(
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[bz_cli_proto::ConfigOverride],
        follow_includes: bool,
        apply_bazel_compat_defaults: bool,
        persistent_cache_project_fs: Option<&ProjectRoot>,
    ) -> bz_error::Result<Self> {
        Self::parse_with_file_ops_and_options_inner(
            file_ops,
            config_args,
            follow_includes,
            apply_bazel_compat_defaults,
            persistent_cache_project_fs,
        )
        .await
        .buck_error_context("Parsing cells")
    }

    async fn parse_with_file_ops_and_options_inner(
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[bz_cli_proto::ConfigOverride],
        follow_includes: bool,
        apply_bazel_compat_defaults: bool,
        _persistent_cache_project_fs: Option<&ProjectRoot>,
    ) -> bz_error::Result<Self> {
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
            ) -> bz_error::Result<Option<Vec<String>>> {
                self.trace.insert(path.clone());
                self.inner.read_file_lines_if_exists(path).await
            }

            async fn read_dir(
                &mut self,
                path: &ConfigPath,
            ) -> bz_error::Result<Vec<ConfigDirEntry>> {
                self.trace.insert(path.clone());
                self.inner.read_dir(path).await
            }

            fn resolve_project_relative_to_absolute(
                &self,
                base: &ProjectRelativePath,
                path: &RelativePath,
            ) -> bz_error::Result<Option<bz_fs::paths::abs_path::AbsPathBuf>> {
                self.inner.resolve_project_relative_to_absolute(base, path)
            }
        }

        let mut file_ops = TracingFileOps {
            inner: file_ops,
            trace: Default::default(),
        };

        // NOTE: This will _not_ perform IO unless it needs to.
        let processed_config_args = dice_state_update_stage("resolving buckconfig args", async {
            resolve_config_args(config_args, &mut file_ops).await
        })
        .await?;

        let started_parse = dice_state_update_stage("reading external buckconfigs", async {
            let external_paths = get_external_buckconfig_paths(&mut file_ops).await?;
            LegacyBuckConfig::start_parse_for_external_files(
                &external_paths,
                &mut file_ops,
                follow_includes,
            )
            .await
        })
        .await?;

        let root_path = CellRootPathBuf::new(ProjectRelativePath::empty().to_owned());

        let root_config = dice_state_update_stage("reading project buckconfigs", async {
            let buckconfig_paths = get_project_buckconfig_paths(&root_path, &mut file_ops).await?;
            LegacyBuckConfig::finish_parse(
                started_parse.clone(),
                buckconfig_paths.as_slice(),
                &root_path,
                &mut file_ops,
                &processed_config_args,
                follow_includes,
            )
            .await
        })
        .await?;
        let bzlmod_module_aliases = None;
        let bazel_compat_project_root =
            dice_state_update_stage("detecting bazel compatibility", async {
                should_apply_bazel_compat_defaults(&root_path, &mut file_ops).await
            })
            .await?;
        let root_config = if apply_bazel_compat_defaults && bazel_compat_project_root {
            let bazelrc_options = dice_state_update_stage("reading bazelrc options", async {
                get_bazelrc_options(&root_path, &mut file_ops, &processed_config_args).await
            })
            .await?;
            let root_config =
                root_config.with_bazel_compat_defaults(&[], &[], &[], &bazelrc_options);
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
        if cell_definitions.is_empty() && !apply_bazel_compat_defaults && bazel_compat_project_root
        {
            // The client needs a root cell to resolve cwd-relative paths in Bazel repos, but not
            // external module aliases or toolchain defaults.
            cell_definitions.push((CellName::unchecked_new("root")?, root_path.clone()));
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
                bzlmod_module_aliases,
            },
        })
    }

    pub(crate) fn get_cell_aliases_from_config(
        config: &LegacyBuckConfig,
    ) -> bz_error::Result<impl Iterator<Item = (NonEmptyCellAlias, NonEmptyCellAlias)> + use<>>
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

    pub(crate) fn get_bazel_cell_alias_resolver_from_config(
        cell_name: CellName,
        cell_resolver: &CellResolver,
        config: &LegacyBuckConfig,
    ) -> bz_error::Result<CellAliasResolver> {
        let mut aliases = StdBuckHashMap::default();
        for alias in ["root", "prelude", "bazel_tools"] {
            let alias = NonEmptyCellAlias::new(alias.to_owned())?;
            let destination = if alias.as_str() == "root" {
                cell_resolver.root_cell()
            } else {
                CellName::unchecked_new(alias.as_str())?
            };
            if cell_resolver.get(destination).is_err() {
                continue;
            }
            aliases.insert(alias, destination);
        }
        for (alias, destination) in Self::get_cell_aliases_from_config(config)? {
            if alias.as_str() == "bazel_tools" {
                continue;
            }
            let destination = CellName::unchecked_new(destination.as_str())?;
            if !is_bzlmod_cell_name(destination.as_str()) {
                cell_resolver.get(destination)?;
            }
            aliases.insert(alias, destination);
        }
        for (alias, destination) in bzlmod_cell_aliases_for_cell(cell_name.as_str()) {
            if alias == "bazel_tools" {
                continue;
            }
            aliases.insert(
                NonEmptyCellAlias::new(alias)?,
                CellName::unchecked_new(&destination)?,
            );
        }
        CellAliasResolver::new(cell_name, aliases)
    }

    pub(crate) async fn parse_single_cell_with_dice_for_cell(
        ctx: &mut DiceComputations<'_>,
        cell_name: CellName,
        cell_path: &CellRootPath,
    ) -> bz_error::Result<LegacyBuckConfig> {
        let resolver = ctx.get_cell_resolver().await?;
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let external_data = ctx.get_injected_external_buckconfig_data().await?;
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
    ) -> bz_error::Result<LegacyBuckConfig> {
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
    ) -> bz_error::Result<LegacyBuckConfig> {
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
    ) -> bz_error::Result<LegacyBuckConfig> {
        let is_bzlmod_cell = is_bzlmod_cell_name(cell_name);
        if is_bzlmod_cell || cell_name == "bazel_tools" {
            return Ok(LegacyBuckConfig::empty().with_bazel_compat_cell_defaults(
                &[],
                &[],
                &BazelCompatBazelrcOptions::default(),
            ));
        }

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

        let apply_bazel_project_defaults =
            should_apply_bazel_compat_defaults(cell_path, file_ops).await?;
        if apply_bazel_project_defaults {
            let bazelrc_options = if apply_bazel_project_defaults {
                get_bazelrc_options(cell_path, file_ops, external_data.args.as_ref()).await?
            } else {
                BazelCompatBazelrcOptions::default()
            };
            Ok(config.with_bazel_compat_cell_defaults(&[], &[], &bazelrc_options))
        } else {
            Ok(config)
        }
    }

    fn parse_external_cell_origin(
        cell: CellName,
        value: &str,
        config: &LegacyBuckConfig,
    ) -> bz_error::Result<ExternalCellOrigin> {
        #[derive(bz_error::Error, Debug)]
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
        } else if let Some(origin) = parse_bzlmod_external_cell_origin(&cell, value, config)? {
            Ok(origin)
        } else {
            Err(ExternalCellOriginParseError::Unknown(value.to_owned()).into())
        }
    }
}

async fn config_file_exists(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
    file: &str,
) -> bz_error::Result<bool> {
    let file = ForwardRelativePath::new(file)?;
    let path = ConfigPath::Project(cell_path.as_project_relative_path().join(file));
    Ok(file_ops.read_file_lines_if_exists(&path).await?.is_some())
}

async fn should_apply_bazel_compat_defaults(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> bz_error::Result<bool> {
    if config_file_exists(cell_path, file_ops, BAZEL_MODULE_FILE).await? {
        return Ok(true);
    }

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

#[derive(Debug)]
struct BazelrcRecord {
    command: String,
    args: Vec<String>,
}

fn bazelrc_tokenize(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if let Some(quote_char) = quote {
            if c == quote_char {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '#' => break,
            '\'' | '"' => quote = Some(c),
            c if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn bazelrc_import_path(
    root_path: &CellRootPath,
    file_ops: &dyn ConfigParserFileOps,
    current_path: &ConfigPath,
    import_path: &str,
) -> bz_error::Result<Option<ConfigPath>> {
    if let Some(path) = import_path.strip_prefix("%workspace%/") {
        let path = RelativePath::new(path);
        let root_path = root_path.as_project_relative_path();
        return match root_path.join_normalized(path) {
            Ok(path) => Ok(Some(ConfigPath::Project(path))),
            Err(_) => file_ops
                .resolve_project_relative_to_absolute(root_path, path)
                .map(|path| path.map(ConfigPath::Global)),
        };
    }
    if import_path == "%workspace%" {
        return Ok(Some(ConfigPath::Project(
            root_path.as_project_relative_path().to_buf(),
        )));
    }
    if let Ok(path) = AbsPath::new(import_path) {
        return Ok(Some(ConfigPath::Global(path.to_owned())));
    }
    current_path
        .join_to_parent_normalized(RelativePath::new(import_path))
        .map(Some)
}

fn collect_bazelrc_records<'a>(
    root_path: &'a CellRootPath,
    file_ops: &'a mut dyn ConfigParserFileOps,
    path: ConfigPath,
    required: bool,
    visited: &'a mut BTreeSet<String>,
    records: &'a mut Vec<BazelrcRecord>,
) -> BoxFuture<'a, bz_error::Result<()>> {
    async move {
        let key = path.to_string();
        if !visited.insert(key) {
            return Ok(());
        }
        let Some(lines) = file_ops.read_file_lines_if_exists(&path).await? else {
            if required {
                return Err(bz_error!(
                    bz_error::ErrorTag::Input,
                    "Bazel rc file `{}` does not exist",
                    path
                ));
            }
            return Ok(());
        };
        for line in lines {
            let words = bazelrc_tokenize(&line);
            let Some((directive, args)) = words.split_first() else {
                continue;
            };
            match directive.as_str() {
                "import" | "try-import" => {
                    let Some(import_path) = args.first() else {
                        continue;
                    };
                    let import_required = directive == "import";
                    let Some(import_path) =
                        bazelrc_import_path(root_path, file_ops, &path, import_path)?
                    else {
                        if import_required {
                            return Err(bz_error!(
                                bz_error::ErrorTag::Input,
                                "Bazel rc import `{}` in `{}` could not be resolved",
                                import_path,
                                path
                            ));
                        }
                        continue;
                    };
                    collect_bazelrc_records(
                        root_path,
                        file_ops,
                        import_path,
                        import_required,
                        visited,
                        records,
                    )
                    .await?;
                }
                _ => records.push(BazelrcRecord {
                    command: directive.clone(),
                    args: args.to_vec(),
                }),
            }
        }
        Ok(())
    }
    .boxed()
}

fn bazelrc_host_config() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        ""
    }
}

fn bazelrc_record_applies(record: &BazelrcRecord, active_configs: &BTreeSet<String>) -> bool {
    let (command, config) = record
        .command
        .split_once(':')
        .map_or((record.command.as_str(), ""), |(command, config)| {
            (command, config)
        });
    if command != "common" && command != "build" {
        return false;
    }
    config.is_empty() || active_configs.contains(config)
}

fn bazelrc_arg_value(args: &[String], index: &mut usize, name: &str) -> Option<String> {
    let arg = &args[*index];
    let prefix = format!("{name}=");
    if let Some(value) = arg.strip_prefix(&prefix) {
        return Some(value.to_owned());
    }
    if arg == name {
        *index += 1;
        return args.get(*index).cloned();
    }
    None
}

fn bazelrc_command_line_build_setting_entry(kind: &str, key: &str, value: &str) -> String {
    format!("{kind}\t{key}\t{value}")
}

fn bazelrc_native_command_line_string_option(name: &str) -> bool {
    matches!(
        name,
        "cpu"
            | "host_cpu"
            | "java_language_version"
            | "tool_java_language_version"
            | "java_runtime_version"
            | "tool_java_runtime_version"
            | "experimental_one_version_enforcement"
    )
}

fn bazelrc_native_command_line_list_option(name: &str) -> bool {
    matches!(
        name,
        "javacopt"
            | "host_javacopt"
            | "platforms"
            | "extra_execution_platforms"
            | "extra_toolchains"
    )
}

fn bazelrc_native_command_line_comma_separated_list_option(name: &str) -> bool {
    matches!(
        name,
        "platforms" | "extra_execution_platforms" | "extra_toolchains"
    )
}

fn bazelrc_command_line_list_build_setting_entries(
    name: &str,
    key: &str,
    value: &str,
) -> Vec<String> {
    if !bazelrc_native_command_line_comma_separated_list_option(name) {
        return vec![bazelrc_command_line_build_setting_entry("list", key, value)];
    }

    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| bazelrc_command_line_build_setting_entry("list", key, value))
        .collect()
}

fn bazelrc_label_build_setting_key(value: &str) -> bool {
    value.starts_with('@') || value.starts_with("//") || value.starts_with(':')
}

fn bazelrc_command_line_build_setting(args: &[String], index: &mut usize) -> Option<Vec<String>> {
    let arg = &args[*index];
    let option = arg.strip_prefix("--")?;
    if option.is_empty() {
        return None;
    }

    let (negated, option) = if let Some(option) = option.strip_prefix("no") {
        (true, option)
    } else {
        (false, option)
    };

    if bazelrc_label_build_setting_key(option) {
        let (key, value) = option
            .split_once('=')
            .map_or((option, None), |(key, value)| (key, Some(value)));
        if !bazelrc_label_build_setting_key(key) {
            return None;
        }
        return Some(vec![match value {
            Some("true" | "True" | "1") => {
                bazelrc_command_line_build_setting_entry("bool", key, "true")
            }
            Some("false" | "False" | "0") => {
                bazelrc_command_line_build_setting_entry("bool", key, "false")
            }
            Some(value) => bazelrc_command_line_build_setting_entry("string", key, value),
            None => bazelrc_command_line_build_setting_entry(
                "bool",
                key,
                if negated { "false" } else { "true" },
            ),
        }]);
    }

    if negated {
        return None;
    }

    let (name, value) = option
        .split_once('=')
        .map_or((option, None), |(name, value)| {
            (name, Some(value.to_owned()))
        });
    if !bazelrc_native_command_line_string_option(name)
        && !bazelrc_native_command_line_list_option(name)
    {
        return None;
    }
    let value = match value {
        Some(value) => value,
        None => {
            *index += 1;
            args.get(*index)?.clone()
        }
    };
    let key = format!("//command_line_option:{name}");
    Some(if bazelrc_native_command_line_list_option(name) {
        bazelrc_command_line_list_build_setting_entries(name, &key, &value)
    } else {
        vec![bazelrc_command_line_build_setting_entry(
            "string", &key, &value,
        )]
    })
}

fn bazelrc_add_options(
    options: &mut BazelCompatBazelrcOptions,
    args: &[String],
    collect_options: bool,
    active_configs: &mut BTreeSet<String>,
) -> bool {
    let mut changed = false;
    let mut index = 0;
    while index < args.len() {
        if let Some(config) = bazelrc_arg_value(args, &mut index, "--config") {
            changed |= active_configs.insert(config);
        } else if collect_options {
            if let Some(value) = bazelrc_arg_value(args, &mut index, "--copt") {
                options.copt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--conlyopt") {
                options.conlyopt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--cxxopt") {
                options.cxxopt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--linkopt") {
                options.linkopt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--host_copt") {
                options.host_copt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--host_conlyopt") {
                options.host_conlyopt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--host_cxxopt") {
                options.host_cxxopt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--host_linkopt") {
                options.host_linkopt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--per_file_copt") {
                options.per_file_copt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--macos_minimum_os") {
                options.macos_minimum_os.push(value);
            } else if let Some(value) =
                bazelrc_arg_value(args, &mut index, "--host_macos_minimum_os")
            {
                options.host_macos_minimum_os.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--repo_env") {
                options.repo_env.push(value);
            } else if let Some(values) = bazelrc_command_line_build_setting(args, &mut index) {
                options.command_line_build_settings.extend(values);
            }
        }
        index += 1;
    }
    changed
}

fn bazelrc_host_config_from_host_platform_constraints(value: &str) -> Option<&'static str> {
    for constraint in value.lines().map(str::trim) {
        match constraint.strip_prefix("@platforms//os:") {
            Some("linux") => return Some("linux"),
            Some("osx" | "macos") => return Some("macos"),
            Some("windows") => return Some("windows"),
            _ => {}
        }
    }
    None
}

fn bazelrc_host_config_from_config_args(
    config_args: &[ResolvedLegacyConfigArg],
) -> Option<&'static str> {
    config_args.iter().rev().find_map(|arg| match arg {
        ResolvedLegacyConfigArg::Flag(flag)
            if flag.cell.is_none()
                && flag.section == "bazel"
                && flag.key == BAZEL_HOST_PLATFORM_CONSTRAINTS =>
        {
            flag.value
                .as_deref()
                .and_then(bazelrc_host_config_from_host_platform_constraints)
        }
        _ => None,
    })
}

fn bazelrc_options_from_records(
    records: &[BazelrcRecord],
    host_config_override: Option<&'static str>,
) -> BazelCompatBazelrcOptions {
    let mut enable_platform_specific_config = false;
    for record in records {
        let (command, config) = record
            .command
            .split_once(':')
            .map_or((record.command.as_str(), ""), |(command, config)| {
                (command, config)
            });
        if (command == "common" || command == "build") && config.is_empty() {
            for arg in &record.args {
                if arg == "--enable_platform_specific_config"
                    || arg == "--enable_platform_specific_config=true"
                {
                    enable_platform_specific_config = true;
                } else if arg == "--noenable_platform_specific_config"
                    || arg == "--enable_platform_specific_config=false"
                {
                    enable_platform_specific_config = false;
                }
            }
        }
    }

    let mut active_configs = BTreeSet::new();
    if enable_platform_specific_config {
        let host_config = host_config_override.unwrap_or_else(bazelrc_host_config);
        if !host_config.is_empty() {
            active_configs.insert(host_config.to_owned());
        }
    }

    loop {
        let mut changed = false;
        let mut ignored_options = BazelCompatBazelrcOptions::default();
        for record in records {
            if bazelrc_record_applies(record, &active_configs) {
                changed |= bazelrc_add_options(
                    &mut ignored_options,
                    &record.args,
                    false,
                    &mut active_configs,
                );
            }
        }
        if !changed {
            break;
        }
    }

    let mut options = BazelCompatBazelrcOptions::default();
    for record in records {
        if bazelrc_record_applies(record, &active_configs) {
            bazelrc_add_options(&mut options, &record.args, true, &mut active_configs);
        }
    }
    options
}

async fn get_bazelrc_options(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
    config_args: &[ResolvedLegacyConfigArg],
) -> bz_error::Result<BazelCompatBazelrcOptions> {
    let root_bazelrc = ConfigPath::Project(
        cell_path
            .as_project_relative_path()
            .join(ForwardRelativePath::new(".bazelrc")?),
    );
    let mut visited = BTreeSet::new();
    let mut records = Vec::new();
    collect_bazelrc_records(
        cell_path,
        file_ops,
        root_bazelrc,
        false,
        &mut visited,
        &mut records,
    )
    .await?;
    Ok(bazelrc_options_from_records(
        &records,
        bazelrc_host_config_from_config_args(config_args),
    ))
}

async fn get_external_buckconfig_paths(
    file_ops: &mut dyn ConfigParserFileOps,
) -> bz_error::Result<Vec<ConfigPath>> {
    let skip_default_external_config = bz_env!(
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
        bz_env!("BUCK2_TEST_EXTRA_EXTERNAL_CONFIG", applicability = testing)?;

    if let Some(f) = extra_external_config {
        buckconfig_paths.push(ConfigPath::Global(AbsPath::new(f)?.to_owned()));
    }

    Ok(buckconfig_paths)
}

async fn get_project_buckconfig_paths(
    path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> bz_error::Result<Vec<ConfigPath>> {
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

    use bz_cli_proto::ConfigOverride;
    use bz_core::cells::cell_root_path::CellRootPath;
    use bz_core::cells::cell_root_path::CellRootPathBuf;
    use bz_core::cells::external::ExternalCellOrigin;
    use bz_core::cells::external::GitCellSetup;
    use bz_core::cells::name::CellName;
    use bz_core::fs::project_rel_path::ProjectRelativePath;
    use dice::DiceComputations;
    use indoc::indoc;

    use crate::external_cells::EXTERNAL_CELLS_IMPL;
    use crate::external_cells::ExternalCellsImpl;
    use crate::file_ops::delegate::FileOpsDelegate;
    use crate::legacy_configs::args::ResolvedConfigFlag;
    use crate::legacy_configs::cells::BuckConfigBasedCells;
    use crate::legacy_configs::configs::testing::TestConfigParserFileOps;
    use crate::legacy_configs::configs::tests::assert_config_value;
    use crate::legacy_configs::key::BuckconfigKeyRef;
    use bz_core::cells::external::bzlmod_cell_name;

    #[tokio::test]
    async fn test_bazelrc_workspace_import_normalizes_path() -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".bazelrc",
                "try-import %workspace%/configs/../imported.bazelrc\n",
            ),
            ("imported.bazelrc", "build --copt=-DFROM_IMPORTED\n"),
        ])?;

        let options =
            super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops, &[]).await?;

        assert_eq!(options.copt, vec!["-DFROM_IMPORTED"]);
        Ok(())
    }

    #[tokio::test]
    async fn test_bazelrc_bazel_native_configuration_flags() -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".bazelrc",
            "build --cpu=k8 --host_cpu=k8 --platforms=//platforms:linux,@platforms//cpu:x86_64 --extra_execution_platforms=@toolchains//platforms:linux_x86_64 --extra_toolchains=@toolchains//cc:linux_x86_64 --linkopt=-Wl,-z,now --host_linkopt=-no-pie --javacopt=-Akey=a,b\n",
        )])?;

        let options =
            super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops, &[]).await?;

        assert_eq!(options.linkopt, vec!["-Wl,-z,now"]);
        assert_eq!(options.host_linkopt, vec!["-no-pie"]);
        assert_eq!(
            options.command_line_build_settings,
            vec![
                "string\t//command_line_option:cpu\tk8",
                "string\t//command_line_option:host_cpu\tk8",
                "list\t//command_line_option:platforms\t//platforms:linux",
                "list\t//command_line_option:platforms\t@platforms//cpu:x86_64",
                "list\t//command_line_option:extra_execution_platforms\t@toolchains//platforms:linux_x86_64",
                "list\t//command_line_option:extra_toolchains\t@toolchains//cc:linux_x86_64",
                "list\t//command_line_option:javacopt\t-Akey=a,b",
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_bazelrc_repo_env() -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".bazelrc",
            "common --repo_env=BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN=1 --repo_env=INHERITED --repo_env==UNSET\n",
        )])?;

        let options =
            super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops, &[]).await?;

        assert_eq!(
            options.repo_env,
            vec!["BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN=1", "INHERITED", "=UNSET",]
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_bazelrc_platform_specific_config_uses_configured_host_platform()
    -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".bazelrc",
            indoc!(
                r#"
                common --enable_platform_specific_config=true
                common:linux --cxxopt=-DLINUX
                common:macos --macos_minimum_os=12.0
                "#
            ),
        )])?;
        let config_args = vec![super::ResolvedLegacyConfigArg::Flag(ResolvedConfigFlag {
            section: "bazel".to_owned(),
            key: super::BAZEL_HOST_PLATFORM_CONSTRAINTS.to_owned(),
            value: Some("@platforms//cpu:x86_64\n@platforms//os:linux".to_owned()),
            cell: None,
        })];

        let options =
            super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops, &config_args)
                .await?;

        assert_eq!(options.cxxopt, vec!["-DLINUX"]);
        assert!(options.macos_minimum_os.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_bazelrc_try_import_workspace_path_outside_project_is_optional()
    -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".bazelrc",
            "try-import %workspace%/../../internal_tools/preset.bazelrc\nbuild --copt=-DLOCAL\n",
        )])?;

        let options =
            super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops, &[]).await?;

        assert_eq!(options.copt, vec!["-DLOCAL"]);
        Ok(())
    }

    #[tokio::test]
    async fn test_bazelrc_import_workspace_path_outside_project_is_required() {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".bazelrc",
            "import %workspace%/../../internal_tools/preset.bazelrc\n",
        )])
        .unwrap();

        let result =
            super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops, &[]).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cells() -> bz_error::Result<()> {
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
                true,
            )
            .await?;

        assert_eq!("other", tp_resolver.resolve("other_alias")?.as_str());

        Ok(())
    }

    #[tokio::test]
    async fn test_bazel_compat_defaults_without_buckconfig() -> bz_error::Result<()> {
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
            &bzlmod_cell_name("rules_go+0.57.0"),
            resolver
                .root_cell_cell_alias_resolver()
                .resolve("rules_go")?
                .as_str()
        );
        assert_eq!(
            &bzlmod_cell_name("rules_go+0.57.0"),
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
                true,
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
        assert_eq!(
            root_config.get(BuckconfigKeyRef {
                section: "parser",
                property: "target_platform_detector_spec",
            }),
            Some("target:bz//...->platforms//host:host"),
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_module_bazel_enables_bazel_compat_defaults_with_buckconfig()
    -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".buckconfig",
                indoc!(
                    r#"
                    [cells]
                        bz = .
                        prelude = prelude

                    [cell_aliases]
                        root = bz
                "#
                ),
            ),
            (
                "MODULE.bazel",
                indoc!(
                    r#"
                    module(name = "hello", repo_name = "hello_root")
                "#
                ),
            ),
        ])?;

        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;
        let root_config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("bz"), &mut file_ops)
            .await?;

        assert_eq!(
            root_config.get(BuckconfigKeyRef {
                section: "cells",
                property: "root",
            }),
            None,
        );
        assert_eq!(
            root_config.get(BuckconfigKeyRef {
                section: "cell_aliases",
                property: "root",
            }),
            Some("bz"),
        );
        assert_eq!(
            root_config.get(BuckconfigKeyRef {
                section: "bazel",
                property: "compatibility",
            }),
            Some("true"),
        );
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

    #[tokio::test]
    async fn test_bazel_cell_alias_resolver_preserves_magic_bazel_tools() -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                    [cells]
                        root = .
                        prelude = prelude
                        bazel_tools = bazel_tools
                        bzlmod_wrong = buck-out/v2/external_cells/bzlmod/wrong+

                    [cell_aliases]
                        bazel_tools = bzlmod_wrong
                "#
            ),
        )])?;
        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;
        let config = cells
            .parse_single_cell_with_file_ops(CellName::testing_new("root"), &mut file_ops)
            .await?;
        let resolver = BuckConfigBasedCells::get_bazel_cell_alias_resolver_from_config(
            CellName::testing_new("root"),
            &cells.cell_resolver,
            &config,
        )?;

        assert_eq!("bazel_tools", resolver.resolve("bazel_tools")?.as_str());

        Ok(())
    }

    #[tokio::test]
    async fn test_bazel_cell_alias_resolver_uses_actual_root_cell() -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                    [cells]
                        bz = .
                        prelude = prelude

                    [cell_aliases]
                        root = bz
                "#
            ),
        )])?;
        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;
        let root_cell = cells.cell_resolver.root_cell();
        let config = cells
            .parse_single_cell_with_file_ops(root_cell, &mut file_ops)
            .await?;
        let resolver = BuckConfigBasedCells::get_bazel_cell_alias_resolver_from_config(
            root_cell,
            &cells.cell_resolver,
            &config,
        )?;

        assert_eq!("bz", resolver.resolve("root")?.as_str());
        assert!(resolver.resolve("bazel_tools").is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_bazel_cell_alias_resolver_includes_bzlmod_root_aliases() -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                    [cells]
                        bz_test_root = .
                        prelude = prelude
                "#
            ),
        )])?;
        let cells = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await?;
        let root_cell = cells.cell_resolver.root_cell();
        register_bzlmod_cell_aliases(
            root_cell.as_str(),
            [(
                "io_test_rules_docker".to_owned(),
                "rules_docker+".to_owned(),
            )],
        );
        let config = cells
            .parse_single_cell_with_file_ops(root_cell, &mut file_ops)
            .await?;
        let resolver = BuckConfigBasedCells::get_bazel_cell_alias_resolver_from_config(
            root_cell,
            &cells.cell_resolver,
            &config,
        )?;

        assert_eq!(
            "rules_docker+",
            resolver.resolve("io_test_rules_docker")?.as_str()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_buckconfig_wins_over_bazel_compat_defaults() -> bz_error::Result<()> {
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
    async fn test_multi_cell_with_config_file() -> bz_error::Result<()> {
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
    async fn test_multi_cell_no_repositories_in_non_root_cell() -> bz_error::Result<()> {
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
    async fn test_multi_cell_with_cell_relative() -> bz_error::Result<()> {
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
    async fn test_local_config_file_overwrite_config_file() -> bz_error::Result<()> {
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
    async fn test_multi_cell_local_config_file_overwrite_config_file() -> bz_error::Result<()> {
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
    async fn test_config_arg_with_no_buckconfig() -> bz_error::Result<()> {
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
    async fn test_cell_config_section_name() -> bz_error::Result<()> {
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
            ) -> bz_error::Result<Arc<dyn FileOpsDelegate>> {
                // Not used in these tests
                unreachable!()
            }

            fn check_bundled_cell_exists(&self, cell_name: CellName) -> bz_error::Result<()> {
                if cell_name.as_str() == "test_bundled_cell"
                    || cell_name.as_str() == "prelude"
                    || cell_name.as_str() == "bazel_tools"
                {
                    Ok(())
                } else {
                    Err(bz_error::bz_error!(
                        bz_error::ErrorTag::Input,
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
            ) -> bz_error::Result<()> {
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
    async fn test_external_cell_configs() -> bz_error::Result<()> {
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
    async fn test_nested_external_cell_configs() -> bz_error::Result<()> {
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
    async fn test_missing_bundled_cell() -> bz_error::Result<()> {
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
    async fn test_git_external_cell() -> bz_error::Result<()> {
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
    async fn test_git_external_cell_invalid_sha1() -> bz_error::Result<()> {
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
