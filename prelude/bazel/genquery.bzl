# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
# You may select, at your option, one of the above-listed licenses.

def _bazel_genquery_impl(ctx):
    if ctx.attr.compressed_output:
        fail("genquery(compressed_output = True) is not implemented")

    output = ctx.actions.write(
        ctx.label.name,
        "",
        has_content_based_path = False,
    )
    return [DefaultInfo(default_output = output)]

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
