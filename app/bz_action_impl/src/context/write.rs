/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashSet;
use std::fmt;

use allocative::Allocative;
use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::artifact_type::OutputArtifact;
use bz_build_api::actions::impls::json::JsonUnpack;
use bz_build_api::actions::impls::workspace_status::WorkspaceStatusKind;
use bz_build_api::artifact_groups::ArtifactGroup;
use bz_build_api::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use bz_build_api::interpreter::rule_defs::artifact::output_artifact_like::OutputArtifactArg;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use bz_build_api::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use bz_build_api::interpreter::rule_defs::artifact_tagging::ArtifactTag;
use bz_build_api::interpreter::rule_defs::bazel::depset::bazel_depset_to_list;
use bz_build_api::interpreter::rule_defs::cmd_args::ArtifactPathMapper;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineArtifactVisitor;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineContext;
use bz_build_api::interpreter::rule_defs::cmd_args::StarlarkCmdArgs;
use bz_build_api::interpreter::rule_defs::cmd_args::StarlarkCommandLineValueUnpack;
use bz_build_api::interpreter::rule_defs::cmd_args::WriteToFileMacroVisitor;
use bz_build_api::interpreter::rule_defs::cmd_args::value::CommandLineArg;
use bz_build_api::interpreter::rule_defs::context::AnalysisActions;
use bz_build_api::interpreter::rule_defs::resolved_macro::ResolvedMacro;
use bz_core::fs::buck_out_path::BazelOutputPathKind;
use bz_core::fs::buck_out_path::BuckOutPathKind;
use bz_execute::execute::request::OutputType;
use bz_hash::BuckHashMap;
use bz_hash::buck_indexset;
use dupe::Dupe;
use either::Either;
use parking_lot::Mutex;
use relative_path::RelativePathBuf;
use sha1::Digest;
use sha1::Sha1;
use starlark::any::ProvidesStaticType;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::AllocValue;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueOf;
use starlark::values::ValueTyped;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::ListRef;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::tuple::TupleRef;
use starlark::values::type_repr::StarlarkTypeRepr;
use starlark::values::typing::StarlarkCallable;
use starlark_map::small_set::SmallSet;

use crate::actions::impls::write::UnregisteredTemplateExpansionAction;
use crate::actions::impls::write::UnregisteredWriteAction;
use crate::actions::impls::write_json::UnregisteredWriteJsonAction;
use crate::actions::impls::write_macros::UnregisteredWriteMacrosToFileAction;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum WriteActionError {
    #[error(
        "Argument type attributes detected in a content to be written into a file, but support for arguments was not turned on. Use `allow_args` parameter to turn on the support for arguments."
    )]
    ArgAttrsDetectedButNotAllowed,
}

#[derive(UnpackValue, StarlarkTypeRepr)]
enum WriteContentArg<'v> {
    CommandLineArg(CommandLineArg<'v>),
    StarlarkCommandLineValueUnpack(StarlarkCommandLineValueUnpack<'v>),
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct StarlarkTemplateDict {
    substitutions: Mutex<Vec<(String, String)>>,
}

impl StarlarkTemplateDict {
    fn new() -> Self {
        Self {
            substitutions: Mutex::new(Vec::new()),
        }
    }

    fn push(&self, key: &str, value: String) {
        self.substitutions.lock().push((key.to_owned(), value));
    }

    fn substitutions(&self) -> Vec<(String, String)> {
        self.substitutions.lock().clone()
    }
}

impl fmt::Display for StarlarkTemplateDict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TemplateDict")
    }
}

starlark_simple_value!(StarlarkTemplateDict);

#[starlark_value(type = "TemplateDict")]
impl<'v> StarlarkValue<'v> for StarlarkTemplateDict {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(template_dict_methods)
    }
}

fn template_dict_format_joined(format: &str, value: &str) -> bz_error::Result<String> {
    let mut converted = String::with_capacity(format.len() + value.len());
    let mut idx = 0;
    let mut found = false;
    while let Some(next) = format[idx..].find('%') {
        let next = idx + next;
        converted.push_str(&format[idx..next]);
        let Some(escaped) = format[next + 1..].chars().next() else {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "Invalid value for parameter `format_joined`: expected string with a single `%s`, got `{}`",
                format
            ));
        };
        match escaped {
            's' if !found => {
                converted.push_str(value);
                found = true;
            }
            's' => {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "Invalid value for parameter `format_joined`: expected string with a single `%s`, got `{}`",
                    format
                ));
            }
            '%' => converted.push('%'),
            _ => {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "Invalid value for parameter `format_joined`: expected string with a single `%s`, got `{}`",
                    format
                ));
            }
        }
        idx = next + 1 + escaped.len_utf8();
    }
    converted.push_str(&format[idx..]);
    if !found {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "Invalid value for parameter `format_joined`: expected string with a single `%s`, got `{}`",
            format
        ));
    }
    Ok(converted)
}

fn template_dict_push_mapped_string(
    value: Value<'_>,
    key: &str,
    original: Value<'_>,
    parts: &mut Vec<String>,
) -> starlark::Result<()> {
    let Some(value_str) = value.unpack_str() else {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "Function provided to map_each must return string, None, or list of strings, but returned list containing element `{}` of type {} for key `{}` and value: {}",
            value.to_repr(),
            value.get_type(),
            key,
            original.to_repr()
        )
        .into());
    };
    parts.push(value_str.to_owned());
    Ok(())
}

fn template_dict_extend_mapped_value(
    mapped: Value<'_>,
    key: &str,
    original: Value<'_>,
    parts: &mut Vec<String>,
) -> starlark::Result<()> {
    if mapped.is_none() {
        return Ok(());
    }
    if let Some(value) = mapped.unpack_str() {
        parts.push(value.to_owned());
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(mapped) {
        for value in list.iter() {
            template_dict_push_mapped_string(value, key, original, parts)?;
        }
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(mapped) {
        for value in tuple.content() {
            template_dict_push_mapped_string(*value, key, original, parts)?;
        }
        return Ok(());
    }
    Err(bz_error::bz_error!(
        bz_error::ErrorTag::Input,
        "Function provided to map_each must return string, None, or list of strings, but returned type {} for key `{}` and value: {}",
        mapped.get_type(),
        key,
        original.to_repr()
    )
    .into())
}

#[starlark_module]
fn template_dict_methods(builder: &mut MethodsBuilder) {
    fn add<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] key: &str,
        #[starlark(require = pos)] value: &str,
    ) -> starlark::Result<Value<'v>> {
        let template_dict =
            StarlarkTemplateDict::from_value(this).expect("validated method receiver");
        template_dict.push(key, value.to_owned());
        Ok(this)
    }

    fn add_joined<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] key: &str,
        #[starlark(require = pos)] values: Value<'v>,
        #[starlark(require = named)] join_with: StringValue<'v>,
        #[starlark(require = named)] map_each: StarlarkCallable<'v>,
        #[starlark(require = named, default = false)] uniquify: bool,
        #[starlark(require = named, default = NoneOr::None)] format_joined: NoneOr<StringValue<'v>>,
        #[starlark(require = named, default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let _unused = allow_closure;
        let template_dict =
            StarlarkTemplateDict::from_value(this).expect("validated method receiver");
        let mut parts = Vec::new();
        for value in bazel_depset_to_list(values)? {
            let mapped = eval.eval_function(map_each.0, &[value], &[])?;
            template_dict_extend_mapped_value(mapped, key, value, &mut parts)?;
        }
        if uniquify {
            let mut seen = HashSet::new();
            parts.retain(|part| seen.insert(part.clone()));
        }
        let joined = parts.join(join_with.as_str());
        let joined = match format_joined.into_option() {
            Some(format_joined) => template_dict_format_joined(format_joined.as_str(), &joined)?,
            None => joined,
        };
        template_dict.push(key, joined);
        Ok(this)
    }
}

/// We don't need to run this visitor in order to provide the inputs to the write actions,
/// because that is done lazily when we run the action.
/// However, we do need to always run this visitor, because it verifies that any content-based
/// inputs are bound. It will also collect "associated artifacts", if requested.
struct CommandLineInputVisitor {
    associated_artifacts: SmallSet<ArtifactGroup>,
    with_associated_artifacts: bool,
}

impl CommandLineInputVisitor {
    fn new(with_associated_artifacts: bool) -> Self {
        Self {
            associated_artifacts: Default::default(),
            with_associated_artifacts,
        }
    }
}

impl<'v> CommandLineArtifactVisitor<'v> for CommandLineInputVisitor {
    fn visit_input(&mut self, input: ArtifactGroup, _tags: Vec<&ArtifactTag>) {
        if self.with_associated_artifacts {
            self.associated_artifacts.insert(input.dupe());
        }
    }

    fn visit_declared_output(&mut self, _artifact: OutputArtifact<'v>, _tags: Vec<&ArtifactTag>) {}

    fn visit_frozen_output(&mut self, _artifact: Artifact, _tags: Vec<&ArtifactTag>) {}

    fn visit_declared_artifact(
        &mut self,
        declared_artifact: bz_artifact::artifact::artifact_type::DeclaredArtifact<'v>,
        tags: Vec<&ArtifactTag>,
    ) -> bz_error::Result<()> {
        if self.with_associated_artifacts || declared_artifact.has_content_based_path() {
            let artifact = declared_artifact.ensure_bound()?.into_artifact();
            self.visit_input(ArtifactGroup::Artifact(artifact), tags);
        }

        Ok(())
    }
}

fn bazel_build_info_substitutions<'v>(
    transform_func: StarlarkCallable<'v>,
    kind: WorkspaceStatusKind,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Vec<(String, String)>> {
    let entries = kind
        .entries()
        .iter()
        .map(|(key, value)| {
            (
                eval.heap().alloc_str(key).to_value(),
                eval.heap().alloc_str(value).to_value(),
            )
        })
        .collect::<Vec<_>>();
    let values = eval.heap().alloc(AllocDict(entries));
    let response = eval.eval_function(transform_func.0, &[values], &[])?;
    let dict = DictRef::from_value(response).ok_or_else(|| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "build info transform callback must return a dict, got `{}`",
            response.get_type()
        )
    })?;

    let mut substitutions = Vec::with_capacity(dict.len());
    for (key, value) in dict.iter() {
        let Some(key) = key.unpack_str() else {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "build info transform callback keys must be strings, got `{}`",
                key.get_type()
            )
            .into());
        };
        let Some(value) = value.unpack_str() else {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "build info transform callback values must be strings, got `{}`",
                value.get_type()
            )
            .into());
        };
        substitutions.push((key.to_owned(), value.to_owned()));
    }
    Ok(substitutions)
}

fn transform_build_info_file<'v>(
    this: &AnalysisActions<'v>,
    transform_func: StarlarkCallable<'v>,
    template: ValueAsInputArtifactLike<'v>,
    output_file_name: &str,
    kind: WorkspaceStatusKind,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
    let substitutions = bazel_build_info_substitutions(transform_func, kind, eval)?;
    let template = template.0;
    let template_artifact = template.get_artifact_group()?;

    let mut state = this.state()?;
    let declared = state.declare_bazel_shareable_output(
        output_file_name,
        OutputType::File,
        eval.call_stack_top_location(),
        BuckOutPathKind::Configuration,
        this.bazel_owner(),
        this.bazel_output_root,
        BazelOutputPathKind::PackageRelative,
        eval.heap(),
    )?;
    let output_artifact = declared.as_output();
    let action_signature = format!("{kind:?}:{template_artifact:?}:{substitutions:?}");
    if state.should_register_bazel_shareable_action(&output_artifact, action_signature)? {
        state.register_action(
            buck_indexset![output_artifact],
            UnregisteredTemplateExpansionAction::new(template_artifact, substitutions, false),
            None,
            None,
        )?;
    }

    Ok(eval.heap().alloc_typed(StarlarkDeclaredArtifact::new(
        eval.call_stack_top_location(),
        declared,
        AssociatedArtifacts::new(),
    )))
}

#[starlark_module]
pub(crate) fn analysis_actions_methods_write(methods: &mut MethodsBuilder) {
    /// Returns a Bazel-compatible template dictionary for computed substitutions.
    fn template_dict<'v>(
        this: &AnalysisActions<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let _unused = this;
        Ok(eval.heap().alloc(StarlarkTemplateDict::new()))
    }

    /// Creates a Bazel-compatible template expansion action.
    fn expand_template<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = named)] template: ValueAsInputArtifactLike<'v>,
        #[starlark(require = named)] output: OutputArtifactArg<'v>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        substitutions: UnpackDictEntries<&'v str, &'v str>,
        #[starlark(require = named, default = false)] is_executable: bool,
        #[starlark(require = named, default = NoneOr::None)] computed_substitutions: NoneOr<
            &'v StarlarkTemplateDict,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let mut substitutions = substitutions
            .entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<Vec<_>>();
        if let Some(computed_substitutions) = computed_substitutions.into_option() {
            substitutions.extend(computed_substitutions.substitutions());
        }
        let mut seen = HashSet::new();
        for (key, _) in &substitutions {
            if !seen.insert(key.clone()) {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "Multiple entries with same key: `{}`",
                    key
                )
                .into());
            }
        }

        let template = template.0;
        let action = UnregisteredTemplateExpansionAction::new(
            template.get_artifact_group()?,
            substitutions,
            is_executable,
        );

        let mut this = this.state()?;
        let (_declaration, output_artifact) =
            this.get_or_declare_output(eval, output, OutputType::File, None)?;
        this.register_action(buck_indexset![output_artifact], action, None, None)?;
        Ok(Value::new_none())
    }

    /// Bazel internal API for transforming the volatile workspace status file through a template.
    fn transform_version_file<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = named)] transform_func: StarlarkCallable<'v>,
        #[starlark(require = named)] template: ValueAsInputArtifactLike<'v>,
        #[starlark(require = named)] output_file_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        transform_build_info_file(
            this,
            transform_func,
            template,
            output_file_name,
            WorkspaceStatusKind::Volatile,
            eval,
        )
    }

    /// Bazel internal API for transforming the stable workspace status file through a template.
    fn transform_info_file<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = named)] transform_func: StarlarkCallable<'v>,
        #[starlark(require = named)] template: ValueAsInputArtifactLike<'v>,
        #[starlark(require = named)] output_file_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>> {
        transform_build_info_file(
            this,
            transform_func,
            template,
            output_file_name,
            WorkspaceStatusKind::Stable,
            eval,
        )
    }

    /// Returns an `artifact` whose contents are `content` written as a JSON value.
    ///
    /// * `output`: can be a string, or an existing artifact created with `declare_output`
    /// * `content`:  must be composed of the basic json types (boolean, number, string, list/tuple,
    ///   dictionary) plus artifacts and command lines
    ///     * An artifact will be written as a string containing the path
    ///     * A command line will be written as a list of strings, unless `joined=True` is set, in
    ///       which case it will be a string
    /// * If you pass `with_inputs = True`, you'll get back a `cmd_args` that expands to the JSON
    ///   file but carries all the underlying inputs as dependencies (so you don't have to use, for
    ///   example, `hidden` for them to be added to an action that already receives the JSON file)
    /// * `pretty` (optional): write formatted JSON (defaults to `False`)
    /// * `absolute` (optional): if set, this action will produce absolute paths in its output when
    ///   rendering artifact paths. You generally shouldn't use this if you plan to use this action
    ///   as the input for anything else, as this would effectively result in losing all shared
    ///   caching. (defaults to `False`)
    fn write_json<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = pos)] output: OutputArtifactArg<'v>,
        #[starlark(require = pos)] content: ValueOf<'v, JsonUnpack<'v>>,
        #[starlark(require = named, default = false)] with_inputs: bool,
        #[starlark(require = named, default = false)] pretty: bool,
        #[starlark(require = named, default = false)] absolute: bool,
        #[starlark(require = named, default = NoneOr::None)] has_content_based_path: NoneOr<bool>,
        #[starlark(require = named, default = false)]
        use_dep_files_placeholder_for_content_based_paths: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<impl AllocValue<'v> + use<'v>> {
        let mut this = this.state()?;
        let (declaration, output_artifact) = this.get_or_declare_output(
            eval,
            output,
            OutputType::File,
            has_content_based_path.into_option(),
        )?;

        let value = declaration.into_declared_artifact(AssociatedArtifacts::new());
        let cli = UnregisteredWriteJsonAction::cli(value.to_value(), content.value)?;

        let mut visitor = CommandLineInputVisitor::new(false);
        cli.visit_contents(&mut visitor)?;

        this.register_action(
            buck_indexset![output_artifact],
            UnregisteredWriteJsonAction::new(
                pretty,
                absolute,
                use_dep_files_placeholder_for_content_based_paths,
            ),
            Some(content.value),
            None,
        )?;

        // TODO(cjhopman): The with_inputs thing can go away once we have artifact dependencies (we'll still
        // need the UnregisteredWriteJsonAction::cli() to represent the dependency though).
        if with_inputs {
            // TODO(nga): we use `AllocValue`, so this function return type for this branch
            //   is `write_json_cli_args`. We want just `cmd_args`,
            //   because users don't care about precise type.
            //   Do it when we migrate to new types not based on strings.
            let cli = UnregisteredWriteJsonAction::cli(value.to_value(), content.value)?;
            Ok(Either::Right(cli))
        } else {
            Ok(Either::Left(value))
        }
    }

    /// Returns an `artifact` whose contents are `content`
    ///
    /// * `is_executable` (optional): indicates whether the resulting file should be marked with
    ///   executable permissions
    /// * `allow_args` (optional): must be set to `True` if you want to write parameter arguments to
    ///   the file (in particular, macros that write to file)
    ///     * If it is true, the result will be a pair of the `artifact` containing content and a
    ///       list of artifact values that were written by macros, which should be used in hidden
    ///       fields or similar
    /// * `with_inputs` (optional): if set, add artifacts in `content` as associated artifacts of the return `artifact`.
    /// * `absolute` (optional): if set, this action will produce absolute paths in its output when
    ///   rendering artifact paths. You generally shouldn't use this if you plan to use this action
    ///   as the input for anything else, as this would effectively result in losing all shared
    ///   caching.
    ///
    /// The content is often a string, but can be any `ArgLike` value. This is occasionally useful
    /// for generating scripts to run as a part of another action. `cmd_args` in the content are
    /// newline separated unless another delimiter is explicitly specified.
    fn write<'v>(
        this: &AnalysisActions<'v>,
        output: OutputArtifactArg<'v>,
        content: WriteContentArg<'v>,
        #[starlark(default = false)] is_executable: bool,
        #[starlark(require = named, default = NoneOr::None)] mnemonic: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] _execution_requirements: NoneOr<
            Value<'v>,
        >,
        #[starlark(require = named, default = false)] allow_args: bool,
        // If set, add artifacts in content as associated artifacts of the output. This will only work for bound artifacts.
        #[starlark(require = named, default = false)] with_inputs: bool,
        #[starlark(require = named, default = false)] absolute: bool,
        #[starlark(require = named, default = NoneOr::None)] has_content_based_path: NoneOr<bool>,
        #[starlark(require = named, default = false)]
        use_dep_files_placeholder_for_content_based_paths: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<
        Either<
            ValueTyped<'v, StarlarkDeclaredArtifact<'v>>,
            (
                ValueTyped<'v, StarlarkDeclaredArtifact<'v>>,
                Vec<StarlarkDeclaredArtifact<'v>>,
            ),
        >,
    > {
        let _unused = mnemonic;
        fn count_write_to_file_macros(
            args_allowed: bool,
            cli: &dyn CommandLineArgLike,
        ) -> bz_error::Result<u32> {
            if !args_allowed && cli.contains_arg_attr() {
                return Err(WriteActionError::ArgAttrsDetectedButNotAllowed.into());
            }

            struct WriteToFileMacrosCounter {
                count: u32,
            }

            impl WriteToFileMacroVisitor for WriteToFileMacrosCounter {
                fn visit_write_to_file_macro(
                    &mut self,
                    _m: &ResolvedMacro,
                    _artifact_path_mapping: &dyn ArtifactPathMapper,
                ) -> bz_error::Result<()> {
                    self.count += 1;
                    Ok(())
                }

                fn set_current_relative_to_path(
                    &mut self,
                    _gen: &dyn Fn(
                        &dyn CommandLineContext,
                    )
                        -> bz_error::Result<Option<RelativePathBuf>>,
                ) -> bz_error::Result<()> {
                    Ok(())
                }
            }

            let mut counter = WriteToFileMacrosCounter { count: 0 };
            // At this point the mapping doesn't matter because we're only doing a count
            cli.visit_write_to_file_macros(&mut counter, &BuckHashMap::default())?;
            Ok(counter.count)
        }

        fn get_cli_inputs(
            with_inputs: bool,
            cli: &dyn CommandLineArgLike,
        ) -> bz_error::Result<SmallSet<ArtifactGroup>> {
            let mut visitor = CommandLineInputVisitor::new(with_inputs);
            cli.visit_artifacts(&mut visitor)?;
            Ok(visitor.associated_artifacts)
        }

        let mut this = this.state()?;
        let (declaration, output_artifact) = this.get_or_declare_output(
            eval,
            output,
            OutputType::File,
            has_content_based_path.into_option(),
        )?;

        let (content_cli, written_macro_count, mut associated_artifacts) = match content {
            WriteContentArg::CommandLineArg(content) => {
                let content_arg = content.as_command_line_arg();
                let count = count_write_to_file_macros(allow_args, content_arg)?;
                let associated_artifacts = get_cli_inputs(with_inputs, content_arg)?;
                (content, count, associated_artifacts)
            }
            WriteContentArg::StarlarkCommandLineValueUnpack(content) => {
                let cli = StarlarkCmdArgs::try_from_value_typed(content)?;
                let count = count_write_to_file_macros(allow_args, &cli)?;
                let associated_artifacts = get_cli_inputs(with_inputs, &cli)?;
                (
                    CommandLineArg::from_cmd_args(eval.heap().alloc_typed(cli)),
                    count,
                    associated_artifacts,
                )
            }
        };

        let path_resolution_method = output_artifact.path_resolution_method();

        let written_macro_files = if written_macro_count > 0 {
            let macro_directory_path = {
                // There might be several write actions at once, use write action output hash to deterministically avoid collisions for .macro files.
                let digest = output_artifact
                    .get_path()
                    .with_full_path(|path| Sha1::digest(path.as_str().as_bytes()));
                let sha = hex::encode(digest);
                format!("__macros/{sha}")
            };

            let mut written_macro_files = buck_indexset![];
            for i in 0..written_macro_count {
                let macro_file = this.declare_output(
                    None,
                    &format!("{}/{}.macro", &macro_directory_path, i),
                    OutputType::File,
                    eval.call_stack_top_location(),
                    path_resolution_method,
                    eval.heap(),
                )?;
                written_macro_files.insert(macro_file);
            }

            let state = &mut *this;
            let action = UnregisteredWriteMacrosToFileAction::new(
                output_artifact
                    .get_path()
                    .with_short_path(|p| p.to_string()),
                use_dep_files_placeholder_for_content_based_paths,
            );
            state.register_action(
                written_macro_files.iter().map(|a| a.as_output()).collect(),
                action,
                Some(content_cli.to_value()),
                None,
            )?;

            written_macro_files
        } else {
            buck_indexset![]
        };

        let action = {
            let maybe_macro_files = if allow_args {
                let mut macro_files = buck_indexset![];
                for a in &written_macro_files {
                    let artifact = a.dupe().ensure_bound()?.into_artifact();
                    macro_files.insert(artifact.dupe());
                }
                Some(macro_files)
            } else {
                None
            };
            UnregisteredWriteAction {
                is_executable,
                macro_files: maybe_macro_files,
                absolute,
                use_dep_files_placeholder_for_content_based_paths,
            }
        };
        this.register_action(
            buck_indexset![output_artifact],
            action,
            Some(content_cli.to_value()),
            None,
        )?;

        if allow_args {
            for a in &written_macro_files {
                associated_artifacts.insert(ArtifactGroup::Artifact(
                    a.dupe().ensure_bound()?.into_artifact(),
                ));
            }
        }

        let value =
            declaration.into_declared_artifact(AssociatedArtifacts::from(associated_artifacts));
        if allow_args {
            let macro_files: Vec<StarlarkDeclaredArtifact> = written_macro_files
                .into_iter()
                .map(|a| StarlarkDeclaredArtifact::new(None, a, AssociatedArtifacts::new()))
                .collect();
            Ok(Either::Right((value, macro_files)))
        } else {
            // Prefer simpler API when there is no possibility for write-to-file macros to be present in a content
            Ok(Either::Left(value))
        }
    }
}
