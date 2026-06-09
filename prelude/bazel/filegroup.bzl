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

def _data_dep_runfiles(ctx, dep):
    # A data dependency contributes its data runfiles plus its files
    # (--incompatible_always_include_files_to_build_in_data).
    info = dep[DefaultInfo]
    return info.data_runfiles.merge(ctx.runfiles(transitive_files = info.files))

def _bazel_filegroup_impl(ctx):
    output_group = ctx.attr.output_group
    if output_group.endswith("_INTERNAL_"):
        fail("Output group {} is not permitted for reference in filegroups.".format(output_group))

    transitive_files = _collect_output_group(ctx.attr.srcs, output_group) if output_group else _collect_files(ctx.attr.srcs)
    files = depset(transitive = transitive_files)
    executable = py_internal.get_singleton_depset(files)

    data_dep_runfiles = [_data_dep_runfiles(ctx, dep) for dep in ctx.attr.data]
    default_runfiles = ctx.runfiles().merge_all(data_dep_runfiles + [
        src[DefaultInfo].default_runfiles
        for src in ctx.attr.srcs
    ])
    data_runfiles = ctx.runfiles(transitive_files = files).merge_all(data_dep_runfiles + [
        src[DefaultInfo].data_runfiles
        for src in ctx.attr.srcs
    ])
    return [DefaultInfo(
        files = files,
        executable = executable,
        default_runfiles = default_runfiles,
        data_runfiles = data_runfiles,
    )]

bazel_filegroup = rule(
    implementation = _bazel_filegroup_impl,
    attrs = {
        "data": attr.label_list(allow_files = True),
        "output_group": attr.string(default = ""),
        "srcs": attr.label_list(allow_files = True),
    },
)
