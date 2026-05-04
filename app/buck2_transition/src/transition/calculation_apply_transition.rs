/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_build_api::actions::query::CONFIGURED_ATTR_TO_VALUE;
use buck2_build_api::actions::query::PackageLabelOption;
use buck2_build_api::analysis::calculation::RuleAnalysisCalculation;
use buck2_build_api::interpreter::rule_defs::provider::builtin::platform_info::PlatformInfo;
use buck2_build_api::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValue;
use buck2_build_api::transition::TRANSITION_CALCULATION;
use buck2_build_api::transition::TransitionAttrs;
use buck2_build_api::transition::TransitionCalculation;
use buck2_common::dice::cells::HasCellResolver;
use buck2_core::configuration::cfg_diff::cfg_diff;
use buck2_core::configuration::data::BazelBuildSettingValue;
use buck2_core::configuration::data::ConfigurationData;
use buck2_core::configuration::data::ConfigurationDataData;
use buck2_core::configuration::transition::applied::TransitionApplied;
use buck2_core::configuration::transition::id::TransitionId;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::target::label::label::TargetLabel;
use buck2_error::BuckErrorContext;
use buck2_events::dispatch::get_dispatcher;
use buck2_hash::BuckHasher;
use buck2_interpreter::dice::starlark_provider::StarlarkEvalKind;
use buck2_interpreter::factory::BuckStarlarkModule;
use buck2_interpreter::factory::StarlarkEvaluatorProvider;
use buck2_interpreter::print_handler::EventDispatcherPrintHandler;
use buck2_interpreter::soft_error::Buck2StarlarkSoftErrorHandler;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use buck2_interpreter::types::target_label::StarlarkTargetLabel;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::configured_attr::ConfiguredAttr;
use buck2_node::attrs::display::AttrDisplayWithContextExt;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::nodes::frontend::TargetGraphCalculation;
use derive_more::Display;
use dice::DiceComputations;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use dupe::OptionDupedExt;
use itertools::Itertools;
use pagable::Pagable;
use pagable::pagable_typetag;
use starlark::eval::Evaluator;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::ListRef;
use starlark::values::structs::AllocStruct;
use starlark_map::ordered_map::OrderedMap;
use starlark_map::sorted_map::SortedMap;

use crate::transition::calculation_fetch_transition::FetchTransition;
use crate::transition::calculation_fetch_transition::TransitionData;

#[derive(buck2_error::Error, Debug)]
#[buck2(tag = Tier0)]
enum ApplyTransitionError {
    #[error("transition function not marked as `split` must return a `PlatformInfo`")]
    NonSplitTransitionMustReturnPlatformInfo,
    #[error("transition function marked `split` must return a dict of `str` to `PlatformInfo`")]
    SplitTransitionMustReturnDict,
    #[error(
        "transition applied again to transition output \
        did not produce identical `PlatformInfo`, the diff:\n{0}"
    )]
    SplitTransitionAgainDifferentPlatformInfo(String),
    #[error("Bazel transition function returned `{0}`, expected a dict of build settings")]
    BazelTransitionMustReturnDict(String),
    #[error("unsupported default value for Bazel build setting `{0}`: `{1}`")]
    UnsupportedBazelBuildSettingDefault(String, String),
}

fn bazel_transition_input_value<'v>(
    key: &str,
    transition: &TransitionData,
    defaults: &BTreeMap<String, BazelBuildSettingValue>,
    conf: &ConfigurationData,
    eval: &mut Evaluator<'v, '_, '_>,
) -> buck2_error::Result<Value<'v>> {
    if key == "//command_line_option:platforms" {
        let value = match conf.data()?.build_settings.get(key) {
            Some(BazelBuildSettingValue::String(value)) => value.as_str(),
            _ => conf.label()?,
        };
        Ok(eval.heap().alloc(value).to_value())
    } else if let Some(value) = conf
        .data()?
        .build_settings
        .get(&transition.bazel_canonical_build_setting_key(key))
        .or_else(|| defaults.get(&transition.bazel_canonical_build_setting_key(key)))
    {
        Ok(bazel_build_setting_value_to_starlark(value, eval))
    } else {
        Ok(Value::new_none())
    }
}

fn bazel_build_setting_value_to_starlark<'v>(
    value: &BazelBuildSettingValue,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    match value {
        BazelBuildSettingValue::Bool(value) => eval.heap().alloc(*value).to_value(),
        BazelBuildSettingValue::Int(value) => eval.heap().alloc(*value).to_value(),
        BazelBuildSettingValue::String(value) => eval.heap().alloc(value.as_str()).to_value(),
        BazelBuildSettingValue::StringList(values) => {
            let values = values.iter().map(String::as_str).collect::<Vec<_>>();
            eval.heap().alloc(values).to_value()
        }
    }
}

fn bazel_build_setting_value_from_attr(value: &CoercedAttr) -> Option<BazelBuildSettingValue> {
    match value {
        CoercedAttr::OneOf(value, _) => bazel_build_setting_value_from_attr(value),
        CoercedAttr::Bool(value) => Some(BazelBuildSettingValue::Bool(value.0)),
        CoercedAttr::Int(value) => Some(BazelBuildSettingValue::Int(*value)),
        CoercedAttr::String(value) | CoercedAttr::EnumVariant(value) => {
            Some(BazelBuildSettingValue::String(value.0.to_string()))
        }
        CoercedAttr::Label(value) | CoercedAttr::Dep(value) | CoercedAttr::SourceLabel(value) => {
            Some(BazelBuildSettingValue::String(value.to_string()))
        }
        CoercedAttr::List(values) => Some(BazelBuildSettingValue::StringList(
            values
                .iter()
                .map(bazel_build_setting_value_from_attr)
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .map(|value| value.as_config_setting_value())
                .collect(),
        )),
        CoercedAttr::None => None,
        _ => None,
    }
}

async fn bazel_transition_input_defaults(
    ctx: &mut DiceComputations<'_>,
    transition: &TransitionData,
) -> buck2_error::Result<BTreeMap<String, BazelBuildSettingValue>> {
    let cell_resolver = ctx.get_cell_resolver().await?;
    let cell_alias_resolver = ctx
        .get_cell_alias_resolver(cell_resolver.root_cell())
        .await?;
    let mut defaults = BTreeMap::new();
    for input in transition.bazel_inputs() {
        let key = transition.bazel_canonical_build_setting_key(input.as_str());
        if key.starts_with("//command_line_option:") {
            continue;
        }
        let target = TargetLabel::parse(
            &key,
            cell_resolver.root_cell(),
            &cell_resolver,
            &cell_alias_resolver,
        )?;
        let node = ctx.get_target_node(&target).await?;
        let default_attr = node
            .attr_or_none("build_setting_default", AttrInspectOptions::All)
            .or_else(|| node.attr_or_none("actual", AttrInspectOptions::All));
        let Some(default_attr) = default_attr else {
            continue;
        };
        let Some(default) = bazel_build_setting_value_from_attr(default_attr.value) else {
            return Err(ApplyTransitionError::UnsupportedBazelBuildSettingDefault(
                key,
                default_attr.value.as_display_no_ctx().to_string(),
            )
            .into());
        };
        defaults.insert(key, default);
    }
    Ok(defaults)
}

fn bazel_transition_setting_key(
    key: Value,
    transition: &TransitionData,
) -> buck2_error::Result<String> {
    if let Some(key) = key.unpack_str() {
        return Ok(transition.bazel_canonical_build_setting_key(key));
    }
    if let Some(label) = StarlarkProvidersLabel::from_value(key) {
        return Ok(label.label().to_string());
    }
    if let Some(label) = StarlarkTargetLabel::from_value(key) {
        return Ok(label.label().to_string());
    }
    Err(ApplyTransitionError::BazelTransitionMustReturnDict(key.get_type().to_owned()).into())
}

fn bazel_transition_setting_value(value: Value) -> BazelBuildSettingValue {
    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
        BazelBuildSettingValue::String(label.label().to_string())
    } else if let Some(label) = StarlarkTargetLabel::from_value(value) {
        BazelBuildSettingValue::String(label.label().to_string())
    } else if let Some(value) = value.unpack_str() {
        BazelBuildSettingValue::String(value.to_owned())
    } else if let Some(value) = value.unpack_bool() {
        BazelBuildSettingValue::Bool(value)
    } else if let Some(value) = value.unpack_i32() {
        BazelBuildSettingValue::Int(value.into())
    } else if let Some(values) = ListRef::from_value(value) {
        BazelBuildSettingValue::StringList(
            values
                .iter()
                .map(|value| {
                    if let Some(value) = value.unpack_str() {
                        value.to_owned()
                    } else if let Some(label) = StarlarkProvidersLabel::from_value(value) {
                        label.label().to_string()
                    } else if let Some(label) = StarlarkTargetLabel::from_value(value) {
                        label.label().to_string()
                    } else {
                        value.to_repr()
                    }
                })
                .collect(),
        )
    } else {
        BazelBuildSettingValue::String(value.to_repr())
    }
}

fn bazel_transitioned_label(
    data: &ConfigurationDataData,
    is_marked_as_exec_platform: bool,
) -> String {
    let mut hasher = BuckHasher::default();
    "bazel_transition".hash(&mut hasher);
    data.hash(&mut hasher);
    is_marked_as_exec_platform.hash(&mut hasher);
    format!("bazeltr-{:016x}", hasher.finish())
}

fn bazel_transition_result_to_configuration(
    result: Value,
    conf: &ConfigurationData,
    transition: &TransitionData,
) -> buck2_error::Result<TransitionApplied> {
    if result.is_none() {
        return Ok(TransitionApplied::Single(conf.dupe()));
    }

    let Some(dict) = DictRef::from_value(result) else {
        return Err(ApplyTransitionError::BazelTransitionMustReturnDict(
            result.get_type().to_owned(),
        )
        .into());
    };
    if dict.is_empty() {
        return Ok(TransitionApplied::Single(conf.dupe()));
    }

    let original_data = conf.data()?.clone();
    let mut data = conf.data()?.clone();
    for (key, value) in dict.iter() {
        let key = bazel_transition_setting_key(key.to_value(), transition)?;
        data.build_settings
            .insert(key, bazel_transition_setting_value(value.to_value()));
    }
    if data == original_data {
        return Ok(TransitionApplied::Single(conf.dupe()));
    }
    let label = bazel_transitioned_label(&data, conf.is_marked_as_exec_platform());
    Ok(TransitionApplied::Single(ConfigurationData::from_platform(
        label,
        data,
        conf.is_marked_as_exec_platform(),
    )?))
}

fn bazel_analysis_test_transition_to_configuration(
    settings: &BTreeMap<String, BazelBuildSettingValue>,
    conf: &ConfigurationData,
) -> buck2_error::Result<TransitionApplied> {
    if settings.is_empty() {
        return Ok(TransitionApplied::Single(conf.dupe()));
    }

    let original_data = conf.data()?.clone();
    let mut data = conf.data()?.clone();
    for (key, value) in settings {
        data.build_settings.insert(key.clone(), value.clone());
    }
    if data == original_data {
        return Ok(TransitionApplied::Single(conf.dupe()));
    }
    let label = bazel_transitioned_label(&data, conf.is_marked_as_exec_platform());
    Ok(TransitionApplied::Single(ConfigurationData::from_platform(
        label,
        data,
        conf.is_marked_as_exec_platform(),
    )?))
}

fn call_transition_function<'v>(
    transition: &TransitionData,
    defaults: &BTreeMap<String, BazelBuildSettingValue>,
    conf: &ConfigurationData,
    refs: Value<'v>,
    attrs: Option<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> buck2_error::Result<TransitionApplied> {
    if transition.is_bazel() {
        let mut settings = Vec::new();
        for input in transition.bazel_inputs() {
            settings.push((
                input.as_str(),
                bazel_transition_input_value(input.as_str(), transition, defaults, conf, eval)?,
            ));
        }
        let settings = eval.heap().alloc(AllocDict(settings));
        let attrs =
            attrs.unwrap_or_else(|| eval.heap().alloc(AllocStruct(Vec::<(&str, Value)>::new())));
        let impl_ = match transition {
            TransitionData::MagicObject(v) => v.implementation.to_value(),
            TransitionData::AnalysisTest(_) => {
                unreachable!("analysis test transitions are applied without Starlark evaluation")
            }
            TransitionData::Target(_) => {
                unreachable!("target transitions are not Bazel transitions")
            }
        };
        let result = eval
            .eval_function(impl_, &[settings, attrs], &[])
            .map_err(buck2_error::Error::from)?;
        return bazel_transition_result_to_configuration(result, conf, transition);
    }

    let mut args = vec![(
        "platform",
        eval.heap()
            .alloc_complex(PlatformInfo::from_configuration(conf, eval.heap())?),
    )];
    let impl_ = match transition {
        TransitionData::MagicObject(v) => {
            args.push(("refs", refs));
            v.implementation.to_value()
        }
        TransitionData::AnalysisTest(_) => {
            unreachable!("analysis test transitions are applied without Starlark evaluation")
        }
        TransitionData::Target(v) => v.r#impl.to_value().get(),
    };
    if let Some(attrs) = attrs {
        args.push(("attrs", attrs));
    }
    let new_platforms = eval
        .eval_function(impl_, &[], &args)
        .map_err(buck2_error::Error::from)?;
    let is_marked_as_exec_platform = conf.is_marked_as_exec_platform();
    if transition.is_split() {
        match UnpackDictEntries::<&str, &PlatformInfo>::unpack_value(new_platforms)? {
            Some(dict) => {
                let mut split = OrderedMap::new();
                for (k, v) in dict.entries {
                    let prev = split.insert(
                        k.to_owned(),
                        v.to_configuration(is_marked_as_exec_platform)?,
                    );
                    assert!(prev.is_none());
                }
                Ok(TransitionApplied::Split(SortedMap::from(split)))
            }
            None => Err(ApplyTransitionError::SplitTransitionMustReturnDict.into()),
        }
    } else {
        match <&PlatformInfo>::unpack_value_err(new_platforms) {
            Ok(platform) => Ok(TransitionApplied::Single(
                platform.to_configuration(is_marked_as_exec_platform)?,
            )),
            Err(_) => Err(ApplyTransitionError::NonSplitTransitionMustReturnPlatformInfo.into()),
        }
    }
}

async fn do_apply_transition(
    ctx: &mut DiceComputations<'_>,
    attrs: Option<&[(String, Option<Arc<ConfiguredAttr>>)]>,
    conf: &ConfigurationData,
    transition_id: &TransitionId,
    cancellation: &CancellationContext,
) -> buck2_error::Result<TransitionApplied> {
    let transition = ctx.fetch_transition(transition_id).await?;
    if let Some(settings) = transition.bazel_analysis_test_settings() {
        return bazel_analysis_test_transition_to_configuration(settings, conf);
    }
    let bazel_defaults = if transition.is_bazel() {
        bazel_transition_input_defaults(ctx, &transition).await?
    } else {
        BTreeMap::new()
    };
    let mut refs = Vec::new();
    let mut refs_refs = Vec::new();
    for (s, t) in transition.refs() {
        let provider_collection_value = ctx.fetch_transition_function_reference(t).await?;
        refs.push((
            *s,
            // This is safe because we store a reference to provider collection in `refs_refs`.
            unsafe { provider_collection_value.value().to_frozen_value() },
        ));
        refs_refs.push(provider_collection_value);
    }
    let print = EventDispatcherPrintHandler(get_dispatcher());
    let eval_kind = StarlarkEvalKind::Transition(Arc::new(transition_id.clone()));
    let provider = StarlarkEvaluatorProvider::new(ctx, eval_kind).await?;
    BuckStarlarkModule::with_profiling(|module| {
        let (finished_eval, res) =
            provider.with_evaluator(&module, cancellation.into(), |eval, _| {
                eval.set_print_handler(&print);
                eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);
                let refs = module.heap().alloc(AllocStruct(refs));
                let attrs = match attrs {
                    Some(values) => {
                        let mut attrs = Vec::new();
                        for (name, value) in values {
                            let value = match value {
                                Some(value) => (CONFIGURED_ATTR_TO_VALUE.get()?)(
                                    value,
                                    PackageLabelOption::TransitionAttr,
                                    module.heap(),
                                )
                                .with_buck_error_context(|| {
                                    format!(
                                        "Error converting attribute `{}={}` to Starlark value",
                                        name,
                                        value.as_display_no_ctx(),
                                    )
                                })?,
                                None => Value::new_none(),
                            };
                            attrs.push((name.as_str(), value));
                        }
                        Some(module.heap().alloc(AllocStruct(attrs)))
                    }
                    None => None,
                };
                let applied = call_transition_function(
                    &transition,
                    &bazel_defaults,
                    conf,
                    refs,
                    attrs,
                    eval,
                )?;
                if transition.is_bazel() {
                    return Ok(applied);
                }
                match applied {
                    TransitionApplied::Single(new) => {
                        let new_2 = match call_transition_function(
                            &transition,
                            &bazel_defaults,
                            &new,
                            refs,
                            attrs,
                            eval,
                        )
                        .buck_error_context("applying transition again on transition output")?
                        {
                            TransitionApplied::Single(new_2) => new_2,
                            TransitionApplied::Split(_) => {
                                unreachable!(
                                    "split transition filtered out in call_transition_function"
                                )
                            }
                        };
                        if let Err(diff) = cfg_diff(&new, &new_2) {
                            return Err(
                                ApplyTransitionError::SplitTransitionAgainDifferentPlatformInfo(
                                    diff,
                                )
                                .into(),
                            );
                        }
                        Ok(TransitionApplied::Single(new))
                    }
                    TransitionApplied::Split(split) => {
                        // Not validating split transitions yet, because it's not 100% clear what to validate,
                        // and because it is not that important, because split transitions
                        // are not used in per-rule transitions.
                        Ok(TransitionApplied::Split(split))
                    }
                }
            })?;
        let (token, _) = finished_eval.finish()?;
        Ok((token, res))
    })
}

#[async_trait]
pub(crate) trait ApplyTransition {
    /// Resolve `refs` param of transition function.
    async fn fetch_transition_function_reference(
        &mut self,
        target: &ProvidersLabel,
    ) -> buck2_error::Result<FrozenProviderCollectionValue>;
}

#[async_trait]
impl ApplyTransition for DiceComputations<'_> {
    async fn fetch_transition_function_reference(
        &mut self,
        target: &ProvidersLabel,
    ) -> buck2_error::Result<FrozenProviderCollectionValue> {
        Ok(self.get_configuration_analysis_result(target).await?.dupe())
    }
}

struct TransitionCalculationImpl;

pub(crate) fn init_transition_calculation() {
    TRANSITION_CALCULATION.init(&TransitionCalculationImpl);
}

#[async_trait]
impl TransitionCalculation for TransitionCalculationImpl {
    async fn apply_transition(
        &self,
        ctx: &mut DiceComputations<'_>,
        configured_attrs: &OrderedMap<&str, Arc<ConfiguredAttr>>,
        cfg: &ConfigurationData,
        transition_id: &TransitionId,
    ) -> buck2_error::Result<Arc<TransitionApplied>> {
        #[derive(Debug, Eq, PartialEq, Hash, Clone, Display, Allocative, Pagable)]
        #[display("{} ({}){}", transition_id, cfg, self.fmt_attrs())]
        #[pagable_typetag(dice::DiceKeyDyn)]
        struct TransitionKey {
            cfg: ConfigurationData,
            transition_id: TransitionId,
            /// Attributes requested by the transition function.
            /// Attributes are added here so multiple targets with the equal attributes
            /// (e.g. the same `java_version = 14`) share the transition computation.
            attrs: Option<Vec<(String, Option<Arc<ConfiguredAttr>>)>>,
        }

        impl TransitionKey {
            fn fmt_attrs(&self) -> String {
                if let Some(attrs) = &self.attrs {
                    format!(
                        " [{}]",
                        attrs
                            .iter()
                            .map(|(name, a)| {
                                if let Some(attr) = a {
                                    format!("{name}={}", attr.as_display_no_ctx())
                                } else {
                                    format!("{name}=None")
                                }
                            })
                            .join(", ")
                    )
                } else {
                    String::new()
                }
            }
        }

        #[async_trait]
        impl Key for TransitionKey {
            type Value = buck2_error::Result<Arc<TransitionApplied>>;

            async fn compute(
                &self,
                ctx: &mut DiceComputations,
                cancellation: &CancellationContext,
            ) -> Self::Value {
                let v: buck2_error::Result<_> = try {
                    do_apply_transition(
                        ctx,
                        self.attrs.as_deref(),
                        &self.cfg,
                        &self.transition_id,
                        cancellation,
                    )
                    .await?
                };

                Ok(Arc::new(v.with_buck_error_context(|| {
                    format!("Error computing transition `{__self}`")
                })?))
            }

            fn equality(x: &Self::Value, y: &Self::Value) -> bool {
                if let (Ok(x), Ok(y)) = (x, y) {
                    x == y
                } else {
                    false
                }
            }

            fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
                OkPagableValueSerialize::<Self::Value>::new()
            }
        }

        let transition = ctx.fetch_transition(transition_id).await?;

        let attrs = match transition.attrs() {
            TransitionAttrs::None => None,
            TransitionAttrs::Listed(attrs) => Some(
                attrs
                    .iter()
                    .map(|attr| (attr.clone(), configured_attrs.get(attr.as_str()).duped()))
                    .collect(),
            ),
            TransitionAttrs::All | TransitionAttrs::BazelAll => Some(
                configured_attrs
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), Some(value.dupe())))
                    .collect(),
            ),
        };

        let key = TransitionKey {
            cfg: cfg.dupe(),
            transition_id: transition_id.clone(),
            attrs,
        };

        ctx.compute(&key).await?
    }
}
