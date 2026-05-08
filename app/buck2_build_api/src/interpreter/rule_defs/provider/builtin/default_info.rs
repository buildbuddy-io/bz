/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::fmt::Debug;
use std::iter;
use std::marker::PhantomData;
use std::ptr;

use allocative::Allocative;
use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_artifact::artifact::artifact_type::OutputArtifact;
use buck2_build_api_derive::internal_provider;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use dupe::Dupe;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Evaluator;
use starlark::starlark_complex_value;
use starlark::values::Freeze;
use starlark::values::FreezeError;
use starlark::values::FrozenHeap;
use starlark::values::FrozenValue;
use starlark::values::FrozenValueOfUnchecked;
use starlark::values::FrozenValueTyped;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::UnpackAndDiscard;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOf;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictType;
use starlark::values::dict::FrozenDictRef;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list::ListType;
use starlark::values::list::UnpackList;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::structs::StructRef;

use crate as buck2_build_api;
use crate::artifact_groups::ArtifactGroup;
use crate::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueIsInputArtifactAnnotation;
use crate::interpreter::rule_defs::artifact_tagging::ArtifactTag;
use crate::interpreter::rule_defs::cmd_args::CommandLineArtifactVisitor;
use crate::interpreter::rule_defs::cmd_args::value_as::ValueAsCommandLineLike;
use crate::interpreter::rule_defs::depset::BazelDepset;
use crate::interpreter::rule_defs::depset::FrozenBazelDepset;
use crate::interpreter::rule_defs::depset::bazel_depset_empty;
use crate::interpreter::rule_defs::depset::bazel_depset_empty_frozen;
use crate::interpreter::rule_defs::depset::bazel_depset_from_values;
use crate::interpreter::rule_defs::depset::bazel_depset_to_list;
use crate::interpreter::rule_defs::provider::ProviderCollection;
use crate::interpreter::rule_defs::provider::collection::FrozenProviderCollection;

#[derive(
    Debug,
    Clone,
    Coerce,
    Trace,
    Freeze,
    ProvidesStaticType,
    NoSerialize,
    Allocative
)]
#[repr(C)]
pub struct BazelRunfilesGen<'v, V: ValueLike<'v>> {
    files: ValueOfUncheckedGeneric<V, FrozenBazelDepset>,
    _marker: PhantomData<&'v ()>,
}

starlark_complex_value!(pub BazelRunfiles<'v>);

impl<'v, V: ValueLike<'v>> fmt::Display for BazelRunfilesGen<'v, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("runfiles")
    }
}

#[starlark_value(type = "runfiles")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for BazelRunfilesGen<'v, V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_runfiles_methods)
    }
}

fn bazel_runfiles_from_depset<'v>(files: Value<'v>) -> BazelRunfiles<'v> {
    BazelRunfiles {
        files: ValueOfUnchecked::new(files),
        _marker: PhantomData,
    }
}

fn bazel_runfiles_empty_value<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(bazel_runfiles_from_depset(bazel_depset_empty(heap)))
}

fn bazel_runfiles_empty_frozen_value(heap: &FrozenHeap) -> FrozenValue {
    heap.alloc(FrozenBazelRunfiles {
        files: FrozenValueOfUnchecked::new(bazel_depset_empty_frozen(heap)),
        _marker: PhantomData,
    })
}

fn push_unique_value<'v>(values: &mut Vec<Value<'v>>, value: Value<'v>) -> starlark::Result<()> {
    for existing in values.iter().copied() {
        if existing.equals(value)? {
            return Ok(());
        }
    }
    values.push(value);
    Ok(())
}

pub(crate) fn bazel_runfiles_from_runfiles<'v, 'a>(
    heap: Heap<'v>,
    runfiles: impl IntoIterator<Item = &'a BazelRunfiles<'v>>,
) -> starlark::Result<BazelRunfiles<'v>>
where
    'v: 'a,
{
    let mut files = Vec::new();
    for runfiles in runfiles {
        for file in bazel_depset_to_list(runfiles.files.get().to_value())? {
            push_unique_value(&mut files, file)?;
        }
    }
    let files = bazel_depset_from_values(heap, files)?;
    Ok(bazel_runfiles_from_depset(files))
}

pub(crate) fn bazel_runfiles_from_files<'v>(
    heap: Heap<'v>,
    direct_files: impl IntoIterator<Item = Value<'v>>,
    transitive_files: Option<Value<'v>>,
) -> starlark::Result<BazelRunfiles<'v>> {
    let mut files = Vec::new();
    for file in direct_files {
        push_unique_value(&mut files, file)?;
    }
    if let Some(transitive_files) = transitive_files {
        for file in bazel_depset_to_list(transitive_files)? {
            push_unique_value(&mut files, file)?;
        }
    }
    let files = bazel_depset_from_values(heap, files)?;
    Ok(bazel_runfiles_from_depset(files))
}

#[starlark_module]
fn bazel_runfiles_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn files<'v>(this: &BazelRunfiles<'v>) -> starlark::Result<Value<'v>> {
        Ok(this.files.get().to_value())
    }

    #[starlark(attribute)]
    fn symlinks<'v>(this: &BazelRunfiles<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_depset_empty(heap))
    }

    #[starlark(attribute)]
    fn root_symlinks<'v>(this: &BazelRunfiles<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_depset_empty(heap))
    }

    #[starlark(attribute)]
    fn empty_filenames<'v>(
        this: &BazelRunfiles<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(bazel_depset_empty(heap))
    }

    fn merge<'v>(
        this: &BazelRunfiles<'v>,
        other: &BazelRunfiles<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<BazelRunfiles<'v>> {
        bazel_runfiles_from_runfiles(heap, [this, other])
    }

    fn merge_all<'v>(
        this: &BazelRunfiles<'v>,
        others: UnpackListOrTuple<Value<'v>>,
        heap: Heap<'v>,
    ) -> starlark::Result<BazelRunfiles<'v>> {
        let mut runfiles = Vec::with_capacity(others.items.len() + 1);
        runfiles.push(this);
        for other in others.items {
            let other = BazelRunfiles::from_value(other).ok_or_else(|| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "`runfiles.merge_all` expected runfiles, got `{}`",
                    other.to_string_for_type_error()
                )
            })?;
            runfiles.push(other);
        }
        bazel_runfiles_from_runfiles(heap, runfiles)
    }
}

/// A provider that all rules' implementations must return
///
/// In many simple cases, this can be inferred for the user.
///
/// Example of a rule's implementation function and how these fields are used by the framework:
///
/// ```python
/// # //foo_binary.bzl
/// def impl(ctx):
///     out = ctx.actions.declare_output("out")
///     ctx.actions.run([ctx.attrs._cc[RunInfo], "-o", out.as_output()] + ctx.attrs.srcs)
///     stripped_out = ctx.actions.declare_output("stripped")
///     debug_symbols_out = ctx.actions.declare_output("debug_info")
///     ctx.actions.run([
///         ctx.attrs._strip[RunInfo],
///         "--binary",
///         out,
///         "--stripped-out",
///         stripped_out.as_output(),
///         "--debug-symbols-out",
///         debug_symbols_out.as_output(),
///     ])
///     return [
///         DefaultInfo(
///             sub_targets = {
///                 "stripped": [
///                     DefaultInfo(default_outputs = [stripped_out, debug_symbols_out]),
///                 ],
///             },
///             default_output = out,
///         ),
///     ]
///
/// foo_binary = rule(
///     impl = impl,
///     attrs = {
///         "srcs": attrs.list(attrs.source()),
///         "_cc": attrs.dep(default = "//tools:cc", providers = [RunInfo]),
///         "_strip_script": attrs.dep(default = "//tools:strip", providers = [RunInfo]),
///     },
/// )
///
///
/// def foo_binary_wrapper(name, srcs):
///     foo_binary(
///         name = name,
///         srcs = src,
///         out = name,
///         stripped = name + ".stripped",
///         debug_info = name + ".debug_info",
///     )
///
/// # //subdir/BUCK
/// load("//:foo_binary.bzl", "foo_binary_wrapper")
///
/// genrule(name = "gen_stuff", ...., default_outs = ["foo.cpp"])
///
/// # ":gen_stuff" pulls the default_outputs for //subdir:gen_stuff
/// foo_binary_wrapper(name = "foo", srcs = glob(["*.cpp"]) + [":gen_stuff"])
///
/// # Builds just 'foo' binary. The strip command is never invoked.
/// $ buck build //subdir:foo
///
/// # builds the 'foo' binary, because it is needed by the 'strip' command. Ensures that
/// # both the stripped binary and the debug symbols are built.
/// $ buck build //subdir:foo[stripped]
/// ```
#[internal_provider(default_info_creator)]
#[derive(Clone, Debug, Freeze, Trace, Coerce, ProvidesStaticType, Allocative)]
#[freeze(validator = validate_default_info, bounds = "V: ValueLike<'freeze>")]
#[repr(C)]
pub struct DefaultInfoGen<V: ValueLifetimeless> {
    /// A mapping of names to `ProviderCollection`s. The keys are used when resolving the
    /// `ProviderName` portion of a `ProvidersLabel` in order to access the providers for a
    /// subtarget, such as when doing `buck2 build cell//foo:bar[baz]`. Just like any
    /// `ProviderCollection`, this collection must include at least a `DefaultInfo` provider. The
    /// subtargets can have their own subtargets as well, which can be accessed by chaining them,
    /// e.g.: `buck2 build cell//foo:bar[baz][qux]`.
    sub_targets: ValueOfUncheckedGeneric<V, DictType<String, FrozenProviderCollection>>,
    /// A list of `Artifact`s that are built by default if this rule is requested
    /// explicitly (via CLI or `$(location)` etc), or depended on as as a "source"
    /// (i.e., `attrs.source()`).
    default_outputs: ValueOfUncheckedGeneric<V, ListType<ValueIsInputArtifactAnnotation>>,
    /// A list of `ArtifactTraversable`. The underlying `Artifact`s they define will
    /// be built by default if this rule is requested (via CLI or `$(location)` etc),
    /// but _not_ when it's depended on as as a "source" (i.e., `attrs.source()`).
    /// `ArtifactTraversable` can be an `Artifact` (which yields itself), or
    /// `cmd_args`, which expand to all their inputs.
    other_outputs: ValueOfUncheckedGeneric<V, ListType<ValueAsCommandLineLike<'static>>>,
    /// Bazel-compatible default files depset.
    files: ValueOfUncheckedGeneric<V, FrozenBazelDepset>,
    /// Bazel-compatible files-to-run provider view.
    files_to_run: ValueOfUncheckedGeneric<V, StructRef<'static>>,
    /// Bazel-compatible runfiles for data dependencies.
    data_runfiles: ValueOfUncheckedGeneric<V, FrozenBazelRunfiles>,
    /// Bazel-compatible runfiles for ordinary dependencies.
    default_runfiles: ValueOfUncheckedGeneric<V, FrozenBazelRunfiles>,
}

fn bazel_files_to_run<'v>(heap: Heap<'v>, executable: Value<'v>) -> Value<'v> {
    heap.alloc(AllocStruct([("executable", executable)]))
}

fn validate_default_info(info: &FrozenDefaultInfo) -> buck2_error::Result<()> {
    // Check length of default outputs
    let default_output_list = ListRef::from_value(info.default_outputs.get().to_value())
        .expect("should be a list from constructor");
    if default_output_list.len() > 1 {
        tracing::info!("DefaultInfo.default_output should only have a maximum of 1 item.");
        // TODO use soft_error when landed
        // TODO error rather than soft warning
        // return Err(buck2_error::buck2_error!(
        //     "DefaultInfo.default_output can only have a maximum of 1 item."
        // ));
    }

    // Check mutable data hasn't been modified.
    for output in info.default_outputs_impl()? {
        output?;
    }
    for sub_target in info.sub_targets_impl()? {
        sub_target?;
    }

    Ok(())
}

impl<'v> DefaultInfo<'v> {
    pub fn empty(heap: Heap<'v>) -> Self {
        let sub_targets = ValueOfUnchecked::<DictType<_, _>>::new(heap.alloc(AllocDict::EMPTY));
        let default_outputs = ValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList::EMPTY));
        let other_outputs = ValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList::EMPTY));
        let files = ValueOfUnchecked::<FrozenBazelDepset>::new(bazel_depset_empty(heap));
        let files_to_run =
            ValueOfUnchecked::<StructRef>::new(bazel_files_to_run(heap, Value::new_none()));
        let data_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        let default_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        DefaultInfo {
            sub_targets,
            default_outputs,
            other_outputs,
            files,
            files_to_run,
            data_runfiles,
            default_runfiles,
        }
    }

    pub fn with_default_outputs(
        heap: Heap<'v>,
        outputs: impl IntoIterator<Item = Value<'v>>,
    ) -> Self {
        let outputs = outputs.into_iter().collect::<Vec<_>>();
        let sub_targets = ValueOfUnchecked::<DictType<_, _>>::new(heap.alloc(AllocDict::EMPTY));
        let default_outputs =
            ValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList(outputs.iter().copied())));
        let other_outputs = ValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList::EMPTY));
        let files = ValueOfUnchecked::<FrozenBazelDepset>::new(
            bazel_depset_from_values(heap, outputs).unwrap(),
        );
        let files_to_run =
            ValueOfUnchecked::<StructRef>::new(bazel_files_to_run(heap, Value::new_none()));
        let data_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        let default_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        DefaultInfo {
            sub_targets,
            default_outputs,
            other_outputs,
            files,
            files_to_run,
            data_runfiles,
            default_runfiles,
        }
    }

    pub fn for_file_target(heap: Heap<'v>, artifact: Value<'v>) -> Self {
        let sub_targets = ValueOfUnchecked::<DictType<_, _>>::new(heap.alloc(AllocDict::EMPTY));
        let default_outputs =
            ValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList([artifact])));
        let other_outputs = ValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList::EMPTY));
        let files = ValueOfUnchecked::<FrozenBazelDepset>::new(
            bazel_depset_from_values(heap, vec![artifact]).unwrap(),
        );
        let files_to_run = ValueOfUnchecked::<StructRef>::new(bazel_files_to_run(heap, artifact));
        let data_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        let default_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        DefaultInfo {
            sub_targets,
            default_outputs,
            other_outputs,
            files,
            files_to_run,
            data_runfiles,
            default_runfiles,
        }
    }
}

impl FrozenDefaultInfo {
    pub(crate) fn testing_empty(heap: &FrozenHeap) -> FrozenValueTyped<'static, FrozenDefaultInfo> {
        let sub_targets = heap
            .alloc_typed_unchecked(AllocDict(
                iter::empty::<(String, FrozenProviderCollection)>(),
            ))
            .cast();
        let default_outputs =
            FrozenValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList::EMPTY));
        let other_outputs =
            FrozenValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList::EMPTY));
        let files =
            FrozenValueOfUnchecked::<FrozenBazelDepset>::new(bazel_depset_empty_frozen(heap));
        let files_to_run = FrozenValueOfUnchecked::<StructRef>::new(
            heap.alloc(AllocStruct([("executable", FrozenValue::new_none())])),
        );
        let data_runfiles = FrozenValueOfUnchecked::<FrozenBazelRunfiles>::new(
            bazel_runfiles_empty_frozen_value(heap),
        );
        let default_runfiles = FrozenValueOfUnchecked::<FrozenBazelRunfiles>::new(
            bazel_runfiles_empty_frozen_value(heap),
        );
        FrozenValueTyped::new_err(heap.alloc(FrozenDefaultInfo {
            sub_targets,
            default_outputs,
            other_outputs,
            files,
            files_to_run,
            data_runfiles,
            default_runfiles,
        }))
        .unwrap()
    }

    fn get_sub_target_providers_impl(
        &self,
        name: &str,
    ) -> buck2_error::Result<Option<FrozenValueTyped<'static, FrozenProviderCollection>>> {
        FrozenDictRef::from_frozen_value(self.sub_targets.get())
            .ok_or_else(|| internal_error!("sub_targets should be a dict-like object"))?
            .get_str(name)
            .map(|v| {
                FrozenValueTyped::new_err(v).buck_error_context(
                    "Values inside of a frozen provider should be frozen provider collection",
                )
            })
            .transpose()
    }

    pub fn get_sub_target_providers(
        &self,
        name: &str,
    ) -> Option<FrozenValueTyped<'static, FrozenProviderCollection>> {
        self.get_sub_target_providers_impl(name).unwrap()
    }

    fn default_outputs_impl(
        &self,
    ) -> buck2_error::Result<impl Iterator<Item = buck2_error::Result<StarlarkArtifact>> + '_> {
        let list = ListRef::from_frozen_value(self.default_outputs.get())
            .ok_or_else(|| internal_error!("Should be list of artifacts"))?;

        Ok(list.iter().map(|v| {
            let frozen_value = v
                .unpack_frozen()
                .ok_or_else(|| internal_error!("should be frozen"))?;

            Ok(
                if let Some(starlark_artifact) = frozen_value.downcast_ref::<StarlarkArtifact>() {
                    starlark_artifact.dupe()
                } else {
                    // This code path is for StarlarkPromiseArtifact. We have to create a `StarlarkArtifact` object here.
                    let artifact_like =
                        ValueAsInputArtifactLike::unpack_value(frozen_value.to_value())?
                            .ok_or_else(|| internal_error!("Should be list of artifacts"))?;
                    artifact_like.0.get_bound_starlark_artifact()?
                },
            )
        }))
    }

    pub fn default_outputs(&self) -> Vec<StarlarkArtifact> {
        self.default_outputs_impl()
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    pub fn default_outputs_raw(&self) -> FrozenValue {
        self.default_outputs.get()
    }

    pub fn files_raw(&self) -> FrozenValue {
        self.files.get()
    }

    pub fn files_to_run_raw(&self) -> FrozenValue {
        self.files_to_run.get()
    }

    pub fn data_runfiles_raw(&self) -> FrozenValue {
        self.data_runfiles.get()
    }

    pub fn default_runfiles_raw(&self) -> FrozenValue {
        self.default_runfiles.get()
    }

    fn sub_targets_impl(
        &self,
    ) -> buck2_error::Result<
        impl Iterator<
            Item = buck2_error::Result<(&str, FrozenValueTyped<'static, FrozenProviderCollection>)>,
        > + '_,
    > {
        let sub_targets = FrozenDictRef::from_frozen_value(self.sub_targets.get())
            .ok_or_else(|| internal_error!("sub_targets should be a dict-like object"))?;

        Ok(sub_targets.iter().map(|(k, v)| {
            buck2_error::Ok((
                k.to_value()
                    .unpack_str()
                    .ok_or_else(|| internal_error!("sub_targets should have string keys"))?,
                FrozenValueTyped::new(v).ok_or_else(|| {
                    internal_error!(
                        "Values inside of a frozen provider should be frozen provider collection",
                    )
                })?,
            ))
        }))
    }

    pub fn sub_targets(
        &self,
    ) -> SmallMap<&str, FrozenValueTyped<'static, FrozenProviderCollection>> {
        self.sub_targets_impl()
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    pub fn sub_targets_raw(&self) -> FrozenValue {
        self.sub_targets.get()
    }

    pub fn for_each_default_output_artifact_only(
        &self,
        processor: &mut dyn FnMut(Artifact),
    ) -> buck2_error::Result<()> {
        self.for_each_in_list(self.default_outputs.get(), |value| {
            processor(
                ValueAsInputArtifactLike::unpack_value_err(value)?
                    .0
                    .get_bound_artifact()?,
            );
            Ok(())
        })
    }

    pub fn for_each_default_output_other_artifacts_only(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup),
    ) -> buck2_error::Result<()> {
        self.for_each_in_list(self.default_outputs.get(), |value| {
            let others = ValueAsInputArtifactLike::unpack_value_err(value)?
                .0
                .get_associated_artifacts();
            others
                .iter()
                .flat_map(|v| v.iter())
                .for_each(|other| processor(other.dupe()));
            Ok(())
        })
    }

    pub fn for_each_other_output(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup),
    ) -> buck2_error::Result<()> {
        struct Visitor<'x>(&'x mut dyn FnMut(ArtifactGroup));

        impl<'v> CommandLineArtifactVisitor<'v> for Visitor<'_> {
            fn visit_input(&mut self, input: ArtifactGroup, _: Vec<&ArtifactTag>) {
                (self.0)(input);
            }

            fn visit_declared_output(
                &mut self,
                _artifact: OutputArtifact<'v>,
                _tags: Vec<&ArtifactTag>,
            ) {
            }

            fn visit_frozen_output(&mut self, _artifact: Artifact, _tags: Vec<&ArtifactTag>) {}
        }

        self.for_each_in_list(self.other_outputs.get(), |value| {
            let arg_like = ValueAsCommandLineLike::unpack_value_err(value)?.0;
            arg_like.visit_artifacts(&mut Visitor(processor))?;
            Ok(())
        })
    }

    pub fn for_each_output(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup),
    ) -> buck2_error::Result<()> {
        self.for_each_default_output_artifact_only(&mut |a| processor(ArtifactGroup::Artifact(a)))?;
        self.for_each_default_output_other_artifacts_only(processor)?;
        self.for_each_other_output(processor)
    }

    fn for_each_in_list(
        &self,
        value: FrozenValue,
        mut processor: impl FnMut(Value) -> buck2_error::Result<()>,
    ) -> buck2_error::Result<()> {
        let outputs_list = ListRef::from_frozen_value(value)
            .unwrap_or_else(|| panic!("expected list, got `{value:?}` from info `{self:?}`"));

        for value in outputs_list.iter() {
            processor(value)?;
        }

        Ok(())
    }
}

impl PartialEq for FrozenDefaultInfo {
    // frozen default infos can be compared by ptr for a simple equality
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self, other)
    }
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum DefaultOutputError {
    #[error("Cannot specify both `default_output` and `default_outputs`.")]
    ConflictingArguments,
}

#[starlark_module]
fn default_info_creator(builder: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenDefaultInfo)]
    fn DefaultInfo<'v>(
        // TODO(nga): parameters must be named only.
        #[starlark(default = NoneOr::None)] default_output: NoneOr<
            ValueOf<'v, ValueIsInputArtifactAnnotation>,
        >,
        #[starlark(default = NoneOr::None)] default_outputs: NoneOr<
            ValueOf<'v, UnpackList<UnpackAndDiscard<ValueIsInputArtifactAnnotation>>>,
        >,
        #[starlark(default = NoneOr::None)] files: NoneOr<ValueOf<'v, &'v BazelDepset<'v>>>,
        #[starlark(default = NoneOr::None)] executable: NoneOr<
            ValueOf<'v, ValueIsInputArtifactAnnotation>,
        >,
        #[starlark(default = NoneOr::None)] runfiles: NoneOr<ValueOf<'v, &'v BazelRunfiles<'v>>>,
        #[starlark(default = NoneOr::None)] data_runfiles: NoneOr<
            ValueOf<'v, &'v BazelRunfiles<'v>>,
        >,
        #[starlark(default = NoneOr::None)] default_runfiles: NoneOr<
            ValueOf<'v, &'v BazelRunfiles<'v>>,
        >,
        #[starlark(default = ValueOf { value: FrozenValue::new_empty_list().to_value(), typed: UnpackList::default()})]
        other_outputs: ValueOf<
            'v,
            UnpackList<UnpackAndDiscard<ValueAsCommandLineLike<'v>>>,
        >,
        #[starlark(default = UnpackDictEntries::default())] sub_targets: UnpackDictEntries<
            StringValue<'v>,
            Value<'v>,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<DefaultInfo<'v>> {
        let heap = eval.heap();
        let executable = executable.into_option();
        let files_to_run_executable = executable
            .as_ref()
            .map(|executable| executable.value)
            .unwrap_or_else(Value::new_none);

        // support both list and singular options for now until we migrate all the rules.
        let (valid_default_outputs, valid_files): (
            ValueOfUnchecked<ListType<ValueIsInputArtifactAnnotation>>,
            ValueOfUnchecked<FrozenBazelDepset>,
        ) = match (
            default_outputs.into_option(),
            default_output.into_option(),
            files.into_option(),
            executable,
        ) {
            (Some(list), None, None, None) => {
                let outputs = ListRef::from_value(list.value)
                    .expect("validated default outputs should be a list")
                    .iter()
                    .collect::<Vec<_>>();
                (
                    list.as_unchecked().cast(),
                    ValueOfUnchecked::<FrozenBazelDepset>::new(bazel_depset_from_values(
                        heap, outputs,
                    )?),
                )
            }
            (None, Some(default_output), None, None) | (None, None, None, Some(default_output)) => {
                // handle where we didn't specify `default_outputs`, which means we should use the new
                // `default_output`.
                (
                    eval.heap()
                        .alloc_typed_unchecked(AllocList([default_output.as_unchecked()]))
                        .cast(),
                    ValueOfUnchecked::<FrozenBazelDepset>::new(bazel_depset_from_values(
                        heap,
                        vec![default_output.value],
                    )?),
                )
            }
            (None, None, Some(files), _) => (
                ValueOfUnchecked::<ListType<_>>::new(
                    heap.alloc(AllocList(bazel_depset_to_list(files.value)?)),
                ),
                ValueOfUnchecked::<FrozenBazelDepset>::new(files.value),
            ),
            (None, None, None, None) => (
                ValueOfUnchecked::<ListType<_>>::new(eval.heap().alloc(AllocList::EMPTY)),
                ValueOfUnchecked::<FrozenBazelDepset>::new(bazel_depset_empty(heap)),
            ),
            _ => {
                return Err(
                    buck2_error::Error::from(DefaultOutputError::ConflictingArguments).into(),
                );
            }
        };

        let runfiles = runfiles.into_option();
        let data_runfiles = data_runfiles.into_option();
        let default_runfiles = default_runfiles.into_option();
        if runfiles.is_some() && (data_runfiles.is_some() || default_runfiles.is_some()) {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Cannot specify the provider 'runfiles' together with 'data_runfiles' or 'default_runfiles'"
            )
            .into());
        }
        let valid_data_runfiles = ValueOfUnchecked::<FrozenBazelRunfiles>::new(
            data_runfiles
                .map(|data_runfiles| data_runfiles.value)
                .unwrap_or_else(|| bazel_runfiles_empty_value(heap)),
        );
        let valid_default_runfiles = ValueOfUnchecked::<FrozenBazelRunfiles>::new(
            default_runfiles
                .map(|default_runfiles| default_runfiles.value)
                .or_else(|| runfiles.map(|runfiles| runfiles.value))
                .unwrap_or_else(|| bazel_runfiles_empty_value(heap)),
        );

        let valid_sub_targets = sub_targets
            .entries
            .into_iter()
            .map(|(k, v)| {
                let as_provider_collection = ProviderCollection::try_from_value_subtarget(v, heap)?;
                Ok((
                    k,
                    ValueOfUnchecked::<FrozenProviderCollection>::new(
                        heap.alloc(as_provider_collection),
                    ),
                ))
            })
            .collect::<buck2_error::Result<Vec<(StringValue<'v>, _)>>>()?;

        Ok(DefaultInfo {
            default_outputs: valid_default_outputs,
            other_outputs: other_outputs.as_unchecked().cast(),
            files: valid_files,
            files_to_run: ValueOfUnchecked::<StructRef>::new(bazel_files_to_run(
                heap,
                files_to_run_executable,
            )),
            data_runfiles: valid_data_runfiles,
            default_runfiles: valid_default_runfiles,
            sub_targets: heap
                .alloc_typed_unchecked(AllocDict(valid_sub_targets))
                .cast(),
        })
    }
}
