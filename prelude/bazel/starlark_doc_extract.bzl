# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
# You may select, at your option, one of the above-listed licenses.

def _starlark_doc_extract_impl(ctx):
    binaryproto = ctx.outputs.binaryproto
    textproto = ctx.outputs.textproto
    ctx.actions.write(binaryproto, "", has_content_based_path = False)
    ctx.actions.write(textproto, "", has_content_based_path = False)
    return [
        DefaultInfo(
            default_output = binaryproto,
            other_outputs = [textproto],
        ),
    ]

starlark_doc_extract = rule(
    implementation = _starlark_doc_extract_impl,
    attrs = {
        "allow_unused_doc_comments": attr.bool(default = False),
        "deps": attr.label_list(allow_files = [".bzl", ".scl"], default = []),
        "render_main_repo_name": attr.bool(default = False),
        "src": attr.label(allow_single_file = [".bzl", ".scl"], mandatory = True),
        "symbol_names": attr.string_list(default = []),
    },
    outputs = {
        "binaryproto": "%{name}.binaryproto",
        "textproto": "%{name}.textproto",
    },
)
