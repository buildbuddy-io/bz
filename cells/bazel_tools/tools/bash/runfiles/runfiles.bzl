def _bazel_tools_runfiles_impl(ctx):
    runfiles = ctx.runfiles(
        root_symlinks = {
            "bazel_tools/tools/bash/runfiles/runfiles.bash": ctx.file.src,
        },
    )
    return [DefaultInfo(
        files = depset([ctx.file.src]),
        default_runfiles = runfiles,
    )]

bazel_tools_runfiles = rule(
    implementation = _bazel_tools_runfiles_impl,
    attrs = {
        "src": attr.label(
            allow_single_file = True,
            mandatory = True,
        ),
    },
)
