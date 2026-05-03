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
use std::str::FromStr;
use std::sync::Arc;

use allocative::Allocative;
use buck2_error::buck2_error;
use derive_more::Display;
use dupe::Dupe;
use pagable::Pagable;

use crate::cells::name::CellName;

#[derive(Debug, Clone, Dupe, Allocative, PartialEq, Eq, Pagable)]
pub enum ExternalCellOrigin {
    Bundled(CellName),
    Git(GitCellSetup),
    Bzlmod(BzlmodCellSetup),
    BzlmodGenerated(BzlmodGeneratedCellSetup),
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
    GoRegisterNogo(BzlmodGoRegisterNogoSetup),
    GoDepsModule(BzlmodGoDepsModuleSetup),
    GoDepsRepositoryConfig(BzlmodGoDepsRepositoryConfigSetup),
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
pub struct BzlmodGoRegisterNogoSetup {
    pub nogo: Arc<str>,
    pub includes: Arc<Vec<Arc<str>>>,
    pub excludes: Arc<Vec<Arc<str>>>,
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
pub struct BzlmodGoDepsModuleSetup {
    pub parent_canonical_repo_name: Arc<str>,
    pub go_mod: Arc<str>,
    pub repo_name: Arc<str>,
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
pub struct BzlmodGoDepsRepositoryConfigSetup {
    pub go_env_json: Arc<str>,
    pub deps_files: Arc<Vec<Arc<str>>>,
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
