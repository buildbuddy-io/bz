/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cell::Ref;
use std::cell::RefCell;
use std::cell::RefMut;
use std::collections::HashSet;
use std::convert::Infallible;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Formatter;
use std::marker::PhantomData;

use allocative::Allocative;
use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_artifact::artifact::artifact_type::OutputArtifact;
use buck2_error::internal_error;
use buck2_fs::paths::RelativePathBuf;
use buck2_hash::BuckIndexSet;
use display_container::display_pair;
use display_container::fmt_container;
use display_container::iter_display_chain;
use dupe::Dupe;
use either::Either;
use gazebo::prelude::*;
use serde::Serialize;
use serde::Serializer;
use starlark::any::ProvidesStaticType;
use starlark::coerce::coerce;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::static_starlark_value;
use starlark::typing::Ty;
use starlark::values::AllocValue;
use starlark::values::Demand;
use starlark::values::Freeze;
use starlark::values::FreezeResult;
use starlark::values::Freezer;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::ThinBoxSliceFrozenValue;
use starlark::values::Trace;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::ValueOf;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list::UnpackList;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::structs::StructRef;
use starlark::values::tuple::TupleRef;
use starlark::values::tuple::UnpackTuple;
use starlark::values::type_repr::StarlarkTypeRepr;
use starlark::values::typing::StarlarkCallable;
use static_assertions::assert_eq_size;

use crate::artifact_groups::ArtifactGroup;
use crate::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use crate::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use crate::interpreter::rule_defs::artifact::starlark_output_artifact::StarlarkOutputArtifact;
use crate::interpreter::rule_defs::artifact_tagging::ArtifactTag;
use crate::interpreter::rule_defs::cmd_args::ArtifactPathMapper;
use crate::interpreter::rule_defs::cmd_args::command_line_arg_like_type::command_line_arg_like_impl;
use crate::interpreter::rule_defs::cmd_args::options::CommandLineOptions;
use crate::interpreter::rule_defs::cmd_args::options::CommandLineOptionsRef;
use crate::interpreter::rule_defs::cmd_args::options::CommandLineOptionsTrait;
use crate::interpreter::rule_defs::cmd_args::options::FrozenCommandLineOptions;
use crate::interpreter::rule_defs::cmd_args::options::ParamFileFormat;
use crate::interpreter::rule_defs::cmd_args::options::ParamFileOptions;
use crate::interpreter::rule_defs::cmd_args::options::QuoteStyle;
use crate::interpreter::rule_defs::cmd_args::options::RelativeOrigin;
use crate::interpreter::rule_defs::cmd_args::regex::CmdArgsRegex;
use crate::interpreter::rule_defs::cmd_args::traits::CommandLineArgLike;
use crate::interpreter::rule_defs::cmd_args::traits::CommandLineArtifactVisitor;
use crate::interpreter::rule_defs::cmd_args::traits::CommandLineBuilder;
use crate::interpreter::rule_defs::cmd_args::traits::CommandLineContext;
use crate::interpreter::rule_defs::cmd_args::traits::SimpleCommandLineArtifactVisitor;
use crate::interpreter::rule_defs::cmd_args::traits::WriteToFileMacroVisitor;
use crate::interpreter::rule_defs::cmd_args::value::CommandLineArg;
use crate::interpreter::rule_defs::cmd_args::value::FrozenCommandLineArg;
use crate::interpreter::rule_defs::depset::BazelDepset;
use crate::interpreter::rule_defs::depset::bazel_depset_to_list;

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelDirectoryExpander;

impl Display for BazelDirectoryExpander {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("<DirectoryExpander>")
    }
}

starlark::starlark_simple_value!(BazelDirectoryExpander);

#[starlark_value(type = "DirectoryExpander")]
impl<'v> StarlarkValue<'v> for BazelDirectoryExpander {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(bazel_directory_expander_methods)
    }
}

#[starlark_module]
fn bazel_directory_expander_methods(builder: &mut MethodsBuilder) {
    fn expand<'v>(
        #[starlark(this)] _this: &BazelDirectoryExpander,
        file: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList([file])))
    }
}

#[derive(Debug, buck2_error::Error)]
pub enum CommandLineError {
    #[error("Artifact(s) {0:?} cannot be used with ignore_artifacts as they are content-based")]
    #[buck2(input)]
    ContentBasedIgnoreArtifacts(BuckIndexSet<String>),
}

/// Fields of `cmd_args`. Abstract mutable and frozen versions.
trait Fields<'v> {
    fn items(&self) -> &[CommandLineArg<'v>];
    fn hidden(&self) -> &[CommandLineArg<'v>];
    fn options(&self) -> Option<&dyn CommandLineOptionsTrait<'v>>;
}

/// Wrapper because we cannot implement traits for traits.
struct FieldsRef<'v, F: Fields<'v>>(F, PhantomData<Value<'v>>);

/// There's no good reason for a user to write `cmd_args` as JSON in analysis or BXL.
///
/// This implementation exists for operations such as:
///
/// ```ignore
/// buck2 cquery :buck2 --providers
/// ```
///
/// which must not fail if a provider contains `cmd_args` (D34887765).
impl<'v, F: Fields<'v>> Serialize for FieldsRef<'v, F> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        /// Make sure mutable and frozen `cmd_args` are serialized identically
        /// by routing through this struct.
        #[derive(Serialize)]
        struct Mirror<'v, 'a> {
            items: &'a [CommandLineArg<'v>],
            hidden: &'a [CommandLineArg<'v>],
            options: Option<CommandLineOptionsRef<'v, 'a>>,
        }

        Mirror {
            items: self.0.items(),
            hidden: self.0.hidden(),
            options: self.0.options().map(|x| x.to_command_line_options()),
        }
        .serialize(serializer)
    }
}

impl<'v, F: Fields<'v>> Display for FieldsRef<'v, F> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        fmt_container(
            f,
            "cmd_args(",
            ")",
            iter_display_chain(
                self.0.items(),
                iter_display_chain(
                    Some(self.0.hidden())
                        .filter(|x| !x.is_empty())
                        .map(|hidden| {
                            struct Wrapper<'a, 'v>(&'a [CommandLineArg<'v>]);
                            impl<'a, 'v> Display for Wrapper<'a, 'v> {
                                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                                    fmt_container(f, "[", "]", self.0.iter())
                                }
                            }
                            display_pair("hidden", "=", Wrapper(hidden))
                        }),
                    self.0
                        .options()
                        .map(|o| o.to_command_line_options())
                        .unwrap_or_default()
                        .iter_fields_display()
                        .map(|(k, v)| display_pair(k, "=", v)),
                ),
            ),
        )
    }
}

impl<'v, F: Fields<'v>> FieldsRef<'v, F> {
    fn copy(&self) -> StarlarkCmdArgs<'v> {
        StarlarkCmdArgs(RefCell::new(StarlarkCommandLineData {
            items: self.0.items().to_vec(),
            hidden: self.0.hidden().to_vec(),
            options: self
                .0
                .options()
                .map(|x| Box::new(x.to_command_line_options().to_owned())),
        }))
    }

    fn ignore_artifacts(&self) -> bool {
        self.0
            .options()
            .map(|o| o.ignore_artifacts())
            .unwrap_or_default()
    }

    fn is_concat(&self) -> bool {
        if let Some(x) = &self.0.options() {
            x.delimiter().is_some()
        } else {
            false
        }
    }

    fn relative_to_path<C>(
        &self,
        ctx: &C,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<Option<RelativePathBuf>>
    where
        C: CommandLineContext + ?Sized,
    {
        match &self.0.options() {
            None => Ok(None),
            Some(options) => options
                .to_command_line_options()
                .relative_to_path(ctx, artifact_path_mapping),
        }
    }
}

impl<'v, F: Fields<'v>> CommandLineArgLike<'v> for FieldsRef<'v, F> {
    fn register_me(&self) {
        command_line_arg_like_impl!(StarlarkCmdArgs::starlark_type_repr());
    }

    fn add_to_command_line(
        &self,
        cli: &mut dyn CommandLineBuilder,
        context: &mut dyn CommandLineContext,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        match self.0.options() {
            None => {
                for item in self.0.items() {
                    item.as_command_line_arg().add_to_command_line(
                        cli,
                        context,
                        artifact_path_mapping,
                    )?;
                }
                Ok(())
            }
            Some(options) => {
                let mut options = options.to_command_line_options();
                if let Some(param_file) = options.param_file.and_then(|param_file| {
                    param_file
                        .arg_format
                        .map(|arg_format| (param_file, arg_format))
                }) {
                    let (param_file, arg_format) = param_file;
                    options.param_file = None;
                    let mut rendered = Vec::new();
                    let expand_directories = options.expand_directories;
                    options.wrap_builder(
                        &mut rendered,
                        context,
                        |cli, context| {
                            for item in self.0.items() {
                                let item = item.as_command_line_arg();
                                if expand_directories {
                                    item.add_to_command_line_expanding_directories(
                                        cli,
                                        context,
                                        artifact_path_mapping,
                                    )?;
                                } else {
                                    item.add_to_command_line(cli, context, artifact_path_mapping)?;
                                }
                            }
                            Ok(())
                        },
                        artifact_path_mapping,
                    )?;

                    if !param_file.use_always && !bazel_param_file_threshold_exceeded(&rendered) {
                        for arg in rendered {
                            cli.push_arg(arg);
                        }
                        return Ok(());
                    }

                    let mut retained_args = Vec::new();
                    let param_file_args = match param_file.format {
                        ParamFileFormat::Shell
                        | ParamFileFormat::Multiline
                        | ParamFileFormat::GccQuoted
                        | ParamFileFormat::Windows => rendered,
                        ParamFileFormat::FlagPerLine => {
                            let mut param_file_args = Vec::new();
                            for arg in rendered {
                                if arg.starts_with("--") {
                                    param_file_args.push(arg);
                                } else {
                                    retained_args.push(arg);
                                }
                            }
                            param_file_args
                        }
                    };
                    let param_file_args = param_file_args
                        .into_iter()
                        .map(|arg| context.normalize_param_file_arg(arg))
                        .collect();
                    let param_file_path = context
                        .add_param_file_args(param_file_args, param_file.format)?
                        .into_string();
                    cli.push_arg(arg_format.as_str().replace("{}", &param_file_path));
                    for arg in retained_args {
                        cli.push_arg(arg);
                    }
                    return Ok(());
                }

                let expand_directories = options.expand_directories;
                options.wrap_builder(
                    cli,
                    context,
                    |cli, context| {
                        for item in self.0.items() {
                            let item = item.as_command_line_arg();
                            if expand_directories {
                                item.add_to_command_line_expanding_directories(
                                    cli,
                                    context,
                                    artifact_path_mapping,
                                )?;
                            } else {
                                item.add_to_command_line(cli, context, artifact_path_mapping)?;
                            }
                        }
                        Ok(())
                    },
                    artifact_path_mapping,
                )
            }
        }
    }

    fn visit_artifacts(
        &self,
        visitor: &mut dyn CommandLineArtifactVisitor<'v>,
    ) -> buck2_error::Result<()> {
        if !self.ignore_artifacts() {
            fn visit_items<'a>(
                visitor: &mut dyn CommandLineArtifactVisitor<'a>,
                items: &[CommandLineArg<'a>],
            ) -> buck2_error::Result<()> {
                for item in items {
                    visitor.push_frame()?;
                    item.as_command_line_arg().visit_artifacts(visitor)?;
                    visitor.pop_frame();
                }

                Ok(())
            }

            visit_items(visitor, self.0.items())?;
            if !visitor.skip_hidden() {
                visit_items(visitor, self.0.hidden())?;
            }
        } else {
            struct IgnoredArtifactsVisitor {
                content_based_artifacts: BuckIndexSet<String>,
            }

            impl IgnoredArtifactsVisitor {
                fn new() -> Self {
                    Self {
                        content_based_artifacts: BuckIndexSet::default(),
                    }
                }
            }

            impl<'v> CommandLineArtifactVisitor<'v> for IgnoredArtifactsVisitor {
                fn visit_input(&mut self, input: ArtifactGroup, _tags: Vec<&ArtifactTag>) {
                    if input.path_resolution_may_require_artifact_value() {
                        self.content_based_artifacts.insert(input.to_string());
                    }
                }

                fn visit_declared_artifact(
                    &mut self,
                    declared_artifact: buck2_artifact::artifact::artifact_type::DeclaredArtifact,
                    _tags: Vec<&ArtifactTag>,
                ) -> buck2_error::Result<()> {
                    if declared_artifact.has_content_based_path() {
                        self.content_based_artifacts
                            .insert(declared_artifact.to_string());
                    }

                    Ok(())
                }

                fn visit_declared_output(
                    &mut self,
                    _artifact: OutputArtifact<'v>,
                    _tags: Vec<&ArtifactTag>,
                ) {
                }

                fn visit_frozen_output(&mut self, _artifact: Artifact, _tags: Vec<&ArtifactTag>) {}
            }
            let mut ignored_artifacts_visitor = IgnoredArtifactsVisitor::new();
            for item in self.0.items().iter().chain(self.0.hidden().iter()) {
                ignored_artifacts_visitor.push_frame()?;
                item.as_command_line_arg()
                    .visit_artifacts(&mut ignored_artifacts_visitor)?;
                ignored_artifacts_visitor.pop_frame();
            }
            if !ignored_artifacts_visitor.content_based_artifacts.is_empty() {
                return Err(CommandLineError::ContentBasedIgnoreArtifacts(
                    ignored_artifacts_visitor.content_based_artifacts,
                )
                .into());
            }
        }
        Ok(())
    }

    fn contains_arg_attr(&self) -> bool {
        self.0
            .items()
            .iter()
            .any(|x| x.as_command_line_arg().contains_arg_attr())
            || self
                .0
                .hidden()
                .iter()
                .any(|x| x.as_command_line_arg().contains_arg_attr())
    }

    fn visit_write_to_file_macros(
        &self,
        visitor: &mut dyn WriteToFileMacroVisitor,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        visitor.set_current_relative_to_path(&|ctx| {
            self.relative_to_path(ctx, artifact_path_mapping)
        })?;

        for item in self.0.items() {
            item.as_command_line_arg()
                .visit_write_to_file_macros(visitor, artifact_path_mapping)?;
        }
        for item in self.0.hidden() {
            item.as_command_line_arg()
                .visit_write_to_file_macros(visitor, artifact_path_mapping)?;
        }
        Ok(())
    }
}

/// Starlark object returned by `cmd_args()`
/// A container for all of the args and nested command lines that a users adds to `ctx.args()`
///
/// This allows more efficient iterative argument building, including O(1) insertion of
/// `CommandLine` / `FrozenCommandLine` args.
///
/// When frozen, a `FrozenCommandLine` is created.
///
/// `items` contains strings, artifacts, command line args (frozen and not), but does not
///         contain any builders.
#[derive(Debug, Default, Clone, Trace, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct StarlarkCommandLineData<'v> {
    items: Vec<CommandLineArg<'v>>,
    hidden: Vec<CommandLineArg<'v>>,
    options: Option<Box<CommandLineOptions<'v>>>,
}

#[derive(Debug, Default, Clone, Trace, ProvidesStaticType, Allocative)]
pub struct StarlarkCmdArgs<'v>(RefCell<StarlarkCommandLineData<'v>>);

impl<'v> Serialize for StarlarkCmdArgs<'v> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        FieldsRef(self.0.borrow(), PhantomData).serialize(serializer)
    }
}

#[derive(Debug, ProvidesStaticType, Allocative)]
pub struct FrozenStarlarkCmdArgs {
    // Elements are `FrozenCommandLineArg`s
    items: ThinBoxSliceFrozenValue<'static>,
    hidden: ThinBoxSliceFrozenValue<'static>,
    options: FrozenCommandLineOptions,
}

static_starlark_value!(EMPTY_FROZEN_CMD_ARGS: FrozenStarlarkCmdArgs = FrozenStarlarkCmdArgs {
    items: ThinBoxSliceFrozenValue::empty(),
    hidden: ThinBoxSliceFrozenValue::empty(),
    options: FrozenCommandLineOptions::empty(),
});

impl Serialize for FrozenStarlarkCmdArgs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        FieldsRef(self, PhantomData).serialize(serializer)
    }
}

impl<'a, 'v> Fields<'v> for Ref<'a, StarlarkCommandLineData<'v>> {
    fn items(&self) -> &[CommandLineArg<'v>] {
        &self.items
    }

    fn hidden(&self) -> &[CommandLineArg<'v>] {
        &self.hidden
    }

    fn options(&self) -> Option<&dyn CommandLineOptionsTrait<'v>> {
        match &self.options {
            None => None,
            Some(x) => Some(&**x),
        }
    }
}

impl<'v> Fields<'v> for FrozenStarlarkCmdArgs {
    fn items(&self) -> &[CommandLineArg<'v>] {
        coerce(FrozenCommandLineArg::slice_from_frozen_value_unchecked(
            &self.items,
        ))
    }

    fn hidden(&self) -> &[CommandLineArg<'v>] {
        coerce(FrozenCommandLineArg::slice_from_frozen_value_unchecked(
            &self.hidden,
        ))
    }

    fn options(&self) -> Option<&dyn CommandLineOptionsTrait<'v>> {
        if self.options.is_empty() {
            None
        } else {
            Some(&self.options)
        }
    }
}

impl<'v, F: Fields<'v>> Fields<'v> for &F {
    fn items(&self) -> &[CommandLineArg<'v>] {
        (*self).items()
    }

    fn hidden(&self) -> &[CommandLineArg<'v>] {
        (*self).hidden()
    }

    fn options(&self) -> Option<&dyn CommandLineOptionsTrait<'v>> {
        (*self).options()
    }
}

impl<'v, A: Fields<'v>, B: Fields<'v>> Fields<'v> for Either<A, B> {
    fn items(&self) -> &[CommandLineArg<'v>] {
        match self {
            Either::Left(x) => x.items(),
            Either::Right(x) => x.items(),
        }
    }

    fn hidden(&self) -> &[CommandLineArg<'v>] {
        match self {
            Either::Left(x) => x.hidden(),
            Either::Right(x) => x.hidden(),
        }
    }

    fn options(&self) -> Option<&dyn CommandLineOptionsTrait<'v>> {
        match self {
            Either::Left(x) => x.options(),
            Either::Right(x) => x.options(),
        }
    }
}

// These types show up a lot in the frozen heaps, so make sure they don't regress
assert_eq_size!(StarlarkCmdArgs<'static>, [usize; 8]);
assert_eq_size!(FrozenStarlarkCmdArgs, [usize; 3]);
assert_eq_size!(CommandLineOptions<'static>, [usize; 11]);

impl<'v> Display for StarlarkCmdArgs<'v> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.0.try_borrow() {
            Ok(x) => Display::fmt(&FieldsRef(x, PhantomData), f),
            Err(_) => write!(f, "<cmd_args borrowed>"),
        }
    }
}

impl Display for FrozenStarlarkCmdArgs {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&FieldsRef(self, PhantomData), f)
    }
}

impl<'v> StarlarkCommandLineData<'v> {
    fn options_mut(&mut self) -> &mut CommandLineOptions<'v> {
        if self.options.is_none() {
            self.options = Some(Box::default());
        }
        self.options.as_mut().unwrap()
    }
}

impl<'v> StarlarkCmdArgs<'v> {
    pub(crate) fn is_concat(&self) -> bool {
        FieldsRef(self.0.borrow(), PhantomData).is_concat()
    }
}

impl FrozenStarlarkCmdArgs {
    pub(crate) fn is_concat(&self) -> bool {
        FieldsRef(self, PhantomData).is_concat()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl<'v> StarlarkCmdArgs<'v> {
    pub fn is_empty(&self) -> bool {
        self.0.borrow().items.is_empty()
    }
}

#[starlark_value(type = "cmd_args")]
impl<'v> StarlarkValue<'v> for StarlarkCmdArgs<'v> {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(cmd_args_methods)
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn CommandLineArgLike>(self);
    }

    fn try_freeze_directly(&self, _freezer: &Freezer<'_>) -> Option<FreezeResult<FrozenValue>> {
        let StarlarkCommandLineData {
            items,
            hidden,
            options,
        } = &*self.0.borrow();
        if items.is_empty() && hidden.is_empty() && options.is_none() {
            Some(Ok(EMPTY_FROZEN_CMD_ARGS.unpack().to_frozen_value()))
        } else {
            None
        }
    }
}

#[starlark_value(type = "cmd_args")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkCmdArgs {
    type Canonical = StarlarkCmdArgs<'v>;

    fn get_methods() -> Option<&'static Methods> {
        // We return the same methods for frozen command lines, even though some of them fail,
        // so the methods remain consistent during freezing
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(cmd_args_methods)
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn CommandLineArgLike<'v>>(self);
    }
}

impl<'v> AllocValue<'v> for StarlarkCmdArgs<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

impl<'v> CommandLineArgLike<'v> for StarlarkCmdArgs<'v> {
    fn register_me(&self) {
        command_line_arg_like_impl!(StarlarkCmdArgs::starlark_type_repr());
    }

    fn add_to_command_line(
        &self,
        cli: &mut dyn CommandLineBuilder,
        context: &mut dyn CommandLineContext,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        FieldsRef(self.0.borrow(), PhantomData).add_to_command_line(
            cli,
            context,
            artifact_path_mapping,
        )
    }

    fn visit_artifacts(
        &self,
        visitor: &mut dyn CommandLineArtifactVisitor<'v>,
    ) -> buck2_error::Result<()> {
        FieldsRef(self.0.borrow(), PhantomData).visit_artifacts(visitor)
    }

    fn contains_arg_attr(&self) -> bool {
        FieldsRef(self.0.borrow(), PhantomData).contains_arg_attr()
    }

    fn visit_write_to_file_macros(
        &self,
        visitor: &mut dyn WriteToFileMacroVisitor,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        FieldsRef(self.0.borrow(), PhantomData)
            .visit_write_to_file_macros(visitor, artifact_path_mapping)
    }
}

impl<'v> CommandLineArgLike<'v> for FrozenStarlarkCmdArgs {
    fn register_me(&self) {
        command_line_arg_like_impl!(FrozenStarlarkCmdArgs::starlark_type_repr());
    }

    fn add_to_command_line(
        &self,
        cli: &mut dyn CommandLineBuilder,
        context: &mut dyn CommandLineContext,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        FieldsRef(self, PhantomData).add_to_command_line(cli, context, artifact_path_mapping)
    }

    fn visit_artifacts(
        &self,
        visitor: &mut dyn CommandLineArtifactVisitor<'v>,
    ) -> buck2_error::Result<()> {
        FieldsRef(self, PhantomData).visit_artifacts(visitor)
    }

    fn contains_arg_attr(&self) -> bool {
        FieldsRef(self, PhantomData).contains_arg_attr()
    }

    fn visit_write_to_file_macros(
        &self,
        visitor: &mut dyn WriteToFileMacroVisitor,
        artifact_path_mapping: &dyn ArtifactPathMapper,
    ) -> buck2_error::Result<()> {
        FieldsRef(self, PhantomData).visit_write_to_file_macros(visitor, artifact_path_mapping)
    }
}

impl<'v> Freeze for StarlarkCmdArgs<'v> {
    type Frozen = FrozenStarlarkCmdArgs;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let StarlarkCommandLineData {
            items,
            hidden,
            options,
        } = self.0.into_inner();

        let items = ThinBoxSliceFrozenValue::from_iter(
            items
                .freeze(freezer)?
                .into_iter()
                .map(|a| a.to_frozen_value()),
        );
        let hidden = ThinBoxSliceFrozenValue::from_iter(
            hidden
                .freeze(freezer)?
                .into_iter()
                .map(|a| a.to_frozen_value()),
        );
        let options = options
            .try_map(|options| (*options).freeze(freezer))?
            .unwrap_or_default();

        Ok(FrozenStarlarkCmdArgs {
            items,
            hidden,
            options,
        })
    }
}

impl<'v> StarlarkCmdArgs<'v> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub fn from_values(values: impl IntoIterator<Item = Value<'v>>) -> buck2_error::Result<Self> {
        let mut builder = Self::new();
        builder.0.get_mut().add_from_iterator(values.into_iter())?;
        Ok(builder)
    }

    pub(crate) fn from_values_with_bazel_param_file(
        values: impl IntoIterator<Item = Value<'v>>,
        arg_format: StringValue<'v>,
        parameter_file_type: &str,
    ) -> buck2_error::Result<Self> {
        let format = match parameter_file_type {
            "GCC_QUOTED" => ParamFileFormat::GccQuoted,
            "UNQUOTED" => ParamFileFormat::Multiline,
            "WINDOWS" => ParamFileFormat::Windows,
            parameter_file_type => {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Invalid Bazel parameter file type `{parameter_file_type}`"
                ));
            }
        };
        let mut builder = Self::from_values(values)?;
        builder.0.get_mut().options_mut().param_file = Some(Box::new(ParamFileOptions {
            arg_format: Some(arg_format),
            format,
            use_always: true,
            format_set: true,
        }));
        Ok(builder)
    }

    pub fn add_hidden_value(&mut self, value: Value<'v>) -> buck2_error::Result<()> {
        self.0
            .get_mut()
            .add_hidden(StarlarkCommandLineValueUnpack::unpack_value_err(value)?)
    }

    pub fn add_bazel_hidden_value(
        &mut self,
        value: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<()> {
        if value.is_none() {
            return Ok(());
        }

        if let Some(executable) = bazel_files_to_run_executable(value) {
            self.add_bazel_hidden_value(executable, heap)?;
            return Ok(());
        }

        if let Some(values) = ListRef::from_value(value) {
            for item in values.iter() {
                self.add_bazel_hidden_value(item, heap)?;
            }
            return Ok(());
        }

        if let Some(values) = TupleRef::from_value(value) {
            for item in values.iter() {
                self.add_bazel_hidden_value(item, heap)?;
            }
            return Ok(());
        }

        if let Some(value) = StarlarkCommandLineValueUnpack::unpack_value(value)? {
            self.0.get_mut().add_hidden(value)?;
            return Ok(());
        }

        let values = value.iterate(heap).map_err(|_| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "expected hidden action input/tool to be a command-line value, sequence, or depset, got `{}`",
                value.get_type()
            )
        })?;
        for item in values {
            self.add_bazel_hidden_value(item, heap)?;
        }
        Ok(())
    }

    pub fn try_from_value(value: Value<'v>) -> buck2_error::Result<Self> {
        Self::try_from_value_typed(StarlarkCommandLineValueUnpack::unpack_value_err(value)?)
    }

    pub fn try_from_value_typed(
        value: StarlarkCommandLineValueUnpack<'v>,
    ) -> buck2_error::Result<Self> {
        let mut builder = Self::new();
        builder.0.get_mut().add_value_typed(value)?;
        Ok(builder)
    }
}

fn bazel_files_to_run_executable<'v>(value: Value<'v>) -> Option<Value<'v>> {
    StructRef::from_value(value).and_then(|st| {
        st.iter().find_map(|(name, value)| {
            (name.as_str() == "executable" && !value.is_none()).then_some(value)
        })
    })
}

#[derive(UnpackValue, StarlarkTypeRepr)]
pub enum StarlarkCommandLineValueUnpack<'v> {
    // This should be `list[Self]`, but we cannot express it.
    List(&'v ListRef<'v>),
    CommandLineArg(CommandLineArg<'v>),
}

impl<'v> StarlarkCommandLineData<'v> {
    fn add_value(&mut self, value: Value<'v>) -> buck2_error::Result<()> {
        self.add_value_typed(StarlarkCommandLineValueUnpack::unpack_value_err(value)?)
    }

    fn add_value_typed(
        &mut self,
        value: StarlarkCommandLineValueUnpack<'v>,
    ) -> buck2_error::Result<()> {
        match value {
            StarlarkCommandLineValueUnpack::List(values) => self.add_values(values.content())?,
            StarlarkCommandLineValueUnpack::CommandLineArg(value) => self.items.push(value),
        }
        Ok(())
    }

    /// Check the types of a list of values, and modify `data` accordingly
    ///
    /// The values must be one of: CommandLineArgLike or a list thereof.
    fn add_values(&mut self, values: &[Value<'v>]) -> buck2_error::Result<()> {
        self.items.reserve(values.len());
        for value in values {
            self.add_value(*value)?
        }
        Ok(())
    }

    fn add_from_iterator(
        &mut self,
        values: impl Iterator<Item = Value<'v>>,
    ) -> buck2_error::Result<()> {
        let (lower, upper) = values.size_hint();
        self.items.reserve(upper.unwrap_or(lower));
        values
            .into_iter()
            .try_for_each(|value| self.add_value(value))?;
        Ok(())
    }

    /// Add values to the artifact that don't show up on the command line, but do for dependency
    fn add_hidden(&mut self, value: StarlarkCommandLineValueUnpack<'v>) -> buck2_error::Result<()> {
        match value {
            StarlarkCommandLineValueUnpack::List(values) => {
                for value in values.content() {
                    self.add_hidden(StarlarkCommandLineValueUnpack::unpack_value_err(*value)?)?
                }
            }
            StarlarkCommandLineValueUnpack::CommandLineArg(arg) => {
                self.hidden.push(arg);
            }
        }
        Ok(())
    }
}

fn bazel_args_format_string<'v>(
    value: NoneOr<StringValue<'v>>,
    heap: Heap<'v>,
    parameter: &str,
) -> starlark::Result<Option<StringValue<'v>>> {
    let Some(value) = value.into_option() else {
        return Ok(None);
    };
    Ok(Some(bazel_args_format_literal(
        value.as_str(),
        heap,
        parameter,
    )?))
}

fn bazel_args_format_literal<'v>(
    format: &str,
    heap: Heap<'v>,
    parameter: &str,
) -> starlark::Result<StringValue<'v>> {
    let mut converted = String::with_capacity(format.len());
    let mut idx = 0;
    let mut found = false;
    while let Some(next) = format[idx..].find('%') {
        let next = idx + next;
        converted.push_str(&format[idx..next]);
        let Some(escaped) = format[next + 1..].chars().next() else {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid value for parameter `{}`: expected string with a single `%s`, got `{}`",
                parameter,
                format
            )
            .into());
        };
        match escaped {
            's' if !found => {
                converted.push_str("{}");
                found = true;
            }
            's' => {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Invalid value for parameter `{}`: expected string with a single `%s`, got `{}`",
                    parameter,
                    format
                )
                .into());
            }
            '%' => converted.push('%'),
            _ => {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Invalid value for parameter `{}`: expected string with a single `%s`, got `{}`",
                    parameter,
                    format
                )
                .into());
            }
        }
        idx = next + 1 + escaped.len_utf8();
    }
    converted.push_str(&format[idx..]);
    if !found {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "Invalid value for parameter `{}`: expected string with a single `%s`, got `{}`",
            parameter,
            format
        )
        .into());
    }
    Ok(heap.alloc_str(&converted))
}

fn bazel_param_file_threshold_exceeded(args: &[String]) -> bool {
    const BAZEL_DEFAULT_MIN_PARAM_FILE_SIZE: usize = 32 * 1024;
    args.iter().map(|arg| arg.len() + 1).sum::<usize>() > BAZEL_DEFAULT_MIN_PARAM_FILE_SIZE
}

fn bazel_args_values<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<Vec<Value<'v>>> {
    if BazelDepset::from_value(value).is_some() {
        return bazel_depset_to_list(value);
    }
    Ok(value
        .iterate(heap)
        .map_err(|_| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "expected value of type `sequence or depset`, got `{}`",
                value.get_type()
            )
        })?
        .collect())
}

fn bazel_args_scalar_to_string<'v>(value: Value<'v>, heap: Heap<'v>) -> Value<'v> {
    if value.is_none() {
        return heap.alloc_str("None").to_value();
    }
    if let Some(value) = value.unpack_i32() {
        return heap.alloc_str(&value.to_string()).to_value();
    }
    if let Some(value) = value.unpack_bool() {
        return heap
            .alloc_str(if value { "true" } else { "false" })
            .to_value();
    }
    value
}

fn bazel_args_stringify_scalars<'v>(values: Vec<Value<'v>>, heap: Heap<'v>) -> Vec<Value<'v>> {
    values
        .into_iter()
        .map(|value| bazel_args_scalar_to_string(value, heap))
        .collect()
}

fn bazel_args_extend_mapped_value<'v>(
    mapped: Value<'v>,
    values: &mut Vec<Value<'v>>,
    heap: Heap<'v>,
) -> starlark::Result<()> {
    if mapped.is_none() {
        return Ok(());
    }
    let mapped = bazel_args_scalar_to_string(mapped, heap);
    if mapped.unpack_str().is_some() {
        values.push(mapped);
        return Ok(());
    }
    match mapped.iterate(heap) {
        Ok(iter) => values.extend(iter),
        Err(_) => values.push(mapped),
    }
    Ok(())
}

fn bazel_args_apply_map_each<'v>(
    values: Vec<Value<'v>>,
    map_each: NoneOr<StarlarkCallable<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Vec<Value<'v>>> {
    let Some(map_each) = map_each.into_option() else {
        return Ok(values);
    };
    let tree_expander = if map_each
        .0
        .parameters_spec()
        .is_some_and(|parameters| parameters.len() >= 2)
    {
        Some(eval.heap().alloc_typed(BazelDirectoryExpander).to_value())
    } else {
        None
    };
    let mut mapped = Vec::new();
    for value in values {
        let result = if let Some(tree_expander) = tree_expander {
            eval.eval_function(map_each.0, &[value, tree_expander], &[])?
        } else {
            eval.eval_function(map_each.0, &[value], &[])?
        };
        bazel_args_extend_mapped_value(result, &mut mapped, eval.heap())?;
    }
    Ok(mapped)
}

fn bazel_args_uniquify(values: &mut Vec<Value<'_>>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.to_repr()));
}

fn bazel_args_nested<'v>(
    values: Vec<Value<'v>>,
    format_each: Option<StringValue<'v>>,
    before_each: Option<StringValue<'v>>,
    join_with: Option<StringValue<'v>>,
    expand_directories: bool,
) -> buck2_error::Result<StarlarkCmdArgs<'v>> {
    let mut nested = StarlarkCommandLineData::default();
    if format_each.is_some() || before_each.is_some() || join_with.is_some() || expand_directories {
        let opts = nested.options_mut();
        opts.format = format_each;
        opts.prepend = before_each;
        opts.delimiter = join_with;
        opts.expand_directories = expand_directories;
    }
    nested.add_from_iterator(values.into_iter())?;
    Ok(StarlarkCmdArgs(RefCell::new(nested)))
}

struct StarlarkCommandLineMut<'v> {
    value: Value<'v>,
    borrow: RefMut<'v, StarlarkCommandLineData<'v>>,
}

impl<'v> StarlarkTypeRepr for StarlarkCommandLineMut<'v> {
    type Canonical = <StarlarkCmdArgs<'v> as StarlarkTypeRepr>::Canonical;

    fn starlark_type_repr() -> Ty {
        StarlarkCmdArgs::starlark_type_repr()
    }
}

impl<'v> UnpackValue<'v> for StarlarkCommandLineMut<'v> {
    type Error = Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        Ok(value.downcast_ref::<StarlarkCmdArgs>().map(|v| Self {
            value,
            borrow: v.0.borrow_mut(),
        }))
    }
}

impl<'v> AllocValue<'v> for StarlarkCommandLineMut<'v> {
    fn alloc_value(self, _heap: Heap<'v>) -> Value<'v> {
        self.value
    }
}

fn cmd_args<'v>(x: Value<'v>) -> FieldsRef<'v, impl Fields<'v>> {
    if let Some(x) = x.downcast_ref::<StarlarkCmdArgs>() {
        FieldsRef(Either::Left(x.0.borrow()), PhantomData)
    } else if let Some(x) = x.downcast_ref::<FrozenStarlarkCmdArgs>() {
        FieldsRef(Either::Right(x), PhantomData)
    } else {
        unreachable!("This parameter must always be a type of command args")
    }
}

/// The `cmd_args` type is created by `cmd_args()` and is consumed by `ctx.actions.run`.
/// The type is a mutable collection of strings and `artifact` values.
/// In general, command lines, artifacts, strings, `RunInfo` and lists thereof can be added to or used to construct a `cmd_args` value.
/// All these methods operate mutably on `cmd` and return that value too.
// TODO(nga): `cmd_args` should be immutable, so that all parameters should be
//   either set in constructor, or operations like `hidden` should return a copy
//   rather than modify this. https://fburl.com/workplace/ihkplvbn
#[starlark_module]
fn cmd_args_methods(builder: &mut MethodsBuilder) {
    /// A list of arguments to be added to the command line, which may including `cmd_args`, artifacts, strings, `RunInfo` or lists thereof.
    /// Note that this operation mutates the input `cmd_args`.
    fn add<'v>(
        mut this: StarlarkCommandLineMut<'v>,
        heap: Heap<'v>,
        args: &Arguments<'v, '_>,
    ) -> starlark::Result<StarlarkCommandLineMut<'v>> {
        let mut format = None;
        for (name, value) in args.names_map()? {
            match name.as_str() {
                "format" => {
                    let Some(value) = value.unpack_str() else {
                        return Err(buck2_error::buck2_error!(
                            buck2_error::ErrorTag::Input,
                            "expected `format` to be a string, got `{}`",
                            value.get_type()
                        )
                        .into());
                    };
                    format = Some(bazel_args_format_literal(value, heap, "format")?);
                }
                _ => {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "unexpected named argument `{}` for Args.add",
                        name.as_str()
                    )
                    .into());
                }
            }
        }
        let positions = args.positions(heap)?.collect::<Vec<_>>();
        if let Some(format) = format {
            if let [arg_name, value] = positions.as_slice()
                && arg_name.unpack_str().is_some()
            {
                this.borrow.add_value(*arg_name)?;
                let values = bazel_args_stringify_scalars(vec![*value], heap);
                let nested = bazel_args_nested(values, Some(format), None, None, false)?;
                this.borrow.add_value(heap.alloc_typed(nested).to_value())?;
                return Ok(this);
            }
            let values = bazel_args_stringify_scalars(positions, heap);
            let nested = bazel_args_nested(values, Some(format), None, None, false)?;
            this.borrow.add_value(heap.alloc_typed(nested).to_value())?;
        } else {
            let values = bazel_args_stringify_scalars(positions, heap);
            this.borrow.add_from_iterator(values.into_iter())?;
        }
        Ok(this)
    }

    /// Bazel-compatible vector addition.
    fn add_all<'v>(
        mut this: StarlarkCommandLineMut<'v>,
        #[starlark(require = pos)] arg_name_or_values: Value<'v>,
        #[starlark(require = pos)] values: Option<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] map_each: NoneOr<StarlarkCallable<'v>>,
        #[starlark(require = named, default = NoneOr::None)] format_each: NoneOr<StringValue<'v>>,
        #[starlark(require = named, default = NoneOr::None)] before_each: NoneOr<StringValue<'v>>,
        #[starlark(require = named, default = true)] omit_if_empty: bool,
        #[starlark(require = named, default = false)] uniquify: bool,
        #[starlark(require = named, default = true)] expand_directories: bool,
        #[starlark(require = named, default = NoneOr::None)] terminate_with: NoneOr<
            StringValue<'v>,
        >,
        #[starlark(require = named, default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkCommandLineMut<'v>> {
        let _unused = allow_closure;
        let (arg_name, values) = match values {
            Some(values) => {
                let Some(arg_name) = arg_name_or_values.unpack_str() else {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "expected value of type `string` for arg name, got `{}`",
                        arg_name_or_values.get_type()
                    )
                    .into());
                };
                (Some(arg_name), values)
            }
            None => (None, arg_name_or_values),
        };
        let mut values = bazel_args_values(values, eval.heap())?;
        values = bazel_args_apply_map_each(values, map_each, eval)?;
        values = bazel_args_stringify_scalars(values, eval.heap());
        if values.is_empty() && omit_if_empty {
            return Ok(this);
        }
        if uniquify {
            bazel_args_uniquify(&mut values);
        }
        if let Some(arg_name) = arg_name {
            this.borrow
                .add_value(eval.heap().alloc_str(arg_name).to_value())?;
        }
        if !values.is_empty() {
            let nested = bazel_args_nested(
                values,
                bazel_args_format_string(format_each, eval.heap(), "format_each")?,
                before_each.into_option(),
                None,
                expand_directories,
            )?;
            this.borrow
                .add_value(eval.heap().alloc_typed(nested).to_value())?;
        }
        if let Some(terminate_with) = terminate_with.into_option() {
            this.borrow.add_value(terminate_with.to_value())?;
        }
        Ok(this)
    }

    /// Bazel-compatible vector addition joined into one argument.
    fn add_joined<'v>(
        mut this: StarlarkCommandLineMut<'v>,
        #[starlark(require = pos)] arg_name_or_values: Value<'v>,
        #[starlark(require = pos)] values: Option<Value<'v>>,
        #[starlark(require = named)] join_with: StringValue<'v>,
        #[starlark(require = named, default = NoneOr::None)] map_each: NoneOr<StarlarkCallable<'v>>,
        #[starlark(require = named, default = NoneOr::None)] format_each: NoneOr<StringValue<'v>>,
        #[starlark(require = named, default = NoneOr::None)] format_joined: NoneOr<StringValue<'v>>,
        #[starlark(require = named, default = true)] omit_if_empty: bool,
        #[starlark(require = named, default = false)] uniquify: bool,
        #[starlark(require = named, default = true)] expand_directories: bool,
        #[starlark(require = named, default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkCommandLineMut<'v>> {
        let _unused = allow_closure;
        let (arg_name, values) = match values {
            Some(values) => {
                let Some(arg_name) = arg_name_or_values.unpack_str() else {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "expected value of type `string` for arg name, got `{}`",
                        arg_name_or_values.get_type()
                    )
                    .into());
                };
                (Some(arg_name), values)
            }
            None => (None, arg_name_or_values),
        };
        let mut values = bazel_args_values(values, eval.heap())?;
        values = bazel_args_apply_map_each(values, map_each, eval)?;
        values = bazel_args_stringify_scalars(values, eval.heap());
        if values.is_empty() && omit_if_empty {
            return Ok(this);
        }
        if uniquify {
            bazel_args_uniquify(&mut values);
        }
        if let Some(arg_name) = arg_name {
            this.borrow
                .add_value(eval.heap().alloc_str(arg_name).to_value())?;
        }
        if values.is_empty() {
            this.borrow
                .add_value(eval.heap().alloc_str("").to_value())?;
            return Ok(this);
        }
        let joined = bazel_args_nested(
            values,
            bazel_args_format_string(format_each, eval.heap(), "format_each")?,
            None,
            Some(join_with),
            expand_directories,
        )?;
        let joined = eval.heap().alloc_typed(joined);
        if let Some(format_joined) =
            bazel_args_format_string(format_joined, eval.heap(), "format_joined")?
        {
            let outer = bazel_args_nested(
                vec![joined.to_value()],
                Some(format_joined),
                None,
                None,
                false,
            )?;
            this.borrow
                .add_value(eval.heap().alloc_typed(outer).to_value())?;
        } else {
            this.borrow.add_value(joined.to_value())?;
        }
        Ok(this)
    }

    /// Records Bazel param-file preferences for command-line lowering.
    fn use_param_file<'v>(
        mut this: StarlarkCommandLineMut<'v>,
        #[starlark(default = "@%s")] param_file_arg: &str,
        #[starlark(require = named, default = false)] use_always: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkCommandLineMut<'v>> {
        let arg_format = bazel_args_format_literal(param_file_arg, eval.heap(), "param_file_arg")?;
        let options = this.borrow.options_mut();
        let param_file = options
            .param_file
            .get_or_insert_with(|| Box::new(ParamFileOptions::default()));
        param_file.arg_format = Some(arg_format);
        param_file.use_always = use_always;
        Ok(this)
    }

    /// Records Bazel param-file formatting preferences.
    fn set_param_file_format<'v>(
        mut this: StarlarkCommandLineMut<'v>,
        #[starlark(require = pos)] format: &str,
    ) -> starlark::Result<StarlarkCommandLineMut<'v>> {
        let format = match ParamFileFormat::parse(format) {
            Ok(format) => format,
            Err(_) => return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid value for parameter `format`: expected one of `shell`, `multiline`, `flag_per_line`"
            )
            .into()),
        };
        let options = this.borrow.options_mut();
        let param_file = options
            .param_file
            .get_or_insert_with(|| Box::new(ParamFileOptions::default()));
        if param_file.format_set {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "set_param_file_format() may only be called once"
            )
            .into());
        }
        param_file.format = format;
        param_file.format_set = true;
        Ok(this)
    }

    /// Make all artifact paths relative to a given location. Typically used when the command
    /// you are running changes directory.
    ///
    /// By default, the paths are relative to the artifacts themselves (equivalent to
    /// `parent = 0`). Use `parent` to make the paths relative to an ancestor directory.
    /// For example `parent = 1` would make all paths relative to the containing dirs
    /// of any artifacts in the `cmd_args`.
    ///
    /// ```python
    /// dir = symlinked_dir(...)
    /// script = [
    ///     cmd_args(cmd_args(dir, format = "cd {}"),
    ///     original_script.relative_to(dir)
    /// ]
    /// ```
    fn relative_to<'v>(
        mut this: StarlarkCommandLineMut<'v>,
        #[starlark(require = pos)] directory: ValueOf<'v, RelativeOrigin<'v>>,
        #[starlark(require = named, default = 0u32)] parent: u32,
    ) -> starlark::Result<StarlarkCommandLineMut<'v>> {
        this.borrow.options_mut().relative_to = Some((directory.as_unchecked(), parent));
        Ok(this)
    }

    /// Returns a copy of the `cmd_args` such that any modifications to the original or the returned value will not impact each other.
    /// Note that this is a shallow copy, so any inner `cmd_args` can still be modified.
    fn copy<'v>(this: Value<'v>) -> starlark::Result<StarlarkCmdArgs<'v>> {
        Ok(cmd_args(this).copy())
    }

    /// Collect all the inputs (including hidden) referenced by this command line.
    ///
    /// The returned collection is opaque and primarily useful for:
    /// - Checking if the command has any artifact dependencies
    /// - Comparing input sets between different `cmd_args` objects
    ///
    /// The collection supports `len()` and equality comparisons but cannot be iterated.
    ///
    /// Example:
    /// ```python
    /// def _impl(ctx):
    ///     cmd = cmd_args(ctx.attrs.srcs)
    ///
    ///     # Check if command has any inputs
    ///     if len(cmd.inputs) > 0:
    ///         pass
    ///
    ///     # Compare input sets
    ///     other_cmd = cmd_args(ctx.attrs.headers, hidden = ctx.attrs.resources)
    ///     if cmd.inputs == other_cmd.inputs:
    ///         pass
    /// ```
    #[starlark(attribute)]
    fn inputs<'v>(this: Value<'v>) -> starlark::Result<StarlarkCommandLineInputs> {
        let mut visitor = SimpleCommandLineArtifactVisitor::new();
        cmd_args(this).visit_artifacts(&mut visitor)?;
        Ok(StarlarkCommandLineInputs {
            inputs: visitor.inputs,
        })
    }

    /// Collect all the outputs (including hidden) referenced by this command line.
    #[starlark(attribute)]
    fn outputs<'v>(
        this: Value<'v>,
        heap: Heap<'_>,
    ) -> starlark::Result<Vec<StarlarkOutputArtifact<'v>>> {
        let mut visitor = SimpleCommandLineArtifactVisitor::new();
        cmd_args(this).visit_artifacts(&mut visitor)?;
        let mut outputs =
            Vec::with_capacity(visitor.declared_outputs.len() + visitor.frozen_outputs.len());
        for out in visitor.declared_outputs {
            let declared = heap.alloc_typed(StarlarkDeclaredArtifact::new(
                None,
                (*out).dupe(),
                AssociatedArtifacts::new(),
            ));
            outputs.push(StarlarkOutputArtifact::new(declared));
        }
        // FIXME(JakobDegen): We should probably not be allowing people to get an `OutputArtifact`
        // for an artifact declared in a downstream action??
        for out in visitor.frozen_outputs {
            let declared = heap.alloc_typed(StarlarkDeclaredArtifact::new(
                None,
                (*out
                    .allocate_new_output_artifact_for(heap)
                    .ok_or_else(|| internal_error!("Expecting artifact to be output artifact"))?)
                .dupe(),
                AssociatedArtifacts::new(),
            ));
            outputs.push(StarlarkOutputArtifact::new(declared));
        }
        Ok(outputs)
    }
}

#[starlark_module]
pub fn register_cmd_args(builder: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenStarlarkCmdArgs)]
    /// The `cmd_args` type is created by this function and is consumed by `ctx.actions.run`.
    /// The type is a mutable collection of strings and artifact values.
    /// In general, command lines, artifacts, strings, `RunInfo` and lists thereof can be added to or used to construct a `cmd_args` value.
    ///
    /// The arguments are:
    ///
    /// * `*args` - a list of things to add to the command line, each of which must be coercible to a command line. Further items can be added with `cmd.add`.
    /// * `format` - a string that provides a format to apply to the argument. for example, `cmd_args(x, format="--args={}")` would prepend `--args=` before `x`, or if `x` was a list, before each element in `x`.
    /// * `delimiter` - added between arguments to join them together. For example, `cmd_args(["--args=",x], delimiter="")` would produce a single argument to the underlying tool.
    /// * `prepend` - added as a separate argument before each argument.
    /// * `quote` - indicates whether quoting is to be applied to each argument. The only current valid value is `"shell"`.
    /// * `ignore_artifacts` - if `True`, artifacts paths are used, but artifacts are not pulled.
    /// * `hidden` - artifacts not present on the command line, but added as dependencies.
    /// * `absolute_prefix` and `absolute_suffix` - added to the start and end of each artifact.
    /// * `parent` - for all the artifacts use their `parent`th directory (e.g. `parent = 1` for the directory the artifact is located, `parent = 2` for that directory's parent, etc.).
    /// * `relative_to` - make all artifact paths relative to a given location.
    /// * `replace_regex` - replaces arguments with a regular expression.
    ///
    /// ### `ignore_artifacts`
    ///
    /// `ignore_artifacts=True` makes `cmd_args` to have no declared dependencies.
    /// Allows you to reference the path of an artifact _without_ introducing dependencies on it.
    ///
    /// As an example where this can be useful, consider passing a dependency that is only accessed at runtime, but whose path
    /// must be baked into the binary. As an example:
    ///
    /// ```python
    /// resources = cmd_args(resource_file, format = "-DFOO={}", ignore_artifacts=True)
    /// ctx.actions.run(cmd_args("gcc", "-c", source_file, resources))
    /// ```
    ///
    /// Note that `ignore_artifacts` sets all artifacts referenced by this `cmd_args` to be ignored, including those added afterwards,
    /// so generally create a special `cmd_args` and scope it quite tightly.
    ///
    /// If you actually do use the inputs referenced by this command,
    /// you will either error out due to missing dependencies (if running actions remotely)
    /// or have untracked dependencies that will fail to rebuild when it should.
    ///
    /// ### `hidden`
    ///
    /// Things to add to the command line which do not show up but are added as dependencies.
    /// The values can be anything normally permissible to pass to `add`.
    ///
    /// Typically used if the command you are running implicitly depends on files that are not
    /// passed on the command line, e.g. headers in the case of a C compilation.
    ///
    /// ### `absolute_prefix` and `absolute_suffix`
    ///
    /// Adds a prefix to the start or end of every artifact.
    ///
    /// Prefix is often used if you have a `$ROOT` variable
    /// in a shell script and want to use it to make files absolute.
    ///
    /// Suffix is often used in conjunction with `absolute_prefix`
    /// to wrap artifacts in function calls.
    ///
    /// ```python
    /// cmd_args(script, absolute_prefix = "$ROOT/")
    /// cmd_args(script, absolute_prefix = "call", absolute_suffix = ")")
    /// ```
    ///
    /// ### `parent`
    ///
    /// For all the artifacts use their parent directory.
    ///
    /// Typically used when the file name is passed one way, and the directory another,
    /// e.g. `cmd_args(artifact, format="-L{}", parent=1)`.
    ///
    /// ### `relative_to=dir` or `relative_to=(dir, parent)`
    ///
    /// Make all artifact paths relative to a given location. Typically used when the command
    /// you are running changes directory.
    ///
    /// By default, the paths are relative to the artifacts themselves (equivalent to
    /// parent equals to `0`). Use `parent` to make the paths relative to an ancestor directory.
    /// For example parent equals to `1` would make all paths relative to the containing dirs
    /// of any artifacts in the `cmd_args`.
    ///
    /// ```python
    /// dir = symlinked_dir(...)
    /// script = [
    ///     cmd_args(dir, format = "cd {}", relative_to=dir),
    /// ]
    /// ```
    ///
    /// ### `replace_regex`
    ///
    /// Replaces all parts matching pattern regular expression (or regular expressions)
    /// in each argument with replacement strings.
    fn cmd_args<'v>(
        #[starlark(args)] args: UnpackTuple<StarlarkCommandLineValueUnpack<'v>>,
        hidden: Option<StarlarkCommandLineValueUnpack<'v>>,
        delimiter: Option<StringValue<'v>>,
        format: Option<StringValue<'v>>,
        prepend: Option<StringValue<'v>>,
        quote: Option<&str>,
        #[starlark(default = false)] ignore_artifacts: bool,
        absolute_prefix: Option<StringValue<'v>>,
        absolute_suffix: Option<StringValue<'v>>,
        #[starlark(default = 0)] parent: u32,
        relative_to: Option<
            Either<ValueOf<'v, RelativeOrigin<'v>>, (ValueOf<'v, RelativeOrigin<'v>>, u32)>,
        >,
        #[starlark(default = Either::Right(UnpackList::default()))] replace_regex: Either<
            (CmdArgsRegex<'v>, StringValue<'v>),
            UnpackList<(CmdArgsRegex<'v>, StringValue<'v>)>,
        >,
    ) -> starlark::Result<StarlarkCmdArgs<'v>> {
        let quote = quote.try_map(QuoteStyle::parse)?;
        let mut builder = StarlarkCommandLineData::default();
        if delimiter.is_some()
            || format.is_some()
            || prepend.is_some()
            || quote.is_some()
            || ignore_artifacts
            || absolute_prefix.is_some()
            || absolute_suffix.is_some()
            || parent != 0
            || relative_to.is_some()
        {
            let opts = builder.options_mut();
            opts.delimiter = delimiter;
            opts.format = format;
            opts.prepend = prepend;
            opts.quote = quote;
            opts.ignore_artifacts = ignore_artifacts;
            opts.absolute_prefix = absolute_prefix;
            opts.absolute_suffix = absolute_suffix;
            opts.parent = parent;
            opts.relative_to = relative_to.map(|either| {
                let (relative_to, parent) = either.map_left(|o| (o, 0)).into_inner();
                (relative_to.as_unchecked(), parent)
            });
        }
        let replace_regex: Vec<(CmdArgsRegex, StringValue)> = replace_regex
            .map_left(|x| vec![x])
            .map_right(|x| x.items)
            .into_inner();
        if !replace_regex.is_empty() {
            for (pattern, _replacement) in &replace_regex {
                pattern.validate()?;
            }
            builder.options_mut().replacements = Some(Box::new(replace_regex));
        }
        for v in args.items {
            builder.add_value_typed(v)?;
        }
        if let Some(hidden) = hidden {
            builder.add_hidden(hidden)?;
        }
        Ok(StarlarkCmdArgs(RefCell::new(builder)))
    }
}

/// A wrapper for a [StarlarkCmdArgs]'s inputs. This is an opaque type that only allows
/// debug-printing and querying the length to tell if any inputs exist.
#[derive(Debug, PartialEq, ProvidesStaticType, NoSerialize, Allocative)]
pub struct StarlarkCommandLineInputs {
    pub inputs: BuckIndexSet<ArtifactGroup>,
}

starlark_simple_value!(StarlarkCommandLineInputs);

impl Display for StarlarkCommandLineInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt_container(f, "command_line_inputs(", ")", self.inputs.iter())
    }
}

#[starlark_value(type = "CommandLineInputs")]
impl<'v> StarlarkValue<'v> for StarlarkCommandLineInputs {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(command_line_inputs_methods)
    }

    fn length(&self) -> starlark::Result<i32> {
        self.inputs
            .len()
            .try_into()
            .map_err(starlark::Error::new_other)
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        if let Some(other) = other.downcast_ref::<Self>() {
            Ok(self.inputs == other.inputs)
        } else {
            Ok(false)
        }
    }
}

/// An opaque collection of input artifacts referenced by a `cmd_args` object.
///
/// Returned by the [`.inputs`](../cmd_args/#cmd_argsinputs) attribute. Supports `len()` and equality comparisons.
/// See the [`.inputs`](../cmd_args/#cmd_argsinputs) attribute documentation for usage examples.
#[starlark_module]
fn command_line_inputs_methods(_builder: &mut MethodsBuilder) {
    // No methods currently - this type only supports len() and equality via StarlarkValue trait
}

#[starlark_module]
#[starlark_types(
    StarlarkCommandLineInputs as CommandLineInputs
)]
pub(crate) fn register_command_line_inputs(globals: &mut GlobalsBuilder) {}
