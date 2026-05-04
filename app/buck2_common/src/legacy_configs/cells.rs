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
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

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
use buck2_core::cells::external::BzlmodRepositoryRuleSetup;
use buck2_core::cells::external::BzlmodShellConfigSetup;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::external::GitCellSetup;
use buck2_core::cells::external::GitObjectFormat;
use buck2_core::cells::external::bzlmod_cell_name;
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
}

#[derive(PartialEq, Eq, Allocative, Clone, Pagable)]
pub struct ExternalPathBuckconfigData {
    pub(crate) parse_state: LegacyConfigParser,
    pub(crate) origin_path: ConfigPath,
}

impl ExternalBuckconfigData {
    pub fn testing_default() -> Self {
        Self {
            external_path_configs: Vec::new(),
            args: Vec::new(),
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

        let config_paths = get_project_buckconfig_paths(cell_path, file_ops).await?;
        let config = LegacyBuckConfig::finish_parse(
            self.external_data.external_path_configs.clone(),
            &config_paths,
            cell_path,
            file_ops,
            &[],
            follow_includes,
        )
        .await?;
        let config = if should_apply_bazel_compat_defaults(cell_path, file_ops).await?
            || cell_name.as_str() == "bazel_tools"
        {
            let root_path = CellRootPathBuf::new(ProjectRelativePath::empty().to_owned());
            let module_aliases = get_bazel_module_resolution(&root_path, file_ops).await?;
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
        Self::parse_with_file_ops_and_options(
            &mut DefaultConfigParserFileOps {
                project_fs: project_fs.dupe(),
            },
            config_args,
            false, /* follow includes */
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
        )
        .await
    }

    async fn parse_with_file_ops_and_options(
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[buck2_cli_proto::ConfigOverride],
        follow_includes: bool,
    ) -> buck2_error::Result<Self> {
        Self::parse_with_file_ops_and_options_inner(file_ops, config_args, follow_includes)
            .await
            .buck_error_context("Parsing cells")
    }

    async fn parse_with_file_ops_and_options_inner(
        file_ops: &mut dyn ConfigParserFileOps,
        config_args: &[buck2_cli_proto::ConfigOverride],
        follow_includes: bool,
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
        let root_config = if should_apply_bazel_compat_defaults(&root_path, &mut file_ops).await? {
            let module_aliases = get_bazel_module_resolution(&root_path, &mut file_ops).await?;
            root_config.with_bazel_compat_defaults(
                module_aliases.aliases_for_cell("root"),
                &module_aliases.external_modules,
                &module_aliases.registered_toolchains,
            )
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
        let config_paths = get_project_buckconfig_paths(cell_path, file_ops).await?;
        let config = LegacyBuckConfig::finish_parse(
            external_data.external_path_configs.clone(),
            &config_paths,
            cell_path,
            file_ops,
            external_data.args.as_ref(),
            /* follow includes */ true,
        )
        .await?;

        if should_apply_bazel_compat_defaults(cell_path, file_ops).await?
            || cell_name == "bazel_tools"
        {
            let root_path = CellRootPathBuf::new(ProjectRelativePath::empty().to_owned());
            let module_aliases = get_bazel_module_resolution(&root_path, file_ops).await?;
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
                BzlmodGeneratedRepoConfig::ModuleExtensionRepo {
                    parent_canonical_repo_name,
                    extension_bzl_file,
                    extension_name,
                    repo_name,
                    extension_usages_json,
                } => BzlmodGeneratedCellGenerator::ModuleExtensionRepo(
                    BzlmodModuleExtensionRepoSetup {
                        parent_canonical_repo_name: Arc::from(parent_canonical_repo_name),
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

#[derive(Default)]
struct BazelModuleCellAliases {
    root_aliases: Vec<BazelCompatCellAlias>,
    cell_aliases: BTreeMap<String, Vec<BazelCompatCellAlias>>,
    external_modules: Vec<BazelCompatExternalModule>,
    registered_toolchains: Vec<String>,
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
    registered_toolchains: Vec<String>,
    deps: Vec<BazelDep>,
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
    kwargs: Vec<(String, String)>,
}

struct BcrResolution {
    external_modules: Vec<BazelCompatExternalModule>,
    root_aliases: Vec<BazelCompatCellAlias>,
    cell_aliases: BTreeMap<String, Vec<BazelCompatCellAlias>>,
    registered_toolchains: Vec<String>,
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
    ModuleExtensionRepo {
        parent_canonical_repo_name: String,
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
    tags: Vec<BzlmodModuleExtensionTagConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BzlmodModuleExtensionTagConfig {
    tag_name: String,
    dev_dependency: bool,
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

async fn get_bazel_module_resolution(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> buck2_error::Result<BazelModuleCellAliases> {
    let mut aliases = BazelModuleCellAliases::default();
    let mut root_deps = Vec::new();
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
            let Some(version) = bzl_string_arg(&call, "version") else {
                continue;
            };
            let apparent_name = bzl_repo_name_arg(&call, &name);
            root_deps.push(BazelDep {
                name,
                version,
                apparent_name,
            });
        }

        for call in collect_bzl_calls(&lines, "archive_override(") {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "archive_override is not implemented in Buck2 bzlmod resolution yet: {}",
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
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "local_path_override is not implemented in Buck2 bzlmod resolution yet: {}",
                call
            ));
        }

        for call in collect_bzl_calls(&lines, "use_repo(") {
            if !bzl_use_repo_aliases(&call).is_empty() {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "module extensions are not implemented in Buck2 bzlmod resolution yet: {}",
                    call
                ));
            }
        }

        for repo_rule in bzl_use_repo_rule_names(&lines) {
            for call in collect_bzl_calls(&lines, &format!("{repo_rule}(")) {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "use_repo_rule repositories are not implemented in Buck2 bzlmod resolution yet: {}",
                    call
                ));
            }
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

    let bcr_resolution = resolve_bcr_modules(root_deps).await?;
    aliases.external_modules = bcr_resolution.external_modules;
    aliases.root_aliases.extend(bcr_resolution.root_aliases);
    aliases.cell_aliases = bcr_resolution.cell_aliases;
    aliases
        .registered_toolchains
        .extend(bcr_resolution.registered_toolchains);
    aliases.normalize();
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

async fn resolve_bcr_modules(root_deps: Vec<BazelDep>) -> buck2_error::Result<BcrResolution> {
    std::thread::Builder::new()
        .name("buck2-bzlmod-resolver".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .buck_error_context("Error creating Tokio runtime for bzlmod resolution")?;
            runtime.block_on(async move {
                let client = bzlmod_http_client().await?;
                resolve_bcr_modules_with_client(root_deps, &client).await
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
    client: &HttpClient,
) -> buck2_error::Result<BcrResolution> {
    let registry = "https://bcr.bazel.build";
    let mut discovered = BTreeMap::<(String, String), DiscoveredBcrModule>::new();
    let mut scheduled = BTreeSet::<(String, String)>::new();
    let mut pending = FuturesUnordered::<BcrModuleFetch>::new();

    for dep in &root_deps {
        schedule_bcr_module_fetch(registry, client, dep.clone(), &mut scheduled, &mut pending);
    }

    while let Some(module) = pending.next().await {
        let module = module?;
        let key = (module.dep.name.clone(), module.dep.version.clone());
        for child in &module.deps {
            schedule_bcr_module_fetch(
                registry,
                client,
                child.clone(),
                &mut scheduled,
                &mut pending,
            );
        }

        discovered.insert(key, module);
    }

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

    let mut root_aliases_by_key = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let mut cell_aliases_by_cell = BTreeMap::<String, BTreeMap<String, String>>::new();
    for dep in &root_deps {
        add_bzlmod_dep_alias(dep, &selected_versions, &mut root_aliases_by_key);
        add_bzlmod_dep_cell_alias("root", dep, &selected_versions, &mut cell_aliases_by_cell);
    }
    for key in &selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_canonical_repo_name(&module.dep.name, &module.dep.version);
        let cell_name = bzlmod_cell_name(&canonical_repo_name);
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
                &mut cell_aliases_by_cell,
            );
        }
    }

    let selected_keys_for_generated = selected_keys.clone();

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

        let canonical_repo_name = bzlmod_canonical_repo_name(&module.dep.name, &module.dep.version);
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
    resolved.extend(resolve_generated_bzlmod_repos(
        &discovered,
        &selected_keys_for_generated,
        &mut cell_aliases_by_cell,
    )?);
    let registered_toolchains = resolve_bzlmod_registered_toolchains(
        &discovered,
        &selected_keys_for_generated,
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

fn resolve_generated_bzlmod_repos(
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    cell_aliases_by_cell: &mut BTreeMap<String, BTreeMap<String, String>>,
) -> buck2_error::Result<Vec<BazelCompatExternalModule>> {
    let mut generated = Vec::new();
    let mut generated_repo_declaring_cells = Vec::new();
    let mut extension_generated_repo_groups =
        BTreeMap::<(String, String, String), Vec<(String, String)>>::new();
    let mut needs_local_config_platform = false;
    let mut local_config_platform_importing_cells = Vec::new();
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let parent_canonical_repo_name =
            bzlmod_canonical_repo_name(&module.dep.name, &module.dep.version);
        let parent_cell_name = bzlmod_cell_name(&parent_canonical_repo_name);
        if module.dep.name == "rules_cc" {
            for alias in &module.use_repo_aliases {
                let generator = match alias.as_str() {
                    "local_config_cc_toolchains" => {
                        needs_local_config_platform = true;
                        local_config_platform_importing_cells.push(parent_cell_name.clone());
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
                let canonical_repo_name = format!(
                    "{}+{}+cc_configure+{}",
                    module.dep.name, module.dep.version, alias
                );
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
                let canonical_repo_name = format!(
                    "{}+{}+sh_configure+{}",
                    module.dep.name, module.dep.version, alias
                );
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
                let canonical_repo_name = format!(
                    "{}+{}+toolchains+{}",
                    module.dep.name, module.dep.version, alias
                );
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

        if module.dep.name == "rules_python" {
            for alias in &module.use_repo_aliases {
                if alias != "pythons_hub" {
                    continue;
                }
                let canonical_repo_name = format!(
                    "{}+{}+python+{}",
                    module.dep.name, module.dep.version, alias
                );
                let generator_json =
                    serde_json::to_string(&BzlmodGeneratedRepoConfig::PythonHub {})
                        .buck_error_context(
                            "Error serializing generated rules_python hub repo configuration",
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
                    "{}+{}+version_extension+{}",
                    module.dep.name, module.dep.version, import.repo_name
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
            let extension_usages_json = bzlmod_module_extension_evaluation_config_json(
                discovered,
                selected_keys,
                &usage.extension_bzl_file,
                &usage.extension_name,
            )?;
            let extension_group_key = (
                parent_cell_name.clone(),
                usage.extension_bzl_file.clone(),
                usage.extension_name.clone(),
            );
            for import in &usage.imports {
                if bzlmod_cell_alias_target(cell_aliases_by_cell, &parent_cell_name, &import.alias)
                    .is_some()
                {
                    continue;
                }
                let canonical_repo_name =
                    bzlmod_extension_repo_canonical_repo_name(module, usage, &import.repo_name);
                let generator_json =
                    serde_json::to_string(&BzlmodGeneratedRepoConfig::ModuleExtensionRepo {
                        parent_canonical_repo_name: parent_canonical_repo_name.clone(),
                        extension_bzl_file: usage.extension_bzl_file.clone(),
                        extension_name: usage.extension_name.clone(),
                        repo_name: import.repo_name.clone(),
                        extension_usages_json: extension_usages_json.clone(),
                    })
                    .buck_error_context(
                        "Error serializing generated module extension repo configuration",
                    )?;
                let generated_cell_name = add_generated_bzlmod_repo(
                    &mut generated,
                    &mut generated_repo_declaring_cells,
                    cell_aliases_by_cell,
                    &parent_cell_name,
                    &import.alias,
                    &canonical_repo_name,
                    generator_json,
                );
                extension_generated_repo_groups
                    .entry(extension_group_key.clone())
                    .or_default()
                    .push((import.repo_name.clone(), generated_cell_name));
            }
            for repo_name in bzlmod_extension_tag_repo_names(usage) {
                let canonical_repo_name =
                    bzlmod_extension_repo_canonical_repo_name(module, usage, &repo_name);
                let generator_json =
                    serde_json::to_string(&BzlmodGeneratedRepoConfig::ModuleExtensionRepo {
                        parent_canonical_repo_name: parent_canonical_repo_name.clone(),
                        extension_bzl_file: usage.extension_bzl_file.clone(),
                        extension_name: usage.extension_name.clone(),
                        repo_name: repo_name.clone(),
                        extension_usages_json: extension_usages_json.clone(),
                    })
                    .buck_error_context(
                        "Error serializing generated module extension repo configuration",
                    )?;
                let generated_cell_name = add_unimported_generated_bzlmod_repo(
                    &mut generated,
                    &mut generated_repo_declaring_cells,
                    &parent_cell_name,
                    &canonical_repo_name,
                    generator_json,
                );
                extension_generated_repo_groups
                    .entry(extension_group_key.clone())
                    .or_default()
                    .push((repo_name, generated_cell_name));
            }
        }
    }
    if needs_local_config_platform {
        let canonical_repo_name = "local_config_platform".to_owned();
        let generator_json =
            serde_json::to_string(&BzlmodGeneratedRepoConfig::LocalConfigPlatform {})
                .buck_error_context(
                    "Error serializing generated local_config_platform repo configuration",
                )?;
        let cell_name = bzlmod_cell_name(&canonical_repo_name);
        local_config_platform_importing_cells.sort();
        local_config_platform_importing_cells.dedup();
        for parent_cell_name in &local_config_platform_importing_cells {
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
    Ok(generated)
}

fn bzlmod_module_extension_evaluation_config_json(
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    extension_bzl_file: &str,
    extension_name: &str,
) -> buck2_error::Result<String> {
    let mut modules = Vec::new();
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let mut has_usage = false;
        let mut tags = Vec::new();
        for usage in &module.extension_usages {
            if usage.extension_bzl_file != extension_bzl_file
                || usage.extension_name != extension_name
            {
                continue;
            }
            has_usage = true;
            tags.extend(usage.tags.iter().map(|tag| BzlmodModuleExtensionTagConfig {
                tag_name: tag.tag_name.clone(),
                dev_dependency: usage.dev_dependency,
                kwargs: tag.kwargs.clone(),
            }));
        }
        if !has_usage {
            continue;
        }
        let canonical_repo_name = bzlmod_canonical_repo_name(&module.dep.name, &module.dep.version);
        modules.push(BzlmodModuleExtensionModuleConfig {
            name: module.dep.name.clone(),
            version: module.dep.version.clone(),
            canonical_repo_name,
            is_root: false,
            tags,
        });
    }

    serde_json::to_string(&BzlmodModuleExtensionEvaluationConfig { modules })
        .buck_error_context("Error serializing module extension evaluation configuration")
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
    extension_generated_repo_groups: &BTreeMap<(String, String, String), Vec<(String, String)>>,
) {
    for (generated_cell_name, declaring_cell_name) in generated_repo_declaring_cells {
        let Some(declaring_aliases) = cell_aliases_by_cell.get(declaring_cell_name).cloned() else {
            continue;
        };
        cell_aliases_by_cell
            .entry(generated_cell_name.clone())
            .or_default()
            .extend(declaring_aliases);
    }

    for generated_repos in extension_generated_repo_groups.values() {
        for (_, generated_cell_name) in generated_repos {
            for (repo_name, target_cell_name) in generated_repos {
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

fn bzlmod_extension_repo_canonical_repo_name(
    module: &DiscoveredBcrModule,
    usage: &BzlmodExtensionUsage,
    repo_name: &str,
) -> String {
    format!(
        "{}+{}+{}+{}",
        module.dep.name, module.dep.version, usage.extension_name, repo_name
    )
}

fn resolve_bzlmod_registered_toolchains(
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    cell_aliases_by_cell: &BTreeMap<String, BTreeMap<String, String>>,
) -> buck2_error::Result<Vec<String>> {
    let mut registered_toolchains = Vec::new();
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_canonical_repo_name(&module.dep.name, &module.dep.version);
        let cell_name = bzlmod_cell_name(&canonical_repo_name);
        for pattern in &module.registered_toolchains {
            registered_toolchains.push(qualify_bzlmod_registered_toolchain(
                pattern,
                &cell_name,
                cell_aliases_by_cell,
            )?);
        }
    }
    registered_toolchains.sort();
    registered_toolchains.dedup();
    Ok(registered_toolchains)
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
    aliases_by_cell: &mut BTreeMap<String, BTreeMap<String, String>>,
) {
    let Some(alias) = dep.apparent_name.as_ref() else {
        return;
    };
    let Some(version) = selected_versions.get(&dep.name) else {
        return;
    };
    let canonical_repo_name = bzlmod_canonical_repo_name(&dep.name, version);
    let cell_name = bzlmod_cell_name(&canonical_repo_name);
    add_bzlmod_cell_alias(aliases_by_cell, current_cell_name, alias, &cell_name);
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
    dep: BazelDep,
    scheduled: &mut BTreeSet<(String, String)>,
    pending: &mut FuturesUnordered<BcrModuleFetch>,
) {
    let key = (dep.name.clone(), dep.version.clone());
    if scheduled.insert(key) {
        pending.push(fetch_bcr_module(registry, client.dupe(), dep).boxed());
    }
}

async fn fetch_bcr_module(
    registry: &'static str,
    client: HttpClient,
    dep: BazelDep,
) -> buck2_error::Result<DiscoveredBcrModule> {
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
            .with_buck_error_context(|| format!("Invalid BCR source metadata at `{source_url}`"))?;
    let module_text = http_get_text(&client, &module_url).await?;
    let module_lines = module_text.lines().map(str::to_owned).collect::<Vec<_>>();
    let extension_usages = bzlmod_extension_usages_from_lines(&module_lines);

    Ok(DiscoveredBcrModule {
        dep,
        source_json,
        module_aliases: bzlmod_module_aliases(&module_lines),
        use_repo_aliases: bzlmod_use_repo_aliases_from_usages(&extension_usages),
        extension_usages,
        registered_toolchains: bzlmod_registered_toolchains_from_lines(&module_lines, true),
        deps: bzlmod_deps_from_lines(&module_lines, true),
    })
}

async fn http_get_text(client: &HttpClient, url: &str) -> buck2_error::Result<String> {
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
    String::from_utf8(bytes)
        .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Input))
        .with_buck_error_context(|| format!("Invalid UTF-8 response from `{url}`"))
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
        let Some(version) = bzl_string_arg(&call, "version") else {
            continue;
        };
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

fn bzlmod_extension_usages_from_lines(lines: &[String]) -> Vec<BzlmodExtensionUsage> {
    let mut usages = lines
        .iter()
        .filter_map(|line| {
            let line = strip_bzl_comment(line);
            let (name, value) = line.split_once('=')?;
            let name = name.trim();
            if !is_bzl_identifier(name) || !value.trim_start().starts_with("use_extension(") {
                return None;
            }
            let args = bzl_call_args(value);
            let extension_bzl_file = args.first().and_then(|arg| bzl_string_value(arg.trim()))?;
            let extension_name = args.get(1).and_then(|arg| bzl_string_value(arg.trim()))?;
            Some(BzlmodExtensionUsage {
                proxy_name: name.to_owned(),
                extension_bzl_file,
                extension_name,
                dev_dependency: bzl_bool_arg(value, "dev_dependency"),
                imports: bzlmod_extension_imports(lines, name),
                tags: bzlmod_extension_tags(lines, name),
            })
        })
        .collect::<Vec<_>>();
    usages.sort_by(|left, right| {
        (
            &left.proxy_name,
            &left.extension_bzl_file,
            &left.extension_name,
            left.tags.len(),
        )
            .cmp(&(
                &right.proxy_name,
                &right.extension_bzl_file,
                &right.extension_name,
                right.tags.len(),
            ))
    });
    usages.dedup_by(|left, right| {
        left.extension_bzl_file == right.extension_bzl_file
            && left.extension_name == right.extension_name
            && left.proxy_name == right.proxy_name
    });
    usages
}

fn bzlmod_extension_tags(lines: &[String], proxy_name: &str) -> Vec<BzlmodExtensionTag> {
    let call_prefix = format!("{proxy_name}.");
    let mut tags = collect_bzl_calls(lines, &call_prefix)
        .into_iter()
        .filter_map(|call| {
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
                kwargs,
            })
        })
        .collect::<Vec<_>>();
    tags.sort_by(|left, right| {
        (&left.tag_name, &left.kwargs).cmp(&(&right.tag_name, &right.kwargs))
    });
    tags
}

fn bzlmod_extension_imports(lines: &[String], proxy_name: &str) -> Vec<BzlmodUseRepoImport> {
    collect_bzl_calls(lines, "use_repo(")
        .into_iter()
        .filter(|call| {
            bzl_call_args(call)
                .first()
                .is_some_and(|arg| arg.trim() == proxy_name)
        })
        .flat_map(|call| bzl_use_repo_imports(&call))
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

fn bzlmod_canonical_repo_name(module_name: &str, version: &str) -> String {
    match module_name {
        "bazel_tools" => "bazel_tools".to_owned(),
        "platforms" => "platforms".to_owned(),
        _ => format!("{module_name}+{version}"),
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
            let Some(start) = line.find(function) else {
                continue;
            };
            let rest = &line[start..];
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

fn bzl_use_repo_aliases(call: &str) -> Vec<String> {
    bzl_use_repo_imports(call)
        .into_iter()
        .map(|import| import.alias)
        .collect()
}

fn bzl_use_repo_imports(call: &str) -> Vec<BzlmodUseRepoImport> {
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
                if let Some(repo_name) = bzl_string_value(actual.trim()) {
                    imports.push(BzlmodUseRepoImport {
                        alias: alias.to_owned(),
                        repo_name,
                    });
                }
            }
        } else if let Some(alias) = bzl_string_value(arg) {
            imports.push(BzlmodUseRepoImport {
                alias: alias.clone(),
                repo_name: alias,
            });
        }
    }
    imports
}

fn bzl_use_repo_rule_names(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| {
            let line = strip_bzl_comment(line);
            let (name, value) = line.split_once('=')?;
            if value.trim_start().starts_with("use_repo_rule(") {
                let name = name.trim();
                if is_bzl_identifier(name) {
                    return Some(name.to_owned());
                }
            }
            None
        })
        .collect()
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
            sdk.from_file(name = "go_default_sdk", go_mod = "//:go.mod")
            use_repo(
                sdk,
                "go_toolchains",
                alias_name = "actual_repo",
            )

            features = use_extension("@bazel_features//:extensions.bzl", "version_extension")
            use_repo(features, "bazel_features_globals")
            "#
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

        let usages = super::bzlmod_extension_usages_from_lines(&lines);
        assert_eq!(usages.len(), 2);
        assert_eq!(usages[0].proxy_name, "features");
        assert_eq!(
            usages[0].extension_bzl_file,
            "@bazel_features//:extensions.bzl"
        );
        assert_eq!(usages[0].extension_name, "version_extension");
        assert_eq!(usages[0].imports.len(), 1);
        assert_eq!(usages[0].imports[0].alias, "bazel_features_globals");
        assert_eq!(usages[0].imports[0].repo_name, "bazel_features_globals");

        assert_eq!(usages[1].proxy_name, "sdk");
        assert_eq!(usages[1].extension_bzl_file, "//go:extensions.bzl");
        assert_eq!(usages[1].extension_name, "go_sdk");
        assert_eq!(usages[1].tags.len(), 1);
        assert_eq!(usages[1].tags[0].tag_name, "from_file");
        assert_eq!(
            usages[1].tags[0].kwargs,
            vec![
                ("go_mod".to_owned(), "\"//:go.mod\"".to_owned()),
                ("name".to_owned(), "\"go_default_sdk\"".to_owned()),
            ]
        );
        assert_eq!(usages[1].imports.len(), 2);
        assert_eq!(usages[1].imports[0].alias, "go_toolchains");
        assert_eq!(usages[1].imports[0].repo_name, "go_toolchains");
        assert_eq!(usages[1].imports[1].alias, "alias_name");
        assert_eq!(usages[1].imports[1].repo_name, "actual_repo");
        assert_eq!(
            super::bzlmod_extension_tag_repo_names(&usages[1]),
            vec!["go_default_sdk".to_owned()]
        );
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
