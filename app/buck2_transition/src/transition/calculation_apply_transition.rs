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
        if let Some(value) = conf.data()?.build_settings.get(key) {
            Ok(bazel_build_setting_value_to_starlark(value, eval))
        } else {
            Ok(eval.heap().alloc(conf.label()?).to_value())
        }
    } else {
        let canonical_key = transition.bazel_canonical_build_setting_key(key);
        let command_line_default = bazel_command_line_option_default(key);
        if let Some(value) = conf
            .data()?
            .build_settings
            .get(&canonical_key)
            .or_else(|| defaults.get(&canonical_key))
            .or(command_line_default.as_ref())
        {
            Ok(bazel_build_setting_value_to_starlark(value, eval))
        } else {
            Ok(Value::new_none())
        }
    }
}

fn bazel_command_line_option_default(key: &str) -> Option<BazelBuildSettingValue> {
    let option = key.strip_prefix("//command_line_option:")?;
    let value = match option {
        "cpu" | "host_cpu" => BazelBuildSettingValue::String(bazel_auto_cpu().to_owned()),
        "compilation_mode" => BazelBuildSettingValue::String("fastbuild".to_owned()),
        "host_compilation_mode" => BazelBuildSettingValue::String("opt".to_owned()),
        "java_language_version" | "tool_java_language_version" => {
            BazelBuildSettingValue::String(String::new())
        }
        "java_runtime_version" => BazelBuildSettingValue::String("local_jdk".to_owned()),
        "tool_java_runtime_version" => BazelBuildSettingValue::String("remotejdk_11".to_owned()),
        "ios_multi_cpus" | "macos_cpus" | "tvos_cpus" | "visionos_cpus" | "watchos_cpus" => {
            BazelBuildSettingValue::StringList(Vec::new())
        }
        "stamp" => BazelBuildSettingValue::Bool(false),
        _ => return None,
    };
    Some(value)
}

fn bazel_auto_cpu() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "x86_64") => "darwin_x86_64",
        ("macos", "aarch64") => "darwin_arm64",
        ("freebsd", _) => "freebsd",
        ("openbsd", _) => "openbsd",
        ("windows", "x86_64") => "x64_windows",
        ("windows", "aarch64") => "arm64_windows",
        ("linux", "x86" | "i386" | "i486" | "i586" | "i686" | "i786") => "piii",
        ("linux", "x86_64") => "k8",
        ("linux", "power" | "powerpc" | "powerpc64" | "powerpc64le") => "ppc",
        ("linux", "arm" | "armv7" | "armv7l") => "arm",
        ("linux", "aarch64") => "aarch64",
        ("linux", "s390x") => "s390x",
        ("linux", "mips64") => "mips64",
        ("linux", "riscv64") => "riscv64",
        _ => "unknown",
    }
}

fn bazel_build_setting_value_to_starlark<'v>(
    value: &BazelBuildSettingValue,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    match value {
        BazelBuildSettingValue::Bool(value) => eval.heap().alloc(*value).to_value(),
        BazelBuildSettingValue::Int(value) => eval.heap().alloc(*value).to_value(),
        BazelBuildSettingValue::Label(value) => eval
            .heap()
            .alloc(StarlarkProvidersLabel::new(value.clone())),
        BazelBuildSettingValue::LabelList(values) => eval.heap().alloc(
            values
                .iter()
                .map(|value| StarlarkProvidersLabel::new(value.clone()))
                .collect::<Vec<_>>(),
        ),
        BazelBuildSettingValue::String(value) => eval.heap().alloc(value.as_str()).to_value(),
        BazelBuildSettingValue::StringList(values) => {
            let values = values.iter().map(String::as_str).collect::<Vec<_>>();
            eval.heap().alloc(values).to_value()
        }
    }
}

fn bazel_build_setting_list_value(
    values: impl IntoIterator<Item = BazelBuildSettingValue>,
) -> Option<BazelBuildSettingValue> {
    let values = values.into_iter().collect::<Vec<_>>();
    if values
        .iter()
        .all(|value| matches!(value, BazelBuildSettingValue::Label(_)))
    {
        return Some(BazelBuildSettingValue::LabelList(
            values
                .into_iter()
                .map(|value| match value {
                    BazelBuildSettingValue::Label(label) => label,
                    _ => unreachable!("checked above"),
                })
                .collect(),
        ));
    }

    let strings = values
        .into_iter()
        .map(|value| match value {
            BazelBuildSettingValue::Bool(value) => Some(value.to_string()),
            BazelBuildSettingValue::Int(value) => Some(value.to_string()),
            BazelBuildSettingValue::Label(value) => Some(value.to_string()),
            BazelBuildSettingValue::String(value) => Some(value),
            BazelBuildSettingValue::StringList(_) | BazelBuildSettingValue::LabelList(_) => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(BazelBuildSettingValue::StringList(strings))
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
            Some(BazelBuildSettingValue::Label(value.dupe()))
        }
        CoercedAttr::List(values) => bazel_build_setting_list_value(
            values
                .iter()
                .map(bazel_build_setting_value_from_attr)
                .collect::<Option<Vec<_>>>()?,
        ),
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
        BazelBuildSettingValue::Label(label.label().dupe())
    } else if let Some(label) = StarlarkTargetLabel::from_value(value) {
        BazelBuildSettingValue::Label(ProvidersLabel::default_for(label.label().dupe()))
    } else if let Some(value) = value.unpack_str() {
        BazelBuildSettingValue::String(value.to_owned())
    } else if let Some(value) = value.unpack_bool() {
        BazelBuildSettingValue::Bool(value)
    } else if let Some(value) = value.unpack_i32() {
        BazelBuildSettingValue::Int(value.into())
    } else if let Some(values) = ListRef::from_value(value) {
        bazel_build_setting_list_value(
            values
                .iter()
                .map(|value| {
                    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
                        BazelBuildSettingValue::Label(label.label().dupe())
                    } else if let Some(label) = StarlarkTargetLabel::from_value(value) {
                        BazelBuildSettingValue::Label(ProvidersLabel::default_for(
                            label.label().dupe(),
                        ))
                    } else if let Some(value) = value.unpack_str() {
                        BazelBuildSettingValue::String(value.to_owned())
                    } else if let Some(value) = value.unpack_bool() {
                        BazelBuildSettingValue::Bool(value)
                    } else if let Some(value) = value.unpack_i32() {
                        BazelBuildSettingValue::Int(value.into())
                    } else {
                        BazelBuildSettingValue::String(value.to_repr())
                    }
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|| BazelBuildSettingValue::String(value.to_repr()))
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
    const PATCH_TRANSITION_KEY: &str = "";

    fn apply_patch(
        dict: DictRef,
        conf: &ConfigurationData,
        transition: &TransitionData,
    ) -> buck2_error::Result<ConfigurationData> {
        if dict.is_empty() {
            return Ok(conf.dupe());
        }

        let original_data = conf.data()?.clone();
        let mut data = conf.data()?.clone();
        for (key, value) in dict.iter() {
            let key = bazel_transition_setting_key(key.to_value(), transition)?;
            data.build_settings
                .insert(key, bazel_transition_setting_value(value.to_value()));
        }
        if data == original_data {
            return Ok(conf.dupe());
        }
        let label = bazel_transitioned_label(&data, conf.is_marked_as_exec_platform());
        Ok(ConfigurationData::from_platform(
            label,
            data,
            conf.is_marked_as_exec_platform(),
        )?)
    }

    fn split_from_patch(
        conf: &ConfigurationData,
        configuration: ConfigurationData,
    ) -> TransitionApplied {
        let mut split = OrderedMap::new();
        let previous = split.insert(PATCH_TRANSITION_KEY.to_owned(), configuration);
        assert!(previous.is_none());
        let _ = conf;
        TransitionApplied::Split(SortedMap::from(split))
    }

    if result.is_none() {
        if transition.is_split() {
            return Ok(split_from_patch(conf, conf.dupe()));
        }
        return Ok(TransitionApplied::Single(conf.dupe()));
    }

    if transition.is_split() {
        if let Some(list) = ListRef::from_value(result) {
            if list.is_empty() {
                return Ok(split_from_patch(conf, conf.dupe()));
            }
            let mut split = OrderedMap::new();
            for (index, value) in list.iter().enumerate() {
                let Some(dict) = DictRef::from_value(value) else {
                    return Err(ApplyTransitionError::BazelTransitionMustReturnDict(
                        value.get_type().to_owned(),
                    )
                    .into());
                };
                let previous =
                    split.insert(index.to_string(), apply_patch(dict, conf, transition)?);
                assert!(previous.is_none());
            }
            return Ok(TransitionApplied::Split(SortedMap::from(split)));
        }
    }

    let Some(dict) = DictRef::from_value(result) else {
        return Err(ApplyTransitionError::BazelTransitionMustReturnDict(
            result.get_type().to_owned(),
        )
        .into());
    };
    if dict.is_empty() {
        if transition.is_split() {
            return Ok(split_from_patch(conf, conf.dupe()));
        }
        return Ok(TransitionApplied::Single(conf.dupe()));
    }

    if transition.is_split() {
        let mut split = OrderedMap::new();
        let mut dict_of_dicts = true;
        for (key, value) in dict.iter() {
            let Some(split_key) = key.to_value().unpack_str() else {
                dict_of_dicts = false;
                break;
            };
            let Some(split_dict) = DictRef::from_value(value.to_value()) else {
                dict_of_dicts = false;
                break;
            };
            let previous = split.insert(
                split_key.to_owned(),
                apply_patch(split_dict, conf, transition)?,
            );
            assert!(previous.is_none());
        }
        if dict_of_dicts {
            return Ok(TransitionApplied::Split(SortedMap::from(split)));
        }
        return Ok(split_from_patch(conf, apply_patch(dict, conf, transition)?));
    }

    Ok(TransitionApplied::Single(apply_patch(
        dict, conf, transition,
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
            TransitionData::MagicObject(v) | TransitionData::BazelAttribute(v) => {
                v.implementation.to_value()
            }
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
        TransitionData::BazelAttribute(_) => {
            unreachable!("Bazel attribute transitions are handled by the Bazel branch")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bazel_command_line_option_defaults_match_core_options() {
        assert_eq!(
            bazel_command_line_option_default("//command_line_option:cpu"),
            Some(BazelBuildSettingValue::String(bazel_auto_cpu().to_owned()))
        );
        assert_eq!(
            bazel_command_line_option_default("//command_line_option:host_cpu"),
            Some(BazelBuildSettingValue::String(bazel_auto_cpu().to_owned()))
        );
        assert_eq!(
            bazel_command_line_option_default("//command_line_option:compilation_mode"),
            Some(BazelBuildSettingValue::String("fastbuild".to_owned()))
        );
        assert_eq!(
            bazel_command_line_option_default("//command_line_option:java_runtime_version"),
            Some(BazelBuildSettingValue::String("local_jdk".to_owned()))
        );
        assert_eq!(
            bazel_command_line_option_default("//command_line_option:tool_java_runtime_version"),
            Some(BazelBuildSettingValue::String("remotejdk_11".to_owned()))
        );
        assert_eq!(
            bazel_command_line_option_default("//command_line_option:java_language_version"),
            Some(BazelBuildSettingValue::String(String::new()))
        );
        assert_eq!(
            bazel_command_line_option_default("//command_line_option:macos_cpus"),
            Some(BazelBuildSettingValue::StringList(Vec::new()))
        );
        assert_eq!(
            bazel_command_line_option_default("//command_line_option:unknown"),
            None
        );
    }
}
