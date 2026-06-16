use super::*;

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
pub(crate) struct BzlmodPatchConfig {
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) integrity: String,
    #[serde(default)]
    pub(crate) path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_sha256: Option<String>,
    #[serde(default)]
    pub(crate) patch_strip: Option<u32>,
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
pub(crate) struct BzlmodOverlayConfig {
    pub(crate) path: String,
    pub(crate) url: String,
    pub(crate) integrity: String,
}

#[derive(Default, Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub(crate) struct BazelModuleCellAliases {
    pub(crate) root_aliases: Vec<BazelCompatCellAlias>,
    pub(crate) cell_aliases: BTreeMap<String, Vec<BazelCompatCellAlias>>,
    pub(crate) external_modules: Vec<BazelCompatExternalModule>,
    pub(crate) registered_toolchains: Vec<String>,
}

impl BazelModuleCellAliases {
    pub(crate) fn dice_config_equal(&self, other: &Self) -> bool {
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

    pub(crate) fn normalize(&mut self) {
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

    pub(crate) fn register_for_starlark_label_resolution(&self, root_cell_name: &str) {
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

    pub(crate) fn register_external_cell_origins(&self) -> bz_error::Result<()> {
        register_bzlmod_cell_canonical_repo_name_for_cell("bazel_tools", "bazel_tools");
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

pub(crate) fn parse_bzlmod_external_cell_origin(
    cell: &CellName,
    value: &str,
    config: &LegacyBuckConfig,
) -> bz_error::Result<Option<ExternalCellOrigin>> {
    #[derive(bz_error::Error, Debug)]
    #[buck2(tag = Input)]
    enum BzlmodExternalCellOriginParseError {
        #[error("Missing buckconfig `{0}.{1}` for external cell configuration")]
        MissingConfiguration(String, String),
    }

    let get_config = |section: &str, property: &str| {
        config
            .get(BuckconfigKeyRef { section, property })
            .ok_or_else(|| {
                BzlmodExternalCellOriginParseError::MissingConfiguration(
                    section.to_owned(),
                    property.to_owned(),
                )
            })
    };

    if value == BZLMOD_EXTERNAL_CELL_KIND {
        let section = &format!("external_cell_{}", cell.as_str());
        let patches: Vec<BzlmodPatchConfig> = serde_json::from_str(get_config(section, "patches")?)
            .buck_error_context("Invalid bzlmod patch configuration")?;
        let overlays: Vec<BzlmodOverlayConfig> = serde_json::from_str(
            config
                .get(BuckconfigKeyRef {
                    section,
                    property: "overlays",
                })
                .unwrap_or("[]"),
        )
        .buck_error_context("Invalid bzlmod overlay configuration")?;
        let module_patch_strip = get_config(section, "patch_strip")?.parse()?;
        let url = get_config(section, "url")?;
        let urls = config
            .get(BuckconfigKeyRef {
                section,
                property: "urls",
            })
            .map(|urls| serde_json::from_str::<Vec<String>>(urls))
            .transpose()
            .buck_error_context("Invalid bzlmod URL configuration")?
            .unwrap_or_else(|| vec![url.to_owned()]);

        Ok(Some(ExternalCellOrigin::Bzlmod(BzlmodCellSetup {
            module_name: get_config(section, "module_name")?.into(),
            version: get_config(section, "version")?.into(),
            canonical_repo_name: get_config(section, "canonical_repo_name")?.into(),
            local_path: config
                .get(BuckconfigKeyRef {
                    section,
                    property: "local_path",
                })
                .map(Arc::from),
            url: Arc::from(url),
            urls: Arc::new(urls.into_iter().map(Arc::from).collect()),
            integrity: get_config(section, "integrity")?.into(),
            strip_prefix: config
                .get(BuckconfigKeyRef {
                    section,
                    property: "strip_prefix",
                })
                .map(Arc::from),
            archive_type: config
                .get(BuckconfigKeyRef {
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
        })))
    } else if value == BZLMOD_GENERATED_EXTERNAL_CELL_KIND {
        let section = &format!("external_cell_{}", cell.as_str());
        let generator: BzlmodGeneratedCellGenerator =
            serde_json::from_str(get_config(section, "generator")?)
                .buck_error_context("Invalid generated bzlmod repo configuration")?;
        Ok(Some(ExternalCellOrigin::BzlmodGenerated(
            BzlmodGeneratedCellSetup {
                canonical_repo_name: get_config(section, "canonical_repo_name")?.into(),
                generator,
            },
        )))
    } else {
        Ok(None)
    }
}

pub(crate) fn dedup_preserve_order<T: Ord + Clone>(values: &mut Vec<T>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

pub(crate) fn bzlmod_external_module_is_local(module: &BazelCompatExternalModule) -> bool {
    match module {
        BazelCompatExternalModule::Registry(module) => module.local_path.is_some(),
        BazelCompatExternalModule::Git(_) => false,
        BazelCompatExternalModule::Generated(_) => false,
    }
}

pub(crate) fn bzlmod_external_module_is_configure_repo(module: &BazelCompatExternalModule) -> bool {
    match module {
        BazelCompatExternalModule::Registry(_) => false,
        BazelCompatExternalModule::Git(_) => false,
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

pub(crate) fn external_cell_origin_from_bazel_module(
    module: &BazelCompatExternalModule,
) -> bz_error::Result<ExternalCellOrigin> {
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
        BazelCompatExternalModule::Git(module) => Ok(ExternalCellOrigin::Git(GitCellSetup {
            git_origin: Arc::from(module.git_origin.as_str()),
            commit: Arc::from(module.commit_hash.as_str()),
            object_format: None,
        })),
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
