/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Parses imports for load_file() calls in build files.

use buck2_core::bzl::ImportPath;
use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::build_file_cell::BuildFileCell;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::cell_path_with_allowed_relative_dir::CellPathWithAllowedRelativeDir;
use buck2_core::cells::external::bzlmod_cell_name;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePath;
use buck2_core::cells::paths::CellRelativePathBuf;
use buck2_fs::paths::RelativePath;

#[derive(buck2_error::Error, Debug)]
#[buck2(input)]
enum ImportParseError {
    #[error(
        "Unable to parse import spec. Expected format `(@<cell>)//package/name:filename.bzl` or `:filename.bzl`. Got `{0}`"
    )]
    MatchFailed(String),
    #[error(
        "Unable to parse import spec. Expected format `(@<cell>)//package/name:filename.bzl` or `:filename.bzl`, but got an empty filename. Got `{0}`"
    )]
    EmptyFileName(String),
    #[error("Unexpected relative import spec. Got `{0}`")]
    ProhibitedRelativeImport(String),
    #[error(
        "Unable to parse import spec. Expected format `(@<cell>)//package/name:filename.bzl` or `:filename.bzl`, but got an invalid file path. Got `{0}`"
    )]
    InvalidFilePath(String),
}

pub enum RelativeImports<'a> {
    Allow {
        current_dir_with_allowed_relative: &'a CellPathWithAllowedRelativeDir,
    },
    Disallow,
}

/// Extra options for parsing a load() or load-like path into a `BuckPath`
pub struct ParseImportOptions<'a> {
    /// Whether '@' is required at the beginning of the import.
    pub allow_missing_at_symbol: bool,
    /// Whether relative imports (':bar.bzl') are allowed.
    pub relative_import_option: RelativeImports<'a>,
}

pub struct ParsedImport {
    pub path: CellPath,
    pub package_root: Option<CellPath>,
}

// Parses a string of the form `(@<cell>)//dir/name` to the corresponding
// alias and cell relative path.
enum ImportCell<'a> {
    Alias(&'a str),
    Canonical(CellName),
}

fn parse_import_cell(alias: &str, repo_is_canonical: bool) -> Option<ImportCell<'_>> {
    if repo_is_canonical {
        let cell = if alias.is_empty() || alias == "root" {
            CellName::unchecked_new("root").ok()?
        } else if alias == "bazel_tools" {
            CellName::unchecked_new("bazel_tools").ok()?
        } else {
            CellName::unchecked_new(&bzlmod_cell_name(alias)).ok()?
        };
        Some(ImportCell::Canonical(cell))
    } else {
        Some(ImportCell::Alias(alias))
    }
}

impl ImportCell<'_> {
    fn resolve(self, cell_resolver: &CellAliasResolver) -> buck2_error::Result<CellName> {
        match self {
            ImportCell::Alias(alias) => cell_resolver.resolve(alias),
            ImportCell::Canonical(cell) => Ok(cell),
        }
    }
}

fn parse_import_cell_path_parts(
    path: &str,
    allow_missing_at_symbol: bool,
) -> Option<(ImportCell<'_>, &str)> {
    let (alias, cell_rel_path) = path.split_once("//")?;
    let cell = if let Some(canonical_repo_name) = alias.strip_prefix("@@") {
        parse_import_cell(canonical_repo_name, true)?
    } else if alias.is_empty() {
        ImportCell::Alias(alias)
    } else if !alias.starts_with('@') {
        if alias.starts_with("bzlmod_") {
            ImportCell::Canonical(CellName::unchecked_new(alias).ok()?)
        } else if !allow_missing_at_symbol {
            return None;
        } else {
            ImportCell::Alias(alias)
        }
    } else {
        ImportCell::Alias(&alias[1..])
    };
    Some((cell, cell_rel_path))
}

fn parse_bazel_repo_root_import(path: &str) -> Option<(ImportCell<'_>, &str)> {
    if !path.starts_with('@') || path.contains("//") {
        return None;
    }
    let (repo_is_canonical, repo) = if let Some(repo) = path.strip_prefix("@@") {
        (true, repo)
    } else {
        (false, &path[1..])
    };
    if repo.is_empty() {
        return None;
    }
    Some((parse_import_cell(repo, repo_is_canonical)?, repo))
}

fn parse_import_file_path<'a>(
    import: &str,
    file_path: &'a str,
) -> buck2_error::Result<&'a CellRelativePath> {
    if file_path.is_empty() {
        return Err(ImportParseError::EmptyFileName(import.to_owned()).into());
    }

    CellRelativePath::from_path(file_path)
        .map_err(|_| ImportParseError::InvalidFilePath(import.to_owned()).into())
}

pub fn parse_import(
    cell_resolver: &CellAliasResolver,
    relative_import_option: RelativeImports,
    import: &str,
) -> buck2_error::Result<CellPath> {
    let opts: ParseImportOptions = ParseImportOptions {
        allow_missing_at_symbol: false,
        relative_import_option,
    };
    parse_import_with_config(cell_resolver, import, &opts)
}

/// Parse import string into a BuckPath, but potentially be more or less flexible with what is
/// accepted.
///
/// Common use case is e.g. allowing "cell//foo:bar.bzl" to be passed on the command line
/// and letting that be turned into an ImportPath eventually, or disallowing relative imports
/// from command line arguments.
///
/// Strings for the `load()` statement in starlark files should use [`parse_import`]
pub fn parse_import_with_config(
    cell_resolver: &CellAliasResolver,
    import: &str,
    opts: &ParseImportOptions,
) -> buck2_error::Result<CellPath> {
    Ok(parse_import_with_config_and_package_root(cell_resolver, import, opts)?.path)
}

pub fn parse_import_with_config_and_package_root(
    cell_resolver: &CellAliasResolver,
    import: &str,
    opts: &ParseImportOptions,
) -> buck2_error::Result<ParsedImport> {
    match import.split_once(':') {
        None => {
            // import without `:`, so just try to parse the cell and cell relative paths

            match parse_import_cell_path_parts(import, opts.allow_missing_at_symbol) {
                None => {
                    if let Some((cell, file_name)) = parse_bazel_repo_root_import(import) {
                        let cell = cell.resolve(cell_resolver)?;
                        return Ok(ParsedImport {
                            path: CellPath::new(
                                cell,
                                CellRelativePathBuf::try_from(file_name.to_owned())?,
                            ),
                            package_root: None,
                        });
                    }
                    if let RelativeImports::Allow {
                        current_dir_with_allowed_relative,
                    } = opts.relative_import_option
                    {
                        Ok(ParsedImport {
                            path: current_dir_with_allowed_relative
                                .join_normalized(RelativePath::from_path(import)?)?,
                            package_root: None,
                        })
                    } else {
                        Err(ImportParseError::ProhibitedRelativeImport(import.to_owned()).into())
                    }
                }
                Some((cell, cell_relative_path)) => {
                    let cell = cell.resolve(cell_resolver)?;
                    Ok(ParsedImport {
                        path: CellPath::new(
                            cell,
                            CellRelativePathBuf::try_from(cell_relative_path.to_owned())?,
                        ),
                        package_root: None,
                    })
                }
            }
        }
        Some((path, filename)) => {
            let file_path = parse_import_file_path(import, filename)?;

            if path.is_empty() {
                if let RelativeImports::Allow {
                    current_dir_with_allowed_relative,
                } = opts.relative_import_option
                {
                    let package_root = current_dir_with_allowed_relative.current_dir().clone();
                    Ok(ParsedImport {
                        path: current_dir_with_allowed_relative
                            .join_normalized(file_path.as_ref())?,
                        package_root: Some(package_root),
                    })
                } else {
                    Err(ImportParseError::ProhibitedRelativeImport(import.to_owned()).into())
                }
            } else {
                let (cell, cell_relative_path) =
                    parse_import_cell_path_parts(path, opts.allow_missing_at_symbol)
                        .ok_or_else(|| ImportParseError::MatchFailed(import.to_owned()))?;
                let cell = cell.resolve(cell_resolver)?;
                let package_root = CellPath::new(
                    cell,
                    CellRelativePathBuf::try_from(cell_relative_path.to_owned())?,
                );
                Ok(ParsedImport {
                    path: CellPath::new(cell, package_root.path().join(file_path)),
                    package_root: Some(package_root),
                })
            }
        }
    }
}

pub fn parse_bzl_path_with_config(
    cell_resolver: &CellAliasResolver,
    import: &str,
    opts: &ParseImportOptions,
    build_cell_path: BuildFileCell,
) -> buck2_error::Result<ImportPath> {
    let parsed = parse_import_with_config_and_package_root(cell_resolver, import, opts)?;
    ImportPath::new_with_build_file_cells_and_package_root(
        parsed.path,
        build_cell_path,
        parsed.package_root,
    )
}

#[cfg(test)]
mod tests {
    use buck2_core::cells::alias::NonEmptyCellAlias;
    use buck2_core::cells::name::CellName;
    use buck2_fs::paths::file_name::FileName;
    use buck2_hash::StdBuckHashMap;

    use super::*;

    fn resolver() -> CellAliasResolver {
        let mut m = StdBuckHashMap::default();
        m.insert(
            NonEmptyCellAlias::new("cell1".to_owned()).unwrap(),
            CellName::testing_new("cell1"),
        );
        m.insert(
            NonEmptyCellAlias::new("alias2".to_owned()).unwrap(),
            CellName::testing_new("cell2"),
        );
        m.insert(
            NonEmptyCellAlias::new("with_cfg.bzl".to_owned()).unwrap(),
            CellName::testing_new("bzlmod_with_cfg_bzl_"),
        );
        CellAliasResolver::new(CellName::testing_new("root"), m).expect("valid resolver")
    }

    fn path(cell: &str, dir: &str, filename: &str) -> CellPath {
        CellPath::new(
            CellName::testing_new(cell),
            CellRelativePath::unchecked_new(dir).join(FileName::unchecked_new(filename)),
        )
    }

    #[test]
    fn root_package() -> buck2_error::Result<()> {
        assert_eq!(
            path("root", "package/path", "import.bzl"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("passport//"),
                        None,
                    ),
                },
                "//package/path:import.bzl"
            )?
        );
        Ok(())
    }

    #[test]
    fn cell_package() -> buck2_error::Result<()> {
        assert_eq!(
            path("cell1", "package/path", "import.bzl"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("root//"),
                        None,
                    ),
                },
                "@cell1//package/path:import.bzl"
            )?
        );
        Ok(())
    }

    #[test]
    fn bazel_canonical_repo_package() -> buck2_error::Result<()> {
        assert_eq!(
            path("bzlmod_rules_go_0_57_0", "go", "def.bzl"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("root//"),
                        None,
                    ),
                },
                "@@rules_go+0.57.0//go:def.bzl"
            )?
        );
        Ok(())
    }

    #[test]
    fn bzlmod_internal_cell_package() -> buck2_error::Result<()> {
        assert_eq!(
            path(
                "bzlmod_unknown_bzlmod_googleapis_cc__protobuf",
                "bazel",
                "cc_proto_library.bzl"
            ),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("root//"),
                        None,
                    ),
                },
                "bzlmod_unknown_bzlmod_googleapis_cc__protobuf//bazel:cc_proto_library.bzl"
            )?
        );
        Ok(())
    }

    #[test]
    fn bazel_canonical_main_repo_package() -> buck2_error::Result<()> {
        assert_eq!(
            path("root", "does/not", "exist"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("bzlmod_rules_jvm_external_//"),
                        None,
                    ),
                },
                "@@//does/not:exist"
            )?
        );
        Ok(())
    }

    #[test]
    fn bazel_package_target_path() -> buck2_error::Result<()> {
        assert_eq!(
            path(
                "root",
                "upb/bazel/private/upb_proto_library_internal",
                "aspect.bzl"
            ),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("root//"),
                        None,
                    ),
                },
                "//upb/bazel/private:upb_proto_library_internal/aspect.bzl"
            )?
        );
        Ok(())
    }

    #[test]
    fn bazel_repo_root_label_shorthand() -> buck2_error::Result<()> {
        assert_eq!(
            path("bzlmod_with_cfg_bzl_", "", "with_cfg.bzl"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("root//rules"),
                        None,
                    ),
                },
                "@with_cfg.bzl"
            )?
        );
        Ok(())
    }

    #[test]
    fn package_relative() -> buck2_error::Result<()> {
        assert_eq!(
            path("cell1", "package/path", "import.bzl"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("cell1//package/path"),
                        None,
                    ),
                },
                ":import.bzl"
            )?
        );
        assert_eq!(
            path("cell1", "package/path/subdir", "import.bzl"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("cell1//package/path"),
                        None,
                    ),
                },
                ":subdir/import.bzl"
            )?
        );
        Ok(())
    }

    #[test]
    fn missing_colon() -> buck2_error::Result<()> {
        let import = "//package/path/import.bzl".to_owned();
        assert_eq!(
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("lighter//"),
                        None,
                    ),
                },
                &import
            )?,
            path("root", "package/path", "import.bzl")
        );
        Ok(())
    }

    #[test]
    fn empty_filename() -> buck2_error::Result<()> {
        let path = "//package/path:".to_owned();
        match parse_import(
            &resolver(),
            RelativeImports::Allow {
                current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                    CellPath::testing_new("root//"),
                    None,
                ),
            },
            &path,
        ) {
            Ok(import) => panic!("Expected parse failure for {path}, got result {import}"),
            Err(e) => {
                assert_eq!(
                    format!("{e:#}"),
                    ImportParseError::EmptyFileName(path.to_owned()).to_string()
                );
            }
        }
        Ok(())
    }

    #[test]
    fn bad_alias() -> buck2_error::Result<()> {
        let path = "bad_alias//package/path:".to_owned();
        match parse_import(
            &resolver(),
            RelativeImports::Allow {
                current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                    CellPath::testing_new("root//"),
                    None,
                ),
            },
            &path,
        ) {
            Ok(import) => panic!("Expected parse failure for {path}, got result {import}"),
            Err(_) => {
                // TODO: should we verify the contents of the error?
            }
        }
        Ok(())
    }

    #[test]
    fn file_relative_import_given_relative_paths_allowed() -> buck2_error::Result<()> {
        assert_eq!(
            path("cell1", "package/path", "bar.bzl"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("cell1//package/path"),
                        None,
                    ),
                },
                "bar.bzl",
            )?
        );
        assert_eq!(
            path("cell1", "package/path", "foo/bar.bzl"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("cell1//package/path"),
                        None,
                    ),
                },
                "foo/bar.bzl",
            )?
        );
        Ok(())
    }

    #[test]
    fn cell_relative_import_given_relative_paths_allowed() -> buck2_error::Result<()> {
        let importer = CellPath::testing_new("cell1//package/path");
        let importee = "foo/bar.bzl";

        assert_eq!(
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        importer, None,
                    ),
                },
                importee
            )?,
            path("cell1", "package/path/foo", "bar.bzl")
        );
        Ok(())
    }

    #[test]
    fn regular_import_given_relative_paths_allowed() -> buck2_error::Result<()> {
        assert_eq!(
            path("cell1", "package/path", "import.bzl"),
            parse_import(
                &resolver(),
                RelativeImports::Allow {
                    current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                        CellPath::testing_new("root//foo/bar"),
                        None,
                    ),
                },
                "@cell1//package/path:import.bzl",
            )?
        );
        Ok(())
    }

    #[test]
    fn allows_non_at_symbols() -> buck2_error::Result<()> {
        assert_eq!(
            path("cell1", "package/path", "import.bzl"),
            parse_import_with_config(
                &resolver(),
                "cell1//package/path:import.bzl",
                &ParseImportOptions {
                    allow_missing_at_symbol: true,
                    relative_import_option: RelativeImports::Allow {
                        current_dir_with_allowed_relative: &CellPathWithAllowedRelativeDir::new(
                            CellPath::testing_new("root//"),
                            None,
                        ),
                    },
                }
            )?,
        );
        Ok(())
    }

    #[test]
    fn fails_relative_import_if_disallowed() -> buck2_error::Result<()> {
        let imported_file = ":bar.bzl";
        let res = parse_import_with_config(
            &resolver(),
            imported_file,
            &ParseImportOptions {
                allow_missing_at_symbol: false,
                relative_import_option: RelativeImports::Disallow,
            },
        );
        match res {
            Ok(res) => panic!("Expected parse failure for {imported_file}, got result {res}"),
            Err(e) => {
                assert_eq!(
                    format!("{e:#}"),
                    ImportParseError::ProhibitedRelativeImport(imported_file.to_owned())
                        .to_string()
                );
            }
        };
        Ok(())
    }
}
