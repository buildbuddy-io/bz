def _collect_files(srcs: list[typing.Any]) -> list[typing.Any]:
    files = []
    for src in srcs:
        files.append(src.files)
    return files

def _collect_output_group(srcs: list[typing.Any], output_group: str) -> list[typing.Any]:
    files = []
    for src in srcs:
        if OutputGroupInfo in src:
            group = src[OutputGroupInfo].groups.get(output_group)
            if group != None:
                files.append(group)
    return files

def _bazel_filegroup_impl(ctx):
    output_group = ctx.attr.output_group
    if output_group.endswith("_INTERNAL_"):
        fail("Output group {} is not permitted for reference in filegroups.".format(output_group))

    transitive_files = _collect_output_group(ctx.attr.srcs, output_group) if output_group else _collect_files(ctx.attr.srcs)
    files = depset(transitive = transitive_files)
    files_list = files.to_list()
    executable = files_list[0] if len(files_list) == 1 else None
    return [DefaultInfo(files = files, executable = executable)]

bazel_filegroup = rule(
    implementation = _bazel_filegroup_impl,
    attrs = {
        "data": attr.label_list(allow_files = True),
        "output_group": attr.string(default = ""),
        "srcs": attr.label_list(allow_files = True),
    },
)
