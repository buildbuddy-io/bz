/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;
use std::time::Instant;

use buck2_artifact::artifact::source_artifact::SourceArtifact;
use buck2_build_api::analysis::AnalysisResult;
use buck2_build_api::analysis::anon_promises_dyn::RunAnonPromisesAccessorPair;
use buck2_build_api::analysis::calculation::RuleAnalysisCalculation;
use buck2_build_api::analysis::registry::AnalysisRegistry;
use buck2_build_api::analysis::registry::RecordedAnalysisValues;
use buck2_build_api::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_artifact::StarlarkArtifact;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use buck2_build_api::interpreter::rule_defs::cmd_args::value::FrozenCommandLineArg;
use buck2_build_api::interpreter::rule_defs::context::AnalysisContext;
use buck2_build_api::interpreter::rule_defs::context::BazelActionsContextOverride;
use buck2_build_api::interpreter::rule_defs::context::BazelCppOptions;
use buck2_build_api::interpreter::rule_defs::context::analysis_actions_to_bazel_ctx_with_overrides;
use buck2_build_api::interpreter::rule_defs::provider::builtin::bazel_output_file_info::FrozenBazelOutputFileInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::bazel_output_file_info::new_bazel_output_file_info;
use buck2_build_api::interpreter::rule_defs::provider::builtin::default_info::DefaultInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::default_info::FrozenDefaultInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::template_placeholder_info::FrozenTemplatePlaceholderInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::template_variable_info::FrozenTemplateVariableInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::toolchain_info::FrozenToolchainInfo;
use buck2_build_api::interpreter::rule_defs::provider::builtin::validation_info::FrozenValidationInfo;
use buck2_build_api::interpreter::rule_defs::provider::collection::FrozenProviderCollection;
use buck2_build_api::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValue;
use buck2_build_api::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValueRef;
use buck2_build_api::interpreter::rule_defs::provider::collection::ProviderCollection;
use buck2_build_api::interpreter::rule_defs::provider::dependency::Dependency;
use buck2_build_api::keep_going::KeepGoing;
use buck2_build_api::validation::transitive_validations::TransitiveValidations;
use buck2_build_api::validation::transitive_validations::TransitiveValidationsData;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::legacy_configs::dice::HasLegacyConfigs;
use buck2_common::legacy_configs::key::BuckconfigKeyRef;
use buck2_common::legacy_configs::view::LegacyBuckConfigView;
use buck2_core::deferred::base_deferred_key::BaseDeferredKey;
use buck2_core::deferred::key::DeferredHolderKey;
use buck2_core::execution_types::execution::ExecutionPlatformResolution;
use buck2_core::fs::buck_out_path::BazelOutputRoot;
use buck2_core::fs::buck_out_path::BuckOutPathKind;
use buck2_core::package::PackageLabel;
use buck2_core::package::package_relative_path::PackageRelativePath;
use buck2_core::package::source_path::SourcePath;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
use buck2_core::unsafe_send_future::UnsafeSendFuture;
use buck2_error::BuckErrorContext;
use buck2_error::conversion::from_any_with_tag;
use buck2_error::internal_error;
use buck2_events::dispatch::get_dispatcher;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_execute::execute::request::OutputType;
use buck2_hash::StdBuckHashMap;
use buck2_hash::StdBuckHashSet;
use buck2_interpreter::dice::starlark_provider::StarlarkEvalKind;
use buck2_interpreter::factory::BuckStarlarkModule;
use buck2_interpreter::factory::StarlarkEvaluatorProvider;
use buck2_interpreter::print_handler::EventDispatcherPrintHandler;
use buck2_interpreter::soft_error::Buck2StarlarkSoftErrorHandler;
use buck2_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use buck2_interpreter::types::label_context::StarlarkLabelResolutionContext;
use buck2_interpreter::types::rule::FROZEN_BAZEL_ASPECT_INFO_GET_IMPL;
use buck2_interpreter::types::rule::FROZEN_BAZEL_ATTR_ASPECTS_GET_IMPL;
use buck2_interpreter::types::rule::FROZEN_PROMISE_ARTIFACT_MAPPINGS_GET_IMPL;
use buck2_interpreter::types::rule::FROZEN_RULE_GET_IMPL;
use buck2_interpreter::types::rule::FrozenBazelAspectInfo;
use buck2_interpreter::types::rule::bazel_aspect_hidden_attr_name;
use buck2_interpreter::types::rule::is_bazel_aspect_hidden_attr;
use buck2_node::attrs::attr_type::dep::DepAttrTransition;
use buck2_node::attrs::attr_type::dep::DepAttrType;
use buck2_node::attrs::attr_type::split_transition_dep::ConfiguredSplitTransitionDep;
use buck2_node::attrs::configured_attr::ConfiguredAttr;
use buck2_node::attrs::display::AttrDisplayWithContextExt;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::nodes::configured::ConfiguredTargetNodeRef;
use buck2_node::provider_id_set::ProviderIdSet;
use buck2_node::rule::BAZEL_OUTPUT_FILE_OUTPUT_ATTR;
use buck2_node::rule_type::StarlarkRuleType;
use dice::CancellationContext;
use dice::DiceComputations;
use dupe::Dupe;
use futures::Future;
use futures::FutureExt;
use starlark::environment::FrozenModule;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::FrozenHeap;
use starlark::values::FrozenValue;
use starlark::values::FrozenValueTyped;
use starlark::values::Value;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueTyped;
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::structs::AllocStruct;
use starlark::values::structs::StructRef;
use starlark::values::tuple::AllocTuple;
use starlark::values::tuple::TupleRef;
use starlark_map::small_map::SmallMap;

use crate::analysis::calculation::AnalysisSplitInstants;
use crate::analysis::plugins::plugins_to_starlark_value;
use crate::attrs::resolve::attr_type::dep::DepAttrTypeExt;
use crate::attrs::resolve::configured_attr::ConfiguredAttrExt;
use crate::attrs::resolve::ctx::AnalysisQueryResult;
use crate::attrs::resolve::ctx::AttrResolutionContext;
use crate::attrs::resolve::node_to_attrs_struct::node_to_attrs_struct;

#[derive(buck2_error::Error, Debug)]
#[buck2(tag = Tier0)]
enum AnalysisError {
    #[error(
        "Analysis context was missing a query result, this shouldn't be possible. Query was `{0}`"
    )]
    MissingQuery(String),
    #[error("required dependency `{0}` was not found")]
    MissingDep(ConfiguredProvidersLabel),
}

// Contains a `module` that things must live on, and various `FrozenProviderCollectionValue`s
// that are NOT tied to that module. Must claim ownership of them via `add_reference` before returning them.
pub struct RuleAnalysisAttrResolutionContext<'a, 'v> {
    pub module: &'a Module<'v>,
    pub dep_analysis_results: StdBuckHashMap<ConfiguredTargetLabel, FrozenProviderCollectionValue>,
    pub query_results: StdBuckHashMap<String, Arc<AnalysisQueryResult>>,
    pub execution_platform_resolution: ExecutionPlatformResolution,
}

impl<'a, 'v> AttrResolutionContext<'v> for &'_ RuleAnalysisAttrResolutionContext<'a, 'v> {
    fn starlark_module(&self) -> &Module<'v> {
        self.module
    }

    fn get_dep(
        &mut self,
        target: &ConfiguredProvidersLabel,
    ) -> buck2_error::Result<FrozenValueTyped<'v, FrozenProviderCollection>> {
        get_dep(&self.dep_analysis_results, target, self.module)
    }

    fn resolve_unkeyed_placeholder(
        &mut self,
        name: &str,
    ) -> buck2_error::Result<Option<FrozenCommandLineArg>> {
        Ok(resolve_unkeyed_placeholder(
            &self.dep_analysis_results,
            name,
            self.module,
        ))
    }

    fn resolve_query(&mut self, query: &str) -> buck2_error::Result<Arc<AnalysisQueryResult>> {
        resolve_query(&self.query_results, query, self.module)
    }

    fn execution_platform_resolution(&self) -> &ExecutionPlatformResolution {
        &self.execution_platform_resolution
    }
}

pub fn get_dep<'v>(
    dep_analysis_results: &StdBuckHashMap<ConfiguredTargetLabel, FrozenProviderCollectionValue>,
    target: &ConfiguredProvidersLabel,
    module: &Module<'v>,
) -> buck2_error::Result<FrozenValueTyped<'v, FrozenProviderCollection>> {
    match dep_analysis_results.get(target.target()) {
        None => Err(AnalysisError::MissingDep(target.dupe()).into()),
        Some(x) => {
            let x = x.lookup_inner(target)?;
            // IMPORTANT: Anything given back to the user must be kept alive
            Ok(x.add_heap_ref(module.heap()))
        }
    }
}

pub fn resolve_unkeyed_placeholder(
    dep_analysis_results: &StdBuckHashMap<ConfiguredTargetLabel, FrozenProviderCollectionValue>,
    name: &str,
    module: &Module,
) -> Option<FrozenCommandLineArg> {
    // TODO(cjhopman): Make it an error if two deps provide a value for the placeholder.
    for providers in dep_analysis_results.values() {
        if let Some(placeholder_info) = providers
            .provider_collection()
            .builtin_provider::<FrozenTemplatePlaceholderInfo>()
        {
            if let Some(value) = placeholder_info.unkeyed_variables().get(name) {
                // IMPORTANT: Anything given back to the user must be kept alive
                module
                    .frozen_heap()
                    .add_reference(providers.value().owner());
                return Some(*value);
            }
        }
    }
    None
}

pub fn resolve_query(
    query_results: &StdBuckHashMap<String, Arc<AnalysisQueryResult>>,
    query: &str,
    module: &Module,
) -> buck2_error::Result<Arc<AnalysisQueryResult>> {
    match query_results.get(query) {
        None => Err(AnalysisError::MissingQuery(query.to_owned()).into()),
        Some(x) => {
            for (_, y) in x.result.iter() {
                // IMPORTANT: Anything given back to the user must be kept alive
                module.frozen_heap().add_reference(y.value().owner());
            }
            Ok(x.dupe())
        }
    }
}

pub trait RuleSpec: Sync {
    fn invoke<'v>(
        &self,
        eval: &mut Evaluator<'v, '_, '_>,
        ctx: ValueTyped<'v, AnalysisContext<'v>>,
    ) -> buck2_error::Result<Value<'v>>;

    fn promise_artifact_mappings<'v>(
        &self,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<SmallMap<String, Value<'v>>>;

    fn bazel_attr_aspects<'v>(
        &self,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<SmallMap<String, Vec<Value<'v>>>>;
}

/// Container for the environment that analysis implementation functions should run in
struct AnalysisEnv<'a> {
    rule_spec: &'a dyn RuleSpec,
    deps: Vec<(&'a ConfiguredTargetLabel, AnalysisResult)>,
    query_results: StdBuckHashMap<String, Arc<AnalysisQueryResult>>,
    execution_platform: &'a ExecutionPlatformResolution,
    label: ConfiguredTargetLabel,
    action_owner_rule_type_name: Arc<str>,
    cancellation: &'a CancellationContext,
}

pub(crate) async fn run_analysis<'a>(
    dice: &'a mut DiceComputations<'_>,
    label: &'a ConfiguredTargetLabel,
    results: Vec<(&'a ConfiguredTargetLabel, AnalysisResult)>,
    query_results: StdBuckHashMap<String, Arc<AnalysisQueryResult>>,
    execution_platform: &'a ExecutionPlatformResolution,
    rule_spec: &'a dyn RuleSpec,
    node: ConfiguredTargetNodeRef<'a>,
    action_owner_rule_type_name: Arc<str>,
    cancellation: &'a CancellationContext,
) -> buck2_error::Result<(AnalysisResult, Option<AnalysisSplitInstants>)> {
    let analysis_env = AnalysisEnv {
        rule_spec,
        deps: results,
        query_results,
        execution_platform,
        label: label.dupe(),
        action_owner_rule_type_name,
        cancellation,
    };
    run_analysis_with_env(dice, analysis_env, node).await
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelOutputFileAnalysisError {
    #[error("Bazel output-file target `{0}` has no generating rule dependency")]
    MissingGeneratingRule(ConfiguredTargetLabel),
    #[error("Bazel output-file target `{0}` has more than one generating rule dependency")]
    MultipleGeneratingRules(ConfiguredTargetLabel),
    #[error("Bazel output-file target `{0}` is missing output attr `{1}`")]
    MissingOutputAttr(ConfiguredTargetLabel, &'static str),
    #[error("Bazel output-file target `{0}` has unsupported output attr value `{1}`")]
    UnsupportedOutputAttrValue(ConfiguredTargetLabel, String),
    #[error("Bazel generating rule `{0}` did not provide output-file metadata")]
    MissingOutputFileInfo(ConfiguredTargetLabel),
    #[error("Bazel generating rule `{0}` did not declare output `{1}`")]
    MissingOutputFile(ConfiguredTargetLabel, String),
}

pub(crate) fn run_bazel_output_file_analysis<'a, 'd: 'a>(
    dice: &'a mut DiceComputations<'d>,
    label: &'a ConfiguredTargetLabel,
    results: Vec<(&'a ConfiguredTargetLabel, AnalysisResult)>,
    execution_platform: &'a ExecutionPlatformResolution,
    node: ConfiguredTargetNodeRef<'a>,
    cancellation: &'a CancellationContext,
) -> impl Future<Output = buck2_error::Result<AnalysisResult>> + 'a + Captures<'d> {
    let fut = async move {
        run_bazel_output_file_analysis_underlying(
            dice,
            label,
            results,
            execution_platform,
            node,
            cancellation,
        )
        .await
    };
    unsafe { UnsafeSendFuture::new_encapsulates_starlark(fut) }
}

pub(crate) fn run_bazel_input_file_analysis<'a, 'd: 'a>(
    dice: &'a mut DiceComputations<'d>,
    label: &'a ConfiguredTargetLabel,
    execution_platform: &'a ExecutionPlatformResolution,
    cancellation: &'a CancellationContext,
) -> impl Future<Output = buck2_error::Result<AnalysisResult>> + 'a + Captures<'d> {
    let fut = async move {
        run_bazel_input_file_analysis_underlying(dice, label, execution_platform, cancellation)
            .await
    };
    unsafe { UnsafeSendFuture::new_encapsulates_starlark(fut) }
}

async fn run_bazel_input_file_analysis_underlying(
    _dice: &mut DiceComputations<'_>,
    label: &ConfiguredTargetLabel,
    _execution_platform: &ExecutionPlatformResolution,
    _cancellation: &CancellationContext,
) -> buck2_error::Result<AnalysisResult> {
    new_bazel_input_file_analysis_result(label)
}

pub(crate) fn new_bazel_input_file_analysis_result(
    label: &ConfiguredTargetLabel,
) -> buck2_error::Result<AnalysisResult> {
    let heap = FrozenHeap::new();
    let path = PackageRelativePath::new(label.unconfigured().name().as_str())?.to_arc();
    let source = SourceArtifact::new(SourcePath::new(label.unconfigured().pkg().dupe(), path));
    let source = heap.alloc(StarlarkArtifact::new_source(source.into(), false));
    let default_info = FrozenDefaultInfo::for_file_target(&heap, source);
    let providers = FrozenProviderCollection::new_default_info(&heap, default_info);
    let recorded_values = RecordedAnalysisValues::new_provider_collection(
        DeferredHolderKey::Base(BaseDeferredKey::TargetLabel(label.dupe())),
        heap,
        providers,
    );
    Ok(AnalysisResult::new(
        recorded_values,
        None,
        StdBuckHashMap::default(),
        0,
        0,
        None,
    ))
}

async fn run_bazel_output_file_analysis_underlying(
    _dice: &mut DiceComputations<'_>,
    label: &ConfiguredTargetLabel,
    results: Vec<(&ConfiguredTargetLabel, AnalysisResult)>,
    execution_platform: &ExecutionPlatformResolution,
    node: ConfiguredTargetNodeRef<'_>,
    cancellation: &CancellationContext,
) -> buck2_error::Result<AnalysisResult> {
    let output_name = match node
        .get(BAZEL_OUTPUT_FILE_OUTPUT_ATTR, AttrInspectOptions::All)
        .map(|attr| attr.value)
    {
        Some(ConfiguredAttr::String(value)) => value.0.to_string(),
        Some(other) => {
            return Err(BazelOutputFileAnalysisError::UnsupportedOutputAttrValue(
                label.dupe(),
                other.as_display_no_ctx().to_string(),
            )
            .into());
        }
        None => {
            return Err(BazelOutputFileAnalysisError::MissingOutputAttr(
                label.dupe(),
                BAZEL_OUTPUT_FILE_OUTPUT_ATTR,
            )
            .into());
        }
    };

    let mut results = results.into_iter();
    let Some((generating_label, generating_result)) = results.next() else {
        return Err(BazelOutputFileAnalysisError::MissingGeneratingRule(label.dupe()).into());
    };
    if results.next().is_some() {
        return Err(BazelOutputFileAnalysisError::MultipleGeneratingRules(label.dupe()).into());
    }

    BuckStarlarkModule::with_profiling_async(async move |env| {
        let print = EventDispatcherPrintHandler(get_dispatcher());
        let registry = AnalysisRegistry::new_from_owner(
            BaseDeferredKey::TargetLabel(label.dupe()),
            execution_platform.dupe(),
        )?;

        let eval_kind = StarlarkEvalKind::Analysis(label.dupe());
        let eval_provider = StarlarkEvaluatorProvider::passthrough(eval_kind);
        let mut reentrant_eval =
            eval_provider.make_reentrant_evaluator(&env, cancellation.into())?;

        let provider_collection = reentrant_eval.with_evaluator(|eval| {
            eval.set_print_handler(&print);
            eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);

            let generating_providers = generating_result.providers()?;
            let generating_provider_collection = generating_providers.value();
            let output_info = generating_provider_collection
                .builtin_provider::<FrozenBazelOutputFileInfo>()
                .ok_or_else(|| {
                    BazelOutputFileAnalysisError::MissingOutputFileInfo(generating_label.dupe())
                })?;
            let output = output_info.output(&output_name)?.ok_or_else(|| {
                BazelOutputFileAnalysisError::MissingOutputFile(
                    generating_label.dupe(),
                    output_name.clone(),
                )
            })?;

            eval.heap().add_reference(generating_providers.owner());
            let output = output.to_value();
            let default_info = eval
                .heap()
                .alloc(DefaultInfo::for_file_target(eval.heap(), output));
            let providers =
                ProviderCollection::try_from_value(eval.heap().alloc(AllocList([default_info])))?;
            Ok(ValueTypedComplex::new_err(eval.heap().alloc(providers))
                .internal_error("Just allocated provider collection")?)
        })?;

        registry
            .analysis_value_storage
            .set_result_value(provider_collection)?;

        let finished_eval = reentrant_eval.finish_evaluation();
        let declared_actions = registry.num_declared_actions();
        let declared_artifacts = registry.num_declared_artifacts();
        let registry_finalizer = registry.finalize(&env)?;
        let (token, frozen_env, profile_data) = finished_eval.freeze_and_finish(env)?;
        let recorded_values = registry_finalizer(&frozen_env)?;

        Ok((
            token,
            AnalysisResult::new(
                recorded_values,
                profile_data,
                StdBuckHashMap::default(),
                declared_actions,
                declared_artifacts,
                None,
            ),
        ))
    })
    .await
}

pub fn get_deps_from_analysis_results(
    results: Vec<(&ConfiguredTargetLabel, AnalysisResult)>,
) -> buck2_error::Result<StdBuckHashMap<ConfiguredTargetLabel, FrozenProviderCollectionValue>> {
    results
        .into_iter()
        .map(|(label, result)| Ok((label.dupe(), result.providers()?.to_owned())))
        .collect::<buck2_error::Result<StdBuckHashMap<ConfiguredTargetLabel, FrozenProviderCollectionValue>>>()
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelPredeclaredOutputError {
    #[error("Bazel predeclared output `{0}` has unsupported value `{1}`")]
    UnsupportedOutputAttrValue(String, String),
    #[error("Bazel implicit output template `{0}` references unsupported placeholder `{1}`")]
    UnsupportedImplicitOutputPlaceholder(String, String),
}

fn struct_field_value<'v>(
    value: ValueOfUnchecked<'v, StructRef<'static>>,
    field: &str,
) -> Option<Value<'v>> {
    StructRef::from_value(value.get())?
        .iter()
        .find_map(|(name, value)| (name.as_str() == field).then_some(value))
}

fn declare_bazel_output_artifact<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    registry: &mut AnalysisRegistry<'v>,
    output_path: &str,
    bazel_output_root: BazelOutputRoot,
) -> buck2_error::Result<Value<'v>> {
    let output_path = normalize_bazel_output_path(output_path);
    let artifact = registry.declare_bazel_predeclared_output(
        output_path,
        OutputType::File,
        None,
        BuckOutPathKind::default(),
        bazel_output_root,
        eval.heap(),
    )?;
    Ok(eval
        .heap()
        .alloc_typed(StarlarkDeclaredArtifact::new(
            None,
            artifact,
            AssociatedArtifacts::new(),
        ))
        .to_value())
}

fn normalize_bazel_output_path(value: &str) -> &str {
    value
        .strip_prefix(':')
        .filter(|output_path| !output_path.is_empty())
        .unwrap_or(value)
}

fn declare_bazel_output_attr<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    registry: &mut AnalysisRegistry<'v>,
    attr_name: &str,
    value: Value<'v>,
    output_file_targets: &mut Vec<(String, Value<'v>)>,
    bazel_output_root: BazelOutputRoot,
) -> buck2_error::Result<Value<'v>> {
    if value.is_none() {
        return Ok(Value::new_none());
    }
    if let Some(output_path) = value.unpack_str() {
        let artifact =
            declare_bazel_output_artifact(eval, registry, output_path, bazel_output_root)?;
        output_file_targets.push((
            normalize_bazel_output_path(output_path).to_owned(),
            artifact,
        ));
        return Ok(artifact);
    }
    if let Some(values) = ListRef::from_value(value) {
        let mut artifacts = Vec::with_capacity(values.len());
        for value in values.iter() {
            let Some(output_path) = value.unpack_str() else {
                return Err(BazelPredeclaredOutputError::UnsupportedOutputAttrValue(
                    attr_name.to_owned(),
                    value.to_repr(),
                )
                .into());
            };
            let artifact =
                declare_bazel_output_artifact(eval, registry, output_path, bazel_output_root)?;
            output_file_targets.push((
                normalize_bazel_output_path(output_path).to_owned(),
                artifact,
            ));
            artifacts.push(artifact);
        }
        return Ok(eval.heap().alloc(AllocList(artifacts)));
    }
    Err(BazelPredeclaredOutputError::UnsupportedOutputAttrValue(
        attr_name.to_owned(),
        value.to_repr(),
    )
    .into())
}

fn implicit_output_template_value<'v>(
    attrs: ValueOfUnchecked<'v, StructRef<'static>>,
    target_name: &str,
    attr_name: &str,
) -> buck2_error::Result<String> {
    if attr_name == "name" {
        return Ok(target_name.to_owned());
    }
    let Some(value) = struct_field_value(attrs, attr_name) else {
        return Err(
            BazelPredeclaredOutputError::UnsupportedImplicitOutputPlaceholder(
                target_name.to_owned(),
                attr_name.to_owned(),
            )
            .into(),
        );
    };
    value.unpack_str().map(str::to_owned).ok_or_else(|| {
        BazelPredeclaredOutputError::UnsupportedImplicitOutputPlaceholder(
            target_name.to_owned(),
            format!("{attr_name}={}", value.to_repr()),
        )
        .into()
    })
}

fn expand_bazel_implicit_output_template<'v>(
    attrs: ValueOfUnchecked<'v, StructRef<'static>>,
    target_name: &str,
    template: &str,
) -> buck2_error::Result<String> {
    let mut output = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("%{") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            return Err(
                BazelPredeclaredOutputError::UnsupportedImplicitOutputPlaceholder(
                    template.to_owned(),
                    after_start.to_owned(),
                )
                .into(),
            );
        };
        let attr_name = &after_start[..end];
        output.push_str(&implicit_output_template_value(
            attrs,
            target_name,
            attr_name,
        )?);
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn declare_bazel_predeclared_outputs<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    registry: &mut AnalysisRegistry<'v>,
    attrs: ValueOfUnchecked<'v, StructRef<'static>>,
    node: ConfiguredTargetNodeRef<'_>,
) -> buck2_error::Result<(
    ValueOfUnchecked<'v, StructRef<'static>>,
    Option<Value<'v>>,
    Vec<Value<'v>>,
    Vec<(String, Value<'v>)>,
)> {
    let owned_node = node.to_owned();
    let target_node = owned_node.target_node();
    let bazel_output_root = if node.bazel_output_to_genfiles() {
        BazelOutputRoot::Genfiles
    } else {
        BazelOutputRoot::Bin
    };
    let mut output_fields = Vec::new();
    let mut output_file_targets = Vec::new();
    for output_attr in &target_node.rule.bazel_output_attrs {
        let value = struct_field_value(attrs, &output_attr.name).unwrap_or_else(Value::new_none);
        let output_value = declare_bazel_output_attr(
            eval,
            registry,
            &output_attr.name,
            value,
            &mut output_file_targets,
            bazel_output_root,
        )?;
        output_fields.push((output_attr.name.to_string(), output_value));
    }

    let target_name = node.label().unconfigured().name().as_str();
    for output in &target_node.rule.bazel_implicit_outputs {
        let output_path =
            expand_bazel_implicit_output_template(attrs, target_name, &output.template)?;
        let artifact =
            declare_bazel_output_artifact(eval, registry, &output_path, bazel_output_root)?;
        output_file_targets.push((output_path, artifact));
        output_fields.push((output.name.to_string(), artifact));
    }

    let outputs_struct = ValueOfUnchecked::new(eval.heap().alloc(AllocStruct(output_fields)));
    let predeclared_outputs = output_file_targets
        .iter()
        .map(|(_, artifact)| *artifact)
        .collect::<Vec<_>>();
    let predeclared_output_files = output_file_targets.clone();
    let output_file_info = if output_file_targets.is_empty() {
        None
    } else {
        let info = new_bazel_output_file_info(output_file_targets, eval);
        Some(eval.heap().alloc(info))
    };
    Ok((
        outputs_struct,
        output_file_info,
        predeclared_outputs,
        predeclared_output_files,
    ))
}

fn configured_node_build_file_path(node: ConfiguredTargetNodeRef<'_>) -> String {
    let package = node.buildfile_path().package();
    let package = package.cell_relative_path().as_str();
    if package.is_empty() {
        node.buildfile_path().filename().as_str().to_owned()
    } else {
        format!("{}/{}", package, node.buildfile_path().filename().as_str())
    }
}

fn public_attrs_struct<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    attrs: ValueOfUnchecked<'v, StructRef<'static>>,
    overrides: &SmallMap<String, Value<'v>>,
) -> buck2_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    let attrs = StructRef::from_value(attrs.get())
        .ok_or_else(|| internal_error!("ctx.attrs should be a struct"))?;
    let mut fields = Vec::new();
    for (name, value) in attrs.iter() {
        let name = name.as_str();
        if is_bazel_aspect_hidden_attr(name) {
            continue;
        }
        fields.push((
            name.to_owned(),
            overrides.get(name).copied().unwrap_or(value),
        ));
    }
    Ok(ValueOfUnchecked::new(
        eval.heap().alloc(AllocStruct(fields)),
    ))
}

fn bazel_split_attr_value<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    deps: &ConfiguredSplitTransitionDep,
    ctx: &RuleAnalysisAttrResolutionContext<'_, 'v>,
) -> buck2_error::Result<Value<'v>> {
    let mut entries = Vec::with_capacity(deps.deps.len());
    for (key, target) in &deps.deps {
        let key = if key.is_empty() {
            Value::new_none()
        } else {
            eval.heap().alloc_str(key).to_value()
        };
        let value =
            DepAttrType::resolve_single_impl(&mut &*ctx, target, &deps.required_providers, false)?;
        entries.push((key, value));
    }
    Ok(eval.heap().alloc(AllocDict(entries)))
}

fn node_to_bazel_split_attrs_struct<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    node: ConfiguredTargetNodeRef,
    ctx: &RuleAnalysisAttrResolutionContext<'_, 'v>,
) -> buck2_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    let mut fields = Vec::new();
    for attr in node.attrs(AttrInspectOptions::All) {
        if let ConfiguredAttr::SplitTransitionDep(dep) = &attr.value {
            fields.push((
                attr.name.to_owned(),
                bazel_split_attr_value(eval, dep.as_ref(), ctx)?,
            ));
        }
    }
    Ok(ValueOfUnchecked::new(
        eval.heap().alloc(AllocStruct(fields)),
    ))
}

fn bazel_source_target_dependency<'v>(
    label: &ConfiguredProvidersLabel,
    ctx: &mut dyn AttrResolutionContext<'v>,
) -> buck2_error::Result<Value<'v>> {
    let path = PackageRelativePath::new(label.target().unconfigured().name().as_str())?.to_arc();
    let source = SourceArtifact::new(SourcePath::new(
        label.target().unconfigured().pkg().dupe(),
        path,
    ));
    let source = ctx
        .heap()
        .alloc(StarlarkArtifact::new_source(source.into(), false));
    let default_info = ctx
        .heap()
        .alloc(DefaultInfo::for_file_target(ctx.heap(), source));
    let providers =
        ProviderCollection::try_from_value(ctx.heap().alloc(AllocList([default_info])))?;
    Ok(ctx
        .heap()
        .alloc(Dependency::new_with_runtime_provider_collection(
            ctx.heap(),
            label.dupe(),
            providers,
            None,
        )))
}

fn resolve_bazel_source_label_for_aspect_rule_attr<'v>(
    label: &ConfiguredProvidersLabel,
    ctx: &mut dyn AttrResolutionContext<'v>,
) -> buck2_error::Result<Value<'v>> {
    resolve_bazel_dep_label_for_aspect_rule_attr(label, &ProviderIdSet::EMPTY, false, ctx)
}

fn resolve_bazel_dep_label_for_aspect_rule_attr<'v>(
    label: &ConfiguredProvidersLabel,
    required_providers: &ProviderIdSet,
    is_exec: bool,
    ctx: &mut dyn AttrResolutionContext<'v>,
) -> buck2_error::Result<Value<'v>> {
    match DepAttrType::resolve_single_impl(ctx, label, required_providers, is_exec) {
        Ok(value) => Ok(value),
        Err(_) => bazel_source_target_dependency(label, ctx),
    }
}

fn resolve_bazel_rule_attr_list_item_for_aspect<'v>(
    attr: &ConfiguredAttr,
    pkg: PackageLabel,
    ctx: &mut dyn AttrResolutionContext<'v>,
) -> buck2_error::Result<Vec<Value<'v>>> {
    match attr {
        ConfiguredAttr::SourceLabel(label) => {
            Ok(vec![resolve_bazel_source_label_for_aspect_rule_attr(
                label, ctx,
            )?])
        }
        ConfiguredAttr::Dep(dep) => {
            let is_exec = matches!(
                &dep.attr_type.transition,
                DepAttrTransition::Exec | DepAttrTransition::Toolchain
            );
            Ok(vec![resolve_bazel_dep_label_for_aspect_rule_attr(
                &dep.label,
                &dep.attr_type.required_providers,
                is_exec,
                ctx,
            )?])
        }
        ConfiguredAttr::ExplicitConfiguredDep(dep) => {
            Ok(vec![resolve_bazel_dep_label_for_aspect_rule_attr(
                &dep.label,
                &dep.attr_type.required_providers,
                false,
                ctx,
            )?])
        }
        ConfiguredAttr::TransitionDep(dep) => {
            Ok(vec![resolve_bazel_dep_label_for_aspect_rule_attr(
                &dep.dep,
                &dep.required_providers,
                false,
                ctx,
            )?])
        }
        ConfiguredAttr::OneOf(box attr, _) => {
            resolve_bazel_rule_attr_list_item_for_aspect(attr, pkg, ctx)
        }
        _ => attr.resolve(pkg, ctx),
    }
}

fn resolve_bazel_rule_attr_for_aspect<'v>(
    attr: &ConfiguredAttr,
    pkg: PackageLabel,
    ctx: &mut dyn AttrResolutionContext<'v>,
) -> buck2_error::Result<Value<'v>> {
    match attr {
        ConfiguredAttr::SourceLabel(label) => {
            resolve_bazel_source_label_for_aspect_rule_attr(label, ctx)
        }
        ConfiguredAttr::Dep(dep) => {
            let is_exec = matches!(
                &dep.attr_type.transition,
                DepAttrTransition::Exec | DepAttrTransition::Toolchain
            );
            resolve_bazel_dep_label_for_aspect_rule_attr(
                &dep.label,
                &dep.attr_type.required_providers,
                is_exec,
                ctx,
            )
        }
        ConfiguredAttr::ExplicitConfiguredDep(dep) => resolve_bazel_dep_label_for_aspect_rule_attr(
            &dep.label,
            &dep.attr_type.required_providers,
            false,
            ctx,
        ),
        ConfiguredAttr::TransitionDep(dep) => resolve_bazel_dep_label_for_aspect_rule_attr(
            &dep.dep,
            &dep.required_providers,
            false,
            ctx,
        ),
        ConfiguredAttr::List(list) => {
            let mut values = Vec::with_capacity(list.len());
            for item in list.iter() {
                values.append(&mut resolve_bazel_rule_attr_list_item_for_aspect(
                    item, pkg, ctx,
                )?);
            }
            Ok(ctx.heap().alloc(values))
        }
        ConfiguredAttr::OneOf(box attr, _) => resolve_bazel_rule_attr_for_aspect(attr, pkg, ctx),
        _ => attr.resolve_bazel(pkg, ctx),
    }
}

fn partial_node_to_attrs_struct<'v>(
    node: ConfiguredTargetNodeRef,
    ctx: &mut dyn AttrResolutionContext<'v>,
) -> buck2_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    let attrs_iter = node.attrs(AttrInspectOptions::All);
    let mut resolved_attrs = Vec::with_capacity(attrs_iter.size_hint().0);
    for a in attrs_iter {
        match resolve_bazel_rule_attr_for_aspect(&a.value, node.label().pkg(), ctx) {
            Ok(value) => resolved_attrs.push((a.name, value)),
            Err(_e) => {}
        }
    }
    Ok(ctx
        .heap()
        .alloc_typed_unchecked(AllocStruct(resolved_attrs))
        .cast())
}

fn frozen_bazel_aspect_info(aspect: Value<'_>) -> buck2_error::Result<FrozenBazelAspectInfo> {
    let frozen = aspect.unpack_frozen().ok_or_else(|| {
        internal_error!(
            "Bazel aspect `{}` should be a frozen value during analysis",
            aspect.to_repr()
        )
    })?;
    (FROZEN_BAZEL_ASPECT_INFO_GET_IMPL.get()?)(frozen)
}

fn provider_requirements_satisfied<'v>(
    providers: Value<'v>,
    required: &[FrozenValue],
) -> buck2_error::Result<bool> {
    for provider in required {
        if !providers.is_in(provider.to_value())? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn aspect_attrs_struct<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    rule_attr: &str,
    aspect_path: &str,
    aspect_info: &FrozenBazelAspectInfo,
) -> buck2_error::Result<Value<'v>> {
    let mut fields = Vec::new();
    for attr_name in &aspect_info.attrs {
        let hidden = bazel_aspect_hidden_attr_name(rule_attr, aspect_path, attr_name);
        let Some(value) = struct_field_value(attrs_with_hidden, &hidden) else {
            return Err(internal_error!(
                "Bazel aspect attr `{}` for rule attr `{}` at aspect path `{}` was not resolved as hidden attr `{}`",
                attr_name,
                rule_attr,
                aspect_path,
                hidden,
            ));
        };
        fields.push((attr_name.clone(), value));
    }
    Ok(eval.heap().alloc(AllocStruct(fields)))
}

fn find_direct_dep_node<'a>(
    node: ConfiguredTargetNodeRef<'a>,
    label: &ConfiguredTargetLabel,
) -> buck2_error::Result<ConfiguredTargetNodeRef<'a>> {
    node.deps()
        .find_map(|dep| (dep.label() == label).then(|| dep.as_ref()))
        .ok_or_else(|| {
            internal_error!(
                "Bazel aspect dependency `{}` was not present in configured deps for `{}`",
                label,
                node.label()
            )
        })
}

fn configured_attr_dep_label(attr: &ConfiguredAttr) -> Option<&ConfiguredProvidersLabel> {
    match attr {
        ConfiguredAttr::Dep(dep) => Some(&dep.label),
        ConfiguredAttr::ExplicitConfiguredDep(dep) => Some(&dep.label),
        ConfiguredAttr::TransitionDep(dep) => Some(&dep.dep),
        ConfiguredAttr::OneOf(box attr, _) => configured_attr_dep_label(attr),
        _ => None,
    }
}

fn collect_configured_attr_dep_labels(
    attr: &ConfiguredAttr,
    labels: &mut Vec<ConfiguredProvidersLabel>,
) {
    match attr {
        ConfiguredAttr::Dep(dep) => labels.push(dep.label.dupe()),
        ConfiguredAttr::ExplicitConfiguredDep(dep) => labels.push(dep.label.dupe()),
        ConfiguredAttr::TransitionDep(dep) => labels.push(dep.dep.dupe()),
        ConfiguredAttr::SplitTransitionDep(dep) => {
            labels.extend(dep.deps.values().cloned());
        }
        ConfiguredAttr::SourceLabel(label) => labels.push(label.dupe()),
        ConfiguredAttr::List(list) => {
            for item in list.iter() {
                collect_configured_attr_dep_labels(item, labels);
            }
        }
        ConfiguredAttr::Tuple(tuple) => {
            for item in tuple.iter() {
                collect_configured_attr_dep_labels(item, labels);
            }
        }
        ConfiguredAttr::Dict(dict) => {
            for (key, value) in dict.iter() {
                collect_configured_attr_dep_labels(key, labels);
                collect_configured_attr_dep_labels(value, labels);
            }
        }
        ConfiguredAttr::OneOf(box attr, _) => collect_configured_attr_dep_labels(attr, labels),
        _ => {}
    }
}

const BAZEL_DEFAULT_MAKE_VARIABLE_ATTRIBUTES: &[&str] = &[
    "toolchains",
    ":cc_toolchain",
    "$toolchains",
    "$cc_toolchain",
];

fn collect_bazel_make_variable_label_attr_template_variables<'v>(
    attr: &ConfiguredAttr,
    dep_analysis_results: &StdBuckHashMap<ConfiguredTargetLabel, FrozenProviderCollectionValue>,
    module: &Module<'v>,
    variables: &mut Vec<Value<'v>>,
) -> buck2_error::Result<()> {
    match attr {
        ConfiguredAttr::Label(label) => {
            let provider_collection = get_dep(dep_analysis_results, label, module)?;
            if let Some(template_variable_info) = provider_collection
                .as_ref()
                .builtin_provider::<FrozenTemplateVariableInfo>()
            {
                variables.push(template_variable_info.to_value());
            }
        }
        ConfiguredAttr::List(list) => {
            for item in list.iter() {
                collect_bazel_make_variable_label_attr_template_variables(
                    item,
                    dep_analysis_results,
                    module,
                    variables,
                )?;
            }
        }
        ConfiguredAttr::Tuple(tuple) => {
            for item in tuple.iter() {
                collect_bazel_make_variable_label_attr_template_variables(
                    item,
                    dep_analysis_results,
                    module,
                    variables,
                )?;
            }
        }
        ConfiguredAttr::Dict(dict) => {
            for (key, value) in dict.iter() {
                collect_bazel_make_variable_label_attr_template_variables(
                    key,
                    dep_analysis_results,
                    module,
                    variables,
                )?;
                collect_bazel_make_variable_label_attr_template_variables(
                    value,
                    dep_analysis_results,
                    module,
                    variables,
                )?;
            }
        }
        ConfiguredAttr::OneOf(box attr, _) => {
            collect_bazel_make_variable_label_attr_template_variables(
                attr,
                dep_analysis_results,
                module,
                variables,
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn collect_bazel_make_variable_attr_template_variables<'v>(
    node: ConfiguredTargetNodeRef<'_>,
    resolution_ctx: &RuleAnalysisAttrResolutionContext<'_, 'v>,
) -> buck2_error::Result<Vec<Value<'v>>> {
    let mut variables = Vec::new();
    for attr_name in BAZEL_DEFAULT_MAKE_VARIABLE_ATTRIBUTES {
        if let Some(attr) = node.get(attr_name, AttrInspectOptions::All) {
            collect_bazel_make_variable_label_attr_template_variables(
                &attr.value,
                &resolution_ctx.dep_analysis_results,
                resolution_ctx.module,
                &mut variables,
            )?;
        }
    }
    Ok(variables)
}

fn bazel_aspect_actual_dep_node<'a>(
    node: ConfiguredTargetNodeRef<'a>,
) -> buck2_error::Result<ConfiguredTargetNodeRef<'a>> {
    let mut current = node;
    loop {
        if current.dupe().rule_type().name() != "alias" {
            return Ok(current);
        }
        let Some(actual) = current.dupe().get("actual", AttrInspectOptions::All) else {
            return Ok(current);
        };
        let Some(actual_label) = configured_attr_dep_label(&actual.value) else {
            return Ok(current);
        };
        current = find_direct_dep_node(current, actual_label.target())?;
    }
}

#[derive(Clone, Eq, PartialEq, Hash)]
struct BazelAspectAnalysisSpec {
    attrs: Vec<String>,
    attr_aspects: Vec<String>,
    requires: Vec<BazelAspectAnalysisSpec>,
}

type BazelAspectApplicationCache<'v> =
    StdBuckHashMap<(ConfiguredProvidersLabel, String, String), ProviderCollection<'v>>;

#[derive(Clone)]
struct BazelAspectApplication<'v> {
    origin_rule_attr: String,
    aspect_path: String,
    aspect: Value<'v>,
}

fn bazel_aspect_analysis_spec(aspect: Value<'_>) -> buck2_error::Result<BazelAspectAnalysisSpec> {
    let aspect_info = frozen_bazel_aspect_info(aspect)?;
    Ok(BazelAspectAnalysisSpec {
        attrs: aspect_info.attrs,
        attr_aspects: aspect_info.attr_aspects,
        requires: aspect_info
            .requires
            .into_iter()
            .map(|aspect| bazel_aspect_analysis_spec(aspect.to_value()))
            .collect::<buck2_error::Result<Vec<_>>>()?,
    })
}

fn collect_bazel_aspect_hidden_attr_deps(
    node: ConfiguredTargetNodeRef<'_>,
    rule_attr: &str,
    aspect_path: &str,
    aspect: &BazelAspectAnalysisSpec,
    labels: &mut StdBuckHashSet<ConfiguredTargetLabel>,
) -> buck2_error::Result<()> {
    for attr_name in &aspect.attrs {
        let hidden = bazel_aspect_hidden_attr_name(rule_attr, aspect_path, attr_name);
        if let Some(attr) = node.get(&hidden, AttrInspectOptions::All) {
            let mut dep_labels = Vec::new();
            collect_configured_attr_dep_labels(&attr.value, &mut dep_labels);
            for dep_label in dep_labels {
                labels.insert(dep_label.target().dupe());
            }
        }
    }

    for (idx, required) in aspect.requires.iter().enumerate() {
        let required_path = format!("{aspect_path}r{idx}");
        collect_bazel_aspect_hidden_attr_deps(node, rule_attr, &required_path, required, labels)?;
    }

    Ok(())
}

fn collect_bazel_aspect_analysis_dep_edge(
    parent_node: ConfiguredTargetNodeRef<'_>,
    dep_label: &ConfiguredProvidersLabel,
    aspects: &[BazelAspectAnalysisSpec],
    labels: &mut StdBuckHashSet<ConfiguredTargetLabel>,
    visited: &mut StdBuckHashSet<(ConfiguredTargetLabel, BazelAspectAnalysisSpec)>,
) -> buck2_error::Result<()> {
    labels.insert(dep_label.target().dupe());
    let dep_node = find_direct_dep_node(parent_node, dep_label.target())?;
    let unwrapped_dep_node = dep_node.to_owned().unwrap_forward().dupe();
    let aspect_dep_node = bazel_aspect_actual_dep_node(unwrapped_dep_node.as_ref())?;
    for aspect in aspects {
        collect_bazel_aspect_analysis_deps_for_aspect(aspect_dep_node, aspect, labels, visited)?;
    }
    Ok(())
}

fn collect_bazel_aspect_analysis_deps_for_aspect(
    node: ConfiguredTargetNodeRef<'_>,
    aspect: &BazelAspectAnalysisSpec,
    labels: &mut StdBuckHashSet<ConfiguredTargetLabel>,
    visited: &mut StdBuckHashSet<(ConfiguredTargetLabel, BazelAspectAnalysisSpec)>,
) -> buck2_error::Result<()> {
    if !visited.insert((node.label().dupe(), aspect.clone())) {
        return Ok(());
    }

    collect_bazel_aspect_base_target_deps(node, labels);

    for required in &aspect.requires {
        collect_bazel_aspect_analysis_deps_for_aspect(node, required, labels, visited)?;
    }

    for attr_aspect in &aspect.attr_aspects {
        if attr_aspect == "*" {
            for attr in node.attrs(AttrInspectOptions::All) {
                if is_bazel_aspect_hidden_attr(attr.name) {
                    continue;
                }
                collect_bazel_aspect_analysis_deps_from_attr(
                    node,
                    &attr.value,
                    aspect,
                    labels,
                    visited,
                )?;
            }
        } else if let Some(attr) = node.get(attr_aspect, AttrInspectOptions::All) {
            collect_bazel_aspect_analysis_deps_from_attr(
                node,
                &attr.value,
                aspect,
                labels,
                visited,
            )?;
        }
    }

    Ok(())
}

fn collect_bazel_aspect_base_target_deps(
    node: ConfiguredTargetNodeRef<'_>,
    labels: &mut StdBuckHashSet<ConfiguredTargetLabel>,
) {
    for attr in node.attrs(AttrInspectOptions::All) {
        if is_bazel_aspect_hidden_attr(attr.name) {
            continue;
        }
        let mut dep_labels = Vec::new();
        collect_configured_attr_dep_labels(&attr.value, &mut dep_labels);
        for dep_label in dep_labels {
            if find_direct_dep_node(node, dep_label.target()).is_ok() {
                labels.insert(dep_label.target().dupe());
            }
        }
    }
}

fn collect_bazel_aspect_analysis_deps_from_attr(
    node: ConfiguredTargetNodeRef<'_>,
    attr: &ConfiguredAttr,
    aspect: &BazelAspectAnalysisSpec,
    labels: &mut StdBuckHashSet<ConfiguredTargetLabel>,
    visited: &mut StdBuckHashSet<(ConfiguredTargetLabel, BazelAspectAnalysisSpec)>,
) -> buck2_error::Result<()> {
    let mut dep_labels = Vec::new();
    collect_configured_attr_dep_labels(attr, &mut dep_labels);
    for dep_label in dep_labels {
        if find_direct_dep_node(node, dep_label.target()).is_err() {
            continue;
        }
        collect_bazel_aspect_analysis_dep_edge(
            node,
            &dep_label,
            std::slice::from_ref(aspect),
            labels,
            visited,
        )?;
    }
    Ok(())
}

fn collect_bazel_aspect_analysis_deps(
    eval: &mut Evaluator<'_, '_, '_>,
    rule_spec: &dyn RuleSpec,
    node: ConfiguredTargetNodeRef<'_>,
) -> buck2_error::Result<Vec<ConfiguredTargetLabel>> {
    let attr_aspects = rule_spec.bazel_attr_aspects(eval)?;
    let attr_aspects = attr_aspects
        .into_iter()
        .map(|(name, aspects)| {
            Ok((
                name,
                aspects
                    .into_iter()
                    .map(bazel_aspect_analysis_spec)
                    .collect::<buck2_error::Result<Vec<_>>>()?,
            ))
        })
        .collect::<buck2_error::Result<SmallMap<_, _>>>()?;
    let mut labels = StdBuckHashSet::default();
    let mut visited = StdBuckHashSet::default();
    for (attr_name, aspects) in attr_aspects {
        for (idx, aspect) in aspects.iter().enumerate() {
            collect_bazel_aspect_hidden_attr_deps(
                node,
                &attr_name,
                &idx.to_string(),
                aspect,
                &mut labels,
            )?;
        }

        let Some(attr) = node.get(&attr_name, AttrInspectOptions::All) else {
            continue;
        };
        let mut dep_labels = Vec::new();
        collect_configured_attr_dep_labels(&attr.value, &mut dep_labels);
        for dep_label in dep_labels {
            if find_direct_dep_node(node, dep_label.target()).is_err() {
                continue;
            }
            collect_bazel_aspect_analysis_dep_edge(
                node,
                &dep_label,
                &aspects,
                &mut labels,
                &mut visited,
            )?;
        }
    }
    Ok(labels.into_iter().collect())
}

fn apply_bazel_aspect_to_dep<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    ctx: ValueTyped<'v, AnalysisContext<'v>>,
    node: ConfiguredTargetNodeRef<'_>,
    attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    resolution_ctx: &RuleAnalysisAttrResolutionContext<'_, 'v>,
    aspect_cache: &mut BazelAspectApplicationCache<'v>,
    rule_attr: &str,
    aspect_path: &str,
    aspect: Value<'v>,
    dep_node: ConfiguredTargetNodeRef<'_>,
    dep_label: &ConfiguredProvidersLabel,
    base_provider_collection: FrozenValueTyped<'v, FrozenProviderCollection>,
    dep_rule_attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    mut providers: ProviderCollection<'v>,
) -> buck2_error::Result<ProviderCollection<'v>> {
    let _ = node;
    let aspect_info = frozen_bazel_aspect_info(aspect)?;
    let cache_key = (
        dep_label.dupe(),
        rule_attr.to_owned(),
        aspect_path.to_owned(),
    );
    if let Some(providers) = aspect_cache.get(&cache_key) {
        return Ok(providers.shallow_clone());
    }
    for (idx, required_aspect) in aspect_info.requires.iter().enumerate() {
        let required_path = format!("{aspect_path}r{idx}");
        providers = apply_bazel_aspect_to_dep(
            eval,
            ctx,
            node,
            attrs_with_hidden,
            resolution_ctx,
            aspect_cache,
            rule_attr,
            &required_path,
            required_aspect.to_value(),
            dep_node,
            dep_label,
            base_provider_collection,
            dep_rule_attrs_with_hidden,
            providers,
        )?;
    }

    let target = eval.heap().alloc(Dependency::new_with_provider_collection(
        eval.heap(),
        dep_label.dupe(),
        base_provider_collection,
        providers.shallow_clone(),
        None,
    ));
    let has_required = provider_requirements_satisfied(target, &aspect_info.required_providers)?;
    let has_required_aspect =
        provider_requirements_satisfied(target, &aspect_info.required_aspect_providers)?;
    if !has_required || !has_required_aspect {
        aspect_cache.insert(cache_key, providers.shallow_clone());
        return Ok(providers);
    }

    let aspect_attrs = aspect_attrs_struct(
        eval,
        attrs_with_hidden,
        rule_attr,
        aspect_path,
        &aspect_info,
    )?;
    let dep_rule_attrs = apply_bazel_recursive_aspects_to_rule_attrs(
        eval,
        ctx,
        dep_node,
        dep_rule_attrs_with_hidden,
        attrs_with_hidden,
        resolution_ctx,
        aspect_cache,
        rule_attr,
        aspect_path,
        aspect,
        &aspect_info.attr_aspects,
    )?;
    let label = eval
        .heap()
        .alloc_typed(StarlarkConfiguredProvidersLabel::new(dep_label.dupe()));
    let build_file_path = configured_node_build_file_path(dep_node);
    let rule_kind = dep_node.rule_type().name().to_owned();
    let aspect_toolchains = ctx
        .as_ref()
        .actions
        .as_ref()
        .toolchains
        .as_ref()
        .with_declared_values(
            eval.heap(),
            aspect_info
                .toolchains
                .iter()
                .map(|toolchain| toolchain.to_value()),
        );
    let actions = ctx.as_ref().actions.as_ref();
    let previous_bazel_context =
        actions.replace_bazel_context_override(Some(BazelActionsContextOverride {
            label: Some(label),
            build_file_path: Some(build_file_path.clone()),
            rule_kind_name: Some(rule_kind.clone()),
            toolchains: Some(aspect_toolchains),
        }));
    let aspect_ctx = analysis_actions_to_bazel_ctx_with_overrides(
        ctx.as_ref().actions,
        eval.heap(),
        aspect_attrs,
        dep_rule_attrs.get(),
        Some(label),
        build_file_path.clone(),
        rule_kind.clone(),
    );

    let previous_attrs = actions
        .attributes
        .replace(Some(ValueOfUnchecked::new(aspect_attrs)));
    let aspect_res = eval.eval_function(
        aspect_info.implementation.to_value(),
        &[target, aspect_ctx],
        &[],
    );
    actions.attributes.replace(previous_attrs);
    actions.replace_bazel_context_override(previous_bazel_context);
    let aspect_res = aspect_res?;
    let aspect_providers = ProviderCollection::try_from_value_bazel_aspect(aspect_res)?;
    providers.extend_from(eval.heap(), aspect_providers)?;
    aspect_cache.insert(cache_key, providers.shallow_clone());
    Ok(providers)
}

fn apply_bazel_recursive_aspects_to_rule_attrs<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    ctx: ValueTyped<'v, AnalysisContext<'v>>,
    dep_node: ConfiguredTargetNodeRef<'_>,
    dep_rule_attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    aspect_attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    resolution_ctx: &RuleAnalysisAttrResolutionContext<'_, 'v>,
    aspect_cache: &mut BazelAspectApplicationCache<'v>,
    rule_attr: &str,
    aspect_path: &str,
    aspect: Value<'v>,
    attr_aspects: &[String],
) -> buck2_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    if attr_aspects.is_empty() {
        return public_attrs_struct(eval, dep_rule_attrs_with_hidden, &SmallMap::new());
    }

    let attrs = StructRef::from_value(dep_rule_attrs_with_hidden.get())
        .ok_or_else(|| internal_error!("ctx.attrs should be a struct"))?;
    let mut recursive_aspects = SmallMap::new();
    let application = BazelAspectApplication {
        origin_rule_attr: rule_attr.to_owned(),
        aspect_path: aspect_path.to_owned(),
        aspect,
    };
    for attr_aspect in attr_aspects {
        if attr_aspect == "*" {
            for (name, _) in attrs.iter() {
                let name = name.as_str();
                if !is_bazel_aspect_hidden_attr(name) {
                    recursive_aspects.insert(name.to_owned(), vec![application.clone()]);
                }
            }
        } else {
            recursive_aspects.insert(attr_aspect.clone(), vec![application.clone()]);
        }
    }

    apply_bazel_edge_aspects(
        eval,
        ctx,
        dep_node,
        dep_rule_attrs_with_hidden,
        aspect_attrs_with_hidden,
        resolution_ctx,
        aspect_cache,
        recursive_aspects,
    )
}

fn apply_bazel_aspects_to_dependency<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    ctx: ValueTyped<'v, AnalysisContext<'v>>,
    node: ConfiguredTargetNodeRef<'_>,
    _attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    aspect_attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    resolution_ctx: &RuleAnalysisAttrResolutionContext<'_, 'v>,
    aspect_cache: &mut BazelAspectApplicationCache<'v>,
    aspects: &[BazelAspectApplication<'v>],
    dep: &Dependency<'v>,
) -> buck2_error::Result<Value<'v>> {
    let dep_label = dep.configured_providers_label();
    let dep_node = find_direct_dep_node(node, dep_label.target())?;
    let unwrapped_dep_node = dep_node.to_owned().unwrap_forward().dupe();
    let aspect_dep_node = bazel_aspect_actual_dep_node(unwrapped_dep_node.as_ref())?;
    let aspect_dep_label =
        ConfiguredProvidersLabel::new(aspect_dep_node.label().dupe(), dep_label.name().dupe());
    let dep_rule_attrs = partial_node_to_attrs_struct(aspect_dep_node, &mut &*resolution_ctx)?;
    let base_provider_collection = dep.base_provider_collection();
    let mut providers = dep.provider_collection_shallow_clone();
    for application in aspects {
        providers = apply_bazel_aspect_to_dep(
            eval,
            ctx,
            node,
            aspect_attrs_with_hidden,
            resolution_ctx,
            aspect_cache,
            &application.origin_rule_attr,
            &application.aspect_path,
            application.aspect,
            aspect_dep_node,
            &aspect_dep_label,
            base_provider_collection,
            dep_rule_attrs,
            providers,
        )?;
    }
    Ok(eval.heap().alloc(Dependency::new_with_provider_collection(
        eval.heap(),
        aspect_dep_label,
        base_provider_collection,
        providers,
        dep.execution_platform()?,
    )))
}

fn apply_bazel_aspects_to_attr_value<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    ctx: ValueTyped<'v, AnalysisContext<'v>>,
    node: ConfiguredTargetNodeRef<'_>,
    attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    aspect_attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    resolution_ctx: &RuleAnalysisAttrResolutionContext<'_, 'v>,
    aspect_cache: &mut BazelAspectApplicationCache<'v>,
    aspects: &[BazelAspectApplication<'v>],
    value: Value<'v>,
) -> buck2_error::Result<Value<'v>> {
    if let Some(dep) = Dependency::from_value(value) {
        return apply_bazel_aspects_to_dependency(
            eval,
            ctx,
            node,
            attrs_with_hidden,
            aspect_attrs_with_hidden,
            resolution_ctx,
            aspect_cache,
            aspects,
            dep,
        );
    }
    if let Some(list) = ListRef::from_value(value) {
        let mut values = Vec::with_capacity(list.len());
        for item in list.iter() {
            values.push(apply_bazel_aspects_to_attr_value(
                eval,
                ctx,
                node,
                attrs_with_hidden,
                aspect_attrs_with_hidden,
                resolution_ctx,
                aspect_cache,
                aspects,
                item,
            )?);
        }
        return Ok(eval.heap().alloc(AllocList(values)));
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        let mut values = Vec::with_capacity(tuple.len());
        for item in tuple.content() {
            values.push(apply_bazel_aspects_to_attr_value(
                eval,
                ctx,
                node,
                attrs_with_hidden,
                aspect_attrs_with_hidden,
                resolution_ctx,
                aspect_cache,
                aspects,
                *item,
            )?);
        }
        return Ok(eval.heap().alloc(AllocTuple(values)));
    }
    Ok(value)
}

fn apply_bazel_edge_aspects<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    ctx: ValueTyped<'v, AnalysisContext<'v>>,
    node: ConfiguredTargetNodeRef<'_>,
    attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    aspect_attrs_with_hidden: ValueOfUnchecked<'v, StructRef<'static>>,
    resolution_ctx: &RuleAnalysisAttrResolutionContext<'_, 'v>,
    aspect_cache: &mut BazelAspectApplicationCache<'v>,
    attr_aspects: SmallMap<String, Vec<BazelAspectApplication<'v>>>,
) -> buck2_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    let mut overrides = SmallMap::new();
    for (name, aspects) in attr_aspects {
        if aspects.is_empty() {
            continue;
        }
        let Some(value) = struct_field_value(attrs_with_hidden, &name) else {
            continue;
        };
        let value = apply_bazel_aspects_to_attr_value(
            eval,
            ctx,
            node,
            attrs_with_hidden,
            aspect_attrs_with_hidden,
            resolution_ctx,
            aspect_cache,
            &aspects,
            value,
        )?;
        overrides.insert(name, value);
    }
    public_attrs_struct(eval, attrs_with_hidden, &overrides)
}

// Used to express that the impl Future below captures multiple named lifetimes.
// See https://github.com/rust-lang/rust/issues/34511#issuecomment-373423999 for more details.
pub(crate) trait Captures<'x> {}
impl<T: ?Sized> Captures<'_> for T {}

fn run_analysis_with_env<'a, 'd: 'a>(
    dice: &'a mut DiceComputations<'d>,
    analysis_env: AnalysisEnv<'a>,
    node: ConfiguredTargetNodeRef<'a>,
) -> impl Future<Output = buck2_error::Result<(AnalysisResult, Option<AnalysisSplitInstants>)>>
+ 'a
+ Captures<'d> {
    let fut = async move { run_analysis_with_env_underlying(dice, analysis_env, node).await };
    unsafe { UnsafeSendFuture::new_encapsulates_starlark(fut) }
}

fn bazel_config_list(value: Option<Arc<str>>) -> Vec<String> {
    value
        .as_deref()
        .map(|value| {
            value
                .split('\n')
                .filter(|value| !value.is_empty())
                .map(|value| value.to_owned())
                .collect()
        })
        .unwrap_or_default()
}

async fn bazel_cpp_options(
    dice: &mut DiceComputations<'_>,
) -> buck2_error::Result<BazelCppOptions> {
    let root_config = dice.get_legacy_root_config_on_dice().await?;
    let mut config = root_config.view(dice);
    Ok(BazelCppOptions {
        copt: bazel_config_list(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "copt",
        })?),
        conlyopt: bazel_config_list(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "conlyopt",
        })?),
        cxxopt: bazel_config_list(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "cxxopt",
        })?),
        host_copt: bazel_config_list(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "host_copt",
        })?),
        host_conlyopt: bazel_config_list(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "host_conlyopt",
        })?),
        host_cxxopt: bazel_config_list(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "host_cxxopt",
        })?),
        per_file_copt: bazel_config_list(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "per_file_copt",
        })?),
        macos_minimum_os: bazel_config_list(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "macos_minimum_os",
        })?),
        host_macos_minimum_os: bazel_config_list(config.get(BuckconfigKeyRef {
            section: "bazel",
            property: "host_macos_minimum_os",
        })?),
    })
}

async fn run_analysis_with_env_underlying(
    dice: &mut DiceComputations<'_>,
    analysis_env: AnalysisEnv<'_>,
    node: ConfiguredTargetNodeRef<'_>,
) -> buck2_error::Result<(AnalysisResult, Option<AnalysisSplitInstants>)> {
    let bazel_cpp_options = bazel_cpp_options(dice).await?;
    BuckStarlarkModule::with_profiling_async(async move |env| {
        let print = EventDispatcherPrintHandler(get_dispatcher());
        let label_resolution_context = if node.is_bazel_rule() {
            let package = node.label().pkg();
            let cell_name = package.cell_name();
            let cell_resolver = dice.get_cell_resolver().await?;
            let cell_alias_resolver = dice.get_cell_alias_resolver(cell_name).await?;
            Some(StarlarkLabelResolutionContext::new(
                cell_name,
                cell_resolver,
                cell_alias_resolver,
                Some(package),
            ))
        } else {
            None
        };

        let validations_from_deps = analysis_env
            .deps
            .iter()
            .filter_map(|(label, analysis_result)| {
                analysis_result
                    .validations
                    .dupe()
                    .map(|v| ((*label).dupe(), v))
            })
            .collect::<SmallMap<_, _>>();

        let bazel_aspect_analysis_deps = if node.is_bazel_rule() {
            let eval_kind = StarlarkEvalKind::Analysis(node.label().dupe());
            let eval_provider = StarlarkEvaluatorProvider::new(dice, eval_kind).await?;
            let mut reentrant_eval =
                eval_provider.make_reentrant_evaluator(&env, analysis_env.cancellation.into())?;
            reentrant_eval.with_evaluator(|eval| {
                if let Some(label_resolution_context) = &label_resolution_context {
                    eval.extra = Some(label_resolution_context);
                }
                collect_bazel_aspect_analysis_deps(eval, analysis_env.rule_spec, node)
            })?
        } else {
            Vec::new()
        };
        // Bazel requests aspect values from Skyframe as a batch. Keep Buck2's
        // synthetic aspect dependencies equally parallel instead of serializing
        // wide Starlark aspect graphs such as rules_go go_proto_library deps.
        let extra_dep_analysis_results =
            KeepGoing::try_compute_join_all(dice, bazel_aspect_analysis_deps, |dice, dep| {
                async move {
                    let result = dice.get_analysis_result(&dep).await?.require_compatible()?;
                    buck2_error::Ok((dep, result))
                }
                .boxed()
            })
            .await?;

        let mut dep_analysis_results = get_deps_from_analysis_results(analysis_env.deps)?;
        for (label, result) in extra_dep_analysis_results {
            dep_analysis_results
                .entry(label)
                .or_insert(result.providers()?.to_owned());
        }
        let resolution_ctx = RuleAnalysisAttrResolutionContext {
            module: &env,
            dep_analysis_results,
            query_results: analysis_env.query_results,
            execution_platform_resolution: node.execution_platform_resolution().clone(),
        };

        let attributes = node_to_attrs_struct(node, &mut &resolution_ctx)?;
        let plugins = plugins_to_starlark_value(node, &mut &resolution_ctx)?;
        let mut resolved_toolchains = SmallMap::new();
        let mut resolved_toolchain_template_variables = Vec::new();
        for resolved in node.bazel_resolved_toolchains() {
            let provider_collection = get_dep(
                &resolution_ctx.dep_analysis_results,
                &resolved.toolchain,
                &env,
            )?;
            if let Some(toolchain_info) = provider_collection
                .as_ref()
                .builtin_provider::<FrozenToolchainInfo>()
            {
                resolved_toolchains
                    .insert(resolved.toolchain_type.clone(), toolchain_info.to_value());
            }
            if let Some(template_variable_info) = provider_collection
                .as_ref()
                .builtin_provider::<FrozenTemplateVariableInfo>()
            {
                resolved_toolchain_template_variables.push(template_variable_info.to_value());
            }
        }
        resolved_toolchain_template_variables.extend(
            collect_bazel_make_variable_attr_template_variables(node, &resolution_ctx)?,
        );

        let registry = AnalysisRegistry::new_from_owner_and_deferred(
            analysis_env.execution_platform.dupe(),
            buck2_core::deferred::key::DeferredHolderKey::Base(BaseDeferredKey::TargetLabel(
                node.label().dupe(),
            )),
            Some(analysis_env.action_owner_rule_type_name.dupe()),
        )?;

        let eval_kind = StarlarkEvalKind::Analysis(node.label().dupe());
        let eval_provider = StarlarkEvaluatorProvider::new(dice, eval_kind).await?;
        let mut reentrant_eval =
            eval_provider.make_reentrant_evaluator(&env, analysis_env.cancellation.into())?;

        let (ctx, list_res, output_file_info, predeclared_outputs) = reentrant_eval
            .with_evaluator(|eval| {
                if let Some(label_resolution_context) = &label_resolution_context {
                    eval.extra = Some(label_resolution_context);
                }
                eval.set_print_handler(&print);
                eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);

                let mut registry = registry;
                let (outputs, output_file_info, predeclared_outputs, predeclared_output_files) =
                    declare_bazel_predeclared_outputs(eval, &mut registry, attributes, node)?;
                let build_file_path = configured_node_build_file_path(node);
                let split_attributes = if node.is_bazel_rule() {
                    Some(node_to_bazel_split_attrs_struct(
                        eval,
                        node,
                        &resolution_ctx,
                    )?)
                } else {
                    None
                };

                let ctx = AnalysisContext::prepare(
                    eval.heap(),
                    Some(attributes),
                    split_attributes,
                    Some(outputs),
                    predeclared_output_files,
                    Some(analysis_env.label),
                    Some(plugins.into()),
                    node.bazel_toolchains()
                        .iter()
                        .map(|toolchain| toolchain.toolchain_type.clone())
                        .collect(),
                    resolved_toolchains,
                    resolved_toolchain_template_variables,
                    bazel_cpp_options,
                    if node.bazel_output_to_genfiles() {
                        BazelOutputRoot::Genfiles
                    } else {
                        BazelOutputRoot::Bin
                    },
                    node.is_bazel_build_setting(),
                    Some(build_file_path),
                    Some(node.rule_type().name().to_owned()),
                    registry,
                    dice.global_data().get_digest_config(),
                );

                if node.is_bazel_rule() {
                    let attr_aspects = analysis_env
                        .rule_spec
                        .bazel_attr_aspects(eval)?
                        .into_iter()
                        .map(|(name, aspects)| {
                            let applications = aspects
                                .into_iter()
                                .enumerate()
                                .map(|(idx, aspect)| BazelAspectApplication {
                                    origin_rule_attr: name.clone(),
                                    aspect_path: idx.to_string(),
                                    aspect,
                                })
                                .collect::<Vec<_>>();
                            (name, applications)
                        })
                        .collect::<SmallMap<_, _>>();
                    let mut aspect_cache = BazelAspectApplicationCache::default();
                    let attributes = apply_bazel_edge_aspects(
                        eval,
                        ctx,
                        node,
                        attributes,
                        attributes,
                        &resolution_ctx,
                        &mut aspect_cache,
                        attr_aspects,
                    )?;
                    ctx.as_ref().set_attrs(attributes);
                }

                let list_res = analysis_env.rule_spec.invoke(eval, ctx)?;

                Ok((ctx, list_res, output_file_info, predeclared_outputs))
            })?;

        let pre_promises = Instant::now();
        let resolved_any = ctx
            .actions
            .run_promises(&mut RunAnonPromisesAccessorPair(&mut reentrant_eval, dice))
            .await?;
        let post_promises = Instant::now();

        let split_instants = if resolved_any {
            Some(AnalysisSplitInstants {
                pre_promises,
                post_promises,
            })
        } else {
            None
        };

        // TODO: Convert the ValueError from `try_from_value` better than just printing its Debug
        let mut res_typed = reentrant_eval.with_evaluator(|eval| {
            if let Some(label_resolution_context) = &label_resolution_context {
                eval.extra = Some(label_resolution_context);
            }
            let res_typed = if node.is_bazel_rule() {
                ProviderCollection::try_from_value_bazel_rule(
                    list_res,
                    eval.heap(),
                    predeclared_outputs,
                )?
            } else {
                ProviderCollection::try_from_value(list_res)?
            };
            buck2_error::Ok(res_typed)
        })?;

        // Pull the ctx object back out, and steal ctx.action's state back.
        let analysis_registry = ctx.take_state();

        if let Some(output_file_info) = output_file_info {
            res_typed.insert_provider(output_file_info)?;
        }
        {
            let provider_collection = ValueTypedComplex::new_err(env.heap().alloc(res_typed))
                .internal_error("Just allocated provider collection")?;
            analysis_registry
                .analysis_value_storage
                .set_result_value(provider_collection)?;
        }

        let finished_eval = reentrant_eval.finish_evaluation();

        let declared_actions = analysis_registry.num_declared_actions();
        let declared_artifacts = analysis_registry.num_declared_artifacts();
        let registry_finalizer = analysis_registry.finalize(&env)?;
        let (token, frozen_env, profile_data) = finished_eval.freeze_and_finish(env)?;
        let recorded_values = registry_finalizer(&frozen_env)?;

        let validations = transitive_validations(
            validations_from_deps,
            recorded_values.provider_collection()?,
        );

        Ok((
            token,
            (
                AnalysisResult::new(
                    recorded_values,
                    profile_data,
                    StdBuckHashMap::default(),
                    declared_actions,
                    declared_artifacts,
                    validations,
                ),
                split_instants,
            ),
        ))
    })
    .await
}

pub fn transitive_validations(
    deps: SmallMap<ConfiguredTargetLabel, TransitiveValidations>,
    provider_collection: FrozenProviderCollectionValueRef,
) -> Option<TransitiveValidations> {
    let provider_collection = provider_collection.to_owned();
    let info = provider_collection
        .value
        .maybe_map(|c| c.as_ref().builtin_provider_value::<FrozenValidationInfo>());
    if info.is_some() || deps.len() > 1 {
        Some(TransitiveValidations(Arc::new(TransitiveValidationsData {
            info,
            children: deps.into_keys().collect(),
        })))
    } else {
        assert!(
            deps.len() <= 1,
            "Reuse the single element if any from one of the deps for current node."
        );
        deps.into_values().next()
    }
}

fn get_rule_callable(
    eval: &mut Evaluator<'_, '_, '_>,
    module: &FrozenModule,
    name: &str,
) -> buck2_error::Result<FrozenValue> {
    let rule_callable = module
        .get_any_visibility(name)
        .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Tier0))
        .with_buck_error_context(|| format!("Couldn't find rule `{name}`"))?
        .0;
    let rule_callable = rule_callable.owned_value(eval.frozen_heap());
    let rule_callable = rule_callable
        .unpack_frozen()
        .ok_or_else(|| internal_error!("Must be frozen"))?;
    Ok(rule_callable)
}

pub fn get_rule_impl(
    eval: &mut Evaluator<'_, '_, '_>,
    module: &FrozenModule,
    name: &str,
) -> buck2_error::Result<FrozenValue> {
    let rule_callable = get_rule_callable(eval, module, name)?;
    let rule_impl = (FROZEN_RULE_GET_IMPL.get()?)(rule_callable)?;
    Ok(rule_impl)
}

pub fn promise_artifact_mappings<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    module: &FrozenModule,
    name: &str,
) -> buck2_error::Result<SmallMap<String, Value<'v>>> {
    let rule_callable = get_rule_callable(eval, module, name)?;
    let frozen_promise_artifact_mappings =
        (FROZEN_PROMISE_ARTIFACT_MAPPINGS_GET_IMPL.get()?)(rule_callable)?;

    Ok(frozen_promise_artifact_mappings
        .iter()
        .map(|(frozen_string, frozen_func)| (frozen_string.to_string(), frozen_func.to_value()))
        .collect::<SmallMap<_, _>>())
}

pub fn get_user_defined_rule_spec(
    module: FrozenModule,
    rule_type: &StarlarkRuleType,
) -> impl RuleSpec + use<> {
    struct Impl {
        module: FrozenModule,
        name: String,
    }

    impl RuleSpec for Impl {
        fn invoke<'v>(
            &self,
            eval: &mut Evaluator<'v, '_, '_>,
            ctx: ValueTyped<'v, AnalysisContext<'v>>,
        ) -> buck2_error::Result<Value<'v>> {
            let rule_impl = get_rule_impl(eval, &self.module, &self.name)?;
            Ok(eval.eval_function(rule_impl.to_value(), &[ctx.to_value()], &[])?)
        }

        fn promise_artifact_mappings<'v>(
            &self,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> buck2_error::Result<SmallMap<String, Value<'v>>> {
            promise_artifact_mappings(eval, &self.module, &self.name)
        }

        fn bazel_attr_aspects<'v>(
            &self,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> buck2_error::Result<SmallMap<String, Vec<Value<'v>>>> {
            let rule_callable = get_rule_callable(eval, &self.module, &self.name)?;
            let aspects = (FROZEN_BAZEL_ATTR_ASPECTS_GET_IMPL.get()?)(rule_callable)?;
            Ok(aspects
                .into_iter()
                .map(|(name, aspects)| {
                    (
                        name,
                        aspects
                            .into_iter()
                            .map(|aspect| aspect.to_value())
                            .collect(),
                    )
                })
                .collect())
        }
    }

    Impl {
        module,
        name: rule_type.name.clone(),
    }
}
