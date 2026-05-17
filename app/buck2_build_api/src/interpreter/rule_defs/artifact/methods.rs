/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_core::deferred::base_deferred_key::BaseDeferredKey;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersName;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use dupe::Dupe;
use starlark::environment::MethodsBuilder;
use starlark::values::AllocValue;
use starlark::values::Heap;
use starlark::values::StringValue;
use starlark::values::Value;
use starlark::values::ValueOf;
use starlark::values::list::UnpackList;
use starlark::values::structs::AllocStruct;
use starlark::values::type_repr::StarlarkTypeRepr;

use crate::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::StarlarkArtifactLike;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::StarlarkInputArtifactLike;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use crate::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use crate::interpreter::rule_defs::artifact::starlark_output_artifact::StarlarkOutputArtifact;
use crate::interpreter::rule_defs::artifact::starlark_promise_artifact::StarlarkPromiseArtifact;
use crate::interpreter::rule_defs::depset::bazel_depset_from_direct;

fn bazel_root_path(path: &str, short_path: &str) -> String {
    if short_path.is_empty() {
        return path.trim_end_matches('/').to_owned();
    }

    let external_short_path;
    let suffix = if let Some(sibling_path) = short_path.strip_prefix("../") {
        external_short_path = format!("external/{sibling_path}");
        external_short_path.as_str()
    } else {
        short_path
    };

    path.strip_suffix(suffix)
        .map(|root| root.trim_end_matches('/').to_owned())
        .unwrap_or_default()
}

fn artifact_owner_label<'v>(
    this: &'v dyn StarlarkArtifactLike<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Value<'v>> {
    if let Some(owner) = this.source_owner()? {
        return Ok(heap.alloc(StarlarkProvidersLabel::new(owner)));
    }
    match this.owner()? {
        None => Ok(Value::new_none()),
        Some(BaseDeferredKey::TargetLabel(target)) => {
            Ok(heap.alloc(StarlarkConfiguredProvidersLabel::new(
                ConfiguredProvidersLabel::new(target.dupe(), ProvidersName::Default),
            )))
        }
        Some(BaseDeferredKey::AnonTarget(_) | BaseDeferredKey::BxlLabel(_)) => {
            Ok(Value::new_none())
        }
    }
}

#[derive(StarlarkTypeRepr, AllocValue)]
pub enum EitherStarlarkInputArtifact<'v> {
    Artifact(StarlarkArtifact),
    DeclaredArtifact(StarlarkDeclaredArtifact<'v>),
    PromiseArtifact(StarlarkPromiseArtifact),
}

#[starlark_module]
pub(crate) fn any_artifact_methods(builder: &mut MethodsBuilder) {
    /// The base name of this artifact. e.g. for an artifact at `foo/bar`, this is `bar`
    #[starlark(attribute)]
    fn basename<'v>(
        this: &'v dyn StarlarkArtifactLike<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<StringValue<'v>> {
        Ok(this.with_filename(&|filename| heap.alloc_str(filename.as_str()))?)
    }

    /// The execution path of this artifact.
    #[starlark(attribute)]
    fn path<'v>(
        this: &'v dyn StarlarkArtifactLike<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<StringValue<'v>> {
        Ok(this.with_bazel_path(&|path| heap.alloc_str(path))?)
    }

    /// The directory name of this artifact's execution path.
    #[starlark(attribute)]
    fn dirname<'v>(
        this: &'v dyn StarlarkArtifactLike<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<StringValue<'v>> {
        Ok(this.with_bazel_path(&|path| {
            let dirname = match path.rsplit_once('/') {
                Some(("", _)) => "/",
                Some((dirname, _)) => dirname,
                None => "/",
            };
            heap.alloc_str(dirname)
        })?)
    }

    /// The Bazel root object for this file.
    #[starlark(attribute)]
    fn root<'v>(
        this: &'v dyn StarlarkArtifactLike<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let path = this.with_bazel_path(&|path| heap.alloc_str(path))?;
        let short_path = this.with_bazel_short_path(&|short_path| heap.alloc_str(short_path))?;
        let root = bazel_root_path(path.as_str(), short_path.as_str());
        Ok(heap.alloc(AllocStruct([("path", heap.alloc_str(&root).to_value())])))
    }

    /// The file extension of this artifact. e.g. for an artifact at foo/bar.sh,
    /// this is `sh`. If no extension is present, `""` is returned.
    #[starlark(attribute)]
    fn extension<'v>(
        this: &'v dyn StarlarkArtifactLike<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<StringValue<'v>> {
        Ok(this.with_filename(&|filename| {
            filename
                .extension()
                .map_or_else(|| heap.alloc_str(""), |x| heap.alloc_str(x))
        })?)
    }

    /// Whether the artifact represents a source file
    #[starlark(attribute)]
    fn is_source<'v>(this: &'v dyn StarlarkArtifactLike<'v>) -> starlark::Result<bool> {
        Ok(this.is_source()?)
    }

    /// Whether this artifact was declared as a directory.
    #[starlark(attribute)]
    fn is_directory<'v>(this: &'v dyn StarlarkArtifactLike<'v>) -> starlark::Result<bool> {
        Ok(this.is_directory()?)
    }

    /// Whether this artifact was declared as a symlink.
    #[starlark(attribute)]
    fn is_symlink<'v>(this: &'v dyn StarlarkArtifactLike<'v>) -> starlark::Result<bool> {
        Ok(this.is_symlink()?)
    }

    /// The `Label` of the rule or source-file target that originally created this artifact. May
    /// also be None if the artifact has not been used in an action, or if the action was not
    /// created by a rule.
    #[starlark(attribute)]
    fn owner<'v>(
        this: &'v dyn StarlarkArtifactLike<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        artifact_owner_label(this, heap)
    }

    /// The Bazel `File.label` of the source-file target or generating rule.
    #[starlark(attribute)]
    fn label<'v>(
        this: &'v dyn StarlarkArtifactLike<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        artifact_owner_label(this, heap)
    }

    /// The Bazel runfiles path for this artifact.
    #[starlark(attribute)]
    fn short_path<'v>(
        this: &'v dyn StarlarkArtifactLike<'v>,
        heap: Heap<'_>,
    ) -> starlark::Result<StringValue<'v>> {
        Ok(this.with_bazel_short_path(&|short_path| heap.alloc_str(short_path))?)
    }
}

#[starlark_module]
fn input_artifact_methods(builder: &mut MethodsBuilder) {
    /// Returns an `OutputArtifact` instance, or fails if the artifact is
    /// either an `Artifact`, or is a bound `Artifact` (You cannot bind twice)
    fn as_output<'v>(
        this: ValueOf<'v, &'v dyn StarlarkInputArtifactLike<'v>>,
    ) -> starlark::Result<StarlarkOutputArtifact<'v>> {
        Ok(this.typed.as_output(this.value)?)
    }

    /// Bazel source-file target shortcut for the singleton file depset.
    #[starlark(attribute)]
    fn files<'v>(
        this: ValueOf<'v, &'v dyn StarlarkInputArtifactLike<'v>>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(heap.alloc(bazel_depset_from_direct(vec![this.value])?))
    }

    /// Create an artifact that lives at path relative from this artifact.
    ///
    /// For example, if artifact foo is a directory containing a file bar, then `foo.project("bar")`
    /// yields the file bar. It is possible for projected artifacts to hide the prefix in order to
    /// have the short name of the resulting artifact only contain the projected path, by passing
    /// `hide_prefix = True` to `project()`.
    fn project<'v>(
        this: &'v dyn StarlarkInputArtifactLike<'v>,
        #[starlark(require = pos)] path: &str,
        #[starlark(require = named, default = false)] hide_prefix: bool,
    ) -> starlark::Result<EitherStarlarkInputArtifact<'v>> {
        let path = ForwardRelativePath::new(path)?;
        Ok(this.project(path, hide_prefix)?)
    }

    /// Returns an `Artifact` instance which is identical to the original artifact, except
    /// with no associated artifacts.
    fn without_associated_artifacts<'v>(
        this: &'v dyn StarlarkInputArtifactLike<'v>,
    ) -> starlark::Result<EitherStarlarkInputArtifact<'v>> {
        Ok(this.without_associated_artifacts()?)
    }

    /// Returns an `Artifact` instance which is identical to the original artifact, but with
    /// potentially additional artifacts. The artifacts must be bound.
    fn with_associated_artifacts<'v>(
        this: &'v dyn StarlarkInputArtifactLike<'v>,
        artifacts: UnpackList<ValueAsInputArtifactLike<'v>>,
    ) -> starlark::Result<EitherStarlarkInputArtifact<'v>> {
        Ok(this.with_associated_artifacts(artifacts)?)
    }
}

/// A single input or output file for an action.
///
/// There is no `.parent` method on `artifact`, but in most cases
/// `cmd_args(my_artifact, parent = 1)` can be used to similar effect.
pub(crate) fn artifact_methods(builder: &mut MethodsBuilder) {
    any_artifact_methods(builder);
    input_artifact_methods(builder);
}
