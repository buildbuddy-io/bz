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

use bz_common::package_listing::listing::PackageListing;
use bz_core::cells::cell_path_with_allowed_relative_dir::CellPathWithAllowedRelativeDir;
use bz_core::cells::external::is_bzlmod_cell_name;
use bz_core::cells::name::CellName;
use bz_core::configuration::transition::id::TransitionId;
use bz_core::package::PackageLabel;
use bz_core::plugins::PluginKindSet;
use bz_core::target::label::interner::ConcurrentTargetLabelInterner;
use bz_error::BuckErrorContext;
use bz_fs::paths::file_name::FileNameBuf;
use bz_interpreter::coerce::COERCE_PROVIDERS_LABEL_FOR_BZL;
use bz_interpreter::types::provider::callable::ValueAsProviderCallableLike;
use bz_interpreter::types::transition::transition_id_from_value;
use bz_interpreter::types::transition::transition_id_from_value_for_bazel_attr;
use bz_node::attrs::attr::Attribute;
use bz_node::attrs::attr::AttributeAllowedValues;
use bz_node::attrs::attr_type::AttrType;
use bz_node::attrs::attr_type::any::AnyAttrType;
use bz_node::attrs::attr_type::bazel::label::BazelAllowedFileTypes;
use bz_node::attrs::coercion_context::AttrCoercionContext;
use bz_node::attrs::configurable::AttrIsConfigurable;
use bz_node::attrs::display::AttrDisplayWithContextExt;
use bz_node::provider_id_set::ProviderIdSet;
use bz_node::rule::BazelOutputAttrKind;
use dupe::Dupe;
use dupe::OptionDupedExt;
use either::Either;
use gazebo::prelude::*;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::StringValue;
use starlark::values::Value;
use starlark::values::ValueOf;
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::tuple::TupleRef;
use starlark::values::tuple::UnpackTuple;

use crate::attrs::coerce::attr_type::AttrTypeExt;
use crate::attrs::coerce::ctx::BuildAttrCoercionContext;
use crate::attrs::starlark_attribute::BazelComputedDefault;
use crate::attrs::starlark_attribute::StarlarkAttribute;
use crate::attrs::starlark_attribute::register_attr_type;
use crate::bazel::config::bazel_exec_transition_from_value;
use crate::bazel::configuration_field::BazelConfigurationField;
use crate::interpreter::build_context::BuildContext;
use crate::interpreter::build_context::PerFileTypeContext;
use crate::interpreter::selector::StarlarkSelector;
use crate::plugins::AllPlugins;
use crate::plugins::PluginKindArg;

const OPTION_NONE_EXPLANATION: &str = "`None` as an attribute value always picks the default. For `attrs.option`, if the default isn't `None`, there is no way to express `None`.";

#[derive(bz_error::Error, Debug)]
#[buck2(input)]
enum AttrError {
    #[error(
        "`attrs.option` `default` parameter must be `None` or absent, got `{0}`.\n{}",
        OPTION_NONE_EXPLANATION
    )]
    OptionDefaultNone(String),
    #[error("`attrs.default_only` argument must have a default")]
    DefaultOnlyMustHaveDefault,
    #[error("unsupported Bazel attr cfg string `{0}`")]
    UnsupportedBazelAttrCfg(String),
    #[error("providers argument contains non-provider value `{0}`")]
    InvalidProviderValue(String),
    #[error("providers argument cannot mix provider values with provider lists")]
    InvalidProviderListShape,
    #[error("`{0}` must be a bool or a list of file extensions, got `{1}`")]
    InvalidBazelAllowFilesValue(&'static str, String),
    #[error("`{0}` extension entries must be strings, got `{1}`")]
    InvalidBazelAllowFilesExtension(&'static str, String),
}

pub(crate) trait AttributeExt {
    /// Helper to create an attribute from attrs.foo functions
    fn attr<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        default: Option<Value<'v>>,
        doc: &str,
        coercer: AttrType,
    ) -> bz_error::Result<StarlarkAttribute<'v>>;
}

impl AttributeExt for Attribute {
    /// Helper to create an attribute from attrs.foo functions
    fn attr<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        default: Option<Value<'v>>,
        doc: &str,
        coercer: AttrType,
    ) -> bz_error::Result<StarlarkAttribute<'v>> {
        let default = match default {
            None => None,
            Some(x) => Some(Arc::new(
                coercer
                    .coerce(
                        AttrIsConfigurable::Yes,
                        &attr_coercion_context_for_bzl(eval)?,
                        x,
                    )
                    .buck_error_context("Error coercing attribute default")?,
            )),
        };
        Ok(StarlarkAttribute::new(Attribute::new(
            default, doc, coercer,
        )?))
    }
}

fn bazel_configurable(configurable: Option<bool>) -> AttrIsConfigurable {
    match configurable {
        Some(false) => AttrIsConfigurable::No,
        Some(true) | None => AttrIsConfigurable::Yes,
    }
}

fn bazel_doc(doc: NoneOr<&str>) -> &str {
    doc.into_option().unwrap_or("")
}

fn bazel_allowed_values<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    values: Vec<Value<'v>>,
    coercer: &AttrType,
) -> bz_error::Result<Option<AttributeAllowedValues>> {
    let ctx = attr_coercion_context_for_bzl(eval)?;
    let values = values
        .into_try_map(|value| coercer.coerce(AttrIsConfigurable::No, &ctx, value))
        .buck_error_context("Error coercing Bazel allowed attribute values")?;
    Ok(AttributeAllowedValues::new(values))
}

fn bazel_attr<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    default: Option<Value<'v>>,
    fallback: Value<'v>,
    mandatory: bool,
    doc: &str,
    coercer: AttrType,
    configurable: Option<bool>,
) -> bz_error::Result<StarlarkAttribute<'v>> {
    bazel_attr_with_allowed_values(
        eval,
        default,
        fallback,
        mandatory,
        doc,
        coercer,
        configurable,
        None,
    )
}

fn bazel_attr_with_allowed_values<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    default: Option<Value<'v>>,
    fallback: Value<'v>,
    mandatory: bool,
    doc: &str,
    coercer: AttrType,
    configurable: Option<bool>,
    allowed_values: Option<AttributeAllowedValues>,
) -> bz_error::Result<StarlarkAttribute<'v>> {
    bazel_attr_with_allowed_values_and_aspects(
        eval,
        default,
        fallback,
        mandatory,
        doc,
        coercer,
        configurable,
        allowed_values,
        Vec::new(),
    )
}

fn bazel_attr_with_allowed_values_and_aspects<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    default: Option<Value<'v>>,
    fallback: Value<'v>,
    mandatory: bool,
    doc: &str,
    coercer: AttrType,
    configurable: Option<bool>,
    allowed_values: Option<AttributeAllowedValues>,
    bazel_aspects: Vec<Value<'v>>,
) -> bz_error::Result<StarlarkAttribute<'v>> {
    let computed_default = match default {
        Some(default) => BazelComputedDefault::from_value(default)?,
        None => None,
    };
    let default = if mandatory {
        None
    } else {
        Some(match default {
            Some(_) if computed_default.is_some() => fallback,
            Some(default) => default,
            None => fallback,
        })
    };
    let default = match default {
        None => None,
        Some(x) => Some(Arc::new(
            coercer
                .coerce(
                    bazel_configurable(configurable),
                    &attr_coercion_context_for_bzl(eval)?,
                    x,
                )
                .buck_error_context("Error coercing Bazel attribute default")?,
        )),
    };
    Ok(StarlarkAttribute::new_bazel_with_aspects_and_computed(
        Attribute::new_with_allowed_values(default, doc, coercer, allowed_values)?,
        bazel_aspects,
        computed_default,
    ))
}

fn bazel_label_default<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    default: Option<Value<'v>>,
) -> bz_error::Result<Option<Value<'v>>> {
    let Some(default) = default else {
        return Ok(None);
    };
    let Some(configuration_field) = BazelConfigurationField::from_value(default) else {
        return Ok(Some(default));
    };

    if configuration_field.fragment() == "coverage"
        && configuration_field.name() == "output_generator"
    {
        // Bazel resolves this late-bound label to None outside `bazel coverage`; Buck2 does
        // not have a Bazel coverage command mode yet, so normal builds use that value.
        return Ok(Some(Value::new_none()));
    }

    let label = match (configuration_field.fragment(), configuration_field.name()) {
        ("apple", "xcode_config_label") => Some("@bazel_tools//tools/cpp:host_xcodes"),
        ("proto", "proto_compiler") => Some("@bazel_tools//tools/proto:protoc"),
        ("proto", "proto_toolchain_for_java") => Some("@bazel_tools//tools/proto:java_toolchain"),
        ("proto", "proto_toolchain_for_java_lite") => {
            Some("@bazel_tools//tools/proto:javalite_toolchain")
        }
        ("proto", "proto_toolchain_for_cc") => Some("@bazel_tools//tools/proto:cc_toolchain"),
        _ => None,
    };
    if let Some(label) = label {
        return Ok(Some(eval.heap().alloc(label)));
    }

    // Buck2 does not currently model Bazel fragment option values. For label attrs,
    // unresolved late-bound defaults behave like an unset Bazel configuration field.
    Ok(Some(Value::new_none()))
}

fn bazel_label_list_default<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    default: Option<Value<'v>>,
) -> bz_error::Result<Option<Value<'v>>> {
    let Some(default) = default else {
        return Ok(None);
    };
    if BazelConfigurationField::from_value(default).is_none() {
        return Ok(Some(default));
    }

    // Buck2 does not currently model Bazel fragment option values. For label-list attrs,
    // unresolved late-bound defaults behave like an unset Bazel configuration field.
    Ok(Some(eval.heap().alloc(AllocList::EMPTY)))
}

fn bazel_allowed_file_types_from_value(
    param: &'static str,
    value: Option<Value>,
) -> bz_error::Result<BazelAllowedFileTypes> {
    let Some(value) = value else {
        return Ok(BazelAllowedFileTypes::None);
    };
    if let Some(value) = value.unpack_bool() {
        return Ok(if value {
            BazelAllowedFileTypes::Any
        } else {
            BazelAllowedFileTypes::None
        });
    }

    let values: Vec<Value> = if let Some(list) = ListRef::from_value(value) {
        list.iter().collect()
    } else if let Some(tuple) = TupleRef::from_value(value) {
        tuple.iter().collect()
    } else {
        return Err(AttrError::InvalidBazelAllowFilesValue(param, value.to_repr()).into());
    };

    let mut extensions = values
        .into_iter()
        .map(|value| {
            value.unpack_str().map(str::to_owned).ok_or_else(|| {
                AttrError::InvalidBazelAllowFilesExtension(param, value.to_repr()).into()
            })
        })
        .collect::<bz_error::Result<Vec<_>>>()?;
    extensions.sort();
    extensions.dedup();
    Ok(if extensions.is_empty() {
        BazelAllowedFileTypes::None
    } else {
        BazelAllowedFileTypes::Extensions(extensions.into_boxed_slice())
    })
}

fn bazel_allowed_file_types(
    allow_files: Option<Value>,
    allow_single_file: Option<Value>,
) -> bz_error::Result<BazelAllowedFileTypes> {
    Ok(
        bazel_allowed_file_types_from_value("allow_files", allow_files)?.combine(
            bazel_allowed_file_types_from_value("allow_single_file", allow_single_file)?,
        ),
    )
}

fn bazel_dep_attr_type<'v>(
    required_providers: ProviderIdSet,
    cfg: Option<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> bz_error::Result<AttrType> {
    match cfg {
        None => Ok(AttrType::dep(required_providers, PluginKindSet::EMPTY)),
        Some(cfg) if cfg.is_none() => Ok(AttrType::dep(required_providers, PluginKindSet::EMPTY)),
        Some(cfg) if bazel_exec_transition_from_value(cfg).is_some() => {
            Ok(AttrType::exec_dep(required_providers))
        }
        Some(cfg) => match cfg.unpack_str() {
            Some("target") => Ok(AttrType::dep(required_providers, PluginKindSet::EMPTY)),
            Some("exec") | Some("host") => Ok(AttrType::exec_dep(required_providers)),
            Some(other) => Err(AttrError::UnsupportedBazelAttrCfg(other.to_owned()).into()),
            None => Ok(AttrType::split_transition_dep(
                required_providers,
                Arc::new(TransitionId::BazelAttribute(
                    transition_id_from_value_for_bazel_attr(cfg, eval)?,
                )),
            )),
        },
    }
}

fn bazel_label_attr_type<'v>(
    providers: UnpackListOrTuple<Value<'v>>,
    allow_files: Option<Value<'v>>,
    allow_single_file: Option<Value<'v>>,
    cfg: Option<Value<'v>>,
    list: bool,
    eval: &mut Evaluator<'v, '_, '_>,
) -> bz_error::Result<AttrType> {
    let dep = bazel_dep_attr_type(
        dep_like_attr_handle_providers_arg(providers.items)?,
        cfg,
        eval,
    )?;
    let allowed_files = bazel_allowed_file_types(allow_files, allow_single_file)?;
    let inner = if allowed_files.allows_files() {
        AttrType::bazel_label(dep, AttrType::source(true), allowed_files)
    } else {
        dep
    };
    Ok(if list { AttrType::list(inner) } else { inner })
}

/// Coerction context for evaluating bzl files (attr default, transition rules).
pub(crate) fn attr_coercion_context_for_bzl<'v>(
    eval: &Evaluator<'v, '_, '_>,
) -> bz_error::Result<BuildAttrCoercionContext> {
    let build_context = BuildContext::from_context(eval)?;
    let global_label_interner = Arc::new(ConcurrentTargetLabelInterner::default());
    if let PerFileTypeContext::Bzl(bzl) = &build_context.additional {
        let bzl_cell = bzl.bzl_path.cell();
        let root_bazel_compat = bzl_cell.as_str() == "root"
            && build_context
                .cell_info()
                .cell_resolver()
                .get(CellName::unchecked_new("bazel_tools")?)
                .is_ok();
        if bzl_cell.as_str() == "bazel_tools"
            || is_bzlmod_cell_name(bzl_cell.as_str())
            || root_bazel_compat
        {
            let package = PackageLabel::from_cell_path(bzl.bzl_path.path_parent())?;
            return Ok(BuildAttrCoercionContext::new_with_package(
                build_context.cell_info().cell_resolver().dupe(),
                build_context.cell_info().cell_alias_resolver().dupe(),
                (
                    package.dupe(),
                    PackageListing::empty(FileNameBuf::unchecked_new("BUILD.bazel")),
                ),
                false,
                global_label_interner,
                CellPathWithAllowedRelativeDir::backwards_relative_not_supported(
                    package.to_cell_path(),
                ),
            ));
        }
    }
    Ok(BuildAttrCoercionContext::new_no_package(
        build_context.cell_info().cell_resolver().dupe(),
        build_context.cell_info().name().name(),
        build_context.cell_info().cell_alias_resolver().dupe(),
        // It is OK to not deduplicate because we don't coerce a lot of labels in bzl files.
        global_label_interner,
    ))
}

pub(crate) fn init_coerce_providers_label_for_bzl() {
    COERCE_PROVIDERS_LABEL_FOR_BZL
        .init(|eval, value| attr_coercion_context_for_bzl(eval)?.coerce_providers_label(value))
}

/// Common code to handle `providers` argument of dep-like attrs.
fn dep_like_attr_handle_providers_arg(providers: Vec<Value>) -> bz_error::Result<ProviderIdSet> {
    fn provider_id_from_value(
        v: Value,
    ) -> bz_error::Result<Arc<bz_core::provider::id::ProviderId>> {
        match v.as_provider_callable() {
            Some(callable) => bz_error::Ok(callable.id()?.dupe()),
            None => Err(AttrError::InvalidProviderValue(v.to_repr()).into()),
        }
    }

    fn provider_list_from_value<'v>(v: Value<'v>) -> Option<Vec<Value<'v>>> {
        if let Some(list) = ListRef::from_value(v) {
            Some(list.iter().collect())
        } else {
            TupleRef::from_value(v).map(|tuple| tuple.iter().collect())
        }
    }

    let mut direct_providers = Vec::new();
    let mut provider_groups = Vec::new();
    for provider in providers {
        if let Some(group) = provider_list_from_value(provider) {
            provider_groups.push(
                group
                    .into_iter()
                    .map(provider_id_from_value)
                    .collect::<bz_error::Result<Vec<_>>>()?,
            );
        } else {
            direct_providers.push(provider_id_from_value(provider)?);
        }
    }

    if !direct_providers.is_empty() && !provider_groups.is_empty() {
        return Err(AttrError::InvalidProviderListShape.into());
    }
    if provider_groups.is_empty() {
        Ok(ProviderIdSet::from(direct_providers))
    } else {
        Ok(ProviderIdSet::any_of(provider_groups))
    }
}

/// This type is available as a global `attrs` symbol, to allow the definition of attributes to the `rule` function.
///
/// As an example:
///
/// ```python
/// rule(impl = _impl, attrs = {"foo": attrs.string(), "bar": attrs.int(default = 42)})
/// ```
///
/// Most attributes take at least two optional parameters:
///
/// * A `doc` parameter, which specifies documentation for the attribute.
///
/// * A `default` parameter, which if present specifies the default value for the attribute if omitted.
///   If there is no default, the user of the rule must supply that parameter.
///
/// Each attribute defines what values it accepts from the user, and which values it gives to the rule.
/// For simple types like `attrs.string` these are the same, for more complex types like `attrs.dep` these
/// are different (string from the user, dependency to the rule).
#[starlark_module]
fn attr_module(registry: &mut GlobalsBuilder) {
    /// Takes a string from the user, supplies a string to the rule.
    fn string<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named)] validate: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = validate;
        Ok(Attribute::attr(eval, default, doc, AttrType::string())?)
    }

    /// Takes a list from the user, supplies a list to the rule.
    fn list<'v>(
        #[starlark(require = pos)] inner: &StarlarkAttribute<'_>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let coercer = AttrType::list(inner.coercer_for_inner()?);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    /// Takes a target from the user, as a string, and supplies a dependency to the rule.
    /// The dependency will transition to the execution platform. Use `exec_dep` if you
    /// plan to execute things from this dependency as part of the compilation.
    fn exec_dep<'v>(
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let required_providers = dep_like_attr_handle_providers_arg(providers.items)?;
        let coercer = AttrType::exec_dep(required_providers);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    /// Takes a target from the user, as a string, and supplies a dependency to the rule.
    /// The dependency will be a toolchain dependency, meaning that its execution platform
    /// dependencies will be used to select the execution platform for this rule.
    fn toolchain_dep<'v>(
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let required_providers = dep_like_attr_handle_providers_arg(providers.items)?;
        let coercer = AttrType::toolchain_dep(required_providers);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    fn transition_dep<'v>(
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let required_providers = dep_like_attr_handle_providers_arg(providers.items)?;
        let label_coercion_ctx = attr_coercion_context_for_bzl(eval)?;

        // FIXME(JakobDegen): Use a proper unpack for this. Easier to do after deleting old API
        let transition_id = if let Some(cfg) = cfg {
            Some(if let Some(s) = StringValue::new(cfg) {
                let transition_target = label_coercion_ctx.coerce_providers_label(&s)?;
                Arc::new(TransitionId::Target(transition_target))
            } else {
                transition_id_from_value(cfg)?
            })
        } else {
            None
        };

        let coercer = AttrType::transition_dep(required_providers, transition_id);
        let coerced_default = match default {
            None => None,
            Some(default) => {
                Some(coercer.coerce(AttrIsConfigurable::Yes, &label_coercion_ctx, default)?)
            }
        };

        Ok(StarlarkAttribute::new(Attribute::new(
            coerced_default.map(Arc::new),
            doc,
            coercer,
        )?))
    }

    fn configured_dep<'v>(
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let required_providers = dep_like_attr_handle_providers_arg(providers.items)?;
        let coercer = AttrType::configured_dep(required_providers);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    fn split_transition_dep<'v>(
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] cfg: Value<'v>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let required_providers = dep_like_attr_handle_providers_arg(providers.items)?;
        let transition_id = transition_id_from_value(cfg)?;
        let coercer = AttrType::split_transition_dep(required_providers, transition_id);

        let coerced_default = match default {
            None => None,
            Some(default) => Some(coercer.coerce(
                AttrIsConfigurable::Yes,
                &attr_coercion_context_for_bzl(eval)?,
                default,
            )?),
        };

        Ok(StarlarkAttribute::new(Attribute::new(
            coerced_default.map(Arc::new),
            doc,
            coercer,
        )?))
    }

    /// Takes a target label from the user and registers it as a plugin dependency.
    ///
    /// Plugin dependencies are propagated as unconfigured target labels up the build graph,
    /// then configured as exec deps when used by a rule with `uses_plugins`. This is useful
    /// for dependencies like Rust proc macros that need to be accessible to transitive dependents.
    ///
    /// See the [`plugins`](../plugins) namespace documentation for a full explanation and examples.
    fn plugin_dep<'v>(
        #[starlark(require = named)] kind: PluginKindArg,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        Ok(Attribute::attr(
            eval,
            default,
            doc,
            AttrType::plugin_dep(kind.plugin_kind),
        )?)
    }

    /// Takes a target from the user, as a string, and supplies a dependency to the rule.
    /// A target can be specified as an absolute dependency `foo//bar:baz`, omitting the
    /// cell (`//bar:baz`) or omitting the package name (`:baz`).
    ///
    /// If supplied the `providers` argument ensures that specific providers will be present
    /// on the dependency.
    ///
    /// The `pulls_plugins` and `pulls_and_pushes_plugins` parameters control plugin propagation.
    /// See the [`plugins`](../plugins) namespace documentation for a full explanation.
    fn dep<'v>(
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        pulls_plugins: UnpackListOrTuple<PluginKindArg>,
        #[starlark(require = named, default = Either::Left(UnpackListOrTuple::default()))]
        pulls_and_pushes_plugins: Either<UnpackListOrTuple<PluginKindArg>, &'v AllPlugins>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let required_providers = dep_like_attr_handle_providers_arg(providers.items)?;
        let plugin_kinds = match pulls_and_pushes_plugins {
            Either::Right(_) => PluginKindSet::ALL,
            Either::Left(pulls_and_pushes_plugins) => {
                let pulls_and_pushes_plugins: Vec<_> = pulls_and_pushes_plugins
                    .items
                    .into_iter()
                    .map(|PluginKindArg { plugin_kind }| plugin_kind)
                    .collect();
                let pulls_plugins: Vec<_> = pulls_plugins
                    .items
                    .into_iter()
                    .map(|PluginKindArg { plugin_kind }| plugin_kind)
                    .collect();
                PluginKindSet::new(pulls_plugins, pulls_and_pushes_plugins)?
            }
        };

        let coercer = AttrType::dep(required_providers, plugin_kinds);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    /// Takes most builtin literals and passes them to the rule as a string.
    /// Discouraged, as it provides little type safety and destroys the structure.
    fn any<'v>(
        #[starlark(require = named, default = "")] doc: &str,
        #[starlark(require = named)] default: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        Ok(Attribute::attr(eval, default, doc, AttrType::any())?)
    }

    /// Takes a boolean and passes it to the rule as a boolean.
    fn bool<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        Ok(Attribute::attr(eval, default, doc, AttrType::bool())?)
    }

    /// Takes a value that may be `None` or some inner type, and passes either `None` or the
    /// value corresponding to the inner to the rule. Often used to make a rule optional:
    ///
    /// ```python
    /// attrs.option(attr.string(), default = None)
    /// ```
    fn option<'v>(
        #[starlark(require = pos)] inner: &StarlarkAttribute<'_>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let coercer = AttrType::option(inner.coercer_for_inner()?);
        let attr = Attribute::attr(eval, default, doc, coercer)?;

        match attr.default() {
            Some(default) if !default.may_return_none() => Err(bz_error::Error::from(
                AttrError::OptionDefaultNone(default.as_display_no_ctx().to_string()),
            )
            .into()),
            _ => Ok(attr),
        }
    }

    /// Rejects all values and uses the default for the inner argument.
    /// Often used to resolve dependencies, which otherwise can't be resolved inside a rule.
    ///
    /// ```python
    /// attrs.default_only(attrs.dep(default = "foo//my_package:my_target"))
    /// ```
    fn default_only<'v>(
        #[starlark(require = pos)] inner: &StarlarkAttribute<'_>,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let Some(default) = inner.default().duped() else {
            return Err(bz_error::Error::from(AttrError::DefaultOnlyMustHaveDefault).into());
        };
        Ok(StarlarkAttribute::new(Attribute::new_default_only(
            default,
            doc,
            inner.coercer_for_default_only(),
        )))
    }

    /// Takes a target (as per `deps`) and passes a `label` to the rule.
    /// Validates that the target exists, but does not introduce a dependency on it.
    fn label<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        Ok(Attribute::attr(eval, default, doc, AttrType::label())?)
    }

    /// Takes a dict from the user, supplies a dict to the rule.
    fn dict<'v>(
        // TODO(nga): require positional only for key and value.
        key: &StarlarkAttribute<'_>,
        value: &StarlarkAttribute<'_>,
        #[starlark(require = named, default = false)] sorted: bool,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let coercer = AttrType::dict(key.coercer_for_inner()?, value.coercer_for_inner()?, sorted);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    /// Takes a command line argument from the user and supplies a `cmd_args` compatible value to the rule.
    /// The argument may contain special macros such as `$(location :my_target)` or `$(exe :my_target)` which
    /// will be replaced with references to those values in the rule. Takes in an optional `anon_target_compatible`
    /// flag, which indicates whether the args can be passed into anon targets. Note that there is a slight memory
    /// hit when using this flag.
    fn arg<'v>(
        #[starlark(require = named, default = false)] json: bool,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        #[starlark(require = named, default = false)] anon_target_compatible: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = json;
        Ok(Attribute::attr(
            eval,
            default,
            doc,
            AttrType::arg(anon_target_compatible),
        )?)
    }

    /// Takes a string from one of the variants given, and gives that string to the rule.
    /// Strings are matched case-insensitively, and always passed to the rule lowercase.
    fn r#enum<'v>(
        #[starlark(require = pos)] variants: UnpackListOrTuple<String>,
        #[starlark(require = named)] default: Option<
            ValueOf<'v, Either<StringValue<'v>, ValueTypedComplex<'v, StarlarkSelector<'v>>>>,
        >,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        // Value seems to usually be a `[String]`, listing the possible values of the
        // enumeration. Unfortunately, for things like `exported_lang_preprocessor_flags`
        // it ends up being `Type` which doesn't match the data we see.
        Ok(Attribute::attr(
            eval,
            default.map(|v| v.value),
            doc,
            AttrType::enumeration(variants.items)?,
        )?)
    }

    fn configuration_label<'v>(
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        // TODO(nga): explain how this is different from `dep`.
        //   This probably meant to be similar to `label`, but not configurable.
        Ok(Attribute::attr(
            eval,
            None,
            doc,
            AttrType::dep(ProviderIdSet::EMPTY, PluginKindSet::EMPTY),
        )?)
    }

    /// Currently an alias for `attrs.string`.
    fn regex<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        Ok(Attribute::attr(eval, default, doc, AttrType::string())?)
    }

    fn set<'v>(
        #[starlark(require = pos)] value_type: &StarlarkAttribute<'_>,
        #[starlark(require = named, default = false)] sorted: bool,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = sorted;
        let coercer = AttrType::list(value_type.coercer_for_inner()?);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    fn named_set<'v>(
        #[starlark(require = pos)] value_type: &StarlarkAttribute<'_>,
        #[starlark(require = named, default = false)] sorted: bool,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let value_coercer = value_type.coercer_for_inner()?;
        let coercer = AttrType::one_of(vec![
            AttrType::dict(AttrType::string(), value_coercer.dupe(), sorted),
            AttrType::list(value_coercer),
        ]);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    /// Given a list of alternative attributes, selects the first that matches and gives that to the rule.
    fn one_of<'v>(
        #[starlark(args)] args: UnpackTuple<&StarlarkAttribute<'_>>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let coercer = AttrType::one_of(args.items.into_try_map(|arg| arg.coercer_for_inner())?);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    /// Takes a tuple of values and gives a tuple to the rule.
    fn tuple<'v>(
        #[starlark(args)] args: UnpackTuple<&StarlarkAttribute<'_>>,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let coercer = AttrType::tuple(args.items.into_try_map(|arg| arg.coercer_for_inner())?);
        Ok(Attribute::attr(eval, default, doc, coercer)?)
    }

    /// Takes an int from the user, supplies an int to the rule.
    fn int<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        Ok(Attribute::attr(eval, default, doc, AttrType::int())?)
    }

    fn query<'v>(
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        Ok(Attribute::attr(eval, None, doc, AttrType::query())?)
    }

    fn versioned<'v>(
        value_type: &StarlarkAttribute<'_>,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        // A versioned field looks like:
        // [ ({"key":"value1"}, arg), ({"key":"value2"}, arg) ]
        let element_type = AttrType::tuple(vec![
            AttrType::dict(AttrType::string(), AttrType::string(), false),
            value_type.coercer_for_inner()?,
        ]);
        let coercer = AttrType::list(element_type.dupe());

        Ok(StarlarkAttribute::new(Attribute::new(
            Some(Arc::new(AnyAttrType::empty_list())),
            doc,
            coercer,
        )?))
    }

    /// Takes a source file from the user, supplies an artifact to the rule.
    /// The source file may be specified as a literal string
    /// (representing the path within this package), or a target (which must have a
    /// `DefaultInfo` with a `default_outputs` value).
    fn source<'v>(
        #[starlark(require = named, default = false)] allow_directory: bool,
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        Ok(Attribute::attr(
            eval,
            default,
            doc,
            AttrType::source(allow_directory),
        )?)
    }
}

#[starlark_module]
fn bazel_attr_module(registry: &mut GlobalsBuilder) {
    fn string<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        values: UnpackListOrTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let coercer = AttrType::string();
        let allowed_values = bazel_allowed_values(eval, values.items, &coercer)?;
        let fallback = eval.heap().alloc("");
        Ok(bazel_attr_with_allowed_values(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            coercer,
            configurable,
            allowed_values,
        )?)
    }

    fn int<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        values: UnpackListOrTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let coercer = AttrType::int();
        let allowed_values = bazel_allowed_values(eval, values.items, &coercer)?;
        let fallback = eval.heap().alloc(0);
        Ok(bazel_attr_with_allowed_values(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            coercer,
            configurable,
            allowed_values,
        )?)
    }

    fn bool<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        Ok(bazel_attr(
            eval,
            default,
            Value::new_bool(false),
            mandatory,
            bazel_doc(doc),
            AttrType::bool(),
            configurable,
        )?)
    }

    fn string_list<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = allow_empty;
        let fallback = eval.heap().alloc(AllocList::EMPTY);
        Ok(bazel_attr(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            AttrType::list(AttrType::string()),
            configurable,
        )?)
    }

    fn int_list<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = allow_empty;
        let fallback = eval.heap().alloc(AllocList::EMPTY);
        Ok(bazel_attr(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            AttrType::list(AttrType::int()),
            configurable,
        )?)
    }

    fn label<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named)] allow_files: Option<Value<'v>>,
        #[starlark(require = named)] allow_single_file: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        aspects: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        flags: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] allow_rules: Option<Value<'v>>,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named)] for_dependency_resolution: Option<Value<'v>>,
        #[starlark(require = named)] materializer: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = (
            executable,
            flags,
            allow_rules,
            skip_validations,
            for_dependency_resolution,
            materializer,
        );
        let inner =
            bazel_label_attr_type(providers, allow_files, allow_single_file, cfg, false, eval)?;
        let coercer = if mandatory {
            inner
        } else {
            AttrType::option(inner)
        };
        let default = bazel_label_default(eval, default)?;
        Ok(bazel_attr_with_allowed_values_and_aspects(
            eval,
            default,
            Value::new_none(),
            mandatory,
            bazel_doc(doc),
            coercer,
            configurable,
            None,
            aspects.items,
        )?)
    }

    fn label_list<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        #[starlark(require = named)] allow_files: Option<Value<'v>>,
        #[starlark(require = named)] allow_rules: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        aspects: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        flags: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named)] for_dependency_resolution: Option<Value<'v>>,
        #[starlark(require = named)] materializer: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = (
            allow_empty,
            allow_rules,
            flags,
            skip_validations,
            for_dependency_resolution,
            materializer,
        );
        let coercer = bazel_label_attr_type(providers, allow_files, None, cfg, true, eval)?;
        let fallback = eval.heap().alloc(AllocList::EMPTY);
        let default = bazel_label_list_default(eval, default)?;
        Ok(bazel_attr_with_allowed_values_and_aspects(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            coercer,
            configurable,
            None,
            aspects.items,
        )?)
    }

    fn output<'v>(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        if mandatory {
            Ok(StarlarkAttribute::new_bazel_output(
                Attribute::new(None, bazel_doc(doc), AttrType::string())?,
                BazelOutputAttrKind::Output,
            ))
        } else {
            Ok(StarlarkAttribute::new_bazel_output(
                bazel_attr(
                    eval,
                    None,
                    Value::new_none(),
                    false,
                    bazel_doc(doc),
                    AttrType::option(AttrType::string()),
                    Some(false),
                )?
                .clone_attribute(),
                BazelOutputAttrKind::Output,
            ))
        }
    }

    fn output_list<'v>(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = allow_empty;
        let fallback = eval.heap().alloc(AllocList::EMPTY);
        Ok(StarlarkAttribute::new_bazel_output(
            bazel_attr(
                eval,
                None,
                fallback,
                mandatory,
                bazel_doc(doc),
                AttrType::list(AttrType::string()),
                Some(false),
            )?
            .clone_attribute(),
            BazelOutputAttrKind::OutputList,
        ))
    }

    fn string_dict<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = allow_empty;
        let fallback = eval.heap().alloc(AllocDict::EMPTY);
        Ok(bazel_attr(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            AttrType::dict(AttrType::string(), AttrType::string(), false),
            configurable,
        )?)
    }

    fn string_list_dict<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = allow_empty;
        let fallback = eval.heap().alloc(AllocDict::EMPTY);
        Ok(bazel_attr(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            AttrType::dict(
                AttrType::string(),
                AttrType::list(AttrType::string()),
                false,
            ),
            configurable,
        )?)
    }

    fn string_keyed_label_dict<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        #[starlark(require = named)] allow_files: Option<Value<'v>>,
        #[starlark(require = named)] allow_rules: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        aspects: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        flags: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] for_dependency_resolution: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = (
            allow_empty,
            allow_rules,
            aspects,
            flags,
            for_dependency_resolution,
        );
        let value = bazel_label_attr_type(providers, allow_files, None, cfg, false, eval)?;
        let fallback = eval.heap().alloc(AllocDict::EMPTY);
        Ok(bazel_attr(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            AttrType::dict(AttrType::string(), value, false),
            configurable,
        )?)
    }

    fn label_keyed_string_dict<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named)] configurable: Option<bool>,
        #[starlark(require = named)] allow_files: Option<Value<'v>>,
        #[starlark(require = named)] allow_rules: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        providers: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        aspects: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        flags: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = false)] skip_validations: bool,
        #[starlark(require = named)] for_dependency_resolution: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let _unused = (
            allow_empty,
            allow_rules,
            aspects,
            flags,
            skip_validations,
            for_dependency_resolution,
        );
        let key = bazel_label_attr_type(providers, allow_files, None, cfg, false, eval)?;
        let fallback = eval.heap().alloc(AllocDict::EMPTY);
        Ok(bazel_attr(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            AttrType::dict(key, AttrType::string(), false),
            configurable,
        )?)
    }

    fn license<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkAttribute<'v>> {
        let fallback = eval.heap().alloc(AllocList::EMPTY);
        Ok(bazel_attr(
            eval,
            default,
            fallback,
            mandatory,
            bazel_doc(doc),
            AttrType::list(AttrType::string()),
            Some(false),
        )?)
    }
}

pub(crate) fn register_attrs(globals: &mut GlobalsBuilder) {
    globals.namespace("attr", bazel_attr_module);
    globals.namespace("attrs", attr_module);
    register_attr_type(globals);
}
