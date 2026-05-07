/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::convert::Infallible;
use std::fmt::Display;
use std::hash::Hash;
use std::hash::Hasher;

use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_core::cells::external::external_cell_origin_for_cell;
use buck2_core::deferred::base_deferred_key::BaseDeferredKey;
use buck2_core::fs::buck_out_path::BuckOutPathResolver;
use buck2_core::fs::buck_out_path::current_bazel_artifact_buck_out_path;
use buck2_core::provider::label::ProvidersLabel;
use buck2_execute::path::artifact_path::ArtifactPath;
use buck2_fs::paths::file_name::FileName;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
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

pub trait StarlarkArtifactLike<'v>: Display {
    fn with_filename(
        &self,
        f: &dyn for<'b> Fn(&'b FileName) -> StringValue<'v>,
    ) -> buck2_error::Result<StringValue<'v>>;

    fn is_source(&'v self) -> buck2_error::Result<bool>;

    fn is_directory(&'v self) -> buck2_error::Result<bool> {
        Ok(false)
    }

    fn is_symlink(&'v self) -> buck2_error::Result<bool> {
        Ok(false)
    }

    fn owner(&'v self) -> buck2_error::Result<Option<BaseDeferredKey>>;

    fn source_owner(&'v self) -> buck2_error::Result<Option<ProvidersLabel>> {
        Ok(None)
    }

    fn with_short_path(
        &self,
        f: &dyn for<'b> Fn(&'b ForwardRelativePath) -> StringValue<'v>,
    ) -> buck2_error::Result<StringValue<'v>>;

    fn with_bazel_path(
        &self,
        f: &dyn Fn(&str) -> StringValue<'v>,
    ) -> buck2_error::Result<StringValue<'v>>;

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
        Either::Left(build) => {
            let buck_out_path_resolver =
                BuckOutPathResolver::new(current_bazel_artifact_buck_out_path());
            buck_out_path_resolver
                .resolve_gen_configuration_hash_path(build)
                .expect("Bazel artifact output path should resolve")
                .join(path.projected_path)
                .to_string()
        }
        Either::Right(source) => {
            let package = source.package();
            let cell = package.cell_name();
            if let Some(origin) = external_cell_origin_for_cell(cell.as_str()) {
                let buck_out_path_resolver =
                    BuckOutPathResolver::new(current_bazel_artifact_buck_out_path());
                return buck_out_path_resolver
                    .resolve_external_cell_source(source.to_cell_path().path(), origin)
                    .join(path.projected_path)
                    .to_string();
            }

            let source_path = package
                .cell_relative_path()
                .as_forward_relative_path()
                .join(source.path().as_forward_rel_path());
            let source_path = source_path.join_cow(path.projected_path);
            if cell.as_str() == "root" {
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
    fn as_output_error(&self) -> buck2_error::Error;

    /// Gets the bound main artifact, or errors if the artifact is not bound
    fn get_bound_artifact(&self) -> buck2_error::Result<Artifact>;

    /// Gets any associated artifacts that should be materialized along with the bound artifact
    fn get_associated_artifacts(&self) -> Option<&AssociatedArtifacts>;

    /// Return an interface for frozen and bound artifacts (`StarlarkArtifact`) to add to a CLI
    ///
    /// Returns None if this artifact isn't the correct type to be added to a CLI object
    fn as_command_line_like(&self) -> &dyn CommandLineArgLike<'v>;

    /// Gets a copy of the StarlarkArtifact, ensuring that the artifact is bound.
    fn get_bound_starlark_artifact(&self) -> buck2_error::Result<StarlarkArtifact> {
        let artifact = self.get_bound_artifact()?;
        let associated_artifacts = self.get_associated_artifacts();
        Ok(StarlarkArtifact {
            artifact,
            associated_artifacts: associated_artifacts
                .map_or(AssociatedArtifacts::new(), |a| a.clone()),
        })
    }

    /// Gets the artifact group.
    fn get_artifact_group(&self) -> buck2_error::Result<ArtifactGroup>;

    fn as_output(&'v self, this: Value<'v>) -> buck2_error::Result<StarlarkOutputArtifact<'v>>;

    fn project(
        &'v self,
        path: &ForwardRelativePath,
        hide_prefix: bool,
    ) -> buck2_error::Result<EitherStarlarkInputArtifact<'v>>;

    fn without_associated_artifacts(
        &'v self,
    ) -> buck2_error::Result<EitherStarlarkInputArtifact<'v>>;

    fn with_associated_artifacts(
        &'v self,
        artifacts: UnpackList<ValueAsInputArtifactLike<'v>>,
    ) -> buck2_error::Result<EitherStarlarkInputArtifact<'v>>;
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
