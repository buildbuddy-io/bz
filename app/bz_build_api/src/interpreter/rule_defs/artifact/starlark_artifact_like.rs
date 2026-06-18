/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::borrow::Cow;
use std::convert::Infallible;
use std::fmt::Display;
use std::hash::Hash;
use std::hash::Hasher;

use bz_artifact::artifact::artifact_type::Artifact;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use bz_core::cells::external::external_cell_origin_for_cell;
use bz_core::deferred::base_deferred_key::BaseDeferredKey;
use bz_core::fs::buck_out_path::BazelOutputPathKind;
use bz_core::fs::buck_out_path::BazelOutputRoot;
use bz_core::provider::label::ProvidersLabel;
use bz_core::target::configured_target_label::ConfiguredTargetLabel;
use bz_execute::path::artifact_path::ArtifactPath;
use bz_fs::paths::file_name::FileName;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use either::Either;
use starlark::collections::StarlarkHasher;
use starlark::typing::Ty;
use starlark::values::StringValue;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueTypedComplex;
use starlark::values::list::UnpackList;
use starlark::values::type_repr::StarlarkTypeRepr;

use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::promise::PromiseArtifactId;
use crate::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use crate::interpreter::rule_defs::artifact::methods::EitherStarlarkInputArtifact;
use crate::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use crate::interpreter::rule_defs::artifact::starlark_output_artifact::StarlarkOutputArtifact;
use crate::interpreter::rule_defs::artifact::starlark_promise_artifact::StarlarkPromiseArtifact;
use crate::interpreter::rule_defs::cmd_args::CommandLineArgLike;

fn bazel_external_repo_name<'a>(cell: &'a str, origin: &'a ExternalCellOrigin) -> &'a str {
    match origin {
        ExternalCellOrigin::Bundled(cell) => cell.as_str(),
        ExternalCellOrigin::Git(_) => cell,
        ExternalCellOrigin::Bzlmod(setup) => setup.canonical_repo_name.as_ref(),
        ExternalCellOrigin::BzlmodGenerated(setup) => setup.canonical_repo_name.as_ref(),
    }
}

fn bazel_cell_is_main(cell: &str) -> bool {
    cell == "root" || bzlmod_canonical_repo_name_for_cell(cell).is_some_and(|repo| repo.is_empty())
}

fn push_bazel_path_component(path: &mut String, component: &str) {
    if component.is_empty() {
        return;
    }
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(component);
}

fn bazel_external_cells_exec_path(path: &str) -> Option<String> {
    let (_, external_cells_path) = path.split_once("/external_cells/")?;
    let repo_path = external_cells_path
        .strip_prefix("bzlmod_generated/")
        .or_else(|| external_cells_path.strip_prefix("bzlmod/"))?;
    let (repo, external_path) = repo_path
        .split_once('/')
        .map_or((repo_path, ""), |(repo, path)| (repo, path));
    if repo.is_empty() {
        return None;
    }

    if external_path.is_empty() {
        Some(format!("external/{repo}"))
    } else {
        Some(format!("external/{repo}/{external_path}"))
    }
}

fn bazel_external_cells_runfiles_path(path: &str) -> Option<String> {
    let exec_path = bazel_external_cells_exec_path(path)?;
    let external_path = exec_path.strip_prefix("external/")?;
    Some(format!("../{external_path}"))
}

fn bazel_path_delimiter(c: char) -> bool {
    matches!(c, '=' | ':' | ',' | ' ' | '\t' | '"' | '\'')
}

pub fn bazel_normalize_external_cells_exec_paths(value: &str) -> String {
    const MARKER: &str = "/external_cells/";

    let Some(_) = value.find(MARKER) else {
        return value.to_owned();
    };

    let mut result = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(marker_offset) = value[cursor..].find(MARKER) {
        let marker = cursor + marker_offset;
        let path_start = value[cursor..marker]
            .rfind(bazel_path_delimiter)
            .map_or(cursor, |index| cursor + index + 1);
        let after_marker = &value[marker + MARKER.len()..];
        let Some(repo_path) = after_marker
            .strip_prefix("bzlmod_generated/")
            .or_else(|| after_marker.strip_prefix("bzlmod/"))
        else {
            result.push_str(&value[cursor..marker + MARKER.len()]);
            cursor = marker + MARKER.len();
            continue;
        };

        let path_len = repo_path
            .find(bazel_path_delimiter)
            .unwrap_or(repo_path.len());
        let path = &repo_path[..path_len];
        let (repo, external_path) = path
            .split_once('/')
            .map_or((path, ""), |(repo, path)| (repo, path));
        if repo.is_empty() {
            result.push_str(&value[cursor..marker + MARKER.len()]);
            cursor = marker + MARKER.len();
            continue;
        }

        result.push_str(&value[cursor..path_start]);
        if external_path.is_empty() {
            result.push_str("external/");
            result.push_str(repo);
        } else {
            result.push_str("external/");
            result.push_str(repo);
            result.push('/');
            result.push_str(external_path);
        }
        cursor = marker + MARKER.len() + after_marker.len() - repo_path.len() + path_len;
    }
    result.push_str(&value[cursor..]);
    result
}

fn bazel_normalize_root_artifact_exec_path(path: &str) -> Option<String> {
    const MARKER: &str = "/art/root/";
    let (_, artifact_path) = path.split_once(MARKER)?;
    let (configuration, output_path) = artifact_path.split_once('/')?;
    if configuration.is_empty() || output_path.is_empty() {
        return None;
    }

    let mut exec_path = format!("buck-out/bin/{configuration}");
    let mut has_components = false;
    for component in output_path
        .split('/')
        .filter(|component| !bazel_hidden_path_component(component))
    {
        has_components = true;
        exec_path.push('/');
        exec_path.push_str(component);
    }
    if !has_components {
        return None;
    }
    Some(exec_path)
}

pub fn bazel_normalize_root_artifact_exec_paths(value: &str) -> String {
    const MARKER: &str = "/art/root/";

    let Some(_) = value.find(MARKER) else {
        return value.to_owned();
    };

    let mut result = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(marker_offset) = value[cursor..].find(MARKER) {
        let marker = cursor + marker_offset;
        let path_start = value[cursor..marker]
            .rfind(bazel_path_delimiter)
            .map_or(cursor, |index| cursor + index + 1);
        let after_marker = &value[marker + MARKER.len()..];
        let path_len = after_marker
            .find(bazel_path_delimiter)
            .unwrap_or(after_marker.len());
        let path_end = marker + MARKER.len() + path_len;
        let Some(path) = bazel_normalize_root_artifact_exec_path(&value[path_start..path_end])
        else {
            result.push_str(&value[cursor..marker + MARKER.len()]);
            cursor = marker + MARKER.len();
            continue;
        };

        result.push_str(&value[cursor..path_start]);
        result.push_str(&path);
        cursor = path_end;
    }
    result.push_str(&value[cursor..]);
    result
}

fn bazel_normalize_output_artifact_exec_path(
    path: &str,
    marker_text: &str,
    output_root: BazelOutputRoot,
) -> Option<String> {
    let (_, artifact_path) = path.split_once(marker_text)?;
    let (configuration, output_path) = artifact_path.split_once('/')?;
    if configuration.is_empty() || output_path.is_empty() {
        return None;
    }

    let mut changed = false;
    let mut has_components = false;
    let mut exec_path = format!("{}/{configuration}", output_root.exec_root());
    for component in output_path.split('/') {
        if bazel_hidden_path_component(component) {
            changed = true;
            continue;
        }
        has_components = true;
        exec_path.push('/');
        exec_path.push_str(component);
    }
    if !changed || !has_components {
        return None;
    }
    Some(exec_path)
}

fn bazel_normalize_output_artifact_exec_paths(
    value: &str,
    marker_text: &str,
    output_root: BazelOutputRoot,
) -> String {
    let Some(_) = value.find(marker_text) else {
        return value.to_owned();
    };

    let mut result = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(marker_offset) = value[cursor..].find(marker_text) {
        let marker_start = cursor + marker_offset;
        let path_start = value[cursor..marker_start]
            .rfind(bazel_path_delimiter)
            .map_or(cursor, |index| cursor + index + 1);
        let after_marker = &value[marker_start + marker_text.len()..];
        let path_len = after_marker
            .find(bazel_path_delimiter)
            .unwrap_or(after_marker.len());
        let path_end = marker_start + marker_text.len() + path_len;
        let Some(path) = bazel_normalize_output_artifact_exec_path(
            &value[path_start..path_end],
            marker_text,
            output_root,
        ) else {
            result.push_str(&value[cursor..marker_start + marker_text.len()]);
            cursor = marker_start + marker_text.len();
            continue;
        };

        result.push_str(&value[cursor..path_start]);
        result.push_str(&path);
        cursor = path_end;
    }
    result.push_str(&value[cursor..]);
    result
}

pub fn bazel_normalize_bin_artifact_exec_paths(value: &str) -> String {
    bazel_normalize_output_artifact_exec_paths(value, "buck-out/bin/", BazelOutputRoot::Bin)
}

pub fn bazel_normalize_genfiles_artifact_exec_paths(value: &str) -> String {
    bazel_normalize_output_artifact_exec_paths(
        value,
        "buck-out/genfiles/",
        BazelOutputRoot::Genfiles,
    )
}

pub fn bazel_normalize_buck_owned_exec_paths(value: &str) -> String {
    let mut value = Cow::Borrowed(value);
    if value.contains("/external_cells/") {
        value = Cow::Owned(bazel_normalize_external_cells_exec_paths(&value));
    }
    if value.contains("/art/root/") {
        value = Cow::Owned(bazel_normalize_root_artifact_exec_paths(&value));
    }
    if value.contains("buck-out/bin/") {
        value = Cow::Owned(bazel_normalize_bin_artifact_exec_paths(&value));
    }
    if value.contains("buck-out/genfiles/") {
        value = Cow::Owned(bazel_normalize_genfiles_artifact_exec_paths(&value));
    }
    value.into_owned()
}

fn bazel_configuration_exec_path(label: &ConfiguredTargetLabel) -> String {
    let mut path = label.cfg().output_hash().as_str().to_owned();
    if let Some(exec_cfg) = label.exec_cfg() {
        path.push('-');
        path.push_str(exec_cfg.output_hash().as_str());
    }
    path
}

fn bazel_build_artifact_owner_label(path: &ArtifactPath<'_>) -> Option<ConfiguredTargetLabel> {
    let Either::Left(build) = path.base_path.as_ref() else {
        return None;
    };
    build
        .bazel_owner()
        .cloned()
        .or_else(|| build.owner().owner().configured_label())
}

pub fn bazel_artifact_owner(path: ArtifactPath<'_>) -> Option<BaseDeferredKey> {
    bazel_artifact_owner_ref(&path)
}

fn bazel_artifact_owner_ref(path: &ArtifactPath<'_>) -> Option<BaseDeferredKey> {
    match path.base_path.as_ref() {
        Either::Left(_) => bazel_build_artifact_owner_label(path).map(BaseDeferredKey::TargetLabel),
        Either::Right(_) => None,
    }
}

fn bazel_package_exec_path(owner: &BaseDeferredKey) -> String {
    let Some(label) = owner.configured_label() else {
        return String::new();
    };
    bazel_label_package_exec_path(&label)
}

fn bazel_label_package_exec_path(label: &ConfiguredTargetLabel) -> String {
    let package = label.pkg();
    let cell = package.cell_name();
    let package_path = package.cell_relative_path();
    let mut path = String::new();
    if let Some(origin) = external_cell_origin_for_cell(cell.as_str()) {
        push_bazel_path_component(&mut path, "external");
        push_bazel_path_component(&mut path, bazel_external_repo_name(cell.as_str(), &origin));
    } else if !bazel_cell_is_main(cell.as_str()) {
        push_bazel_path_component(&mut path, cell.as_str());
    }
    push_bazel_path_component(&mut path, package_path.as_str());
    path
}

fn bazel_package_runfiles_path(owner: &BaseDeferredKey) -> String {
    let Some(label) = owner.configured_label() else {
        return String::new();
    };
    let package = label.pkg();
    let cell = package.cell_name();
    let package_path = package.cell_relative_path();
    let mut path = String::new();
    if let Some(origin) = external_cell_origin_for_cell(cell.as_str()) {
        push_bazel_path_component(&mut path, "..");
        push_bazel_path_component(&mut path, bazel_external_repo_name(cell.as_str(), &origin));
    } else if !bazel_cell_is_main(cell.as_str()) {
        push_bazel_path_component(&mut path, cell.as_str());
    }
    push_bazel_path_component(&mut path, package_path.as_str());
    path
}

fn bazel_path_has_package_prefix(path: &str, package_exec_path: &str) -> bool {
    !package_exec_path.is_empty()
        && (path == package_exec_path
            || path
                .strip_prefix(package_exec_path)
                .is_some_and(|path| path.starts_with('/')))
}

fn bazel_output_dir_relative_runfiles_path(path: String) -> String {
    if let Some(path) = path.strip_prefix("external/") {
        format!("../{path}")
    } else {
        path
    }
}

fn bazel_hidden_path_component(component: &str) -> bool {
    component.starts_with("__") && component.ends_with("__")
}

fn bazel_visible_build_artifact_short_path(path: &ArtifactPath<'_>) -> String {
    let Either::Left(build) = path.base_path.as_ref() else {
        unreachable!("called with a source artifact path")
    };
    let base_short_path = build.path().join_cow(path.projected_path);
    let Some(short_path) = base_short_path.strip_prefix_components(path.hidden_components_count)
    else {
        return String::new();
    };

    let mut result = String::new();
    for component in short_path
        .iter()
        .filter(|component| !bazel_hidden_path_component(component.as_str()))
    {
        push_bazel_path_component(&mut result, component.as_str());
    }
    result
}

fn bazel_visible_declared_output_short_path(path: &ForwardRelativePath) -> String {
    let mut result = String::new();
    for component in path
        .iter()
        .filter(|component| !bazel_hidden_path_component(component.as_str()))
    {
        push_bazel_path_component(&mut result, component.as_str());
    }
    result
}

pub fn bazel_declared_output_artifact_path(
    path: &ForwardRelativePath,
    owner: Option<&ConfiguredTargetLabel>,
    output_root: BazelOutputRoot,
    output_path_kind: BazelOutputPathKind,
) -> String {
    let short_path = bazel_visible_declared_output_short_path(path);
    let mut exec_path = output_root.exec_root().to_owned();
    if let Some(owner) = owner {
        push_bazel_path_component(&mut exec_path, &bazel_configuration_exec_path(owner));
    }
    if output_path_kind == BazelOutputPathKind::PackageRelative {
        if let Some(owner) = owner {
            let package_exec_path = bazel_label_package_exec_path(owner);
            if !bazel_path_has_package_prefix(&short_path, &package_exec_path) {
                push_bazel_path_component(&mut exec_path, &package_exec_path);
            }
        }
    }
    push_bazel_path_component(&mut exec_path, &short_path);
    exec_path
}

fn bazel_build_artifact_path(path: ArtifactPath<'_>) -> String {
    let Either::Left(build) = path.base_path.as_ref() else {
        unreachable!("called with a source artifact path")
    };
    let short_path = bazel_visible_build_artifact_short_path(&path);
    let mut exec_path = build.bazel_output_root().exec_root().to_owned();
    if let Some(label) = bazel_build_artifact_owner_label(&path) {
        push_bazel_path_component(&mut exec_path, &bazel_configuration_exec_path(&label));
    }
    if build.bazel_output_path_kind() == BazelOutputPathKind::PackageRelative {
        if let Some(owner) = bazel_artifact_owner_ref(&path) {
            let package_exec_path = bazel_package_exec_path(&owner);
            if !bazel_path_has_package_prefix(&short_path, &package_exec_path) {
                push_bazel_path_component(&mut exec_path, &package_exec_path);
            }
        }
    }
    push_bazel_path_component(&mut exec_path, &short_path);
    exec_path
}

fn bazel_build_artifact_short_path(path: ArtifactPath<'_>) -> String {
    let Either::Left(build) = path.base_path.as_ref() else {
        unreachable!("called with a source artifact path")
    };
    let short_path = bazel_visible_build_artifact_short_path(&path);
    if build.bazel_output_path_kind() == BazelOutputPathKind::OutputDirRelative {
        return bazel_output_dir_relative_runfiles_path(short_path);
    }
    let Some(owner) = bazel_artifact_owner_ref(&path) else {
        return short_path;
    };
    let package_exec_path = bazel_package_exec_path(&owner);
    if bazel_path_has_package_prefix(&short_path, &package_exec_path) {
        return bazel_output_dir_relative_runfiles_path(short_path);
    }
    let mut runfiles_path = bazel_package_runfiles_path(&owner);
    push_bazel_path_component(&mut runfiles_path, &short_path);
    runfiles_path
}

pub trait StarlarkArtifactLike<'v>: Display {
    fn with_filename(
        &self,
        f: &dyn for<'b> Fn(&'b FileName) -> StringValue<'v>,
    ) -> bz_error::Result<StringValue<'v>>;

    fn is_source(&'v self) -> bz_error::Result<bool>;

    fn is_directory(&'v self) -> bz_error::Result<bool> {
        Ok(false)
    }

    fn is_symlink(&'v self) -> bz_error::Result<bool> {
        Ok(false)
    }

    fn owner(&'v self) -> bz_error::Result<Option<BaseDeferredKey>>;

    fn source_owner(&'v self) -> bz_error::Result<Option<ProvidersLabel>> {
        Ok(None)
    }

    fn with_short_path(
        &self,
        f: &dyn for<'b> Fn(&'b ForwardRelativePath) -> StringValue<'v>,
    ) -> bz_error::Result<StringValue<'v>>;

    fn with_bazel_short_path(
        &self,
        f: &dyn Fn(&str) -> StringValue<'v>,
    ) -> bz_error::Result<StringValue<'v>> {
        self.with_short_path(&|path| f(path.as_str()))
    }

    fn with_bazel_path(
        &self,
        f: &dyn Fn(&str) -> StringValue<'v>,
    ) -> bz_error::Result<StringValue<'v>>;

    /// It's very important that the Hash/Eq of the StarlarkArtifactLike things doesn't change
    /// during freezing, otherwise Starlark invariants are broken. Use the fingerprint
    /// as the inputs to Hash/Eq to ensure they are consistent
    fn fingerprint<'s>(&'s self) -> ArtifactFingerprint<'s>
    where
        'v: 's;

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(<&dyn StarlarkArtifactLike<'v>>::unpack_value(other)?
            .is_some_and(|other| self.fingerprint() == other.fingerprint()))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.fingerprint().hash(hasher);
        Ok(())
    }
}

pub fn bazel_artifact_path(path: ArtifactPath<'_>) -> String {
    match path.base_path.as_ref() {
        Either::Left(_) => bazel_build_artifact_path(path),
        Either::Right(source) => {
            let package = source.package();
            let cell = package.cell_name();
            if let Some(origin) = external_cell_origin_for_cell(cell.as_str()) {
                let repo = bazel_external_repo_name(cell.as_str(), &origin);
                let source_cell_path = source.to_cell_path();
                let external_path = source_cell_path
                    .path()
                    .as_forward_relative_path()
                    .join_cow(path.projected_path);
                return if external_path.is_empty() {
                    format!("external/{repo}")
                } else {
                    format!("external/{repo}/{external_path}")
                };
            }

            let source_path = package
                .cell_relative_path()
                .as_forward_relative_path()
                .join(source.path().as_forward_rel_path());
            let source_path = source_path.join_cow(path.projected_path);
            if let Some(path) = bazel_external_cells_exec_path(source_path.as_str()) {
                return path;
            }
            if bazel_cell_is_main(cell.as_str()) {
                source_path.to_string()
            } else if source_path.is_empty() {
                cell.as_str().to_owned()
            } else {
                format!("{}/{}", cell.as_str(), source_path)
            }
        }
    }
}

pub fn bazel_artifact_short_path(path: ArtifactPath<'_>) -> String {
    match path.base_path.as_ref() {
        Either::Left(_) => bazel_build_artifact_short_path(path),
        Either::Right(source) => {
            let package = source.package();
            let cell = package.cell_name();
            let source_path = if let Some(origin) = external_cell_origin_for_cell(cell.as_str()) {
                let repo = bazel_external_repo_name(cell.as_str(), &origin);
                let source_cell_path = source.to_cell_path();
                let external_path = source_cell_path
                    .path()
                    .as_forward_relative_path()
                    .join_cow(path.projected_path);
                let mut runfiles_path = String::new();
                push_bazel_path_component(&mut runfiles_path, "..");
                push_bazel_path_component(&mut runfiles_path, repo);
                push_bazel_path_component(&mut runfiles_path, external_path.as_str());
                return runfiles_path;
            } else {
                package
                    .cell_relative_path()
                    .as_forward_relative_path()
                    .join(source.path().as_forward_rel_path())
            };
            let source_path = source_path.join_cow(path.projected_path);
            if let Some(path) = bazel_external_cells_runfiles_path(source_path.as_str()) {
                return path;
            }
            if bazel_cell_is_main(cell.as_str()) {
                source_path.to_string()
            } else if source_path.is_empty() {
                cell.as_str().to_owned()
            } else {
                format!("{}/{}", cell.as_str(), source_path)
            }
        }
    }
}

/// A trait representing starlark representations of input artifacts.
///
/// Not implemented for `OutputArtifact`
pub trait StarlarkInputArtifactLike<'v>: StarlarkArtifactLike<'v> {
    /// Returns an apppropriate error for when this is used in a location that expects an output declaration.
    fn as_output_error(&self) -> bz_error::Error;

    /// Gets the bound main artifact, or errors if the artifact is not bound
    fn get_bound_artifact(&self) -> bz_error::Result<Artifact>;

    /// Gets any associated artifacts that should be materialized along with the bound artifact
    fn get_associated_artifacts(&self) -> Option<&AssociatedArtifacts>;

    fn bound_source_is_directory(&self) -> bool {
        false
    }

    /// Return an interface for frozen and bound artifacts (`StarlarkArtifact`) to add to a CLI
    ///
    /// Returns None if this artifact isn't the correct type to be added to a CLI object
    fn as_command_line_like(&self) -> &dyn CommandLineArgLike<'v>;

    /// Gets a copy of the StarlarkArtifact, ensuring that the artifact is bound.
    fn get_bound_starlark_artifact(&self) -> bz_error::Result<StarlarkArtifact> {
        let artifact = self.get_bound_artifact()?;
        let source_is_directory = artifact.is_source() && self.bound_source_is_directory();
        let associated_artifacts = self.get_associated_artifacts();
        Ok(StarlarkArtifact {
            artifact,
            associated_artifacts: associated_artifacts
                .map_or(AssociatedArtifacts::new(), |a| a.clone()),
            source_is_directory,
        })
    }

    /// Gets the artifact group.
    fn get_artifact_group(&self) -> bz_error::Result<ArtifactGroup>;

    fn as_output(&'v self, this: Value<'v>) -> bz_error::Result<StarlarkOutputArtifact<'v>>;

    fn project(
        &'v self,
        path: &ForwardRelativePath,
        hide_prefix: bool,
    ) -> bz_error::Result<EitherStarlarkInputArtifact<'v>>;

    fn without_associated_artifacts(&'v self) -> bz_error::Result<EitherStarlarkInputArtifact<'v>>;

    fn with_associated_artifacts(
        &'v self,
        artifacts: UnpackList<ValueAsInputArtifactLike<'v>>,
    ) -> bz_error::Result<EitherStarlarkInputArtifact<'v>>;
}

/// Helper type to unpack artifacts.
#[derive(StarlarkTypeRepr, UnpackValue)]
pub enum ValueAsInputArtifactLikeUnpack<'v> {
    Artifact(&'v StarlarkArtifact),
    DeclaredArtifact(&'v StarlarkDeclaredArtifact<'v>),
    PromiseArtifact(&'v StarlarkPromiseArtifact),
}

impl<'v> StarlarkTypeRepr for &'v dyn StarlarkInputArtifactLike<'v> {
    type Canonical = <ValueAsInputArtifactLikeUnpack<'v> as StarlarkTypeRepr>::Canonical;

    fn starlark_type_repr() -> Ty {
        ValueAsInputArtifactLikeUnpack::starlark_type_repr()
    }
}

impl<'v> UnpackValue<'v> for &'v dyn StarlarkInputArtifactLike<'v> {
    type Error = Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        match ValueAsInputArtifactLikeUnpack::unpack_value_opt(value) {
            Some(ValueAsInputArtifactLikeUnpack::Artifact(artifact)) => Ok(Some(artifact)),
            Some(ValueAsInputArtifactLikeUnpack::DeclaredArtifact(artifact)) => Ok(Some(artifact)),
            Some(ValueAsInputArtifactLikeUnpack::PromiseArtifact(artifact)) => Ok(Some(artifact)),
            None => Ok(None),
        }
    }
}

#[derive(UnpackValue, StarlarkTypeRepr)]
pub struct ValueAsInputArtifactLike<'v>(pub &'v dyn StarlarkInputArtifactLike<'v>);

#[derive(StarlarkTypeRepr, UnpackValue)]
pub enum ValueAsArtifactLikeUnpack<'v> {
    OutputArtifact(ValueTypedComplex<'v, StarlarkOutputArtifact<'v>>),
    InputArtifact(&'v dyn StarlarkInputArtifactLike<'v>),
}

impl<'v> StarlarkTypeRepr for &'v dyn StarlarkArtifactLike<'v> {
    type Canonical = <ValueAsArtifactLikeUnpack<'v> as StarlarkTypeRepr>::Canonical;

    fn starlark_type_repr() -> Ty {
        ValueAsArtifactLikeUnpack::starlark_type_repr()
    }
}

impl<'v> UnpackValue<'v> for &'v dyn StarlarkArtifactLike<'v> {
    type Error = Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        match ValueAsArtifactLikeUnpack::unpack_value_opt(value) {
            Some(ValueAsArtifactLikeUnpack::OutputArtifact(artifact)) => match artifact.unpack() {
                Either::Left(artifact) => Ok(Some(artifact)),
                Either::Right(artifact) => Ok(Some(artifact)),
            },
            Some(ValueAsArtifactLikeUnpack::InputArtifact(artifact)) => Ok(Some(artifact)),
            None => Ok(None),
        }
    }
}

/// A helper type that is used in providers and function parameters to mark the type but not
/// otherwise provide a useful unpack implementation.
///
/// This is useful because unlike `ValueAsArtifactLike`, it does not carry a lifetime. See <D?> for
/// some more discussion of why this was necessary.
pub struct ValueIsInputArtifactAnnotation;

impl StarlarkTypeRepr for ValueIsInputArtifactAnnotation {
    type Canonical = <ValueAsInputArtifactLikeUnpack<'static> as StarlarkTypeRepr>::Canonical;

    fn starlark_type_repr() -> Ty {
        ValueAsInputArtifactLikeUnpack::<'static>::starlark_type_repr()
    }
}

impl<'v> UnpackValue<'v> for ValueIsInputArtifactAnnotation {
    type Error = Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        Ok(
            ValueAsInputArtifactLikeUnpack::<'v>::unpack_value_opt(value)
                .map(|_| ValueIsInputArtifactAnnotation),
        )
    }
}

#[derive(PartialEq)]
pub enum ArtifactFingerprint<'a> {
    Normal {
        path: ArtifactPath<'a>,
        associated_artifacts: Option<&'a AssociatedArtifacts>,
        is_output: bool,
    },
    Promise {
        id: PromiseArtifactId,
    },
}

impl Hash for ArtifactFingerprint<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match &self {
            ArtifactFingerprint::Normal {
                path,
                associated_artifacts,
                is_output,
            } => {
                path.hash(state);
                is_output.hash(state);
                if let Some(associated) = associated_artifacts {
                    associated.len().hash(state);
                    associated.iter().for_each(|ag| ag.hash(state));
                }
            }
            ArtifactFingerprint::Promise { id } => id.hash(state),
        }
    }
}
