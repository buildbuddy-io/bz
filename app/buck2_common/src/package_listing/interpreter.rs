/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use async_trait::async_trait;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::cell_path::CellPathRef;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePath;
use buck2_core::package::PackageLabel;
use buck2_core::package::package_relative_path::PackageRelativePath;
use buck2_core::package::package_relative_path::PackageRelativePathBuf;
use buck2_fs::paths::file_name::FileNameBuf;
use buck2_util::arc_str::ArcS;
use dice::DiceComputations;
use dupe::Dupe;
use futures::FutureExt;
use futures::future::BoxFuture;
use starlark_map::sorted_set::SortedSet;
use starlark_map::sorted_vec::SortedVec;

use crate::dice::cells::HasCellResolver;
use crate::file_ops::dice::DiceFileComputations;
use crate::find_buildfile::find_buildfile;
use crate::ignores::file_ignores::FileIgnoreReason;
use crate::io::DirectoryDoesNotExistSuggestion;
use crate::io::ReadDirError;
use crate::legacy_configs::dice::HasLegacyConfigs;
use crate::legacy_configs::dice::OpaqueLegacyBuckConfigOnDice;
use crate::legacy_configs::key::BuckconfigKeyRef;
use crate::package_listing::listing::PackageListing;
use crate::package_listing::resolver::PackageListingResolver;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum PackageListingError {
    #[error("Expected `{0}` to be within a package directory, but there was no buildfile in any parent directories. Expected one of `{}`", .1.join("`, `"))]
    NoContainingPackage(CellPath, Vec<FileNameBuf>),
}

#[async_trait]
impl PackageListingResolver for InterpreterPackageListingResolver<'_, '_> {
    async fn resolve(&mut self, package: PackageLabel) -> buck2_error::Result<PackageListing> {
        Ok(self.gather_package_listing(package.dupe()).await?)
    }

    async fn get_enclosing_package(
        &mut self,
        path: CellPathRef<'async_trait>,
    ) -> buck2_error::Result<PackageLabel> {
        let buildfile_candidates = DiceFileComputations::buildfiles(self.ctx, path.cell()).await?;
        if let Some(path) = path.parent() {
            for path in path.ancestors() {
                let listing = DiceFileComputations::read_dir(self.ctx, path)
                    .await?
                    .included;
                if find_buildfile(&buildfile_candidates, &listing).is_some() {
                    return PackageLabel::from_cell_path(path);
                }
            }
        }
        Err(PackageListingError::NoContainingPackage(
            path.to_owned(),
            buildfile_candidates.to_vec(),
        )
        .into())
    }

    async fn get_enclosing_packages(
        &mut self,
        path: CellPathRef<'async_trait>,
        enclosing_path: CellPathRef<'async_trait>,
    ) -> buck2_error::Result<Vec<PackageLabel>> {
        let buildfile_candidates = DiceFileComputations::buildfiles(self.ctx, path.cell()).await?;
        if let Some(path) = path.parent() {
            let mut packages = Vec::new();
            for path in path.ancestors() {
                if !path.starts_with(enclosing_path.dupe()) {
                    // stop when we are no longer within the enclosing path
                    break;
                }
                let listing = DiceFileComputations::read_dir(self.ctx, path.dupe())
                    .await?
                    .included;
                if find_buildfile(&buildfile_candidates, &listing).is_some() {
                    packages.push(PackageLabel::from_cell_path(path)?);
                }
            }
            Ok(packages)
        } else {
            Err(PackageListingError::NoContainingPackage(
                path.to_owned(),
                buildfile_candidates.to_vec(),
            )
            .into())
        }
    }
}

pub struct InterpreterPackageListingResolver<'c, 'd> {
    ctx: &'c mut DiceComputations<'d>,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Hash,
    allocative::Allocative,
    pagable::Pagable
)]
pub enum PackageListingStrategy {
    Recursive,
    Shallow,
    Selective(Vec<PackageRelativePathBuf>),
}

impl PackageListingStrategy {
    pub fn selective(mut prefixes: Vec<PackageRelativePathBuf>) -> Self {
        if prefixes.iter().any(|prefix| prefix.is_empty()) {
            return Self::Recursive;
        }
        prefixes.sort();
        prefixes.dedup();
        if prefixes.is_empty() {
            Self::Shallow
        } else {
            Self::Selective(prefixes)
        }
    }

    fn should_recurse_into(&self, child: &PackageRelativePath) -> bool {
        match self {
            Self::Recursive => true,
            Self::Shallow => false,
            Self::Selective(prefixes) => prefixes.iter().any(|prefix| {
                let prefix: &PackageRelativePath = prefix.as_ref();
                prefix.starts_with(child) || child.starts_with(prefix)
            }),
        }
    }

    pub fn covers(&self, required: &Self) -> bool {
        match (self, required) {
            (Self::Recursive, _) => true,
            (_, Self::Shallow) => true,
            (Self::Shallow, _) => false,
            (Self::Selective(_), Self::Recursive) => false,
            (Self::Selective(available), Self::Selective(required)) => {
                required.iter().all(|required_prefix| {
                    available.iter().any(|available_prefix| {
                        let available_prefix: &PackageRelativePath = available_prefix.as_ref();
                        let required_prefix: &PackageRelativePath = required_prefix.as_ref();
                        required_prefix.starts_with(available_prefix)
                    })
                })
            }
        }
    }

    pub fn union(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Recursive, _) | (_, Self::Recursive) => Self::Recursive,
            (Self::Shallow, strategy) | (strategy, Self::Shallow) => strategy.clone(),
            (Self::Selective(left), Self::Selective(right)) => {
                let mut prefixes = left.clone();
                prefixes.extend(right.iter().cloned());
                Self::selective(prefixes)
            }
        }
    }
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
pub enum GatherPackageListingError {
    #[buck2(input)]
    NoBuildFile {
        package: CellPath,
        candidates: Vec<FileNameBuf>,
    },
    #[buck2(input)]
    DirectoryDoesNotExist {
        package: CellPath,
        expected_path: CellPath,
        suggestion: DirectoryDoesNotExistSuggestion,
    },
    #[buck2(input)]
    DirectoryIsIgnored {
        package: CellPath,
        path: CellPath,
        ignore_reason: FileIgnoreReason,
    },
    #[buck2(input)]
    NotADirectory {
        package: CellPath,
        path: CellPath,
        node_type: String,
    },
    Error {
        package: CellPath,
        #[source]
        error: buck2_error::Error,
    },
}

impl GatherPackageListingError {
    fn error<E: Into<buck2_error::Error>>(
        package_path: CellPathRef<'_>,
        err: E,
    ) -> GatherPackageListingError {
        GatherPackageListingError::Error {
            package: package_path.to_owned(),
            error: err.into(),
        }
    }

    fn from_read_dir(
        package_path: CellPathRef<'_>,
        err: ReadDirError,
    ) -> GatherPackageListingError {
        match err {
            ReadDirError::DirectoryDoesNotExist { path, suggestion } => {
                GatherPackageListingError::DirectoryDoesNotExist {
                    package: package_path.to_owned(),
                    expected_path: path,
                    suggestion,
                }
            }
            ReadDirError::DirectoryIsIgnored(path, ignore_reason) => {
                GatherPackageListingError::DirectoryIsIgnored {
                    package: package_path.to_owned(),
                    path,
                    ignore_reason,
                }
            }
            ReadDirError::NotADirectory(path, node_type) => {
                GatherPackageListingError::NotADirectory {
                    package: package_path.to_owned(),
                    path,
                    node_type,
                }
            }
            ReadDirError::Error(e) => GatherPackageListingError::Error {
                package: package_path.to_owned(),
                error: e,
            },
        }
    }

    fn no_build_file(
        package_path: CellPathRef<'_>,
        candidates: Vec<FileNameBuf>,
    ) -> GatherPackageListingError {
        GatherPackageListingError::NoBuildFile {
            package: package_path.to_owned(),
            candidates,
        }
    }
}

impl std::fmt::Display for GatherPackageListingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /*
         package `fbsource//foo/target/x/y/lmnop:` does not exist
                  ^--------------------^
             dir `fbsource//foo/target/x` does not exist

         package `fbsource//foo/target/x/y/lmnop:` does not exist
                  ^--------------------^
             dir `fbsource//foo/target/x` is ignored (config project.ignore contains `foo/target/ **`)

         package `fbsource//fbcode/target/x/y/lmnop:` does not exist
                  ^--------------^
             this package is using the wrong cell, use `fbcode//target/x/y/lmnop:` instead


         package `fbsource//foo/target/x/y/lmnop:` does not exist
                  ^--------------------^
            path `fbsource//foo/target/x` is a file, not a directory

         package `fbsource//foo/target/x/y/lmnop:` does not exist
             missing `TARGETS` file (also missing alternatives `TARGETS.v2`, `BUCK`, `BUCK.v2`)

         error loading package `fbsource//foo/target/x/y/lmnop:`
              ... # just display the buck2_error for now
        */

        let prefix = "package `";
        let underlined = |path_as_string: &str| {
            format!(
                "{}^{}^",
                " ".repeat(prefix.len()),
                "-".repeat(path_as_string.len().saturating_sub(2))
            )
        };

        let (package, submessage) = match self {
            GatherPackageListingError::Error { package, .. } => {
                // in this case we return the buck2_error as our source and we're just displayed as context
                write!(f, "gathering package listing for `{}`", &package)?;
                return Ok(());
            }
            GatherPackageListingError::NoBuildFile {
                candidates,
                package,
            } => {
                if let Some(primary_candidate) =
                    candidates.iter().find(|v| v.extension() != Some("v2"))
                {
                    let alternatives: Vec<_> = candidates
                        .iter()
                        .filter(|v| *v != primary_candidate)
                        .map(|v| format!("`{v}`"))
                        .collect();

                    let message = if alternatives.is_empty() {
                        format!("    missing `{}` file", primary_candidate)
                    } else {
                        format!(
                            "    missing `{}` file (also missing alternatives {})",
                            primary_candidate,
                            alternatives.join(", ")
                        )
                    };

                    (package, message)
                } else {
                    unreachable!()
                }
            }
            GatherPackageListingError::DirectoryDoesNotExist {
                package,
                expected_path,
                suggestion,
            } => {
                let path_as_str = expected_path.to_string();
                let suggestion_msg = match suggestion {
                    DirectoryDoesNotExistSuggestion::Cell(cell_suggestion) => {
                        format!("Did you mean one of [`{}`]?", cell_suggestion.join("`, `"))
                    }
                    DirectoryDoesNotExistSuggestion::Typo(suggestion) => {
                        let suggested_target = match expected_path.parent() {
                            Some(parent) => {
                                if parent.path().is_empty() {
                                    format!("{}//{}", parent.cell(), suggestion)
                                } else {
                                    format!("{}/{}", parent, suggestion)
                                }
                            }
                            None => {
                                format!("{}//{}", expected_path.cell(), suggestion)
                            }
                        };

                        format!("Did you mean `{}`?", suggested_target)
                    }
                    DirectoryDoesNotExistSuggestion::NoSuggestion => "".to_owned(),
                };

                (
                    package,
                    format!(
                        "{}\n    dir `{}` does not exist. {}",
                        underlined(&path_as_str),
                        path_as_str,
                        suggestion_msg
                    ),
                )
            }
            GatherPackageListingError::NotADirectory {
                package,
                path,
                node_type,
            } => {
                let path_as_str = path.to_string();
                (
                    package,
                    format!(
                        "{}\n   path `{}` is a {}, not a directory",
                        underlined(&path_as_str),
                        path_as_str,
                        node_type
                    ),
                )
            }
            GatherPackageListingError::DirectoryIsIgnored {
                package,
                path,
                ignore_reason: FileIgnoreReason::IgnoredByPattern { pattern, .. },
            } => {
                let path_as_str = path.to_string();
                (
                    package,
                    format!(
                        "{}\n    dir `{}` does not exist (project.ignore contains `{}`)",
                        underlined(&path_as_str),
                        path_as_str,
                        &pattern
                    ),
                )
            }
            GatherPackageListingError::DirectoryIsIgnored {
                package,
                path,
                ignore_reason: FileIgnoreReason::IgnoredByCell { cell_name, .. },
            } => {
                let path_as_str = path.to_string();
                let corrected = {
                    match package.strip_prefix(path.as_ref()) {
                        Ok(fixed) => {
                            CellPath::new(*cell_name, CellRelativePath::new(fixed).to_owned())
                                .to_string()
                        }
                        _ => format!("{cell_name}//"),
                    }
                };
                (
                    package,
                    format!(
                        "{}\n    this package is using the wrong cell, use `{}` instead",
                        underlined(&path_as_str),
                        corrected,
                    ),
                )
            }
        };

        writeln!(f, "{prefix}{package}:` does not exist")?;
        f.write_str(&submessage)?;
        Ok(())
    }
}

impl<'c, 'd> InterpreterPackageListingResolver<'c, 'd> {
    pub fn new(ctx: &'c mut DiceComputations<'d>) -> Self {
        Self { ctx }
    }

    pub async fn gather_package_listing(
        &mut self,
        root: PackageLabel,
    ) -> Result<PackageListing, GatherPackageListingError> {
        gather_package_listing_impl(self.ctx, root).await
    }

    pub async fn gather_package_listing_with_strategy(
        &mut self,
        root: PackageLabel,
        strategy: PackageListingStrategy,
    ) -> Result<PackageListing, GatherPackageListingError> {
        gather_package_listing_with_strategy_impl(self.ctx, root, strategy).await
    }
}

struct Directory {
    path: ArcS<PackageRelativePath>,
    files: Vec<ArcS<PackageRelativePath>>,
    subdirs: Vec<Directory>,
    subpackages: Vec<ArcS<PackageRelativePath>>,
    buildfile: Option<FileNameBuf>,

    recursive_files_count: usize,
    recursive_dirs_count: usize,
    recursive_subpackages_count: usize,
}

impl Directory {
    fn shallow(path: PackageRelativePathBuf) -> Self {
        Self {
            path: path.to_arc(),
            files: Vec::new(),
            subdirs: Vec::new(),
            subpackages: Vec::new(),
            buildfile: None,
            recursive_files_count: 0,
            recursive_dirs_count: 0,
            recursive_subpackages_count: 0,
        }
    }

    // Ok(None) indicates that the path is a subpackage
    async fn gather(
        ctx: &mut DiceComputations<'_>,
        buildfile_candidates: &[FileNameBuf],
        root: CellPathRef<'_>,
        path: &PackageRelativePath,
        is_root: bool,
        strategy: &PackageListingStrategy,
    ) -> Result<Option<Directory>, GatherPackageListingError> {
        let cell_path = root.join(path.as_forward_rel_path());
        let entries = DiceFileComputations::read_dir_ext(ctx, cell_path.as_ref())
            .await
            .map_err(|e| GatherPackageListingError::from_read_dir(cell_path.as_ref(), e))?
            .included;
        let buildfile = find_buildfile(buildfile_candidates, &entries);

        match (is_root, buildfile) {
            (true, None) => {
                return Err(GatherPackageListingError::no_build_file(
                    cell_path.as_ref(),
                    buildfile_candidates.to_vec(),
                ));
            }
            (false, Some(_)) => {
                return Ok(None);
            }
            _ => {}
        }

        let mut child_dirs = Vec::new();
        let mut files = Vec::new();

        for d in &*entries {
            let child_path = path.join(&d.file_name);
            if d.file_type.is_dir() {
                child_dirs.push(child_path);
            } else {
                files.push(child_path.to_arc());
            }
        }

        let (subdirs, subpackages) = match strategy {
            PackageListingStrategy::Recursive => {
                Self::gather_subdirs(ctx, buildfile_candidates, root, child_dirs, strategy).await?
            }
            PackageListingStrategy::Shallow => (
                child_dirs.into_iter().map(Self::shallow).collect(),
                Vec::new(),
            ),
            PackageListingStrategy::Selective(_) => {
                let mut recursive_child_dirs = Vec::new();
                let mut shallow_subdirs = Vec::new();
                for child_dir in child_dirs {
                    if strategy.should_recurse_into(&child_dir) {
                        recursive_child_dirs.push(child_dir);
                    } else {
                        shallow_subdirs.push(Self::shallow(child_dir));
                    }
                }
                let (mut subdirs, subpackages) = Self::gather_subdirs(
                    ctx,
                    buildfile_candidates,
                    root,
                    recursive_child_dirs,
                    strategy,
                )
                .await?;
                subdirs.extend(shallow_subdirs);
                (subdirs, subpackages)
            }
        };

        let mut recursive_files_count = files.len();
        let mut recursive_dirs_count = subdirs.len();
        let mut recursive_subpackages_count = subpackages.len();
        for d in &subdirs {
            recursive_files_count += d.recursive_files_count;
            recursive_dirs_count += d.recursive_dirs_count;
            recursive_subpackages_count += d.recursive_subpackages_count;
        }

        Ok(Some(Directory {
            path: path.to_arc(),
            files,
            subdirs,
            subpackages,
            buildfile: buildfile.map(|v| v.to_owned()),
            recursive_files_count,
            recursive_dirs_count,
            recursive_subpackages_count,
        }))
    }

    fn gather_subdirs<'a>(
        ctx: &'a mut DiceComputations<'_>,
        buildfile_candidates: &'a [FileNameBuf],
        root: CellPathRef<'a>,
        subdirs: Vec<PackageRelativePathBuf>,
        strategy: &'a PackageListingStrategy,
    ) -> BoxFuture<
        'a,
        Result<(Vec<Directory>, Vec<ArcS<PackageRelativePath>>), GatherPackageListingError>,
    > {
        async move {
            let mut new_subdirs = Vec::new();
            let mut subpackages = Vec::new();

            for res in ctx
                .compute_join(subdirs, |ctx: &mut DiceComputations, path| {
                    async move {
                        let res = Directory::gather(
                            ctx,
                            buildfile_candidates,
                            root,
                            &path,
                            false,
                            strategy,
                        )
                        .await?;
                        Ok((path, res))
                    }
                    .boxed()
                })
                .await
            {
                let (path, res) = res?;
                match res {
                    Some(v) => new_subdirs.push(v),
                    None => subpackages.push(path.to_arc()),
                }
            }
            Ok((new_subdirs, subpackages))
        }
        .boxed()
    }

    fn collect_into(
        self,
        files: &mut Vec<ArcS<PackageRelativePath>>,
        dirs: &mut Vec<ArcS<PackageRelativePath>>,
        pkgs: &mut Vec<ArcS<PackageRelativePath>>,
    ) {
        files.extend(self.files);
        pkgs.extend(self.subpackages);
        if !self.path.is_empty() {
            dirs.push(self.path);
        }
        for d in self.subdirs {
            d.collect_into(files, dirs, pkgs)
        }
    }

    fn flatten(mut self) -> PackageListing {
        let buildfile = self.buildfile.take().unwrap();
        let mut files = Vec::with_capacity(self.recursive_files_count);
        let mut dirs = Vec::with_capacity(self.recursive_dirs_count);
        let mut subpackages = Vec::with_capacity(self.recursive_subpackages_count);

        self.collect_into(&mut files, &mut dirs, &mut subpackages);

        // The files are discovered in a deterministic order but not necessarily sorted.
        // TODO(cjhopman): Do we require that they be sorted for anything?
        let files = SortedVec::from(files);
        let dirs = SortedVec::from(dirs);
        let subpackages = SortedVec::from(subpackages);

        PackageListing::new(
            SortedSet::from(files),
            SortedSet::from(dirs),
            subpackages,
            buildfile,
        )
    }
}

async fn gather_package_listing_impl(
    ctx: &mut DiceComputations<'_>,
    root: PackageLabel,
) -> Result<PackageListing, GatherPackageListingError> {
    let cell_path = root.as_cell_path();
    let buildfile_candidates = DiceFileComputations::buildfiles(ctx, root.cell_name())
        .await
        .map_err(|e| GatherPackageListingError::error(cell_path, e))?;
    let strategy = package_listing_strategy(ctx, cell_path, &buildfile_candidates).await?;
    gather_package_listing_with_buildfiles(ctx, root, &buildfile_candidates, strategy).await
}

async fn gather_package_listing_with_strategy_impl(
    ctx: &mut DiceComputations<'_>,
    root: PackageLabel,
    strategy: PackageListingStrategy,
) -> Result<PackageListing, GatherPackageListingError> {
    let cell_path = root.as_cell_path();
    let buildfile_candidates = DiceFileComputations::buildfiles(ctx, root.cell_name())
        .await
        .map_err(|e| GatherPackageListingError::error(cell_path, e))?;
    gather_package_listing_with_buildfiles(ctx, root, &buildfile_candidates, strategy).await
}

async fn gather_package_listing_with_buildfiles(
    ctx: &mut DiceComputations<'_>,
    root: PackageLabel,
    buildfile_candidates: &[FileNameBuf],
    strategy: PackageListingStrategy,
) -> Result<PackageListing, GatherPackageListingError> {
    let cell_path = root.as_cell_path();
    Ok(Directory::gather(
        ctx,
        &buildfile_candidates,
        cell_path,
        PackageRelativePath::empty(),
        true,
        &strategy,
    )
    .await?
    .unwrap()
    .flatten())
}

async fn package_listing_strategy(
    ctx: &mut DiceComputations<'_>,
    package: CellPathRef<'_>,
    buildfile_candidates: &[FileNameBuf],
) -> Result<PackageListingStrategy, GatherPackageListingError> {
    if !bazel_compat_package_listing_enabled(ctx, package.cell())
        .await
        .map_err(|e| GatherPackageListingError::error(package, e))?
    {
        return Ok(PackageListingStrategy::Recursive);
    }

    let entries = DiceFileComputations::read_dir_ext(ctx, package)
        .await
        .map_err(|e| GatherPackageListingError::from_read_dir(package, e))?
        .included;
    let Some(buildfile) = find_buildfile(buildfile_candidates, &entries) else {
        return Ok(PackageListingStrategy::Recursive);
    };
    let buildfile_path = package.join(buildfile);
    let contents = DiceFileComputations::read_file_if_exists(ctx, buildfile_path.as_ref())
        .await
        .map_err(|e| GatherPackageListingError::error(package, e))?;

    match contents {
        Some(contents) if !build_file_requires_recursive_listing(&contents) => {
            Ok(PackageListingStrategy::Shallow)
        }
        _ => Ok(PackageListingStrategy::Recursive),
    }
}

pub async fn bazel_compat_package_listing_enabled(
    ctx: &mut DiceComputations<'_>,
    cell_name: CellName,
) -> buck2_error::Result<bool> {
    let cells = ctx.get_cell_resolver().await?;
    let instance = cells.get(cell_name)?;
    if matches!(
        instance.external(),
        Some(ExternalCellOrigin::Bzlmod(_)) | Some(ExternalCellOrigin::BzlmodGenerated(_))
    ) {
        return Ok(true);
    }

    let config = ctx.get_legacy_config_on_dice(cell_name).await?;
    bazel_compat_enabled(ctx, &config)
}

fn bazel_compat_enabled(
    ctx: &mut DiceComputations<'_>,
    config: &OpaqueLegacyBuckConfigOnDice,
) -> buck2_error::Result<bool> {
    let enabled = config.lookup(
        ctx,
        BuckconfigKeyRef {
            section: "bazel",
            property: "compatibility",
        },
    )?;
    Ok(enabled
        .as_deref()
        .map(|value| matches!(value.trim(), "1" | "true" | "True" | "TRUE"))
        .unwrap_or(false))
}

fn build_file_requires_recursive_listing(contents: &str) -> bool {
    // Keep the text fallback aligned with Bazel's BUILD syntax prefetch:
    // direct glob/subpackages calls require package traversal. Loaded macros
    // that actually call glob/sub_packages request a richer listing during
    // BUILD evaluation, matching Bazel's restart model more closely.
    may_contain_starlark_identifier(contents, "glob")
        || may_contain_starlark_identifier(contents, "subpackages")
        || may_contain_starlark_identifier(contents, "sub_packages")
}

fn may_contain_starlark_identifier(contents: &str, needle: &str) -> bool {
    let mut rest = contents;
    while let Some(index) = rest.find(needle) {
        let before = rest[..index].chars().next_back();
        let after = rest[index + needle.len()..].chars().next();
        if !before.is_some_and(is_starlark_identifier_char)
            && !after.is_some_and(is_starlark_identifier_char)
        {
            return true;
        }
        rest = &rest[index + needle.len()..];
    }
    false
}

fn is_starlark_identifier_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::build_file_requires_recursive_listing;

    #[test]
    fn test_build_file_requires_recursive_listing() {
        assert!(build_file_requires_recursive_listing(
            "srcs = glob([\"*.go\"])"
        ));
        assert!(build_file_requires_recursive_listing(
            "g = glob\ng([\"**\"])"
        ));
        assert!(build_file_requires_recursive_listing(
            "native.glob([\"*\"])"
        ));
        assert!(build_file_requires_recursive_listing(
            "subpackages(include = [\"foo/**\"])"
        ));
        assert!(build_file_requires_recursive_listing("sub_packages()"));
        assert!(!build_file_requires_recursive_listing(
            "load(\":defs.bzl\", \"macro\")"
        ));
        assert!(!build_file_requires_recursive_listing(
            "go_library(name = \"globular\")"
        ));
        assert!(!build_file_requires_recursive_listing(
            "my_glob_helper(name = \"x\")"
        ));
        assert!(!build_file_requires_recursive_listing(
            "genrule(name = \"upload\")"
        ));
    }
}
