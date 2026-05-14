# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
# You may select, at your option, one of the above-listed licenses.

def _starlark_doc_extract_impl(ctx):
    fail(
        "starlark_doc_extract is not implemented in Buck2. Bazel loads the " +
        "source .bzl/.scl module, validates its documented transitive loads, " +
        "and writes real ModuleInfo proto outputs; refusing to emit empty " +
        "binaryproto/textproto files for `{}`.".format(ctx.attr.src),
    )

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
