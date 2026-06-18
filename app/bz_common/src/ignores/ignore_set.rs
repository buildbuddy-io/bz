/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use allocative::Allocative;
use bz_core::cells::paths::CellRelativePath;
use bz_error::internal_error;
use globset::Candidate;
use globset::GlobSetBuilder;
use once_cell::sync::Lazy;
use pagable::PagableDeserialize;
use pagable::PagableDeserializer;
use pagable::PagableSerialize;
use pagable::PagableSerializer;
use regex::Regex;

#[derive(Debug, Clone, Allocative)]
pub struct IgnoreSet {
    #[allocative(skip)]
    globset: globset::GlobSet,
    // Patterns that were added to the globset. Storing this seprately to support ser/de and
    // so that error messages can refer to the specific pattern that was matched.
    // This should be in the same order as the strings were added to the GlobSet to match the indices returned from it.
    patterns: Vec<String>,
}

impl PagableSerialize for IgnoreSet {
    fn pagable_serialize(&self, serializer: &mut dyn PagableSerializer) -> pagable::Result<()> {
        self.patterns.pagable_serialize(serializer)
    }
}

impl<'de> PagableDeserialize<'de> for IgnoreSet {
    fn pagable_deserialize<D: PagableDeserializer<'de> + ?Sized>(
        deserializer: &mut D,
    ) -> pagable::Result<Self> {
        let patterns = Vec::<String>::pagable_deserialize(deserializer)?;
        let globset = Self::build_globset(&patterns)
            .map_err(|e| pagable::Error::new(e).context("rebuilding IgnoreSet globset"))?;
        Ok(Self { globset, patterns })
    }
}

impl PartialEq for IgnoreSet {
    fn eq(&self, other: &Self) -> bool {
        // Only compare patterns because globset is derived from patterns.
        self.patterns == other.patterns
    }
}

impl Eq for IgnoreSet {}

impl IgnoreSet {
    /// Creates an IgnoreSet from an "ignore spec".
    ///
    /// This is modeled after buck1's parsing of project.ignores.
    ///
    /// An ignore spec is a comma-separated list of ignore patterns. If an ignore pattern
    /// contains a glob character, then it uses java.nio.file.FileSystem.getPathMatcher,
    /// otherwise it creates a com.facebook.buck.io.filesystem.RecursivePathMatcher
    ///
    /// Java's path matcher does not allow  '*' to cross directory boundaries. We get
    /// the RecursivePathMatcher behavior by identifying non-globby things and appending
    /// a '/**'.
    ///
    /// Always ignores build output directories if it is a `root_cell`.
    pub fn from_ignore_spec(spec: &str, root_cell: bool) -> bz_error::Result<Self> {
        // TODO(cjhopman): There's opportunity to greatly improve the performance of IgnoreSet by
        // constructing special cases for a couple of common patterns we see in ignore specs. We
        // know that these can get large wins in some places where we've done this same ignore (watchman, buck1's ignores).
        // `**/filename`: a filename filter. These can all be merged into one hashset lookup.
        // `**/*.ext`: an extension filter. These can all be merged into one hashset lookup.
        // `**/*x*x*`: just some general glob on the filename alone, can merge these into one GlobSet that just needs to check against the filename.
        // `some/prefix/**`: a directory prefix. These can all be merged into one trie lookup.
        let mut patterns = Vec::new();
        let root_output_dirs = root_cell
            .then_some(["buck-out", "bazel-*", "bazel-*/**"])
            .into_iter()
            .flatten();
        for val in root_output_dirs.chain(spec.split(',')) {
            let val = val.trim();
            if val.is_empty() {
                continue;
            }

            let val = val.trim_end_matches('/');
            patterns.push(val.to_owned());
        }

        let globset = Self::build_globset(&patterns).map_err(|e| internal_error!("{}", e))?;

        Ok(Self { globset, patterns })
    }

    /// Build a `GlobSet` from the given patterns.
    ///
    /// Glob-containing patterns use `literal_separator(true)`, while plain
    /// directory names are turned into `{name,name/**}` matchers.
    fn build_globset(patterns: &[String]) -> Result<globset::GlobSet, globset::Error> {
        static GLOB_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r"[*?{\[]").unwrap());

        let mut builder = GlobSetBuilder::new();
        for val in patterns {
            if GLOB_CHARS.is_match(val) {
                builder.add(
                    globset::GlobBuilder::new(val)
                        .literal_separator(true)
                        .build()?,
                );
            } else {
                builder.add(globset::Glob::new(&format!("{{{val},{val}/**}}"))?);
            }
        }
        builder.build()
    }

    /// Returns a pattern that matches the candidate if there is one.
    pub(crate) fn matches_candidate(&self, candidate: &Candidate) -> Option<&str> {
        match self.globset.matches_candidate(candidate).as_slice() {
            [] => None,
            [v, ..] => Some(&self.patterns[*v]),
        }
    }

    /// Returns whether any pattern matches.
    pub fn is_match(&self, path: &CellRelativePath) -> bool {
        self.globset.is_match(path.as_str())
    }
}

pub fn bazelignore_to_ignore_spec(contents: &str) -> bz_error::Result<String> {
    let mut patterns = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('/') {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                ".bazelignore line {} must be relative, got `{}`",
                index + 1,
                line
            ));
        }
        patterns.push(line.to_owned());
    }
    Ok(patterns.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ignore_set_defaults() {
        let set = IgnoreSet::from_ignore_spec("", true).unwrap();
        assert!(set.is_match(CellRelativePath::testing_new("buck-out/gen/src/file.txt")));
        assert!(set.is_match(CellRelativePath::testing_new("buck-out/art/src/file.txt")));
        assert!(set.is_match(CellRelativePath::testing_new("bazel-bin")));
        assert!(set.is_match(CellRelativePath::testing_new("bazel-bin/app/file")));
        assert!(set.is_match(CellRelativePath::testing_new(
            "bazel-out/darwin-fastbuild/bin"
        )));
        assert!(set.is_match(CellRelativePath::testing_new(
            "bazel-my_workspace/external/repo"
        )));
        assert!(!set.is_match(CellRelativePath::testing_new("src/file.txt")));
    }

    #[test]
    fn test_bazelignore_to_ignore_spec() {
        let spec = bazelignore_to_ignore_spec(
            r#"
# comment
node_modules
generated_site/build/

"#,
        )
        .unwrap();
        let set = IgnoreSet::from_ignore_spec(&spec, false).unwrap();
        assert!(set.is_match(CellRelativePath::testing_new("node_modules/pkg/index.js")));
        assert!(set.is_match(CellRelativePath::testing_new(
            "generated_site/build/index.html"
        )));
        assert!(!set.is_match(CellRelativePath::testing_new("generated_site/src/index.ts")));
    }

    #[test]
    fn test_bazelignore_rejects_absolute_paths() {
        assert!(bazelignore_to_ignore_spec("/tmp").is_err());
    }
}
