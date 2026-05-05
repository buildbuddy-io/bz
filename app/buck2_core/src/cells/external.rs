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
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use allocative::Allocative;
use buck2_error::buck2_error;
use derive_more::Display;
use dupe::Dupe;
use once_cell::sync::Lazy;
use pagable::Pagable;

use crate::cells::name::CellName;

pub const BZLMOD_BAZEL_COMPAT_VERSION: &str = "9.1.0";

pub fn bzlmod_cell_name(canonical_repo_name: &str) -> String {
    let mut cell = String::from("bzlmod_");
    for ch in canonical_repo_name.chars() {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            cell.push(ch);
        } else {
            cell.push('_');
        }
    }
    cell
}

static BZLMOD_CANONICAL_REPO_NAMES: Lazy<Mutex<BTreeMap<String, String>>> =
    Lazy::new(|| Mutex::new(BTreeMap::new()));

static BZLMOD_CELL_ALIASES: Lazy<Mutex<BTreeMap<String, Vec<(String, String)>>>> =
    Lazy::new(|| Mutex::new(BTreeMap::new()));

pub fn register_bzlmod_cell_canonical_repo_name(canonical_repo_name: &str) {
    BZLMOD_CANONICAL_REPO_NAMES
        .lock()
        .expect("bzlmod canonical repo map poisoned")
        .insert(
            bzlmod_cell_name(canonical_repo_name),
            canonical_repo_name.to_owned(),
        );
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

pub fn bzlmod_cell_aliases_for_cell(cell_name: &str) -> Vec<(String, String)> {
    BZLMOD_CELL_ALIASES
        .lock()
        .expect("bzlmod cell alias map poisoned")
        .get(cell_name)
        .cloned()
        .unwrap_or_default()
}

#[derive(Debug, Clone, Dupe, Allocative, PartialEq, Eq, Pagable)]
pub enum ExternalCellOrigin {
    Bundled(CellName),
    Git(GitCellSetup),
    Bzlmod(BzlmodCellSetup),
    BzlmodGenerated(BzlmodGeneratedCellSetup),
}

static EXTERNAL_CELL_ORIGINS: Lazy<Mutex<BTreeMap<String, ExternalCellOrigin>>> =
    Lazy::new(|| Mutex::new(BTreeMap::new()));

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
    pub url: Arc<str>,
    pub integrity: Arc<str>,
    pub strip_prefix: Option<Arc<str>>,
    pub archive_type: Option<Arc<str>>,
    pub patches: Arc<Vec<BzlmodPatch>>,
    pub patch_strip: u32,
}

#[derive(Debug, Clone, allocative::Allocative, PartialEq, Eq, Hash, Pagable)]
pub struct BzlmodPatch {
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
    Pagable
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
    Pagable
)]
pub enum BzlmodGeneratedCellGenerator {
    BazelFeaturesGlobals(BzlmodBazelFeaturesGlobalsSetup),
    BazelFeaturesVersion(BzlmodBazelFeaturesVersionSetup),
    HostPlatform(BzlmodHostPlatformSetup),
    LocalConfigPlatform(BzlmodLocalConfigPlatformSetup),
    CcAutoconfToolchains(BzlmodCcAutoconfToolchainsSetup),
    CcAutoconf(BzlmodCcAutoconfSetup),
    ShellConfig(BzlmodShellConfigSetup),
    HttpArchive(BzlmodHttpArchiveSetup),
    JavaLocalJdk(BzlmodJavaLocalJdkSetup),
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
    Pagable
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
    Pagable
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
    Pagable
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
    Pagable
)]
pub struct BzlmodLocalConfigPlatformSetup {}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable
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
    Pagable
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
    Pagable
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
    Pagable
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
    Pagable
)]
pub struct BzlmodJavaLocalJdkSetup {}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable
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
    Pagable
)]
pub struct BzlmodRepositoryRuleSetup {
    pub files_json: Arc<str>,
    pub source_dir: Option<Arc<str>>,
}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable
)]
pub struct BzlmodRepositoryRuleInvocationSetup {
    pub repo_name: Arc<str>,
    pub rule_bzl_cell: Arc<str>,
    pub rule_bzl_path: Arc<str>,
    pub rule_bzl_build_file_cell: Arc<str>,
    pub rule_name: Arc<str>,
    pub attrs: Arc<Vec<(Arc<str>, Arc<str>)>>,
    pub label_deps: Arc<Vec<Arc<str>>>,
}

#[derive(
    Debug,
    Clone,
    Dupe,
    allocative::Allocative,
    PartialEq,
    Eq,
    Hash,
    Pagable
)]
pub struct BzlmodModuleExtensionRepoSetup {
    pub parent_canonical_repo_name: Arc<str>,
    pub parent_is_root: bool,
    pub extension_bzl_file: Arc<str>,
    pub extension_bzl_cell: Arc<str>,
    pub extension_bzl_path: Arc<str>,
    pub extension_name: Arc<str>,
    pub repo_name: Arc<str>,
    pub extension_usages_json: Arc<str>,
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
