# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("//rules:ci.bzl", "ci")

def _impl(ctx: AnalysisContext) -> list[Provider]:
    out = ctx.actions.declare_output(ctx.attrs.name, dir = True, has_content_based_path = False)
    ctx.actions.run(
        cmd_args(
            ctx.attrs.download_tool[DefaultInfo].default_outputs[0],
            ctx.attrs.rpm_name,
            out.as_output(),
        ),
        category = "download_rpm",
        local_only = True,
    )
    return [DefaultInfo(default_output = out)]

download_rpm_impl = rule(
    impl = _impl,
    attrs = {
        "download_tool": attrs.exec_dep(),
        "labels": attrs.list(attrs.string(), default = []),
        "rpm_name": attrs.string(),
    },
)

def download_rpm(**kwargs):
    prelude = native

    download_rpm_impl(
        download_tool = "//deps/rpm_download:download.sh",
        visibility = ["PUBLIC"],
        labels = ci.remove_labels(
            ci.windows(),
            ci.mac(ci.aarch64()),
        ),
        **kwargs
    )
