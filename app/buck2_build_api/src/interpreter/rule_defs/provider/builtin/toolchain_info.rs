use std::fmt;
use std::sync::Arc;
use std::sync::OnceLock;

use allocative::Allocative;
use buck2_core::provider::id::ProviderId;
use buck2_interpreter::types::provider::callable::ProviderCallableLike;
use dupe::Dupe;
use serde::Serializer;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::environment::GlobalsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::values::Demand;
use starlark::values::Freeze;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::starlark_value;
use starlark_map::StarlarkHasher;

use crate::interpreter::rule_defs::provider::FrozenBuiltinProviderLike;
use crate::interpreter::rule_defs::provider::ProviderLike;
use crate::interpreter::rule_defs::provider::callable::provider_callable_equals;
use crate::interpreter::rule_defs::provider::callable::provider_callable_write_hash;

#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct ToolchainInfoGen<V: ValueLifetimeless> {
    values: Box<[(String, V)]>,
}

starlark::starlark_complex_value!(pub ToolchainInfo);

impl<V: ValueLifetimeless> fmt::Display for ToolchainInfoGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ToolchainInfo(<{} field(s)>)", self.values.len())
    }
}

impl<'v, V: ValueLike<'v>> serde::Serialize for ToolchainInfoGen<V> {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.collect_map(
            self.values
                .iter()
                .map(|(name, value)| (name, value.to_value())),
        )
    }
}

#[starlark_value(type = "ToolchainInfo")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ToolchainInfoGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn dir_attr(&self) -> Vec<String> {
        self.values.iter().map(|(name, _)| name.clone()).collect()
    }

    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.values
            .iter()
            .find_map(|(name, value)| (name == attribute).then(|| value.to_value()))
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderLike>(self);
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        let Some(other) = ToolchainInfo::from_value(other) else {
            return Ok(false);
        };
        if self.values.len() != other.values.len() {
            return Ok(false);
        }
        for ((name, value), (other_name, other_value)) in self.values.iter().zip(&other.values) {
            if name != other_name || !value.to_value().equals(other_value.to_value())? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl<'v, V: ValueLike<'v>> ProviderLike<'v> for ToolchainInfoGen<V> {
    fn id(&self) -> &Arc<ProviderId> {
        ToolchainInfoCallable::provider_id()
    }

    fn items(&self) -> Vec<(&str, Value<'v>)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_value()))
            .collect()
    }
}

impl FrozenBuiltinProviderLike for FrozenToolchainInfo {
    fn builtin_provider_id() -> &'static Arc<ProviderId> {
        ToolchainInfoCallable::provider_id()
    }
}

#[derive(Debug, Clone, Dupe, ProvidesStaticType, NoSerialize, Allocative)]
struct ToolchainInfoCallable {
    id: &'static Arc<ProviderId>,
}

impl ToolchainInfoCallable {
    fn provider_id() -> &'static Arc<ProviderId> {
        static PROVIDER_ID: OnceLock<Arc<ProviderId>> = OnceLock::new();
        PROVIDER_ID.get_or_init(|| {
            Arc::new(ProviderId {
                path: None,
                name: "ToolchainInfo".to_owned(),
            })
        })
    }

    fn new() -> Self {
        Self {
            id: Self::provider_id(),
        }
    }
}

impl fmt::Display for ToolchainInfoCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ToolchainInfo")
    }
}

starlark::starlark_simple_value!(ToolchainInfoCallable);

#[starlark_value(type = "toolchain_info_callable")]
impl<'v> StarlarkValue<'v> for ToolchainInfoCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        static RES: GlobalsStatic = GlobalsStatic::new();
        ValueLike::invoke(
            RES.function(
                concat!(module_path!(), "::toolchain_info_creator"),
                toolchain_info_creator,
            ),
            args,
            eval,
        )
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderCallableLike>(self);
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        provider_callable_equals(self, other)
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        provider_callable_write_hash(self, hasher)
    }
}

impl ProviderCallableLike for ToolchainInfoCallable {
    fn id(&self) -> buck2_error::Result<&Arc<ProviderId>> {
        Ok(self.id)
    }
}

#[starlark_module]
fn toolchain_info_creator(globals: &mut GlobalsBuilder) {
    fn ToolchainInfo<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<ToolchainInfo<'v>> {
        let mut values = kwargs.into_iter().collect::<Vec<_>>();
        values.sort_by(|(left, _), (right, _)| left.cmp(right));
        Ok(ToolchainInfo {
            values: values.into_boxed_slice(),
        })
    }
}

pub(crate) fn register_toolchain_info(globals: &mut GlobalsBuilder) {
    globals.set("ToolchainInfo", ToolchainInfoCallable::new());
}
