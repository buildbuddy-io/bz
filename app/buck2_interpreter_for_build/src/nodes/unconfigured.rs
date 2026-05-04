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

use buck2_core::plugins::PluginKindSet;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::target::label::label::TargetLabel;
use buck2_core::target::name::TargetNameRef;
use buck2_node::attrs::attr::Attribute;
use buck2_node::attrs::attr_type::AttrType;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::coerced_deps_collector::CoercedDeps;
use buck2_node::attrs::coerced_deps_collector::CoercedDepsCollector;
use buck2_node::attrs::display::AttrDisplayWithContextExt;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::attrs::spec::AttributeId;
use buck2_node::attrs::spec::AttributeSpec;
use buck2_node::call_stack::StarlarkCallStack;
use buck2_node::nodes::unconfigured::TargetNode;
use buck2_node::package::Package;
use buck2_node::provider_id_set::ProviderIdSet;
use buck2_node::rule::BAZEL_OUTPUT_FILE_GENERATING_RULE_ATTR;
use buck2_node::rule::BAZEL_OUTPUT_FILE_OUTPUT_ATTR;
use buck2_node::rule::BazelOutputAttrKind;
use buck2_node::rule::Rule;
use buck2_node::rule::RuleIncomingTransition;
use buck2_node::rule_type::RuleType;
use buck2_util::arc_str::ArcStr;
use dupe::Dupe;
use dupe::OptionDupedExt;
use starlark::eval::CallStack;
use starlark::eval::ParametersParser;
use starlark::values::StringValue;
use starlark::values::Value;
use starlark_map::small_map::SmallMap;

use crate::call_stack::StarlarkCallStackWrapper;
use crate::interpreter::module_internals::ModuleInternals;
use crate::nodes::attr_spec::AttributeSpecExt;

pub trait TargetNodeExt: Sized {
    fn from_params_ignore_attrs_for_profiling<'v>(
        rule: Arc<Rule>,
        package: Arc<Package>,
        internals: &ModuleInternals,
        param_parser: &mut ParametersParser<'v, '_>,
    ) -> buck2_error::Result<Self>;

    fn from_params<'v>(
        rule: Arc<Rule>,
        package: Arc<Package>,
        internals: &ModuleInternals,
        param_parser: &mut ParametersParser<'v, '_>,
        arg_count: usize,
        ignore_attrs_for_profiling: bool,
        call_stack: Option<CallStack>,
    ) -> buck2_error::Result<Self>;

    fn from_named_values<'v>(
        rule: Arc<Rule>,
        package: Arc<Package>,
        internals: &ModuleInternals,
        named: &SmallMap<StringValue<'v>, Value<'v>>,
        ignore_attrs_for_profiling: bool,
        call_stack: Option<CallStack>,
    ) -> buck2_error::Result<Self>;
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelOutputFileTargetError {
    #[error("Bazel output attr `{0}` produced unsupported value `{1}`")]
    UnsupportedOutputAttrValue(String, String),
    #[error("Bazel implicit output template `{0}` references unsupported placeholder `{1}`")]
    UnsupportedImplicitOutputPlaceholder(String, String),
}

fn bazel_output_file_rule() -> buck2_error::Result<Arc<Rule>> {
    let generating_rule = Attribute::new(
        None,
        "",
        AttrType::dep(ProviderIdSet::EMPTY, PluginKindSet::EMPTY),
    )?;
    let output = Attribute::new(None, "", AttrType::string())?;
    Ok(Arc::new(Rule {
        attributes: AttributeSpec::from(
            vec![
                (
                    BAZEL_OUTPUT_FILE_GENERATING_RULE_ATTR.to_owned(),
                    generating_rule,
                ),
                (BAZEL_OUTPUT_FILE_OUTPUT_ATTR.to_owned(), output),
            ],
            false,
            &RuleIncomingTransition::None,
            false,
        )?,
        rule_type: RuleType::BazelOutputFile,
        rule_kind: buck2_node::nodes::unconfigured::RuleKind::Normal,
        cfg: RuleIncomingTransition::None,
        uses_plugins: Vec::new(),
        bazel_toolchains: Vec::new(),
        bazel_output_attrs: Vec::new(),
        bazel_implicit_outputs: Vec::new(),
        is_bazel_rule: false,
        is_bazel_build_setting: false,
    }))
}

fn attr_as_output_names(attr_name: &str, attr: &CoercedAttr) -> buck2_error::Result<Vec<String>> {
    match attr {
        CoercedAttr::String(value) => Ok(vec![value.0.to_string()]),
        CoercedAttr::List(values) => values
            .iter()
            .map(|value| match value {
                CoercedAttr::String(value) => Ok(value.0.to_string()),
                other => Err(BazelOutputFileTargetError::UnsupportedOutputAttrValue(
                    attr_name.to_owned(),
                    other.as_display_no_ctx().to_string(),
                )
                .into()),
            })
            .collect(),
        CoercedAttr::None => Ok(Vec::new()),
        CoercedAttr::OneOf(value, _) => attr_as_output_names(attr_name, value),
        other => Err(BazelOutputFileTargetError::UnsupportedOutputAttrValue(
            attr_name.to_owned(),
            other.as_display_no_ctx().to_string(),
        )
        .into()),
    }
}

fn implicit_output_attr_value(
    target_node: &TargetNode,
    attr_name: &str,
) -> buck2_error::Result<String> {
    if attr_name == "name" {
        return Ok(target_node.label().name().as_str().to_owned());
    }
    match target_node.attr_or_none(attr_name, AttrInspectOptions::All) {
        Some(attr) => match attr.value {
            CoercedAttr::String(value) => Ok(value.0.to_string()),
            CoercedAttr::OneOf(value, _) => {
                implicit_output_attr_value_from_coerced(attr_name, value)
            }
            other => Err(
                BazelOutputFileTargetError::UnsupportedImplicitOutputPlaceholder(
                    target_node.rule_type().name().to_owned(),
                    format!("{attr_name}={}", other.as_display_no_ctx()),
                )
                .into(),
            ),
        },
        None => Err(
            BazelOutputFileTargetError::UnsupportedImplicitOutputPlaceholder(
                target_node.rule_type().name().to_owned(),
                attr_name.to_owned(),
            )
            .into(),
        ),
    }
}

fn implicit_output_attr_value_from_coerced(
    attr_name: &str,
    value: &CoercedAttr,
) -> buck2_error::Result<String> {
    match value {
        CoercedAttr::String(value) => Ok(value.0.to_string()),
        CoercedAttr::OneOf(value, _) => implicit_output_attr_value_from_coerced(attr_name, value),
        other => Err(
            BazelOutputFileTargetError::UnsupportedImplicitOutputPlaceholder(
                attr_name.to_owned(),
                other.as_display_no_ctx().to_string(),
            )
            .into(),
        ),
    }
}

fn expand_bazel_implicit_output_template(
    target_node: &TargetNode,
    template: &str,
) -> buck2_error::Result<String> {
    let mut output = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("%{") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            return Err(
                BazelOutputFileTargetError::UnsupportedImplicitOutputPlaceholder(
                    template.to_owned(),
                    after_start.to_owned(),
                )
                .into(),
            );
        };
        let attr_name = &after_start[..end];
        output.push_str(&implicit_output_attr_value(target_node, attr_name)?);
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn attr_id(spec: &AttributeSpec, name: &str) -> buck2_error::Result<AttributeId> {
    spec.attr_specs()
        .find_map(|(attr_name, id, _)| (attr_name == name).then_some(id))
        .ok_or_else(|| buck2_error::internal_error!("missing attr `{name}` in output file rule"))
}

fn new_bazel_output_file_target(
    rule: Arc<Rule>,
    package: Arc<Package>,
    generating_label: &TargetLabel,
    output_name: &str,
    internals: &ModuleInternals,
) -> buck2_error::Result<TargetNode> {
    let name_id = attr_id(&rule.attributes, "name")?;
    let generating_rule_id = attr_id(&rule.attributes, BAZEL_OUTPUT_FILE_GENERATING_RULE_ATTR)?;
    let output_id = attr_id(&rule.attributes, BAZEL_OUTPUT_FILE_OUTPUT_ATTR)?;

    let mut attr_values = buck2_node::attrs::values::AttrValues::with_capacity(3);
    attr_values.push_sorted(
        name_id,
        CoercedAttr::String(buck2_node::attrs::attr_type::string::StringLiteral(
            ArcStr::from(output_name),
        )),
    );
    attr_values.push_sorted(
        generating_rule_id,
        CoercedAttr::Dep(ProvidersLabel::default_for(generating_label.dupe())),
    );
    attr_values.push_sorted(
        output_id,
        CoercedAttr::String(buck2_node::attrs::attr_type::string::StringLiteral(
            ArcStr::from(output_name),
        )),
    );
    attr_values.shrink_to_fit();

    let label = TargetLabel::new(
        generating_label.pkg().dupe(),
        TargetNameRef::new(output_name)?,
    );
    let mut deps_cache = CoercedDepsCollector::new();
    for a in rule.attributes.attrs(&attr_values, AttrInspectOptions::All) {
        a.traverse(label.pkg(), &mut deps_cache)?;
    }

    let super_package = internals.super_package();
    let package_cfg_modifiers = super_package.cfg_modifiers().duped();
    let test_config_unification_rollout = super_package.test_config_unification_rollout();
    drop(super_package);

    Ok(TargetNode::new(
        rule,
        package,
        label,
        attr_values,
        CoercedDeps::from(deps_cache),
        None,
        package_cfg_modifiers,
        test_config_unification_rollout,
    ))
}

pub(crate) fn bazel_output_file_targets(
    target_node: &TargetNode,
    internals: &ModuleInternals,
) -> buck2_error::Result<Vec<TargetNode>> {
    if target_node.rule.bazel_output_attrs.is_empty()
        && target_node.rule.bazel_implicit_outputs.is_empty()
    {
        return Ok(Vec::new());
    }

    let mut output_names = Vec::new();
    for output_attr in &target_node.rule.bazel_output_attrs {
        let Some(value) = target_node.attr_or_none(&output_attr.name, AttrInspectOptions::All)
        else {
            continue;
        };
        let names = attr_as_output_names(&output_attr.name, value.value)?;
        match output_attr.kind {
            BazelOutputAttrKind::Output | BazelOutputAttrKind::OutputList => {
                output_names.extend(names);
            }
        }
    }
    for output in &target_node.rule.bazel_implicit_outputs {
        let _key = &output.name;
        output_names.push(expand_bazel_implicit_output_template(
            target_node,
            &output.template,
        )?);
    }

    let rule = bazel_output_file_rule()?;
    let package = internals.package();
    output_names
        .into_iter()
        .map(|output_name| {
            new_bazel_output_file_target(
                rule.dupe(),
                package.dupe(),
                target_node.label(),
                &output_name,
                internals,
            )
        })
        .collect()
}

impl TargetNodeExt for TargetNode {
    /// Extract only the name attribute from rule arguments, ignore the others.
    fn from_params_ignore_attrs_for_profiling<'v>(
        rule: Arc<Rule>,
        package: Arc<Package>,
        internals: &ModuleInternals,
        param_parser: &mut ParametersParser<'v, '_>,
    ) -> buck2_error::Result<Self> {
        let (name, indices, attr_values) = rule.attributes.start_parse(param_parser, 1)?;

        for (_, _, _) in indices {
            // Consume all the arguments.
            // We call `next_opt` even for non-optional parameters. starlark-rust doesn't check.
            param_parser.next_opt::<Value>()?;
        }

        let super_package = internals.super_package();
        let package_cfg_modifiers = super_package.cfg_modifiers().duped();
        let test_config_unification_rollout = super_package.test_config_unification_rollout();
        drop(super_package);
        let label = TargetLabel::new(internals.buildfile_path().package().dupe(), name);
        Ok(TargetNode::new(
            rule.dupe(),
            package,
            label,
            attr_values,
            CoercedDeps::default(),
            None,
            package_cfg_modifiers,
            test_config_unification_rollout,
        ))
    }

    /// The body of the callable returned by `rule()`. Records the target in this package's `TargetMap`
    #[allow(clippy::box_collection)] // Parameter `call_stack`, because this is the field type.
    fn from_params<'v>(
        rule: Arc<Rule>,
        package: Arc<Package>,
        internals: &ModuleInternals,
        param_parser: &mut ParametersParser<'v, '_>,
        arg_count: usize,
        ignore_attrs_for_profiling: bool,
        call_stack: Option<CallStack>,
    ) -> buck2_error::Result<Self> {
        if ignore_attrs_for_profiling {
            return Self::from_params_ignore_attrs_for_profiling(
                rule,
                package,
                internals,
                param_parser,
            );
        }

        let (target_name, attr_values) =
            rule.attributes
                .parse_params(param_parser, arg_count, internals)?;
        let package_name = internals.buildfile_path().package();

        let label = TargetLabel::new(package_name.dupe(), target_name);
        let mut deps_cache = CoercedDepsCollector::new();

        for a in rule.attributes.attrs(&attr_values, AttrInspectOptions::All) {
            a.traverse(label.pkg(), &mut deps_cache)?;
        }

        let super_package = internals.super_package();
        let package_cfg_modifiers = super_package.cfg_modifiers().duped();
        let test_config_unification_rollout = super_package.test_config_unification_rollout();
        drop(super_package);

        Ok(TargetNode::new(
            rule,
            package,
            label,
            attr_values,
            CoercedDeps::from(deps_cache),
            call_stack
                .map(StarlarkCallStackWrapper)
                .map(StarlarkCallStack::new),
            package_cfg_modifiers,
            test_config_unification_rollout,
        ))
    }

    fn from_named_values<'v>(
        rule: Arc<Rule>,
        package: Arc<Package>,
        internals: &ModuleInternals,
        named: &SmallMap<StringValue<'v>, Value<'v>>,
        ignore_attrs_for_profiling: bool,
        call_stack: Option<CallStack>,
    ) -> buck2_error::Result<Self> {
        let _ = ignore_attrs_for_profiling;
        let (target_name, attr_values) =
            rule.attributes
                .parse_named_values(named, internals, rule.rule_type.name())?;
        let package_name = internals.buildfile_path().package();
        let label = TargetLabel::new(package_name.dupe(), target_name);
        let mut deps_cache = CoercedDepsCollector::new();

        for a in rule.attributes.attrs(&attr_values, AttrInspectOptions::All) {
            a.traverse(label.pkg(), &mut deps_cache)?;
        }

        let super_package = internals.super_package();
        let package_cfg_modifiers = super_package.cfg_modifiers().duped();
        let test_config_unification_rollout = super_package.test_config_unification_rollout();
        drop(super_package);

        Ok(TargetNode::new(
            rule,
            package,
            label,
            attr_values,
            CoercedDeps::from(deps_cache),
            call_stack
                .map(StarlarkCallStackWrapper)
                .map(StarlarkCallStack::new),
            package_cfg_modifiers,
            test_config_unification_rollout,
        ))
    }
}
