use std::fmt;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::starlark_value;

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct BazelConfigurationField {
    fragment: String,
    name: String,
}

impl BazelConfigurationField {
    pub(crate) fn fragment(&self) -> &str {
        &self.fragment
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for BazelConfigurationField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "configuration_field(fragment = {:?}, name = {:?})",
            self.fragment, self.name
        )
    }
}

starlark_simple_value!(BazelConfigurationField);

#[starlark_value(type = "LateBoundDefault")]
impl<'v> StarlarkValue<'v> for BazelConfigurationField {}

#[starlark_module]
pub(crate) fn register_bazel_configuration_field(builder: &mut GlobalsBuilder) {
    fn configuration_field(
        fragment: &str,
        name: &str,
    ) -> starlark::Result<BazelConfigurationField> {
        Ok(BazelConfigurationField {
            fragment: fragment.to_owned(),
            name: name.to_owned(),
        })
    }
}
