/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;

use allocative::Allocative;
use buck2_core::cells::external::bazel_canonical_label_key;
use buck2_core::plugins::PluginKind;
use buck2_error::internal_error;
use buck2_interpreter::late_binding_ty::AnalysisContextReprLate;
use buck2_interpreter::late_binding_ty::ProviderReprLate;
use buck2_interpreter::starlark_promise::StarlarkPromise;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use buck2_interpreter::types::rule::FROZEN_BAZEL_ASPECT_INFO_GET_IMPL;
use buck2_interpreter::types::rule::FROZEN_BAZEL_ASPECTS_GET_IMPL;
use buck2_interpreter::types::rule::FROZEN_BAZEL_ATTR_ASPECTS_GET_IMPL;
use buck2_interpreter::types::rule::FROZEN_PROMISE_ARTIFACT_MAPPINGS_GET_IMPL;
use buck2_interpreter::types::rule::FROZEN_RULE_GET_IMPL;
use buck2_interpreter::types::target_label::StarlarkTargetLabel;
use buck2_interpreter::types::transition::transition_id_from_value;
use buck2_interpreter::types::transition::transition_id_from_value_for_bazel_attr;
use buck2_node::attrs::attr::Attribute;
use buck2_node::attrs::attr_type::AttrType;
use buck2_node::attrs::attr_type::bool::BoolLiteral;
use buck2_node::attrs::attr_type::dict::DictLiteral;
use buck2_node::attrs::attr_type::list::ListLiteral;
use buck2_node::attrs::attr_type::string::StringLiteral;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::attrs::display::AttrDisplayWithContextExt;
use buck2_node::attrs::spec::AttributeSpec;
use buck2_node::bzl_or_bxl_path::BzlOrBxlPath;
use buck2_node::nodes::unconfigured::RuleKind;
use buck2_node::nodes::unconfigured::TargetNode;
use buck2_node::rule::BazelImplicitOutput;
use buck2_node::rule::BazelOutputAttr;
use buck2_node::rule::BazelToolchainRequirement;
use buck2_node::rule::Rule;
use buck2_node::rule::RuleIncomingTransition;
use buck2_node::rule_type::RuleType;
use buck2_node::rule_type::StarlarkRuleType;
use buck2_util::arc_str::ArcSlice;
use buck2_util::arc_str::ArcStr;
use derive_more::Display;
use dupe::Dupe;
use either::Either;
use itertools::Itertools;
use starlark::any::ProvidesStaticType;
use starlark::docs::DocFunction;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::DocStringKind;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::eval::ParametersSpec;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::typing::ParamSpec;
use starlark::typing::Ty;
use starlark::values::AllocValue;
use starlark::values::Freeze;
use starlark::values::FreezeError;
use starlark::values::FreezeResult;
use starlark::values::Freezer;
use starlark::values::FrozenStringValue;
use starlark::values::FrozenValue;
use starlark::values::FrozenValueTyped;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::dict::DictRef;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::ListType;
use starlark::values::list::UnpackList;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::structs::StructRef;
use starlark::values::typing::FrozenStarlarkCallable;
use starlark::values::typing::StarlarkCallable;
use starlark::values::typing::StarlarkCallableChecked;
use starlark_map::small_map::SmallMap;

use crate::attrs::attrs_global::attr_coercion_context_for_bzl;
use crate::attrs::starlark_attribute::BazelComputedDefault;
use crate::attrs::starlark_attribute::FrozenBazelComputedDefault;
use crate::attrs::starlark_attribute::StarlarkAttribute;
use crate::bazel::aspect::collect_bazel_aspect_hidden_attributes;
use crate::bazel::aspect::collect_bazel_aspect_toolchains;
use crate::bazel::aspect::frozen_aspect_implementation;
use crate::bazel::aspect::frozen_aspect_info;
use crate::interpreter::build_context::BuildContext;
use crate::interpreter::build_context::PerFileTypeContext;
use crate::interpreter::module_internals::ModuleInternals;
use crate::nodes::attr_spec::AttributeSpecExt;
use crate::nodes::unconfigured::TargetNodeExt;
use crate::nodes::unconfigured::bazel_output_file_targets;
use crate::plugins::PluginKindArg;

pub static NAME_ATTRIBUTE_FIELD: &str = "name";

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct StarlarkExecGroup {
    toolchains: Vec<String>,
    exec_compatible_with: Vec<String>,
}

impl fmt::Display for StarlarkExecGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<exec_group toolchains={} exec_compatible_with={}>",
            self.toolchains.len(),
            self.exec_compatible_with.len()
        )
    }
}

starlark_simple_value!(StarlarkExecGroup);

#[starlark_value(type = "exec_group")]
impl<'v> StarlarkValue<'v> for StarlarkExecGroup {}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelSubruleCppFragment;

impl fmt::Display for BazelSubruleCppFragment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<cpp fragment>")
    }
}

starlark_simple_value!(BazelSubruleCppFragment);

#[starlark_value(type = "cpp")]
impl<'v> StarlarkValue<'v> for BazelSubruleCppFragment {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_subrule_cpp_fragment_methods)
    }
}

#[starlark_module]
fn bazel_subrule_cpp_fragment_methods(builder: &mut MethodsBuilder) {
    fn compilation_mode(
        #[starlark(this)] _this: &BazelSubruleCppFragment,
    ) -> starlark::Result<&'static str> {
        Ok("fastbuild")
    }
}

fn bazel_subrule_context<'v>(heap: Heap<'v>) -> Value<'v> {
    let cpp = heap.alloc(BazelSubruleCppFragment);
    let fragments = heap.alloc(AllocStruct([("cpp", cpp)]));
    heap.alloc(AllocStruct([("fragments", fragments)]))
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
struct StarlarkMacroCallable<'v> {
    implementation: Value<'v>,
    default_none_attrs: Vec<String>,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct FrozenStarlarkMacroCallable {
    implementation: FrozenValue,
    default_none_attrs: Vec<String>,
}

impl<'v> fmt::Display for StarlarkMacroCallable<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("macro(...)")
    }
}

impl fmt::Display for FrozenStarlarkMacroCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("macro(...)")
    }
}

impl<'v> AllocValue<'v> for StarlarkMacroCallable<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

impl<'v> Freeze for StarlarkMacroCallable<'v> {
    type Frozen = FrozenStarlarkMacroCallable;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkMacroCallable {
            implementation: self.implementation.freeze(freezer)?,
            default_none_attrs: self.default_none_attrs,
        })
    }
}

fn invoke_bazel_macro<'v>(
    implementation: Value<'v>,
    default_none_attrs: &[String],
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let positional = args.positions(eval.heap())?.collect::<Vec<_>>();
    let mut named = args
        .names_map()?
        .into_iter()
        .map(|(name, value)| (name.as_str().to_owned(), value))
        .collect::<Vec<_>>();
    for attr_name in default_none_attrs {
        if !named.iter().any(|(name, _)| name == attr_name) {
            named.push((attr_name.clone(), Value::new_none()));
        }
    }
    let named = named
        .iter()
        .map(|(name, value)| (name.as_str(), *value))
        .collect::<Vec<_>>();

    eval.eval_function(implementation, &positional, &named)
}

#[starlark_value(type = "macro")]
impl<'v> StarlarkValue<'v> for StarlarkMacroCallable<'v> {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        invoke_bazel_macro(self.implementation, &self.default_none_attrs, args, eval)
    }
}

starlark_simple_value!(FrozenStarlarkMacroCallable);

#[starlark_value(type = "macro")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkMacroCallable {
    type Canonical = StarlarkMacroCallable<'v>;

    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        invoke_bazel_macro(
            self.implementation.to_value(),
            &self.default_none_attrs,
            args,
            eval,
        )
    }
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
struct StarlarkSubrule<'v> {
    implementation: Value<'v>,
    attr_names: Vec<String>,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct FrozenStarlarkSubrule {
    implementation: FrozenValue,
    attr_names: Vec<String>,
}

impl<'v> fmt::Display for StarlarkSubrule<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<subrule>")
    }
}

impl fmt::Display for FrozenStarlarkSubrule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<subrule>")
    }
}

impl<'v> AllocValue<'v> for StarlarkSubrule<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

impl<'v> Freeze for StarlarkSubrule<'v> {
    type Frozen = FrozenStarlarkSubrule;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkSubrule {
            implementation: self.implementation.freeze(freezer)?,
            attr_names: self.attr_names,
        })
    }
}

fn invoke_bazel_subrule<'v>(
    implementation: Value<'v>,
    attr_names: &[String],
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let mut positional = Vec::with_capacity(args.len()? + 1);
    positional.push(bazel_subrule_context(eval.heap()));
    positional.extend(args.positions(eval.heap())?);

    let mut named = args
        .names_map()?
        .into_iter()
        .map(|(name, value)| (name.as_str().to_owned(), value))
        .collect::<Vec<_>>();
    for attr_name in attr_names {
        if !named.iter().any(|(name, _)| name == attr_name) {
            named.push((attr_name.clone(), Value::new_none()));
        }
    }
    let named = named
        .iter()
        .map(|(name, value)| (name.as_str(), *value))
        .collect::<Vec<_>>();

    eval.eval_function(implementation, &positional, &named)
}

#[starlark_value(type = "subrule")]
impl<'v> StarlarkValue<'v> for StarlarkSubrule<'v> {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        invoke_bazel_subrule(self.implementation, &self.attr_names, args, eval)
    }
}

starlark_simple_value!(FrozenStarlarkSubrule);

#[starlark_value(type = "subrule")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkSubrule {
    type Canonical = StarlarkSubrule<'v>;

    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        invoke_bazel_subrule(self.implementation.to_value(), &self.attr_names, args, eval)
    }
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative, Clone, Copy)]
enum RuleImpl<'v> {
    BuildRule(StarlarkCallable<'v, (FrozenValue,), ListType<FrozenValue>>),
    BxlAnon(StarlarkCallable<'v, (FrozenValue, FrozenValue), ListType<FrozenValue>>),
}

/// The callable that's returned from a `rule()` call. Once frozen, and called, it adds targets'
/// parameters to the context
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub struct StarlarkRuleCallable<'v> {
    /// The import path that contains the rule() call; stored here so we can retrieve extra
    /// information during `export_as()`
    rule_path: BzlOrBxlPath,
    /// Once exported, the `import_path` and `name` of the callable. Used in DICE to retrieve rule
    /// implementations
    id: RefCell<Option<StarlarkRuleType>>,
    /// The implementation function for this rule.
    /// If is a build rule or anon rule in bzl must take a ctx,
    /// If is a bxl anon rule must take a bxl context and attrs.
    implementation: RuleImpl<'v>,
    // Field Name -> Attribute
    attributes: AttributeSpec,
    /// Type for the typechecker.
    ty: Ty,
    /// When specified, this transition will be applied to the target before configuring it.
    cfg: RuleIncomingTransition,
    /// The plugins that are used by these targets
    uses_plugins: Vec<PluginKind>,
    /// Bazel toolchain types declared by `rule(toolchains = ...)`.
    bazel_toolchains: Vec<BazelToolchainRequirement>,
    /// Bazel toolchain types declared by aspects attached to this rule's attrs.
    bazel_aspect_toolchains: Vec<BazelToolchainRequirement>,
    /// Bazel explicit output attrs declared by `attr.output()` / `attr.output_list()`.
    bazel_output_attrs: Vec<BazelOutputAttr>,
    /// Bazel implicit outputs declared by `rule(outputs = {...})`.
    bazel_implicit_outputs: Vec<BazelImplicitOutput>,
    /// Whether Bazel output artifacts from this rule are declared under genfiles instead of bin.
    bazel_output_to_genfiles: bool,
    /// Whether the rule was declared through Bazel's `rule(implementation = ...)` API.
    is_bazel_rule: bool,
    /// Whether the rule was declared with Bazel's `build_setting = ...`.
    is_bazel_build_setting: bool,
    /// Bazel rule initializer called at target declaration time before attr coercion.
    bazel_initializer: Option<Value<'v>>,
    /// Public Starlark attrs passed to the Bazel initializer when explicitly provided.
    bazel_initializer_attrs: Vec<String>,
    /// Bazel aspects attached to this rule's label-like attrs.
    bazel_attr_aspects: SmallMap<String, Vec<Value<'v>>>,
    /// Bazel Starlark computed defaults keyed by attr name.
    bazel_computed_defaults: SmallMap<String, BazelComputedDefault<'v>>,
    /// This kind of the rule, e.g. whether it can be used in configuration context.
    rule_kind: RuleKind,
    /// The raw docstring for this rule
    docs: Option<String>,
    /// When evaluating rule function, take only the `name` argument, ignore the others.
    ignore_attrs_for_profiling: bool,
    /// Optional map of the promise artifact name to starlark function.
    /// `None` for normal rules, `Some` for anon targets.
    artifact_promise_mappings: Option<ArtifactPromiseMappings<'v>>,
}

/// Mappings of promise artifact name to the starlark function that will produce it, for anon targets.
#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
struct ArtifactPromiseMappings<'v> {
    mappings: SmallMap<StringValue<'v>, Value<'v>>,
}

/// Mappings of frozen promise artifact name to the frozen starlark function that will produce it, for anon targets.
#[derive(Debug, ProvidesStaticType, Trace, Allocative)]
pub struct FrozenArtifactPromiseMappings {
    pub mappings: SmallMap<FrozenStringValue, FrozenValue>,
}

impl<'v> Display for StarlarkRuleCallable<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.id.borrow() {
            Some(id) => write!(f, "<rule {}>", id.name),
            None => write!(f, "<rule>"),
        }
    }
}

/// Errors around rule declaration, instantiation, validation, etc
#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum RuleError {
    #[error("The output of rule() may only be called after the module is loaded")]
    RuleCalledBeforeFreezing,
    #[error("`{0}` is not a valid attribute name")]
    InvalidParameterName(String),
    #[error("Rule defined in `{0}` must be assigned to a variable, e.g. `my_rule = rule(...)`")]
    RuleNotAssigned(BzlOrBxlPath),
    #[error(
        "Rule defined with both `is_configuration_rule` and `is_toolchain_rule`, these options are mutually exclusive"
    )]
    IsConfigurationAndToolchain,
    #[error("`rule` can only be declared in bzl files")]
    RuleNotInBzl,
    #[error("Cannot specify `cfg` and `supports_incoming_transition` at the same time")]
    CfgAndSupportsIncomingTransition,
    #[error("{0} rules do not support incoming transitions")]
    RuleDoesNotSupportIncomingTransition(&'static str),
    #[error("`rule` requires exactly one implementation function")]
    MissingOrConflictingImplementation,
    #[error("Bazel `build_setting` must be created by `config.*`, got `{0}`")]
    InvalidBazelBuildSetting(String),
    #[error("unsupported Bazel build setting type `{0}`")]
    UnsupportedBazelBuildSettingType(String),
    #[error("unsupported Bazel toolchain declaration `{0}`")]
    UnsupportedBazelToolchain(String),
    #[error("unsupported Bazel rule outputs declaration `{0}`")]
    UnsupportedBazelOutputs(String),
    #[error("Bazel rule initializer only supports named rule arguments")]
    BazelInitializerPositionalArgs,
    #[error("Bazel rule initializer returned `{0}`, expected a dict or None")]
    InvalidBazelInitializerReturn(String),
    #[error("Bazel rule initializer returned non-string key `{0}`")]
    InvalidBazelInitializerReturnKey(String),
    #[error("Bazel rule initializer cannot change the target name")]
    BazelInitializerChangedName,
}

fn bazel_build_setting_attrs(
    build_setting: Option<Value<'_>>,
) -> buck2_error::Result<Vec<(String, Attribute)>> {
    let Some(build_setting_value) = build_setting else {
        return Ok(Vec::new());
    };
    let Some(build_setting) = StructRef::from_value(build_setting_value) else {
        return Err(RuleError::InvalidBazelBuildSetting(build_setting_value.to_repr()).into());
    };
    let kind = build_setting
        .iter()
        .find_map(|(name, value)| (name.as_str() == "type").then_some(value))
        .and_then(|value| value.unpack_str())
        .ok_or_else(|| RuleError::InvalidBazelBuildSetting(build_setting_value.to_repr()))?;
    let attr_type = match kind {
        "int" => AttrType::int(),
        "bool" => AttrType::bool(),
        "string" => AttrType::string(),
        "string_list" => AttrType::list(AttrType::string()),
        other => return Err(RuleError::UnsupportedBazelBuildSettingType(other.to_owned()).into()),
    };
    Ok(vec![
        (
            "build_setting_default".to_owned(),
            Attribute::new(None, "", attr_type)?,
        ),
        (
            "help".to_owned(),
            Attribute::new(
                Some(Arc::new(CoercedAttr::String(StringLiteral(ArcStr::from(
                    "",
                ))))),
                "",
                AttrType::string(),
            )?,
        ),
    ])
}

fn add_bazel_common_implicit_attrs(
    attrs: &mut Vec<(String, Attribute)>,
) -> buck2_error::Result<()> {
    fn add_if_absent(
        attrs: &mut Vec<(String, Attribute)>,
        name: &str,
        default: CoercedAttr,
        attr_type: AttrType,
    ) -> buck2_error::Result<()> {
        if attrs.iter().any(|(existing, _)| existing == name) {
            return Ok(());
        }
        attrs.push((
            name.to_owned(),
            Attribute::new(Some(Arc::new(default)), "", attr_type)?,
        ));
        Ok(())
    }
    fn empty_list() -> CoercedAttr {
        CoercedAttr::List(ListLiteral(ArcSlice::new([])))
    }
    fn empty_dict() -> CoercedAttr {
        CoercedAttr::Dict(DictLiteral(ArcSlice::new([])))
    }
    fn empty_string() -> CoercedAttr {
        CoercedAttr::String(StringLiteral(ArcStr::from("")))
    }

    add_if_absent(
        attrs,
        "applicable_licenses",
        empty_list(),
        AttrType::list(AttrType::label()),
    )?;
    add_if_absent(
        attrs,
        "aspect_hints",
        empty_list(),
        AttrType::list(AttrType::label()),
    )?;
    add_if_absent(attrs, "deprecation", empty_string(), AttrType::string())?;
    add_if_absent(
        attrs,
        "exec_group_compatible_with",
        empty_dict(),
        AttrType::dict(AttrType::string(), AttrType::list(AttrType::label()), false),
    )?;
    add_if_absent(
        attrs,
        "exec_properties",
        empty_dict(),
        AttrType::dict(AttrType::string(), AttrType::string(), false),
    )?;
    add_if_absent(attrs, "expect_failure", empty_string(), AttrType::string())?;
    add_if_absent(
        attrs,
        "features",
        empty_list(),
        AttrType::list(AttrType::string()),
    )?;
    add_if_absent(
        attrs,
        "generator_function",
        empty_string(),
        AttrType::string(),
    )?;
    add_if_absent(
        attrs,
        "generator_location",
        empty_string(),
        AttrType::string(),
    )?;
    add_if_absent(attrs, "generator_name", empty_string(), AttrType::string())?;
    add_if_absent(
        attrs,
        "package_metadata",
        empty_list(),
        AttrType::list(AttrType::label()),
    )?;
    add_if_absent(
        attrs,
        "restricted_to",
        empty_list(),
        AttrType::list(AttrType::label()),
    )?;
    add_if_absent(
        attrs,
        "tags",
        empty_list(),
        AttrType::list(AttrType::string()),
    )?;
    add_if_absent(
        attrs,
        "testonly",
        CoercedAttr::Bool(BoolLiteral(false)),
        AttrType::bool(),
    )?;
    add_if_absent(
        attrs,
        "toolchains",
        empty_list(),
        AttrType::list(AttrType::label()),
    )?;
    add_if_absent(
        attrs,
        "transitive_configs",
        empty_list(),
        AttrType::list(AttrType::label()),
    )?;
    attrs.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(())
}

fn add_bazel_test_implicit_attrs(attrs: &mut Vec<(String, Attribute)>) -> buck2_error::Result<()> {
    fn add_if_absent(
        attrs: &mut Vec<(String, Attribute)>,
        name: &str,
        default: CoercedAttr,
        attr_type: AttrType,
    ) -> buck2_error::Result<()> {
        if attrs.iter().any(|(existing, _)| existing == name) {
            return Ok(());
        }
        attrs.push((
            name.to_owned(),
            Attribute::new(Some(Arc::new(default)), "", attr_type)?,
        ));
        Ok(())
    }

    add_if_absent(
        attrs,
        "size",
        CoercedAttr::String(StringLiteral(ArcStr::from("medium"))),
        AttrType::string(),
    )?;
    add_if_absent(
        attrs,
        "timeout",
        CoercedAttr::String(StringLiteral(ArcStr::from("moderate"))),
        AttrType::string(),
    )?;
    add_if_absent(
        attrs,
        "flaky",
        CoercedAttr::Bool(BoolLiteral(false)),
        AttrType::bool(),
    )?;
    add_if_absent(attrs, "shard_count", CoercedAttr::Int(-1), AttrType::int())?;
    add_if_absent(
        attrs,
        "local",
        CoercedAttr::Bool(BoolLiteral(false)),
        AttrType::bool(),
    )?;
    add_if_absent(
        attrs,
        "args",
        CoercedAttr::List(ListLiteral(ArcSlice::new([]))),
        AttrType::list(AttrType::string()),
    )?;
    attrs.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(())
}

fn add_bazel_executable_implicit_attrs(
    attrs: &mut Vec<(String, Attribute)>,
) -> buck2_error::Result<()> {
    fn add_string_list_attr(
        attrs: &mut Vec<(String, Attribute)>,
        name: &str,
    ) -> buck2_error::Result<()> {
        if attrs.iter().any(|(existing, _)| existing == name) {
            return Ok(());
        }
        attrs.push((
            name.to_owned(),
            Attribute::new(
                Some(Arc::new(CoercedAttr::List(ListLiteral(ArcSlice::new([]))))),
                "",
                AttrType::list(AttrType::string()),
            )?,
        ));
        Ok(())
    }

    add_string_list_attr(attrs, "args")?;
    add_string_list_attr(attrs, "output_licenses")?;
    attrs.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(())
}

fn normalize_bazel_toolchain_key(key: &str) -> String {
    key.trim_start_matches('@').to_owned()
}

fn bazel_toolchain_key_from_value(
    value: Value<'_>,
    label_ctx: &dyn AttrCoercionContext,
) -> buck2_error::Result<String> {
    if let Some(toolchain) = StarlarkProvidersLabel::from_value(value) {
        return Ok(normalize_bazel_toolchain_key(&bazel_canonical_label_key(
            toolchain.label().target(),
        )));
    }
    if let Some(toolchain) = StarlarkTargetLabel::from_value(value) {
        return Ok(normalize_bazel_toolchain_key(&bazel_canonical_label_key(
            toolchain.label(),
        )));
    }
    if let Some(toolchain) = value.unpack_str() {
        return Ok(normalize_bazel_toolchain_key(&bazel_canonical_label_key(
            label_ctx.coerce_providers_label(toolchain)?.target(),
        )));
    }
    if let Some(toolchain_type) = StructRef::from_value(value).and_then(|st| {
        st.iter()
            .find_map(|(name, value)| (name.as_str() == "toolchain_type").then_some(value))
    }) {
        return bazel_toolchain_key_from_value(toolchain_type, label_ctx);
    }
    Err(RuleError::UnsupportedBazelToolchain(value.to_repr()).into())
}

fn bazel_toolchain_requirement_from_value(
    value: Value<'_>,
    label_ctx: &dyn AttrCoercionContext,
) -> buck2_error::Result<BazelToolchainRequirement> {
    let mandatory = StructRef::from_value(value)
        .and_then(|st| {
            st.iter()
                .find_map(|(name, value)| (name.as_str() == "mandatory").then_some(value))
        })
        .map(|value| {
            value.unpack_bool().ok_or_else(|| {
                buck2_error::Error::from(RuleError::UnsupportedBazelToolchain(value.to_repr()))
            })
        })
        .transpose()?
        .unwrap_or(true);
    Ok(BazelToolchainRequirement {
        toolchain_type: bazel_toolchain_key_from_value(value, label_ctx)?,
        mandatory,
    })
}

fn bazel_implicit_outputs_from_value(
    outputs: Option<Value<'_>>,
) -> buck2_error::Result<Vec<BazelImplicitOutput>> {
    let Some(outputs) = outputs else {
        return Ok(Vec::new());
    };
    if outputs.is_none() {
        return Ok(Vec::new());
    }
    let Some(outputs) = DictRef::from_value(outputs) else {
        return Err(RuleError::UnsupportedBazelOutputs(outputs.to_repr()).into());
    };

    outputs
        .iter()
        .map(|(name, template)| {
            let Some(name) = name.unpack_str() else {
                return Err(RuleError::UnsupportedBazelOutputs(name.to_repr()).into());
            };
            let Some(template) = template.unpack_str() else {
                return Err(RuleError::UnsupportedBazelOutputs(template.to_repr()).into());
            };
            Ok(BazelImplicitOutput {
                name: ArcStr::from(name),
                template: ArcStr::from(template),
            })
        })
        .collect()
}

impl<'v> AllocValue<'v> for StarlarkRuleCallable<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

impl<'v> StarlarkRuleCallable<'v> {
    fn new(
        implementation: RuleImpl<'v>,
        attrs: UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        cfg: Option<Value<'v>>,
        supports_incoming_transition: Option<bool>,
        doc: &str,
        is_configuration_rule: bool,
        is_toolchain_rule: bool,
        uses_plugins: Vec<PluginKind>,
        bazel_toolchains: Vec<BazelToolchainRequirement>,
        bazel_implicit_outputs: Vec<BazelImplicitOutput>,
        bazel_output_to_genfiles: bool,
        is_bazel_rule: bool,
        is_bazel_test_rule: bool,
        is_bazel_executable_rule: bool,
        build_setting: Option<Value<'v>>,
        bazel_initializer: Option<Value<'v>>,
        bazel_initializer_attrs: Vec<String>,
        artifact_promise_mappings: Option<ArtifactPromiseMappings<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<StarlarkRuleCallable<'v>> {
        let build_context = BuildContext::from_context(eval)?;

        let rule_path: BzlOrBxlPath = match (&build_context.additional, &implementation) {
            (PerFileTypeContext::Bzl(bzl_path), RuleImpl::BuildRule(_)) => {
                BzlOrBxlPath::Bzl(bzl_path.bzl_path.clone())
            }
            (PerFileTypeContext::Bxl(bxl_path), RuleImpl::BxlAnon(_)) => {
                BzlOrBxlPath::Bxl(bxl_path.clone())
            }
            (PerFileTypeContext::Bxl(_), RuleImpl::BuildRule(_)) => {
                return Err(RuleError::RuleNotInBzl.into());
            }
            // TODO(nero): add error for it
            (_, _) => unreachable!(
                "unreachable, since bxl.anon_rule is not registered for eval for bzl files"
            ),
        };

        let mut bazel_output_attrs = Vec::new();
        let attr_entries = attrs.entries;
        let mut bazel_attr_aspects = SmallMap::new();
        let mut bazel_computed_defaults = SmallMap::new();
        for (name, value) in &attr_entries {
            if !value.bazel_aspects().is_empty() {
                bazel_attr_aspects.insert(
                    (*name).to_owned(),
                    value.bazel_aspects().iter().copied().collect::<Vec<_>>(),
                );
            }
            if let Some(computed_default) = value.bazel_computed_default() {
                bazel_computed_defaults.insert((*name).to_owned(), computed_default.clone());
            }
        }
        let mut sorted_validated_attrs = attr_entries
            .into_iter()
            .sorted_by(|(k1, _), (k2, _)| Ord::cmp(k1, k2))
            .map(|(name, value)| {
                if name == NAME_ATTRIBUTE_FIELD {
                    Err(RuleError::InvalidParameterName(NAME_ATTRIBUTE_FIELD.to_owned()).into())
                } else {
                    if let Some(kind) = value.bazel_output_kind() {
                        bazel_output_attrs.push(BazelOutputAttr {
                            name: ArcStr::from(name),
                            kind,
                        });
                    }
                    Ok((name.to_owned(), value.clone_attribute()))
                }
            })
            .collect::<buck2_error::Result<Vec<(String, Attribute)>>>()?;
        for (name, aspects) in &bazel_attr_aspects {
            collect_bazel_aspect_hidden_attributes(name, aspects, &mut sorted_validated_attrs);
        }
        let mut bazel_aspect_toolchains = Vec::new();
        for aspects in bazel_attr_aspects.values() {
            collect_bazel_aspect_toolchains(aspects, &mut bazel_aspect_toolchains);
        }
        let toolchain_label_ctx = attr_coercion_context_for_bzl(eval)?;
        let bazel_aspect_toolchains = bazel_aspect_toolchains
            .into_iter()
            .map(|toolchain| {
                bazel_toolchain_requirement_from_value(toolchain, &toolchain_label_ctx)
            })
            .collect::<buck2_error::Result<Vec<_>>>()?;
        let is_bazel_build_setting = build_setting.is_some();
        let build_setting_attrs = bazel_build_setting_attrs(build_setting)?;
        if !build_setting_attrs.is_empty() {
            sorted_validated_attrs.extend(build_setting_attrs);
            sorted_validated_attrs.sort_by(|(a, _), (b, _)| a.cmp(b));
        }
        add_bazel_common_implicit_attrs(&mut sorted_validated_attrs)?;
        if is_bazel_test_rule {
            add_bazel_test_implicit_attrs(&mut sorted_validated_attrs)?;
        }
        if is_bazel_executable_rule {
            add_bazel_executable_implicit_attrs(&mut sorted_validated_attrs)?;
        }

        let cfg = match (cfg, supports_incoming_transition) {
            (Some(_), Some(_)) => return Err(RuleError::CfgAndSupportsIncomingTransition.into()),
            (Some(cfg), None) => {
                let transition_id = if is_bazel_rule {
                    transition_id_from_value_for_bazel_attr(cfg, eval)?
                } else {
                    transition_id_from_value(cfg)?
                };
                RuleIncomingTransition::Fixed(transition_id)
            }
            (None, Some(true)) => RuleIncomingTransition::FromAttribute,
            (None, Some(false) | None) => RuleIncomingTransition::None,
        };

        let rule_kind = match (is_configuration_rule, is_toolchain_rule) {
            (false, false) => RuleKind::Normal,
            (true, false) => RuleKind::Configuration,
            (false, true) => RuleKind::Toolchain,
            (true, true) => return Err(RuleError::IsConfigurationAndToolchain.into()),
        };

        if cfg != RuleIncomingTransition::None {
            let unsupported_rule_kind_str = match rule_kind {
                RuleKind::Normal => None,
                RuleKind::Configuration => Some("Configuration"),
                RuleKind::Toolchain => Some("Toolchain"),
            };
            if let Some(unsupported_rule_kind_str) = unsupported_rule_kind_str {
                return Err(RuleError::RuleDoesNotSupportIncomingTransition(
                    unsupported_rule_kind_str,
                )
                .into());
            }
        }

        let attributes = AttributeSpec::from(
            sorted_validated_attrs,
            artifact_promise_mappings.is_some(),
            &cfg,
            is_bazel_rule,
        )?;
        let ty = Ty::ty_function(attributes.ty_function());

        Ok(StarlarkRuleCallable {
            rule_path,
            id: RefCell::new(None),
            implementation,
            attributes,
            ty,
            cfg,
            rule_kind,
            uses_plugins,
            bazel_toolchains,
            bazel_aspect_toolchains,
            bazel_output_attrs,
            bazel_implicit_outputs,
            bazel_output_to_genfiles,
            is_bazel_rule,
            is_bazel_build_setting,
            bazel_initializer,
            bazel_initializer_attrs,
            bazel_attr_aspects,
            bazel_computed_defaults,
            docs: Some(doc.to_owned()),
            ignore_attrs_for_profiling: build_context.ignore_attrs_for_profiling,
            artifact_promise_mappings,
        })
    }

    fn new_anon_impl(
        implementation: RuleImpl<'v>,
        attrs: UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        doc: &str,
        artifact_promise_mappings: SmallMap<
            StringValue<'v>,
            StarlarkCallable<'v, (FrozenValue,), UnpackList<FrozenValue>>,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<Self> {
        Self::new(
            implementation,
            attrs,
            None,
            None,
            doc,
            false,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
            false,
            false,
            None,
            None,
            Vec::new(),
            Some(ArtifactPromiseMappings {
                mappings: artifact_promise_mappings
                    .iter()
                    .map(|(k, v)| (*k, v.0))
                    .collect::<SmallMap<_, _>>(),
            }),
            eval,
        )
    }

    fn new_anon(
        implementation: StarlarkCallable<'v, (FrozenValue,), ListType<FrozenValue>>,
        attrs: UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        doc: &str,
        artifact_promise_mappings: SmallMap<
            StringValue<'v>,
            StarlarkCallable<'v, (FrozenValue,), UnpackList<FrozenValue>>,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<Self> {
        Self::new_anon_impl(
            RuleImpl::BuildRule(implementation),
            attrs,
            doc,
            artifact_promise_mappings,
            eval,
        )
    }

    pub fn new_bxl_anon(
        implementation: StarlarkCallable<'v, (FrozenValue, FrozenValue), ListType<FrozenValue>>,
        attrs: UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        doc: &str,
        artifact_promise_mappings: SmallMap<
            StringValue<'v>,
            StarlarkCallable<'v, (FrozenValue,), UnpackList<FrozenValue>>,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> buck2_error::Result<Self> {
        Self::new_anon_impl(
            RuleImpl::BxlAnon(implementation),
            attrs,
            doc,
            artifact_promise_mappings,
            eval,
        )
    }

    fn documentation_impl(&self) -> DocItem {
        let name = self
            .id
            .borrow()
            .as_ref()
            .map_or_else(|| "unbound_rule".to_owned(), |rt| rt.name.clone());
        let parameters_spec = self.attributes.signature_with_default_value(name);
        let parameter_types = self.attributes.starlark_types();
        let parameter_docs = self.attributes.docstrings();
        let params = parameters_spec.documentation_with_default_value_formatter(
            parameter_types,
            parameter_docs,
            |v| v.as_display_no_ctx().to_string(),
        );

        let function_docs = DocFunction::from_docstring(
            DocStringKind::Starlark,
            params,
            Ty::none(),
            self.docs.as_deref(),
        );

        DocItem::Member(DocMember::Function(function_docs))
    }
}

#[starlark_value(type = "Rule")]
impl<'v> StarlarkValue<'v> for StarlarkRuleCallable<'v> {
    fn export_as(
        &self,
        variable_name: &str,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<()> {
        *self.id.borrow_mut() = Some(StarlarkRuleType {
            path: self.rule_path.clone(),
            name: variable_name.to_owned(),
        });
        Ok(())
    }

    fn invoke(
        &self,
        _me: Value<'v>,
        _args: &Arguments<'v, '_>,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Err(buck2_error::Error::from(RuleError::RuleCalledBeforeFreezing).into())
    }

    fn documentation(&self) -> DocItem {
        self.documentation_impl()
    }

    fn typechecker_ty(&self) -> Option<Ty> {
        Some(self.ty.clone())
    }

    fn get_type_starlark_repr() -> Ty {
        Ty::function(ParamSpec::kwargs(Ty::any()), Ty::none())
    }
}

#[derive(Debug, ProvidesStaticType, Allocative, Clone, Dupe)]
enum FrozenRuleImpl {
    BuildRule(FrozenStarlarkCallable<(FrozenValue,), ListType<FrozenValue>>),
    BxlAnon(FrozenStarlarkCallable<(FrozenValue, FrozenValue), ListType<FrozenValue>>),
}

impl FrozenRuleImpl {
    fn into_frozen_value(self) -> FrozenValue {
        match self {
            FrozenRuleImpl::BuildRule(callable) => callable.0,
            FrozenRuleImpl::BxlAnon(callable) => callable.0,
        }
    }
}

impl<'v> Freeze for RuleImpl<'v> {
    type Frozen = FrozenRuleImpl;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        match self {
            RuleImpl::BuildRule(impl_) => Ok(FrozenRuleImpl::BuildRule(impl_.freeze(freezer)?)),
            RuleImpl::BxlAnon(impl_) => Ok(FrozenRuleImpl::BxlAnon(impl_.freeze(freezer)?)),
        }
    }
}

impl<'v> Freeze for StarlarkRuleCallable<'v> {
    type Frozen = FrozenStarlarkRuleCallable;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let frozen_impl = self.implementation.freeze(freezer)?;
        let rule_docs = self.documentation_impl();
        let id = match self.id.into_inner() {
            Some(x) => x,
            None => {
                return Err(FreezeError::new(
                    RuleError::RuleNotAssigned(self.rule_path).to_string(),
                ));
            }
        };
        let rule_type = Arc::new(id);
        let rule_name = rule_type.name.to_owned();

        // For StarlarkRuleCallable, it doesn't rely on `signature` to get the default value, instead we get the default value from `Rule.attributes`,
        // so use `signature(rule_name)` method here.
        // TODO(nero): It need to some refactor to make it more clear, e.g. add a new type `ParametersSpec<NoDefaults>` here.
        let signature = self.attributes.signature(rule_name).freeze(freezer)?;

        let artifact_promise_mappings = match self.artifact_promise_mappings {
            Some(artifacts) => {
                let mut mappings = SmallMap::new();
                for (name, implementation) in artifacts.mappings {
                    mappings.insert(name.freeze(freezer)?, implementation.freeze(freezer)?);
                }
                Some(FrozenArtifactPromiseMappings { mappings })
            }
            None => None,
        };
        let bazel_initializer = match self.bazel_initializer {
            Some(initializer) => Some(initializer.freeze(freezer)?),
            None => None,
        };
        let bazel_attr_aspects = self
            .bazel_attr_aspects
            .into_iter()
            .map(|(name, aspects)| {
                let aspects = aspects
                    .into_iter()
                    .map(|aspect| aspect.freeze(freezer))
                    .collect::<FreezeResult<Vec<_>>>()?;
                Ok((name, aspects))
            })
            .collect::<FreezeResult<SmallMap<_, _>>>()?;
        let bazel_computed_defaults = self
            .bazel_computed_defaults
            .into_iter()
            .map(|(name, computed_default)| Ok((name, computed_default.freeze(freezer)?)))
            .collect::<FreezeResult<SmallMap<_, _>>>()?;

        Ok(FrozenStarlarkRuleCallable {
            rule: Arc::new(Rule {
                attributes: self.attributes,
                rule_type: RuleType::Starlark(rule_type.dupe()),
                cfg: self.cfg,
                rule_kind: self.rule_kind,
                uses_plugins: self.uses_plugins,
                bazel_toolchains: self.bazel_toolchains,
                bazel_aspect_toolchains: self.bazel_aspect_toolchains,
                bazel_output_attrs: self.bazel_output_attrs,
                bazel_implicit_outputs: self.bazel_implicit_outputs,
                bazel_output_to_genfiles: self.bazel_output_to_genfiles,
                is_bazel_rule: self.is_bazel_rule,
                is_bazel_build_setting: self.is_bazel_build_setting,
            }),
            rule_type,
            implementation: frozen_impl,
            signature,
            rule_docs,
            ty: self.ty,
            ignore_attrs_for_profiling: self.ignore_attrs_for_profiling,
            artifact_promise_mappings,
            bazel_initializer,
            bazel_initializer_attrs: self.bazel_initializer_attrs,
            bazel_attr_aspects,
            bazel_computed_defaults,
        })
    }
}

#[derive(Debug, Display, ProvidesStaticType, NoSerialize, Allocative)]
#[display("<rule {}>", rule.rule_type.name())]
pub struct FrozenStarlarkRuleCallable {
    rule: Arc<Rule>,
    /// Identical to `rule.rule_type` but more specific type.
    rule_type: Arc<StarlarkRuleType>,
    implementation: FrozenRuleImpl,
    /// We don't need rely on `signature` to get the default value here, instead we get the default
    /// value from `Rule.attributes`. So use in the ParametersSpecNoDefaults for more clarity
    signature: ParametersSpec<FrozenValue>,
    rule_docs: DocItem,
    ty: Ty,
    ignore_attrs_for_profiling: bool,
    artifact_promise_mappings: Option<FrozenArtifactPromiseMappings>,
    bazel_initializer: Option<FrozenValue>,
    bazel_initializer_attrs: Vec<String>,
    bazel_attr_aspects: SmallMap<String, Vec<FrozenValue>>,
    bazel_computed_defaults: SmallMap<String, FrozenBazelComputedDefault>,
}
starlark_simple_value!(FrozenStarlarkRuleCallable);

fn unpack_frozen_rule(
    rule: FrozenValue,
) -> buck2_error::Result<FrozenValueTyped<'static, FrozenStarlarkRuleCallable>> {
    FrozenValueTyped::new(rule).ok_or_else(|| internal_error!("Expecting FrozenRuleCallable"))
}

pub(crate) fn init_frozen_rule_get_impl() {
    FROZEN_RULE_GET_IMPL.init(|rule| {
        let rule = unpack_frozen_rule(rule)?;
        Ok(rule.implementation.dupe().into_frozen_value())
    })
}

pub(crate) fn init_frozen_promise_artifact_mappings_get_impl() {
    FROZEN_PROMISE_ARTIFACT_MAPPINGS_GET_IMPL.init(|rule| {
        let rule = unpack_frozen_rule(rule)?;
        Ok(rule
            .artifact_promise_mappings
            .as_ref()
            .map_or_else(SmallMap::new, |m| m.mappings.clone()))
    })
}

pub(crate) fn init_frozen_bazel_aspects_get_impl() {
    FROZEN_BAZEL_ASPECTS_GET_IMPL.init(|rule| {
        let rule = unpack_frozen_rule(rule)?;
        let mut aspects = Vec::new();
        for attr_aspects in rule.bazel_attr_aspects().values() {
            for aspect in attr_aspects {
                let Some(implementation) = frozen_aspect_implementation(*aspect) else {
                    continue;
                };
                if !aspects.contains(&implementation) {
                    aspects.push(implementation);
                }
            }
        }
        Ok(aspects)
    });
    FROZEN_BAZEL_ATTR_ASPECTS_GET_IMPL.init(|rule| {
        let rule = unpack_frozen_rule(rule)?;
        Ok(rule.bazel_attr_aspects().clone())
    });
    FROZEN_BAZEL_ASPECT_INFO_GET_IMPL.init(frozen_aspect_info)
}

impl FrozenStarlarkRuleCallable {
    pub fn rule_type(&self) -> &Arc<StarlarkRuleType> {
        &self.rule_type
    }

    pub fn attributes(&self) -> &AttributeSpec {
        &self.rule.attributes
    }

    pub fn artifact_promise_mappings(&self) -> &Option<FrozenArtifactPromiseMappings> {
        &self.artifact_promise_mappings
    }

    pub fn bazel_attr_aspects(&self) -> &SmallMap<String, Vec<FrozenValue>> {
        &self.bazel_attr_aspects
    }

    fn named_value<'v>(
        named: &SmallMap<StringValue<'v>, Value<'v>>,
        key: &str,
    ) -> Option<Value<'v>> {
        named
            .iter()
            .find_map(|(name, value)| (name.as_str() == key).then_some(*value))
    }

    fn insert_named_value<'v>(
        named: &mut SmallMap<StringValue<'v>, Value<'v>>,
        key: &str,
        value: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) {
        let mut updated = SmallMap::with_capacity(named.len() + usize::from(value.is_some()));
        let mut found = false;
        for (existing_key, existing_value) in std::mem::take(named) {
            if existing_key.as_str() == key {
                found = true;
                if let Some(value) = value {
                    updated.insert(existing_key, value);
                }
            } else {
                updated.insert(existing_key, existing_value);
            }
        }
        if !found && let Some(value) = value {
            updated.insert(eval.heap().alloc_str(key), value);
        }
        *named = updated;
    }

    fn apply_bazel_initializer<'v>(
        &self,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<SmallMap<StringValue<'v>, Value<'v>>> {
        if args.positions(eval.heap())?.next().is_some() {
            return Err(buck2_error::Error::from(RuleError::BazelInitializerPositionalArgs).into());
        }

        let mut named = args.names_map()?;
        let Some(initializer) = self.bazel_initializer else {
            return Ok(named);
        };

        let mut initializer_kwargs_owned = Vec::new();
        if let Some(value) = Self::named_value(&named, NAME_ATTRIBUTE_FIELD)
            && !value.is_none()
        {
            initializer_kwargs_owned.push((NAME_ATTRIBUTE_FIELD.to_owned(), value));
        }
        for attr_name in &self.bazel_initializer_attrs {
            if let Some(value) = Self::named_value(&named, attr_name)
                && !value.is_none()
            {
                initializer_kwargs_owned.push((attr_name.clone(), value));
            }
        }
        let initializer_kwargs = initializer_kwargs_owned
            .iter()
            .map(|(name, value)| (name.as_str(), *value))
            .collect::<Vec<_>>();
        let initialized = eval.eval_function(initializer.to_value(), &[], &initializer_kwargs)?;
        if initialized.is_none() {
            return Ok(named);
        }
        let initialized = DictRef::from_value(initialized).ok_or_else(|| {
            buck2_error::Error::from(RuleError::InvalidBazelInitializerReturn(
                initialized.get_type().to_owned(),
            ))
        })?;

        for (key, value) in initialized.iter() {
            let Some(key) = key.unpack_str() else {
                return Err(
                    buck2_error::Error::from(RuleError::InvalidBazelInitializerReturnKey(
                        key.get_type().to_owned(),
                    ))
                    .into(),
                );
            };
            if key == NAME_ATTRIBUTE_FIELD {
                if Self::named_value(&named, NAME_ATTRIBUTE_FIELD)
                    .and_then(|name| name.unpack_str())
                    != value.unpack_str()
                {
                    return Err(
                        buck2_error::Error::from(RuleError::BazelInitializerChangedName).into(),
                    );
                }
                continue;
            }
            Self::insert_named_value(&mut named, key, (!value.is_none()).then_some(value), eval);
        }

        Ok(named)
    }
}

#[starlark_value(type = "Rule")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkRuleCallable {
    type Canonical = StarlarkRuleCallable<'v>;

    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let record_target_call_stack =
            ModuleInternals::from_context(eval, self.rule.rule_type.name())?
                .record_target_call_stacks();
        let call_stack = if record_target_call_stack {
            Some(eval.call_stack())
        } else {
            None
        };
        if self.bazel_initializer.is_some() || !self.bazel_computed_defaults.is_empty() {
            if self.bazel_initializer.is_none() && args.positions(eval.heap())?.next().is_some() {
                return Err(
                    buck2_error::Error::from(RuleError::BazelInitializerPositionalArgs).into(),
                );
            }
            let named = self.apply_bazel_initializer(args, eval)?;
            let internals = ModuleInternals::from_context(eval, self.rule.rule_type.name())?
                as *const ModuleInternals;
            // Computed defaults are evaluated while the BUILD-file evaluator is active. The
            // ModuleInternals pointer is stored in the evaluator's stable extra state; Rust cannot
            // express borrowing that state while also calling back into the evaluator.
            let target_node = unsafe {
                TargetNode::from_named_values_with_bazel_computed_defaults(
                    self.rule.dupe(),
                    (&*internals).package(),
                    &*internals,
                    &named,
                    &self.bazel_computed_defaults,
                    eval,
                    self.ignore_attrs_for_profiling,
                    call_stack,
                )?
            };
            let internals = unsafe { &*internals };
            let output_file_targets = bazel_output_file_targets(&target_node, internals)?;
            internals.record(target_node)?;
            for output_file_target in output_file_targets {
                internals.record(output_file_target)?;
            }
            return Ok(Value::new_none());
        }
        let arg_count = args.len()?;
        self.signature.parser(args, eval, |param_parser, eval| {
            // The body of the callable returned by `rule()`.
            // Records the target in this package's `TargetMap`.
            let internals = ModuleInternals::from_context(eval, self.rule.rule_type.name())?;
            let target_node = TargetNode::from_params(
                self.rule.dupe(),
                internals.package(),
                internals,
                param_parser,
                arg_count,
                self.ignore_attrs_for_profiling,
                call_stack,
            )?;
            let output_file_targets = bazel_output_file_targets(&target_node, internals)?;
            internals.record(target_node)?;
            for output_file_target in output_file_targets {
                internals.record(output_file_target)?;
            }
            Ok(Value::new_none())
        })
    }

    fn documentation(&self) -> DocItem {
        self.rule_docs.clone()
    }

    fn typechecker_ty(&self) -> Option<Ty> {
        Some(self.ty.clone())
    }

    fn get_type_starlark_repr() -> Ty {
        StarlarkRuleCallable::get_type_starlark_repr()
    }
}

fn bazel_macro_default_none_attrs_from_spec(spec: &AttributeSpec) -> Vec<String> {
    spec.attr_specs()
        .filter_map(|(name, _, attr)| {
            if name == NAME_ATTRIBUTE_FIELD || name == "visibility" || name.starts_with('_') {
                return None;
            }
            attr.default().is_some().then(|| name.to_owned())
        })
        .collect()
}

fn bazel_macro_default_none_attrs(inherit_attrs: Option<Value<'_>>) -> Vec<String> {
    let Some(inherit_attrs) = inherit_attrs else {
        return Vec::new();
    };
    if inherit_attrs.is_none() {
        return Vec::new();
    }
    if let Some(rule) = inherit_attrs.downcast_ref::<StarlarkRuleCallable>() {
        return bazel_macro_default_none_attrs_from_spec(&rule.attributes);
    }
    if let Some(rule) = inherit_attrs.downcast_ref::<FrozenStarlarkRuleCallable>() {
        return bazel_macro_default_none_attrs_from_spec(rule.attributes());
    }
    Vec::new()
}

#[starlark_module]
#[starlark_types(StarlarkRuleCallable<'_> as Rule)]
pub fn register_rule_function(builder: &mut GlobalsBuilder) {
    /// Define a Bazel symbolic macro. The returned callable currently preserves the implementation
    /// function so macro definitions can be loaded and invoked through Buck's existing package
    /// loading path while broader symbolic macro metadata is added.
    fn r#macro<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] inherit_attrs: Option<Value<'v>>,
        #[starlark(require = named, default = false)] finalizer: bool,
        #[starlark(require = named)] doc: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let _ = (attrs, finalizer, doc);
        Ok(eval.heap().alloc(StarlarkMacroCallable {
            implementation,
            default_none_attrs: bazel_macro_default_none_attrs(inherit_attrs),
        }))
    }

    /// Define a Bazel subrule.
    fn subrule<'v>(
        #[starlark(require = named)] implementation: StarlarkCallable<'v, (), Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        attrs: UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        #[starlark(require = named)] fragments: Option<Value<'v>>,
        #[starlark(require = named)] toolchains: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        subrules: UnpackListOrTuple<Value<'v>>,
    ) -> starlark::Result<StarlarkSubrule<'v>> {
        let _ = (fragments, toolchains, subrules);
        Ok(StarlarkSubrule {
            implementation: implementation.0,
            attr_names: attrs
                .entries
                .into_iter()
                .map(|(name, _)| name.to_owned())
                .collect(),
        })
    }

    /// Define a Bazel execution group for rules that declare per-action toolchain sets.
    fn exec_group<'v>(
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        toolchains: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        exec_compatible_with: UnpackListOrTuple<Value<'v>>,
    ) -> starlark::Result<StarlarkExecGroup> {
        Ok(StarlarkExecGroup {
            toolchains: toolchains.items.into_iter().map(|v| v.to_repr()).collect(),
            exec_compatible_with: exec_compatible_with
                .items
                .into_iter()
                .map(|v| v.to_repr())
                .collect(),
        })
    }

    /// Define a rule. As a simple example:
    ///
    /// ```python
    /// def _my_rule(ctx: AnalysisContext) -> list[Provider]:
    ///     output = ctx.actions.write("hello.txt", ctx.attrs.contents, executable = ctx.attrs.exe)
    ///     return [DefaultInfo(outputs = [output])]
    ///
    /// MyRule = rule(impl = _my_rule, attrs = {
    ///     "contents": attrs.string(),
    ///     "exe": attrs.option(attrs.bool(), default = False),
    /// })
    /// ```
    fn rule<'v>(
        r#impl: Option<
            StarlarkCallableChecked<
                'v,
                (AnalysisContextReprLate,),
                Either<ListType<ProviderReprLate>, StarlarkPromise<'v>>,
            >,
        >,
        #[starlark(require = named)] implementation: Option<
            StarlarkCallableChecked<
                'v,
                (AnalysisContextReprLate,),
                Either<ListType<ProviderReprLate>, StarlarkPromise<'v>>,
            >,
        >,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        attrs: UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named)] supports_incoming_transition: Option<bool>,
        #[starlark(require = named, default = "")] doc: &str,
        #[starlark(require = named, default = false)] is_configuration_rule: bool,
        #[starlark(require = named, default = false)] is_toolchain_rule: bool,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named)] test: Option<bool>,
        #[starlark(require = named)] outputs: Option<Value<'v>>,
        #[starlark(require = named, default = false)] output_to_genfiles: bool,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        fragments: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        host_fragments: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = false)] _skylark_testable: bool,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        toolchains: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        provides: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = false)] dependency_resolution_rule: bool,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        exec_compatible_with: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = false)] analysis_test: bool,
        #[starlark(require = named)] build_setting: Option<Value<'v>>,
        #[starlark(require = named)] exec_groups: Option<Value<'v>>,
        #[starlark(require = named)] initializer: Option<Value<'v>>,
        #[starlark(require = named)] parent: Option<Value<'v>>,
        #[starlark(require = named)] extendable: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        subrules: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        uses_plugins: UnpackListOrTuple<PluginKindArg>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkRuleCallable<'v>> {
        let has_bazel_attrs = attrs.entries.iter().any(|(_, attr)| attr.is_bazel());
        let bazel_initializer_attrs = attrs
            .entries
            .iter()
            .filter_map(|(name, _)| (!name.starts_with('_')).then_some((*name).to_owned()))
            .collect::<Vec<_>>();
        let toolchain_label_ctx = attr_coercion_context_for_bzl(eval)?;
        let bazel_toolchains = toolchains
            .items
            .into_iter()
            .map(|toolchain| {
                bazel_toolchain_requirement_from_value(toolchain, &toolchain_label_ctx)
            })
            .collect::<buck2_error::Result<Vec<_>>>()?;
        let bazel_implicit_outputs = bazel_implicit_outputs_from_value(outputs)?;

        let _unused = (
            fragments,
            host_fragments,
            _skylark_testable,
            provides,
            dependency_resolution_rule,
            exec_compatible_with,
            analysis_test,
            exec_groups,
            parent,
            extendable,
            subrules,
        );
        let has_bazel_rule_options = has_bazel_attrs
            || !bazel_toolchains.is_empty()
            || !bazel_implicit_outputs.is_empty()
            || output_to_genfiles
            || executable
            || test.is_some()
            || build_setting.is_some()
            || initializer
                .as_ref()
                .is_some_and(|initializer| !initializer.is_none());
        let (implementation, is_bazel_rule) = match (r#impl, implementation) {
            (Some(r#impl), None) => (r#impl, has_bazel_rule_options),
            (None, Some(implementation)) => (implementation, true),
            _ => {
                return Err(buck2_error::Error::from(
                    RuleError::MissingOrConflictingImplementation,
                )
                .into());
            }
        };
        Ok(StarlarkRuleCallable::new(
            RuleImpl::BuildRule(StarlarkCallable::unchecked_new(implementation.0)),
            attrs,
            cfg.filter(|cfg| !cfg.is_none()),
            supports_incoming_transition,
            doc,
            is_configuration_rule,
            is_toolchain_rule,
            uses_plugins
                .items
                .into_iter()
                .map(|PluginKindArg { plugin_kind }| plugin_kind)
                .collect(),
            bazel_toolchains,
            bazel_implicit_outputs,
            is_bazel_rule && output_to_genfiles,
            is_bazel_rule,
            is_bazel_rule && test.unwrap_or(false),
            is_bazel_rule && executable,
            build_setting,
            initializer.filter(|initializer| !initializer.is_none()),
            bazel_initializer_attrs,
            None,
            eval,
        )?)
    }

    /// Define an anon rule, similar to how a normal rule is defined, except with an extra `artifact_promise_mappings` field. This
    /// is a dict where the keys are the string name of the artifact, and the values are the callable functions that produce
    /// the artifact. This is only intended to be used with anon targets.
    fn anon_rule<'v>(
        #[starlark(require = named)] r#impl: StarlarkCallable<
            'v,
            (FrozenValue,),
            ListType<FrozenValue>,
        >,
        #[starlark(require = named)] attrs: UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        #[starlark(require = named)] artifact_promise_mappings: SmallMap<
            StringValue<'v>,
            StarlarkCallable<'v, (FrozenValue,), UnpackList<FrozenValue>>,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkRuleCallable<'v>> {
        StarlarkRuleCallable::new_anon(r#impl, attrs, doc, artifact_promise_mappings, eval)
            .map_err(Into::into)
    }
}
