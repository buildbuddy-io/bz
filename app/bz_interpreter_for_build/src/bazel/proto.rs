use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;
use starlark::values::list::AllocList;

#[starlark_module]
fn bazel_proto_common_do_not_use(builder: &mut GlobalsBuilder) {
    fn incompatible_enable_proto_toolchain_resolution() -> starlark::Result<bool> {
        Ok(false)
    }

    fn external_proto_infos<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }
}

pub(crate) fn register_bazel_proto_common(builder: &mut GlobalsBuilder) {
    builder.namespace("proto_common_do_not_use", |proto_common| {
        proto_common.set("INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION", false);
        proto_common.set("INCOMPATIBLE_PASS_TOOLCHAIN_TYPE", false);
        bazel_proto_common_do_not_use(proto_common);
    });
}
