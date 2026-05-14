def _aspect_impl(_target, ctx):
    if "//:consumer_toolchain_type" in ctx.toolchains:
        fail("aspect ctx.toolchains unexpectedly contains the consuming rule toolchain")
    if "//:aspect_toolchain_type" not in ctx.toolchains:
        fail("aspect ctx.toolchains is missing the aspect toolchain")
    return []

_toolchain_aspect = aspect(
    implementation = _aspect_impl,
    toolchains = ["//:aspect_toolchain_type"],
)

def _leaf_impl(_ctx):
    return []

leaf = rule(
    implementation = _leaf_impl,
)

def _consumer_impl(ctx):
    return []

consumer = rule(
    implementation = _consumer_impl,
    attrs = {
        "dep": attr.label(aspects = [_toolchain_aspect]),
    },
    toolchains = ["//:consumer_toolchain_type"],
)
