# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

def _collect_files(srcs: list[typing.Any]) -> list[Artifact]:
    files = []
    for src in srcs:
        files.extend(src.files.to_list())
    return files

def _bazel_filegroup_impl(ctx):
    files = _collect_files(ctx.attr.srcs)
    return [DefaultInfo(files = depset(files))]

bazel_filegroup = rule(
    implementation = _bazel_filegroup_impl,
    attrs = {
        "data": attr.label_list(allow_files = True),
        "output_group": attr.string(default = ""),
        "srcs": attr.label_list(allow_files = True),
    },
)
