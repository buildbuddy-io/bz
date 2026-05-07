def _bazel_package_group_impl(_ctx):
    return [
        DefaultInfo(),
        PackageSpecificationInfo(),
    ]

bazel_package_group = rule(
    implementation = _bazel_package_group_impl,
    attrs = {
        "includes": attr.label_list(default = []),
        "packages": attr.string_list(default = []),
    },
)
