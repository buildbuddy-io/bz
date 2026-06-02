/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Debug;
use std::iter;
use std::marker::PhantomData;
use std::ptr;

use allocative::Allocative;
use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::artifact_type::OutputArtifact;
use bz_build_api_derive::internal_provider;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
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
use starlark::values::dict::DictRef;
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

use crate as bz_build_api;
use crate::artifact_groups::ArtifactGroup;
use crate::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueIsInputArtifactAnnotation;
use crate::interpreter::rule_defs::artifact_tagging::ArtifactTag;
use crate::interpreter::rule_defs::bazel::depset::BazelDepset;
use crate::interpreter::rule_defs::bazel::depset::FrozenBazelDepset;
use crate::interpreter::rule_defs::bazel::depset::bazel_depset_empty;
use crate::interpreter::rule_defs::bazel::depset::bazel_depset_empty_frozen;
use crate::interpreter::rule_defs::bazel::depset::bazel_depset_from_direct_and_transitive;
use crate::interpreter::rule_defs::bazel::depset::bazel_depset_from_frozen_values;
use crate::interpreter::rule_defs::bazel::depset::bazel_depset_from_transitive;
use crate::interpreter::rule_defs::bazel::depset::bazel_depset_from_values;
use crate::interpreter::rule_defs::bazel::depset::bazel_depset_to_list;
use crate::interpreter::rule_defs::cmd_args::CommandLineArtifactVisitor;
use crate::interpreter::rule_defs::cmd_args::value_as::ValueAsCommandLineLike;
use crate::interpreter::rule_defs::context::bazel_runfiles_prefix;
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
    symlinks: ValueOfUncheckedGeneric<V, FrozenBazelDepset>,
    root_symlinks: ValueOfUncheckedGeneric<V, FrozenBazelDepset>,
    empty_filenames: ValueOfUncheckedGeneric<V, FrozenBazelDepset>,
    _marker: PhantomData<&'v ()>,
}

starlark_complex_value!(pub BazelRunfiles<'v>);

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
pub struct BazelSymlinkEntryGen<V: ValueLifetimeless> {
    path: String,
    target_file: V,
}

starlark_complex_value!(pub BazelSymlinkEntry);

impl<V: ValueLifetimeless> fmt::Display for BazelSymlinkEntryGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SymlinkEntry(path = {:?}, target_file = <computed>)",
            self.path
        )
    }
}

#[starlark_value(type = "SymlinkEntry")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for BazelSymlinkEntryGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn dir_attr(&self) -> Vec<String> {
        vec!["path".to_owned(), "target_file".to_owned()]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "path" => Some(heap.alloc_str(&self.path).to_value()),
            "target_file" => Some(self.target_file.to_value()),
            _ => None,
        }
    }
}

impl<'v, V: ValueLike<'v>> fmt::Display for BazelRunfilesGen<'v, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("runfiles")
    }
}

impl<'v, V: ValueLike<'v>> BazelRunfilesGen<'v, V> {
    pub fn files_value(&self) -> Value<'v> {
        self.files.get().to_value()
    }

    pub fn symlinks_value(&self) -> Value<'v> {
        self.symlinks.get().to_value()
    }

    pub fn root_symlinks_value(&self) -> Value<'v> {
        self.root_symlinks.get().to_value()
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

fn bazel_runfiles_from_depsets<'v>(
    files: Value<'v>,
    symlinks: Value<'v>,
    root_symlinks: Value<'v>,
    empty_filenames: Value<'v>,
) -> BazelRunfiles<'v> {
    BazelRunfiles {
        files: ValueOfUnchecked::new(files),
        symlinks: ValueOfUnchecked::new(symlinks),
        root_symlinks: ValueOfUnchecked::new(root_symlinks),
        empty_filenames: ValueOfUnchecked::new(empty_filenames),
        _marker: PhantomData,
    }
}

fn bazel_runfiles_with_file<'v>(
    heap: Heap<'v>,
    runfiles: Value<'v>,
    file: Value<'v>,
) -> starlark::Result<Value<'v>> {
    let runfiles = BazelRunfiles::from_value(runfiles).ok_or_else(|| {
        bz_error::internal_error!("DefaultInfo runfiles should be a runfiles object")
    })?;
    let files = bazel_depset_from_direct_and_transitive(
        heap,
        vec![file],
        vec![runfiles.files.get().to_value()],
    )?;
    Ok(heap.alloc(bazel_runfiles_from_depsets(
        files,
        runfiles.symlinks.get().to_value(),
        runfiles.root_symlinks.get().to_value(),
        runfiles.empty_filenames.get().to_value(),
    )))
}

fn bazel_runfiles_empty_value<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(bazel_runfiles_from_depsets(
        bazel_depset_empty(heap),
        bazel_depset_empty(heap),
        bazel_depset_empty(heap),
        bazel_depset_empty(heap),
    ))
}

fn bazel_runfiles_empty_frozen_value(heap: &FrozenHeap) -> FrozenValue {
    heap.alloc(FrozenBazelRunfiles {
        files: FrozenValueOfUnchecked::new(bazel_depset_empty_frozen(heap)),
        symlinks: FrozenValueOfUnchecked::new(bazel_depset_empty_frozen(heap)),
        root_symlinks: FrozenValueOfUnchecked::new(bazel_depset_empty_frozen(heap)),
        empty_filenames: FrozenValueOfUnchecked::new(bazel_depset_empty_frozen(heap)),
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
    let mut symlinks = Vec::new();
    let mut root_symlinks = Vec::new();
    let mut empty_filenames = Vec::new();
    for runfiles in runfiles {
        files.push(runfiles.files.get().to_value());
        symlinks.push(runfiles.symlinks.get().to_value());
        root_symlinks.push(runfiles.root_symlinks.get().to_value());
        empty_filenames.push(runfiles.empty_filenames.get().to_value());
    }
    let files = bazel_depset_from_transitive(heap, files)?;
    let symlinks = bazel_depset_from_transitive(heap, symlinks)?;
    let root_symlinks = bazel_depset_from_transitive(heap, root_symlinks)?;
    let empty_filenames = bazel_depset_from_transitive(heap, empty_filenames)?;
    Ok(bazel_runfiles_from_depsets(
        files,
        symlinks,
        root_symlinks,
        empty_filenames,
    ))
}

pub(crate) fn bazel_runfiles_from_files<'v>(
    heap: Heap<'v>,
    direct_files: impl IntoIterator<Item = Value<'v>>,
    transitive_files: Option<Value<'v>>,
    symlinks: Option<Value<'v>>,
    root_symlinks: Option<Value<'v>>,
) -> starlark::Result<BazelRunfiles<'v>> {
    let mut files = Vec::new();
    for file in direct_files {
        files.push(file);
    }
    let files = bazel_depset_from_direct_and_transitive(
        heap,
        files,
        transitive_files.into_iter().collect(),
    )?;
    let symlinks = bazel_runfiles_symlinks_from_value(heap, symlinks, "symlinks")?;
    let root_symlinks = bazel_runfiles_symlinks_from_value(heap, root_symlinks, "root_symlinks")?;
    Ok(bazel_runfiles_from_depsets(
        files,
        symlinks,
        root_symlinks,
        bazel_depset_empty(heap),
    ))
}

fn bazel_runfiles_symlinks_from_value<'v>(
    heap: Heap<'v>,
    value: Option<Value<'v>>,
    arg_name: &'static str,
) -> starlark::Result<Value<'v>> {
    let Some(value) = value else {
        return Ok(bazel_depset_empty(heap));
    };
    if value.is_none() {
        return Ok(bazel_depset_empty(heap));
    }
    if BazelDepset::from_value(value).is_some() {
        return Ok(value);
    }
    let dict = DictRef::from_value(value).ok_or_else(|| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "ctx.runfiles argument `{}` expected dict or depset, got `{}`",
            arg_name,
            value.to_string_for_type_error()
        )
    })?;
    let mut symlink_entries = Vec::with_capacity(dict.len());
    for (path, target_file) in dict.iter() {
        let path = path.unpack_str().ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "ctx.runfiles argument `{}` expected string keys, got `{}`",
                arg_name,
                path.to_string_for_type_error()
            )
        })?;
        ValueIsInputArtifactAnnotation::unpack_value(target_file)?.ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "ctx.runfiles argument `{}` expected File values, got `{}`",
                arg_name,
                target_file.to_string_for_type_error()
            )
        })?;
        symlink_entries.push(
            heap.alloc(BazelSymlinkEntry {
                path: path.to_owned(),
                target_file,
            })
            .to_value(),
        );
    }
    bazel_depset_from_values(heap, symlink_entries)
}

fn path_parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn path_file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn path_join(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}/{child}")
    }
}

fn path_is_multi_segment(path: &str) -> bool {
    path.contains('/')
}

fn path_ends_with_segment(path: &str, segment: &str) -> bool {
    path_file_name(path) == segment
}

fn path_requires_init(path: &str) -> bool {
    path.ends_with(".py") || path.ends_with(".so") || path.ends_with(".pyc")
}

fn path_is_package_init(path: &str) -> bool {
    matches!(path_file_name(path), "__init__.py" | "__init__.pyc")
}

fn bazel_runfiles_raw_file_path<'v>(heap: Heap<'v>, file: Value<'v>) -> starlark::Result<String> {
    let file = ValueAsInputArtifactLike::unpack_value(file)?.ok_or_else(|| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "runfiles.files expected File values, got `{}`",
            file.to_string_for_type_error()
        )
    })?;
    let path = file
        .0
        .with_bazel_short_path(&|short_path| heap.alloc_str(short_path))?
        .as_str()
        .to_owned();
    Ok(path)
}

fn bazel_runfiles_file_path<'v>(heap: Heap<'v>, file: Value<'v>) -> starlark::Result<String> {
    let path = bazel_runfiles_raw_file_path(heap, file)?;
    Ok(bazel_runfiles_prefixed_path(&path))
}

fn bazel_runfiles_prefixed_path(path: &str) -> String {
    if let Some(external_path) = path.strip_prefix("../") {
        external_path.to_owned()
    } else {
        path_join(bazel_runfiles_prefix(), path)
    }
}

fn bazel_runfiles_symlink_path<'v>(heap: Heap<'v>, symlink: Value<'v>) -> starlark::Result<String> {
    if let Some(symlink) = BazelSymlinkEntry::from_value(symlink) {
        return Ok(symlink.path.clone());
    }
    let path = symlink.get_attr_error("path", heap)?;
    path.unpack_str().map(str::to_owned).ok_or_else(|| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "runfiles.symlinks expected SymlinkEntry values, got `{}`",
            symlink.to_string_for_type_error()
        )
        .into()
    })
}

fn bazel_runfiles_symlink_target_file<'v>(
    heap: Heap<'v>,
    symlink: Value<'v>,
) -> starlark::Result<Value<'v>> {
    if let Some(symlink) = BazelSymlinkEntry::from_value(symlink) {
        return Ok(symlink.target_file.to_value());
    }
    let target_file = symlink.get_attr_error("target_file", heap)?;
    if ValueAsInputArtifactLike::unpack_value(target_file)?.is_none() {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "runfiles.symlinks expected SymlinkEntry target_file values to be File, got `{}`",
            target_file.to_string_for_type_error()
        )
        .into());
    }
    Ok(target_file)
}

fn bazel_runfiles_artifact_entry<'v>(
    heap: Heap<'v>,
    path: &str,
    target_file: Value<'v>,
) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("path", heap.alloc_str(path).to_value()),
        ("target_file", target_file),
    ]))
}

pub fn bazel_runfiles_artifact_entries<'v>(
    heap: Heap<'v>,
    runfiles: &BazelRunfiles<'v>,
) -> starlark::Result<Value<'v>> {
    let mut entries = Vec::new();
    for file in bazel_depset_to_list(runfiles.files.get().to_value())? {
        let path = bazel_runfiles_file_path(heap, file)?;
        entries.push(bazel_runfiles_artifact_entry(heap, &path, file));
    }
    for symlink in bazel_depset_to_list(runfiles.symlinks.get().to_value())? {
        let path = bazel_runfiles_prefixed_path(&bazel_runfiles_symlink_path(heap, symlink)?);
        let target_file = bazel_runfiles_symlink_target_file(heap, symlink)?;
        entries.push(bazel_runfiles_artifact_entry(heap, &path, target_file));
    }
    for symlink in bazel_depset_to_list(runfiles.root_symlinks.get().to_value())? {
        let path = bazel_runfiles_symlink_path(heap, symlink)?;
        let target_file = bazel_runfiles_symlink_target_file(heap, symlink)?;
        entries.push(bazel_runfiles_artifact_entry(heap, &path, target_file));
    }
    Ok(heap.alloc(AllocList(entries)))
}

fn generated_init_empty_filenames(manifest_paths: BTreeSet<String>) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut has_package_init_dirs = BTreeSet::new();

    for source in &manifest_paths {
        if path_is_package_init(source) {
            if let Some(parent) = path_parent(source) {
                has_package_init_dirs.insert(parent.to_owned());
            }
        }
    }

    for source in &manifest_paths {
        if !path_requires_init(source) {
            continue;
        }
        let mut current = source.as_str();
        while path_is_multi_segment(current) {
            let Some(parent) = path_parent(current) else {
                break;
            };
            current = parent;
            if path_ends_with_segment(current, "__pycache__")
                || has_package_init_dirs.contains(current)
            {
                continue;
            }
            let init_py = path_join(current, "__init__.py");
            let init_pyc = path_join(current, "__init__.pyc");
            if !manifest_paths.contains(&init_py) && !manifest_paths.contains(&init_pyc) {
                result.insert(init_py);
            }
        }
    }

    result
}

/// Bazel Python runfiles helper that adds generated empty `__init__.py` entries.
pub fn bazel_runfiles_with_generated_inits_empty_files_supplier<'v>(
    heap: Heap<'v>,
    runfiles: &BazelRunfiles<'v>,
) -> starlark::Result<BazelRunfiles<'v>> {
    let files = bazel_depset_to_list(runfiles.files.get().to_value())?;
    let symlinks = bazel_depset_to_list(runfiles.symlinks.get().to_value())?;
    let root_symlinks = bazel_depset_to_list(runfiles.root_symlinks.get().to_value())?;
    if files.is_empty() && symlinks.is_empty() && root_symlinks.is_empty() {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "input runfiles cannot be empty"
        )
        .into());
    }

    let mut manifest_paths = BTreeSet::new();
    for file in &files {
        manifest_paths.insert(bazel_runfiles_raw_file_path(heap, *file)?);
    }
    for symlink in &symlinks {
        manifest_paths.insert(bazel_runfiles_symlink_path(heap, *symlink)?);
    }

    let mut empty_filenames = bazel_depset_to_list(runfiles.empty_filenames.get().to_value())?;
    for empty_filename in generated_init_empty_filenames(manifest_paths) {
        push_unique_value(
            &mut empty_filenames,
            heap.alloc_str(&empty_filename).to_value(),
        )?;
    }

    Ok(bazel_runfiles_from_depsets(
        runfiles.files.get().to_value(),
        runfiles.symlinks.get().to_value(),
        runfiles.root_symlinks.get().to_value(),
        bazel_depset_from_values(heap, empty_filenames)?,
    ))
}

#[starlark_module]
fn bazel_runfiles_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn files<'v>(this: &BazelRunfiles<'v>) -> starlark::Result<Value<'v>> {
        Ok(this.files.get().to_value())
    }

    #[starlark(attribute)]
    fn symlinks<'v>(this: &BazelRunfiles<'v>) -> starlark::Result<Value<'v>> {
        Ok(this.symlinks.get().to_value())
    }

    #[starlark(attribute)]
    fn root_symlinks<'v>(this: &BazelRunfiles<'v>) -> starlark::Result<Value<'v>> {
        Ok(this.root_symlinks.get().to_value())
    }

    #[starlark(attribute)]
    fn empty_filenames<'v>(this: &BazelRunfiles<'v>) -> starlark::Result<Value<'v>> {
        Ok(this.empty_filenames.get().to_value())
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
                bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
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
    /// subtarget, such as when doing `bz build cell//foo:bar[baz]`. Just like any
    /// `ProviderCollection`, this collection must include at least a `DefaultInfo` provider. The
    /// subtargets can have their own subtargets as well, which can be accessed by chaining them,
    /// e.g.: `bz build cell//foo:bar[baz][qux]`.
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

pub const BAZEL_FILES_TO_RUN_EXECUTABLE_FIELD: &str = "executable";
pub const BAZEL_FILES_TO_RUN_RUNFILES_FIELD: &str = "_bz_default_runfiles";

pub fn bazel_files_to_run_executable<'v>(value: Value<'v>) -> Option<Value<'v>> {
    StructRef::from_value(value).and_then(|st| {
        st.iter().find_map(|(name, value)| {
            (name.as_str() == BAZEL_FILES_TO_RUN_EXECUTABLE_FIELD && !value.is_none())
                .then_some(value)
        })
    })
}

pub fn bazel_files_to_run_runfiles<'v>(value: Value<'v>) -> Option<Value<'v>> {
    StructRef::from_value(value).and_then(|st| {
        st.iter().find_map(|(name, value)| {
            (name.as_str() == BAZEL_FILES_TO_RUN_RUNFILES_FIELD && !value.is_none())
                .then_some(value)
        })
    })
}

fn bazel_files_to_run<'v>(
    heap: Heap<'v>,
    executable: Value<'v>,
    default_runfiles: Value<'v>,
) -> Value<'v> {
    heap.alloc(AllocStruct([
        (BAZEL_FILES_TO_RUN_EXECUTABLE_FIELD, executable),
        ("repo_mapping_manifest", Value::new_none()),
        ("runfiles_manifest", Value::new_none()),
        (BAZEL_FILES_TO_RUN_RUNFILES_FIELD, default_runfiles),
    ]))
}

fn validate_default_info(info: &FrozenDefaultInfo) -> bz_error::Result<()> {
    // Check length of default outputs
    let default_output_list = ListRef::from_value(info.default_outputs.get().to_value())
        .expect("should be a list from constructor");
    if default_output_list.len() > 1 {
        tracing::info!("DefaultInfo.default_output should only have a maximum of 1 item.");
        // TODO use soft_error when landed
        // TODO error rather than soft warning
        // return Err(bz_error::bz_error!(
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
        let data_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        let default_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        let files_to_run = ValueOfUnchecked::<StructRef>::new(bazel_files_to_run(
            heap,
            Value::new_none(),
            default_runfiles.get().to_value(),
        ));
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
        let data_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        let default_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        let files_to_run = ValueOfUnchecked::<StructRef>::new(bazel_files_to_run(
            heap,
            Value::new_none(),
            default_runfiles.get().to_value(),
        ));
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
        let data_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        let default_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(bazel_runfiles_empty_value(heap));
        let files_to_run = ValueOfUnchecked::<StructRef>::new(bazel_files_to_run(
            heap,
            artifact,
            default_runfiles.get().to_value(),
        ));
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

impl<'v, V: ValueLike<'v>> DefaultInfoGen<V> {
    pub fn default_output_values_for_dependency(&self) -> bz_error::Result<Vec<Value<'v>>> {
        let default_outputs = ListRef::from_value(self.default_outputs.get().to_value())
            .ok_or_else(|| internal_error!("Should be list of artifacts"))?;
        if !default_outputs.is_empty() {
            return Ok(default_outputs.iter().collect());
        }
        Ok(bazel_depset_to_list(self.files.get().to_value())?)
    }

    pub fn files_raw_for_dependency(&self) -> Value<'v> {
        self.files.get().to_value()
    }

    pub fn files_to_run_raw_for_dependency(&self) -> Value<'v> {
        self.files_to_run.get().to_value()
    }

    pub fn data_runfiles_raw_for_dependency(&self) -> Value<'v> {
        self.data_runfiles.get().to_value()
    }

    pub fn default_runfiles_raw_for_dependency(&self) -> Value<'v> {
        self.default_runfiles.get().to_value()
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
        let data_runfiles = FrozenValueOfUnchecked::<FrozenBazelRunfiles>::new(
            bazel_runfiles_empty_frozen_value(heap),
        );
        let default_runfiles = FrozenValueOfUnchecked::<FrozenBazelRunfiles>::new(
            bazel_runfiles_empty_frozen_value(heap),
        );
        let files_to_run = FrozenValueOfUnchecked::<StructRef>::new(heap.alloc(AllocStruct([
            (BAZEL_FILES_TO_RUN_EXECUTABLE_FIELD, FrozenValue::new_none()),
            ("repo_mapping_manifest", FrozenValue::new_none()),
            ("runfiles_manifest", FrozenValue::new_none()),
            (BAZEL_FILES_TO_RUN_RUNFILES_FIELD, default_runfiles.get()),
        ])));
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

    pub fn for_file_target(
        heap: &FrozenHeap,
        artifact: FrozenValue,
    ) -> FrozenValueTyped<'static, FrozenDefaultInfo> {
        let sub_targets = heap
            .alloc_typed_unchecked(AllocDict(
                iter::empty::<(String, FrozenProviderCollection)>(),
            ))
            .cast();
        let default_outputs =
            FrozenValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList([artifact])));
        let other_outputs =
            FrozenValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList::EMPTY));
        let files = FrozenValueOfUnchecked::<FrozenBazelDepset>::new(
            bazel_depset_from_frozen_values(heap, vec![artifact]),
        );
        let data_runfiles = FrozenValueOfUnchecked::<FrozenBazelRunfiles>::new(
            bazel_runfiles_empty_frozen_value(heap),
        );
        let default_runfiles = FrozenValueOfUnchecked::<FrozenBazelRunfiles>::new(
            bazel_runfiles_empty_frozen_value(heap),
        );
        let files_to_run = FrozenValueOfUnchecked::<StructRef>::new(heap.alloc(AllocStruct([
            (BAZEL_FILES_TO_RUN_EXECUTABLE_FIELD, artifact),
            ("repo_mapping_manifest", FrozenValue::new_none()),
            ("runfiles_manifest", FrozenValue::new_none()),
            (BAZEL_FILES_TO_RUN_RUNFILES_FIELD, default_runfiles.get()),
        ])));
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
    ) -> bz_error::Result<Option<FrozenValueTyped<'static, FrozenProviderCollection>>> {
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
    ) -> bz_error::Result<impl Iterator<Item = bz_error::Result<StarlarkArtifact>> + '_> {
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

    pub fn default_output_values<'v>(&self) -> bz_error::Result<Vec<Value<'v>>> {
        let default_outputs = ListRef::from_frozen_value(self.default_outputs.get())
            .ok_or_else(|| internal_error!("Should be list of artifacts"))?;
        if !default_outputs.is_empty() {
            return Ok(default_outputs.iter().collect());
        }
        Ok(bazel_depset_to_list(self.files.get().to_value())?)
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
    ) -> bz_error::Result<
        impl Iterator<
            Item = bz_error::Result<(&str, FrozenValueTyped<'static, FrozenProviderCollection>)>,
        > + '_,
    > {
        let sub_targets = FrozenDictRef::from_frozen_value(self.sub_targets.get())
            .ok_or_else(|| internal_error!("sub_targets should be a dict-like object"))?;

        Ok(sub_targets.iter().map(|(k, v)| {
            bz_error::Ok((
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
    ) -> bz_error::Result<()> {
        self.for_each_default_output_value(|value| {
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
    ) -> bz_error::Result<()> {
        self.for_each_default_output_value(|value| {
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
    ) -> bz_error::Result<()> {
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

    pub fn for_each_default_runfiles_artifact(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup),
    ) -> bz_error::Result<()> {
        let runfiles = self.default_runfiles.get().to_value();
        let runfiles = runfiles
            .downcast_ref::<FrozenBazelRunfiles>()
            .ok_or_else(|| internal_error!("DefaultInfo.default_runfiles should be runfiles"))?;

        for value in bazel_depset_to_list(runfiles.files.get().to_value())? {
            let artifact = ValueAsInputArtifactLike::unpack_value_err(value)?
                .0
                .get_bound_artifact()?;
            processor(ArtifactGroup::Artifact(artifact));
        }

        for value in bazel_depset_to_list(runfiles.symlinks.get().to_value())?
            .into_iter()
            .chain(bazel_depset_to_list(
                runfiles.root_symlinks.get().to_value(),
            )?)
        {
            let target_file = if let Some(symlink) = BazelSymlinkEntry::from_value(value) {
                symlink.target_file.to_value()
            } else if let Some(symlink) = value.downcast_ref::<FrozenBazelSymlinkEntry>() {
                symlink.target_file.to_value()
            } else {
                return Err(internal_error!(
                    "DefaultInfo.default_runfiles symlink entry should be SymlinkEntry"
                ));
            };
            let artifact = ValueAsInputArtifactLike::unpack_value_err(target_file)?
                .0
                .get_bound_artifact()?;
            processor(ArtifactGroup::Artifact(artifact));
        }

        Ok(())
    }

    pub fn for_each_output(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup),
    ) -> bz_error::Result<()> {
        self.for_each_default_output_artifact_only(&mut |a| processor(ArtifactGroup::Artifact(a)))?;
        self.for_each_default_output_other_artifacts_only(processor)?;
        self.for_each_other_output(processor)
    }

    fn for_each_default_output_value(
        &self,
        mut processor: impl FnMut(Value) -> bz_error::Result<()>,
    ) -> bz_error::Result<()> {
        let default_outputs = ListRef::from_frozen_value(self.default_outputs.get())
            .ok_or_else(|| internal_error!("Should be list of artifacts"))?;
        if !default_outputs.is_empty() {
            for value in default_outputs.iter() {
                processor(value)?;
            }
            return Ok(());
        }

        for value in bazel_depset_to_list(self.files.get().to_value())? {
            processor(value)?;
        }
        Ok(())
    }

    fn for_each_in_list(
        &self,
        value: FrozenValue,
        mut processor: impl FnMut(Value) -> bz_error::Result<()>,
    ) -> bz_error::Result<()> {
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

#[derive(Debug, bz_error::Error)]
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
        let executable_value = files_to_run_executable;

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
                ValueOfUnchecked::<ListType<_>>::new(heap.alloc(AllocList::EMPTY)),
                ValueOfUnchecked::<FrozenBazelDepset>::new(files.value),
            ),
            (None, None, None, None) => (
                ValueOfUnchecked::<ListType<_>>::new(eval.heap().alloc(AllocList::EMPTY)),
                ValueOfUnchecked::<FrozenBazelDepset>::new(bazel_depset_empty(heap)),
            ),
            _ => {
                return Err(
                    bz_error::Error::from(DefaultOutputError::ConflictingArguments).into(),
                );
            }
        };

        let runfiles = runfiles.into_option();
        let data_runfiles = data_runfiles.into_option();
        let default_runfiles = default_runfiles.into_option();
        let no_runfiles_arguments =
            runfiles.is_none() && data_runfiles.is_none() && default_runfiles.is_none();
        if runfiles.is_some() && (data_runfiles.is_some() || default_runfiles.is_some()) {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "Cannot specify the provider 'runfiles' together with 'data_runfiles' or 'default_runfiles'"
            )
            .into());
        }
        let mut valid_data_runfiles = data_runfiles
            .map(|data_runfiles| data_runfiles.value)
            .or_else(|| runfiles.map(|runfiles| runfiles.value))
            .unwrap_or_else(|| bazel_runfiles_empty_value(heap));
        let mut valid_default_runfiles = default_runfiles
            .map(|default_runfiles| default_runfiles.value)
            .or_else(|| runfiles.map(|runfiles| runfiles.value))
            .unwrap_or_else(|| bazel_runfiles_empty_value(heap));

        if !executable_value.is_none() {
            valid_default_runfiles =
                bazel_runfiles_with_file(heap, valid_default_runfiles, executable_value)?;
            if no_runfiles_arguments || runfiles.is_some() {
                valid_data_runfiles =
                    bazel_runfiles_with_file(heap, valid_data_runfiles, executable_value)?;
            }
        }

        let valid_data_runfiles = ValueOfUnchecked::<FrozenBazelRunfiles>::new(valid_data_runfiles);
        let valid_default_runfiles =
            ValueOfUnchecked::<FrozenBazelRunfiles>::new(valid_default_runfiles);

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
            .collect::<bz_error::Result<Vec<(StringValue<'v>, _)>>>()?;

        Ok(DefaultInfo {
            default_outputs: valid_default_outputs,
            other_outputs: other_outputs.as_unchecked().cast(),
            files: valid_files,
            files_to_run: ValueOfUnchecked::<StructRef>::new(bazel_files_to_run(
                heap,
                files_to_run_executable,
                valid_default_runfiles.get().to_value(),
            )),
            data_runfiles: valid_data_runfiles,
            default_runfiles: valid_default_runfiles,
            sub_targets: heap
                .alloc_typed_unchecked(AllocDict(valid_sub_targets))
                .cast(),
        })
    }
}
