def _aspect_impl(_target, ctx):
    if "//:consumer_toolchain_type" in ctx.toolchains:
        fail("aspect ctx.toolchains unexpectedly contains the consuming rule toolchain")
    if "//:aspect_toolchain_type" not in ctx.toolchains:
        fail("aspect ctx.toolchains is missing the aspect toolchain")
    return []

_toolchain_aspect = aspect(
    implementation = _aspect_impl,
    toolchains = [config_common.toolchain_type("//:aspect_toolchain_type", mandatory = False)],
)

RequiredAspectInfo = provider()

def _missing_toolchain_aspect_impl(_target, ctx):
    if ctx.toolchains["//:missing_aspect_toolchain_type"] == None:
        fail("missing mandatory aspect toolchain was not resolved")
    return []

_missing_toolchain_aspect = aspect(
    implementation = _missing_toolchain_aspect_impl,
    required_providers = [RequiredAspectInfo],
    toolchains = [config_common.toolchain_type("//:missing_aspect_toolchain_type")],
)

def _leaf_impl(_ctx):
    return []

leaf = rule(
    implementation = _leaf_impl,
)

def _required_leaf_impl(_ctx):
    return [RequiredAspectInfo()]

required_leaf = rule(
    implementation = _required_leaf_impl,
)

def _consumer_impl(ctx):
    return []

consumer = rule(
    implementation = _consumer_impl,
    attrs = {
        "dep": attr.label(aspects = [_toolchain_aspect]),
    },
    toolchains = [config_common.toolchain_type("//:consumer_toolchain_type", mandatory = False)],
)

missing_toolchain_consumer = rule(
    implementation = _consumer_impl,
    attrs = {
        "dep": attr.label(aspects = [_missing_toolchain_aspect]),
    },
)
