/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::file_ops::dice::DiceFileComputations;
use buck2_common::file_ops::dice::FollowedPathType;
use buck2_common::file_ops::metadata::SimpleDirEntry;
use buck2_common::find_buildfile::find_buildfile;
use buck2_common::package_listing::PackageListingStrategy;
use buck2_common::package_listing::dice::DicePackageListingResolver;
use buck2_core::cells::cell_path::CellPathRef;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::package::PackageLabel;
use buck2_core::package::package_relative_path::PackageRelativePath;
use buck2_core::package::package_relative_path::PackageRelativePathBuf;
use buck2_error::BuckErrorContext;
use buck2_fs::paths::file_name::FileNameBuf;
use dice::DiceComputations;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use pagable::pagable_typetag;

use crate::interpreter::globspec::GlobSpec;
use crate::interpreter::interpreter_for_dir::package_listing_strategy_from_glob_patterns;

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Hash,
    Ord,
    PartialOrd,
    allocative::Allocative,
    pagable::Pagable
)]
pub(crate) struct BazelGlobRequest {
    pub(crate) include: Vec<String>,
    pub(crate) exclude: Vec<String>,
    pub(crate) include_directories: bool,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Hash,
    Ord,
    PartialOrd,
    allocative::Allocative,
    pagable::Pagable
)]
pub(crate) enum BazelPackageDataRequest {
    Glob(BazelGlobRequest),
    Subpackages,
}

#[derive(
    Clone,
    Debug,
    Eq,
    Hash,
    PartialEq,
    allocative::Allocative,
    pagable::Pagable
)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub(crate) struct BazelPackageDataKey {
    pub(crate) package: PackageLabel,
    pub(crate) request: BazelPackageDataRequest,
}

#[derive(
    Clone,
    Debug,
    Eq,
    Hash,
    PartialEq,
    allocative::Allocative,
    pagable::Pagable
)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BazelPackageDataBatchKey {
    package: PackageLabel,
    requests: BTreeSet<BazelPackageDataRequest>,
}

impl fmt::Display for BazelPackageDataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.request {
            BazelPackageDataRequest::Glob(_) => write!(f, "GLOB({})", self.package),
            BazelPackageDataRequest::Subpackages => write!(f, "GLOBS({})", self.package),
        }
    }
}

impl fmt::Display for BazelPackageDataBatchKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GLOBS({})", self.package)
    }
}

#[async_trait]
impl Key for BazelPackageDataKey {
    type Value = buck2_error::Result<Arc<Vec<String>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        match &self.request {
            BazelPackageDataRequest::Glob(request) => Ok(Arc::new(
                compute_glob(ctx, self.package.dupe(), request).await?,
            )),
            BazelPackageDataRequest::Subpackages => Ok(Arc::new(
                compute_subpackages(ctx, self.package.dupe()).await?,
            )),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

#[async_trait]
impl Key for BazelPackageDataBatchKey {
    type Value = buck2_error::Result<Arc<BTreeMap<BazelPackageDataRequest, Arc<Vec<String>>>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        if should_use_fast_package_listing_for_bazel_package_data(ctx, self.package.dupe()).await? {
            return Ok(Arc::new(
                compute_package_data_from_single_listing(ctx, self.package.dupe(), &self.requests)
                    .await?,
            ));
        }

        let mut results = BTreeMap::new();
        for request in &self.requests {
            let result = match request {
                BazelPackageDataRequest::Glob(request) => {
                    compute_glob(ctx, self.package.dupe(), request).await?
                }
                BazelPackageDataRequest::Subpackages => {
                    compute_subpackages(ctx, self.package.dupe()).await?
                }
            };
            results.insert(request.clone(), Arc::new(result));
        }
        Ok(Arc::new(results))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

pub(crate) async fn compute_bazel_package_data(
    ctx: &mut DiceComputations<'_>,
    package: PackageLabel,
    requests: BTreeSet<BazelPackageDataRequest>,
) -> buck2_error::Result<BTreeMap<BazelPackageDataRequest, Arc<Vec<String>>>> {
    Ok((*ctx
        .compute(&BazelPackageDataBatchKey { package, requests })
        .await??)
        .clone())
}

async fn compute_package_data_from_single_listing(
    ctx: &mut DiceComputations<'_>,
    package: PackageLabel,
    requests: &BTreeSet<BazelPackageDataRequest>,
) -> buck2_error::Result<BTreeMap<BazelPackageDataRequest, Arc<Vec<String>>>> {
    let mut strategy = PackageListingStrategy::Shallow;
    for request in requests {
        strategy = strategy.union(&match request {
            BazelPackageDataRequest::Glob(request) => {
                package_listing_strategy_from_glob_patterns(&request.include)
            }
            BazelPackageDataRequest::Subpackages => PackageListingStrategy::Recursive,
        });
    }
    let listing = DicePackageListingResolver(ctx)
        .resolve_package_listing_with_strategy(package.dupe(), strategy)
        .await?;

    let mut results = BTreeMap::new();
    for request in requests {
        let result = match request {
            BazelPackageDataRequest::Glob(request) => {
                let spec = GlobSpec::new(&request.include, &request.exclude)?;
                let mut result = spec
                    .resolve_glob(listing.files())
                    .map(|path| path.as_str().to_owned())
                    .collect::<Vec<_>>();
                if request.include_directories {
                    result.extend(
                        listing
                            .dirs()
                            .filter(|path| spec.matches(path.as_str()))
                            .map(|path| path.as_str().to_owned()),
                    );
                }
                add_exact_matches_from_file_values(
                    ctx,
                    package.dupe(),
                    &spec,
                    request.include_directories,
                    &mut result,
                )
                .await?;
                result.sort();
                result.dedup();
                result
            }
            BazelPackageDataRequest::Subpackages => listing
                .subpackages_within(PackageRelativePath::empty())
                .map(|path| path.as_str().to_owned())
                .collect(),
        };
        results.insert(request.clone(), Arc::new(result));
    }

    Ok(results)
}

async fn compute_glob(
    ctx: &mut DiceComputations<'_>,
    package: PackageLabel,
    request: &BazelGlobRequest,
) -> buck2_error::Result<Vec<String>> {
    let spec = GlobSpec::new(&request.include, &request.exclude)?;
    let strategy = package_listing_strategy_from_glob_patterns(&request.include);
    if should_use_fast_package_listing_for_bazel_package_data(ctx, package.dupe()).await? {
        let listing = DicePackageListingResolver(ctx)
            .resolve_package_listing_with_strategy(package.dupe(), strategy)
            .await?;
        let mut results = spec
            .resolve_glob(listing.files())
            .map(|path| path.as_str().to_owned())
            .collect::<Vec<_>>();
        if request.include_directories {
            results.extend(
                listing
                    .dirs()
                    .filter(|path| spec.matches(path.as_str()))
                    .map(|path| path.as_str().to_owned()),
            );
        }
        add_exact_matches_from_file_values(
            ctx,
            package.dupe(),
            &spec,
            request.include_directories,
            &mut results,
        )
        .await?;
        results.sort();
        results.dedup();
        return Ok(results);
    }
    let buildfile_candidates = DiceFileComputations::buildfiles(ctx, package.cell_name()).await?;
    let package_root = package.as_cell_path().to_owned();
    let mut results = Vec::new();
    let mut stack = vec![Visit {
        path: PackageRelativePathBuf::unchecked_new(String::new()),
        traverse_children: true,
    }];

    while let Some(visit) = stack.pop() {
        let package_path = visit.path.as_path();
        let cell_path = package_root.join(package_path.as_forward_rel_path());
        let entries = read_dir_entries(ctx, cell_path.as_ref()).await?;
        let buildfile = find_buildfile(&buildfile_candidates, &entries);
        if package_path.is_empty() {
            if buildfile.is_none() {
                missing_buildfile(cell_path.as_ref(), &buildfile_candidates)?;
            }
        } else if buildfile.is_some() {
            continue;
        }

        if request.include_directories
            && !package_path.is_empty()
            && spec.matches(package_path.as_str())
        {
            results.push(package_path.as_str().to_owned());
        }
        if !visit.traverse_children {
            continue;
        }

        for entry in entries.iter().rev() {
            let child_path = package_path.join(&entry.file_name);
            if entry.file_type.is_dir() {
                if should_recurse_into(&strategy, &child_path) {
                    stack.push(Visit {
                        path: child_path,
                        traverse_children: true,
                    });
                } else if request.include_directories && spec.matches(child_path.as_path().as_str())
                {
                    stack.push(Visit {
                        path: child_path,
                        traverse_children: false,
                    });
                }
            } else if spec.matches(child_path.as_path().as_str()) {
                results.push(child_path.as_path().as_str().to_owned());
            }
        }
    }

    results.sort();
    results.dedup();
    Ok(results)
}

async fn add_exact_matches_from_file_values(
    ctx: &mut DiceComputations<'_>,
    package: PackageLabel,
    spec: &GlobSpec,
    include_directories: bool,
    results: &mut Vec<String>,
) -> buck2_error::Result<()> {
    for exact in spec.exact_matches() {
        if !spec.matches(exact) {
            continue;
        }
        let package_path = PackageRelativePath::new(exact)?;
        let cell_path = package
            .as_cell_path()
            .join(package_path.as_forward_rel_path());
        match DiceFileComputations::followed_path_type_if_exists(ctx, cell_path.as_ref()).await? {
            Some(FollowedPathType::File) => results.push(exact.to_owned()),
            Some(FollowedPathType::Directory) if include_directories => {
                results.push(exact.to_owned());
            }
            Some(FollowedPathType::Directory) | None => {}
        }
    }
    Ok(())
}

async fn compute_subpackages(
    ctx: &mut DiceComputations<'_>,
    package: PackageLabel,
) -> buck2_error::Result<Vec<String>> {
    if should_use_fast_package_listing_for_bazel_package_data(ctx, package.dupe()).await? {
        let listing = DicePackageListingResolver(ctx)
            .resolve_package_listing_with_strategy(package, PackageListingStrategy::Recursive)
            .await?;
        return Ok(listing
            .subpackages_within(PackageRelativePath::empty())
            .map(|path| path.as_str().to_owned())
            .collect());
    }
    let buildfile_candidates = DiceFileComputations::buildfiles(ctx, package.cell_name()).await?;
    let package_root = package.as_cell_path().to_owned();
    let mut results = Vec::new();
    let mut stack = vec![PackageRelativePathBuf::unchecked_new(String::new())];

    while let Some(path) = stack.pop() {
        let package_path = path.as_path();
        let cell_path = package_root.join(package_path.as_forward_rel_path());
        let entries = read_dir_entries(ctx, cell_path.as_ref()).await?;
        let buildfile = find_buildfile(&buildfile_candidates, &entries);
        if package_path.is_empty() {
            if buildfile.is_none() {
                missing_buildfile(cell_path.as_ref(), &buildfile_candidates)?;
            }
        } else if buildfile.is_some() {
            results.push(package_path.as_str().to_owned());
            continue;
        }

        for entry in entries.iter().rev() {
            if entry.file_type.is_dir() {
                stack.push(package_path.join(&entry.file_name));
            }
        }
    }

    results.sort();
    Ok(results)
}

struct Visit {
    path: PackageRelativePathBuf,
    traverse_children: bool,
}

async fn read_dir_entries(
    ctx: &mut DiceComputations<'_>,
    path: CellPathRef<'_>,
) -> buck2_error::Result<Arc<[SimpleDirEntry]>> {
    Ok(DiceFileComputations::read_dir_ext(ctx, path)
        .await
        .with_buck_error_context(|| format!("Error reading `{path}` while evaluating Bazel glob"))?
        .included)
}

fn missing_buildfile(
    package: CellPathRef<'_>,
    buildfile_candidates: &[FileNameBuf],
) -> buck2_error::Result<()> {
    let candidates = buildfile_candidates
        .iter()
        .map(|candidate| format!("`{candidate}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::Input,
        "package `{package}` has no build file; expected one of {candidates}"
    ))
}

async fn should_use_fast_package_listing_for_bazel_package_data(
    ctx: &mut DiceComputations<'_>,
    package: PackageLabel,
) -> buck2_error::Result<bool> {
    let cells = ctx.get_cell_resolver().await?;
    let cell = cells.get(package.cell_name())?;
    Ok(matches!(
        cell.external(),
        Some(ExternalCellOrigin::Bzlmod(_)) | Some(ExternalCellOrigin::BzlmodGenerated(_))
    ))
}

fn should_recurse_into(strategy: &PackageListingStrategy, child: &PackageRelativePath) -> bool {
    match strategy {
        PackageListingStrategy::Recursive => true,
        PackageListingStrategy::Shallow => false,
        PackageListingStrategy::Selective(prefixes) => prefixes.iter().any(|prefix| {
            let prefix: &PackageRelativePath = prefix.as_ref();
            prefix.starts_with(child) || child.starts_with(prefix)
        }),
    }
}
