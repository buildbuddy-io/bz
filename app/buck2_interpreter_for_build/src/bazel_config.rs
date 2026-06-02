use allocative::Allocative;
use buck2_core::provider::id::ProviderId;
use buck2_interpreter::types::provider::callable::ProviderCallableLike;
use buck2_interpreter::types::provider::callable::ProviderLike;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::Demand;
use starlark::values::Freeze;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark_map::StarlarkHasher;

use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelConfigError {
    #[error("'repeatable' can only be set for a setting with 'flag = True'")]
    RepeatableRequiresFlag,
    #[error("FeatureFlagInfo() got unexpected named argument `{0}`")]
    FeatureFlagInfoUnexpectedArgument(String),
    #[error("FeatureFlagInfo() missing required named argument `value`")]
    FeatureFlagInfoMissingValue,
    #[error("FeatureFlagInfo(value = ...) expected string, got `{0}`")]
    FeatureFlagInfoValueNotString(String),
}

#[derive(Clone, Debug, Freeze, ProvidesStaticType, Trace, Allocative)]
#[repr(C)]
pub(crate) struct BazelFeatureFlagInfoGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[trace(unsafe_ignore)]
    id: Arc<ProviderId>,
    value: V,
}

starlark_complex_value!(pub(crate) BazelFeatureFlagInfo);

unsafe impl<FromV, ToV> Coerce<BazelFeatureFlagInfoGen<ToV>> for BazelFeatureFlagInfoGen<FromV>
where
    FromV: ValueLifetimeless + Coerce<ToV>,
    ToV: ValueLifetimeless,
{
}

impl<V: ValueLifetimeless> fmt::Display for BazelFeatureFlagInfoGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FeatureFlagInfo(<computed>)")
    }
}

impl<'v, V: ValueLike<'v>> serde::Serialize for BazelFeatureFlagInfoGen<V> {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.collect_map(self.items())
    }
}

#[starlark_value(type = "FeatureFlagInfo")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for BazelFeatureFlagInfoGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_feature_flag_info_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "error".to_owned(),
            "is_valid_value".to_owned(),
            "value".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "error" => Some(Value::new_none()),
            "value" => Some(self.value.to_value()),
            _ => None,
        }
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderLike>(self);
    }
}

impl<'v, V: ValueLike<'v>> ProviderLike<'v> for BazelFeatureFlagInfoGen<V> {
    fn id(&self) -> &Arc<ProviderId> {
        &self.id
    }

    fn items(&self) -> Vec<(&str, Value<'v>)> {
        vec![
            ("error", Value::new_none()),
            ("value", self.value.to_value()),
        ]
    }
}

#[starlark_module]
fn bazel_feature_flag_info_methods(builder: &mut MethodsBuilder) {
    fn is_valid_value<'v>(
        #[starlark(this)] _this: &BazelFeatureFlagInfo<'v>,
        value: Value<'v>,
    ) -> starlark::Result<bool> {
        if value.unpack_str().is_none() {
            return Err(
                buck2_error::Error::from(BazelConfigError::FeatureFlagInfoValueNotString(
                    value.get_type().to_owned(),
                ))
                .into(),
            );
        }
        Ok(true)
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct BazelExecTransition {
    exec_group: Option<String>,
}

impl fmt::Display for BazelExecTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.exec_group {
            Some(exec_group) => write!(f, "<exec transition for {exec_group}>"),
            None => f.write_str("<exec transition>"),
        }
    }
}

starlark_simple_value!(BazelExecTransition);

#[starlark_value(type = "ExecTransitionFactory")]
impl<'v> StarlarkValue<'v> for BazelExecTransition {}

pub(crate) fn bazel_exec_transition_from_value(value: Value<'_>) -> Option<&BazelExecTransition> {
    value.downcast_ref::<BazelExecTransition>()
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelConfigProviderCallable {
    name: &'static str,
    id: Arc<ProviderId>,
}

impl BazelConfigProviderCallable {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            id: Arc::new(ProviderId {
                path: None,
                name: name.to_owned(),
            }),
        }
    }
}

impl fmt::Display for BazelConfigProviderCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

starlark_simple_value!(BazelConfigProviderCallable);

#[starlark_value(type = "bazel_config_provider_callable")]
impl<'v> StarlarkValue<'v> for BazelConfigProviderCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        let mut value = None;
        for (name, v) in args.names_map()? {
            match name.as_str() {
                "value" => value = Some(v),
                other => {
                    return Err(buck2_error::Error::from(
                        BazelConfigError::FeatureFlagInfoUnexpectedArgument(other.to_owned()),
                    )
                    .into());
                }
            }
        }
        let value = value.ok_or_else(|| {
            buck2_error::Error::from(BazelConfigError::FeatureFlagInfoMissingValue)
        })?;
        if value.unpack_str().is_none() {
            return Err(
                buck2_error::Error::from(BazelConfigError::FeatureFlagInfoValueNotString(
                    value.get_type().to_owned(),
                ))
                .into(),
            );
        }
        Ok(eval
            .heap()
            .alloc(BazelFeatureFlagInfo {
                id: self.id.clone(),
                value,
            })
            .to_value())
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderCallableLike>(self);
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        let Some(other) = other.request_value::<&dyn ProviderCallableLike>() else {
            return Ok(false);
        };
        Ok(&self.id == other.id()?)
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.id.hash(hasher);
        Ok(())
    }
}

impl ProviderCallableLike for BazelConfigProviderCallable {
    fn id(&self) -> buck2_error::Result<&Arc<ProviderId>> {
        Ok(&self.id)
    }
}

fn build_setting<'v>(
    kind: &'static str,
    flag: bool,
    allow_multiple: bool,
    repeatable: bool,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    let kind = eval.heap().alloc(kind);
    let flag = eval.heap().alloc(flag);
    let allow_multiple = eval.heap().alloc(allow_multiple);
    let repeatable = eval.heap().alloc(repeatable);
    eval.heap().alloc(AllocStruct([
        ("type", kind),
        ("flag", flag),
        ("allow_multiple", allow_multiple),
        ("repeatable", repeatable),
    ]))
}

#[starlark_module]
fn bazel_config_module(builder: &mut GlobalsBuilder) {
    fn int<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("int", flag, false, false, eval))
    }

    fn bool<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("bool", flag, false, false, eval))
    }

    fn string<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        #[starlark(require = named, default = false)] allow_multiple: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(build_setting("string", flag, allow_multiple, false, eval))
    }

    fn string_list<'v>(
        #[starlark(require = named, default = false)] flag: bool,
        #[starlark(require = named, default = false)] repeatable: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if repeatable && !flag {
            return Err(buck2_error::Error::from(BazelConfigError::RepeatableRequiresFlag).into());
        }
        Ok(build_setting("string_list", flag, false, repeatable, eval))
    }

    fn exec(
        #[starlark(default = NoneOr::None)] exec_group: NoneOr<&str>,
    ) -> starlark::Result<BazelExecTransition> {
        Ok(BazelExecTransition {
            exec_group: exec_group.into_option().map(str::to_owned),
        })
    }
}

#[starlark_module]
fn bazel_config_common_module(builder: &mut GlobalsBuilder) {
    fn toolchain_type<'v>(
        toolchain_type: Value<'v>,
        #[starlark(require = named, default = true)] mandatory: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let mandatory = eval.heap().alloc(mandatory);
        Ok(eval.heap().alloc(AllocStruct([
            ("toolchain_type", toolchain_type),
            ("mandatory", mandatory),
        ])))
    }
}

pub(crate) fn register_bazel_config(builder: &mut GlobalsBuilder) {
    builder.namespace("config", bazel_config_module);
    builder.namespace("config_common", |config_common| {
        bazel_config_common_module(config_common);
        config_common.set(
            "FeatureFlagInfo",
            BazelConfigProviderCallable::new("FeatureFlagInfo"),
        );
    });
}
