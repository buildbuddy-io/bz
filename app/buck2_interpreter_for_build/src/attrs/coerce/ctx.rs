/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cell::RefCell;
use std::fmt;
use std::fmt::Debug;
use std::sync::Arc;

use buck2_common::package_listing::listing::PackageListing;
use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::CellResolver;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::cell_path_with_allowed_relative_dir::CellPathWithAllowedRelativeDir;
use buck2_core::cells::external::bzlmod_cell_name;
use buck2_core::cells::external::register_bzlmod_cell_canonical_repo_name_for_cell;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePathBuf;
use buck2_core::package::PackageLabel;
use buck2_core::package::package_relative_path::PackageRelativePath;
use buck2_core::package::package_relative_path::PackageRelativePathBuf;
use buck2_core::pattern::pattern::ParsedPattern;
use buck2_core::pattern::pattern::TargetParsingRel;
use buck2_core::pattern::pattern_type::PatternType;
use buck2_core::pattern::pattern_type::ProvidersPatternExtra;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::provider::label::ProvidersName;
use buck2_core::soft_error;
use buck2_core::target::label::interner::ConcurrentTargetLabelInterner;
use buck2_core::target::name::TargetName;
use buck2_core::target::name::TargetNameRef;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::coerced_path::CoercedDirectory;
use buck2_node::attrs::coerced_path::CoercedPath;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::configuration::resolved::ConfigurationSettingKey;
use buck2_node::query::query_functions::CONFIGURED_GRAPH_QUERY_FUNCTIONS;
use buck2_query::query::syntax::simple::eval::error::QueryError;
use buck2_query::query::syntax::simple::functions::QueryLiteralVisitor;
use buck2_query_parser::Expr;
use buck2_query_parser::spanned::Spanned;
use buck2_util::arc_str::ArcSlice;
use buck2_util::arc_str::ArcStr;
use bumpalo::Bump;
use dupe::Dupe;
use dupe::IterDupedExt;
use hashbrown::HashTable;
use hashbrown::hash_table;
use tracing::info;

use super::interner::AttrCoercionInterner;
use crate::attrs::coerce::arc_str_interner::ArcStrInterner;
use crate::attrs::coerce::str_hash::str_hash;
use crate::bazel_label::bazel_absolute_label_parts;
use crate::bazel_label::parse_bazel_canonical_providers_label;

#[derive(Debug, buck2_error::Error)]
#[buck2(input)]
enum BuildAttrCoercionContextError {
    #[error("Expected a label, got the pattern `{0}`.")]
    RequiredLabel(String),
    #[error("Expected a package: `{0}` can only be specified in a build file.")]
    NotBuildFileContext(String),
    #[error("Expected file, but got a directory for path `{1}` in package `{0}`.")]
    SourceFileIsDirectory(PackageLabel, String),
    #[error("Source file `{1}` does not exist as a member of package `{0}`.")]
    SourceFileMissing(PackageLabel, String),
    #[error(
        "Directory `{1}` of package `{0}` may not cover any subpackages, but includes subpackage `{2}`."
    )]
    SourceDirectoryIncludesSubPackage(PackageLabel, String, PackageRelativePathBuf),
}

/// An incomplete attr coercion context. Will be replaced with a real one later.
pub struct BuildAttrCoercionContext {
    /// Used to coerce targets
    cell_resolver: CellResolver,
    cell_name: CellName,
    cell_alias_resolver: CellAliasResolver,
    /// Used to resolve relative targets. This is present when a build file
    /// is being evaluated, however it is absent if an extension file is being
    /// evaluated. The latter case occurs when default values for attributes
    /// are coerced when a UDR is declared.
    enclosing_package: Option<(PackageLabel, PackageListing)>,
    /// This defines the limited scope in which we allow parsing patterns beginning with `../`
    current_dir_with_allowed_relative_dirs: CellPathWithAllowedRelativeDir,
    /// Does this package (if present) have a package boundary exception on it.
    package_boundary_exception: bool,
    /// Allocator for `label_cache`.
    alloc: Bump,
    global_label_interner: Arc<ConcurrentTargetLabelInterner>,
    /// Label coercion cache. We use `RawTable` where because `HashMap` API
    /// requires either computing hash twice (for get, then for insert) or
    /// allocating a key to perform a query using `entry` API.
    /// Strings are owned by `alloc`, using bump allocator makes evaluation 0.5% faster.
    label_cache: RefCell<HashTable<(u64, *const str, ProvidersLabel)>>,
    str_interner: ArcStrInterner,
    list_interner: AttrCoercionInterner<ArcSlice<CoercedAttr>>,
    // TODO(scottcao): Dict and selects need separate interners right now because
    // they have different key types. We can optimize this by interning keys and values
    // separately and use the same interner for dict and select values. This will also
    // reduce key duplication in selects since select keys are more likely to be deduplicated
    // than select values
    dict_interner: AttrCoercionInterner<ArcSlice<(CoercedAttr, CoercedAttr)>>,
    select_interner: AttrCoercionInterner<ArcSlice<(ConfigurationSettingKey, CoercedAttr)>>,
}

impl Debug for BuildAttrCoercionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildAttrCoercionContext")
            .finish_non_exhaustive()
    }
}

impl BuildAttrCoercionContext {
    fn is_bazel_compat_cell(&self) -> bool {
        let cell = self.cell_name.as_str();
        cell == "root"
            || cell == "bazel_tools"
            || cell.starts_with("bzlmod_")
            || (self.cell_name == self.cell_resolver.root_cell()
                && self.cell_alias_resolver.resolve("bazel_tools").is_ok())
    }

    fn new(
        cell_resolver: CellResolver,
        cell_name: CellName,
        cell_alias_resolver: CellAliasResolver,
        enclosing_package: Option<(PackageLabel, PackageListing)>,
        package_boundary_exception: bool,
        global_label_interner: Arc<ConcurrentTargetLabelInterner>,
        current_dir_with_allowed_relative_dirs: CellPathWithAllowedRelativeDir,
    ) -> Self {
        Self {
            cell_resolver,
            cell_name,
            cell_alias_resolver,
            enclosing_package,
            current_dir_with_allowed_relative_dirs,
            package_boundary_exception,
            alloc: Bump::new(),
            global_label_interner,
            label_cache: RefCell::new(HashTable::new()),
            str_interner: ArcStrInterner::new(),
            list_interner: AttrCoercionInterner::new(),
            dict_interner: AttrCoercionInterner::new(),
            select_interner: AttrCoercionInterner::new(),
        }
    }

    pub fn new_no_package(
        cell_resolver: CellResolver,
        cell_name: CellName,
        cell_alias_resolver: CellAliasResolver,
        global_label_interner: Arc<ConcurrentTargetLabelInterner>,
    ) -> Self {
        Self::new(
            cell_resolver,
            cell_name,
            cell_alias_resolver,
            None,
            false,
            global_label_interner,
            CellPathWithAllowedRelativeDir::backwards_relative_not_supported(CellPath::new(
                cell_name,
                CellRelativePathBuf::unchecked_new("".into()),
            )),
        )
    }

    pub fn new_with_package(
        cell_resolver: CellResolver,
        cell_alias_resolver: CellAliasResolver,
        enclosing_package: (PackageLabel, PackageListing),
        package_boundary_exception: bool,
        global_label_interner: Arc<ConcurrentTargetLabelInterner>,
        current_dir_with_allowed_relative_dirs: CellPathWithAllowedRelativeDir,
    ) -> Self {
        Self::new(
            cell_resolver,
            enclosing_package.0.cell_name(),
            cell_alias_resolver,
            Some(enclosing_package),
            package_boundary_exception,
            global_label_interner,
            current_dir_with_allowed_relative_dirs,
        )
    }

    pub fn parse_pattern<P: PatternType>(
        &self,
        value: &str,
    ) -> buck2_error::Result<ParsedPattern<P>> {
        let target_parsing_rel = match self.enclosing_package.as_ref().map(|x| x.0.as_cell_path()) {
            Some(package) => {
                if self
                    .current_dir_with_allowed_relative_dirs
                    .has_allowed_relative_dir()
                {
                    TargetParsingRel::AllowRelative(
                        &self.current_dir_with_allowed_relative_dirs,
                        None,
                    )
                } else {
                    TargetParsingRel::AllowLimitedRelative(package)
                }
            }
            None => TargetParsingRel::RequireAbsolute(self.cell_name),
        };
        ParsedPattern::parse_not_relaxed(
            value,
            target_parsing_rel,
            &self.cell_resolver,
            &self.cell_alias_resolver,
        )
    }

    fn coerce_label_no_cache(&self, value: &str) -> buck2_error::Result<ProvidersLabel> {
        // TODO(nmj): Make this take an import path / package
        let normalized_value;
        let value = if let Some(root_label) = value.strip_prefix("@@root//") {
            normalized_value = format!("root//{root_label}");
            normalized_value.as_str()
        } else {
            value
        };
        if let Some(label) =
            parse_bazel_canonical_providers_label(value, self.cell_resolver.root_cell())?
        {
            return Ok(label);
        }
        let pattern = match self.parse_pattern::<ProvidersPatternExtra>(value) {
            Ok(pattern) => pattern,
            Err(_)
                if self.enclosing_package.is_some()
                    && is_bazel_relative_target_shorthand(value) =>
            {
                match self.parse_pattern::<ProvidersPatternExtra>(&format!(":{value}")) {
                    Ok(pattern) => pattern,
                    Err(e) => {
                        if let Some(label) = self.coerce_bazel_compat_label(value)? {
                            return Ok(label);
                        }
                        return Err(e);
                    }
                }
            }
            Err(_)
                if self.enclosing_package.is_some()
                    && self.is_bazel_compat_cell()
                    && is_bazel_package_relative_target(value) =>
            {
                match self.parse_pattern::<ProvidersPatternExtra>(&format!(":{value}")) {
                    Ok(pattern) => pattern,
                    Err(e) => {
                        if let Some(label) = self.coerce_bazel_compat_label(value)? {
                            return Ok(label);
                        }
                        return Err(e);
                    }
                }
            }
            Err(e) => {
                if let Some(label) = self.coerce_bazel_compat_label(value)? {
                    return Ok(label);
                }
                if let Some(label) = self.coerce_bazel_repo_shorthand_label(value)? {
                    return Ok(label);
                }
                if let Some(label) = self.coerce_bazel_non_visible_repo_label(value)? {
                    return Ok(label);
                }
                return Err(e);
            }
        };
        match pattern {
            ParsedPattern::Target(package, target_name, providers) => {
                Ok(providers.into_providers_label(package, target_name.as_ref()))
            }
            ParsedPattern::Package(package) => {
                let Some(target_name) = package.cell_relative_path().file_name() else {
                    return Err(
                        BuildAttrCoercionContextError::RequiredLabel(value.to_owned()).into(),
                    );
                };
                let target_name = TargetNameRef::new(target_name.as_str())?;
                Ok(ProvidersLabel::new(
                    buck2_core::target::label::label::TargetLabel::new(package, target_name),
                    ProvidersName::Default,
                ))
            }
            _ => Err(BuildAttrCoercionContextError::RequiredLabel(value.to_owned()).into()),
        }
    }

    fn coerce_bazel_repo_shorthand_label(
        &self,
        value: &str,
    ) -> buck2_error::Result<Option<ProvidersLabel>> {
        if !self.is_bazel_compat_cell() {
            return Ok(None);
        }

        let (repo, cell_name) = if let Some(repo) = value.strip_prefix("@@") {
            if repo.is_empty() || repo.contains(['/', ':', '[', ']']) {
                return Ok(None);
            }
            let cell_name = if repo == "bazel_tools" {
                CellName::unchecked_new("bazel_tools")?
            } else {
                CellName::unchecked_new(&bzlmod_cell_name(repo))?
            };
            (repo, cell_name)
        } else if let Some(repo) = value.strip_prefix('@') {
            if repo.is_empty() || repo.contains(['/', ':', '[', ']']) {
                return Ok(None);
            }
            let cell_name = match self.cell_alias_resolver.resolve(repo) {
                Ok(cell_name) => cell_name,
                Err(_) => self.bazel_non_visible_repo_cell_name(repo)?,
            };
            (repo, cell_name)
        } else {
            return Ok(None);
        };

        let package = PackageLabel::new(
            cell_name,
            CellRelativePathBuf::try_from(String::new())?.as_ref(),
        )?;
        let target = TargetNameRef::new(repo)?;
        Ok(Some(ProvidersLabel::new(
            buck2_core::target::label::label::TargetLabel::new(package, target),
            ProvidersName::Default,
        )))
    }

    fn coerce_bazel_non_visible_repo_label(
        &self,
        value: &str,
    ) -> buck2_error::Result<Option<ProvidersLabel>> {
        if !self.is_bazel_compat_cell() {
            return Ok(None);
        }

        let Some(value) = value.strip_prefix('@') else {
            return Ok(None);
        };
        if value.starts_with('@') {
            return Ok(None);
        }

        let Some((repo, label)) = value.split_once("//") else {
            return Ok(None);
        };
        if repo.is_empty() || self.cell_alias_resolver.resolve(repo).is_ok() {
            return Ok(None);
        }

        let Some((package, target)) = bazel_absolute_label_parts(label) else {
            return Ok(None);
        };

        // Bazel keeps labels with repos that are not visible from the current repo as
        // non-visible RepositoryName values and reports the repo-mapping error only if
        // analysis actually reaches that label.
        let cell_name = self.bazel_non_visible_repo_cell_name(repo)?;
        let package =
            PackageLabel::new(cell_name, CellRelativePathBuf::try_from(package)?.as_ref())?;
        let target = TargetNameRef::new_bazel(&target)?;
        Ok(Some(ProvidersLabel::new(
            buck2_core::target::label::label::TargetLabel::new(package, target),
            ProvidersName::Default,
        )))
    }

    fn coerce_bazel_compat_label(
        &self,
        value: &str,
    ) -> buck2_error::Result<Option<ProvidersLabel>> {
        if !self.is_bazel_compat_cell() {
            return Ok(None);
        }

        if let Some(target) = value.strip_prefix(':') {
            return self.coerce_bazel_package_label(target, value).map(Some);
        }

        if let Some(package_and_target) = value.strip_prefix("//") {
            return self
                .coerce_bazel_absolute_label(self.cell_name, package_and_target)
                .map(Some);
        }

        if let Some(value) = value.strip_prefix('@') {
            if value.starts_with('@') {
                return Ok(None);
            }
            let Some((repo, package_and_target)) = value.split_once("//") else {
                return Ok(None);
            };
            let cell_name = if repo.is_empty() {
                self.cell_resolver.root_cell()
            } else {
                match self.cell_alias_resolver.resolve(repo) {
                    Ok(cell_name) => cell_name,
                    Err(_) => return Ok(None),
                }
            };
            return self
                .coerce_bazel_absolute_label(cell_name, package_and_target)
                .map(Some);
        }

        if let Some((cell, package_and_target)) = value.split_once("//") {
            if !cell.is_empty()
                && !cell.contains(['@', '/', ':', '[', ']'])
                && let Ok(cell_name) = if cell == "root" {
                    Ok(self.cell_resolver.root_cell())
                } else if cell == "bazel_tools" {
                    CellName::unchecked_new("bazel_tools")
                } else {
                    self.cell_alias_resolver.resolve(cell)
                }
            {
                return self
                    .coerce_bazel_absolute_label(cell_name, package_and_target)
                    .map(Some);
            }
        }

        if self.enclosing_package.is_some() && is_bazel_package_relative_target(value) {
            return self.coerce_bazel_package_label(value, value).map(Some);
        }

        Ok(None)
    }

    fn coerce_bazel_package_label(
        &self,
        target: &str,
        original: &str,
    ) -> buck2_error::Result<ProvidersLabel> {
        let package = self.require_enclosing_package(original)?.0.dupe();
        let target = TargetName::new_bazel(target)?;
        Ok(ProvidersLabel::new(
            buck2_core::target::label::label::TargetLabel::new(package, target.as_ref()),
            ProvidersName::Default,
        ))
    }

    fn coerce_bazel_absolute_label(
        &self,
        cell_name: CellName,
        package_and_target: &str,
    ) -> buck2_error::Result<ProvidersLabel> {
        let Some((package, target)) = bazel_absolute_label_parts(package_and_target) else {
            return Err(BuildAttrCoercionContextError::RequiredLabel(
                package_and_target.to_owned(),
            )
            .into());
        };
        let package =
            PackageLabel::new(cell_name, CellRelativePathBuf::try_from(package)?.as_ref())?;
        let target = TargetName::new_bazel(&target)?;
        Ok(ProvidersLabel::new(
            buck2_core::target::label::label::TargetLabel::new(package, target.as_ref()),
            ProvidersName::Default,
        ))
    }

    fn coerce_bazel_non_visible_repo_visibility_pattern(
        &self,
        value: &str,
    ) -> buck2_error::Result<Option<ParsedPattern<TargetPatternExtra>>> {
        if !self.is_bazel_compat_cell() {
            return Ok(None);
        }

        let Some(value) = value.strip_prefix('@') else {
            return Ok(None);
        };
        if value.starts_with('@') {
            return Ok(None);
        }

        let Some((repo, pattern)) = value.split_once("//") else {
            return Ok(None);
        };
        if repo.is_empty() || self.cell_alias_resolver.resolve(repo).is_ok() {
            return Ok(None);
        }

        let cell_name = self.bazel_non_visible_repo_cell_name(repo)?;
        Ok(Some(parse_non_visible_repo_target_pattern(
            cell_name, pattern,
        )?))
    }

    fn bazel_non_visible_repo_cell_name(&self, repo: &str) -> buck2_error::Result<CellName> {
        let canonical_repo_name = format!("unknown+{}+{}", self.cell_name.as_str(), repo);
        let cell_name = bzlmod_cell_name(&canonical_repo_name);
        register_bzlmod_cell_canonical_repo_name_for_cell(&cell_name, &canonical_repo_name);
        CellName::unchecked_new(&cell_name)
    }

    fn require_enclosing_package(
        &self,
        msg: &str,
    ) -> buck2_error::Result<&(PackageLabel, PackageListing)> {
        self.enclosing_package.as_ref().ok_or_else(|| {
            BuildAttrCoercionContextError::NotBuildFileContext(msg.to_owned()).into()
        })
    }
}

fn is_bazel_relative_target_shorthand(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['@', ':', '/', '.'])
        && !value.contains('/')
        && !value.contains(':')
        && !value.contains('[')
        && !value.contains(']')
}

fn is_bazel_package_relative_target(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['@', ':', '/'])
        && !value.contains(':')
        && !value.contains('[')
        && !value.contains(']')
}

fn parse_non_visible_repo_target_pattern(
    cell_name: CellName,
    pattern: &str,
) -> buck2_error::Result<ParsedPattern<TargetPatternExtra>> {
    if pattern == "..." {
        return Ok(ParsedPattern::Recursive(CellPath::new(
            cell_name,
            CellRelativePathBuf::try_from(String::new())?,
        )));
    }

    if let Some(package) = pattern.strip_suffix("/...") {
        return Ok(ParsedPattern::Recursive(CellPath::new(
            cell_name,
            CellRelativePathBuf::try_from(package.to_owned())?,
        )));
    }

    let (package, target) = if let Some((package, target)) = pattern.rsplit_once(':') {
        if target.is_empty() {
            let package = PackageLabel::new(
                cell_name,
                CellRelativePathBuf::try_from(package.to_owned())?.as_ref(),
            )?;
            return Ok(ParsedPattern::Package(package));
        }
        (package.to_owned(), target.to_owned())
    } else {
        let target = pattern.rsplit('/').next().unwrap_or(pattern);
        (pattern.to_owned(), target.to_owned())
    };

    let package = PackageLabel::new(cell_name, CellRelativePathBuf::try_from(package)?.as_ref())?;
    let target = TargetName::new_bazel(&target)?;
    Ok(ParsedPattern::Target(package, target, TargetPatternExtra))
}

impl AttrCoercionContext for BuildAttrCoercionContext {
    fn coerce_providers_label(&self, value: &str) -> buck2_error::Result<ProvidersLabel> {
        let hash = str_hash(value);
        let mut label_cache = self.label_cache.borrow_mut();

        match label_cache.entry(
            hash,
            |(h, v, _)| *h == hash && value == unsafe { &**v },
            |(h, _, _)| *h,
        ) {
            hash_table::Entry::Occupied(e) => Ok(e.get().2.dupe()),
            hash_table::Entry::Vacant(e) => {
                let label = self.coerce_label_no_cache(value)?;

                let (target_label, providers) = label.into_parts();
                let target_label = self.global_label_interner.intern(target_label);
                let label = ProvidersLabel::new(target_label, providers);

                e.insert((hash, self.alloc.alloc_str(value), label.dupe()));
                Ok(label)
            }
        }
    }

    fn intern_str(&self, value: &str) -> ArcStr {
        self.str_interner.intern(value)
    }

    fn intern_list(&self, value: Vec<CoercedAttr>) -> ArcSlice<CoercedAttr> {
        self.list_interner.intern(value)
    }

    fn intern_dict(
        &self,
        value: Vec<(CoercedAttr, CoercedAttr)>,
    ) -> ArcSlice<(CoercedAttr, CoercedAttr)> {
        self.dict_interner.intern(value)
    }

    fn intern_select(
        &self,
        value: Vec<(ConfigurationSettingKey, CoercedAttr)>,
    ) -> ArcSlice<(ConfigurationSettingKey, CoercedAttr)> {
        self.select_interner.intern(value)
    }

    fn coerce_path(&self, value: &str, allow_directory: bool) -> buck2_error::Result<CoercedPath> {
        let path = <&PackageRelativePath>::try_from(value)?;
        let (package, listing) = self.require_enclosing_package(value)?;

        if let Some(path) = listing.get_file(path) {
            return Ok(CoercedPath::File(path));
        }

        // TODO: Make the warnings below into errors
        if let Some(path) = listing.get_dir(path) {
            if !allow_directory {
                return Err(BuildAttrCoercionContextError::SourceFileIsDirectory(
                    package.dupe(),
                    value.to_owned(),
                )
                .into());
            } else if let Some(subpackage) = listing.subpackages_within(&path).next() {
                let e = BuildAttrCoercionContextError::SourceDirectoryIncludesSubPackage(
                    package.dupe(),
                    value.to_owned(),
                    subpackage.to_owned(),
                );
                if self.package_boundary_exception {
                    info!("{} (could be due to a package boundary violation)", e);
                } else {
                    soft_error!("source_directory_includes_subpackage", e.into(), error_on_oss: true)?;
                }
            }
            let files = listing.files_within(&path).duped().collect();
            Ok(CoercedPath::Directory(Box::new(CoercedDirectory {
                dir: path,
                files,
            })))
        } else {
            let e =
                BuildAttrCoercionContextError::SourceFileMissing(package.dupe(), value.to_owned());
            if self.package_boundary_exception {
                info!("{} (could be due to a package boundary violation)", e);
            } else {
                soft_error!(
                    "source_file_missing",
                    e.into(),
                    quiet: true,
                    error_on_oss: !self.is_bazel_compat_cell()
                )?;
            }

            Ok(CoercedPath::File(path.to_arc()))
        }
    }

    fn coerce_existing_path(
        &self,
        value: &str,
        allow_directory: bool,
    ) -> buck2_error::Result<Option<CoercedPath>> {
        let path = <&PackageRelativePath>::try_from(value)?;
        let (package, listing) = self.require_enclosing_package(value)?;

        if let Some(path) = listing.get_file(path) {
            return Ok(Some(CoercedPath::File(path)));
        }

        if let Some(path) = listing.get_dir(path) {
            if !allow_directory {
                return Ok(None);
            } else if let Some(subpackage) = listing.subpackages_within(&path).next() {
                let e = BuildAttrCoercionContextError::SourceDirectoryIncludesSubPackage(
                    package.dupe(),
                    value.to_owned(),
                    subpackage.to_owned(),
                );
                if self.package_boundary_exception {
                    info!("{} (could be due to a package boundary violation)", e);
                } else {
                    soft_error!(
                        "source_directory_includes_subpackage",
                        e.into(),
                        error_on_oss: true
                    )?;
                }
            }
            let files = listing.files_within(&path).duped().collect();
            return Ok(Some(CoercedPath::Directory(Box::new(CoercedDirectory {
                dir: path,
                files,
            }))));
        }

        Ok(None)
    }

    fn coerce_target_pattern(
        &self,
        pattern: &str,
    ) -> buck2_error::Result<ParsedPattern<TargetPatternExtra>> {
        self.parse_pattern(pattern)
    }

    fn coerce_visibility_pattern(
        &self,
        pattern: &str,
    ) -> buck2_error::Result<Option<ParsedPattern<TargetPatternExtra>>> {
        match self.parse_pattern(pattern) {
            Ok(pattern) => Ok(Some(pattern)),
            Err(e) => {
                if let Some(pattern) =
                    self.coerce_bazel_non_visible_repo_visibility_pattern(pattern)?
                {
                    return Ok(Some(pattern));
                }
                Err(e)
            }
        }
    }

    fn enclosing_package(&self) -> Option<PackageLabel> {
        self.enclosing_package
            .as_ref()
            .map(|(package, _)| package.dupe())
    }

    fn is_bazel_compat_cell(&self) -> bool {
        BuildAttrCoercionContext::is_bazel_compat_cell(self)
    }

    fn visit_query_function_literals<'q>(
        &self,
        visitor: &mut dyn QueryLiteralVisitor<'q>,
        expr: &Spanned<Expr<'q>>,
        query: &'q str,
    ) -> buck2_error::Result<()> {
        CONFIGURED_GRAPH_QUERY_FUNCTIONS
            .get()?
            .visit_literals(visitor, expr)
            .map_err(|e| QueryError::convert_error(e, query))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use buck2_node::attrs::coercion_context::AttrCoercionContext;

    use crate::attrs::coerce::testing::coercion_ctx;

    #[test]
    fn bazel_compat_accepts_package_relative_label_with_slashes() -> buck2_error::Result<()> {
        let ctx = coercion_ctx();
        let label = ctx.coerce_providers_label("bin/nodejs/bin/node")?;
        assert_eq!(
            "root//package/subdir:bin/nodejs/bin/node",
            label.to_string()
        );
        Ok(())
    }

    #[test]
    fn bazel_compat_accepts_bazel_target_name_characters() -> buck2_error::Result<()> {
        let ctx = coercion_ctx();
        let label = ctx.coerce_providers_label(
            "lib/python3.11/site-packages/setuptools/_vendor/jaraco/text/Lorem ipsum.txt",
        )?;
        assert_eq!(
            "root//package/subdir:lib/python3.11/site-packages/setuptools/_vendor/jaraco/text/Lorem ipsum.txt",
            label.to_string()
        );

        let label = ctx.coerce_providers_label("//:b$() ar")?;
        assert_eq!("root//:b$() ar", label.to_string());
        Ok(())
    }
}
