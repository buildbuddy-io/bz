/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashMap;
use std::sync::Arc;

use bz_core::target::label::label::TargetLabelRef;
use bz_core::target::name::TargetNameRef;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_node::attrs::attr::Attribute;
use bz_node::attrs::attr::CoercedValue;
use bz_node::attrs::attr_type::bool::BoolLiteral;
use bz_node::attrs::attr_type::string::StringLiteral;
use bz_node::attrs::coerced_attr::CoercedAttr;
use bz_node::attrs::configurable::AttrIsConfigurable;
use bz_node::attrs::fmt_context::AttrFmtContext;
use bz_node::attrs::inspect_options::AttrInspectOptions;
use bz_node::attrs::spec::AttributeId;
use bz_node::attrs::spec::AttributeSpec;
use bz_node::attrs::spec::internal::NAME_ATTRIBUTE;
use bz_node::attrs::spec::internal::VISIBILITY_ATTRIBUTE;
use bz_node::attrs::spec::internal::WITHIN_VIEW_ATTRIBUTE;
use bz_node::attrs::spec::internal::attr_is_configurable;
use bz_node::attrs::values::AttrValues;
use bz_util::arc_str::ArcStr;
use dupe::Dupe;
use starlark::docs::DocString;
use starlark::eval::Evaluator;
use starlark::eval::ParametersParser;
use starlark::eval::ParametersSpec;
use starlark::eval::ParametersSpecParam;
use starlark::typing::ParamIsRequired;
use starlark::typing::ParamSpec;
use starlark::typing::Ty;
use starlark::typing::TyFunction;
use starlark::values::StringValue;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark_map::small_map::SmallMap;

use crate::attrs::AttributeCoerceExt;
use crate::attrs::starlark_attribute::FrozenBazelComputedDefault;
use crate::interpreter::module_internals::ModuleInternals;
use crate::nodes::check_within_view::check_within_view;
use bz_interpreter::types::configured_providers_label::StarlarkProvidersLabel;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum AttributeSpecParseError {
    #[error("Missing required attribute `{0}` for `{1}`")]
    MissingRequiredAttribute(String, String),
    #[error("Unknown attribute `{0}` for `{1}`")]
    UnknownAttribute(String, String),
    #[error("Expected string value for `name`, got `{0}`")]
    ExpectedStringName(String),
    #[error("Bazel computed default for attribute `{0}` depends on computed attribute `{1}`")]
    ComputedDefaultDependsOnComputedAttribute(String, String),
    #[error("Bazel computed default for attribute `{0}` depends on unknown attribute `{1}`")]
    ComputedDefaultUnknownDependency(String, String),
    #[error(
        "Bazel computed default for attribute `{0}` selected a default but the attribute has no default"
    )]
    ComputedDefaultMissingFallback(String),
}

fn coerce_attr_value(
    attr_name: &str,
    attribute: &Attribute,
    configurable: AttrIsConfigurable,
    internals: &ModuleInternals,
    value: Value,
) -> bz_error::Result<CoercedValue> {
    if attr_name == "testonly"
        && let Some(value) = i64::unpack_value(value)?
        && (value == 0 || value == 1)
    {
        return Ok(CoercedValue::Custom(CoercedAttr::Bool(BoolLiteral(
            value != 0,
        ))));
    }

    attribute.coerce(
        attr_name,
        configurable,
        internals.attr_coercion_context(),
        value,
    )
}

fn package_default_testonly_attr(
    attr_name: &str,
    internals: &ModuleInternals,
) -> Option<CoercedAttr> {
    if attr_name == "testonly" && internals.super_package().default_testonly() {
        Some(CoercedAttr::Bool(BoolLiteral(true)))
    } else {
        None
    }
}

fn alloc_coerced_attr_value<'v>(
    value: &CoercedAttr,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    match value {
        CoercedAttr::Label(label)
        | CoercedAttr::SourceLabel(label)
        | CoercedAttr::Dep(label)
        | CoercedAttr::ConfigurationDep(label)
        | CoercedAttr::SplitTransitionDep(label) => {
            return Ok(eval
                .heap()
                .alloc(StarlarkProvidersLabel::new(label.clone())));
        }
        CoercedAttr::List(list) => {
            let values = list
                .iter()
                .map(|item| alloc_coerced_attr_value(item, eval))
                .collect::<starlark::Result<Vec<_>>>()?;
            return Ok(eval.heap().alloc(AllocList(values)));
        }
        CoercedAttr::Tuple(tuple) => {
            let values = tuple
                .iter()
                .map(|item| alloc_coerced_attr_value(item, eval))
                .collect::<starlark::Result<Vec<_>>>()?;
            return Ok(eval.heap().alloc(AllocList(values)));
        }
        CoercedAttr::Dict(dict) => {
            let values = dict
                .iter()
                .map(|(key, value)| {
                    Ok((
                        alloc_coerced_attr_value(key, eval)?,
                        alloc_coerced_attr_value(value, eval)?,
                    ))
                })
                .collect::<starlark::Result<Vec<_>>>()?;
            return Ok(eval.heap().alloc(AllocDict(values)));
        }
        CoercedAttr::OneOf(value, _) => return alloc_coerced_attr_value(value, eval),
        CoercedAttr::None => return Ok(Value::new_none()),
        _ => {}
    }
    let json = value
        .to_json(&AttrFmtContext::NO_CONTEXT)
        .map_err(starlark::Error::from)?;
    Ok(eval.heap().alloc(json))
}

fn named_values_contains(named: &SmallMap<StringValue<'_>, Value<'_>>, attr_name: &str) -> bool {
    named.iter().any(|(name, _)| name.as_str() == attr_name)
}

fn attr_spec_entry<'a>(
    spec: &'a AttributeSpec,
    attr_name: &str,
) -> Option<(&'a str, AttributeId, &'a Attribute)> {
    spec.attr_specs().find(|(name, _, _)| *name == attr_name)
}

fn computed_default_depends_on_computed_attr(
    computed_defaults: &SmallMap<String, FrozenBazelComputedDefault>,
    dependency: &str,
) -> bool {
    computed_defaults
        .iter()
        .any(|(name, _)| name.as_str() == dependency)
}

fn apply_bazel_computed_defaults<'v>(
    spec: &AttributeSpec,
    attr_values: AttrValues,
    named: &SmallMap<StringValue<'v>, Value<'v>>,
    internals: &ModuleInternals,
    computed_defaults: &SmallMap<String, FrozenBazelComputedDefault>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> bz_error::Result<AttrValues> {
    if computed_defaults.is_empty() {
        return Ok(attr_values);
    }

    let mut computed_values = Vec::new();
    for (attr_name, computed_default) in computed_defaults {
        let Some((_, attr_idx, attribute)) = attr_spec_entry(spec, attr_name) else {
            continue;
        };
        if named_values_contains(named, attr_name) || attr_values.get(attr_idx).is_some() {
            continue;
        }

        let mut positional = Vec::with_capacity(computed_default.dependencies().len());
        for dependency in computed_default.dependencies() {
            if computed_default_depends_on_computed_attr(computed_defaults, dependency) {
                return Err(
                    AttributeSpecParseError::ComputedDefaultDependsOnComputedAttribute(
                        attr_name.to_owned(),
                        dependency.to_owned(),
                    )
                    .into(),
                );
            }
            let value = spec
                .attrs(&attr_values, AttrInspectOptions::All)
                .find(|attr| attr.name == dependency)
                .ok_or_else(|| {
                    AttributeSpecParseError::ComputedDefaultUnknownDependency(
                        attr_name.to_owned(),
                        dependency.to_owned(),
                    )
                })?;
            positional.push(
                alloc_coerced_attr_value(value.value, eval).map_err(bz_error::Error::from)?,
            );
        }

        let raw_value = eval
            .eval_function(computed_default.callback().to_value(), &positional, &[])
            .map_err(bz_error::Error::from)?;
        let coerced = coerce_attr_value(
            attr_name,
            attribute,
            attr_is_configurable(attr_name),
            internals,
            raw_value,
        )
        .with_buck_error_context(|| {
            format!("Error coercing Bazel computed default for attribute `{attr_name}`")
        })?;
        match coerced {
            CoercedValue::Custom(value) => {
                computed_values.push((attr_idx, attr_name.clone(), value));
            }
            CoercedValue::Default => {
                if attribute.default().is_none() {
                    return Err(AttributeSpecParseError::ComputedDefaultMissingFallback(
                        attr_name.clone(),
                    )
                    .into());
                }
            }
        }
    }

    if computed_values.is_empty() {
        return Ok(attr_values);
    }

    computed_values.sort_by_key(|(attr_idx, _, _)| *attr_idx);

    if let Some(within_view) = attr_values.get(WITHIN_VIEW_ATTRIBUTE.id) {
        let within_view = match within_view {
            CoercedAttr::WithinView(within_view) => within_view,
            _ => return Err(internal_error!("`within_view` coerced incorrectly")),
        };
        for (attr_idx, attr_name, value) in &computed_values {
            let Some((_, _, attribute)) = spec.attr_specs().find(|(_, idx, _)| idx == attr_idx)
            else {
                continue;
            };
            check_within_view(
                value,
                internals.buildfile_path().package(),
                attribute.coercer(),
                within_view,
                None,
            )
            .with_buck_error_context(|| {
                format!(
                    "checking `within_view` for Bazel computed default attribute `{}`",
                    attr_name,
                )
            })?;
        }
    }

    let mut merged = AttrValues::with_capacity(spec.len());
    let mut computed_values = computed_values.into_iter().peekable();
    for (_, attr_idx, _) in spec.attr_specs() {
        if let Some(value) = attr_values.get(attr_idx) {
            merged.push_sorted(attr_idx, value.clone());
        } else if computed_values
            .peek()
            .is_some_and(|(computed_idx, _, _)| *computed_idx == attr_idx)
        {
            let (_, _, value) = computed_values
                .next()
                .expect("computed value exists after peek");
            merged.push_sorted(attr_idx, value);
        }
    }
    merged.shrink_to_fit();
    Ok(merged)
}

pub trait AttributeSpecExt {
    fn start_parse<'a, 'v>(
        &'a self,
        param_parser: &mut ParametersParser<'v, '_>,
        size_hint: usize,
        use_bazel_target_names: bool,
    ) -> bz_error::Result<(
        // "name" attribute value.
        &'v TargetNameRef,
        // Remaining attributes.
        impl ExactSizeIterator<Item = (&'a str, AttributeId, &'a Attribute)> + 'a,
        // Populated with name.
        AttrValues,
    )>;

    fn parse_params<'v>(
        &self,
        param_parser: &mut ParametersParser<'v, '_>,
        arg_count: usize,
        internals: &ModuleInternals,
    ) -> bz_error::Result<(&'v TargetNameRef, AttrValues)>;

    fn parse_named_values<'v>(
        &self,
        named: &SmallMap<StringValue<'v>, Value<'v>>,
        internals: &ModuleInternals,
        rule_name: &str,
    ) -> bz_error::Result<(&'v TargetNameRef, AttrValues)>;

    fn parse_named_values_with_bazel_computed_defaults<'v>(
        &self,
        named: &SmallMap<StringValue<'v>, Value<'v>>,
        internals: &ModuleInternals,
        rule_name: &str,
        computed_defaults: &SmallMap<String, FrozenBazelComputedDefault>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> bz_error::Result<(&'v TargetNameRef, AttrValues)>;

    /// Returns a starlark Parameters for the rule callable, but not default values.
    fn signature(&self, rule_name: String) -> ParametersSpec<Value<'_>>;

    /// Returns a starlark Parameters for the rule callable, with default values.
    fn signature_with_default_value(&self, rule_name: String) -> ParametersSpec<Arc<CoercedAttr>>;

    fn ty_function(&self) -> TyFunction;

    fn starlark_types(&self) -> Vec<Ty>;
    fn docstrings(&self) -> HashMap<String, Option<DocString>>;
}

fn target_name<'v>(
    name: &'v str,
    use_bazel_target_names: bool,
) -> bz_error::Result<&'v TargetNameRef> {
    if use_bazel_target_names {
        TargetNameRef::new_bazel(name)
    } else {
        TargetNameRef::new(name)
    }
}

impl AttributeSpecExt for AttributeSpec {
    fn start_parse<'a, 'v>(
        &'a self,
        param_parser: &mut ParametersParser<'v, '_>,
        size_hint: usize,
        use_bazel_target_names: bool,
    ) -> bz_error::Result<(
        &'v TargetNameRef,
        impl ExactSizeIterator<Item = (&'a str, AttributeId, &'a Attribute)> + 'a,
        AttrValues,
    )> {
        let mut attr_values = AttrValues::with_capacity(size_hint);

        let mut indices = self.attr_specs();
        let name = match indices.next() {
            Some((name_name, attr_idx, _attr)) if name_name == NAME_ATTRIBUTE.name => {
                let name = param_parser.next()?;
                attr_values.push_sorted(
                    attr_idx,
                    CoercedAttr::String(StringLiteral(ArcStr::from(name))),
                );
                name
            }
            _ => {
                return Err(internal_error!("First attribute is `name`, it is known"));
            }
        };
        let name = target_name(name, use_bazel_target_names)?;
        Ok((name, indices, attr_values))
    }

    /// Parses params extracting the TargetName and the attribute values to store in the TargetNode.
    fn parse_params<'v>(
        &self,
        param_parser: &mut ParametersParser<'v, '_>,
        arg_count: usize,
        internals: &ModuleInternals,
    ) -> bz_error::Result<(&'v TargetNameRef, AttrValues)> {
        let (name, indices, mut attr_values) = self.start_parse(
            param_parser,
            arg_count,
            internals.is_bazel_compat_build_file(),
        )?;

        let target_label = TargetLabelRef::new(internals.buildfile_path().package(), name);

        let mut default_allowed_deps = HashMap::new();

        for (attr_name, attr_idx, attribute) in indices {
            let configurable = attr_is_configurable(attr_name);

            let user_value: Option<Value> = match attribute.default() {
                Some(_) => param_parser.next_opt()?,
                None => Some(param_parser.next()?),
            };

            let attr_is_visibility = attr_name == VISIBILITY_ATTRIBUTE.name;
            let attr_is_within_view = attr_name == WITHIN_VIEW_ATTRIBUTE.name;
            if let Some(v) = user_value {
                let mut coerced =
                    coerce_attr_value(attr_name, attribute, configurable, internals, v)
                        .with_buck_error_context(|| {
                            format!("Error coercing attribute `{attr_name}` of `{target_label}`",)
                        })?;

                if attr_is_visibility {
                    if coerced == CoercedValue::Default {
                        let super_package = internals.super_package();
                        coerced = CoercedValue::Custom(CoercedAttr::Visibility(
                            super_package.visibility().dupe(),
                        ));
                    }
                } else if attr_is_within_view {
                    if coerced == CoercedValue::Default {
                        let super_package = internals.super_package();
                        coerced = CoercedValue::Custom(CoercedAttr::WithinView(
                            super_package.within_view().dupe(),
                        ));
                    }
                }

                match coerced {
                    CoercedValue::Custom(v) => {
                        attr_values.push_sorted(attr_idx, v);
                        default_allowed_deps.insert(attr_name, attribute.default_allowed_deps());
                    }
                    CoercedValue::Default => {}
                }
            } else if attr_is_visibility {
                let super_package = internals.super_package();
                attr_values.push_sorted(
                    attr_idx,
                    CoercedAttr::Visibility(super_package.visibility().dupe()),
                );
            } else if attr_is_within_view {
                let super_package = internals.super_package();
                attr_values.push_sorted(
                    attr_idx,
                    CoercedAttr::WithinView(super_package.within_view().dupe()),
                );
            } else if let Some(default_testonly) =
                package_default_testonly_attr(attr_name, internals)
            {
                attr_values.push_sorted(attr_idx, default_testonly);
            }
        }

        attr_values.shrink_to_fit();

        // For now `within_view` is always set, but let's make code more robust.
        if let Some(within_view) = attr_values.get(WITHIN_VIEW_ATTRIBUTE.id) {
            let within_view = match within_view {
                CoercedAttr::WithinView(within_view) => within_view,
                _ => return Err(internal_error!("`within_view` coerced incorrectly")),
            };
            for a in self.attrs(&attr_values, AttrInspectOptions::DefinedOnly) {
                let default_deps = default_allowed_deps.get(&a.name).copied().flatten();
                check_within_view(
                    a.value,
                    internals.buildfile_path().package(),
                    a.attr.coercer(),
                    within_view,
                    default_deps,
                )
                .with_buck_error_context(|| {
                    format!(
                        "checking `within_view` for attribute `{}` of `{}`",
                        a.name, target_label,
                    )
                })?;
            }
        }

        Ok((name, attr_values))
    }

    fn parse_named_values<'v>(
        &self,
        named: &SmallMap<StringValue<'v>, Value<'v>>,
        internals: &ModuleInternals,
        rule_name: &str,
    ) -> bz_error::Result<(&'v TargetNameRef, AttrValues)> {
        for (provided_name, _) in named {
            if !self
                .attr_specs()
                .any(|(attr_name, _, _)| attr_name == provided_name.as_str())
            {
                return Err(AttributeSpecParseError::UnknownAttribute(
                    provided_name.as_str().to_owned(),
                    rule_name.to_owned(),
                )
                .into());
            }
        }

        let name_value = named
            .iter()
            .find_map(|(key, value)| (key.as_str() == NAME_ATTRIBUTE.name).then_some(*value))
            .ok_or_else(|| {
                AttributeSpecParseError::MissingRequiredAttribute(
                    NAME_ATTRIBUTE.name.to_owned(),
                    rule_name.to_owned(),
                )
            })?;
        let Some(name) = name_value.unpack_str() else {
            return Err(AttributeSpecParseError::ExpectedStringName(
                name_value.get_type().to_owned(),
            )
            .into());
        };
        let name = target_name(name, internals.is_bazel_compat_build_file())?;
        let target_label = TargetLabelRef::new(internals.buildfile_path().package(), name);
        let mut attr_values = AttrValues::with_capacity(named.len());
        let mut default_allowed_deps = HashMap::new();

        for (attr_name, attr_idx, attribute) in self.attr_specs() {
            let attr_is_visibility = attr_name == VISIBILITY_ATTRIBUTE.name;
            let attr_is_within_view = attr_name == WITHIN_VIEW_ATTRIBUTE.name;
            let value = named
                .iter()
                .find_map(|(key, value)| (key.as_str() == attr_name).then_some(*value));

            if attr_name == NAME_ATTRIBUTE.name {
                attr_values.push_sorted(
                    attr_idx,
                    CoercedAttr::String(StringLiteral(ArcStr::from(name.as_str()))),
                );
                continue;
            }

            if let Some(v) = value {
                let configurable = attr_is_configurable(attr_name);
                let mut coerced =
                    coerce_attr_value(attr_name, attribute, configurable, internals, v)
                        .with_buck_error_context(|| {
                            format!("Error coercing attribute `{attr_name}` of `{target_label}`",)
                        })?;

                if attr_is_visibility {
                    if coerced == CoercedValue::Default {
                        let super_package = internals.super_package();
                        coerced = CoercedValue::Custom(CoercedAttr::Visibility(
                            super_package.visibility().dupe(),
                        ));
                    }
                } else if attr_is_within_view && coerced == CoercedValue::Default {
                    let super_package = internals.super_package();
                    coerced = CoercedValue::Custom(CoercedAttr::WithinView(
                        super_package.within_view().dupe(),
                    ));
                }

                match coerced {
                    CoercedValue::Custom(v) => {
                        attr_values.push_sorted(attr_idx, v);
                        default_allowed_deps.insert(attr_name, attribute.default_allowed_deps());
                    }
                    CoercedValue::Default => {}
                }
            } else if attr_is_visibility {
                let super_package = internals.super_package();
                attr_values.push_sorted(
                    attr_idx,
                    CoercedAttr::Visibility(super_package.visibility().dupe()),
                );
            } else if attr_is_within_view {
                let super_package = internals.super_package();
                attr_values.push_sorted(
                    attr_idx,
                    CoercedAttr::WithinView(super_package.within_view().dupe()),
                );
            } else if let Some(default_testonly) =
                package_default_testonly_attr(attr_name, internals)
            {
                attr_values.push_sorted(attr_idx, default_testonly);
            } else if attribute.default().is_none() {
                return Err(AttributeSpecParseError::MissingRequiredAttribute(
                    attr_name.to_owned(),
                    rule_name.to_owned(),
                )
                .into());
            }
        }

        attr_values.shrink_to_fit();

        if let Some(within_view) = attr_values.get(WITHIN_VIEW_ATTRIBUTE.id) {
            let within_view = match within_view {
                CoercedAttr::WithinView(within_view) => within_view,
                _ => return Err(internal_error!("`within_view` coerced incorrectly")),
            };
            for a in self.attrs(&attr_values, AttrInspectOptions::DefinedOnly) {
                let default_deps = default_allowed_deps.get(&a.name).copied().flatten();
                check_within_view(
                    a.value,
                    internals.buildfile_path().package(),
                    a.attr.coercer(),
                    within_view,
                    default_deps,
                )
                .with_buck_error_context(|| {
                    format!(
                        "checking `within_view` for attribute `{}` of `{}`",
                        a.name, target_label,
                    )
                })?;
            }
        }

        Ok((name, attr_values))
    }

    fn parse_named_values_with_bazel_computed_defaults<'v>(
        &self,
        named: &SmallMap<StringValue<'v>, Value<'v>>,
        internals: &ModuleInternals,
        rule_name: &str,
        computed_defaults: &SmallMap<String, FrozenBazelComputedDefault>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> bz_error::Result<(&'v TargetNameRef, AttrValues)> {
        let (name, attr_values) = self.parse_named_values(named, internals, rule_name)?;
        let attr_values = apply_bazel_computed_defaults(
            self,
            attr_values,
            named,
            internals,
            computed_defaults,
            eval,
        )?;
        Ok((name, attr_values))
    }

    /// Returns a starlark Parameters for the rule callable, but not default values.
    fn signature(&self, rule_name: String) -> ParametersSpec<Value<'_>> {
        ParametersSpec::new_named_only(
            &rule_name,
            self.attr_specs().map(|(name, _idx, attribute)| {
                let default = attribute.default();
                (
                    name,
                    match default {
                        Some(_) => ParametersSpecParam::Optional,
                        None => ParametersSpecParam::Required,
                    },
                )
            }),
        )
    }

    /// Returns a starlark Parameters for the rule callable, with default values.
    fn signature_with_default_value(&self, rule_name: String) -> ParametersSpec<Arc<CoercedAttr>> {
        ParametersSpec::new_named_only(
            &rule_name,
            self.attr_specs().map(|(name, _idx, attribute)| {
                let default = attribute.default();
                (
                    name,
                    match default {
                        Some(default) => ParametersSpecParam::Defaulted(default.dupe()),
                        None => ParametersSpecParam::Required,
                    },
                )
            }),
        )
    }

    fn ty_function(&self) -> TyFunction {
        let mut params = Vec::with_capacity(self.attr_specs().len());
        for (name, _idx, attribute) in self.attr_specs() {
            let ty = match attr_is_configurable(name) {
                AttrIsConfigurable::Yes => attribute.starlark_type().to_ty_with_select(),
                AttrIsConfigurable::No => attribute.starlark_type().to_ty(),
            };
            let required = match attribute.default() {
                Some(_) => ParamIsRequired::No,
                None => ParamIsRequired::Yes,
            };
            params.push((starlark::util::ArcStr::from(name), required, ty));
        }
        let params = ParamSpec::new_named_only(params).unwrap();
        TyFunction::new(params, Ty::none())
    }

    fn starlark_types(&self) -> Vec<Ty> {
        self.attr_specs()
            .map(|(_, _, a)| a.starlark_type().to_ty())
            .collect()
    }

    fn docstrings(&self) -> HashMap<String, Option<DocString>> {
        self.attr_specs()
            .map(|(name, _idx, attr)| (name.to_owned(), attr.docstring()))
            .collect()
    }
}
