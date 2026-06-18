NeedInfo = provider()

def _plain_rule_impl(ctx):
    return [DefaultInfo()]

plain_rule = rule(implementation = _plain_rule_impl)

def _allow_files_consumer_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join([src.basename for src in ctx.files.srcs]))
    return [DefaultInfo(files = depset([out]))]

allow_files_consumer = rule(
    implementation = _allow_files_consumer_impl,
    attrs = {
        "srcs": attr.label_list(
            allow_files = [".txt"],
            providers = [NeedInfo],
        ),
    },
)
