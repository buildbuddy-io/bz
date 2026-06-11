# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//:native.bzl", "native")

def add_os_labels(**kwargs):
    if "labels" not in kwargs:
        kwargs["labels"] = []

    if native.host_info().os.is_macos:
        kwargs["labels"] += ["tpx:platform:macos"]
    if native.host_info().os.is_linux:
        kwargs["labels"] += ["tpx:platform:linux"]
    if native.host_info().os.is_windows:
        kwargs["labels"] += ["tpx:platform:windows"]

    kwargs["labels"] += ["tpx:is_standalone_build"]

    return kwargs
