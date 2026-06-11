# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.


BUILD_MODE_DEBUG = "debug"
BUILD_MODE_PROFILE = "profile"
BUILD_MODE_RELEASE = "release"

APPLE_BUILD_MODES = [BUILD_MODE_DEBUG, BUILD_MODE_PROFILE, BUILD_MODE_RELEASE]

# 1:1 mapping of build mode to canonical (supported/default) apple build modes
REMAPPED_BUILD_MODES = {}

BUILD_MODE = struct(
    DEBUG = BUILD_MODE_DEBUG,
    PROFILE = BUILD_MODE_PROFILE,
    RELEASE = BUILD_MODE_RELEASE,
)

CONSTRAINT_PACKAGE = "prelude//platforms/apple/constraints"

def get_build_mode():
    return read_root_config("apple", "build_mode", BUILD_MODE_DEBUG)

def get_build_mode_debug():
    return BUILD_MODE.DEBUG

def get_build_mode_release():
    return BUILD_MODE.RELEASE
