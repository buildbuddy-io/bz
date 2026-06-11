# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//:native.bzl", _native = "native")

set_platform_decorator_for_python = lambda **kwargs: kwargs

def meta_python_test(name, **kwargs):
    # Set the platform attributes as needed for proper exec platform resolution
    kwargs = set_platform_decorator_for_python(
        **kwargs
    )

    _native.python_test(
        name = name,
        **kwargs
    )

_PYTHON_SCRUBBER = "prelude//apple/tools/selective_debugging:tool"

def apple_oso_scrubber_target():
    return _PYTHON_SCRUBBER

def bundle_telemetry_logger_target():
    return read_root_config("apple", "bundle_telemetry_logger", None)
