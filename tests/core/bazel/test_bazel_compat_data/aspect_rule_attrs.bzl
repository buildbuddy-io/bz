GoLikeInfo = provider()
ProtoLikeInfo = provider(fields = ["path"])
ProtoImportInfo = provider(fields = ["imports"])

def _noop_transition_impl(_settings, _attr):
    return {}

noop_transition = transition(
    implementation = _noop_transition_impl,
    inputs = [],
    outputs = [],
)

def _proto_like_impl(ctx):
    return [ProtoLikeInfo(path = ctx.attr.path)]

proto_like = rule(
    implementation = _proto_like_impl,
    attrs = {
        "path": attr.string(),
    },
)

def _imports(attr, importpath):
    direct = []
    if hasattr(attr, "proto") and attr.proto and type(attr.proto) == type([]) and ProtoLikeInfo in attr.proto[0]:
        direct.append("{}={}".format(attr.proto[0][ProtoLikeInfo].path, importpath))
    transitive = [
        dep[ProtoImportInfo].imports
        for dep in getattr(attr, "deps", [])
        if ProtoImportInfo in dep
    ]
    return depset(direct = direct, transitive = transitive)

def _proto_import_aspect_impl(_target, ctx):
    attr = ctx.rule.attr
    return [ProtoImportInfo(imports = _imports(attr, attr.importpath))]

_proto_import_aspect = aspect(
    implementation = _proto_import_aspect_impl,
    attr_aspects = ["deps"],
)

def _go_proto_like_impl(ctx):
    imports = _imports(ctx.attr, ctx.attr.importpath).to_list()
    missing = [
        expected
        for expected in ctx.attr.expected_imports
        if expected not in imports
    ]
    if missing:
        fail("missing imports {} from {}".format(missing, imports))
    return [GoLikeInfo()]

go_proto_like = rule(
    implementation = _go_proto_like_impl,
    attrs = {
        "proto": attr.label(
            cfg = noop_transition,
            providers = [ProtoLikeInfo],
        ),
        "deps": attr.label_list(
            providers = [GoLikeInfo],
            aspects = [_proto_import_aspect],
        ),
        "importpath": attr.string(),
        "expected_imports": attr.string_list(),
    },
)
