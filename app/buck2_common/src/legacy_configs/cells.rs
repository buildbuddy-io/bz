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
use std::future::Future;
use std::io::ErrorKind;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::bzlmod_archive::ArchiveKind;
use crate::bzlmod_archive::archive_kind_from_type_or_url;
use crate::bzlmod_archive::extract_archive;
use allocative::Allocative;
use buck2_core::buck2_env;
use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::CellResolver;
use buck2_core::cells::alias::NonEmptyCellAlias;
use buck2_core::cells::cell_root_path::CellRootPath;
use buck2_core::cells::cell_root_path::CellRootPathBuf;
use buck2_core::cells::external::BZLMOD_BAZEL_COMPAT_VERSION;
use buck2_core::cells::external::BZLMOD_EXTERNAL_CELL_KIND;
use buck2_core::cells::external::BZLMOD_GENERATED_EXTERNAL_CELL_KIND;
use buck2_core::cells::external::BzlmodBazelFeaturesGlobalsSetup;
use buck2_core::cells::external::BzlmodBazelFeaturesVersionSetup;
use buck2_core::cells::external::BzlmodCellSetup;
use buck2_core::cells::external::BzlmodGeneratedCellGenerator;
use buck2_core::cells::external::BzlmodGeneratedCellSetup;
use buck2_core::cells::external::BzlmodHostPlatformSetup;
use buck2_core::cells::external::BzlmodModuleExtensionRepoSetup;
use buck2_core::cells::external::BzlmodOverlay;
use buck2_core::cells::external::BzlmodPatch;
use buck2_core::cells::external::BzlmodRepositoryRuleInvocationSetup;
use buck2_core::cells::external::BzlmodShellConfigSetup;
use buck2_core::cells::external::BzlmodXcodeConfigSetup;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::external::GitCellSetup;
use buck2_core::cells::external::GitObjectFormat;
use buck2_core::cells::external::bzlmod_cell_aliases_for_cell;
use buck2_core::cells::external::bzlmod_cell_name;
use buck2_core::cells::external::is_bzlmod_cell_name;
use buck2_core::cells::external::register_bzlmod_cell_aliases_from_refs;
use buck2_core::cells::external::register_bzlmod_cell_canonical_repo_name_for_cell;
use buck2_core::cells::external::register_bzlmod_module_extension_usages_json;
use buck2_core::cells::external::register_external_cell_origin;
use buck2_core::cells::name::CellName;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_error::BuckErrorContext;
use buck2_error::buck2_error;
use buck2_error::conversion::from_any_with_tag;
use buck2_fs::paths::RelativePath;
use buck2_fs::paths::abs_path::AbsPath;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_hash::StdBuckHashMap;
use buck2_hash::StdBuckHashSet;
use buck2_http::HttpClient;
use buck2_http::HttpClientBuilder;
use buck2_http::retries::HttpError as RetryingHttpError;
use buck2_http::retries::HttpErrorForRetry;
use buck2_http::retries::IntoBuck2Error;
use buck2_http::retries::http_retry;
use buck2_util::late_binding::LateBinding;
use derive_more::Display;
use dice::DiceComputations;
use dice::DiceTransactionUpdater;
use dice::InjectedKey;
use dice::Key;
use dice::NoValueSerialize;
use dice::UserComputationData;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use pagable::Pagable;
use pagable::pagable_typetag;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::bzlmod_integrity::parse_bzlmod_integrity;
use crate::dice::cells::HasCellResolver;
use crate::dice::data::HasIoProvider;
use crate::external_cells::EXTERNAL_CELLS_IMPL;
use crate::legacy_configs::aggregator::CellsAggregator;
use crate::legacy_configs::args::ResolvedLegacyConfigArg;
use crate::legacy_configs::args::resolve_config_args;
use crate::legacy_configs::args::to_proto_config_args;
use crate::legacy_configs::configs::BazelCompatBazelrcOptions;
use crate::legacy_configs::configs::BazelCompatCellAlias;
use crate::legacy_configs::configs::BazelCompatExternalModule;
use crate::legacy_configs::configs::BazelCompatGeneratedModule;
use crate::legacy_configs::configs::BazelCompatRegistryModule;
use crate::legacy_configs::configs::LegacyBuckConfig;
use crate::legacy_configs::dice::HasInjectedLegacyConfigs;
use crate::legacy_configs::dice::HasLegacyConfigs;
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
const BAZEL_MODULE_FILE: &str = "MODULE.bazel";
const BAZEL_PROJECT_ROOT_MARKERS: &[&str] = &["WORKSPACE.bazel", "WORKSPACE"];

async fn buckconfig_load_stage<T, Fut>(stage: impl Into<String>, fut: Fut) -> buck2_error::Result<T>
where
    Fut: Future<Output = buck2_error::Result<T>>,
{
    buck2_events::dispatch::span_async(
        buck2_data::DiceStateUpdateStageStart {
            stage: stage.into(),
        },
        async { (fut.await, buck2_data::DiceStateUpdateStageEnd {}) },
    )
    .await
}

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
    ) -> buck2_error::Result<CellAliasResolver> {
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
    ) -> buck2_error::Result<CellAliasResolver> {
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
    ) -> buck2_error::Result<CellAliasResolver> {
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
                get_bazelrc_options(cell_path, file_ops).await?
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
        config_args: &[buck2_cli_proto::ConfigOverride],
    ) -> buck2_error::Result<Self> {
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

    pub async fn parse_for_immediate_config(project_fs: &ProjectRoot) -> buck2_error::Result<Self> {
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
        config_args: &[buck2_cli_proto::ConfigOverride],
    ) -> buck2_error::Result<Self> {
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
        config_args: &[buck2_cli_proto::ConfigOverride],
        follow_includes: bool,
        apply_bazel_compat_defaults: bool,
        persistent_cache_project_fs: Option<&ProjectRoot>,
    ) -> buck2_error::Result<Self> {
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
        config_args: &[buck2_cli_proto::ConfigOverride],
        follow_includes: bool,
        apply_bazel_compat_defaults: bool,
        _persistent_cache_project_fs: Option<&ProjectRoot>,
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
                self.trace.insert(path.clone());
                self.inner.read_file_lines_if_exists(path).await
            }

            async fn read_dir(
                &mut self,
                path: &ConfigPath,
            ) -> buck2_error::Result<Vec<ConfigDirEntry>> {
                self.trace.insert(path.clone());
                self.inner.read_dir(path).await
            }

            fn resolve_project_relative_to_absolute(
                &self,
                base: &ProjectRelativePath,
                path: &RelativePath,
            ) -> buck2_error::Result<Option<buck2_fs::paths::abs_path::AbsPathBuf>> {
                self.inner.resolve_project_relative_to_absolute(base, path)
            }
        }

        let mut file_ops = TracingFileOps {
            inner: file_ops,
            trace: Default::default(),
        };

        // NOTE: This will _not_ perform IO unless it needs to.
        let processed_config_args = buckconfig_load_stage("resolving buckconfig args", async {
            resolve_config_args(config_args, &mut file_ops).await
        })
        .await?;

        let started_parse = buckconfig_load_stage("reading external buckconfigs", async {
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

        let root_config = buckconfig_load_stage("reading project buckconfigs", async {
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
            buckconfig_load_stage("detecting bazel compatibility", async {
                should_apply_bazel_compat_defaults(&root_path, &mut file_ops).await
            })
            .await?;
        let root_config = if apply_bazel_compat_defaults && bazel_compat_project_root {
            let bazelrc_options = buckconfig_load_stage("reading bazelrc options", async {
                get_bazelrc_options(&root_path, &mut file_ops).await
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

    pub(crate) fn get_bazel_cell_alias_resolver_from_config(
        cell_name: CellName,
        cell_resolver: &CellResolver,
        config: &LegacyBuckConfig,
    ) -> buck2_error::Result<CellAliasResolver> {
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
        if is_bzlmod_cell_name(cell_name.as_str()) || cell_name.as_str() == "bazel_tools" {
            for (alias, destination) in bzlmod_cell_aliases_for_cell(cell_name.as_str()) {
                if alias == "bazel_tools" {
                    continue;
                }
                aliases.insert(
                    NonEmptyCellAlias::new(alias)?,
                    CellName::unchecked_new(&destination)?,
                );
            }
        }
        CellAliasResolver::new(cell_name, aliases)
    }

    pub(crate) async fn parse_single_cell_with_dice_for_cell(
        ctx: &mut DiceComputations<'_>,
        cell_name: CellName,
        cell_path: &CellRootPath,
    ) -> buck2_error::Result<LegacyBuckConfig> {
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
                get_bazelrc_options(cell_path, file_ops).await?
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
        } else if value == BZLMOD_EXTERNAL_CELL_KIND {
            let section = &format!("external_cell_{}", cell.as_str());
            let patches: Vec<BzlmodPatchConfig> =
                serde_json::from_str(get_config(section, "patches")?)
                    .buck_error_context("Invalid bzlmod patch configuration")?;
            let overlays: Vec<BzlmodOverlayConfig> = serde_json::from_str(
                config
                    .get(crate::legacy_configs::key::BuckconfigKeyRef {
                        section,
                        property: "overlays",
                    })
                    .unwrap_or("[]"),
            )
            .buck_error_context("Invalid bzlmod overlay configuration")?;
            let module_patch_strip = get_config(section, "patch_strip")?.parse()?;
            let url = get_config(section, "url")?;
            let urls = config
                .get(crate::legacy_configs::key::BuckconfigKeyRef {
                    section,
                    property: "urls",
                })
                .map(|urls| serde_json::from_str::<Vec<String>>(urls))
                .transpose()
                .buck_error_context("Invalid bzlmod URL configuration")?
                .unwrap_or_else(|| vec![url.to_owned()]);
            Ok(ExternalCellOrigin::Bzlmod(BzlmodCellSetup {
                module_name: get_config(section, "module_name")?.into(),
                version: get_config(section, "version")?.into(),
                canonical_repo_name: get_config(section, "canonical_repo_name")?.into(),
                local_path: config
                    .get(crate::legacy_configs::key::BuckconfigKeyRef {
                        section,
                        property: "local_path",
                    })
                    .map(Arc::from),
                url: Arc::from(url),
                urls: Arc::new(urls.into_iter().map(Arc::from).collect()),
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
                            path: patch.path.map(Arc::from),
                            content_sha256: patch.content_sha256.map(Arc::from),
                            patch_strip: patch.patch_strip.unwrap_or(module_patch_strip),
                        })
                        .collect(),
                ),
                overlays: Arc::new(
                    overlays
                        .into_iter()
                        .map(|overlay| BzlmodOverlay {
                            path: Arc::from(overlay.path),
                            url: Arc::from(overlay.url),
                            integrity: Arc::from(overlay.integrity),
                        })
                        .collect(),
                ),
                patch_strip: module_patch_strip,
            }))
        } else if value == BZLMOD_GENERATED_EXTERNAL_CELL_KIND {
            let section = &format!("external_cell_{}", cell.as_str());
            let generator: BzlmodGeneratedCellGenerator =
                serde_json::from_str(get_config(section, "generator")?)
                    .buck_error_context("Invalid generated bzlmod repo configuration")?;
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

fn external_cell_origin_from_bazel_module(
    module: &BazelCompatExternalModule,
) -> buck2_error::Result<ExternalCellOrigin> {
    match module {
        BazelCompatExternalModule::Registry(module) => {
            let patches: Vec<BzlmodPatchConfig> = serde_json::from_str(&module.patches_json)
                .buck_error_context("Invalid bzlmod patch configuration")?;
            let overlays: Vec<BzlmodOverlayConfig> = serde_json::from_str(&module.overlays_json)
                .buck_error_context("Invalid bzlmod overlay configuration")?;
            let urls: Vec<String> = serde_json::from_str(&module.urls_json)
                .buck_error_context("Invalid bzlmod URL configuration")?;
            Ok(ExternalCellOrigin::Bzlmod(BzlmodCellSetup {
                module_name: Arc::from(module.module_name.as_str()),
                version: Arc::from(module.version.as_str()),
                canonical_repo_name: Arc::from(module.canonical_repo_name.as_str()),
                local_path: module
                    .local_path
                    .as_ref()
                    .map(|path| Arc::from(path.as_str())),
                url: Arc::from(module.url.as_str()),
                urls: Arc::new(urls.into_iter().map(Arc::from).collect()),
                integrity: Arc::from(module.integrity.as_str()),
                strip_prefix: module
                    .strip_prefix
                    .as_ref()
                    .map(|strip_prefix| Arc::from(strip_prefix.as_str())),
                archive_type: module
                    .archive_type
                    .as_ref()
                    .map(|archive_type| Arc::from(archive_type.as_str())),
                patches: Arc::new(
                    patches
                        .into_iter()
                        .map(|patch| BzlmodPatch {
                            url: Arc::from(patch.url),
                            integrity: Arc::from(patch.integrity),
                            path: patch.path.map(Arc::from),
                            content_sha256: patch.content_sha256.map(Arc::from),
                            patch_strip: patch.patch_strip.unwrap_or(module.patch_strip),
                        })
                        .collect(),
                ),
                overlays: Arc::new(
                    overlays
                        .into_iter()
                        .map(|overlay| BzlmodOverlay {
                            path: Arc::from(overlay.path),
                            url: Arc::from(overlay.url),
                            integrity: Arc::from(overlay.integrity),
                        })
                        .collect(),
                ),
                patch_strip: module.patch_strip,
            }))
        }
        BazelCompatExternalModule::Generated(module) => {
            let generator: BzlmodGeneratedCellGenerator =
                serde_json::from_str(&module.generator_json)
                    .buck_error_context("Invalid generated bzlmod repo configuration")?;
            Ok(ExternalCellOrigin::BzlmodGenerated(
                BzlmodGeneratedCellSetup {
                    canonical_repo_name: Arc::from(module.canonical_repo_name.as_str()),
                    generator,
                },
            ))
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
) -> buck2_error::Result<Option<ConfigPath>> {
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
) -> BoxFuture<'a, buck2_error::Result<()>> {
    async move {
        let key = path.to_string();
        if !visited.insert(key) {
            return Ok(());
        }
        let Some(lines) = file_ops.read_file_lines_if_exists(&path).await? else {
            if required {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
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
                            return Err(buck2_error!(
                                buck2_error::ErrorTag::Input,
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
    matches!(name, "javacopt" | "host_javacopt" | "platforms")
}

fn bazelrc_native_command_line_comma_separated_list_option(name: &str) -> bool {
    matches!(name, "platforms")
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
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--host_copt") {
                options.host_copt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--host_conlyopt") {
                options.host_conlyopt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--host_cxxopt") {
                options.host_cxxopt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--per_file_copt") {
                options.per_file_copt.push(value);
            } else if let Some(value) = bazelrc_arg_value(args, &mut index, "--macos_minimum_os") {
                options.macos_minimum_os.push(value);
            } else if let Some(value) =
                bazelrc_arg_value(args, &mut index, "--host_macos_minimum_os")
            {
                options.host_macos_minimum_os.push(value);
            } else if let Some(values) = bazelrc_command_line_build_setting(args, &mut index) {
                options.command_line_build_settings.extend(values);
            }
        }
        index += 1;
    }
    changed
}

fn bazelrc_options_from_records(records: &[BazelrcRecord]) -> BazelCompatBazelrcOptions {
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
        let host_config = bazelrc_host_config();
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
) -> buck2_error::Result<BazelCompatBazelrcOptions> {
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
    Ok(bazelrc_options_from_records(&records))
}

#[derive(Default, Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub(crate) struct BazelModuleCellAliases {
    root_aliases: Vec<BazelCompatCellAlias>,
    cell_aliases: BTreeMap<String, Vec<BazelCompatCellAlias>>,
    external_modules: Vec<BazelCompatExternalModule>,
    registered_toolchains: Vec<String>,
}

impl BazelModuleCellAliases {
    fn dice_config_equal(&self, other: &Self) -> bool {
        self.root_aliases == other.root_aliases
            && self.cell_aliases == other.cell_aliases
            && self.registered_toolchains == other.registered_toolchains
    }

    pub(crate) fn aliases_for_cell(
        &self,
        cell_name: &str,
        root_cell_name: &str,
    ) -> Vec<BazelCompatCellAlias> {
        let aliases = if cell_name == root_cell_name {
            self.root_aliases.as_slice()
        } else {
            self.cell_aliases
                .get(cell_name)
                .map(Vec::as_slice)
                .unwrap_or(&[])
        };
        aliases
            .iter()
            .map(|alias| alias.with_actual_root_cell(root_cell_name))
            .collect()
    }

    fn normalize(&mut self) {
        self.root_aliases.sort_unstable();
        self.root_aliases.dedup();
        for aliases in self.cell_aliases.values_mut() {
            aliases.sort_unstable();
            aliases.dedup();
        }
        dedup_preserve_order(&mut self.registered_toolchains);
        self.external_modules
            .sort_unstable_by(|a, b| a.cell_name().cmp(b.cell_name()));
        self.external_modules
            .dedup_by(|a, b| a.cell_name() == b.cell_name());
    }

    fn register_for_starlark_label_resolution(&self, root_cell_name: &str) {
        register_bzlmod_cell_aliases_from_refs(
            root_cell_name,
            self.root_aliases.iter().map(|alias| {
                (
                    alias.alias.as_str(),
                    alias.actual_root_cell_name(root_cell_name),
                )
            }),
        );
        for (cell_name, aliases) in &self.cell_aliases {
            register_bzlmod_cell_aliases_from_refs(
                cell_name,
                aliases.iter().map(|alias| {
                    (
                        alias.alias.as_str(),
                        alias.actual_root_cell_name(root_cell_name),
                    )
                }),
            );
        }
    }

    fn register_external_cell_origins(&self) -> buck2_error::Result<()> {
        for module in &self.external_modules {
            register_bzlmod_cell_canonical_repo_name_for_cell(
                module.cell_name(),
                module.canonical_repo_name(),
            );
            let cell = CellName::unchecked_new(module.cell_name())?;
            register_external_cell_origin(cell, external_cell_origin_from_bazel_module(module)?);
        }
        Ok(())
    }
}

fn dedup_preserve_order<T: Ord + Clone>(values: &mut Vec<T>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn bzlmod_external_module_is_local(module: &BazelCompatExternalModule) -> bool {
    match module {
        BazelCompatExternalModule::Registry(module) => module.local_path.is_some(),
        BazelCompatExternalModule::Generated(_) => false,
    }
}

fn bzlmod_external_module_is_configure_repo(module: &BazelCompatExternalModule) -> bool {
    match module {
        BazelCompatExternalModule::Registry(_) => false,
        BazelCompatExternalModule::Generated(module) => {
            match serde_json::from_str::<BzlmodGeneratedCellGenerator>(&module.generator_json) {
                Ok(
                    BzlmodGeneratedCellGenerator::HostPlatform(_)
                    | BzlmodGeneratedCellGenerator::CcAutoconfToolchains(_)
                    | BzlmodGeneratedCellGenerator::CcAutoconf(_)
                    | BzlmodGeneratedCellGenerator::XcodeConfig(_)
                    | BzlmodGeneratedCellGenerator::ShellConfig(_),
                ) => true,
                Ok(_) | Err(_) => false,
            }
        }
    }
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Hash,
    Allocative,
    Pagable
)]
struct BazelDep {
    name: String,
    version: String,
    apparent_name: Option<String>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
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

static BZLMOD_HTTP_CLIENT: LazyLock<tokio::sync::OnceCell<HttpClient>> =
    LazyLock::new(tokio::sync::OnceCell::new);

const BAZEL_TOOLS_MODULE_TOOLS: &str = include_str!("../../../../bazel_tools/MODULE.tools");

#[derive(
    Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Allocative, Pagable
)]
struct RootBzlmodModule {
    name: String,
    version: String,
    repo_name: String,
    canonical_repo_name: String,
    lockfile_extension_generated_repos: BTreeMap<String, BTreeSet<String>>,
    lockfile_extension_facts: BTreeSet<String>,
    constants: Vec<(String, String)>,
    extension_usages: Vec<BzlmodExtensionUsage>,
    use_repo_rule_invocations: Vec<BzlmodUseRepoRuleInvocation>,
    registered_toolchains: Vec<String>,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodUseRepoImport {
    alias: String,
    repo_name: String,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodExtensionUsage {
    proxy_name: String,
    extension_bzl_file: String,
    extension_name: String,
    dev_dependency: bool,
    imports: Vec<BzlmodUseRepoImport>,
    repo_overrides: Vec<BzlmodRepoOverride>,
    tags: Vec<BzlmodExtensionTag>,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodRepoOverride {
    repo_name: String,
    overriding_repo_name: String,
    must_exist: bool,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodExtensionTag {
    tag_name: String,
    bindings: Vec<(String, String)>,
    kwargs: Vec<(String, String)>,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodUseRepoRuleInvocation {
    rule_bzl_file: String,
    rule_name: String,
    repo_name: String,
    attrs: Vec<(String, String)>,
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Allocative, Pagable
)]
struct BzlmodExtensionId {
    bzl_cell_name: String,
    bzl_path: String,
    extension_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodResolvedExtension {
    id: BzlmodExtensionId,
    unique_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodDepGraph {
    root_module: RootBzlmodModule,
    discovered: DiscoveredBcrModules,
    selected_keys: BTreeSet<(String, String)>,
    selected_keys_in_bfs_order: Vec<(String, String)>,
    selected_keys_in_dependency_order: Vec<(String, String)>,
    canonical_repo_names_by_key: BTreeMap<(String, String), String>,
    canonical_repo_names_by_cell: BTreeMap<String, String>,
    root_aliases_by_key: BTreeMap<(String, String), BTreeSet<String>>,
    cell_aliases_by_cell: BzlmodCellAliasesByCell,
    extension_eval_cell_aliases_by_cell: BzlmodCellAliasesByCell,
    extension_unique_names: BTreeMap<BzlmodExtensionId, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub struct BzlmodModuleExtensionRepoMappingBase {
    pub host_aliases: Vec<(String, String)>,
    pub repo_overrides: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodSingleExtensionUsagesValue {
    extension_id: BzlmodExtensionId,
    unique_name: String,
    extension_usages_json: String,
}

fn bzlmod_single_extension_eval_setup(
    usage: &BzlmodSingleExtensionUsagesValue,
) -> BzlmodModuleExtensionRepoSetup {
    BzlmodModuleExtensionRepoSetup {
        parent_canonical_repo_name: Arc::from(""),
        parent_is_root: true,
        extension_bzl_file: Arc::from(format!(
            "{}//{}",
            usage.extension_id.bzl_cell_name, usage.extension_id.bzl_path
        )),
        extension_bzl_cell: Arc::from(usage.extension_id.bzl_cell_name.as_str()),
        extension_bzl_path: Arc::from(usage.extension_id.bzl_path.as_str()),
        extension_unique_name: Arc::from(usage.unique_name.as_str()),
        extension_name: Arc::from(usage.extension_id.extension_name.as_str()),
        repo_name: Arc::from(""),
        extension_usages_key: register_bzlmod_module_extension_usages_json(
            &usage.extension_usages_json,
        ),
        extension_usages_json: Arc::from(usage.extension_usages_json.as_str()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodInspectionModule {
    name: String,
    version: String,
    canonical_repo_name: String,
    selected: bool,
    deps: Vec<BazelDep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodModuleInspectionValue {
    modules: Vec<BzlmodInspectionModule>,
    modules_index: BTreeMap<String, BTreeSet<String>>,
    extension_to_repo_internal_names: BTreeMap<BzlmodExtensionId, BTreeSet<String>>,
    module_key_to_canonical_repo_name: BTreeMap<(String, String), String>,
    errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodModTidyValue {
    root_extension_usages: Vec<BzlmodSingleExtensionUsagesValue>,
    errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodFetchAllValue {
    repos_to_vendor: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRepoDefinitionValue {
    module: Option<BazelCompatExternalModule>,
    configure: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRepositoryDirectoryValue {
    found: bool,
    exclude_from_vendoring: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodVendorFileValue {
    ignored_repos: Vec<String>,
    pinned_repos: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BcrResolution {
    external_modules: Vec<BazelCompatExternalModule>,
    root_aliases: Vec<BazelCompatCellAlias>,
    cell_aliases: BTreeMap<String, Vec<BazelCompatCellAlias>>,
    registered_toolchains: Vec<String>,
}

type BzlmodCellAliasMap = StdBuckHashMap<String, String>;
type BzlmodCellAliasesByCell = StdBuckHashMap<String, BzlmodCellAliasMap>;

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Hash,
    Allocative,
    Pagable
)]
struct BzlmodPatchConfig {
    #[serde(default)]
    url: String,
    #[serde(default)]
    integrity: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_sha256: Option<String>,
    #[serde(default)]
    patch_strip: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative, Pagable)]
struct BzlmodRootPatch {
    path: String,
    content: Arc<str>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Hash,
    Allocative,
    Pagable
)]
struct BzlmodOverlayConfig {
    path: String,
    url: String,
    integrity: String,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
struct BzlmodModuleExtensionEvaluationConfig {
    root_module_has_non_dev_dependency: bool,
    modules: Vec<BzlmodModuleExtensionModuleConfig>,
    #[serde(default)]
    usages: Vec<BzlmodModuleExtensionUsageConfig>,
    #[serde(default)]
    repo_overrides: Vec<(String, String)>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
struct BzlmodModuleExtensionModuleConfig {
    name: String,
    version: String,
    canonical_repo_name: String,
    is_root: bool,
    extension_bzl_file: String,
    extension_name: String,
    cell_aliases: Vec<(String, String)>,
    constants: Vec<(String, String)>,
    tags: Vec<BzlmodModuleExtensionTagConfig>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
struct BzlmodModuleExtensionTagConfig {
    tag_name: String,
    dev_dependency: bool,
    bindings: Vec<(String, String)>,
    kwargs: Vec<(String, String)>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
struct BzlmodModuleExtensionUsageConfig {
    imports: Vec<BzlmodUseRepoImport>,
    repo_overrides: Vec<BzlmodRepoOverride>,
}

#[derive(Clone, Debug, Deserialize)]
struct BzlmodModuleLockfile {
    #[serde(default, rename = "registryFileHashes")]
    registry_file_hashes: BTreeMap<String, Option<String>>,
    #[serde(default, rename = "selectedYankedVersions")]
    selected_yanked_versions: BTreeMap<String, String>,
    #[serde(default, rename = "moduleExtensions")]
    module_extensions: BTreeMap<String, BTreeMap<String, BzlmodModuleLockfileExtension>>,
    #[serde(default)]
    facts: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct BzlmodModuleLockfileExtension {
    #[serde(default, rename = "generatedRepoSpecs")]
    generated_repo_specs: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodModuleLockfileData {
    registry_file_hashes: BTreeMap<String, Option<String>>,
    selected_yanked_versions: BTreeMap<(String, String), String>,
    extension_generated_repos: BTreeMap<String, BTreeSet<String>>,
    extension_facts: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRegistryValue {
    url: String,
    registry_file_hashes: BTreeMap<String, Option<String>>,
    selected_yanked_versions: BTreeMap<(String, String), String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRegistryInvalidationValue {
    epoch_hour: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodYankedVersionsValue {
    yanked_versions: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodClientEnvironmentVariableValue {
    value: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRepositoryEnvironmentVariableValue {
    value: Option<String>,
}

// Bazel exposes the full repo environment through repository_ctx.os.environ, but
// that access does not establish a Skyframe dependency. Track only explicitly
// requested repo-env variables as DICE keys.
struct BzlmodRepositoryEnvironmentData {
    vars: Arc<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodAllowedYankedVersionsValue {
    allow_all: bool,
    modules: BTreeSet<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRepoSpecValue {
    source_json: BcrSourceJson,
    registry_file_hashes: BTreeMap<String, Option<String>>,
}

pub const BZLMOD_ALLOWED_YANKED_VERSIONS_ENV: &str = "BZLMOD_ALLOW_YANKED_VERSIONS";
pub const BZLMOD_REPOSITORY_OS_NAME_ENV: &str = "BUCK2_REPOSITORY_OS_NAME";
pub const BZLMOD_REPOSITORY_OS_ARCH_ENV: &str = "BUCK2_REPOSITORY_OS_ARCH";

fn empty_bzlmod_lockfile_data() -> BzlmodModuleLockfileData {
    BzlmodModuleLockfileData {
        registry_file_hashes: BTreeMap::new(),
        selected_yanked_versions: BTreeMap::new(),
        extension_generated_repos: BTreeMap::new(),
        extension_facts: BTreeSet::new(),
    }
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Hash,
    Allocative,
    Pagable
)]
struct BcrSourceJson {
    url: String,
    urls: Option<Vec<String>>,
    integrity: String,
    strip_prefix: Option<String>,
    archive_type: Option<String>,
    patches: Option<BTreeMap<String, String>>,
    overlay: Option<BTreeMap<String, String>>,
    patch_strip: Option<u32>,
}

fn bcr_source_urls(source_json: &BcrSourceJson) -> Vec<String> {
    source_json
        .urls
        .as_ref()
        .filter(|urls| !urls.is_empty())
        .cloned()
        .unwrap_or_else(|| vec![source_json.url.clone()])
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative, Pagable)]
struct BzlmodArchiveOverride {
    module_name: String,
    urls: Vec<String>,
    integrity: String,
    strip_prefix: Option<String>,
    archive_type: Option<String>,
    patches: Vec<BzlmodRootPatch>,
    patch_strip: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative, Pagable)]
struct BzlmodSingleVersionOverride {
    version: Option<String>,
    patches: Vec<BzlmodRootPatch>,
    patch_strip: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative, Pagable)]
struct BzlmodLocalPathOverride {
    module_name: String,
    path: String,
    module_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRootResolutionInput {
    aliases: BazelModuleCellAliases,
    root_deps: Vec<BazelDep>,
    root_module: RootBzlmodModule,
    builtin_bazel_tools_module: DiscoveredBcrModule,
    archive_overrides: BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: BTreeMap<String, BzlmodSingleVersionOverride>,
    local_path_overrides: BTreeMap<String, BzlmodLocalPathOverride>,
}

async fn read_bazel_module_resolution_inputs(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> buck2_error::Result<BzlmodRootResolutionInput> {
    let mut aliases = BazelModuleCellAliases::default();
    let mut root_deps = Vec::new();
    let mut archive_overrides = BTreeMap::new();
    let mut single_version_overrides = BTreeMap::new();
    let mut local_path_overrides = BTreeMap::new();
    let mut root_module_lines = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stack = vec!["MODULE.bazel".to_owned()];

    buckconfig_load_stage("reading MODULE.bazel files", async {
        while let Some(module_file) = stack.pop() {
            if !seen.insert(module_file.clone()) {
                continue;
            }

            let file = ForwardRelativePath::new(&module_file)?;
            let path = ConfigPath::Project(cell_path.as_project_relative_path().join(file));
            let Some(lines) = file_ops.read_file_lines_if_exists(&path).await? else {
                continue;
            };
            validate_bzlmod_module_lines(&module_file, &lines)?;
            let constants = bzlmod_module_constants_from_lines(&lines);
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
                let mut archive_override =
                    bzlmod_archive_override_from_call(&module_file, &call, &constants)?;
                read_bzlmod_root_patch_contents(cell_path, file_ops, &mut archive_override.patches)
                    .await?;
                archive_overrides.insert(archive_override.module_name.clone(), archive_override);
            }

            for call in collect_bzl_calls(&lines, "single_version_override(") {
                let Some((module_name, mut single_version_override)) =
                    bzlmod_single_version_override_from_call(&module_file, &call, &constants)?
                else {
                    continue;
                };
                read_bzlmod_root_patch_contents(
                    cell_path,
                    file_ops,
                    &mut single_version_override.patches,
                )
                .await?;
                single_version_overrides.insert(module_name, single_version_override);
            }

            for call in collect_bzl_calls(&lines, "multiple_version_override(") {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "multiple_version_override is not implemented in Buck2 bzlmod resolution yet. Bazel allows multiple selected versions of the same module; refusing to silently collapse that graph: {}",
                    call
                ));
            }

            for call in collect_bzl_calls(&lines, "git_override(") {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "git_override is not implemented in Buck2 bzlmod resolution yet: {}",
                    call
                ));
            }

            for call in collect_bzl_calls(&lines, "local_path_override(") {
                let mut local_path_override = bzlmod_local_path_override_from_call(cell_path, &call)?;
                let module_file = ProjectRelativePath::new(&local_path_override.path)?
                    .join(ForwardRelativePath::new("MODULE.bazel")?);
                let module_path = ConfigPath::Project(module_file);
                let Some(module_lines) = file_ops.read_file_lines_if_exists(&module_path).await?
                else {
                    return Err(buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "local_path_override for module `{}` points to `{}`, but `{}/MODULE.bazel` does not exist",
                        local_path_override.module_name,
                        local_path_override.path,
                        local_path_override.path
                    ));
                };
                local_path_override.module_text = module_lines.join("\n");
                local_path_overrides
                    .insert(local_path_override.module_name.clone(), local_path_override);
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
        Ok(())
    })
    .await?;

    let mut builtin_bazel_tools_module = builtin_bazel_tools_module()?;
    apply_bzlmod_dep_overrides(
        &mut root_deps,
        &archive_overrides,
        &single_version_overrides,
        &local_path_overrides,
    );
    apply_bzlmod_dep_overrides(
        &mut builtin_bazel_tools_module.deps,
        &archive_overrides,
        &single_version_overrides,
        &local_path_overrides,
    );

    let root_module = bzlmod_root_module_from_lines(&root_module_lines)?;
    Ok(BzlmodRootResolutionInput {
        aliases,
        root_deps,
        root_module,
        builtin_bazel_tools_module,
        archive_overrides,
        single_version_overrides,
        local_path_overrides,
    })
}

pub(crate) async fn bzlmod_resolution_enabled_on_dice(
    ctx: &mut DiceComputations<'_>,
) -> buck2_error::Result<bool> {
    let root_cell = ctx.get_cell_resolver().await?.root_cell();
    Ok(ctx
        .parse_legacy_config_property::<bool>(
            root_cell,
            BuckconfigKeyRef {
                section: "bazel",
                property: "compatibility",
            },
        )
        .await?
        .unwrap_or(false))
}

pub(crate) async fn get_bazel_module_resolution_on_dice(
    ctx: &mut DiceComputations<'_>,
) -> buck2_error::Result<Arc<BazelModuleCellAliases>> {
    if !bzlmod_resolution_enabled_on_dice(ctx).await? {
        return Ok(Arc::new(BazelModuleCellAliases::default()));
    }
    let aliases = ctx.compute(&BzlmodResolutionKey).await??;
    Ok(aliases)
}

pub async fn get_bazel_module_registered_toolchains_on_dice(
    ctx: &mut DiceComputations<'_>,
) -> buck2_error::Result<Vec<String>> {
    Ok(get_bazel_module_resolution_on_dice(ctx)
        .await?
        .registered_toolchains
        .clone())
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_LOCK_FILE")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodLockFileKey;

#[async_trait::async_trait]
impl Key for BzlmodLockFileKey {
    type Value = buck2_error::Result<Arc<BzlmodModuleLockfileData>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let root_path = resolver.root_cell_instance().path().to_buf();
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let mut file_ops = DiceConfigFileOps::new(ctx, project_fs, &resolver);
        Ok(Arc::new(
            buckconfig_load_stage("parsing MODULE.bazel.lock", async {
                bzlmod_lockfile_data(&root_path, &mut file_ops).await
            })
            .await?,
        ))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_LOCK_FILE(hidden)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodHiddenLockFileKey;

#[async_trait::async_trait]
impl Key for BzlmodHiddenLockFileKey {
    type Value = buck2_error::Result<Arc<BzlmodModuleLockfileData>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let mut file_ops = DiceConfigFileOps::new(ctx, project_fs, &resolver);
        let hidden_lockfile_path = ConfigPath::Project(ProjectRelativePathBuf::unchecked_new(
            "buck-out/v2/cache/bzlmod_hidden/MODULE.bazel.lock".to_owned(),
        ));
        let Some(lines) = file_ops
            .read_file_lines_if_exists(&hidden_lockfile_path)
            .await?
        else {
            return Ok(Arc::new(empty_bzlmod_lockfile_data()));
        };
        Ok(Arc::new(bzlmod_lockfile_data_from_str(&lines.join("\n"))?))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REGISTRY({url})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRegistryKey {
    url: String,
}

fn bzlmod_default_registry_key() -> BzlmodRegistryKey {
    BzlmodRegistryKey {
        url: "https://bcr.bazel.build".to_owned(),
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REGISTRY_LAST_INVALIDATION")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRegistryInvalidationKey;

impl InjectedKey for BzlmodRegistryInvalidationKey {
    type Value = buck2_error::Result<Arc<BzlmodRegistryInvalidationValue>>;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

pub trait SetBzlmodRegistryInvalidation {
    fn set_bzlmod_registry_invalidation(&mut self, epoch_hour: u64) -> buck2_error::Result<()>;
}

impl SetBzlmodRegistryInvalidation for DiceTransactionUpdater {
    fn set_bzlmod_registry_invalidation(&mut self, epoch_hour: u64) -> buck2_error::Result<()> {
        Ok(self.changed_to([(
            BzlmodRegistryInvalidationKey,
            Ok(Arc::new(BzlmodRegistryInvalidationValue { epoch_hour })),
        )])?)
    }
}

#[async_trait::async_trait]
impl Key for BzlmodRegistryKey {
    type Value = buck2_error::Result<Arc<BzlmodRegistryValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let lockfile = ctx.compute(&BzlmodLockFileKey).await??;
        Ok(Arc::new(BzlmodRegistryValue {
            url: self.url.clone(),
            registry_file_hashes: lockfile.registry_file_hashes.clone(),
            selected_yanked_versions: lockfile.selected_yanked_versions.clone(),
        }))
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
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("YANKED_VERSIONS({module_name}, {registry_url})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodYankedVersionsKey {
    module_name: String,
    registry_url: String,
}

#[async_trait::async_trait]
impl Key for BzlmodYankedVersionsKey {
    type Value = buck2_error::Result<Arc<BzlmodYankedVersionsValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        // Match Bazel's YankedVersionsFunction: depend on RegistryKey, but
        // fail open if metadata.json cannot be read.
        let registry = ctx
            .compute(&BzlmodRegistryKey {
                url: self.registry_url.clone(),
            })
            .await??;
        let client = shared_bzlmod_http_client().await?;
        let metadata_url = format!(
            "{}/modules/{}/metadata.json",
            registry.url, self.module_name
        );
        let yanked_versions = match http_get_text(&client, &metadata_url).await {
            Ok(metadata) => bzlmod_yanked_versions_from_metadata_json(&metadata).ok(),
            Err(_) => None,
        };
        Ok(Arc::new(BzlmodYankedVersionsValue { yanked_versions }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("CLIENT_ENVIRONMENT_VARIABLE({name})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodClientEnvironmentVariableKey {
    name: String,
}

impl InjectedKey for BzlmodClientEnvironmentVariableKey {
    type Value = buck2_error::Result<Arc<BzlmodClientEnvironmentVariableValue>>;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

pub trait SetBzlmodClientEnvironment {
    fn set_bzlmod_client_environment(
        &mut self,
        vars: Vec<(String, Option<String>)>,
    ) -> buck2_error::Result<()>;
}

impl SetBzlmodClientEnvironment for DiceTransactionUpdater {
    fn set_bzlmod_client_environment(
        &mut self,
        vars: Vec<(String, Option<String>)>,
    ) -> buck2_error::Result<()> {
        let vars = vars.into_iter().map(|(name, value)| {
            (
                BzlmodClientEnvironmentVariableKey { name },
                Ok(Arc::new(BzlmodClientEnvironmentVariableValue { value })),
            )
        });
        Ok(self.changed_to(vars)?)
    }
}

pub trait SetBzlmodRepositoryEnvironment {
    fn set_bzlmod_repository_environment(
        &mut self,
        vars: BTreeMap<String, String>,
    ) -> buck2_error::Result<()>;
}

pub trait SetBzlmodRepositoryEnvironmentData {
    fn set_bzlmod_repository_environment_data(&mut self, vars: BTreeMap<String, String>);
}

impl SetBzlmodRepositoryEnvironment for DiceTransactionUpdater {
    fn set_bzlmod_repository_environment(
        &mut self,
        vars: BTreeMap<String, String>,
    ) -> buck2_error::Result<()> {
        let changed_vars = self
            .existing_key_values_of_type_for_introspection::<
                BzlmodRepositoryEnvironmentVariableKey,
            >()
            .into_iter()
            .filter_map(move |(key, old)| {
                let fresh = Ok(Arc::new(BzlmodRepositoryEnvironmentVariableValue {
                    value: vars.get(&key.name).cloned(),
                }));
                if old
                    .as_ref()
                    .is_some_and(|old| {
                        BzlmodRepositoryEnvironmentVariableKey::equality(old, &fresh)
                    })
                {
                    None
                } else {
                    Some((key, fresh))
                }
            });
        Ok(self.changed_to(changed_vars)?)
    }
}

impl SetBzlmodRepositoryEnvironmentData for UserComputationData {
    fn set_bzlmod_repository_environment_data(&mut self, vars: BTreeMap<String, String>) {
        self.data.set(BzlmodRepositoryEnvironmentData {
            vars: Arc::new(vars),
        });
    }
}

#[async_trait::async_trait]
pub trait GetBzlmodRepositoryEnvironment {
    async fn get_bzlmod_repository_environment(
        &mut self,
    ) -> buck2_error::Result<Arc<BTreeMap<String, String>>>;
}

#[async_trait::async_trait]
impl GetBzlmodRepositoryEnvironment for DiceComputations<'_> {
    async fn get_bzlmod_repository_environment(
        &mut self,
    ) -> buck2_error::Result<Arc<BTreeMap<String, String>>> {
        Ok(self
            .per_transaction_data()
            .data
            .get::<BzlmodRepositoryEnvironmentData>()
            .map(|data| data.vars.dupe())
            .unwrap_or_else(|_| Arc::new(BTreeMap::new())))
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REPOSITORY_ENVIRONMENT_VARIABLE({name})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRepositoryEnvironmentVariableKey {
    name: String,
}

#[async_trait::async_trait]
impl Key for BzlmodRepositoryEnvironmentVariableKey {
    type Value = buck2_error::Result<Arc<BzlmodRepositoryEnvironmentVariableValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let env = ctx
            .per_transaction_data()
            .data
            .get::<BzlmodRepositoryEnvironmentData>()
            .map(|data| data.vars.dupe())
            .unwrap_or_else(|_| Arc::new(BTreeMap::new()));
        Ok(Arc::new(BzlmodRepositoryEnvironmentVariableValue {
            value: env.get(&self.name).cloned(),
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[async_trait::async_trait]
pub trait GetBzlmodRepositoryEnvironmentVariable {
    async fn get_bzlmod_repository_environment_variable(
        &mut self,
        name: &str,
    ) -> buck2_error::Result<Option<String>>;
}

#[async_trait::async_trait]
impl GetBzlmodRepositoryEnvironmentVariable for DiceComputations<'_> {
    async fn get_bzlmod_repository_environment_variable(
        &mut self,
        name: &str,
    ) -> buck2_error::Result<Option<String>> {
        Ok(self
            .compute(&BzlmodRepositoryEnvironmentVariableKey {
                name: name.to_owned(),
            })
            .await??
            .value
            .clone())
    }
}

#[async_trait::async_trait]
pub trait GetBzlmodModuleExtensionRepoMappingBase {
    async fn get_bzlmod_module_extension_repo_mapping_base(
        &mut self,
        extension_bzl_cell: &str,
        extension_bzl_path: &str,
        extension_name: &str,
    ) -> buck2_error::Result<Arc<BzlmodModuleExtensionRepoMappingBase>>;
}

#[async_trait::async_trait]
impl GetBzlmodModuleExtensionRepoMappingBase for DiceComputations<'_> {
    async fn get_bzlmod_module_extension_repo_mapping_base(
        &mut self,
        extension_bzl_cell: &str,
        extension_bzl_path: &str,
        extension_name: &str,
    ) -> buck2_error::Result<Arc<BzlmodModuleExtensionRepoMappingBase>> {
        let dep_graph = self.compute(&BzlmodDepGraphKey).await??;
        let extension_id = BzlmodExtensionId {
            bzl_cell_name: extension_bzl_cell.to_owned(),
            bzl_path: extension_bzl_path.to_owned(),
            extension_name: extension_name.to_owned(),
        };
        let mut host_aliases = dep_graph
            .cell_aliases_by_cell
            .get(extension_bzl_cell)
            .into_iter()
            .flat_map(|aliases| {
                aliases
                    .iter()
                    .map(|(alias, target)| (alias.clone(), target.clone()))
            })
            .collect::<Vec<_>>();
        let root_cell_name = self
            .get_cell_resolver()
            .await?
            .root_cell()
            .as_str()
            .to_owned();
        for (_alias, target) in &mut host_aliases {
            if target == "root" {
                *target = root_cell_name.clone();
            }
        }
        host_aliases.sort_unstable();
        host_aliases.dedup();
        let mut repo_overrides =
            bzlmod_module_extension_repo_overrides_for_extension(&dep_graph, &extension_id)?;
        for (_alias, target) in &mut repo_overrides {
            if target == "root" {
                *target = root_cell_name.clone();
            }
        }
        Ok(Arc::new(BzlmodModuleExtensionRepoMappingBase {
            host_aliases,
            repo_overrides,
        }))
    }
}

#[async_trait::async_trait]
pub trait BzlmodModuleExtensionEvaluator: Send + Sync + 'static {
    async fn evaluate_bzlmod_module_extension(
        &self,
        ctx: &mut DiceComputations<'_>,
        setup: BzlmodModuleExtensionRepoSetup,
    ) -> buck2_error::Result<Vec<String>>;
}

pub static BZLMOD_MODULE_EXTENSION_EVALUATOR: LateBinding<
    &'static dyn BzlmodModuleExtensionEvaluator,
> = LateBinding::new("BZLMOD_MODULE_EXTENSION_EVALUATOR");

#[async_trait::async_trait]
pub trait EvaluateBzlmodModuleExtension {
    async fn evaluate_bzlmod_module_extension(
        &mut self,
        setup: BzlmodModuleExtensionRepoSetup,
    ) -> buck2_error::Result<Vec<String>>;
}

#[async_trait::async_trait]
impl EvaluateBzlmodModuleExtension for DiceComputations<'_> {
    async fn evaluate_bzlmod_module_extension(
        &mut self,
        setup: BzlmodModuleExtensionRepoSetup,
    ) -> buck2_error::Result<Vec<String>> {
        BZLMOD_MODULE_EXTENSION_EVALUATOR
            .get()?
            .evaluate_bzlmod_module_extension(self, setup)
            .await
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("CLIENT_ENVIRONMENT_VARIABLE(BZLMOD_ALLOW_YANKED_VERSIONS)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodAllowedYankedVersionsKey;

#[async_trait::async_trait]
impl Key for BzlmodAllowedYankedVersionsKey {
    type Value = buck2_error::Result<Arc<BzlmodAllowedYankedVersionsValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let env = ctx
            .compute(&BzlmodClientEnvironmentVariableKey {
                name: BZLMOD_ALLOWED_YANKED_VERSIONS_ENV.to_owned(),
            })
            .await??;
        Ok(Arc::new(bzlmod_allowed_yanked_versions_from_env(
            env.value.as_deref(),
        )?))
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
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("MODULE_FILE(root)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRootModuleKey;

#[async_trait::async_trait]
impl Key for BzlmodRootModuleKey {
    type Value = buck2_error::Result<Arc<BzlmodRootResolutionInput>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let root_path = resolver.root_cell_instance().path().to_buf();
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let mut file_ops = DiceConfigFileOps::new(ctx, project_fs, &resolver);
        Ok(Arc::new(
            read_bazel_module_resolution_inputs(&root_path, &mut file_ops).await?,
        ))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("MODULE_FILE({}@{})", name, version)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModuleFileKey {
    name: String,
    version: String,
}

#[async_trait::async_trait]
impl Key for BzlmodModuleFileKey {
    type Value = buck2_error::Result<Arc<DiscoveredBcrModule>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let registry = ctx.compute(&bzlmod_default_registry_key()).await??;
        let dep = BazelDep {
            name: self.name.clone(),
            version: self.version.clone(),
            apparent_name: None,
        };
        if let Some(local_path_override) = root.local_path_overrides.get(&dep.name).cloned() {
            return Ok(Arc::new(
                fetch_local_bzlmod_module(dep, local_path_override).await?,
            ));
        }
        let project_fs = ctx.global_data().get_io_provider().project_root().dupe();
        let client = shared_bzlmod_http_client().await?;
        let archive_override = root.archive_overrides.get(&dep.name).cloned();
        let single_version_override = root.single_version_overrides.get(&dep.name).cloned();
        let repo = format!("{}@{}", dep.name, dep.version);
        Ok(Arc::new(
            bzlmod_repo_progress_span(
                repo,
                format!("modules/{}/{}/MODULE.bazel", dep.name, dep.version),
                "registry module",
                "fetching MODULE.bazel",
                fetch_bcr_module_file(
                    &project_fs,
                    &registry.url,
                    client,
                    dep,
                    archive_override,
                    single_version_override,
                ),
            )
            .await?,
        ))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodDiscoveryResult {
    discovered: DiscoveredBcrModules,
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_DEP_GRAPH(discovery)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodDiscoveryKey;

#[async_trait::async_trait]
impl Key for BzlmodDiscoveryKey {
    type Value = buck2_error::Result<Arc<BzlmodDiscoveryResult>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let mut discovered = DiscoveredBcrModules::new();
        let mut scheduled = BTreeSet::<(String, String)>::new();
        let mut frontier = Vec::new();
        let mut discovery_roots = root.root_deps.clone();
        discovery_roots.extend(root.builtin_bazel_tools_module.deps.iter().cloned());
        for dep in discovery_roots {
            if let Some(dep) = bzlmod_discovery_dep(
                dep,
                &root.root_module.name,
                &root.archive_overrides,
                &root.single_version_overrides,
                &root.local_path_overrides,
            ) && scheduled.insert((dep.name.clone(), dep.version.clone()))
            {
                frontier.push(dep);
            }
        }

        while !frontier.is_empty() {
            let keys = frontier
                .drain(..)
                .map(|dep| BzlmodModuleFileKey {
                    name: dep.name,
                    version: dep.version,
                })
                .collect::<Vec<_>>();
            let modules: Vec<Arc<DiscoveredBcrModule>> = ctx
                .try_compute_join(keys, |ctx, key| {
                    async move { ctx.compute(&key).await? }.boxed()
                })
                .await?;
            for module in modules {
                let key = (module.dep.name.clone(), module.dep.version.clone());
                for child in &module.deps {
                    if let Some(child) = bzlmod_discovery_dep(
                        child.clone(),
                        &root.root_module.name,
                        &root.archive_overrides,
                        &root.single_version_overrides,
                        &root.local_path_overrides,
                    ) && scheduled.insert((child.name.clone(), child.version.clone()))
                    {
                        frontier.push(child);
                    }
                }
                discovered.insert(key, (*module).clone());
            }
        }
        Ok(Arc::new(BzlmodDiscoveryResult { discovered }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodSelectionResult {
    discovered: DiscoveredBcrModules,
    selected_keys: BTreeSet<(String, String)>,
    selected_keys_in_bfs_order: Vec<(String, String)>,
    selected_keys_in_dependency_order: Vec<(String, String)>,
    canonical_repo_names_by_key: BTreeMap<(String, String), String>,
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MODULE_RESOLUTION")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModuleResolutionKey;

#[async_trait::async_trait]
impl Key for BzlmodModuleResolutionKey {
    type Value = buck2_error::Result<Arc<BzlmodSelectionResult>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let discovery = ctx.compute(&BzlmodDiscoveryKey).await??;
        let selection = select_bzlmod_modules(
            &root.root_deps,
            root.builtin_bazel_tools_module.clone(),
            &discovery.discovered,
        )?;
        collect_bzlmod_yanked_versions(ctx, &root, &selection).await?;
        Ok(Arc::new(selection))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MODULE_RESOLUTION(selection)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodSelectionKey;

#[async_trait::async_trait]
impl Key for BzlmodSelectionKey {
    type Value = buck2_error::Result<Arc<BzlmodSelectionResult>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        ctx.compute(&BzlmodModuleResolutionKey).await?
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REPO_SPEC({}@{})", name, version)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRepoSpecKey {
    name: String,
    version: String,
}

#[async_trait::async_trait]
impl Key for BzlmodRepoSpecKey {
    type Value = buck2_error::Result<Arc<BzlmodRepoSpecValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let registry = ctx.compute(&bzlmod_default_registry_key()).await??;
        let _module_file = ctx
            .compute(&BzlmodModuleFileKey {
                name: self.name.clone(),
                version: self.version.clone(),
            })
            .await??;
        let dep = BazelDep {
            name: self.name.clone(),
            version: self.version.clone(),
            apparent_name: None,
        };
        let project_fs = ctx.global_data().get_io_provider().project_root().dupe();
        let client = shared_bzlmod_http_client().await?;
        let repo = format!("{}@{}", dep.name, dep.version);
        let source_json_path = format!("modules/{}/{}/source.json", dep.name, dep.version);
        let source_json_url = format!("{}/{}", registry.url, source_json_path);
        let source_json = bzlmod_repo_progress_span(
            repo,
            source_json_path,
            "registry repo spec",
            "fetching source.json",
            fetch_bcr_module_source_json(
                &project_fs,
                &registry.url,
                client,
                &dep,
                root.archive_overrides.get(&dep.name),
                root.local_path_overrides.get(&dep.name),
            ),
        )
        .await?;
        let registry_file_hashes = registry
            .registry_file_hashes
            .iter()
            .filter(|(url, _hash)| *url == &source_json_url)
            .map(|(url, hash)| (url.clone(), hash.clone()))
            .collect();
        Ok(Arc::new(BzlmodRepoSpecValue {
            source_json,
            registry_file_hashes,
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_DEP_GRAPH")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodDepGraphKey;

#[async_trait::async_trait]
impl Key for BzlmodDepGraphKey {
    type Value = buck2_error::Result<Arc<BzlmodDepGraph>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let lockfile = ctx.compute(&BzlmodLockFileKey).await??;
        let hidden_lockfile = ctx.compute(&BzlmodHiddenLockFileKey).await??;
        let selection = ctx.compute(&BzlmodSelectionKey).await??;
        let repo_spec_keys = selection
            .selected_keys
            .iter()
            .filter(|(name, _version)| name != "bazel_tools")
            .map(|(name, version)| BzlmodRepoSpecKey {
                name: name.clone(),
                version: version.clone(),
            })
            .collect::<Vec<_>>();
        let repo_specs: Vec<(BzlmodRepoSpecKey, Arc<BzlmodRepoSpecValue>)> = ctx
            .try_compute_join(repo_spec_keys, |ctx, key| {
                async move {
                    let repo_spec = ctx.compute(&key).await??;
                    buck2_error::Ok((key, repo_spec))
                }
                .boxed()
            })
            .await?;
        let mut discovered = selection.discovered.clone();
        for (key, repo_spec) in repo_specs {
            let _registry_file_hashes = &repo_spec.registry_file_hashes;
            if let Some(module) = discovered.get_mut(&(key.name, key.version)) {
                module.source_json = repo_spec.source_json.clone();
            }
        }
        let mut root_module = root.root_module.clone();
        root_module.lockfile_extension_generated_repos = lockfile.extension_generated_repos.clone();
        for (extension_key, repo_names) in &hidden_lockfile.extension_generated_repos {
            root_module
                .lockfile_extension_generated_repos
                .entry(extension_key.clone())
                .or_default()
                .extend(repo_names.iter().cloned());
        }
        root_module.lockfile_extension_facts = lockfile
            .extension_facts
            .union(&hidden_lockfile.extension_facts)
            .cloned()
            .collect();
        Ok(Arc::new(bzlmod_dep_graph_from_selection(
            &root.root_deps,
            root_module,
            discovered,
            &selection,
        )?))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("SINGLE_EXTENSION_USAGES({extension_id:?})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodSingleExtensionUsagesKey {
    extension_id: BzlmodExtensionId,
}

#[async_trait::async_trait]
impl Key for BzlmodSingleExtensionUsagesKey {
    type Value = buck2_error::Result<Arc<BzlmodSingleExtensionUsagesValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        let unique_name = dep_graph
            .extension_unique_names
            .get(&self.extension_id)
            .ok_or_else(|| {
                buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "bzlmod module extension `{}//{}%{}` has no usages",
                    self.extension_id.bzl_cell_name,
                    self.extension_id.bzl_path,
                    self.extension_id.extension_name
                )
            })?
            .clone();
        let extension_usages_json = bzlmod_module_extension_evaluation_config_json(
            &dep_graph.root_module,
            &dep_graph.discovered,
            &dep_graph.selected_keys_in_bfs_order,
            &dep_graph.canonical_repo_names_by_key,
            &dep_graph.canonical_repo_names_by_cell,
            &dep_graph.extension_eval_cell_aliases_by_cell,
            &self.extension_id,
            &dep_graph.extension_unique_names,
        )?;
        Ok(Arc::new(BzlmodSingleExtensionUsagesValue {
            extension_id: self.extension_id.clone(),
            unique_name,
            extension_usages_json,
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MODULE_RESOLUTION(full)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodResolutionKey;

#[async_trait::async_trait]
impl Key for BzlmodResolutionKey {
    type Value = buck2_error::Result<Arc<BazelModuleCellAliases>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let registry = ctx.compute(&bzlmod_default_registry_key()).await??;
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        let extension_usage_keys = dep_graph
            .extension_unique_names
            .keys()
            .cloned()
            .map(|extension_id| BzlmodSingleExtensionUsagesKey { extension_id })
            .collect::<Vec<_>>();
        let single_extension_usages: Vec<Arc<BzlmodSingleExtensionUsagesValue>> = ctx
            .try_compute_join(extension_usage_keys, |ctx, key| {
                async move {
                    let value = ctx.compute(&key).await??;
                    buck2_error::Ok(value)
                }
                .boxed()
            })
            .await?;
        let extension_usages_json_by_id = single_extension_usages
            .iter()
            .map(|value| {
                (
                    value.extension_id.clone(),
                    value.extension_usages_json.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let BcrResolution {
            external_modules,
            root_aliases,
            cell_aliases,
            registered_toolchains,
        } = resolve_bcr_modules_from_dep_graph(
            &registry.url,
            &dep_graph,
            &root.archive_overrides,
            &root.single_version_overrides,
            &root.local_path_overrides,
            &extension_usages_json_by_id,
        )?;
        let mut aliases = root.aliases.clone();
        aliases.external_modules = external_modules;
        aliases.root_aliases.extend(root_aliases);
        aliases.cell_aliases = cell_aliases;
        aliases.registered_toolchains.extend(registered_toolchains);
        aliases.normalize();
        let root_cell_name = ctx
            .get_cell_resolver()
            .await?
            .root_cell()
            .as_str()
            .to_owned();
        register_bzlmod_cell_canonical_repo_name_for_cell(&root_cell_name, "");
        // Bazel keeps repository mappings as Skyframe values and consumers reuse the computed value.
        // Register Buck2's global lookup side effects with the DICE value computation instead of
        // repeating them on every accessor of the cached resolution.
        aliases.register_for_starlark_label_resolution(&root_cell_name);
        aliases.register_external_cell_origins()?;
        Ok(Arc::new(aliases))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MODULE_INSPECTION")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModuleInspectionKey;

#[async_trait::async_trait]
impl Key for BzlmodModuleInspectionKey {
    type Value = buck2_error::Result<Arc<BzlmodModuleInspectionValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let _root = ctx.compute(&BzlmodRootModuleKey).await??;
        let _module_resolution = ctx.compute(&BzlmodModuleResolutionKey).await??;
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        Ok(Arc::new(bzlmod_module_inspection_value(&dep_graph)?))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MOD_TIDY")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModTidyKey;

#[async_trait::async_trait]
impl Key for BzlmodModTidyKey {
    type Value = buck2_error::Result<Arc<BzlmodModTidyValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        let mut keys = BTreeSet::new();
        for usage in &root.root_module.extension_usages {
            keys.insert(bzlmod_resolve_extension_id(
                "root",
                usage,
                &dep_graph.extension_eval_cell_aliases_by_cell,
            )?);
        }
        let root_extension_usages = ctx
            .try_compute_join(
                keys.into_iter()
                    .map(|extension_id| BzlmodSingleExtensionUsagesKey { extension_id })
                    .collect::<Vec<_>>(),
                |ctx, key| async move { ctx.compute(&key).await? }.boxed(),
            )
            .await?
            .into_iter()
            .map(|value| (*value).clone())
            .collect();
        let mut errors = Vec::new();
        for usage in &root_extension_usages {
            let setup = bzlmod_single_extension_eval_setup(usage);
            if let Err(error) = ctx.evaluate_bzlmod_module_extension(setup).await {
                errors.push(error.to_string());
            }
        }
        Ok(Arc::new(BzlmodModTidyValue {
            root_extension_usages,
            errors,
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REPO_DEFINITION({canonical_repo_name})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRepoDefinitionKey {
    canonical_repo_name: String,
}

#[async_trait::async_trait]
impl Key for BzlmodRepoDefinitionKey {
    type Value = buck2_error::Result<Arc<BzlmodRepoDefinitionValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let aliases = ctx.compute(&BzlmodResolutionKey).await??;
        let Some(module) = aliases
            .external_modules
            .iter()
            .find(|module| module.canonical_repo_name() == self.canonical_repo_name)
            .cloned()
        else {
            return Ok(Arc::new(BzlmodRepoDefinitionValue {
                module: None,
                configure: false,
            }));
        };
        let configure = bzlmod_external_module_is_configure_repo(&module);
        Ok(Arc::new(BzlmodRepoDefinitionValue {
            module: Some(module),
            configure,
        }))
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
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REPOSITORY_DIRECTORY({canonical_repo_name})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRepositoryDirectoryKey {
    canonical_repo_name: String,
}

#[async_trait::async_trait]
impl Key for BzlmodRepositoryDirectoryKey {
    type Value = buck2_error::Result<Arc<BzlmodRepositoryDirectoryValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let repo_definition = ctx
            .compute(&BzlmodRepoDefinitionKey {
                canonical_repo_name: self.canonical_repo_name.clone(),
            })
            .await??;
        let Some(module) = repo_definition.module.as_ref() else {
            return Ok(Arc::new(BzlmodRepositoryDirectoryValue {
                found: false,
                exclude_from_vendoring: true,
            }));
        };
        let local = bzlmod_external_module_is_local(module);
        Ok(Arc::new(BzlmodRepositoryDirectoryValue {
            found: true,
            exclude_from_vendoring: local || repo_definition.configure,
        }))
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
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_FETCH_ALL(configure={configure})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodFetchAllKey {
    configure: bool,
}

#[async_trait::async_trait]
impl Key for BzlmodFetchAllKey {
    type Value = buck2_error::Result<Arc<BzlmodFetchAllValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let aliases = ctx.compute(&BzlmodResolutionKey).await??;
        let mut repos_to_vendor = aliases
            .external_modules
            .iter()
            .map(|module| module.canonical_repo_name().to_owned())
            .collect::<Vec<_>>();
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        let extension_usage_keys = dep_graph
            .extension_unique_names
            .keys()
            .cloned()
            .map(|extension_id| BzlmodSingleExtensionUsagesKey { extension_id })
            .collect::<Vec<_>>();
        let single_extension_usages: Vec<Arc<BzlmodSingleExtensionUsagesValue>> = ctx
            .try_compute_join(extension_usage_keys, |ctx, key| {
                async move {
                    let value = ctx.compute(&key).await??;
                    buck2_error::Ok(value)
                }
                .boxed()
            })
            .await?;
        let extension_eval_setups = single_extension_usages
            .iter()
            .map(|usage| bzlmod_single_extension_eval_setup(usage))
            .collect::<Vec<_>>();
        let extension_generated_repos: Vec<Vec<String>> = ctx
            .try_compute_join(extension_eval_setups, |ctx, setup| {
                async move {
                    let unique_name = setup.extension_unique_name.to_string();
                    let repo_names = ctx.evaluate_bzlmod_module_extension(setup).await?;
                    buck2_error::Ok(
                        repo_names
                            .into_iter()
                            .map(|repo_name| format!("{unique_name}+{repo_name}"))
                            .collect::<Vec<_>>(),
                    )
                }
                .boxed()
            })
            .await?;
        repos_to_vendor.extend(extension_generated_repos.into_iter().flatten());
        if self.configure {
            let repo_definitions: Vec<(BzlmodRepoDefinitionKey, Arc<BzlmodRepoDefinitionValue>)> =
                ctx.try_compute_join(
                    repos_to_vendor
                        .iter()
                        .cloned()
                        .map(|canonical_repo_name| BzlmodRepoDefinitionKey {
                            canonical_repo_name,
                        })
                        .collect::<Vec<_>>(),
                    |ctx, key| {
                        async move {
                            let value = ctx.compute(&key).await??;
                            buck2_error::Ok((key, value))
                        }
                        .boxed()
                    },
                )
                .await?;
            repos_to_vendor = repo_definitions
                .into_iter()
                .filter_map(|(key, value)| value.configure.then_some(key.canonical_repo_name))
                .collect();
        }
        let repo_directories: Vec<(
            BzlmodRepositoryDirectoryKey,
            Arc<BzlmodRepositoryDirectoryValue>,
        )> = ctx
            .try_compute_join(
                repos_to_vendor
                    .iter()
                    .cloned()
                    .map(|canonical_repo_name| BzlmodRepositoryDirectoryKey {
                        canonical_repo_name,
                    })
                    .collect::<Vec<_>>(),
                |ctx, key| {
                    async move {
                        let value = ctx.compute(&key).await??;
                        buck2_error::Ok((key, value))
                    }
                    .boxed()
                },
            )
            .await?;
        repos_to_vendor = repo_directories
            .into_iter()
            .filter_map(|(key, value)| {
                (value.found && !value.exclude_from_vendoring).then_some(key.canonical_repo_name)
            })
            .collect();
        repos_to_vendor.sort_unstable();
        repos_to_vendor.dedup();
        Ok(Arc::new(BzlmodFetchAllValue { repos_to_vendor }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("VENDOR_FILE")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodVendorFileKey;

#[async_trait::async_trait]
impl Key for BzlmodVendorFileKey {
    type Value = buck2_error::Result<Arc<BzlmodVendorFileValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let root_path = resolver.root_cell_instance().path().to_buf();
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let mut file_ops = DiceConfigFileOps::new(ctx, project_fs, &resolver);
        Ok(Arc::new(
            bzlmod_vendor_file_data(&root_path, &mut file_ops).await?,
        ))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn collect_bzlmod_yanked_versions(
    ctx: &mut DiceComputations<'_>,
    root: &BzlmodRootResolutionInput,
    selection: &BzlmodSelectionResult,
) -> buck2_error::Result<()> {
    let registry = ctx.compute(&bzlmod_default_registry_key()).await??;
    let allowed_yanked_versions = ctx.compute(&BzlmodAllowedYankedVersionsKey).await??;
    let mut keys = Vec::new();
    for (name, version) in &selection.selected_keys {
        if name == "bazel_tools"
            || root.archive_overrides.contains_key(name)
            || root.local_path_overrides.contains_key(name)
        {
            continue;
        }
        if let Some(info) = registry
            .selected_yanked_versions
            .get(&(name.clone(), version.clone()))
        {
            if bzlmod_yanked_version_allowed(&allowed_yanked_versions, name, version) {
                continue;
            }
            return Err(bzlmod_yanked_version_error(name, version, info));
        }
        let source_json_url = format!("{}/modules/{name}/{version}/source.json", registry.url);
        if registry.registry_file_hashes.contains_key(&source_json_url) {
            continue;
        }
        keys.push(BzlmodYankedVersionsKey {
            module_name: name.clone(),
            registry_url: registry.url.clone(),
        });
    }

    let yanked_versions: Vec<(BzlmodYankedVersionsKey, Arc<BzlmodYankedVersionsValue>)> = ctx
        .try_compute_join(keys, |ctx, key| {
            async move {
                let value = ctx.compute(&key).await??;
                buck2_error::Ok((key, value))
            }
            .boxed()
        })
        .await?;
    for (key, value) in yanked_versions {
        let Some(info) = value.yanked_versions.as_ref().and_then(|versions| {
            versions.get(selection.selected_versions_for_name(&key.module_name)?)
        }) else {
            continue;
        };
        let version = selection
            .selected_versions_for_name(&key.module_name)
            .unwrap_or("");
        if bzlmod_yanked_version_allowed(&allowed_yanked_versions, &key.module_name, version) {
            continue;
        }
        return Err(bzlmod_yanked_version_error(&key.module_name, version, info));
    }
    Ok(())
}

fn bzlmod_yanked_version_allowed(
    allowed_yanked_versions: &BzlmodAllowedYankedVersionsValue,
    name: &str,
    version: &str,
) -> bool {
    allowed_yanked_versions.allow_all
        || allowed_yanked_versions
            .modules
            .contains(&(name.to_owned(), version.to_owned()))
}

fn bzlmod_yanked_version_error(name: &str, version: &str, info: &str) -> buck2_error::Error {
    buck2_error!(
        buck2_error::ErrorTag::Input,
        "Yanked version detected in bzlmod dependency graph: {}@{}, for the reason: {}. Use a newer version of this module, record the allowed yanked version in MODULE.bazel.lock with Bazel, or allow it with {}.",
        name,
        version,
        info,
        BZLMOD_ALLOWED_YANKED_VERSIONS_ENV
    )
}

fn bzlmod_allowed_yanked_versions_from_env(
    value: Option<&str>,
) -> buck2_error::Result<BzlmodAllowedYankedVersionsValue> {
    let mut modules = BTreeSet::new();
    let Some(value) = value else {
        return Ok(BzlmodAllowedYankedVersionsValue {
            allow_all: false,
            modules,
        });
    };
    for module in value.split(',') {
        if module.is_empty() {
            continue;
        }
        if module == "all" {
            return Ok(BzlmodAllowedYankedVersionsValue {
                allow_all: true,
                modules: BTreeSet::new(),
            });
        }
        let Some((name, version)) = module.split_once('@') else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Parsing environment variable {}={} failed, module versions must be of the form '<module name>@<version>'",
                BZLMOD_ALLOWED_YANKED_VERSIONS_ENV,
                value
            ));
        };
        if !is_valid_bzlmod_module_name(name) {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Parsing environment variable {}={} failed, invalid module name `{}`",
                BZLMOD_ALLOWED_YANKED_VERSIONS_ENV,
                value,
                name
            ));
        }
        parse_bzlmod_version(version).with_buck_error_context(|| {
            format!(
                "Parsing environment variable {}={} failed, invalid version specified for module `{}`",
                BZLMOD_ALLOWED_YANKED_VERSIONS_ENV, value, name
            )
        })?;
        modules.insert((name.to_owned(), version.to_owned()));
    }
    Ok(BzlmodAllowedYankedVersionsValue {
        allow_all: false,
        modules,
    })
}

impl BzlmodSelectionResult {
    fn selected_versions_for_name(&self, name: &str) -> Option<&str> {
        self.selected_keys
            .iter()
            .find_map(|(selected_name, version)| {
                (selected_name == name).then_some(version.as_str())
            })
    }
}

fn bzlmod_discovery_dep(
    mut dep: BazelDep,
    root_module_name: &str,
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, BzlmodSingleVersionOverride>,
    local_path_overrides: &BTreeMap<String, BzlmodLocalPathOverride>,
) -> Option<BazelDep> {
    if dep.name == root_module_name {
        return None;
    }
    if archive_overrides.contains_key(&dep.name) {
        dep.version.clear();
    } else if local_path_overrides.contains_key(&dep.name) {
        dep.version.clear();
    } else if let Some(version_override) = single_version_overrides.get(&dep.name)
        && let Some(version) = &version_override.version
    {
        dep.version = version.clone();
    }
    Some(dep)
}

fn select_bzlmod_modules(
    root_deps: &[BazelDep],
    builtin_bazel_tools_module: DiscoveredBcrModule,
    discovered: &DiscoveredBcrModules,
) -> buck2_error::Result<BzlmodSelectionResult> {
    let mut discovered = discovered.clone();
    discovered.insert(
        (
            builtin_bazel_tools_module.dep.name.clone(),
            builtin_bazel_tools_module.dep.version.clone(),
        ),
        builtin_bazel_tools_module,
    );
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
    for dep in root_deps {
        if let Some(version) = selected_versions.get(&dep.name) {
            visit.push_back((dep.name.clone(), version.clone()));
        }
    }
    if let Some(version) = selected_versions.get("bazel_tools") {
        visit.push_back(("bazel_tools".to_owned(), version.clone()));
    }
    let mut selected_keys_in_bfs_order = Vec::new();
    while let Some(key) = visit.pop_front() {
        if !selected_keys.insert(key.clone()) {
            continue;
        }
        selected_keys_in_bfs_order.push(key.clone());
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
        root_deps,
        &selected_versions,
        &selected_keys,
    );
    let canonical_repo_names_by_key = bzlmod_canonical_repo_names_by_key(&selected_keys);
    Ok(BzlmodSelectionResult {
        discovered,
        selected_keys,
        selected_keys_in_bfs_order,
        selected_keys_in_dependency_order,
        canonical_repo_names_by_key,
    })
}

async fn bzlmod_repo_progress_span<T, Fut>(
    repo: String,
    path: String,
    kind: &'static str,
    progress: &'static str,
    fut: Fut,
) -> buck2_error::Result<T>
where
    Fut: Future<Output = buck2_error::Result<T>>,
{
    buck2_events::dispatch::span_async(
        buck2_data::BzlmodRepoStart {
            repo: repo.clone(),
            path: path.clone(),
            kind: kind.to_owned(),
            progress: progress.to_owned(),
        },
        async {
            (
                fut.await,
                buck2_data::BzlmodRepoEnd {
                    repo,
                    path,
                    kind: kind.to_owned(),
                },
            )
        },
    )
    .await
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

async fn shared_bzlmod_http_client() -> buck2_error::Result<HttpClient> {
    Ok(BZLMOD_HTTP_CLIENT
        .get_or_try_init(bzlmod_http_client)
        .await?
        .dupe())
}

fn apply_bzlmod_dep_overrides(
    deps: &mut [BazelDep],
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, BzlmodSingleVersionOverride>,
    local_path_overrides: &BTreeMap<String, BzlmodLocalPathOverride>,
) {
    for dep in deps {
        if archive_overrides.contains_key(&dep.name) {
            dep.version.clear();
        } else if local_path_overrides.contains_key(&dep.name) {
            dep.version.clear();
        } else if let Some(version_override) = single_version_overrides.get(&dep.name) {
            if let Some(version) = &version_override.version {
                dep.version = version.clone();
            }
        }
    }
}

fn bzlmod_dep_graph_from_selection(
    root_deps: &[BazelDep],
    root_module: RootBzlmodModule,
    discovered: DiscoveredBcrModules,
    selection: &BzlmodSelectionResult,
) -> buck2_error::Result<BzlmodDepGraph> {
    let selected_keys = selection.selected_keys.clone();
    let selected_keys_in_bfs_order = selection.selected_keys_in_bfs_order.clone();
    let selected_keys_in_dependency_order = selection.selected_keys_in_dependency_order.clone();
    let canonical_repo_names_by_key = selection.canonical_repo_names_by_key.clone();
    let selected_versions = selected_keys
        .iter()
        .map(|(name, version)| (name.clone(), version.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut root_aliases_by_key = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let mut cell_aliases_by_cell = BzlmodCellAliasesByCell::default();
    if !root_module.repo_name.is_empty() {
        add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "root",
            &root_module.repo_name,
            "root",
        );
    }
    for dep in root_deps {
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
        let cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
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
            if dep.name == root_module.name {
                if let Some(alias) = dep.apparent_name.as_ref() {
                    add_bzlmod_cell_alias(&mut cell_aliases_by_cell, &cell_name, alias, "root");
                }
                continue;
            }
            add_bzlmod_dep_cell_alias(
                &cell_name,
                dep,
                &selected_versions,
                &canonical_repo_names_by_key,
                &mut cell_aliases_by_cell,
            )?;
        }
    }

    let mut canonical_repo_names_by_cell = BTreeMap::<String, String>::new();
    canonical_repo_names_by_cell.insert("bazel_tools".to_owned(), "bazel_tools".to_owned());
    canonical_repo_names_by_cell.insert("root".to_owned(), root_module.canonical_repo_name.clone());
    for key in &selected_keys {
        let canonical_repo_name = canonical_repo_names_by_key
            .get(key)
            .expect("selected key should have canonical repo name")
            .clone();
        canonical_repo_names_by_cell.insert(
            bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name),
            canonical_repo_name,
        );
    }
    let extension_unique_names = bzlmod_extension_unique_names(
        &root_module,
        &discovered,
        &selected_keys,
        &canonical_repo_names_by_key,
        &cell_aliases_by_cell,
        &canonical_repo_names_by_cell,
    )?;
    let extension_eval_cell_aliases_by_cell = bzlmod_extension_eval_cell_aliases_by_cell(
        &root_module,
        &discovered,
        &selected_keys,
        &canonical_repo_names_by_key,
        &cell_aliases_by_cell,
        &extension_unique_names,
    )?;

    Ok(BzlmodDepGraph {
        root_module,
        discovered,
        selected_keys,
        selected_keys_in_bfs_order,
        selected_keys_in_dependency_order,
        canonical_repo_names_by_key,
        canonical_repo_names_by_cell,
        root_aliases_by_key,
        cell_aliases_by_cell,
        extension_eval_cell_aliases_by_cell,
        extension_unique_names,
    })
}

fn resolve_bcr_modules_from_dep_graph(
    registry: &str,
    dep_graph: &BzlmodDepGraph,
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, BzlmodSingleVersionOverride>,
    local_path_overrides: &BTreeMap<String, BzlmodLocalPathOverride>,
    extension_usages_json_by_id: &BTreeMap<BzlmodExtensionId, String>,
) -> buck2_error::Result<BcrResolution> {
    let root_module = &dep_graph.root_module;
    let discovered = &dep_graph.discovered;
    let selected_keys = &dep_graph.selected_keys;
    let canonical_repo_names_by_key = &dep_graph.canonical_repo_names_by_key;
    let mut root_aliases_by_key = dep_graph.root_aliases_by_key.clone();
    let mut cell_aliases_by_cell = dep_graph.cell_aliases_by_cell.clone();

    let mut resolved = BTreeMap::<String, BazelCompatExternalModule>::new();
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let mut aliases = root_aliases_by_key
            .remove(key)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        aliases.sort_unstable();
        aliases.dedup();

        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            &canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let archive_override = archive_overrides.get(&module.dep.name);
        let single_version_override = single_version_overrides.get(&module.dep.name);
        let local_path_override = local_path_overrides.get(&module.dep.name);
        let patch_configs = bzlmod_patch_configs(
            registry,
            &module.dep,
            &module.source_json,
            archive_override,
            single_version_override,
        );
        let patches_json = serde_json::to_string(&patch_configs)
            .buck_error_context("Error serializing bzlmod patch configuration")?;
        let overlay_configs = bzlmod_overlay_configs(registry, &module.dep, &module.source_json);
        let overlays_json = serde_json::to_string(&overlay_configs)
            .buck_error_context("Error serializing bzlmod overlay configuration")?;
        let patch_strip = archive_override
            .and_then(|archive_override| archive_override.patch_strip)
            .or_else(|| {
                single_version_override.and_then(|version_override| version_override.patch_strip)
            })
            .or(module.source_json.patch_strip)
            .unwrap_or(0);
        let urls = bcr_source_urls(&module.source_json);
        let urls_json =
            serde_json::to_string(&urls).buck_error_context("Error serializing bzlmod URLs")?;
        let cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        if module.dep.name == "bazel_tools" {
            continue;
        }
        resolved.insert(
            cell_name.clone(),
            BazelCompatExternalModule::Registry(BazelCompatRegistryModule {
                cell_name,
                aliases,
                module_name: module.dep.name.clone(),
                version: module.dep.version.clone(),
                canonical_repo_name,
                local_path: local_path_override
                    .map(|local_path_override| local_path_override.path.clone()),
                url: module.source_json.url.clone(),
                urls_json,
                integrity: module.source_json.integrity.clone(),
                strip_prefix: module.source_json.strip_prefix.clone(),
                archive_type: module.source_json.archive_type.clone(),
                patches_json,
                overlays_json,
                patch_strip,
            }),
        );
    }

    let mut resolved = resolved.into_values().collect::<Vec<_>>();
    let generated_resolution = resolve_generated_bzlmod_repos(
        root_module,
        discovered,
        &dep_graph.selected_keys_in_dependency_order,
        canonical_repo_names_by_key,
        &mut cell_aliases_by_cell,
        &dep_graph.canonical_repo_names_by_cell,
        &dep_graph.extension_unique_names,
        extension_usages_json_by_id,
    )?;
    resolved.extend(generated_resolution.external_modules);
    let registered_toolchains = resolve_bzlmod_registered_toolchains(
        root_module,
        discovered,
        &dep_graph.selected_keys_in_bfs_order,
        canonical_repo_names_by_key,
        &cell_aliases_by_cell,
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
    })
}

fn bzlmod_extension_eval_cell_aliases_by_cell(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    base_cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
) -> buck2_error::Result<BzlmodCellAliasesByCell> {
    let mut cell_aliases_by_cell = base_cell_aliases_by_cell.clone();
    for usage in &root_module.extension_usages {
        add_bzlmod_extension_usage_eval_aliases(
            usage,
            "root",
            base_cell_aliases_by_cell,
            &mut cell_aliases_by_cell,
            extension_unique_names,
        )?;
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
        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        for usage in &module.extension_usages {
            add_bzlmod_extension_usage_eval_aliases(
                usage,
                &module_cell_name,
                base_cell_aliases_by_cell,
                &mut cell_aliases_by_cell,
                extension_unique_names,
            )?;
        }
    }
    Ok(cell_aliases_by_cell)
}

fn add_bzlmod_extension_usage_eval_aliases(
    usage: &BzlmodExtensionUsage,
    parent_cell_name: &str,
    base_cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
) -> buck2_error::Result<()> {
    let resolved_extension = bzlmod_resolve_extension(
        parent_cell_name,
        usage,
        base_cell_aliases_by_cell,
        extension_unique_names,
    )?;
    let repo_override_targets = bzlmod_extension_repo_override_targets(
        usage,
        parent_cell_name,
        base_cell_aliases_by_cell,
        &resolved_extension,
    )?;
    for import in &usage.imports {
        if let Some(target_cell_name) = repo_override_targets.get(&import.repo_name) {
            add_bzlmod_cell_alias(
                cell_aliases_by_cell,
                parent_cell_name,
                &import.alias,
                target_cell_name,
            );
            continue;
        }
        let canonical_repo_name =
            bzlmod_extension_repo_canonical_repo_name(&resolved_extension, &import.repo_name);
        let target_cell_name = bzlmod_cell_name(&canonical_repo_name);
        add_bzlmod_cell_alias(
            cell_aliases_by_cell,
            parent_cell_name,
            &import.alias,
            &target_cell_name,
        );
    }
    Ok(())
}

fn bzlmod_module_inspection_value(
    dep_graph: &BzlmodDepGraph,
) -> buck2_error::Result<BzlmodModuleInspectionValue> {
    let mut modules = Vec::new();
    let mut modules_index = BTreeMap::<String, BTreeSet<String>>::new();
    let mut extension_to_repo_internal_names =
        BTreeMap::<BzlmodExtensionId, BTreeSet<String>>::new();

    modules.push(BzlmodInspectionModule {
        name: dep_graph.root_module.name.clone(),
        version: dep_graph.root_module.version.clone(),
        canonical_repo_name: dep_graph.root_module.canonical_repo_name.clone(),
        selected: true,
        deps: Vec::new(),
    });
    modules_index
        .entry(dep_graph.root_module.name.clone())
        .or_default()
        .insert(dep_graph.root_module.version.clone());
    for usage in &dep_graph.root_module.extension_usages {
        bzlmod_add_inspection_extension_repo_names(
            usage,
            "root",
            dep_graph,
            &mut extension_to_repo_internal_names,
        )?;
    }

    for (key, module) in &dep_graph.discovered {
        let selected = dep_graph.selected_keys.contains(key);
        let canonical_repo_name = dep_graph
            .canonical_repo_names_by_key
            .get(key)
            .cloned()
            .unwrap_or_else(|| {
                bzlmod_canonical_repo_name(&module.dep.name, &module.dep.version, false)
            });
        modules.push(BzlmodInspectionModule {
            name: module.dep.name.clone(),
            version: module.dep.version.clone(),
            canonical_repo_name: canonical_repo_name.clone(),
            selected,
            deps: module.deps.clone(),
        });
        modules_index
            .entry(module.dep.name.clone())
            .or_default()
            .insert(module.dep.version.clone());

        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        for usage in &module.extension_usages {
            bzlmod_add_inspection_extension_repo_names(
                usage,
                &module_cell_name,
                dep_graph,
                &mut extension_to_repo_internal_names,
            )?;
        }
    }

    modules.sort_unstable_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.version.cmp(&b.version))
            .then_with(|| a.canonical_repo_name.cmp(&b.canonical_repo_name))
    });
    Ok(BzlmodModuleInspectionValue {
        modules,
        modules_index,
        extension_to_repo_internal_names,
        module_key_to_canonical_repo_name: dep_graph.canonical_repo_names_by_key.clone(),
        errors: Vec::new(),
    })
}

fn bzlmod_add_inspection_extension_repo_names(
    usage: &BzlmodExtensionUsage,
    parent_cell_name: &str,
    dep_graph: &BzlmodDepGraph,
    extension_to_repo_internal_names: &mut BTreeMap<BzlmodExtensionId, BTreeSet<String>>,
) -> buck2_error::Result<()> {
    let resolved_extension = bzlmod_resolve_extension(
        parent_cell_name,
        usage,
        &dep_graph.cell_aliases_by_cell,
        &dep_graph.extension_unique_names,
    )?;
    let repo_names = extension_to_repo_internal_names
        .entry(resolved_extension.id.clone())
        .or_default();
    repo_names.extend(usage.imports.iter().map(|import| import.repo_name.clone()));
    repo_names.extend(bzlmod_extension_tag_repo_names(usage));
    repo_names.extend(
        usage
            .repo_overrides
            .iter()
            .filter(|repo_override| repo_override.must_exist)
            .map(|repo_override| repo_override.repo_name.clone()),
    );
    let lockfile_extension_key = bzlmod_lockfile_extension_key(
        &resolved_extension.id,
        &dep_graph.canonical_repo_names_by_cell,
    )?;
    if let Some(lockfile_repo_names) = dep_graph
        .root_module
        .lockfile_extension_generated_repos
        .get(&lockfile_extension_key)
    {
        repo_names.extend(lockfile_repo_names.iter().cloned());
    }
    Ok(())
}

struct GeneratedBzlmodReposResolution {
    external_modules: Vec<BazelCompatExternalModule>,
}

fn resolve_generated_bzlmod_repos(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys_in_dependency_order: &[(String, String)],
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
    extension_usages_json_by_id: &BTreeMap<BzlmodExtensionId, String>,
) -> buck2_error::Result<GeneratedBzlmodReposResolution> {
    let mut generated = Vec::new();
    let mut generated_repo_declaring_cells = Vec::new();
    let mut extension_generated_repo_groups = BTreeMap::<String, Vec<(String, String)>>::new();
    let mut extension_repo_override_groups = BTreeMap::<String, Vec<(String, String)>>::new();
    resolve_bzlmod_use_repo_rule_generated_repos(
        &root_module.use_repo_rule_invocations,
        &root_module.canonical_repo_name,
        "root",
        true,
        cell_aliases_by_cell,
        &mut generated,
        &mut generated_repo_declaring_cells,
    )?;
    let local_config_xcode_generator_json = serde_json::to_string(
        &BzlmodGeneratedCellGenerator::XcodeConfig(BzlmodXcodeConfigSetup {}),
    )
    .buck_error_context("Error serializing generated Xcode config repo configuration")?;
    let local_config_xcode_canonical_repo_name =
        "bazel_tools+xcode_configure_extension+local_config_xcode";
    let local_config_xcode_cell = add_unimported_generated_bzlmod_repo(
        &mut generated,
        &mut generated_repo_declaring_cells,
        "bazel_tools",
        local_config_xcode_canonical_repo_name,
        local_config_xcode_generator_json,
    );
    add_bzlmod_cell_alias(
        cell_aliases_by_cell,
        "root",
        "local_config_xcode",
        &local_config_xcode_cell,
    );
    add_bzlmod_cell_alias(
        cell_aliases_by_cell,
        "bazel_tools",
        "local_config_xcode",
        &local_config_xcode_cell,
    );
    for key in selected_keys_in_dependency_order {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let parent_canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let parent_cell_name =
            bzlmod_cell_name_for_canonical_repo_name(&parent_canonical_repo_name);
        resolve_bzlmod_use_repo_rule_generated_repos(
            &module.use_repo_rule_invocations,
            &parent_canonical_repo_name,
            &parent_cell_name,
            false,
            cell_aliases_by_cell,
            &mut generated,
            &mut generated_repo_declaring_cells,
        )?;
        // These built-ins are normally emitted by module extensions. Keep
        // static placeholders for known imports so the cell graph can stay
        // demand-driven and defer real extension evaluation until a generated
        // repo is materialized.
        if module.dep.name == "rules_shell" {
            for alias in &module.use_repo_aliases {
                if alias != "local_config_shell" {
                    continue;
                }
                let canonical_repo_name =
                    format!("{parent_canonical_repo_name}+sh_configure+{alias}");
                let generator_json = serde_json::to_string(
                    &BzlmodGeneratedCellGenerator::ShellConfig(BzlmodShellConfigSetup {}),
                )
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
                let generator_json = serde_json::to_string(
                    &BzlmodGeneratedCellGenerator::HostPlatform(BzlmodHostPlatformSetup {}),
                )
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
                        Some(BzlmodGeneratedCellGenerator::BazelFeaturesGlobals(
                            BzlmodBazelFeaturesGlobalsSetup {
                                parent_canonical_repo_name: Arc::from(
                                    parent_canonical_repo_name.clone(),
                                ),
                                bazel_version: Arc::from(BZLMOD_BAZEL_COMPAT_VERSION),
                            },
                        ))
                    }
                    "bazel_features_version" => {
                        Some(BzlmodGeneratedCellGenerator::BazelFeaturesVersion(
                            BzlmodBazelFeaturesVersionSetup {
                                bazel_version: Arc::from(BZLMOD_BAZEL_COMPAT_VERSION),
                            },
                        ))
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
                cell_aliases_by_cell,
                canonical_repo_names_by_cell,
                extension_unique_names,
                extension_usages_json_by_id,
                &mut generated,
                &mut generated_repo_declaring_cells,
                &mut extension_generated_repo_groups,
                &mut extension_repo_override_groups,
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
            cell_aliases_by_cell,
            canonical_repo_names_by_cell,
            extension_unique_names,
            extension_usages_json_by_id,
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut extension_generated_repo_groups,
            &mut extension_repo_override_groups,
        )?;
    }

    add_generated_bzlmod_repo_mappings(
        cell_aliases_by_cell,
        &generated_repo_declaring_cells,
        &extension_generated_repo_groups,
        &extension_repo_override_groups,
    );
    Ok(GeneratedBzlmodReposResolution {
        external_modules: generated,
    })
}

fn resolve_bzlmod_extension_usage_generated_repos(
    usage: &BzlmodExtensionUsage,
    parent_canonical_repo_name: &str,
    parent_cell_name: &str,
    parent_is_root: bool,
    root_module: &RootBzlmodModule,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
    extension_usages_json_by_id: &BTreeMap<BzlmodExtensionId, String>,
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    extension_generated_repo_groups: &mut BTreeMap<String, Vec<(String, String)>>,
    extension_repo_override_groups: &mut BTreeMap<String, Vec<(String, String)>>,
) -> buck2_error::Result<()> {
    let resolved_extension = bzlmod_resolve_extension(
        parent_cell_name,
        usage,
        cell_aliases_by_cell,
        extension_unique_names,
    )?;
    let extension_group_key = resolved_extension.unique_name.clone();
    let extension_host_cell_name = resolved_extension.id.bzl_cell_name.clone();
    let mut existing_generated_repos = extension_generated_repo_groups
        .get(&extension_group_key)
        .map(|generated_repos| {
            generated_repos
                .iter()
                .cloned()
                .collect::<StdBuckHashMap<_, _>>()
        })
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
    static_repo_names.extend(
        usage
            .repo_overrides
            .iter()
            .filter(|repo_override| repo_override.must_exist)
            .map(|repo_override| repo_override.repo_name.clone()),
    );
    let lockfile_extension_key =
        bzlmod_lockfile_extension_key(&resolved_extension.id, canonical_repo_names_by_cell)?;
    if let Some(lockfile_repo_names) = root_module
        .lockfile_extension_generated_repos
        .get(&lockfile_extension_key)
    {
        static_repo_names.extend(lockfile_repo_names.iter().cloned());
    }
    let repo_override_targets = bzlmod_extension_repo_override_targets(
        usage,
        parent_cell_name,
        cell_aliases_by_cell,
        &resolved_extension,
    )?;
    if !repo_override_targets.is_empty() {
        extension_repo_override_groups
            .entry(extension_group_key.clone())
            .or_default()
            .extend(
                repo_override_targets
                    .iter()
                    .map(|(repo_name, target_cell_name)| {
                        (repo_name.clone(), target_cell_name.clone())
                    }),
            );
    }
    let extension_usages_json = extension_usages_json_by_id
        .get(&resolved_extension.id)
        .ok_or_else(|| {
            buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod module extension `{}//{}%{}` has no single-extension usages value",
                resolved_extension.id.bzl_cell_name,
                resolved_extension.id.bzl_path,
                resolved_extension.id.extension_name
            )
        })?;
    if static_repo_names.is_empty() {
        return Ok(());
    }
    let mut generated_repo_names = static_repo_names;

    for import in imports_needing_generic_repos {
        if let Some(target_cell_name) = repo_override_targets.get(&import.repo_name) {
            if usage.repo_overrides.iter().any(|repo_override| {
                repo_override.repo_name == import.repo_name && !repo_override.must_exist
            }) {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "bzlmod module extension `{}`%`{}` for `{}` imports injected repository `{}`; refer to `{}` directly",
                    usage.extension_bzl_file,
                    usage.extension_name,
                    parent_canonical_repo_name,
                    import.repo_name,
                    target_cell_name
                ));
            }
            add_bzlmod_cell_alias(
                cell_aliases_by_cell,
                parent_cell_name,
                &import.alias,
                target_cell_name,
            );
            continue;
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
            &resolved_extension,
            parent_canonical_repo_name,
            parent_is_root,
            usage,
            &import.repo_name,
            &extension_usages_json,
        )?)
        .buck_error_context("Error serializing generated module extension repo configuration")?;
        let generated_cell_name = add_generated_bzlmod_repo_with_mapping_cell(
            generated,
            generated_repo_declaring_cells,
            cell_aliases_by_cell,
            parent_cell_name,
            &import.alias,
            &extension_host_cell_name,
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
            &resolved_extension,
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
            &extension_host_cell_name,
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

fn bzlmod_extension_repo_override_targets(
    usage: &BzlmodExtensionUsage,
    parent_cell_name: &str,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    resolved_extension: &BzlmodResolvedExtension,
) -> buck2_error::Result<BTreeMap<String, String>> {
    let mut targets = BTreeMap::new();
    for repo_override in &usage.repo_overrides {
        let Some(target_cell_name) = bzlmod_cell_alias_target(
            cell_aliases_by_cell,
            parent_cell_name,
            &repo_override.overriding_repo_name,
        ) else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod module extension `{}//{}%{}` maps repository `{}` to `{}`, but `{}` is not visible from `{}`",
                resolved_extension.id.bzl_cell_name,
                resolved_extension.id.bzl_path,
                resolved_extension.id.extension_name,
                repo_override.repo_name,
                repo_override.overriding_repo_name,
                repo_override.overriding_repo_name,
                parent_cell_name
            ));
        };
        targets.insert(repo_override.repo_name.clone(), target_cell_name.to_owned());
    }
    Ok(targets)
}

fn add_bzlmod_module_extension_repo_overrides_for_usage(
    usage: &BzlmodExtensionUsage,
    parent_cell_name: &str,
    dep_graph: &BzlmodDepGraph,
    extension_id: &BzlmodExtensionId,
    targets: &mut BTreeMap<String, String>,
) -> buck2_error::Result<()> {
    let resolved_extension = bzlmod_resolve_extension(
        parent_cell_name,
        usage,
        &dep_graph.cell_aliases_by_cell,
        &dep_graph.extension_unique_names,
    )?;
    if &resolved_extension.id != extension_id {
        return Ok(());
    }
    targets.extend(bzlmod_extension_repo_override_targets(
        usage,
        parent_cell_name,
        &dep_graph.cell_aliases_by_cell,
        &resolved_extension,
    )?);
    Ok(())
}

fn bzlmod_module_extension_repo_overrides_for_extension(
    dep_graph: &BzlmodDepGraph,
    extension_id: &BzlmodExtensionId,
) -> buck2_error::Result<Vec<(String, String)>> {
    let mut targets = BTreeMap::new();
    for usage in &dep_graph.root_module.extension_usages {
        add_bzlmod_module_extension_repo_overrides_for_usage(
            usage,
            "root",
            dep_graph,
            extension_id,
            &mut targets,
        )?;
    }
    for key in &dep_graph.selected_keys {
        let Some(module) = dep_graph.discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            &dep_graph.canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        for usage in &module.extension_usages {
            add_bzlmod_module_extension_repo_overrides_for_usage(
                usage,
                &module_cell_name,
                dep_graph,
                extension_id,
                &mut targets,
            )?;
        }
    }
    Ok(targets.into_iter().collect())
}

fn bzlmod_module_extension_evaluation_config_json(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys_in_bfs_order: &[(String, String)],
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    extension_id: &BzlmodExtensionId,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
) -> buck2_error::Result<String> {
    let mut modules = Vec::new();
    let mut usages = Vec::new();
    let mut repo_overrides = BTreeMap::new();
    let mut root_has_usage = false;
    let mut root_module_has_non_dev_dependency = false;
    let mut root_extension_bzl_file = None;
    let mut root_extension_name = None;
    let mut root_tags = Vec::new();
    for usage in &root_module.extension_usages {
        let resolved_extension =
            bzlmod_resolve_extension("root", usage, cell_aliases_by_cell, extension_unique_names)?;
        if &resolved_extension.id != extension_id {
            continue;
        }
        root_has_usage = true;
        root_extension_bzl_file.get_or_insert_with(|| usage.extension_bzl_file.clone());
        root_extension_name.get_or_insert_with(|| usage.extension_name.clone());
        root_module_has_non_dev_dependency |= !usage.dev_dependency;
        usages.push(BzlmodModuleExtensionUsageConfig {
            imports: usage.imports.clone(),
            repo_overrides: usage.repo_overrides.clone(),
        });
        for (repo_name, target_cell_name) in bzlmod_extension_repo_override_targets(
            usage,
            "root",
            cell_aliases_by_cell,
            &resolved_extension,
        )? {
            repo_overrides.insert(
                repo_name,
                bzlmod_canonical_repo_name_for_cell_name(
                    &target_cell_name,
                    canonical_repo_names_by_cell,
                )?,
            );
        }
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
            extension_bzl_file: root_extension_bzl_file.unwrap_or_default(),
            extension_name: root_extension_name.unwrap_or_default(),
            cell_aliases: bzlmod_module_extension_cell_aliases(cell_aliases_by_cell, "root"),
            constants: root_module.constants.clone(),
            tags: root_tags,
        });
    }

    for key in selected_keys_in_bfs_order {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        let mut has_usage = false;
        let mut extension_bzl_file = None;
        let mut extension_name = None;
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
            extension_bzl_file.get_or_insert_with(|| usage.extension_bzl_file.clone());
            extension_name.get_or_insert_with(|| usage.extension_name.clone());
            usages.push(BzlmodModuleExtensionUsageConfig {
                imports: usage.imports.clone(),
                repo_overrides: usage.repo_overrides.clone(),
            });
            for (repo_name, target_cell_name) in bzlmod_extension_repo_override_targets(
                usage,
                &module_cell_name,
                cell_aliases_by_cell,
                &resolved_extension,
            )? {
                repo_overrides.insert(
                    repo_name,
                    bzlmod_canonical_repo_name_for_cell_name(
                        &target_cell_name,
                        canonical_repo_names_by_cell,
                    )?,
                );
            }
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
            extension_bzl_file: extension_bzl_file.unwrap_or_default(),
            extension_name: extension_name.unwrap_or_default(),
            cell_aliases: bzlmod_module_extension_cell_aliases(
                cell_aliases_by_cell,
                &module_cell_name,
            ),
            constants: module.constants.clone(),
            tags,
        });
    }

    serde_json::to_string(&BzlmodModuleExtensionEvaluationConfig {
        root_module_has_non_dev_dependency,
        modules,
        usages,
        repo_overrides: repo_overrides.into_iter().collect(),
    })
    .buck_error_context("Error serializing module extension evaluation configuration")
}

fn bzlmod_canonical_repo_name_for_cell_name(
    cell_name: &str,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
) -> buck2_error::Result<String> {
    canonical_repo_names_by_cell
        .get(cell_name)
        .cloned()
        .ok_or_else(|| {
            buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod cell `{}` does not have a canonical repository name",
                cell_name
            )
        })
}

fn bzlmod_module_extension_cell_aliases(
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    cell_name: &str,
) -> Vec<(String, String)> {
    let mut aliases = cell_aliases_by_cell
        .get(cell_name)
        .into_iter()
        .flat_map(|aliases| {
            aliases
                .iter()
                .map(|(alias, target)| (alias.clone(), target.clone()))
        })
        .collect::<Vec<_>>();
    aliases.sort_unstable();
    aliases.dedup();
    aliases
}

async fn bzlmod_lockfile_data(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> buck2_error::Result<BzlmodModuleLockfileData> {
    let lockfile_path = ConfigPath::Project(
        cell_path
            .as_project_relative_path()
            .join(ForwardRelativePath::new("MODULE.bazel.lock")?),
    );
    let Some(lines) = file_ops.read_file_lines_if_exists(&lockfile_path).await? else {
        return Ok(empty_bzlmod_lockfile_data());
    };
    bzlmod_lockfile_data_from_str(&lines.join("\n"))
}

async fn bzlmod_vendor_file_data(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> buck2_error::Result<BzlmodVendorFileValue> {
    let vendor_file_path = ConfigPath::Project(
        cell_path
            .as_project_relative_path()
            .join(ForwardRelativePath::new("VENDOR.bazel")?),
    );
    let Some(lines) = file_ops
        .read_file_lines_if_exists(&vendor_file_path)
        .await?
    else {
        return Ok(BzlmodVendorFileValue {
            ignored_repos: Vec::new(),
            pinned_repos: Vec::new(),
        });
    };
    Ok(BzlmodVendorFileValue {
        ignored_repos: bzlmod_vendor_repos_from_calls(&lines, "ignore("),
        pinned_repos: bzlmod_vendor_repos_from_calls(&lines, "pin("),
    })
}

fn bzlmod_vendor_repos_from_calls(lines: &[String], function: &str) -> Vec<String> {
    let mut repos = collect_bzl_calls(lines, function)
        .into_iter()
        .flat_map(|call| {
            bzl_call_args(&call)
                .into_iter()
                .filter_map(|arg| bzl_string_value(arg.trim()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    repos.sort_unstable();
    repos.dedup();
    repos
}

fn bzlmod_lockfile_data_from_str(contents: &str) -> buck2_error::Result<BzlmodModuleLockfileData> {
    let lockfile: BzlmodModuleLockfile = serde_json::from_str(contents)
        .buck_error_context("Error parsing MODULE.bazel.lock for bzlmod generated repositories")?;
    let mut repos_by_extension = BTreeMap::new();
    for (extension_key, evaluations) in lockfile.module_extensions {
        for evaluation in evaluations.into_values() {
            repos_by_extension
                .entry(extension_key.clone())
                .or_insert_with(BTreeSet::new)
                .extend(evaluation.generated_repo_specs.into_keys());
        }
    }
    Ok(BzlmodModuleLockfileData {
        registry_file_hashes: lockfile.registry_file_hashes,
        selected_yanked_versions: lockfile
            .selected_yanked_versions
            .into_iter()
            .filter_map(|(key, info)| {
                let (name, version) = key.rsplit_once('@')?;
                Some(((name.to_owned(), version.to_owned()), info))
            })
            .collect(),
        extension_generated_repos: repos_by_extension,
        extension_facts: lockfile.facts.into_keys().collect(),
    })
}

fn bzlmod_lockfile_extension_key(
    extension_id: &BzlmodExtensionId,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
) -> buck2_error::Result<String> {
    let canonical_repo_name = if extension_id.bzl_cell_name == "root" {
        ""
    } else {
        canonical_repo_names_by_cell
            .get(&extension_id.bzl_cell_name)
            .ok_or_else(|| {
                buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "bzlmod module extension `{}//{}%{}` resolves to unknown cell `{}`",
                    extension_id.bzl_cell_name,
                    extension_id.bzl_path,
                    extension_id.extension_name,
                    extension_id.bzl_cell_name
                )
            })?
            .as_str()
    };
    if canonical_repo_name.is_empty() {
        return Ok(format!(
            "//{}%{}",
            bzlmod_bzl_path_to_label_path(&extension_id.bzl_path),
            extension_id.extension_name
        ));
    }
    Ok(format!(
        "@@{}//{}%{}",
        canonical_repo_name,
        bzlmod_bzl_path_to_label_path(&extension_id.bzl_path),
        extension_id.extension_name
    ))
}

fn bzlmod_bzl_path_to_label_path(path: &str) -> String {
    if let Some((package, target)) = path.rsplit_once('/') {
        format!("{package}:{target}")
    } else {
        format!(":{path}")
    }
}

fn bzlmod_module_extension_repo_config(
    resolved_extension: &BzlmodResolvedExtension,
    parent_canonical_repo_name: &str,
    parent_is_root: bool,
    usage: &BzlmodExtensionUsage,
    repo_name: &str,
    extension_usages_json: &str,
) -> buck2_error::Result<BzlmodGeneratedCellGenerator> {
    let extension_usages_key = register_bzlmod_module_extension_usages_json(extension_usages_json);
    Ok(BzlmodGeneratedCellGenerator::ModuleExtensionRepo(
        BzlmodModuleExtensionRepoSetup {
            parent_canonical_repo_name: Arc::from(parent_canonical_repo_name),
            parent_is_root,
            extension_bzl_file: Arc::from(usage.extension_bzl_file.clone()),
            extension_bzl_cell: Arc::from(resolved_extension.id.bzl_cell_name.clone()),
            extension_bzl_path: Arc::from(resolved_extension.id.bzl_path.clone()),
            extension_unique_name: Arc::from(resolved_extension.unique_name.clone()),
            extension_name: Arc::from(usage.extension_name.clone()),
            repo_name: Arc::from(repo_name),
            extension_usages_key,
            extension_usages_json: Arc::from(extension_usages_json),
        },
    ))
}

fn add_generated_bzlmod_repo(
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    declaring_cell_name: &str,
    alias: &str,
    canonical_repo_name: &str,
    generator_json: String,
) -> String {
    add_generated_bzlmod_repo_with_mapping_cell(
        generated,
        generated_repo_declaring_cells,
        cell_aliases_by_cell,
        declaring_cell_name,
        alias,
        declaring_cell_name,
        canonical_repo_name,
        generator_json,
    )
}

fn add_generated_bzlmod_repo_with_mapping_cell(
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    importing_cell_name: &str,
    alias: &str,
    mapping_cell_name: &str,
    canonical_repo_name: &str,
    generator_json: String,
) -> String {
    let cell_name = bzlmod_cell_name(canonical_repo_name);
    add_bzlmod_cell_alias(cell_aliases_by_cell, importing_cell_name, alias, &cell_name);
    add_unimported_generated_bzlmod_repo(
        generated,
        generated_repo_declaring_cells,
        mapping_cell_name,
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
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    generated_repo_declaring_cells: &[(String, String)],
    extension_generated_repo_groups: &BTreeMap<String, Vec<(String, String)>>,
    extension_repo_override_groups: &BTreeMap<String, Vec<(String, String)>>,
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

    for (extension_group_key, generated_repos) in extension_generated_repo_groups {
        let generated_repos = generated_repos
            .iter()
            .cloned()
            .collect::<StdBuckHashMap<_, _>>();
        let mut visible_repos = generated_repos.clone();
        if let Some(repo_overrides) = extension_repo_override_groups.get(extension_group_key) {
            visible_repos.extend(repo_overrides.iter().cloned());
        }
        for generated_cell_name in generated_repos.values() {
            for (repo_name, target_cell_name) in &visible_repos {
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
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
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
        let generator_json =
            serde_json::to_string(&BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(
                BzlmodRepositoryRuleInvocationSetup {
                    repo_name: Arc::from(invocation.repo_name.clone()),
                    rule_bzl_cell: Arc::from(rule_bzl_cell),
                    rule_bzl_path: Arc::from(rule_bzl_path),
                    rule_bzl_build_file_cell: Arc::from(parent_cell_name),
                    rule_name: Arc::from(invocation.rule_name.clone()),
                    attrs: Arc::new(
                        invocation
                            .attrs
                            .iter()
                            .map(|(key, value)| (Arc::from(key.clone()), Arc::from(value.clone())))
                            .collect(),
                    ),
                },
            ))
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
    repo_names.sort_unstable();
    repo_names.dedup();
    repo_names
}

fn bzlmod_extension_unique_names(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
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
        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
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
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
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
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
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
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
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
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &[(String, String)],
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
) -> buck2_error::Result<Vec<String>> {
    let mut registered_toolchains = Vec::new();
    for pattern in &root_module.registered_toolchains {
        registered_toolchains.push(qualify_bzlmod_registered_toolchain(
            pattern,
            "root",
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
        let cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        for pattern in &module.registered_toolchains {
            registered_toolchains.push(qualify_bzlmod_registered_toolchain(
                pattern,
                &cell_name,
                cell_aliases_by_cell,
            )?);
        }
    }
    dedup_preserve_order(&mut registered_toolchains);
    Ok(registered_toolchains)
}

fn qualify_bzlmod_registered_toolchain(
    pattern: &str,
    module_cell_name: &str,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
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
    aliases_by_cell: &mut BzlmodCellAliasesByCell,
) -> buck2_error::Result<()> {
    let Some(alias) = dep.apparent_name.as_ref() else {
        return Ok(());
    };
    let Some(version) = selected_versions.get(&dep.name) else {
        return Ok(());
    };
    let canonical_repo_name =
        bzlmod_selected_canonical_repo_name(canonical_repo_names_by_key, &dep.name, version)?;
    let cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
    add_bzlmod_cell_alias(aliases_by_cell, current_cell_name, alias, &cell_name);
    Ok(())
}

fn add_bzlmod_cell_alias(
    aliases_by_cell: &mut BzlmodCellAliasesByCell,
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
    aliases_by_cell: &'a BzlmodCellAliasesByCell,
    current_cell_name: &str,
    alias: &str,
) -> Option<&'a str> {
    aliases_by_cell
        .get(current_cell_name)
        .and_then(|aliases| aliases.get(alias))
        .map(String::as_str)
}

fn bzlmod_cell_alias_map_to_vec(aliases: BzlmodCellAliasMap) -> Vec<BazelCompatCellAlias> {
    aliases
        .into_iter()
        .map(|(alias, cell_name)| BazelCompatCellAlias { alias, cell_name })
        .collect()
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Http)]
enum BcrHttpGetError {
    #[error("Error fetching `{url}`")]
    Fetch {
        url: String,
        #[source]
        source: RetryingHttpError,
    },
}

impl HttpErrorForRetry for BcrHttpGetError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Fetch { source, .. } => source.is_retryable(),
        }
    }
}

impl IntoBuck2Error for BcrHttpGetError {
    fn into_buck2_error(self) -> buck2_error::Error {
        buck2_error::Error::from(self)
    }
}

fn empty_bcr_source_json() -> BcrSourceJson {
    BcrSourceJson {
        url: String::new(),
        urls: None,
        integrity: String::new(),
        strip_prefix: None,
        archive_type: None,
        patches: None,
        overlay: None,
        patch_strip: None,
    }
}

fn bzlmod_archive_override_source_json(archive_override: &BzlmodArchiveOverride) -> BcrSourceJson {
    BcrSourceJson {
        url: bzlmod_archive_override_primary_url(archive_override).to_owned(),
        urls: Some(archive_override.urls.clone()),
        integrity: archive_override.integrity.clone(),
        strip_prefix: archive_override.strip_prefix.clone(),
        archive_type: archive_override.archive_type.clone(),
        patches: None,
        overlay: None,
        patch_strip: archive_override.patch_strip,
    }
}

fn parse_discovered_bzlmod_module(
    dep: BazelDep,
    source_json: BcrSourceJson,
    module_text: String,
) -> buck2_error::Result<DiscoveredBcrModule> {
    let module_lines = module_text.lines().map(str::to_owned).collect::<Vec<_>>();
    validate_bzlmod_module_lines(&dep.name, &module_lines)?;
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

async fn fetch_local_bzlmod_module(
    dep: BazelDep,
    local_path_override: BzlmodLocalPathOverride,
) -> buck2_error::Result<DiscoveredBcrModule> {
    let module_lines = local_path_override
        .module_text
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    validate_bzlmod_module_lines(&local_path_override.module_name, &module_lines)?;
    let constants = bzlmod_module_constants_from_lines(&module_lines);
    let extension_usages = bzlmod_extension_usages_from_lines(&module_lines, &constants, true);
    let use_repo_rule_invocations =
        bzlmod_use_repo_rule_invocations_from_lines(&module_lines, &constants, true)?;

    Ok(DiscoveredBcrModule {
        dep,
        source_json: empty_bcr_source_json(),
        module_aliases: bzlmod_module_aliases(&module_lines),
        use_repo_aliases: bzlmod_use_repo_aliases_from_usages(&extension_usages),
        extension_usages,
        use_repo_rule_invocations,
        constants,
        registered_toolchains: bzlmod_registered_toolchains_from_lines(&module_lines, true),
        deps: bzlmod_deps_from_lines(&module_lines, true),
    })
}

fn builtin_bazel_tools_module() -> buck2_error::Result<DiscoveredBcrModule> {
    let module_lines = BAZEL_TOOLS_MODULE_TOOLS
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    validate_bzlmod_module_lines("bazel_tools", &module_lines)?;
    let constants = bzlmod_module_constants_from_lines(&module_lines);
    let extension_usages = bzlmod_extension_usages_from_lines(&module_lines, &constants, true);
    let use_repo_rule_invocations =
        bzlmod_use_repo_rule_invocations_from_lines(&module_lines, &constants, true)?;

    Ok(DiscoveredBcrModule {
        dep: BazelDep {
            name: "bazel_tools".to_owned(),
            version: String::new(),
            apparent_name: Some("bazel_tools".to_owned()),
        },
        source_json: BcrSourceJson {
            url: String::new(),
            urls: None,
            integrity: String::new(),
            strip_prefix: None,
            archive_type: None,
            patches: None,
            overlay: None,
            patch_strip: None,
        },
        module_aliases: bzlmod_module_aliases(&module_lines),
        use_repo_aliases: bzlmod_use_repo_aliases_from_usages(&extension_usages),
        extension_usages,
        use_repo_rule_invocations,
        constants,
        registered_toolchains: bzlmod_registered_toolchains_from_lines(&module_lines, true),
        deps: bzlmod_deps_from_lines(&module_lines, true),
    })
}

fn bzlmod_bcr_discovery_cache_key(registry: &str, dep: &BazelDep, kind: &str) -> String {
    let mut hasher = Sha256::new();
    for field in [
        "buck2-bzlmod-bcr-discovery-v1",
        kind,
        registry,
        dep.name.as_str(),
        dep.version.as_str(),
    ] {
        hasher.update(field.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(field.as_bytes());
        hasher.update(b"\0");
    }
    hex::encode(hasher.finalize())
}

fn bzlmod_bcr_discovery_cache_path(
    registry: &str,
    dep: &BazelDep,
    kind: &str,
) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/cache/bzlmod_bcr_discovery/{}/{}",
        bzlmod_bcr_discovery_cache_key(registry, dep, kind),
        kind,
    ))
}

fn read_bzlmod_bcr_discovery_cache(
    project_fs: &ProjectRoot,
    path: &ProjectRelativePathBuf,
) -> buck2_error::Result<Option<String>> {
    let path = project_fs.resolve(path);
    match fs::read_to_string(path.as_path()) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_buck_error_context(|| {
            format!("Error reading bzlmod registry cache `{}`", path.display())
        }),
    }
}

fn write_bzlmod_bcr_discovery_cache(
    project_fs: &ProjectRoot,
    path: &ProjectRelativePathBuf,
    contents: &str,
) -> buck2_error::Result<()> {
    let path = project_fs.resolve(path);
    if let Some(parent) = path.as_path().parent() {
        fs::create_dir_all(parent).with_buck_error_context(|| {
            format!(
                "Error creating parent directory for bzlmod registry cache `{}`",
                path.display()
            )
        })?;
    }
    let temp = path
        .as_path()
        .with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&temp, contents).with_buck_error_context(|| {
        format!(
            "Error writing temporary bzlmod registry cache `{}`",
            temp.display()
        )
    })?;
    fs::rename(&temp, path.as_path()).with_buck_error_context(|| {
        format!(
            "Error committing bzlmod registry cache `{}`",
            path.display()
        )
    })?;
    Ok(())
}

async fn fetch_bcr_module_file(
    project_fs: &ProjectRoot,
    registry: &str,
    client: HttpClient,
    dep: BazelDep,
    archive_override: Option<BzlmodArchiveOverride>,
    single_version_override: Option<BzlmodSingleVersionOverride>,
) -> buck2_error::Result<DiscoveredBcrModule> {
    let (source_json, mut module_text) = if let Some(archive_override) = archive_override.as_ref() {
        let source_json = bzlmod_archive_override_source_json(archive_override);
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
        let module_url = format!(
            "{registry}/modules/{}/{}/MODULE.bazel",
            dep.name, dep.version
        );
        let cache_path = bzlmod_bcr_discovery_cache_path(registry, &dep, "MODULE.bazel");
        let module_text =
            if let Some(module_text) = read_bzlmod_bcr_discovery_cache(project_fs, &cache_path)? {
                module_text
            } else {
                let module_text = http_get_text(&client, &module_url).await?;
                write_bzlmod_bcr_discovery_cache(project_fs, &cache_path, &module_text)?;
                module_text
            };
        (empty_bcr_source_json(), module_text)
    };
    if archive_override.is_none()
        && let Some(single_version_override) = single_version_override.as_ref()
    {
        module_text = apply_bzlmod_single_version_module_patches(
            &dep.name,
            &module_text,
            single_version_override,
        )?;
    }
    parse_discovered_bzlmod_module(dep, source_json, module_text)
}

async fn fetch_bcr_module_source_json(
    project_fs: &ProjectRoot,
    registry: &str,
    client: HttpClient,
    dep: &BazelDep,
    archive_override: Option<&BzlmodArchiveOverride>,
    local_path_override: Option<&BzlmodLocalPathOverride>,
) -> buck2_error::Result<BcrSourceJson> {
    if local_path_override.is_some() {
        return Ok(empty_bcr_source_json());
    }
    if let Some(archive_override) = archive_override {
        return Ok(bzlmod_archive_override_source_json(archive_override));
    }
    let source_url = format!(
        "{registry}/modules/{}/{}/source.json",
        dep.name, dep.version
    );
    let cache_path = bzlmod_bcr_discovery_cache_path(registry, dep, "source.json");
    let source_json =
        if let Some(source_json) = read_bzlmod_bcr_discovery_cache(project_fs, &cache_path)? {
            source_json
        } else {
            let source_json = http_get_text(&client, &source_url).await?;
            write_bzlmod_bcr_discovery_cache(project_fs, &cache_path, &source_json)?;
            source_json
        };
    serde_json::from_str(&source_json)
        .with_buck_error_context(|| format!("Invalid BCR source metadata at `{source_url}`"))
}

fn bzlmod_yanked_versions_from_metadata_json(
    metadata: &str,
) -> buck2_error::Result<BTreeMap<String, String>> {
    #[derive(Deserialize)]
    struct MetadataJson {
        #[serde(default, rename = "yanked_versions")]
        yanked_versions: BTreeMap<String, String>,
    }

    let metadata: MetadataJson = serde_json::from_str(metadata)
        .buck_error_context("Invalid bzlmod registry metadata.json")?;
    Ok(metadata.yanked_versions)
}

async fn http_get_text(client: &HttpClient, url: &str) -> buck2_error::Result<String> {
    let bytes = http_get_bytes(client, url).await?;
    String::from_utf8(bytes)
        .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Input))
        .with_buck_error_context(|| format!("Invalid UTF-8 response from `{url}`"))
}

async fn http_get_bytes(client: &HttpClient, url: &str) -> buck2_error::Result<Vec<u8>> {
    http_retry(
        || async {
            let response = client
                .get(url)
                .await
                .map_err(|error| BcrHttpGetError::Fetch {
                    url: url.to_owned(),
                    source: RetryingHttpError::Client(error),
                })?;
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|error| BcrHttpGetError::Fetch {
                    url: url.to_owned(),
                    source: RetryingHttpError::Transfer {
                        received: bytes.len() as u64,
                        url: url.to_owned(),
                        source: error,
                    },
                })?;
                bytes.extend_from_slice(&chunk);
            }
            Result::<_, BcrHttpGetError>::Ok(bytes)
        },
        vec![2, 4, 8].into_iter().map(Duration::from_secs).collect(),
    )
    .await
    .map_err(buck2_error::Error::from)
}

async fn http_get_bytes_from_urls(
    client: &HttpClient,
    urls: &[String],
) -> buck2_error::Result<(String, Vec<u8>)> {
    let mut last_error = None;
    for url in urls {
        match http_get_bytes(client, url).await {
            Ok(bytes) => return Ok((url.clone(), bytes)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(buck2_error!(
        buck2_error::ErrorTag::Input,
        "failed to download from any archive_override URL {:?}: {}",
        urls,
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no URL provided".to_owned())
    ))
}

async fn fetch_archive_override_module_file(
    client: &HttpClient,
    archive_override: &BzlmodArchiveOverride,
) -> buck2_error::Result<String> {
    let (url, bytes) = http_get_bytes_from_urls(client, &archive_override.urls).await?;
    verify_bzlmod_archive_integrity(&url, &archive_override.integrity, &bytes)?;

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

    let module_root = archive_override
        .strip_prefix
        .as_ref()
        .map(|strip_prefix| extract_dir.join(strip_prefix))
        .unwrap_or_else(|| extract_dir.clone());
    for patch in &archive_override.patches {
        apply_bzlmod_root_patch_to_directory(
            &module_root,
            patch,
            archive_override.patch_strip.unwrap_or(0),
        )
        .with_buck_error_context(|| {
            format!(
                "Error applying archive_override patch `{}` to module `{}` for MODULE.bazel discovery",
                patch.path, archive_override.module_name
            )
        })?;
    }

    let module_file = module_root.join("MODULE.bazel");
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
    let primary_url = bzlmod_archive_override_primary_url(archive_override);
    let kind = bzlmod_archive_override_kind(archive_override).ok_or_else(|| {
        buck2_error!(
            buck2_error::ErrorTag::Input,
            "unsupported archive_override archive type for `{}`",
            primary_url
        )
    })?;
    extract_archive(archive, extract_dir, kind, "", 0, &[]).with_buck_error_context(|| {
        format!("archive_override extraction failed for `{}`", primary_url)
    })
}

fn bzlmod_archive_override_kind(archive_override: &BzlmodArchiveOverride) -> Option<ArchiveKind> {
    archive_kind_from_type_or_url(
        archive_override.archive_type.as_deref(),
        bzlmod_archive_override_primary_url(archive_override),
    )
}

fn bzlmod_archive_override_primary_url(archive_override: &BzlmodArchiveOverride) -> &str {
    archive_override
        .urls
        .first()
        .expect("archive_override URLs should be non-empty")
}

fn verify_bzlmod_archive_integrity(
    url: &str,
    integrity: &str,
    bytes: &[u8],
) -> buck2_error::Result<()> {
    let Some(expected) = parse_bzlmod_integrity(integrity)? else {
        return Ok(());
    };
    let got = expected.kind().digest(bytes);
    if got.as_slice() != expected.bytes() {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "archive_override integrity mismatch for `{}`: expected {}, got {}",
            url,
            hex::encode(expected.bytes()),
            hex::encode(got)
        ));
    }
    Ok(())
}

fn apply_bzlmod_single_version_module_patches(
    module_name: &str,
    module_text: &str,
    single_version_override: &BzlmodSingleVersionOverride,
) -> buck2_error::Result<String> {
    let mut module_text = module_text.to_owned();
    let patch_strip = single_version_override.patch_strip.unwrap_or(0);
    for patch in &single_version_override.patches {
        let Some(filtered_patch) = filter_bzlmod_module_file_patch(&patch.content, patch_strip)
        else {
            continue;
        };
        module_text = apply_bzlmod_module_file_patch(
            module_name,
            &module_text,
            patch,
            &filtered_patch,
            patch_strip,
        )?;
    }
    Ok(module_text)
}

fn filter_bzlmod_module_file_patch(patch_content: &str, patch_strip: u32) -> Option<String> {
    let mut chunks = Vec::<Vec<&str>>::new();
    let mut current = Vec::<&str>::new();
    for line in patch_content.lines() {
        let starts_file_chunk = line.starts_with("diff --git ")
            || (line.starts_with("--- ") && current.iter().any(|line| line.starts_with("+++ ")));
        if starts_file_chunk && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    let mut filtered = String::new();
    for chunk in chunks {
        if !bzlmod_patch_chunk_touches_module_file(&chunk, patch_strip) {
            continue;
        }
        for line in chunk {
            filtered.push_str(line);
            filtered.push('\n');
        }
    }
    (!filtered.is_empty()).then_some(filtered)
}

fn bzlmod_patch_chunk_touches_module_file(chunk: &[&str], patch_strip: u32) -> bool {
    chunk.iter().any(|line| {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            return rest.split_whitespace().any(|path| {
                bzlmod_patch_path_after_strip(path, patch_strip).as_deref() == Some("MODULE.bazel")
            });
        }
        if let Some(rest) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            return bzlmod_patch_path_after_strip(rest, patch_strip).as_deref()
                == Some("MODULE.bazel");
        }
        false
    })
}

fn bzlmod_patch_path_after_strip(path: &str, patch_strip: u32) -> Option<String> {
    crate::bzlmod_patch::patch_path_after_strip(path, patch_strip)
}

fn apply_bzlmod_module_file_patch(
    module_name: &str,
    module_text: &str,
    patch: &BzlmodRootPatch,
    filtered_patch: &str,
    patch_strip: u32,
) -> buck2_error::Result<String> {
    let temp = bzlmod_temp_dir(&format!(
        "module-patch-{}",
        sanitize_bzlmod_temp_name(module_name)
    ))?;
    let module_file = temp.join("MODULE.bazel");
    let patch_file = temp.join("module.patch");
    fs::create_dir_all(&temp)
        .with_buck_error_context(|| format!("Error creating `{}`", temp.display()))?;
    fs::write(&module_file, module_text)
        .with_buck_error_context(|| format!("Error writing `{}`", module_file.display()))?;
    fs::write(&patch_file, filtered_patch)
        .with_buck_error_context(|| format!("Error writing `{}`", patch_file.display()))?;

    let result = run_bzlmod_patch(&temp, &patch_file, patch_strip).with_buck_error_context(|| {
        format!(
            "Error applying single_version_override patch `{}` to MODULE.bazel for module `{}`",
            patch.path, module_name
        )
    });
    let module_text = result.and_then(|()| {
        fs::read_to_string(&module_file)
            .with_buck_error_context(|| format!("Error reading `{}`", module_file.display()))
    });
    let _ = fs::remove_dir_all(&temp);
    module_text
}

fn apply_bzlmod_root_patch_to_directory(
    directory: &Path,
    patch: &BzlmodRootPatch,
    patch_strip: u32,
) -> buck2_error::Result<()> {
    if patch.content.is_empty() {
        return Ok(());
    }
    let temp = bzlmod_temp_dir("root-patch")?;
    fs::create_dir_all(&temp)
        .with_buck_error_context(|| format!("Error creating `{}`", temp.display()))?;
    let patch_file = temp.join("root.patch");
    fs::write(&patch_file, patch.content.as_bytes())
        .with_buck_error_context(|| format!("Error writing `{}`", patch_file.display()))?;
    let result = run_bzlmod_patch(directory, &patch_file, patch_strip);
    let _ = fs::remove_dir_all(&temp);
    result
}

fn run_bzlmod_patch(
    directory: &Path,
    patch_file: &Path,
    patch_strip: u32,
) -> buck2_error::Result<()> {
    crate::bzlmod_patch::apply_unified_patch_file(directory, patch_file, patch_strip)
}

fn bzlmod_temp_dir(prefix: &str) -> buck2_error::Result<std::path::PathBuf> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Tier0))?
        .as_nanos();
    Ok(std::env::temp_dir().join(format!("buck2-bzlmod-{prefix}-{unique}")))
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

fn bzlmod_archive_override_from_call(
    current_module_file: &str,
    call: &str,
    constants: &[(String, String)],
) -> buck2_error::Result<BzlmodArchiveOverride> {
    let module_name =
        bzl_string_expression_arg(call, "module_name", constants).ok_or_else(|| {
            buck2_error!(
                buck2_error::ErrorTag::Input,
                "archive_override must have a literal string `module_name`: {}",
                call
            )
        })?;
    let mut urls = Vec::new();
    if let Some(url) = bzl_string_expression_arg(call, "url", constants) {
        urls.push(url);
    }
    urls.extend(
        bzl_call_named_arg_value(call, "urls")
            .as_deref()
            .and_then(|value| bzl_string_sequence_expression_raw_values(value, constants))
            .unwrap_or_default(),
    );
    if urls.is_empty() {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "archive_override for module `{}` must have a literal `url` or non-empty `urls`",
            module_name
        ));
    }
    let integrity = bzl_string_expression_arg(call, "integrity", constants).unwrap_or_default();
    let strip_prefix = bzl_string_expression_arg(call, "strip_prefix", constants);
    let archive_type = bzl_string_expression_arg(call, "type", constants)
        .or_else(|| bzl_string_expression_arg(call, "archive_type", constants));
    let patches = bzlmod_override_patch_paths_from_call(current_module_file, call, constants)?;
    let patch_strip =
        bzlmod_override_patch_strip_from_call("archive_override", &module_name, call)?;
    Ok(BzlmodArchiveOverride {
        module_name,
        urls,
        integrity,
        strip_prefix,
        archive_type,
        patches,
        patch_strip,
    })
}

fn bzlmod_single_version_override_from_call(
    current_module_file: &str,
    call: &str,
    constants: &[(String, String)],
) -> buck2_error::Result<Option<(String, BzlmodSingleVersionOverride)>> {
    let Some(module_name) = bzl_string_expression_arg(call, "module_name", constants) else {
        return Ok(None);
    };
    reject_unsupported_bzlmod_override_arg(
        "single_version_override",
        &module_name,
        call,
        "registry",
        "Bazel uses `registry` to fetch the module from a non-default registry.",
    )?;
    reject_unsupported_bzlmod_override_arg(
        "single_version_override",
        &module_name,
        call,
        "patch_cmds",
        "Bazel runs `patch_cmds` after applying patch files.",
    )?;
    let version = bzl_string_expression_arg(call, "version", constants);
    let patches = bzlmod_override_patch_paths_from_call(current_module_file, call, constants)?;
    let patch_strip =
        bzlmod_override_patch_strip_from_call("single_version_override", &module_name, call)?;
    Ok(Some((
        module_name,
        BzlmodSingleVersionOverride {
            version,
            patches,
            patch_strip,
        },
    )))
}

fn reject_unsupported_bzlmod_override_arg(
    directive: &str,
    module_name: &str,
    call: &str,
    arg: &str,
    bazel_behavior: &str,
) -> buck2_error::Result<()> {
    let Some(value) = bzl_call_named_arg_value(call, arg) else {
        return Ok(());
    };
    if bzl_is_default_override_value(&value) {
        return Ok(());
    }
    Err(buck2_error!(
        buck2_error::ErrorTag::Input,
        "{} for module `{}` uses unsupported `{}`. {} Buck2 does not implement that behavior yet, so refusing to ignore it: {}",
        directive,
        module_name,
        arg,
        bazel_behavior,
        call
    ))
}

fn bzl_is_default_override_value(value: &str) -> bool {
    matches!(value.trim(), "\"\"" | "''" | "[]")
}

fn bzlmod_local_path_override_from_call(
    cell_path: &CellRootPath,
    call: &str,
) -> buck2_error::Result<BzlmodLocalPathOverride> {
    let module_name = bzl_string_arg(call, "module_name").ok_or_else(|| {
        buck2_error!(
            buck2_error::ErrorTag::Input,
            "local_path_override must have a literal string `module_name`: {}",
            call
        )
    })?;
    let path = bzl_string_arg(call, "path").ok_or_else(|| {
        buck2_error!(
            buck2_error::ErrorTag::Input,
            "local_path_override for module `{}` must have a literal string `path`",
            module_name
        )
    })?;
    let path = cell_path
        .as_project_relative_path()
        .join_normalized(RelativePath::new(&path))
        .with_buck_error_context(|| {
            format!(
                "local_path_override for module `{}` has invalid path `{}`",
                module_name, path
            )
        })?;
    Ok(BzlmodLocalPathOverride {
        module_name,
        path: path.as_str().to_owned(),
        module_text: String::new(),
    })
}

fn bzlmod_override_patch_paths_from_call(
    current_module_file: &str,
    call: &str,
    constants: &[(String, String)],
) -> buck2_error::Result<Vec<BzlmodRootPatch>> {
    let patches = bzl_call_named_arg_value(call, "patches")
        .as_deref()
        .and_then(|value| bzl_string_sequence_expression_raw_values(value, constants))
        .unwrap_or_default();
    patches
        .into_iter()
        .map(|label| {
            let path = module_include_to_path(current_module_file, &label).ok_or_else(|| {
                buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "bzlmod override patch must be a root-module label, got `{}`",
                    label
                )
            })?;
            Ok(BzlmodRootPatch {
                path,
                content: Arc::from(""),
            })
        })
        .collect()
}

fn bzlmod_override_patch_strip_from_call(
    function: &str,
    module_name: &str,
    call: &str,
) -> buck2_error::Result<Option<u32>> {
    bzl_call_named_arg_value(call, "patch_strip")
        .as_deref()
        .and_then(bzl_integer_expression_value)
        .map(|value| {
            u32::try_from(value).map_err(|_| {
                buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "{} for module `{}` has negative patch_strip `{}`",
                    function,
                    module_name,
                    value
                )
            })
        })
        .transpose()
}

fn bzlmod_root_module_from_lines(lines: &[String]) -> buck2_error::Result<RootBzlmodModule> {
    let mut name = "root".to_owned();
    let mut version = String::new();
    let mut repo_name = "root".to_owned();
    for call in collect_bzl_calls(lines, "module(") {
        if let Some(module_name) = bzl_string_arg(&call, "name") {
            if !module_name.is_empty() {
                name = module_name;
            }
        }
        if let Some(module_version) = bzl_string_arg(&call, "version") {
            version = module_version;
        }
        if let Some(module_repo_name) = bzl_repo_name_arg(&call, &name) {
            if !module_repo_name.is_empty() {
                repo_name = module_repo_name;
            }
        }
    }
    let constants = bzlmod_module_constants_from_lines(lines);
    let extension_usages = bzlmod_extension_usages_from_lines(lines, &constants, false);
    let use_repo_rule_invocations =
        bzlmod_use_repo_rule_invocations_from_lines(lines, &constants, false)?;
    let registered_toolchains = bzlmod_registered_toolchains_from_lines(lines, false);
    Ok(RootBzlmodModule {
        name,
        version,
        repo_name,
        canonical_repo_name: String::new(),
        lockfile_extension_generated_repos: BTreeMap::new(),
        lockfile_extension_facts: BTreeSet::new(),
        constants,
        extension_usages,
        use_repo_rule_invocations,
        registered_toolchains,
    })
}

fn validate_bzlmod_module_lines(module_file: &str, lines: &[String]) -> buck2_error::Result<()> {
    let mut depth = 0i32;
    for (idx, line) in lines.iter().enumerate() {
        let without_comment = strip_bzl_comment(line);
        let trimmed = without_comment.trim_start();
        if depth == 0 && !trimmed.is_empty() {
            let invalid = if trimmed.starts_with("load(") || trimmed.starts_with("load ") {
                Some("load statements are not allowed")
            } else if trimmed.starts_with("def ") {
                Some("def statements are not allowed")
            } else if trimmed.starts_with("if ") {
                Some("if statements are not allowed")
            } else if trimmed.starts_with("for ") {
                Some("for statements are not allowed")
            } else if trimmed.starts_with("while ") {
                Some("while statements are not allowed")
            } else if trimmed == "return" || trimmed.starts_with("return ") {
                Some("return statements are not allowed")
            } else {
                None
            };
            if let Some(message) = invalid {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Invalid MODULE.bazel syntax in `{}` at line {}: {}",
                    module_file,
                    idx + 1,
                    message
                ));
            }
        }
        depth += delimiter_delta(&without_comment);
        if depth < 0 {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid MODULE.bazel syntax in `{}` at line {}: unmatched closing delimiter",
                module_file,
                idx + 1
            ));
        }
    }
    if depth != 0 {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "Invalid MODULE.bazel syntax in `{}`: unclosed delimiter",
            module_file
        ));
    }
    Ok(())
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
    let constants = bzlmod_module_constants_from_lines(lines);
    let mut toolchains = collect_bzl_calls(lines, "register_toolchains(")
        .into_iter()
        .filter(|call| !(ignore_dev_dependency && bzl_bool_arg(call, "dev_dependency")))
        .flat_map(|call| {
            bzl_call_args(&call)
                .into_iter()
                .filter_map(|arg| bzl_string_expression_value(arg.trim(), &constants))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for collected_call in
        collect_bzl_list_comprehension_calls(lines, "register_toolchains(", &constants)
    {
        if ignore_dev_dependency && bzl_bool_arg(&collected_call.call, "dev_dependency") {
            continue;
        }
        let constants = bzl_constants_with_bindings(&constants, &collected_call.bindings);
        toolchains.extend(
            bzl_call_args(&collected_call.call)
                .into_iter()
                .filter_map(|arg| bzl_string_expression_value(arg.trim(), &constants)),
        );
    }
    dedup_preserve_order(&mut toolchains);
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
                repo_overrides: bzlmod_extension_repo_overrides(lines, name, constants),
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct BzlmodCollectedTagCall {
    order: usize,
    source_line: usize,
    source_end_line: usize,
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
    let comprehension_call_ranges = comprehension_calls
        .iter()
        .map(|call| (call.source_line, call.source_end_line))
        .collect::<BTreeSet<_>>();
    let normal_calls = collect_bzl_calls_with_order(lines, &call_prefix)
        .into_iter()
        .filter(|(source_line, _)| {
            !comprehension_call_ranges
                .iter()
                .any(|(start, end)| *start <= *source_line && source_line < end)
        })
        .map(|(source_line, call)| BzlmodCollectedTagCall {
            order: source_line * BZL_CALL_ORDER_STRIDE,
            source_line,
            source_end_line: source_line + 1,
            call,
            bindings: Vec::new(),
        });
    let mut calls = normal_calls.chain(comprehension_calls).collect::<Vec<_>>();
    calls.sort_by_key(|call| call.order);
    calls
        .into_iter()
        .filter_map(|collected_call| {
            let call = collected_call.call;
            let rest = call.strip_prefix(&call_prefix)?;
            let (tag_name, _) = rest.split_once('(')?;
            if !is_bzl_identifier(tag_name) {
                return None;
            }
            let kwargs = bzl_call_args(&call)
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
            Some(BzlmodExtensionTag {
                tag_name: tag_name.to_owned(),
                bindings: collected_call.bindings,
                kwargs,
            })
        })
        .collect()
}

fn collect_bzl_list_comprehension_calls(
    lines: &[String],
    function: &str,
    constants: &[(String, String)],
) -> Vec<BzlmodCollectedTagCall> {
    let mut calls = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let block_start_line = index;
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
        let block_end_line = index;

        let Some(binding_groups) = bzl_list_comprehension_binding_groups(&block, constants) else {
            continue;
        };
        let block_text = block.join("\n");
        let block_calls = collect_bzl_calls_anywhere_with_order(&block_text, function);
        let mut expanded_order = 0usize;
        for bindings in &binding_groups {
            for (_, call) in &block_calls {
                calls.push(BzlmodCollectedTagCall {
                    order: block_start_line * BZL_CALL_ORDER_STRIDE + expanded_order,
                    source_line: block_start_line,
                    source_end_line: block_end_line,
                    call: call.clone(),
                    bindings: bindings.clone(),
                });
                expanded_order += 1;
            }
        }
    }
    calls
}

fn collect_bzl_calls_anywhere_with_order(value: &str, function: &str) -> Vec<(usize, String)> {
    let mut calls = Vec::new();
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut index = 0usize;
    while index < value.len() {
        let Some(ch) = value[index..].chars().next() else {
            break;
        };
        if escaped {
            escaped = false;
            index += ch.len_utf8();
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            index += ch.len_utf8();
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            index += ch.len_utf8();
            continue;
        }
        if value[index..].starts_with(function)
            && value[..index]
                .chars()
                .next_back()
                .is_none_or(|ch| !is_bzl_ident(ch))
        {
            if let Some((call, end)) = bzl_call_from_offset(value, index) {
                calls.push((index, call));
                index = end;
                continue;
            }
        }
        index += ch.len_utf8();
    }
    calls
}

fn bzl_call_from_offset(value: &str, offset: usize) -> Option<(String, usize)> {
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut depth = 0i32;
    let mut opened = false;
    let mut index = offset;
    while index < value.len() {
        let ch = value[index..].chars().next()?;
        if escaped {
            escaped = false;
            index += ch.len_utf8();
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            index += ch.len_utf8();
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            index += ch.len_utf8();
            continue;
        }
        match ch {
            '(' => {
                opened = true;
                depth += 1;
            }
            ')' if opened => {
                depth -= 1;
                if depth == 0 {
                    let end = index + ch.len_utf8();
                    return Some((value[offset..end].trim().to_owned(), end));
                }
            }
            _ => {}
        }
        index += ch.len_utf8();
    }
    None
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
        let mut next_groups = Vec::new();
        for group in &groups {
            let constants = bzl_constants_with_bindings(constants, group);
            let options = bzl_list_comprehension_clause_bindings(
                &clause.names,
                &clause.expression,
                &constants,
            )?;
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
    let expression = block.join("\n");
    let mut depth = 0i32;
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut index = 0usize;
    while index < expression.len() {
        let ch = expression[index..].chars().next()?;
        if escaped {
            escaped = false;
            index += ch.len_utf8();
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            index += ch.len_utf8();
            continue;
        };
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            index += ch.len_utf8();
            continue;
        }
        if depth == 1 && bzl_keyword_at(&expression, index, "for") {
            let mut cursor = index + "for".len();
            cursor = bzl_skip_whitespace(&expression, cursor);
            let name_start = cursor;
            let in_index = bzl_find_top_level_keyword(&expression, cursor, "in", 1)?;
            let names = bzl_list_comprehension_binding_names(&expression[name_start..in_index])?;
            cursor = bzl_skip_whitespace(&expression, in_index + "in".len());
            let expression_start = cursor;
            let expression_end = bzl_list_comprehension_clause_expression_end(&expression, cursor)?;
            let expression = expression[expression_start..expression_end]
                .trim()
                .trim_end_matches(',')
                .trim()
                .to_owned();
            clauses.push(BzlmodListComprehensionForClause { names, expression });
            index = expression_end;
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        index += ch.len_utf8();
    }
    (!clauses.is_empty()).then_some(clauses)
}

fn bzl_skip_whitespace(value: &str, mut index: usize) -> usize {
    while index < value.len() {
        let Some(ch) = value[index..].chars().next() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn bzl_keyword_at(value: &str, index: usize, keyword: &str) -> bool {
    value[index..].starts_with(keyword)
        && value[..index]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_bzl_ident(ch))
        && value[index + keyword.len()..]
            .chars()
            .next()
            .is_none_or(|ch| !is_bzl_ident(ch))
}

fn bzl_find_top_level_keyword(
    value: &str,
    mut index: usize,
    keyword: &str,
    target_depth: i32,
) -> Option<usize> {
    let mut depth = target_depth;
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    while index < value.len() {
        let ch = value[index..].chars().next()?;
        if escaped {
            escaped = false;
            index += ch.len_utf8();
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            index += ch.len_utf8();
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            index += ch.len_utf8();
            continue;
        }
        if depth == target_depth && bzl_keyword_at(value, index, keyword) {
            return Some(index);
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        index += ch.len_utf8();
    }
    None
}

fn bzl_list_comprehension_clause_expression_end(value: &str, mut index: usize) -> Option<usize> {
    let mut depth = 1i32;
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    while index < value.len() {
        let ch = value[index..].chars().next()?;
        if escaped {
            escaped = false;
            index += ch.len_utf8();
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            index += ch.len_utf8();
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            index += ch.len_utf8();
            continue;
        }
        if depth == 1 && bzl_keyword_at(value, index, "for") {
            return Some(index);
        }
        if depth == 1 && ch == ']' {
            return Some(index);
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        index += ch.len_utf8();
    }
    None
}

fn bzl_constants_with_bindings(
    constants: &[(String, String)],
    bindings: &[(String, String)],
) -> Vec<(String, String)> {
    let mut merged = bindings.to_vec();
    merged.extend(constants.iter().cloned());
    merged
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
        let values = bzl_iterable_string_expression_values(expression, constants)?;
        return Some(
            values
                .into_iter()
                .map(|value| vec![(names[0].clone(), value)])
                .collect(),
        );
    }

    let values = bzl_split_string_list_comprehension_values(expression, constants)
        .or_else(|| bzl_dict_items_expression_values(expression, constants))?;
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

fn bzl_split_string_list_comprehension_values(
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<Vec<String>>> {
    let inner = expression
        .trim()
        .strip_prefix('[')
        .and_then(|expression| expression.strip_suffix(']'))?
        .trim();
    let (map_expression, rest) = inner.split_once(" for ")?;
    let (binding_name, values_expression) = rest.split_once(" in ")?;
    let binding_name = binding_name.trim();
    if !is_bzl_identifier(binding_name) {
        return None;
    }
    let (receiver, args) = bzl_top_level_method_call(map_expression.trim(), "split")?;
    if receiver.trim() != binding_name {
        return None;
    }
    let delimiter = bzl_string_expression_value(args.trim(), constants)?;
    if delimiter.is_empty() {
        return None;
    }
    let values = bzl_string_sequence_expression_values(values_expression.trim(), constants)?;
    values
        .into_iter()
        .map(|value| {
            let value = serde_json::from_str::<String>(&value).ok()?;
            value
                .split(&delimiter)
                .map(|part| serde_json::to_string(part).ok())
                .collect()
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

fn bzl_string_dict_expression_items(
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<(String, String)>> {
    bzl_string_dict_expression_raw_items(expression, constants)?
        .into_iter()
        .map(|(key, value)| Some((key, bzl_string_expression_value(value.trim(), constants)?)))
        .collect()
}

fn bzl_string_dict_expression_raw_items(
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<(String, String)>> {
    let expression = expression.trim();
    let dict = if is_bzl_identifier(expression) {
        constants
            .iter()
            .find_map(|(name, value)| (name == expression).then_some(value.as_str()))?
    } else {
        expression
    };
    bzl_string_dict_literal_items(dict)
}

fn bzl_supported_module_literal_expression(value: &str) -> Option<String> {
    let value = value.trim();
    if bzl_string_literal_value(value).is_some()
        || bzl_string_sequence_literal_raw_values(value).is_some()
        || bzl_string_dict_literal_items(value).is_some()
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
    bzl_string_sequence_expression_raw_values(expression, constants)?
        .into_iter()
        .map(|value| serde_json::to_string(&value).ok())
        .collect()
}

fn bzl_iterable_string_expression_values(
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<String>> {
    if let Some(values) = bzl_string_sequence_expression_values(expression, constants) {
        return Some(values);
    }
    bzl_string_dict_expression_raw_items(expression, constants)?
        .into_iter()
        .map(|(key, _)| serde_json::to_string(&key).ok())
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
    let expression = bzl_strip_balanced_outer_parens(expression);
    if let Some(values) = bzl_string_sequence_literal_raw_values(expression) {
        return Some(values);
    }
    if let Some(values) = bzl_string_sequence_list_expression_raw_values(expression, constants) {
        return Some(values);
    }
    if let Some(values) = bzl_string_list_comprehension_raw_values(expression, constants) {
        return Some(values);
    }
    if let Some((receiver, index)) = bzl_top_level_index(expression) {
        let key = bzl_string_expression_value(index.trim(), constants)?;
        let (_, value) = bzl_string_dict_expression_raw_items(receiver, constants)?
            .into_iter()
            .find(|(item_key, _)| item_key == &key)?;
        return bzl_string_sequence_expression_raw_values(&value, constants);
    }
    if is_bzl_identifier(expression) {
        let (_, value) = constants.iter().find(|(name, _)| name == expression)?;
        return bzl_string_sequence_expression_raw_values(value, constants);
    }
    None
}

fn bzl_string_sequence_list_expression_raw_values(
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<String>> {
    let expression = expression.trim();
    let inner = if let Some(inner) = expression
        .strip_prefix('[')
        .and_then(|expression| expression.strip_suffix(']'))
    {
        inner
    } else if let Some(inner) = expression
        .strip_prefix('(')
        .and_then(|expression| expression.strip_suffix(')'))
    {
        inner
    } else {
        return None;
    };
    bzl_split_top_level(inner, ',')
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .map(|item| bzl_string_expression_value(item.trim(), constants))
        .collect()
}

fn bzl_string_list_comprehension_raw_values(
    expression: &str,
    constants: &[(String, String)],
) -> Option<Vec<String>> {
    let expression = expression.trim();
    if !expression.starts_with('[') || !expression.ends_with(']') {
        return None;
    }
    let first_for = bzl_list_comprehension_first_for(expression)?;
    let map_expression = expression[1..first_for].trim().trim_end_matches(',').trim();
    if map_expression.is_empty() {
        return None;
    }
    let block = expression.lines().map(str::to_owned).collect::<Vec<_>>();
    bzl_list_comprehension_binding_groups(&block, constants)?
        .into_iter()
        .map(|bindings| {
            let constants = bzl_constants_with_bindings(constants, &bindings);
            bzl_string_expression_value(map_expression, &constants)
        })
        .collect()
}

fn bzl_list_comprehension_first_for(value: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut index = 0usize;
    while index < value.len() {
        let ch = value[index..].chars().next()?;
        if escaped {
            escaped = false;
            index += ch.len_utf8();
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            index += ch.len_utf8();
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            index += ch.len_utf8();
            continue;
        }
        if depth == 1 && bzl_keyword_at(value, index, "for") {
            return Some(index);
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        index += ch.len_utf8();
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
    let comprehension_calls = collect_bzl_list_comprehension_calls(lines, "use_repo(", constants);
    let comprehension_call_strings = comprehension_calls
        .iter()
        .map(|call| call.call.clone())
        .collect::<BTreeSet<_>>();
    let mut imports = collect_bzl_calls(lines, "use_repo(")
        .into_iter()
        .filter(|call| !comprehension_call_strings.contains(call))
        .filter(|call| {
            bzl_call_args(call)
                .first()
                .is_some_and(|arg| arg.trim() == proxy_name)
        })
        .flat_map(|call| bzl_use_repo_imports(&call, constants))
        .collect::<Vec<_>>();
    for collected_call in comprehension_calls {
        if !bzl_call_args(&collected_call.call)
            .first()
            .is_some_and(|arg| arg.trim() == proxy_name)
        {
            continue;
        }
        let constants = bzl_constants_with_bindings(constants, &collected_call.bindings);
        imports.extend(bzl_use_repo_imports(&collected_call.call, &constants));
    }
    imports
}

fn bzlmod_extension_repo_overrides(
    lines: &[String],
    proxy_name: &str,
    constants: &[(String, String)],
) -> Vec<BzlmodRepoOverride> {
    let mut repo_overrides = Vec::new();
    for (function, must_exist) in [("override_repo(", true), ("inject_repo(", false)] {
        repo_overrides.extend(
            collect_bzl_calls(lines, function)
                .into_iter()
                .filter(|call| {
                    bzl_call_args(call)
                        .first()
                        .is_some_and(|arg| arg.trim() == proxy_name)
                })
                .flat_map(|call| bzl_repo_overrides_from_call(&call, constants, must_exist)),
        );
    }
    repo_overrides.sort_by(|left, right| {
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
    repo_overrides.dedup_by(|left, right| {
        left.repo_name == right.repo_name
            && left.overriding_repo_name == right.overriding_repo_name
            && left.must_exist == right.must_exist
    });
    repo_overrides
}

fn bzl_repo_overrides_from_call(
    call: &str,
    constants: &[(String, String)],
    must_exist: bool,
) -> Vec<BzlmodRepoOverride> {
    let args = bzl_call_args(call);
    let mut repo_overrides = Vec::new();
    for arg in args.into_iter().skip(1) {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        if let Some(dict) = arg.strip_prefix("**") {
            if let Some(items) = bzl_string_dict_expression_items(dict.trim(), constants) {
                repo_overrides.extend(items.into_iter().map(
                    |(repo_name, overriding_repo_name)| BzlmodRepoOverride {
                        repo_name,
                        overriding_repo_name,
                        must_exist,
                    },
                ));
            }
        } else if let Some((repo_name, overriding_repo_name)) = bzl_top_level_assignment(arg) {
            let repo_name = repo_name.trim();
            if is_bzl_identifier(repo_name) {
                if let Some(overriding_repo_name) =
                    bzl_string_expression_value(overriding_repo_name.trim(), constants)
                {
                    repo_overrides.push(BzlmodRepoOverride {
                        repo_name: repo_name.to_owned(),
                        overriding_repo_name,
                        must_exist,
                    });
                }
            }
        } else if let Some(repo_name) = bzl_string_expression_value(arg, constants) {
            repo_overrides.push(BzlmodRepoOverride {
                overriding_repo_name: repo_name.clone(),
                repo_name,
                must_exist,
            });
        }
    }
    repo_overrides
}

async fn read_bzlmod_root_patch_contents(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
    patches: &mut [BzlmodRootPatch],
) -> buck2_error::Result<()> {
    for patch in patches {
        let path = cell_path
            .as_project_relative_path()
            .join(ForwardRelativePath::new(&patch.path)?);
        let config_path = ConfigPath::Project(path);
        let Some(lines) = file_ops.read_file_lines_if_exists(&config_path).await? else {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod override patch `{}` does not exist",
                patch.path
            ));
        };
        patch.content = if lines.is_empty() {
            Arc::from("")
        } else {
            Arc::from(format!("{}\n", lines.join("\n")))
        };
    }
    Ok(())
}

fn bzlmod_patch_configs(
    registry: &str,
    dep: &BazelDep,
    source_json: &BcrSourceJson,
    archive_override: Option<&BzlmodArchiveOverride>,
    single_version_override: Option<&BzlmodSingleVersionOverride>,
) -> Vec<BzlmodPatchConfig> {
    let mut patches = source_json
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
            path: None,
            content_sha256: None,
            patch_strip: source_json.patch_strip,
        })
        .collect::<Vec<_>>();
    if let Some(archive_override) = archive_override {
        patches.extend(
            archive_override
                .patches
                .iter()
                .map(|path| BzlmodPatchConfig {
                    url: String::new(),
                    integrity: String::new(),
                    path: Some(path.path.clone()),
                    content_sha256: Some(hex::encode(Sha256::digest(path.content.as_bytes()))),
                    patch_strip: archive_override.patch_strip,
                }),
        );
    }
    if let Some(single_version_override) = single_version_override {
        patches.extend(
            single_version_override
                .patches
                .iter()
                .map(|path| BzlmodPatchConfig {
                    url: String::new(),
                    integrity: String::new(),
                    path: Some(path.path.clone()),
                    content_sha256: Some(hex::encode(Sha256::digest(path.content.as_bytes()))),
                    patch_strip: single_version_override.patch_strip,
                }),
        );
    }
    patches
}

fn bzlmod_overlay_configs(
    registry: &str,
    dep: &BazelDep,
    source_json: &BcrSourceJson,
) -> Vec<BzlmodOverlayConfig> {
    source_json
        .overlay
        .as_ref()
        .into_iter()
        .flat_map(|overlays| overlays.iter())
        .map(|(path, integrity)| BzlmodOverlayConfig {
            path: path.clone(),
            url: format!(
                "{registry}/modules/{}/{}/overlay/{}",
                dep.name, dep.version, path
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

fn bzlmod_cell_name_for_canonical_repo_name(canonical_repo_name: &str) -> String {
    bzlmod_cell_name(canonical_repo_name)
}

fn is_valid_bzlmod_module_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut last = first;
    for ch in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '-' || ch == '_')
        {
            return false;
        }
        last = ch;
    }
    last.is_ascii_lowercase() || last.is_ascii_digit()
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

const BZL_CALL_ORDER_STRIDE: usize = 1_000_000;

fn collect_bzl_calls(lines: &[String], function: &str) -> Vec<String> {
    collect_bzl_calls_with_order(lines, function)
        .into_iter()
        .map(|(_, call)| call)
        .collect()
}

fn collect_bzl_calls_with_order(lines: &[String], function: &str) -> Vec<(usize, String)> {
    let mut calls = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut start_line = 0usize;

    for (line_index, line) in lines.iter().enumerate() {
        if current.is_empty() {
            let rest = line.trim_start();
            if !rest.starts_with(function) {
                continue;
            };
            start_line = line_index;
            let line = strip_bzl_comment(line);
            let rest = line.trim_start();
            depth = paren_delta(rest);
            current.push_str(rest);
        } else {
            let line = strip_bzl_comment(line);
            current.push('\n');
            current.push_str(&line);
            depth += paren_delta(&line);
        }

        if depth <= 0 {
            calls.push((start_line, std::mem::take(&mut current)));
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

fn bzl_string_expression_arg(
    call: &str,
    arg: &str,
    constants: &[(String, String)],
) -> Option<String> {
    let value = bzl_call_named_arg_value(call, arg)?;
    bzl_string_expression_value(&value, constants)
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
    let value = bzl_strip_balanced_outer_parens(value);
    if let Some(literal) = bzl_string_literal_value(value) {
        return Some(literal);
    }
    if let Some((if_index, else_index)) = bzl_top_level_if_else(value) {
        let condition = value[if_index + "if".len()..else_index].trim();
        let branch = if bzl_bool_expression_value(condition, constants)? {
            value[..if_index].trim()
        } else {
            value[else_index + "else".len()..].trim()
        };
        return bzl_string_expression_value(branch, constants);
    }
    let parts = bzl_split_top_level(value, '+');
    if parts.len() > 1 {
        let mut result = String::new();
        for part in parts {
            result.push_str(&bzl_string_expression_value(part.trim(), constants)?);
        }
        return Some(result);
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

fn bzl_strip_balanced_outer_parens(value: &str) -> &str {
    let mut value = value.trim();
    loop {
        let Some(inner) = value
            .strip_prefix('(')
            .and_then(|value| value.strip_suffix(')'))
        else {
            return value;
        };
        let mut depth = 0i32;
        let mut in_string = false;
        let mut quote = '\0';
        let mut escaped = false;
        let mut closes_at_end = false;
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
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        closes_at_end = idx + ch.len_utf8() == value.len();
                        break;
                    }
                }
                _ => {}
            }
        }
        if !closes_at_end {
            return value;
        }
        value = inner.trim();
    }
}

fn bzl_top_level_if_else(value: &str) -> Option<(usize, usize)> {
    let if_index = bzl_find_top_level_keyword(value, 0, "if", 0)?;
    let else_index = bzl_find_top_level_keyword(value, if_index + "if".len(), "else", 0)?;
    Some((if_index, else_index))
}

fn bzl_bool_expression_value(value: &str, constants: &[(String, String)]) -> Option<bool> {
    let value = bzl_strip_balanced_outer_parens(value);
    if value == "True" {
        return Some(true);
    }
    if value == "False" {
        return Some(false);
    }
    for (operator, equal_value) in [("==", true), ("!=", false)] {
        if let Some(index) = bzl_find_top_level_operator(value, operator) {
            let left = bzl_string_expression_value(value[..index].trim(), constants)?;
            let right =
                bzl_string_expression_value(value[index + operator.len()..].trim(), constants)?;
            return Some((left == right) == equal_value);
        }
    }
    None
}

fn bzl_find_top_level_operator(value: &str, operator: &str) -> Option<usize> {
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut depth = 0i32;
    let mut index = 0usize;
    while index < value.len() {
        let ch = value[index..].chars().next()?;
        if escaped {
            escaped = false;
            index += ch.len_utf8();
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            index += ch.len_utf8();
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            index += ch.len_utf8();
            continue;
        }
        if depth == 0 && value[index..].starts_with(operator) {
            return Some(index);
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        index += ch.len_utf8();
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
    use buck2_error::BuckErrorContext;
    use dice::DiceComputations;
    use indoc::indoc;

    use crate::external_cells::EXTERNAL_CELLS_IMPL;
    use crate::external_cells::ExternalCellsImpl;
    use crate::file_ops::delegate::FileOpsDelegate;
    use crate::legacy_configs::cells::BuckConfigBasedCells;
    use crate::legacy_configs::configs::testing::TestConfigParserFileOps;
    use crate::legacy_configs::configs::tests::assert_config_value;
    use crate::legacy_configs::key::BuckconfigKeyRef;

    fn insert_test_extension_usages_json(
        extension_usages_json_by_id: &mut std::collections::BTreeMap<
            super::BzlmodExtensionId,
            String,
        >,
        extension_id: super::BzlmodExtensionId,
    ) -> buck2_error::Result<()> {
        extension_usages_json_by_id.insert(
            extension_id,
            serde_json::to_string(&super::BzlmodModuleExtensionEvaluationConfig {
                root_module_has_non_dev_dependency: false,
                modules: Vec::new(),
                usages: Vec::new(),
                repo_overrides: Vec::new(),
            })
            .buck_error_context("Error serializing test extension usages")?,
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_module_validation_rejects_load() {
        let lines = vec![
            "module(name = \"demo\")".to_owned(),
            "load(\"//:defs.bzl\", \"dep\")".to_owned(),
        ];
        assert!(super::validate_bzlmod_module_lines("MODULE.bazel", &lines).is_err());
    }

    #[test]
    fn test_bzlmod_configure_repo_detection_uses_structured_generator_kind()
    -> buck2_error::Result<()> {
        fn generated(
            canonical_repo_name: &str,
            generator_json: String,
        ) -> super::BazelCompatExternalModule {
            super::BazelCompatExternalModule::Generated(super::BazelCompatGeneratedModule {
                cell_name: super::bzlmod_cell_name(canonical_repo_name),
                aliases: Vec::new(),
                canonical_repo_name: canonical_repo_name.to_owned(),
                generator_json,
            })
        }

        let host_platform_json = serde_json::to_string(
            &super::BzlmodGeneratedCellGenerator::HostPlatform(super::BzlmodHostPlatformSetup {}),
        )?;
        assert!(super::bzlmod_external_module_is_configure_repo(&generated(
            "platforms+host_platform+host_platform",
            host_platform_json
        )));

        let non_configure_json =
            serde_json::to_string(&super::BzlmodGeneratedCellGenerator::BazelFeaturesVersion(
                super::BzlmodBazelFeaturesVersionSetup {
                    bazel_version: Arc::from("9.1.0"),
                },
            ))?;
        assert!(!super::bzlmod_external_module_is_configure_repo(
            &generated("rules_example+toolchain_config_repo", non_configure_json)
        ));
        assert!(!super::bzlmod_external_module_is_configure_repo(
            &generated("rules_example+configure_repo", "{".to_owned())
        ));
        Ok(())
    }

    #[tokio::test]
    async fn test_bazelrc_workspace_import_normalizes_path() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".bazelrc",
                "try-import %workspace%/configs/../imported.bazelrc\n",
            ),
            ("imported.bazelrc", "build --copt=-DFROM_IMPORTED\n"),
        ])?;

        let options =
            super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops).await?;

        assert_eq!(options.copt, vec!["-DFROM_IMPORTED"]);
        Ok(())
    }

    #[tokio::test]
    async fn test_bazelrc_bazel_native_configuration_flags() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".bazelrc",
            "build --cpu=k8 --host_cpu=k8 --platforms=//platforms:linux,@platforms//cpu:x86_64 --javacopt=-Akey=a,b\n",
        )])?;

        let options =
            super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops).await?;

        assert_eq!(
            options.command_line_build_settings,
            vec![
                "string\t//command_line_option:cpu\tk8",
                "string\t//command_line_option:host_cpu\tk8",
                "list\t//command_line_option:platforms\t//platforms:linux",
                "list\t//command_line_option:platforms\t@platforms//cpu:x86_64",
                "list\t//command_line_option:javacopt\t-Akey=a,b",
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_bazelrc_try_import_workspace_path_outside_project_is_optional()
    -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".bazelrc",
            "try-import %workspace%/../../internal_tools/preset.bazelrc\nbuild --copt=-DLOCAL\n",
        )])?;

        let options =
            super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops).await?;

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

        let result = super::get_bazelrc_options(CellRootPath::testing_new(""), &mut file_ops).await;

        assert!(result.is_err());
    }

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
                true,
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
            Some("target:gh_facebook_buck2//...->platforms//host:host"),
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_module_bazel_enables_bazel_compat_defaults_with_buckconfig()
    -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[
            (
                ".buckconfig",
                indoc!(
                    r#"
                    [cells]
                        gh_facebook_buck2 = .
                        prelude = prelude

                    [cell_aliases]
                        root = gh_facebook_buck2
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
            .parse_single_cell_with_file_ops(
                CellName::testing_new("gh_facebook_buck2"),
                &mut file_ops,
            )
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
            Some("gh_facebook_buck2"),
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
    async fn test_bazel_cell_alias_resolver_preserves_magic_bazel_tools() -> buck2_error::Result<()>
    {
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
    async fn test_bazel_cell_alias_resolver_uses_actual_root_cell() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            ".buckconfig",
            indoc!(
                r#"
                    [cells]
                        gh_facebook_buck2 = .
                        prelude = prelude

                    [cell_aliases]
                        root = gh_facebook_buck2
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

        assert_eq!("gh_facebook_buck2", resolver.resolve("root")?.as_str());
        assert!(resolver.resolve("bazel_tools").is_err());

        Ok(())
    }

    #[test]
    fn test_bzlmod_single_version_override_allows_patches_without_version() {
        let (_, override_config) = super::bzlmod_single_version_override_from_call(
            "MODULE.bazel",
            indoc!(
                r#"
                single_version_override(
                    module_name = "grpc-java",
                    patch_strip = 1,
                    patches = [
                        "//third_party:grpc-java.patch",
                        "//third_party:grpc-java-addloads.patch",
                    ],
                )
                "#
            ),
            &[],
        )
        .unwrap()
        .unwrap();

        assert_eq!(override_config.version, None);
        assert_eq!(override_config.patch_strip, Some(1));
        assert_eq!(
            override_config
                .patches
                .iter()
                .map(|patch| patch.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "third_party/grpc-java.patch",
                "third_party/grpc-java-addloads.patch",
            ]
        );
    }

    #[test]
    fn test_bzlmod_archive_override_preserves_url_mirror_order() {
        let archive_override = super::bzlmod_archive_override_from_call(
            "MODULE.bazel",
            indoc!(
                r#"
                archive_override(
                    module_name = "example",
                    url = "https://primary.example.com/source.tar.gz",
                    urls = [
                        "https://mirror1.example.com/source.tar.gz",
                        "https://mirror2.example.com/source.tar.gz",
                    ],
                )
                "#
            ),
            &[],
        )
        .unwrap();

        assert_eq!(
            archive_override.urls,
            vec![
                "https://primary.example.com/source.tar.gz",
                "https://mirror1.example.com/source.tar.gz",
                "https://mirror2.example.com/source.tar.gz",
            ]
        );
    }

    #[test]
    fn test_bzlmod_archive_override_infers_kind_from_url_without_type() {
        let archive_override = super::bzlmod_archive_override_from_call(
            "MODULE.bazel",
            indoc!(
                r#"
                archive_override(
                    module_name = "rules_webtesting",
                    url = "https://github.com/bazelbuild/rules_webtesting/archive/e09c04b7d4d1e91ac1cd6f08283246d350c65379.tar.gz",
                )
                "#
            ),
            &[],
        )
        .unwrap();

        assert_eq!(
            super::bzlmod_archive_override_kind(&archive_override),
            Some(crate::bzlmod_archive::ArchiveKind::TarGz)
        );
    }

    #[test]
    fn test_bzlmod_archive_integrity_accepts_sri_algorithms() {
        for integrity in [
            "sha1-iEPX+SQWIR3p67lj/0zigSWTKHg=",
            "sha256-w6uP8Tcg6K2QR905Rms8iXTlksL6OD1KOWBxTK7wxPI=",
            "sha384-PJww2fZl501RXIQpYNSkUcg6ASX9Pec5LXs3IxrxDHLqWK7fzfiaV2W/kCr5Ps8G",
            "sha512-ClAmHr0aOQ/tK/Mm8mc8FFWCpjQtUjIElz0CGTN/gWFqgGmwElh89WNfaSXxtWw2AjDBmyc1AO4BPgMGAb8kJQ==",
        ] {
            super::verify_bzlmod_archive_integrity(
                "https://example.com/archive.tar.gz",
                integrity,
                b"foobar",
            )
            .unwrap();
        }
    }

    #[test]
    fn test_bzlmod_archive_override_resolves_module_constants() {
        let lines = indoc!(
            r#"
            COMMIT = "abc123"

            archive_override(
                module_name = "example",
                strip_prefix = "example-{}".format(COMMIT),
                urls = ["https://example.com/{}.tar.gz".format(COMMIT)],
            )
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let constants = super::bzlmod_module_constants_from_lines(&lines);
        let call = super::collect_bzl_calls(&lines, "archive_override(")
            .into_iter()
            .next()
            .unwrap();

        let archive_override =
            super::bzlmod_archive_override_from_call("MODULE.bazel", &call, &constants).unwrap();

        assert_eq!(
            archive_override.strip_prefix.as_deref(),
            Some("example-abc123")
        );
        assert_eq!(
            archive_override.urls,
            vec!["https://example.com/abc123.tar.gz"]
        );
    }

    #[test]
    fn test_bzlmod_single_version_override_rejects_patch_cmds() {
        let error = super::bzlmod_single_version_override_from_call(
            "MODULE.bazel",
            indoc!(
                r#"
                single_version_override(
                    module_name = "example",
                    patch_cmds = ["echo patched"],
                )
                "#
            ),
            &[],
        )
        .unwrap_err();

        let error = format!("{error:?}");
        assert!(error.contains("unsupported `patch_cmds`"), "error: {error}");
    }

    #[test]
    fn test_bzlmod_single_version_override_rejects_registry() {
        let error = super::bzlmod_single_version_override_from_call(
            "MODULE.bazel",
            indoc!(
                r#"
                single_version_override(
                    module_name = "example",
                    registry = "https://registry.example.com",
                )
                "#
            ),
            &[],
        )
        .unwrap_err();

        let error = format!("{error:?}");
        assert!(error.contains("unsupported `registry`"), "error: {error}");
    }

    #[tokio::test]
    async fn test_bzlmod_multiple_version_override_rejected() -> buck2_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            "MODULE.bazel",
            indoc!(
                r#"
                module(name = "root")
                multiple_version_override(
                    module_name = "example",
                    versions = ["1.0.0", "2.0.0"],
                )
                "#
            ),
        )])?;

        let error = BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[])
            .await
            .unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("multiple_version_override is not implemented"),
            "error: {error}"
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
            INJECTED_REPOS = {
                "non_identifier.repo": "actual_repo",
            }
            inject_repo(
                sdk,
                "com_github_buildbuddy_io_buildbuddy",
                googleapis_alias = "googleapis",
                **INJECTED_REPOS,
            )
            override_repo(sdk, go_toolchains = "actual_repo")

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
                ),
                (
                    "INJECTED_REPOS".to_owned(),
                    "{\n    \"non_identifier.repo\": \"actual_repo\",\n}".to_owned()
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
        assert_eq!(usages[0].repo_overrides.len(), 4);
        assert_eq!(
            usages[0].repo_overrides[0].repo_name,
            "com_github_buildbuddy_io_buildbuddy"
        );
        assert_eq!(
            usages[0].repo_overrides[0].overriding_repo_name,
            "com_github_buildbuddy_io_buildbuddy"
        );
        assert!(!usages[0].repo_overrides[0].must_exist);
        assert_eq!(usages[0].repo_overrides[1].repo_name, "go_toolchains");
        assert_eq!(
            usages[0].repo_overrides[1].overriding_repo_name,
            "actual_repo"
        );
        assert!(usages[0].repo_overrides[1].must_exist);
        assert_eq!(usages[0].repo_overrides[2].repo_name, "googleapis_alias");
        assert_eq!(
            usages[0].repo_overrides[2].overriding_repo_name,
            "googleapis"
        );
        assert!(!usages[0].repo_overrides[2].must_exist);
        assert_eq!(usages[0].repo_overrides[3].repo_name, "non_identifier.repo");
        assert_eq!(
            usages[0].repo_overrides[3].overriding_repo_name,
            "actual_repo"
        );
        assert!(!usages[0].repo_overrides[3].must_exist);
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
    fn test_bzlmod_extension_tags_preserve_bazel_evaluation_order() {
        let lines = indoc!(
            r#"
            ext = use_extension("//:extensions.bzl", "ext")

            ext.zeta(second = "2", first = "1")

            [
                ext.alpha(version = version)
                for version in ["3.11", "3.12"]
            ]

            ext.beta(name = "last")
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let constants = super::bzlmod_module_constants_from_lines(&lines);
        let usages = super::bzlmod_extension_usages_from_lines(&lines, &constants, false);
        assert_eq!(usages.len(), 1);
        assert_eq!(
            usages[0]
                .tags
                .iter()
                .map(|tag| tag.tag_name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha", "alpha", "beta"]
        );
        assert_eq!(
            usages[0].tags[0].kwargs,
            vec![
                ("second".to_owned(), "\"2\"".to_owned()),
                ("first".to_owned(), "\"1\"".to_owned()),
            ]
        );
        assert_eq!(
            usages[0].tags[1].bindings,
            vec![("version".to_owned(), "\"3.11\"".to_owned())]
        );
        assert_eq!(
            usages[0].tags[2].bindings,
            vec![("version".to_owned(), "\"3.12\"".to_owned())]
        );
    }

    #[test]
    fn test_bzlmod_extension_tags_expand_split_tuple_list_comprehensions() {
        let lines = indoc!(
            r#"
            maven = use_extension("@rules_jvm_external//:extensions.bzl", "maven")

            [
                maven.artifact(
                    artifact = artifact,
                    group = group,
                    version = version,
                )
                for group, artifact, version in [coord.split(":") for coord in [
                    "com.google.guava:guava-testlib:33.2.1-jre",
                    "com.google.truth:truth:1.4.2",
                ]]
            ]
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let constants = super::bzlmod_module_constants_from_lines(&lines);
        let usages = super::bzlmod_extension_usages_from_lines(&lines, &constants, false);
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].tags.len(), 2);
        assert!(usages[0].tags.iter().any(|tag| {
            tag.kwargs
                .contains(&("group".to_owned(), "\"com.google.guava\"".to_owned()))
                && tag
                    .kwargs
                    .contains(&("artifact".to_owned(), "\"guava-testlib\"".to_owned()))
                && tag
                    .kwargs
                    .contains(&("version".to_owned(), "\"33.2.1-jre\"".to_owned()))
        }));
    }

    #[test]
    fn test_bzlmod_extension_list_comprehensions_do_not_match_suffix_proxy_names() {
        let lines = indoc!(
            r#"
            pip = use_extension("//python/extensions:pip.bzl", "pip")

            [pip.parse(hub_name = "prod_pip", python_version = version) for version in ["3.11"]]

            dev_pip = use_extension("//python/extensions:pip.bzl", "pip", dev_dependency = True)

            [dev_pip.parse(hub_name = "dev_pip", python_version = version) for version in ["3.14"]]
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let constants = super::bzlmod_module_constants_from_lines(&lines);
        let usages = super::bzlmod_extension_usages_from_lines(&lines, &constants, true);
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].proxy_name, "pip");
        assert_eq!(usages[0].tags.len(), 1);
        assert_eq!(
            usages[0].tags[0].kwargs,
            vec![
                ("hub_name".to_owned(), "\"prod_pip\"".to_owned()),
                ("python_version".to_owned(), "version".to_owned()),
            ]
        );
    }

    #[test]
    fn test_bzlmod_rules_java_repo_list_comprehensions() {
        let lines = indoc!(
            r#"
            JDKS = {
                "8": ["linux"],
                "25": ["macos_aarch64"],
            }
            REMOTE_JDK_REPOS = [
                (("remote_jdk" if version == "8" else "remotejdk") + version + "_" + platform)
                for version in JDKS
                for platform in JDKS[version]
            ]

            toolchains = use_extension("//toolchains:extensions.bzl", "toolchains")

            [use_repo(toolchains, repo + "_toolchain_config_repo") for repo in REMOTE_JDK_REPOS]
            [register_toolchains("@" + name + "_toolchain_config_repo//:all") for name in REMOTE_JDK_REPOS]
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let constants = super::bzlmod_module_constants_from_lines(&lines);
        let usages = super::bzlmod_extension_usages_from_lines(&lines, &constants, false);
        assert_eq!(usages.len(), 1);
        let mut imports = usages[0]
            .imports
            .iter()
            .map(|import| (import.alias.clone(), import.repo_name.clone()))
            .collect::<Vec<_>>();
        imports.sort();
        assert_eq!(
            imports,
            vec![
                (
                    "remote_jdk8_linux_toolchain_config_repo".to_owned(),
                    "remote_jdk8_linux_toolchain_config_repo".to_owned()
                ),
                (
                    "remotejdk25_macos_aarch64_toolchain_config_repo".to_owned(),
                    "remotejdk25_macos_aarch64_toolchain_config_repo".to_owned()
                ),
            ]
        );

        assert_eq!(
            super::bzlmod_registered_toolchains_from_lines(&lines, false),
            vec![
                "@remote_jdk8_linux_toolchain_config_repo//:all".to_owned(),
                "@remotejdk25_macos_aarch64_toolchain_config_repo//:all".to_owned(),
            ]
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
    fn test_bzlmod_extension_generated_repos_inherit_extension_host_repo_mapping() {
        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        let gazelle_cell = bzlmod_cell_name("gazelle+");
        let package_metadata_cell = bzlmod_cell_name("package_metadata+");
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            &gazelle_cell,
            "package_metadata",
            &package_metadata_cell,
        );

        let mut generated = Vec::new();
        let mut generated_repo_declaring_cells = Vec::new();
        let generated_cell_name = super::add_generated_bzlmod_repo_with_mapping_cell(
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut cell_aliases_by_cell,
            "root",
            "com_github_example_dep",
            &gazelle_cell,
            "gazelle++go_deps+com_github_example_dep",
            "{}".to_owned(),
        );

        let mut extension_generated_repo_groups = std::collections::BTreeMap::new();
        extension_generated_repo_groups.insert(
            "gazelle++go_deps".to_owned(),
            vec![(
                "com_github_example_dep".to_owned(),
                generated_cell_name.clone(),
            )],
        );
        let mut extension_repo_override_groups = std::collections::BTreeMap::new();
        extension_repo_override_groups.insert(
            "gazelle++go_deps".to_owned(),
            vec![(
                "com_github_buildbuddy_io_buildbuddy".to_owned(),
                "root".to_owned(),
            )],
        );

        super::add_generated_bzlmod_repo_mappings(
            &mut cell_aliases_by_cell,
            &generated_repo_declaring_cells,
            &extension_generated_repo_groups,
            &extension_repo_override_groups,
        );

        assert_eq!(
            super::bzlmod_cell_alias_target(
                &cell_aliases_by_cell,
                "root",
                "com_github_example_dep"
            ),
            Some(generated_cell_name.as_str())
        );
        assert_eq!(
            super::bzlmod_cell_alias_target(
                &cell_aliases_by_cell,
                &generated_cell_name,
                "package_metadata"
            ),
            Some(package_metadata_cell.as_str())
        );
        assert_eq!(
            super::bzlmod_cell_alias_target(
                &cell_aliases_by_cell,
                &generated_cell_name,
                "com_github_buildbuddy_io_buildbuddy"
            ),
            Some("root")
        );
        assert_eq!(
            super::bzlmod_cell_alias_target(
                &cell_aliases_by_cell,
                &generated_cell_name,
                "com_github_example_dep"
            ),
            Some(generated_cell_name.as_str())
        );
    }

    #[test]
    fn test_bzlmod_extension_without_lockfile_uses_static_repos() -> buck2_error::Result<()> {
        let usage = super::BzlmodExtensionUsage {
            proxy_name: "npm".to_owned(),
            extension_bzl_file: "@aspect_rules_js//npm:extensions.bzl".to_owned(),
            extension_name: "npm".to_owned(),
            dev_dependency: false,
            imports: vec![super::BzlmodUseRepoImport {
                alias: "npm".to_owned(),
                repo_name: "npm".to_owned(),
            }],
            repo_overrides: Vec::new(),
            tags: vec![super::BzlmodExtensionTag {
                tag_name: "npm_translate_lock".to_owned(),
                bindings: Vec::new(),
                kwargs: vec![("name".to_owned(), "\"npm\"".to_owned())],
            }],
        };
        let root_module = super::RootBzlmodModule {
            name: "buildbuddy".to_owned(),
            version: String::new(),
            repo_name: "buildbuddy".to_owned(),
            canonical_repo_name: String::new(),
            lockfile_extension_generated_repos: std::collections::BTreeMap::new(),
            lockfile_extension_facts: std::collections::BTreeSet::new(),
            constants: Vec::new(),
            extension_usages: vec![usage.clone()],
            use_repo_rule_invocations: Vec::new(),
            registered_toolchains: Vec::new(),
        };

        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "root",
            "aspect_rules_js",
            "bzlmod_aspect_rules_js_",
        );

        let mut extension_unique_names = std::collections::BTreeMap::new();
        extension_unique_names.insert(
            super::BzlmodExtensionId {
                bzl_cell_name: "bzlmod_aspect_rules_js_".to_owned(),
                bzl_path: "npm/extensions.bzl".to_owned(),
                extension_name: "npm".to_owned(),
            },
            "aspect_rules_js++npm".to_owned(),
        );

        let mut canonical_repo_names_by_cell = std::collections::BTreeMap::new();
        canonical_repo_names_by_cell.insert(
            "bzlmod_aspect_rules_js_".to_owned(),
            "aspect_rules_js+".to_owned(),
        );

        let mut generated = Vec::new();
        let mut generated_repo_declaring_cells = Vec::new();
        let mut extension_generated_repo_groups = std::collections::BTreeMap::new();
        let mut extension_repo_override_groups = std::collections::BTreeMap::new();
        let mut extension_usages_json_by_id = std::collections::BTreeMap::new();
        insert_test_extension_usages_json(
            &mut extension_usages_json_by_id,
            super::BzlmodExtensionId {
                bzl_cell_name: "bzlmod_aspect_rules_js_".to_owned(),
                bzl_path: "npm/extensions.bzl".to_owned(),
                extension_name: "npm".to_owned(),
            },
        )?;

        super::resolve_bzlmod_extension_usage_generated_repos(
            &usage,
            "",
            "root",
            true,
            &root_module,
            &mut cell_aliases_by_cell,
            &canonical_repo_names_by_cell,
            &extension_unique_names,
            &mut extension_usages_json_by_id,
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut extension_generated_repo_groups,
            &mut extension_repo_override_groups,
        )?;

        assert_eq!(
            super::bzlmod_cell_alias_target(&cell_aliases_by_cell, "root", "npm"),
            Some("bzlmod_aspect_rules_js__npm_npm")
        );

        Ok(())
    }

    #[test]
    fn test_bzlmod_root_tag_only_extension_without_lockfile_stays_demand_driven()
    -> buck2_error::Result<()> {
        let usage = super::BzlmodExtensionUsage {
            proxy_name: "go_sdk".to_owned(),
            extension_bzl_file: "@io_bazel_rules_go//go:extensions.bzl".to_owned(),
            extension_name: "go_sdk".to_owned(),
            dev_dependency: false,
            imports: Vec::new(),
            repo_overrides: Vec::new(),
            tags: vec![super::BzlmodExtensionTag {
                tag_name: "download".to_owned(),
                bindings: Vec::new(),
                kwargs: vec![("version".to_owned(), "\"1.24.0\"".to_owned())],
            }],
        };
        let root_module = super::RootBzlmodModule {
            name: "bazelisk".to_owned(),
            version: String::new(),
            repo_name: "bazelisk".to_owned(),
            canonical_repo_name: String::new(),
            lockfile_extension_generated_repos: std::collections::BTreeMap::new(),
            lockfile_extension_facts: std::collections::BTreeSet::new(),
            constants: Vec::new(),
            extension_usages: vec![usage.clone()],
            use_repo_rule_invocations: Vec::new(),
            registered_toolchains: Vec::new(),
        };

        let rules_go_cell = bzlmod_cell_name("rules_go+");
        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "root",
            "io_bazel_rules_go",
            &rules_go_cell,
        );

        let mut extension_unique_names = std::collections::BTreeMap::new();
        extension_unique_names.insert(
            super::BzlmodExtensionId {
                bzl_cell_name: rules_go_cell.clone(),
                bzl_path: "go/extensions.bzl".to_owned(),
                extension_name: "go_sdk".to_owned(),
            },
            "rules_go++go_sdk".to_owned(),
        );

        let mut canonical_repo_names_by_cell = std::collections::BTreeMap::new();
        canonical_repo_names_by_cell.insert(rules_go_cell.clone(), "rules_go+".to_owned());

        let mut generated = Vec::new();
        let mut generated_repo_declaring_cells = Vec::new();
        let mut extension_generated_repo_groups = std::collections::BTreeMap::new();
        let mut extension_repo_override_groups = std::collections::BTreeMap::new();
        let mut extension_usages_json_by_id = std::collections::BTreeMap::new();
        insert_test_extension_usages_json(
            &mut extension_usages_json_by_id,
            super::BzlmodExtensionId {
                bzl_cell_name: rules_go_cell,
                bzl_path: "go/extensions.bzl".to_owned(),
                extension_name: "go_sdk".to_owned(),
            },
        )?;

        super::resolve_bzlmod_extension_usage_generated_repos(
            &usage,
            "",
            "root",
            true,
            &root_module,
            &mut cell_aliases_by_cell,
            &canonical_repo_names_by_cell,
            &extension_unique_names,
            &mut extension_usages_json_by_id,
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut extension_generated_repo_groups,
            &mut extension_repo_override_groups,
        )?;

        assert!(generated.is_empty());

        Ok(())
    }

    #[test]
    fn test_bzlmod_non_root_extension_without_lockfile_uses_static_repos() -> buck2_error::Result<()>
    {
        let usage = super::BzlmodExtensionUsage {
            proxy_name: "npm".to_owned(),
            extension_bzl_file: "@aspect_rules_js//npm:extensions.bzl".to_owned(),
            extension_name: "npm".to_owned(),
            dev_dependency: false,
            imports: vec![super::BzlmodUseRepoImport {
                alias: "npm".to_owned(),
                repo_name: "npm".to_owned(),
            }],
            repo_overrides: Vec::new(),
            tags: Vec::new(),
        };
        let root_module = super::RootBzlmodModule {
            name: "buildbuddy".to_owned(),
            version: String::new(),
            repo_name: "buildbuddy".to_owned(),
            canonical_repo_name: String::new(),
            lockfile_extension_generated_repos: std::collections::BTreeMap::new(),
            lockfile_extension_facts: std::collections::BTreeSet::new(),
            constants: Vec::new(),
            extension_usages: Vec::new(),
            use_repo_rule_invocations: Vec::new(),
            registered_toolchains: Vec::new(),
        };

        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "bzlmod_dep_",
            "aspect_rules_js",
            "bzlmod_aspect_rules_js_",
        );

        let mut extension_unique_names = std::collections::BTreeMap::new();
        extension_unique_names.insert(
            super::BzlmodExtensionId {
                bzl_cell_name: "bzlmod_aspect_rules_js_".to_owned(),
                bzl_path: "npm/extensions.bzl".to_owned(),
                extension_name: "npm".to_owned(),
            },
            "aspect_rules_js++npm".to_owned(),
        );

        let mut canonical_repo_names_by_cell = std::collections::BTreeMap::new();
        canonical_repo_names_by_cell.insert(
            "bzlmod_aspect_rules_js_".to_owned(),
            "aspect_rules_js+".to_owned(),
        );

        let mut generated = Vec::new();
        let mut generated_repo_declaring_cells = Vec::new();
        let mut extension_generated_repo_groups = std::collections::BTreeMap::new();
        let mut extension_repo_override_groups = std::collections::BTreeMap::new();
        let mut extension_usages_json_by_id = std::collections::BTreeMap::new();
        insert_test_extension_usages_json(
            &mut extension_usages_json_by_id,
            super::BzlmodExtensionId {
                bzl_cell_name: "bzlmod_aspect_rules_js_".to_owned(),
                bzl_path: "npm/extensions.bzl".to_owned(),
                extension_name: "npm".to_owned(),
            },
        )?;

        super::resolve_bzlmod_extension_usage_generated_repos(
            &usage,
            "dep+",
            "bzlmod_dep_",
            false,
            &root_module,
            &mut cell_aliases_by_cell,
            &canonical_repo_names_by_cell,
            &extension_unique_names,
            &mut extension_usages_json_by_id,
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut extension_generated_repo_groups,
            &mut extension_repo_override_groups,
        )?;

        assert_eq!(
            super::bzlmod_cell_alias_target(&cell_aliases_by_cell, "bzlmod_dep_", "npm"),
            Some("bzlmod_aspect_rules_js__npm_npm")
        );

        Ok(())
    }

    #[test]
    fn test_bzlmod_lockfile_extension_generated_repos() -> buck2_error::Result<()> {
        let lockfile_data = super::bzlmod_lockfile_data_from_str(indoc!(
            r#"
            {
              "facts": {
                "@@rules_go+//go:extensions.bzl%go_sdk": {
                  "1.23.5": {}
                }
              },
              "moduleExtensions": {
                "@@rules_go+//go:extensions.bzl%go_sdk": {
                  "general": {
                    "generatedRepoSpecs": {
                      "go_toolchains": {},
                      "main___download_0": {}
                    }
                  },
                  "os:darwin": {
                    "generatedRepoSpecs": {
                      "darwin_only": {}
                    }
                  }
                },
                "@@googleapis+//:extensions.bzl%switched_rules": {
                  "general": {
                    "generatedRepoSpecs": {}
                  }
                }
              }
            }
            "#
        ))?;
        let repos = lockfile_data.extension_generated_repos;

        assert_eq!(
            repos
                .get("@@rules_go+//go:extensions.bzl%go_sdk")
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                "darwin_only".to_owned(),
                "go_toolchains".to_owned(),
                "main___download_0".to_owned(),
            ]
        );
        assert!(
            lockfile_data
                .extension_facts
                .contains("@@rules_go+//go:extensions.bzl%go_sdk")
        );
        assert_eq!(
            repos
                .get("@@googleapis+//:extensions.bzl%switched_rules")
                .unwrap(),
            &std::collections::BTreeSet::new()
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_lockfile_extension_key() -> buck2_error::Result<()> {
        let mut canonical_repo_names_by_cell = std::collections::BTreeMap::new();
        let rules_go_cell = bzlmod_cell_name("rules_go+");
        canonical_repo_names_by_cell.insert(rules_go_cell.clone(), "rules_go+".to_owned());

        assert_eq!(
            super::bzlmod_lockfile_extension_key(
                &super::BzlmodExtensionId {
                    bzl_cell_name: "root".to_owned(),
                    bzl_path: "repositories.bzl".to_owned(),
                    extension_name: "async_profiler_repos".to_owned(),
                },
                &canonical_repo_names_by_cell,
            )?,
            "//:repositories.bzl%async_profiler_repos"
        );
        assert_eq!(
            super::bzlmod_lockfile_extension_key(
                &super::BzlmodExtensionId {
                    bzl_cell_name: rules_go_cell,
                    bzl_path: "go/extensions.bzl".to_owned(),
                    extension_name: "go_sdk".to_owned(),
                },
                &canonical_repo_names_by_cell,
            )?,
            "@@rules_go+//go:extensions.bzl%go_sdk"
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_registered_toolchains_resolve_declaring_repo_mapping() -> buck2_error::Result<()>
    {
        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        let rules_go_cell = bzlmod_cell_name("rules_go+0.57.0");
        let go_toolchains_cell = bzlmod_cell_name("rules_go+0.57.0+go_sdk+go_toolchains");
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            &rules_go_cell,
            "go_toolchains",
            &go_toolchains_cell,
        );

        assert_eq!(
            super::qualify_bzlmod_registered_toolchain(
                "@go_toolchains//:all",
                &rules_go_cell,
                &cell_aliases_by_cell,
            )?,
            format!("{go_toolchains_cell}//:all")
        );
        assert_eq!(
            super::qualify_bzlmod_registered_toolchain(
                "//:all",
                &rules_go_cell,
                &cell_aliases_by_cell,
            )?,
            format!("{rules_go_cell}//:all")
        );

        Ok(())
    }

    #[test]
    fn test_bzlmod_registered_toolchains_include_root_module() -> buck2_error::Result<()> {
        let root_module = super::RootBzlmodModule {
            name: "root".to_owned(),
            version: String::new(),
            repo_name: "root".to_owned(),
            canonical_repo_name: String::new(),
            lockfile_extension_generated_repos: std::collections::BTreeMap::new(),
            lockfile_extension_facts: std::collections::BTreeSet::new(),
            constants: Vec::new(),
            extension_usages: Vec::new(),
            use_repo_rule_invocations: Vec::new(),
            registered_toolchains: vec![
                "@rust_toolchains//:all".to_owned(),
                "//tools:toolchain".to_owned(),
            ],
        };
        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "root",
            "rust_toolchains",
            "bzlmod_rules_rust__rust_rust_toolchains",
        );

        assert_eq!(
            super::resolve_bzlmod_registered_toolchains(
                &root_module,
                &std::collections::BTreeMap::new(),
                &[],
                &std::collections::BTreeMap::new(),
                &cell_aliases_by_cell,
            )?,
            vec![
                "bzlmod_rules_rust__rust_rust_toolchains//:all",
                "root//tools:toolchain",
            ]
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
