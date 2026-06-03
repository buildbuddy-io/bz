use std::fmt;

use allocative::Allocative;
use bz_core::cells::external::BZLMOD_BAZEL_COMPAT_VERSION;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use bz_core::cells::external::external_cell_origin_for_cell;
use bz_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use bz_node::attrs::coercion_context::AttrCoercionContext;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::dict::AllocDict;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::StructRef;
use starlark::values::tuple::UnpackTuple;

use crate::interpreter::build_context::BuildContext;
use crate::interpreter::module_internals::ModuleInternals;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum BazelNativeError {
    #[error(
        "Bazel native rule `{0}` requires the `bz_bazel_native_rules` prelude backing struct"
    )]
    MissingNativeRuleBacking(&'static str),
    #[error("`bz_bazel_native_rules` must be a struct, got `{0}`")]
    InvalidNativeRuleBacking(String),
    #[error("`native.register_toolchains` expected a string target pattern, got `{0}`")]
    RegisterToolchainsNonString(String),
    #[error("Bazel label build setting requires the prelude `alias` rule to be loaded")]
    MissingAliasRule,
    #[error("Bazel native rule `{0}` requires a Buck rule backing with the same name")]
    MissingNativeRule(&'static str),
    #[error("`native.package_relative_label` expected a string or label, got `{0}`")]
    PackageRelativeLabelInvalidInput(String),
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct NativeRuleCallable {
    name: &'static str,
}

impl fmt::Display for NativeRuleCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in rule {}>", self.name)
    }
}

starlark::starlark_simple_value!(NativeRuleCallable);

#[starlark_value(type = "native_rule_callable")]
impl<'v> StarlarkValue<'v> for NativeRuleCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let backing = eval
            .module()
            .get("bz_bazel_native_rules")
            .ok_or_else(|| {
                bz_error::Error::from(BazelNativeError::MissingNativeRuleBacking(self.name))
            })?;
        let backing = StructRef::from_value(backing).ok_or_else(|| {
            bz_error::Error::from(BazelNativeError::InvalidNativeRuleBacking(
                backing.get_type().to_owned(),
            ))
        })?;
        let rule = backing
            .iter()
            .find_map(|(name, rule)| (name.as_str() == self.name).then_some(rule))
            .ok_or_else(|| {
                bz_error::Error::from(BazelNativeError::MissingNativeRule(self.name))
            })?;
        if self.name == "sh_binary" {
            return invoke_bazel_sh_binary(rule, args, eval);
        }
        ValueLike::invoke(rule, args, eval)
    }
}

fn list_first<'v>(value: Value<'v>) -> Option<Value<'v>> {
    ListRef::from_value(value).and_then(|list| list.iter().next())
}

fn invoke_bazel_sh_binary<'v>(
    rule: Value<'v>,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let positions = args.positions(eval.heap())?.collect::<Vec<_>>();
    let named = args.names_map()?;
    let has_main = named.iter().any(|(name, _)| name.as_str() == "main");
    let has_resources = named.iter().any(|(name, _)| name.as_str() == "resources");
    let srcs = named
        .iter()
        .find(|(name, _)| name.as_str() == "srcs")
        .map(|(_, value)| *value);
    let data = named
        .iter()
        .find(|(name, _)| name.as_str() == "data")
        .map(|(_, value)| *value);

    let mut kwargs_owned = Vec::new();
    for (name, value) in named {
        match name.as_str() {
            "srcs" | "data" => {}
            _ => kwargs_owned.push((name.as_str().to_owned(), value)),
        }
    }
    if !has_main && let Some(main) = srcs.and_then(list_first) {
        kwargs_owned.push(("main".to_owned(), main));
    }
    if !has_resources && let Some(data) = data {
        kwargs_owned.push(("resources".to_owned(), data));
    }

    let kwargs = kwargs_owned
        .iter()
        .map(|(name, value)| (name.as_str(), *value))
        .collect::<Vec<_>>();
    eval.eval_function(rule, &positions, &kwargs)
}

fn current_bazel_repo_name(eval: &mut Evaluator) -> bz_error::Result<String> {
    let cell_name = BuildContext::from_context(eval)?.cell_info().name().name();
    let cell_name = cell_name.as_str();
    if cell_name == "root" {
        return Ok(String::new());
    }
    Ok(bzlmod_canonical_repo_name_for_cell(cell_name).unwrap_or_else(|| cell_name.to_owned()))
}

fn current_bazel_module_name(eval: &mut Evaluator) -> bz_error::Result<String> {
    let cell_name = BuildContext::from_context(eval)?.cell_info().name().name();
    let cell_name = cell_name.as_str();
    if cell_name == "root" {
        return Ok(String::new());
    }
    match external_cell_origin_for_cell(cell_name) {
        Some(ExternalCellOrigin::Bzlmod(setup)) => Ok(setup.module_name.to_string()),
        _ => {
            let repo = current_bazel_repo_name(eval)?;
            Ok(repo.strip_suffix('+').unwrap_or(&repo).to_owned())
        }
    }
}

fn current_bazel_module_version(eval: &mut Evaluator) -> bz_error::Result<String> {
    let cell_name = BuildContext::from_context(eval)?.cell_info().name().name();
    let cell_name = cell_name.as_str();
    match external_cell_origin_for_cell(cell_name) {
        Some(ExternalCellOrigin::Bzlmod(setup)) => Ok(setup.version.to_string()),
        _ => Ok(String::new()),
    }
}

#[starlark_module]
fn bazel_native_module(builder: &mut GlobalsBuilder) {
    fn existing_rule<'v>(
        _name: &str,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(Value::new_none())
    }

    fn existing_rules<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocDict::EMPTY))
    }

    fn repo_name(eval: &mut Evaluator) -> starlark::Result<String> {
        Ok(current_bazel_repo_name(eval)?)
    }

    fn repository_name(eval: &mut Evaluator) -> starlark::Result<String> {
        Ok(format!("@{}", current_bazel_repo_name(eval)?))
    }

    fn package_name(eval: &mut Evaluator) -> starlark::Result<String> {
        Ok(BuildContext::from_context(eval)?
            .base_path()?
            .path()
            .to_string())
    }

    fn module_name(eval: &mut Evaluator) -> starlark::Result<String> {
        Ok(current_bazel_module_name(eval)?)
    }

    fn module_version(eval: &mut Evaluator) -> starlark::Result<String> {
        Ok(current_bazel_module_version(eval)?)
    }

    fn register_toolchains<'v>(
        #[starlark(args)] toolchains: UnpackTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let _build_context = BuildContext::from_context(eval)?;
        for toolchain in toolchains.items {
            if toolchain.unpack_str().is_none() {
                return Err(bz_error::Error::from(
                    BazelNativeError::RegisterToolchainsNonString(toolchain.get_type().to_owned()),
                )
                .into());
            }
        }
        Ok(NoneType)
    }

    fn package_relative_label<'v>(
        input: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if StarlarkProvidersLabel::from_value(input).is_some() {
            return Ok(input);
        }
        let Some(label) = input.unpack_str() else {
            return Err(bz_error::Error::from(
                BazelNativeError::PackageRelativeLabelInvalidInput(input.get_type().to_owned()),
            )
            .into());
        };
        let build_context = ModuleInternals::from_context(eval, "native.package_relative_label")?;
        let label = build_context
            .attr_coercion_context()
            .coerce_providers_label(label)?;
        Ok(eval.heap().alloc(StarlarkProvidersLabel::new(label)))
    }
}

fn label_build_setting<'v>(
    name: &str,
    build_setting_default: Value<'v>,
    visibility: Option<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<NoneType> {
    let alias = eval
        .module()
        .get("alias")
        .ok_or_else(|| bz_error::Error::from(BazelNativeError::MissingAliasRule))?;
    let name = eval.heap().alloc(name);
    let mut kwargs = vec![("name", name), ("actual", build_setting_default)];
    if let Some(visibility) = visibility {
        kwargs.push(("visibility", visibility));
    }
    eval.eval_function(alias, &[], &kwargs)
        .map_err(bz_error::Error::from)?;
    Ok(NoneType)
}

#[starlark_module]
fn bazel_build_setting_rules(builder: &mut GlobalsBuilder) {
    fn label_flag<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] build_setting_default: Value<'v>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        label_build_setting(name, build_setting_default, visibility, eval)
    }

    fn label_setting<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] build_setting_default: Value<'v>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        label_build_setting(name, build_setting_default, visibility, eval)
    }
}

#[starlark_module]
fn bazel_native_toplevels(builder: &mut GlobalsBuilder) {
    fn module_name(eval: &mut Evaluator) -> starlark::Result<String> {
        Ok(current_bazel_module_name(eval)?)
    }

    fn module_version(eval: &mut Evaluator) -> starlark::Result<String> {
        Ok(current_bazel_module_version(eval)?)
    }
}

pub(crate) fn register_bazel_native(builder: &mut GlobalsBuilder) {
    builder.namespace("native", |globals| {
        globals.set("bazel_version", BZLMOD_BAZEL_COMPAT_VERSION);
        bazel_native_module(globals);
        for name in [
            "alias",
            "cc_binary",
            "cc_import",
            "cc_library",
            "cc_libc_top_alias",
            "cc_shared_library",
            "cc_test",
            "cc_toolchain",
            "cc_toolchain_suite",
            "config_setting",
            "constraint_setting",
            "constraint_value",
            "filegroup",
            "genquery",
            "genrule",
            "java_binary",
            "java_import",
            "java_library",
            "java_package_configuration",
            "java_plugin",
            "java_runtime",
            "java_test",
            "java_toolchain",
            "package_group",
            "platform",
            "starlark_doc_extract",
            "test_suite",
            "toolchain",
            "toolchain_type",
        ] {
            globals.set(name, NativeRuleCallable { name });
        }
    });
    bazel_build_setting_rules(builder);
}

pub(crate) fn register_bazel_native_toplevels(builder: &mut GlobalsBuilder) {
    bazel_build_setting_rules(builder);
    bazel_native_toplevels(builder);
}
