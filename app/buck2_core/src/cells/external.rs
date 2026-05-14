/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use allocative::Allocative;
use buck2_error::buck2_error;
use buck2_hash::StdBuckHashMap;
use derive_more::Display;
use dupe::Dupe;
use once_cell::sync::Lazy;
use pagable::Pagable;
use serde::Deserialize;
use serde::Serialize;

use crate::cells::name::CellName;

pub const BZLMOD_BAZEL_COMPAT_VERSION: &str = "9.1.0";
pub const EXTERNAL_CELLS_ROOT: &str = "buck-out/v2/external_cells";
pub const BZLMOD_EXTERNAL_CELL_KIND: &str = "bzlmod";
pub const BZLMOD_GENERATED_EXTERNAL_CELL_KIND: &str = "bzlmod_generated";
pub const BZLMOD_GENERATED_EXTERNAL_CELL_PATH_MARKER: &str = "/bzlmod_generated/";
pub const BAZEL_REPOSITORY_ACCEPT_ENCODING_HEADER: &str = "Accept-Encoding";
pub const BAZEL_REPOSITORY_ACCEPT_ENCODING: &str = "gzip";
pub const BAZEL_REPOSITORY_USER_AGENT_HEADER: &str = "User-Agent";

pub fn bazel_repository_user_agent() -> String {
    format!("Bazel/release {BZLMOD_BAZEL_COMPAT_VERSION}")
}

pub fn external_cell_source_path(kind: &str, canonical_repo_name: &str) -> String {
    format!("{EXTERNAL_CELLS_ROOT}/{kind}/{canonical_repo_name}")
}

pub fn bzlmod_cell_name(canonical_repo_name: &str) -> String {
    let mut cell = String::with_capacity("bzlmod_".len() + canonical_repo_name.len());
    cell.push_str("bzlmod_");
    for ch in canonical_repo_name.bytes() {
        if ch == b'_' || ch.is_ascii_alphanumeric() {
            cell.push(ch as char);
        } else {
            cell.push('_');
        }
    }
    cell
}

static BZLMOD_CANONICAL_REPO_NAMES: Lazy<Mutex<StdBuckHashMap<String, String>>> =
    Lazy::new(|| Mutex::new(StdBuckHashMap::default()));

static BZLMOD_CELL_ALIASES: Lazy<Mutex<StdBuckHashMap<String, Vec<(String, String)>>>> =
    Lazy::new(|| Mutex::new(StdBuckHashMap::default()));

pub fn register_bzlmod_cell_canonical_repo_name(canonical_repo_name: &str) {
    let cell_name = bzlmod_cell_name(canonical_repo_name);
    register_bzlmod_cell_canonical_repo_name_for_cell(&cell_name, canonical_repo_name);
}

pub fn register_bzlmod_cell_canonical_repo_name_for_cell(
    cell_name: &str,
    canonical_repo_name: &str,
) {
    let mut names = BZLMOD_CANONICAL_REPO_NAMES
        .lock()
        .expect("bzlmod canonical repo map poisoned");
    if matches!(
        names.get(cell_name),
        Some(existing) if existing == canonical_repo_name
    ) {
        return;
    }
    names.insert(cell_name.to_owned(), canonical_repo_name.to_owned());
}

pub fn bzlmod_canonical_repo_name_for_cell(cell_name: &str) -> Option<String> {
    BZLMOD_CANONICAL_REPO_NAMES
        .lock()
        .expect("bzlmod canonical repo map poisoned")
        .get(cell_name)
        .cloned()
}

pub fn register_bzlmod_cell_aliases(
    cell_name: &str,
    aliases: impl IntoIterator<Item = (String, String)>,
) {
    let aliases = aliases.into_iter().collect();
    BZLMOD_CELL_ALIASES
        .lock()
        .expect("bzlmod cell alias map poisoned")
        .insert(cell_name.to_owned(), aliases);
}

pub fn register_bzlmod_cell_aliases_from_refs<'a, I>(cell_name: &str, aliases: I)
where
    I: Clone + IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut aliases_by_cell = BZLMOD_CELL_ALIASES
        .lock()
        .expect("bzlmod cell alias map poisoned");
    if let Some(existing) = aliases_by_cell.get(cell_name)
        && bzlmod_cell_aliases_equal(existing, aliases.clone())
    {
        return;
    }
    aliases_by_cell.insert(
        cell_name.to_owned(),
        aliases
            .into_iter()
            .map(|(alias, cell_name)| (alias.to_owned(), cell_name.to_owned()))
            .collect(),
    );
}

fn bzlmod_cell_aliases_equal<'a, I>(existing: &[(String, String)], aliases: I) -> bool
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut aliases = aliases.into_iter();
    for (existing_alias, existing_cell_name) in existing {
        let Some((alias, cell_name)) = aliases.next() else {
            return false;
        };
        if existing_alias != alias || existing_cell_name != cell_name {
            return false;
        }
    }
    aliases.next().is_none()
}

pub fn bzlmod_cell_aliases_for_cell(cell_name: &str) -> Vec<(String, String)> {
    BZLMOD_CELL_ALIASES
        .lock()
        .expect("bzlmod cell alias map poisoned")
        .get(cell_name)
        .cloned()
        .unwrap_or_default()
}

pub fn bzlmod_all_cell_aliases() -> Vec<(String, Vec<(String, String)>)> {
    let mut aliases = BZLMOD_CELL_ALIASES
        .lock()
        .expect("bzlmod cell alias map poisoned")
        .iter()
        .map(|(cell_name, aliases)| (cell_name.clone(), aliases.clone()))
        .collect::<Vec<_>>();
    aliases.sort_by(|(a, _), (b, _)| a.cmp(b));
    aliases
}

#[derive(Debug, Clone, Dupe, Allocative, PartialEq, Eq, Pagable)]
pub enum ExternalCellOrigin {
    Bundled(CellName),
    Git(GitCellSetup),
    Bzlmod(BzlmodCellSetup),
    BzlmodGenerated(BzlmodGeneratedCellSetup),
}

static EXTERNAL_CELL_ORIGINS: Lazy<Mutex<StdBuckHashMap<String, ExternalCellOrigin>>> =
    Lazy::new(|| Mutex::new(StdBuckHashMap::default()));

pub fn register_external_cell_origin(cell_name: CellName, origin: ExternalCellOrigin) {
    EXTERNAL_CELL_ORIGINS
        .lock()
        .expect("external cell origin map poisoned")
        .insert(cell_name.as_str().to_owned(), origin);
}

pub fn external_cell_origin_for_cell(cell_name: &str) -> Option<ExternalCellOrigin> {
    EXTERNAL_CELL_ORIGINS
        .lock()
        .expect("external cell origin map poisoned")
        .get(cell_name)
        .cloned()
}

#[derive(
    Debug,
    derive_more::Display,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable
)]
#[display("git({}, {})", git_origin, commit)]
pub struct GitCellSetup {
    pub git_origin: Arc<str>,
    // Guaranteed to be a valid commit hash
    pub commit: Arc<str>,
    pub object_format: Option<GitObjectFormat>,
}

#[derive(
    Debug,
    derive_more::Display,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable
)]
#[display("bzlmod({}@{})", module_name, version)]
pub struct BzlmodCellSetup {
    pub module_name: Arc<str>,
    pub version: Arc<str>,
    pub canonical_repo_name: Arc<str>,
    pub local_path: Option<Arc<str>>,
    pub url: Arc<str>,
    pub urls: Arc<Vec<Arc<str>>>,
    pub integrity: Arc<str>,
    pub strip_prefix: Option<Arc<str>>,
    pub archive_type: Option<Arc<str>>,
    pub patches: Arc<Vec<BzlmodPatch>>,
    pub overlays: Arc<Vec<BzlmodOverlay>>,
    pub patch_strip: u32,
}

#[derive(Debug, Clone, allocative::Allocative, PartialEq, Eq, Hash, Pagable)]
pub struct BzlmodPatch {
    pub url: Arc<str>,
    pub integrity: Arc<str>,
    pub path: Option<Arc<str>>,
    pub content_sha256: Option<Arc<str>>,
    pub patch_strip: u32,
}

#[derive(Debug, Clone, allocative::Allocative, PartialEq, Eq, Hash, Pagable)]
pub struct BzlmodOverlay {
    pub path: Arc<str>,
    pub url: Arc<str>,
    pub integrity: Arc<str>,
}

#[derive(
    Debug,
    derive_more::Display,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
#[display("bzlmod-generated({})", canonical_repo_name)]
pub struct BzlmodGeneratedCellSetup {
    pub canonical_repo_name: Arc<str>,
    pub generator: BzlmodGeneratedCellGenerator,
}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BzlmodGeneratedCellGenerator {
    BazelFeaturesGlobals(BzlmodBazelFeaturesGlobalsSetup),
    BazelFeaturesVersion(BzlmodBazelFeaturesVersionSetup),
    HostPlatform(BzlmodHostPlatformSetup),
    CcAutoconfToolchains(BzlmodCcAutoconfToolchainsSetup),
    CcAutoconf(BzlmodCcAutoconfSetup),
    ShellConfig(BzlmodShellConfigSetup),
    HttpArchive(BzlmodHttpArchiveSetup),
    PythonHub(BzlmodPythonHubSetup),
    RepositoryRule(BzlmodRepositoryRuleSetup),
    RepositoryRuleInvocation(BzlmodRepositoryRuleInvocationSetup),
    ModuleExtensionRepo(BzlmodModuleExtensionRepoSetup),
}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodBazelFeaturesGlobalsSetup {
    pub parent_canonical_repo_name: Arc<str>,
    pub bazel_version: Arc<str>,
}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodBazelFeaturesVersionSetup {
    pub bazel_version: Arc<str>,
}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodHostPlatformSetup {}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Deserialize,
    Serialize
)]
pub struct BzlmodCcAutoconfToolchainsSetup {
    pub parent_canonical_repo_name: Arc<str>,
}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodCcAutoconfSetup {}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodShellConfigSetup {}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodHttpArchiveSetup {
    pub repo_name: Arc<str>,
    pub url: Arc<str>,
    pub sha256: Arc<str>,
    pub strip_prefix: Option<Arc<str>>,
    pub archive_type: Option<Arc<str>>,
}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodPythonHubSetup {}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodRepositoryRuleSetup {
    pub files: Arc<Vec<BzlmodRepositoryRuleFile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_dir: Option<Arc<str>>,
}

#[derive(
    Debug,
    Clone,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodRepositoryRuleFile {
    pub path: Arc<str>,
    pub content: Arc<str>,
    pub executable: bool,
}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable,
    Serialize,
    Deserialize
)]
pub struct BzlmodRepositoryRuleInvocationSetup {
    pub repo_name: Arc<str>,
    pub rule_bzl_cell: Arc<str>,
    pub rule_bzl_path: Arc<str>,
    pub rule_bzl_build_file_cell: Arc<str>,
    pub rule_name: Arc<str>,
    pub attrs: Arc<Vec<(Arc<str>, Arc<str>)>>,
}

#[derive(Debug, Clone, Dupe, allocative::Allocative, Pagable, Serialize)]
pub struct BzlmodModuleExtensionRepoSetup {
    pub parent_canonical_repo_name: Arc<str>,
    pub parent_is_root: bool,
    pub extension_bzl_file: Arc<str>,
    pub extension_bzl_cell: Arc<str>,
    pub extension_bzl_path: Arc<str>,
    pub extension_name: Arc<str>,
    pub repo_name: Arc<str>,
    #[serde(skip_serializing)]
    pub extension_usages_key: Arc<str>,
    pub extension_usages_json: Arc<str>,
}

impl BzlmodModuleExtensionRepoSetup {
    pub fn extension_usages_key_from_json(extension_usages_json: &str) -> String {
        blake3::hash(extension_usages_json.as_bytes())
            .to_hex()
            .to_string()
    }
}

impl<'de> Deserialize<'de> for BzlmodModuleExtensionRepoSetup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct BzlmodModuleExtensionRepoSetupConfig {
            parent_canonical_repo_name: Arc<str>,
            #[serde(default)]
            parent_is_root: bool,
            extension_bzl_file: Arc<str>,
            extension_bzl_cell: Arc<str>,
            extension_bzl_path: Arc<str>,
            extension_name: Arc<str>,
            repo_name: Arc<str>,
            extension_usages_json: Arc<str>,
        }

        let config = BzlmodModuleExtensionRepoSetupConfig::deserialize(deserializer)?;
        let extension_usages_key =
            Self::extension_usages_key_from_json(&config.extension_usages_json);
        Ok(Self {
            parent_canonical_repo_name: config.parent_canonical_repo_name,
            parent_is_root: config.parent_is_root,
            extension_bzl_file: config.extension_bzl_file,
            extension_bzl_cell: config.extension_bzl_cell,
            extension_bzl_path: config.extension_bzl_path,
            extension_name: config.extension_name,
            repo_name: config.repo_name,
            extension_usages_key: Arc::from(extension_usages_key),
            extension_usages_json: config.extension_usages_json,
        })
    }
}

impl PartialEq for BzlmodModuleExtensionRepoSetup {
    fn eq(&self, other: &Self) -> bool {
        self.parent_canonical_repo_name == other.parent_canonical_repo_name
            && self.parent_is_root == other.parent_is_root
            && self.extension_bzl_file == other.extension_bzl_file
            && self.extension_bzl_cell == other.extension_bzl_cell
            && self.extension_bzl_path == other.extension_bzl_path
            && self.extension_name == other.extension_name
            && self.repo_name == other.repo_name
            && self.extension_usages_key == other.extension_usages_key
    }
}

impl Eq for BzlmodModuleExtensionRepoSetup {}

impl Hash for BzlmodModuleExtensionRepoSetup {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parent_canonical_repo_name.hash(state);
        self.parent_is_root.hash(state);
        self.extension_bzl_file.hash(state);
        self.extension_bzl_cell.hash(state);
        self.extension_bzl_path.hash(state);
        self.extension_name.hash(state);
        self.repo_name.hash(state);
        self.extension_usages_key.hash(state);
    }
}

impl fmt::Display for ExternalCellOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bundled(cell) => write!(f, "bundled({cell})"),
            Self::Git(git) => write!(f, "{git}"),
            Self::Bzlmod(bzlmod) => write!(f, "{bzlmod}"),
            Self::BzlmodGenerated(generated) => write!(f, "{generated}"),
        }
    }
}

#[derive(Debug, Display, Eq, PartialEq, Clone, Dupe, Hash, Allocative, Pagable)]
pub enum GitObjectFormat {
    #[display("sha1")]
    Sha1,
    #[display("sha256")]
    Sha256,
}

impl FromStr for GitObjectFormat {
    type Err = buck2_error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sha1" => Ok(GitObjectFormat::Sha1),
            "sha256" => Ok(GitObjectFormat::Sha256),
            _ => Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "object_format must be one of `sha1` or `sha256` (got: {})",
                &s,
            )),
        }
    }
}

impl GitObjectFormat {
    pub fn check(&self, s: &str) -> Result<(), buck2_error::Error> {
        match self {
            Self::Sha1 => {
                if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                    Ok(())
                } else {
                    Err(buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "not a valid SHA1 digest (got: {})",
                        &s,
                    ))
                }
            }
            Self::Sha256 => {
                if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                    Ok(())
                } else {
                    Err(buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "not a valid SHA256 digest (got: {})",
                        &s,
                    ))
                }
            }
        }
    }
}
