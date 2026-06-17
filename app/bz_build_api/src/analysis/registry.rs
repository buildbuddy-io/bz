/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cell::OnceCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::Arc;

use allocative::Allocative;
use bz_artifact::actions::key::ActionIndex;
use bz_artifact::actions::key::ActionKey;
use bz_artifact::artifact::artifact_type::DeclaredArtifact;
use bz_artifact::artifact::artifact_type::OutputArtifact;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_core::deferred::base_deferred_key::BaseDeferredKey;
use bz_core::deferred::key::DeferredHolderKey;
use bz_core::execution_types::execution::ExecutionPlatformResolution;
use bz_core::fs::buck_out_path::BazelOutputPathKind;
use bz_core::fs::buck_out_path::BazelOutputRoot;
use bz_core::fs::buck_out_path::BuckOutPathKind;
use bz_core::target::configured_target_label::ConfiguredTargetLabel;
use bz_error::internal_error;
use bz_execute::execute::request::OutputType;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_hash::BuckIndexSet;
use bz_interpreter::testing::Buck2TestHeapName;
use bz_util::thin_box::ThinBoxSlice;
use derivative::Derivative;
use dupe::Dupe;
use itertools::Itertools;
use starlark::any::ProvidesStaticType;
use starlark::codemap::FileSpan;
use starlark::environment::FrozenModule;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::register_any_complex_frozen;
use starlark::values::DynStarlark;
use starlark::values::Freeze;
use starlark::values::FreezeError;
use starlark::values::FreezeResult;
use starlark::values::Freezer;
use starlark::values::FrozenHeap;
use starlark::values::FrozenHeapRef;
use starlark::values::FrozenValue;
use starlark::values::FrozenValueTyped;
use starlark::values::Heap;
use starlark::values::OwnedFrozenValue;
use starlark::values::OwnedFrozenValueTyped;
use starlark::values::OwnedRefFrozenRef;
use starlark::values::Trace;
use starlark::values::Tracer;
use starlark::values::Value;
use starlark::values::ValueTyped;
use starlark::values::ValueTypedComplex;
use starlark::values::any_complex::StarlarkAnyComplex;
use starlark::values::typing::FrozenStarlarkCallable;
use starlark::values::typing::StarlarkCallable;
use starlark_map::small_map::SmallMap;

use crate::actions::RegisteredAction;
use crate::actions::UnregisteredAction;
use crate::actions::impls::solib_symlink::UnregisteredSolibSymlinkAction;
use crate::actions::registry::ActionsRegistry;
use crate::actions::registry::RecordedActions;
use crate::analysis::anon_promises_dyn::AnonPromisesDyn;
use crate::analysis::anon_targets_registry::ANON_TARGET_REGISTRY_NEW;
use crate::analysis::anon_targets_registry::AnonTargetsRegistryDyn;
use crate::analysis::extra_v::AnalysisExtraValue;
use crate::analysis::extra_v::FrozenAnalysisExtraValue;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::deferred::TransitiveSetIndex;
use crate::artifact_groups::deferred::TransitiveSetKey;
use crate::artifact_groups::promise::PromiseArtifact;
use crate::artifact_groups::promise::PromiseArtifactId;
use crate::deferred::calculation::ActionLookup;
use crate::dynamic::storage::DYNAMIC_LAMBDA_PARAMS_STORAGES;
use crate::dynamic::storage::DynamicLambdaParamsStorage;
use crate::dynamic::storage::FrozenDynamicLambdaParamsStorage;
use crate::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use crate::interpreter::rule_defs::artifact::output_artifact_like::OutputArtifactArg;
use crate::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use crate::interpreter::rule_defs::provider::collection::FrozenProviderCollection;
use crate::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValueRef;
use crate::interpreter::rule_defs::provider::collection::ProviderCollection;
use crate::interpreter::rule_defs::transitive_set::FrozenTransitiveSet;
use crate::interpreter::rule_defs::transitive_set::FrozenTransitiveSetDefinition;
use crate::interpreter::rule_defs::transitive_set::TransitiveSet;

#[derive(Derivative, Trace, Allocative)]
#[derivative(Debug)]
pub struct AnalysisRegistry<'v> {
    #[derivative(Debug = "ignore")]
    pub actions: ActionsRegistry<'v>,
    pub anon_targets: Box<DynStarlark<'v, dyn AnonTargetsRegistryDyn<'v>>>,
    pub analysis_value_storage: AnalysisValueStorage<'v>,
    bazel_predeclared_outputs: SmallMap<String, DeclaredArtifact<'v>>,
    bazel_shareable_outputs: SmallMap<String, DeclaredArtifact<'v>>,
    bazel_shareable_output_keys: SmallMap<OutputArtifact<'v>, String>,
    bazel_shareable_action_identities: SmallMap<String, BazelShareableActionIdentity>,
    bazel_pending_solib_symlink_actions: Vec<BazelPendingSolibSymlinkAction<'v>>,
    pub short_path_assertions: HashMap<PromiseArtifactId, ForwardRelativePathBuf>,
    pub content_based_path_assertions: HashSet<PromiseArtifactId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Trace, Allocative)]
pub struct BazelShareableActionIdentity {
    action_key: String,
    mandatory_inputs: Vec<String>,
    outputs: Vec<String>,
}

impl BazelShareableActionIdentity {
    pub fn new(
        action_key: impl Into<String>,
        mandatory_inputs: Vec<String>,
        outputs: Vec<String>,
    ) -> Self {
        Self {
            action_key: action_key.into(),
            mandatory_inputs,
            outputs,
        }
    }

    fn conflict_message(&self) -> String {
        format!("{self:?}")
    }
}

#[derive(Debug, Trace, Allocative)]
struct BazelPendingSolibSymlinkAction<'v> {
    src: DeclaredArtifact<'v>,
    output: OutputArtifact<'v>,
}

#[derive(bz_error::Error, Debug)]
#[buck2(tag = Input)]
enum DeclaredArtifactError {
    #[error("Can't declare an artifact with an empty filename component")]
    DeclaredEmptyFileName,
    #[error(
        "Artifact `{0}` was declared with `has_content_based_path = {1}`, but is now being used with `has_content_based_path = {2}`"
    )]
    AlreadyDeclaredWithDifferentContentBasedPathHashing(String, bool, bool),
    #[error(
        "Bazel shareable output `{path}` was registered with conflicting actions: `{previous}` and `{current}`"
    )]
    BazelShareableActionConflict {
        path: String,
        previous: String,
        current: String,
    },
}

impl<'v> AnalysisRegistry<'v> {
    pub fn new_from_owner(
        owner: BaseDeferredKey,
        execution_platform: ExecutionPlatformResolution,
    ) -> bz_error::Result<AnalysisRegistry<'v>> {
        Self::new_from_owner_and_deferred(execution_platform, DeferredHolderKey::Base(owner), None)
    }

    pub fn new_from_owner_and_deferred(
        execution_platform: ExecutionPlatformResolution,
        self_key: DeferredHolderKey,
        target_rule_type_name: Option<Arc<str>>,
    ) -> bz_error::Result<Self> {
        Ok(AnalysisRegistry {
            actions: ActionsRegistry::new(
                self_key.dupe(),
                execution_platform.dupe(),
                target_rule_type_name,
            ),
            anon_targets: (ANON_TARGET_REGISTRY_NEW.get()?)(PhantomData, execution_platform),
            analysis_value_storage: AnalysisValueStorage::new(self_key),
            bazel_predeclared_outputs: SmallMap::new(),
            bazel_shareable_outputs: SmallMap::new(),
            bazel_shareable_output_keys: SmallMap::new(),
            bazel_shareable_action_identities: SmallMap::new(),
            bazel_pending_solib_symlink_actions: Vec::new(),
            short_path_assertions: HashMap::new(),
            content_based_path_assertions: HashSet::new(),
        })
    }

    /// Reserves a path in an output directory. Doesn't declare artifact,
    /// but checks that there is no previously declared artifact with a path
    /// which is in conflict with claimed `path`.
    pub fn claim_output_path(
        &mut self,
        eval: &Evaluator<'_, '_, '_>,
        path: &ForwardRelativePath,
    ) -> bz_error::Result<()> {
        let declaration_location = eval.call_stack_top_location();
        self.actions.claim_output_path(path, declaration_location)
    }

    pub fn declare_dynamic_output(
        &mut self,
        artifact: &BuildArtifact,
        heap: Heap<'v>,
    ) -> bz_error::Result<DeclaredArtifact<'v>> {
        self.actions.declare_dynamic_output(artifact, heap)
    }

    pub fn declare_output(
        &mut self,
        prefix: Option<&str>,
        filename: &str,
        output_type: OutputType,
        declaration_location: Option<FileSpan>,
        path_resolution_method: BuckOutPathKind,
        heap: Heap<'v>,
    ) -> bz_error::Result<DeclaredArtifact<'v>> {
        self.declare_output_with_bazel_owner(
            prefix,
            filename,
            output_type,
            declaration_location,
            path_resolution_method,
            None,
            heap,
        )
    }

    pub fn declare_output_with_bazel_owner(
        &mut self,
        prefix: Option<&str>,
        filename: &str,
        output_type: OutputType,
        declaration_location: Option<FileSpan>,
        path_resolution_method: BuckOutPathKind,
        bazel_owner: Option<ConfiguredTargetLabel>,
        heap: Heap<'v>,
    ) -> bz_error::Result<DeclaredArtifact<'v>> {
        self.declare_output_with_bazel_owner_and_output_root(
            prefix,
            filename,
            output_type,
            declaration_location,
            path_resolution_method,
            bazel_owner,
            BazelOutputRoot::Bin,
            heap,
        )
    }

    pub fn declare_output_with_bazel_owner_and_output_root(
        &mut self,
        prefix: Option<&str>,
        filename: &str,
        output_type: OutputType,
        declaration_location: Option<FileSpan>,
        path_resolution_method: BuckOutPathKind,
        bazel_owner: Option<ConfiguredTargetLabel>,
        bazel_output_root: BazelOutputRoot,
        heap: Heap<'v>,
    ) -> bz_error::Result<DeclaredArtifact<'v>> {
        self.declare_output_with_bazel_owner_output_root_and_path_kind(
            prefix,
            filename,
            output_type,
            declaration_location,
            path_resolution_method,
            bazel_owner,
            bazel_output_root,
            BazelOutputPathKind::PackageRelative,
            heap,
        )
    }

    pub fn declare_output_with_bazel_owner_output_root_and_path_kind(
        &mut self,
        prefix: Option<&str>,
        filename: &str,
        output_type: OutputType,
        declaration_location: Option<FileSpan>,
        path_resolution_method: BuckOutPathKind,
        bazel_owner: Option<ConfiguredTargetLabel>,
        bazel_output_root: BazelOutputRoot,
        bazel_output_path_kind: BazelOutputPathKind,
        heap: Heap<'v>,
    ) -> bz_error::Result<DeclaredArtifact<'v>> {
        // We don't allow declaring `` as an output, although technically there's nothing preventing
        // that
        if filename.is_empty() {
            return Err(DeclaredArtifactError::DeclaredEmptyFileName.into());
        }

        let path = ForwardRelativePath::new(filename)?.to_owned();
        let prefix = match prefix {
            None => None,
            Some(x) => Some(ForwardRelativePath::new(x)?.to_owned()),
        };
        let full_path = match &prefix {
            Some(prefix) => prefix.join(&path),
            None => path.clone(),
        };
        let predeclared_key = Self::bazel_predeclared_output_key(
            full_path.as_str(),
            bazel_output_root,
            bazel_output_path_kind,
        );
        if let Some(artifact) = self.bazel_predeclared_outputs.get(&predeclared_key) {
            if artifact.output_type() == output_type {
                return Ok(artifact.dupe());
            }
        }
        self.actions
            .declare_artifact_with_bazel_owner_output_root_and_path_kind(
                prefix,
                path,
                output_type,
                declaration_location,
                path_resolution_method,
                bazel_owner,
                bazel_output_root,
                bazel_output_path_kind,
                heap,
            )
    }

    fn bazel_predeclared_output_key(
        path: &str,
        bazel_output_root: BazelOutputRoot,
        bazel_output_path_kind: BazelOutputPathKind,
    ) -> String {
        format!(
            "{}/{:?}/{}",
            bazel_output_root.as_str(),
            bazel_output_path_kind,
            path
        )
    }

    pub fn declare_bazel_predeclared_output(
        &mut self,
        filename: &str,
        output_type: OutputType,
        declaration_location: Option<FileSpan>,
        path_resolution_method: BuckOutPathKind,
        bazel_output_root: BazelOutputRoot,
        heap: Heap<'v>,
    ) -> bz_error::Result<DeclaredArtifact<'v>> {
        let artifact = self.declare_output_with_bazel_owner_and_output_root(
            None,
            filename,
            output_type,
            declaration_location,
            path_resolution_method,
            self.analysis_value_storage
                .self_key
                .owner()
                .configured_label(),
            bazel_output_root,
            heap,
        )?;
        self.bazel_predeclared_outputs.insert(
            Self::bazel_predeclared_output_key(
                ForwardRelativePath::new(filename)?.as_str(),
                bazel_output_root,
                BazelOutputPathKind::PackageRelative,
            ),
            artifact.dupe(),
        );
        Ok(artifact)
    }

    pub fn declare_bazel_shareable_output(
        &mut self,
        filename: &str,
        output_type: OutputType,
        declaration_location: Option<FileSpan>,
        path_resolution_method: BuckOutPathKind,
        bazel_owner: Option<ConfiguredTargetLabel>,
        bazel_output_root: BazelOutputRoot,
        bazel_output_path_kind: BazelOutputPathKind,
        heap: Heap<'v>,
    ) -> bz_error::Result<DeclaredArtifact<'v>> {
        let path = ForwardRelativePath::new(filename)?;
        let key = Self::bazel_shareable_output_path_key(
            path.as_str(),
            bazel_owner.as_ref(),
            bazel_output_root,
            bazel_output_path_kind,
        );
        if let Some(artifact) = self.bazel_shareable_outputs.get(&key) {
            if artifact.output_type() == output_type {
                return Ok(artifact.dupe());
            }
        }
        let artifact = self.declare_output_with_bazel_owner_output_root_and_path_kind(
            None,
            filename,
            output_type,
            declaration_location,
            path_resolution_method,
            bazel_owner,
            bazel_output_root,
            bazel_output_path_kind,
            heap,
        )?;
        self.bazel_shareable_outputs
            .insert(key.clone(), artifact.dupe());
        self.bazel_shareable_output_keys
            .insert(artifact.as_output(), key);
        Ok(artifact)
    }

    fn bazel_shareable_output_path_key(
        path: &str,
        bazel_owner: Option<&ConfiguredTargetLabel>,
        bazel_output_root: BazelOutputRoot,
        bazel_output_path_kind: BazelOutputPathKind,
    ) -> String {
        format!(
            "{}:{}",
            bazel_owner
                .map(|owner| owner.to_string())
                .unwrap_or_else(|| "<no-bazel-owner>".to_owned()),
            Self::bazel_predeclared_output_key(path, bazel_output_root, bazel_output_path_kind)
        )
    }

    fn bazel_shareable_output_key_for_artifact(&self, output: &OutputArtifact<'v>) -> Option<&str> {
        self.bazel_shareable_output_keys
            .get(output)
            .map(String::as_str)
    }

    pub fn bazel_shareable_output_identity(&self, output: &OutputArtifact<'v>) -> String {
        let path = self
            .bazel_shareable_output_key_for_artifact(output)
            .map(str::to_owned)
            .unwrap_or_else(|| output.get_path().with_full_path(|path| path.to_string()));
        format!("{path}:{:?}", output.output_type())
    }

    pub fn bazel_shareable_output_identities(
        &self,
        outputs: &BuckIndexSet<OutputArtifact<'v>>,
    ) -> Vec<String> {
        outputs
            .iter()
            .map(|output| self.bazel_shareable_output_identity(output))
            .collect()
    }

    pub fn bazel_shareable_artifact_group_identity(&self, input: &ArtifactGroup) -> String {
        match input {
            ArtifactGroup::Artifact(artifact) => {
                artifact.get_path().with_full_path(|path| path.to_string())
            }
            ArtifactGroup::TransitiveSetProjection(_) | ArtifactGroup::Promise(_) => {
                format!("{input:?}")
            }
        }
    }

    pub fn bazel_shareable_artifact_group_identities<'a>(
        &self,
        inputs: impl IntoIterator<Item = &'a ArtifactGroup>,
    ) -> Vec<String> {
        inputs
            .into_iter()
            .map(|input| self.bazel_shareable_artifact_group_identity(input))
            .collect()
    }

    /// Single-output wrapper around `should_register_bazel_shareable_action_for_outputs`.
    pub fn should_register_bazel_shareable_action(
        &mut self,
        output: &OutputArtifact<'v>,
        identity: impl FnOnce(&Self) -> bz_error::Result<BazelShareableActionIdentity>,
    ) -> bz_error::Result<bool> {
        self.should_register_bazel_shareable_action_for_outputs(
            &BuckIndexSet::from_iter([output.dupe()]),
            identity,
        )
    }

    /// Returns whether a Bazel shareable action should be registered.
    ///
    /// Bazel interns derived artifacts by path during analysis and then tolerates duplicate
    /// shareable actions when their action key, mandatory inputs, and ownerless outputs match.
    /// Buck binds outputs immediately, so for the Bazel action surface we track that Bazel
    /// identity before binding and skip an identical second registration.
    pub fn should_register_bazel_shareable_action_for_outputs(
        &mut self,
        outputs: &BuckIndexSet<OutputArtifact<'v>>,
        identity: impl FnOnce(&Self) -> bz_error::Result<BazelShareableActionIdentity>,
    ) -> bz_error::Result<bool> {
        let keys = outputs
            .iter()
            .filter_map(|output| {
                self.bazel_shareable_output_key_for_artifact(output)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return Ok(true);
        };

        let identity = identity(self)?;
        let mut existing = None;
        let mut has_new_key = false;
        for key in &keys {
            match self.bazel_shareable_action_identities.get(key) {
                Some(previous) if previous == &identity => {
                    existing.get_or_insert_with(|| (key.to_owned(), previous.clone()));
                }
                Some(previous) => {
                    return Err(DeclaredArtifactError::BazelShareableActionConflict {
                        path: key.to_owned(),
                        previous: previous.conflict_message(),
                        current: identity.conflict_message(),
                    }
                    .into());
                }
                None => has_new_key = true,
            }
        }

        if let Some((path, previous)) = existing {
            if has_new_key || keys.len() != outputs.len() {
                return Err(DeclaredArtifactError::BazelShareableActionConflict {
                    path,
                    previous: previous.conflict_message(),
                    current: format!(
                        "{} (partially overlaps an already registered shareable action)",
                        identity.conflict_message()
                    ),
                }
                .into());
            }
            return Ok(false);
        }

        for key in keys {
            self.bazel_shareable_action_identities
                .insert(key, identity.clone());
        }
        Ok(true)
    }

    pub fn register_bazel_solib_symlink_action(
        &mut self,
        src: DeclaredArtifact<'v>,
        output: OutputArtifact<'v>,
        identity: BazelShareableActionIdentity,
    ) -> bz_error::Result<()> {
        if !self.should_register_bazel_shareable_action(&output, |_| Ok(identity))? {
            return Ok(());
        }
        self.register_or_defer_bazel_solib_symlink_action(src, output)?;
        self.flush_bazel_pending_solib_symlink_actions()
    }

    fn register_or_defer_bazel_solib_symlink_action(
        &mut self,
        src: DeclaredArtifact<'v>,
        output: OutputArtifact<'v>,
    ) -> bz_error::Result<()> {
        match src.dupe().ensure_bound() {
            Ok(src) => self.register_action_no_flush(
                BuckIndexSet::from_iter([output]),
                UnregisteredSolibSymlinkAction::new(ArtifactGroup::Artifact(src.into_artifact())),
                None,
                None,
            ),
            Err(_) => {
                self.bazel_pending_solib_symlink_actions
                    .push(BazelPendingSolibSymlinkAction { src, output });
                Ok(())
            }
        }
    }

    fn flush_bazel_pending_solib_symlink_actions(&mut self) -> bz_error::Result<()> {
        let pending = std::mem::take(&mut self.bazel_pending_solib_symlink_actions);
        for pending in pending {
            self.register_or_defer_bazel_solib_symlink_action(pending.src, pending.output)?;
        }
        Ok(())
    }

    /// Takes a string or artifact/output artifact and converts it into an output artifact
    ///
    /// This is handy for functions like `ctx.actions.write` where it's nice to just let
    /// the user give us a string if they want as the output name.
    ///
    /// This function can declare new artifacts depending on the input.
    /// If there is no error, it returns a wrapper around the artifact (ArtifactDeclaration) and the corresponding OutputArtifact
    ///
    /// The valid types for `value` and subsequent actions are as follows:
    ///  - `str`: A new file is declared with this name.
    ///  - `StarlarkOutputArtifact`: The original artifact is returned
    ///  - `StarlarkArtifact`/`StarlarkDeclaredArtifact`: If the artifact is already bound, an error is raised. Otherwise we proceed with the original artifact.
    pub fn get_or_declare_output(
        &mut self,
        eval: &Evaluator<'v, '_, '_>,
        value: OutputArtifactArg<'v>,
        output_type: OutputType,
        has_content_based_path: Option<bool>,
    ) -> bz_error::Result<(ArtifactDeclaration<'v>, OutputArtifact<'v>)> {
        let declaration_location = eval.call_stack_top_location();
        let heap = eval.heap();
        let declared_artifact = match value {
            OutputArtifactArg::Str(path) => {
                let artifact = self.declare_output(
                    None,
                    path,
                    output_type,
                    declaration_location.dupe(),
                    match has_content_based_path {
                        Some(true) => BuckOutPathKind::ContentHash,
                        Some(false) => BuckOutPathKind::Configuration,
                        None => {
                            if *crate::interpreter::rule_defs::context::ACTION_HAS_CONTENT_BASED_PATH_DEFAULT
                                .get()
                                .unwrap_or(&false)
                            {
                                BuckOutPathKind::ContentHash
                            } else {
                                BuckOutPathKind::default()
                            }
                        }
                    },
                    heap,
                )?;
                heap.alloc_typed(StarlarkDeclaredArtifact::new(
                    declaration_location,
                    artifact,
                    AssociatedArtifacts::new(),
                ))
            }
            OutputArtifactArg::OutputArtifact(output) => output.inner(),
            OutputArtifactArg::DeclaredArtifact(artifact) => artifact,
            OutputArtifactArg::WrongArtifact(artifact) => {
                return Err(artifact.0.as_output_error());
            }
        };

        let output = declared_artifact.output_artifact();
        output.ensure_output_type(output_type)?;

        if let Some(has_content_based_path) = has_content_based_path {
            if has_content_based_path != output.has_content_based_path() {
                return Err(
                    DeclaredArtifactError::AlreadyDeclaredWithDifferentContentBasedPathHashing(
                        format!("{output}"),
                        output.has_content_based_path(),
                        has_content_based_path,
                    )
                    .into(),
                );
            }
        }

        Ok((
            ArtifactDeclaration {
                artifact: declared_artifact,
                heap,
            },
            output,
        ))
    }

    fn register_action_no_flush<A: UnregisteredAction + 'static>(
        &mut self,
        outputs: BuckIndexSet<OutputArtifact>,
        action: A,
        associated_value: Option<Value<'v>>,
        error_handler: Option<StarlarkCallable<'v>>,
    ) -> bz_error::Result<()> {
        let id = self
            .actions
            .register(&self.analysis_value_storage.self_key, outputs, action)?;
        self.analysis_value_storage
            .set_action_data(id, (associated_value, error_handler))?;
        Ok(())
    }

    pub fn register_action<A: UnregisteredAction + 'static>(
        &mut self,
        outputs: BuckIndexSet<OutputArtifact>,
        action: A,
        associated_value: Option<Value<'v>>,
        error_handler: Option<StarlarkCallable<'v>>,
    ) -> bz_error::Result<()> {
        self.register_action_no_flush(outputs, action, associated_value, error_handler)?;
        self.flush_bazel_pending_solib_symlink_actions()
    }

    pub fn create_transitive_set(
        &mut self,
        definition: FrozenValueTyped<'v, FrozenTransitiveSetDefinition>,
        value: Option<Value<'v>>,
        children: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, TransitiveSet<'v>>> {
        Ok(self
            .analysis_value_storage
            .register_transitive_set(move |key| {
                let set =
                    TransitiveSet::new_from_values(key.dupe(), definition, value, children, eval)?;
                Ok(eval.heap().alloc_typed(set))
            })?)
    }

    pub(crate) fn take_promises(&mut self) -> Option<Box<dyn AnonPromisesDyn<'v>>> {
        self.anon_targets.take_promises()
    }

    pub fn consumer_analysis_artifacts(&self) -> Vec<PromiseArtifact> {
        self.anon_targets.consumer_analysis_artifacts()
    }

    pub fn record_short_path_assertion(
        &mut self,
        short_path: ForwardRelativePathBuf,
        promise_artifact_id: PromiseArtifactId,
    ) {
        self.short_path_assertions
            .insert(promise_artifact_id, short_path);
    }

    pub fn record_has_content_based_path_assertion(
        &mut self,
        promise_artifact_id: PromiseArtifactId,
    ) {
        self.content_based_path_assertions
            .insert(promise_artifact_id);
    }

    pub fn assert_no_promises(&self) -> bz_error::Result<()> {
        self.anon_targets.assert_no_promises()
    }

    pub fn num_declared_actions(&self) -> u64 {
        self.actions.actions_len() as u64
    }

    pub fn num_declared_artifacts(&self) -> u64 {
        self.actions.artifacts_len() as u64
    }

    /// You MUST pass the same module to both the first function and the second one.
    /// It requires both to get the lifetimes to line up.
    pub fn finalize(
        mut self,
        env: &Module<'v>,
    ) -> bz_error::Result<
        impl FnOnce(&FrozenModule) -> bz_error::Result<RecordedAnalysisValues> + use<>,
    > {
        self.flush_bazel_pending_solib_symlink_actions()?;
        if let Some(pending) = self.bazel_pending_solib_symlink_actions.first() {
            pending.src.dupe().ensure_bound()?;
        }

        let AnalysisRegistry {
            actions,
            anon_targets: _,
            analysis_value_storage,
            bazel_predeclared_outputs: _,
            bazel_shareable_outputs: _,
            bazel_shareable_output_keys: _,
            bazel_shareable_action_identities: _,
            bazel_pending_solib_symlink_actions: _,
            short_path_assertions: _,
            content_based_path_assertions: _,
        } = self;

        let finalize_actions = actions.finalize()?;

        let self_key = analysis_value_storage.self_key.dupe();
        analysis_value_storage.write_to_module(env)?;
        Ok(move |frozen_env: &FrozenModule| {
            let analysis_value_fetcher = AnalysisValueFetcher {
                self_key,
                frozen_module: Some(frozen_env.dupe()),
            };
            let actions = (finalize_actions)(&analysis_value_fetcher)?;
            let recorded_values = analysis_value_fetcher.get_recorded_values(actions)?;

            Ok(recorded_values)
        })
    }

    pub fn execution_platform(&self) -> &ExecutionPlatformResolution {
        self.actions.execution_platform()
    }
}

pub struct ArtifactDeclaration<'v> {
    artifact: ValueTyped<'v, StarlarkDeclaredArtifact<'v>>,
    heap: Heap<'v>,
}

impl<'v> ArtifactDeclaration<'v> {
    pub fn into_declared_artifact(
        self,
        extra_associated_artifacts: AssociatedArtifacts,
    ) -> ValueTyped<'v, StarlarkDeclaredArtifact<'v>> {
        self.heap.alloc_typed(
            self.artifact
                .with_extended_associated_artifacts(extra_associated_artifacts),
        )
    }
}

/// Store `Value<'v>` values for actions registered in an implementation function
///
/// These values eventually are written into the mutable `Module`, and a wrapper is
/// made available to get the `OwnedFrozenValue` back out after that `Module` is frozen.
///
/// Note that this object has internal mutation and is only expected to live for the duration
/// of impl function execution.
///
/// At the end of impl function execution, `write_to_module` should be called
/// to write this object to `Module` extra value to get the values frozen.
#[derive(Debug, Allocative, ProvidesStaticType)]
pub struct AnalysisValueStorage<'v> {
    pub self_key: DeferredHolderKey,
    action_data: SmallMap<ActionIndex, (Option<Value<'v>>, Option<StarlarkCallable<'v>>)>,
    transitive_sets: Vec<ValueTyped<'v, TransitiveSet<'v>>>,
    pub lambda_params: Box<DynStarlark<'v, dyn DynamicLambdaParamsStorage<'v>>>,
    result_value: OnceCell<ValueTypedComplex<'v, ProviderCollection<'v>>>,
}

#[derive(Debug, Allocative, ProvidesStaticType)]
pub struct FrozenAnalysisValueStorage {
    pub self_key: DeferredHolderKey,
    action_data: SmallMap<ActionIndex, (Option<FrozenValue>, Option<FrozenStarlarkCallable>)>,
    transitive_sets: ThinBoxSlice<FrozenValueTyped<'static, FrozenTransitiveSet>>,
    pub lambda_params: Box<dyn FrozenDynamicLambdaParamsStorage>,
    result_value: Option<FrozenValueTyped<'static, FrozenProviderCollection>>,
}

unsafe impl<'v> Trace<'v> for AnalysisValueStorage<'v> {
    fn trace(&mut self, tracer: &Tracer<'v>) {
        let AnalysisValueStorage {
            action_data,
            transitive_sets,
            lambda_params,
            self_key,
            result_value,
        } = self;
        for (k, v) in action_data.iter_mut() {
            tracer.trace_static(k);
            v.trace(tracer);
        }
        for v in transitive_sets.iter_mut() {
            v.trace(tracer);
        }
        lambda_params.trace(tracer);
        tracer.trace_static(self_key);
        result_value.trace(tracer);
    }
}

impl<'v> Freeze for AnalysisValueStorage<'v> {
    type Frozen = FrozenAnalysisValueStorage;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let AnalysisValueStorage {
            self_key,
            action_data,
            transitive_sets,
            lambda_params,
            result_value,
        } = self;

        // N.B. collect::<Result<_>> sets the lower bound to zero,
        // which can cause over-allocations in frozen containers.
        let mut frozen_action_data = SmallMap::with_capacity(action_data.len());
        for (k, v) in action_data {
            frozen_action_data.insert(k, v.freeze(freezer)?);
        }
        let mut frozen_transitive_sets = Vec::with_capacity(transitive_sets.len());
        for v in transitive_sets {
            frozen_transitive_sets.push(
                FrozenValueTyped::new_err(v.to_value().freeze(freezer)?)
                    .map_err(|e| FreezeError::new(e.to_string()))?,
            );
        }
        Ok(FrozenAnalysisValueStorage {
            self_key,
            action_data: frozen_action_data,
            transitive_sets: frozen_transitive_sets
                .into_iter()
                .collect::<ThinBoxSlice<_>>(),
            lambda_params: lambda_params.freeze(freezer)?,
            result_value: result_value.freeze(freezer)?,
        })
    }
}

/// Simple fetcher that fetches the values written in `AnalysisValueStorage::write_to_module`
///
/// These values are pulled from the `FrozenModule` that results from `env.freeze()`.
/// This is used by the action registry to make an `OwnedFrozenValue` available to
/// Actions' register function.
pub struct AnalysisValueFetcher {
    self_key: DeferredHolderKey,
    frozen_module: Option<FrozenModule>,
}

impl AnalysisValueFetcher {
    pub fn testing_new(self_key: DeferredHolderKey) -> Self {
        AnalysisValueFetcher {
            self_key,
            frozen_module: None,
        }
    }
}

impl<'v> AnalysisValueStorage<'v> {
    fn new(self_key: DeferredHolderKey) -> Self {
        Self {
            self_key: self_key.dupe(),
            action_data: SmallMap::new(),
            transitive_sets: Vec::new(),
            lambda_params: DYNAMIC_LAMBDA_PARAMS_STORAGES
                .get()
                .unwrap()
                .new_dynamic_lambda_params_storage(self_key),
            result_value: OnceCell::new(),
        }
    }

    /// Write self to `module` extra value.
    fn write_to_module(self, module: &Module<'v>) -> bz_error::Result<()> {
        let extra_v = AnalysisExtraValue::get_or_init(module)?;
        let res = extra_v.analysis_value_storage.set(
            module
                .heap()
                .alloc_typed(StarlarkAnyComplex { value: self }),
        );
        if res.is_err() {
            return Err(internal_error!("analysis_value_storage is already set"));
        }
        Ok(())
    }

    pub(crate) fn register_transitive_set<
        F: FnOnce(TransitiveSetKey) -> bz_error::Result<ValueTyped<'v, TransitiveSet<'v>>>,
    >(
        &mut self,
        func: F,
    ) -> bz_error::Result<ValueTyped<'v, TransitiveSet<'v>>> {
        let key = TransitiveSetKey::new(
            self.self_key.dupe(),
            TransitiveSetIndex(self.transitive_sets.len().try_into()?),
        );
        let set = func(key.dupe())?;
        self.transitive_sets.push(set.dupe());
        Ok(set)
    }

    fn set_action_data(
        &mut self,
        id: ActionKey,
        action_data: (Option<Value<'v>>, Option<StarlarkCallable<'v>>),
    ) -> bz_error::Result<()> {
        if &self.self_key != id.holder_key() {
            return Err(internal_error!(
                "Wrong action owner: expecting `{}`, got `{}`",
                self.self_key,
                id
            ));
        }
        self.action_data.insert(id.action_index(), action_data);
        Ok(())
    }

    pub fn set_result_value(
        &self,
        providers: ValueTypedComplex<'v, ProviderCollection<'v>>,
    ) -> bz_error::Result<()> {
        if self.result_value.set(providers).is_err() {
            return Err(internal_error!("result_value is already set"));
        }
        Ok(())
    }
}

impl AnalysisValueFetcher {
    fn extra_value(
        &self,
    ) -> bz_error::Result<Option<(&FrozenAnalysisValueStorage, &FrozenHeapRef)>> {
        match &self.frozen_module {
            None => Ok(None),
            Some(module) => {
                let analysis_extra_value = FrozenAnalysisExtraValue::get(module)?
                    .value
                    .analysis_value_storage
                    .ok_or_else(|| internal_error!("analysis_value_storage not set"))?
                    .as_ref();
                Ok(Some((&analysis_extra_value.value, module.frozen_heap())))
            }
        }
    }

    /// Get the `OwnedFrozenValue` that corresponds to a `DeferredId`, if present
    pub fn get_action_data(
        &self,
        id: &ActionKey,
    ) -> bz_error::Result<(Option<OwnedFrozenValue>, Option<OwnedFrozenValue>)> {
        let Some((storage, heap_ref)) = self.extra_value()? else {
            return Ok((None, None));
        };

        if id.holder_key() != &storage.self_key {
            return Err(internal_error!(
                "Wrong action owner: expecting `{}`, got `{}`",
                storage.self_key,
                id
            ));
        }

        let Some(value) = storage.action_data.get(&id.action_index()) else {
            return Ok((None, None));
        };

        unsafe {
            Ok((
                value.0.map(|v| OwnedFrozenValue::new(heap_ref.dupe(), v)),
                value.1.map(|v| OwnedFrozenValue::new(heap_ref.dupe(), v.0)),
            ))
        }
    }

    pub(crate) fn get_recorded_values(
        &self,
        actions: RecordedActions,
    ) -> bz_error::Result<RecordedAnalysisValues> {
        let analysis_storage = match &self.frozen_module {
            None => None,
            Some(module) => Some(FrozenAnalysisExtraValue::get(module)?.try_map(|v| {
                v.value
                    .analysis_value_storage
                    .ok_or_else(|| internal_error!("analysis_value_storage not set"))
            })?),
        };

        Ok(RecordedAnalysisValues {
            self_key: self.self_key.dupe(),
            analysis_storage,
            actions,
        })
    }
}

/// The analysis values stored in DeferredHolder.
#[derive(Debug, Allocative)]
pub struct RecordedAnalysisValues {
    self_key: DeferredHolderKey,
    analysis_storage: Option<OwnedFrozenValueTyped<StarlarkAnyComplex<FrozenAnalysisValueStorage>>>,
    actions: RecordedActions,
}

register_any_complex_frozen!(FrozenAnalysisValueStorage);

impl RecordedAnalysisValues {
    pub fn new_provider_collection(
        self_key: DeferredHolderKey,
        heap: FrozenHeap,
        providers: FrozenValueTyped<'static, FrozenProviderCollection>,
    ) -> Self {
        let value = heap.alloc_simple(StarlarkAnyComplex {
            value: FrozenAnalysisValueStorage {
                self_key: self_key.dupe(),
                action_data: SmallMap::new(),
                transitive_sets: Vec::new().into_iter().collect(),
                lambda_params: DYNAMIC_LAMBDA_PARAMS_STORAGES
                    .get()
                    .unwrap()
                    .new_frozen_dynamic_lambda_params_storage(),
                result_value: Some(providers),
            },
        });
        Self {
            self_key,
            analysis_storage: Some(
                unsafe {
                    OwnedFrozenValue::new(
                        heap.into_ref_named(Buck2TestHeapName::frozen_heap_name()),
                        value,
                    )
                }
                .downcast()
                .unwrap(),
            ),
            actions: RecordedActions::new(0),
        }
    }

    /// Creates a minimal RecordedAnalysisValues for testing action lookups only.
    /// This version doesn't require DYNAMIC_LAMBDA_PARAMS_STORAGES to be initialized.
    pub fn testing_new_actions_only(self_key: DeferredHolderKey, actions: RecordedActions) -> Self {
        Self {
            self_key,
            analysis_storage: None,
            actions,
        }
    }

    pub fn testing_new(
        self_key: DeferredHolderKey,
        transitive_sets: Vec<(TransitiveSetKey, OwnedFrozenValueTyped<FrozenTransitiveSet>)>,
        actions: RecordedActions,
    ) -> Self {
        let heap = FrozenHeap::new();
        let mut alloced_tsets = Vec::new();
        for (_key, tset) in transitive_sets
            .iter()
            .sorted_by_key(|(key, _)| key.index().0)
        {
            heap.add_reference(tset.owner());
            let tset = tset.owned_frozen_value_typed(&heap);
            alloced_tsets.push(tset);
        }

        let providers = FrozenProviderCollection::testing_new_default(&heap);

        let value = heap.alloc_simple(StarlarkAnyComplex {
            value: FrozenAnalysisValueStorage {
                self_key: self_key.dupe(),
                action_data: SmallMap::new(),
                transitive_sets: alloced_tsets.into_iter().collect(),
                lambda_params: DYNAMIC_LAMBDA_PARAMS_STORAGES
                    .get()
                    .unwrap()
                    .new_frozen_dynamic_lambda_params_storage(),
                result_value: Some(
                    FrozenValueTyped::<FrozenProviderCollection>::new(heap.alloc(providers))
                        .unwrap(),
                ),
            },
        });
        Self {
            self_key,
            analysis_storage: Some(
                unsafe {
                    OwnedFrozenValue::new(
                        heap.into_ref_named(Buck2TestHeapName::frozen_heap_name()),
                        value,
                    )
                }
                .downcast()
                .unwrap(),
            ),
            actions,
        }
    }

    pub(crate) fn lookup_transitive_set(
        &self,
        key: &TransitiveSetKey,
    ) -> bz_error::Result<OwnedFrozenValueTyped<FrozenTransitiveSet>> {
        if key.holder_key() != &self.self_key {
            return Err(internal_error!(
                "Wrong owner for transitive set: expecting `{}`, got `{}`",
                self.self_key,
                key
            ));
        }
        self.analysis_storage
            .as_ref()
            .ok_or_else(|| internal_error!("Missing analysis storage for `{key}`"))?
            .maybe_map(|v| v.value.transitive_sets.get(key.index().0 as usize).copied())
            .ok_or_else(|| internal_error!("Missing transitive set `{key}`"))
    }

    pub fn lookup_action(&self, key: &ActionKey) -> bz_error::Result<ActionLookup> {
        if key.holder_key() != &self.self_key {
            return Err(internal_error!(
                "Wrong owner for action: expecting `{}`, got `{}`",
                self.self_key,
                key
            ));
        }
        self.actions.lookup(key)
    }

    /// Iterates over the actions created in this analysis.
    pub fn iter_actions(&self) -> impl Iterator<Item = &Arc<RegisteredAction>> + '_ {
        self.actions.iter_actions()
    }

    pub fn analysis_storage(
        &self,
    ) -> bz_error::Result<OwnedRefFrozenRef<'_, FrozenAnalysisValueStorage>> {
        Ok(self
            .analysis_storage
            .as_ref()
            .ok_or_else(|| internal_error!("missing analysis storage"))?
            .as_owned_ref_frozen_ref()
            .map(|v| &v.value))
    }

    /// Iterates over the declared dynamic_output/actions.
    pub fn iter_dynamic_lambda_outputs(&self) -> impl Iterator<Item = BuildArtifact> + '_ {
        self.analysis_storage
            .iter()
            .flat_map(|v| v.value.lambda_params.iter_dynamic_lambda_outputs())
    }

    pub fn provider_collection(&self) -> bz_error::Result<FrozenProviderCollectionValueRef<'_>> {
        let analysis_storage = self
            .analysis_storage
            .as_ref()
            .ok_or_else(|| internal_error!("missing analysis storage"))?;
        let value = analysis_storage
            .as_ref()
            .value
            .result_value
            .ok_or_else(|| internal_error!("missing provider collection"))?;
        unsafe {
            Ok(FrozenProviderCollectionValueRef::new(
                analysis_storage.owner(),
                value,
            ))
        }
    }

    pub(crate) fn retained_memory(&self) -> bz_error::Result<usize> {
        Ok(self
            .analysis_storage
            .as_ref()
            .ok_or_else(|| internal_error!("missing analysis storage"))?
            .owner()
            .allocated_bytes())
    }
}
