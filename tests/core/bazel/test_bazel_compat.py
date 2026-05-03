# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-strict

import shutil

import pytest
from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.buck_workspace import buck_test


@buck_test()
async def test_rules_go_binary_from_bazel_root(buck: Buck) -> None:
    if shutil.which("go") is None:
        pytest.skip("rules_go compatibility smoke test requires a Go toolchain")

    await buck.build("//:hello")


@buck_test()
async def test_proto_rules_load_from_bazel_root(buck: Buck) -> None:
    await buck.build("//:message_proto", "//:message_go_proto", "//:message_compiler")


@buck_test()
async def test_rules_go_library_embed_from_bazel_root(buck: Buck) -> None:
    if shutil.which("go") is None:
        pytest.skip("rules_go compatibility smoke test requires a Go toolchain")

    await buck.build("//:combined")


@buck_test()
async def test_bzlmod_root_repo_alias_from_bazel_root(buck: Buck) -> None:
    if shutil.which("go") is None:
        pytest.skip("rules_go compatibility smoke test requires a Go toolchain")

    await buck.build("//:root_alias_hello")
