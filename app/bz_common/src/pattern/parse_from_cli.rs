/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_core::cells::CellAliasResolver;
use bz_core::cells::CellResolver;
use bz_core::cells::cell_path::CellPath;
use bz_core::cells::cell_path::CellPathRef;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::package::PackageLabel;
use bz_core::pattern::pattern::Modifiers;
use bz_core::pattern::pattern::ParsedPattern;
use bz_core::pattern::pattern::ParsedPatternWithModifiers;
use bz_core::pattern::pattern::PatternDataOrAmbiguous;
use bz_core::pattern::pattern::PatternParts;
use bz_core::pattern::pattern::lex_target_pattern;
use bz_core::pattern::pattern_type::PatternType;
use bz_core::pattern::unparsed::UnparsedPatterns;
use bz_core::target::name::TargetName;
use bz_core::target_aliases::TargetAliasResolver;
use bz_error::BuckErrorContext;
use bz_fs::paths::RelativePath;
use dice::DiceComputations;

use crate::dice::cells::HasCellResolver;
use crate::file_ops::trait_::DiceFileOps;
use crate::file_ops::trait_::FileOps;
use crate::find_buildfile::find_buildfile;
use crate::pattern::resolve::ResolveTargetPatterns;
use crate::pattern::resolve::ResolvedPattern;
use crate::target_aliases::BuckConfigTargetAliasResolver;
use crate::target_aliases::HasTargetAliasResolver;

#[derive(bz_error::Error, Debug)]
#[buck2(input)]
enum PathAsTargetError {
    #[error("couldn't determine target from filename '{0}'")]
    CannotDetermineTargetFromFilename(String),
}

struct PatternParser {
    cell_resolver: CellResolver,
    cell_alias_resolver: CellAliasResolver,
    cwd: CellPath,
    target_alias_resolver: BuckConfigTargetAliasResolver,
}

impl PatternParser {
    async fn new(
        ctx: &mut DiceComputations<'_>,
        cwd: &ProjectRelativePath,
    ) -> bz_error::Result<Self> {
        let cell_resolver = ctx.get_cell_resolver().await?;

        let cwd = cell_resolver.get_cell_path(&cwd);
        let cell_name = cwd.cell();

        let target_alias_resolver = ctx.target_alias_resolver().await?;
        let cell_alias_resolver = ctx.get_cell_alias_resolver(cell_name).await?;

        Ok(Self {
            cell_resolver,
            cell_alias_resolver,
            cwd,
            target_alias_resolver,
        })
    }

    async fn parse_pattern<T: PatternType>(
        &self,
        file_ops: &dyn FileOps,
        pattern: &str,
    ) -> bz_error::Result<ParsedPattern<T>> {
        let pattern_with_modifiers = self.parse_pattern_with_modifiers(file_ops, pattern).await?;
        let ParsedPatternWithModifiers {
            parsed_pattern,
            modifiers,
        } = pattern_with_modifiers;

        match modifiers.as_slice() {
            None => Ok(parsed_pattern),
            Some(_) => Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "The ?modifier syntax is unsupported for this command"
            )),
        }
    }

    async fn parse_pattern_with_modifiers<T: PatternType>(
        &self,
        file_ops: &dyn FileOps,
        pattern: &str,
    ) -> bz_error::Result<ParsedPatternWithModifiers<T>> {
        if let Some(pattern) = self
            .parse_path_as_target_pattern(file_ops, pattern)
            .await
            .with_buck_error_context(|| format!("Parsing target pattern `{pattern}`"))?
        {
            return Ok(pattern);
        }

        ParsedPatternWithModifiers::parse_relaxed(
            &self.target_alias_resolver,
            self.cwd.as_ref(),
            pattern,
            &self.cell_resolver,
            &self.cell_alias_resolver,
        )
    }

    async fn parse_path_as_target_pattern<T: PatternType>(
        &self,
        file_ops: &dyn FileOps,
        pattern: &str,
    ) -> bz_error::Result<Option<ParsedPatternWithModifiers<T>>> {
        let lex = lex_target_pattern(pattern, true)?;
        let PatternParts {
            cell_alias,
            pattern,
        } = lex;

        if cell_alias.is_some() {
            return Ok(None);
        }

        let PatternDataOrAmbiguous::Ambiguous {
            pattern: path,
            strip_package_trailing_slash,
            extra,
            modifiers,
        } = pattern
        else {
            return Ok(None);
        };

        if self.target_alias_resolver.get(path)?.is_some() {
            return Ok(None);
        }

        let path = if strip_package_trailing_slash {
            path.strip_suffix('/').unwrap_or(path)
        } else {
            path
        };
        let path = self.cwd.join_normalized(RelativePath::new(path))?;

        resolve_path_as_target(file_ops, path, extra, modifiers)
            .await
            .map(Some)
    }
}

async fn resolve_path_as_target<T: PatternType>(
    file_ops: &dyn FileOps,
    path: CellPath,
    extra: T,
    modifiers: Modifiers,
) -> bz_error::Result<ParsedPatternWithModifiers<T>> {
    let (package, target_name) = if is_package(file_ops, path.as_ref()).await? {
        let target_name = path.path().file_name().ok_or_else(|| {
            PathAsTargetError::CannotDetermineTargetFromFilename(path.to_string())
        })?;
        (
            PackageLabel::from_cell_path(path.as_ref())?,
            TargetName::new_bazel(target_name.as_str())?,
        )
    } else {
        let mut package_path = path.parent();
        let mut package_and_target = None;

        while let Some(package_path_value) = package_path {
            if is_package(file_ops, package_path_value).await? {
                let target_name = path.strip_prefix(package_path_value)?;
                package_and_target = Some((
                    PackageLabel::from_cell_path(package_path_value)?,
                    TargetName::new_bazel(target_name.as_str())?,
                ));
                break;
            }
            package_path = package_path_value.parent();
        }

        package_and_target
            .ok_or_else(|| PathAsTargetError::CannotDetermineTargetFromFilename(path.to_string()))?
    };

    Ok(ParsedPatternWithModifiers {
        parsed_pattern: ParsedPattern::Target(package, target_name, extra),
        modifiers,
    })
}

async fn is_package(file_ops: &dyn FileOps, path: CellPathRef<'_>) -> bz_error::Result<bool> {
    let listing = match file_ops.read_dir(path).await {
        Ok(listing) => listing.included,
        Err(_) => return Ok(false),
    };

    let buildfiles = file_ops.buildfiles(path.cell()).await?;
    Ok(find_buildfile(&buildfiles, &listing).is_some())
}

/// Parse target patterns out of command line arguments.
///
/// The format allowed here is more relaxed than in build files and elsewhere, so only use this
/// with strings passed by the user on the CLI.
/// See `ParsedPattern::parse_relaxed` for details.
pub async fn parse_patterns_from_cli_args<T: PatternType>(
    ctx: &mut DiceComputations<'_>,
    target_patterns: &[String],
    cwd: &ProjectRelativePath,
) -> bz_error::Result<Vec<ParsedPattern<T>>> {
    let parser = PatternParser::new(ctx, cwd).await?;

    ctx.with_linear_recompute(|ctx| async move {
        let file_ops = DiceFileOps(&ctx);
        let mut parsed = Vec::with_capacity(target_patterns.len());
        for value in target_patterns {
            parsed.push(parser.parse_pattern(&file_ops, value).await?);
        }
        bz_error::Ok(parsed)
    })
    .await
}

pub async fn parse_patterns_with_modifiers_from_cli_args<T: PatternType>(
    ctx: &mut DiceComputations<'_>,
    target_patterns: &[String],
    cwd: &ProjectRelativePath,
) -> bz_error::Result<Vec<ParsedPatternWithModifiers<T>>> {
    let parser = PatternParser::new(ctx, cwd).await?;

    ctx.with_linear_recompute(|ctx| async move {
        let file_ops = DiceFileOps(&ctx);
        let mut parsed = Vec::with_capacity(target_patterns.len());
        for value in target_patterns {
            parsed.push(
                parser
                    .parse_pattern_with_modifiers(&file_ops, value)
                    .await?,
            );
        }
        bz_error::Ok(parsed)
    })
    .await
}

pub async fn parse_patterns_from_cli_args_typed<T: PatternType>(
    ctx: &mut DiceComputations<'_>,
    patterns: &UnparsedPatterns<T>,
) -> bz_error::Result<Vec<ParsedPattern<T>>> {
    parse_patterns_from_cli_args(ctx, patterns.patterns(), patterns.working_dir()).await
}

pub async fn parse_and_resolve_patterns_from_cli_args<T: PatternType>(
    ctx: &mut DiceComputations<'_>,
    target_patterns: &[String],
    cwd: &ProjectRelativePath,
) -> bz_error::Result<ResolvedPattern<T>> {
    let patterns = parse_patterns_from_cli_args(ctx, target_patterns, cwd).await?;
    ResolveTargetPatterns::resolve(ctx, &patterns).await
}

pub async fn parse_and_resolve_patterns_with_modifiers_from_cli_args<T: PatternType>(
    ctx: &mut DiceComputations<'_>,
    target_patterns: &[String],
    cwd: &ProjectRelativePath,
) -> bz_error::Result<ResolvedPattern<T>> {
    let patterns = parse_patterns_with_modifiers_from_cli_args(ctx, target_patterns, cwd).await?;
    ResolveTargetPatterns::resolve_with_modifiers(ctx, &patterns).await
}
