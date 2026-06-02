def _bazel_genquery_impl(ctx):
    if ctx.attr.compressed_output:
        fail("genquery(compressed_output = True) is not implemented")

    fail(
        "genquery is not implemented in Buck2. Bazel evaluates genquery by " +
        "running the query engine over the transitive closure of the rule's " +
        "scope; refusing to emit a placeholder output for expression `{}`.".format(
            ctx.attr.expression,
        ),
    )

bazel_genquery = rule(
    implementation = _bazel_genquery_impl,
    attrs = {
        "compressed_output": attr.bool(default = False),
        "expression": attr.string(mandatory = True),
        "opts": attr.string_list(default = []),
        "scope": attr.label_list(mandatory = True),
        "strict": attr.bool(default = True),
    },
)
