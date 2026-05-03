# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//:native.bzl", "native")

def exports_files(srcs, visibility = None, **_kwargs):
    for src in srcs:
        native.export_file(
            name = src,
            src = src,
            visibility = visibility,
        )
