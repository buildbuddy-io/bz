# pyre-strict

import shutil

import pytest
from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.asserts import expect_failure
from buck2.tests.e2e_util.buck_workspace import buck_test


@buck_test()
async def test_rules_go_binary_from_bazel_root(buck: Buck) -> None:
    if shutil.which("go") is None:
        pytest.skip("rules_go compatibility smoke test requires a Go toolchain")

    await buck.build("//:hello")


@buck_test()
async def test_rules_go_binary_with_bazelrc_build_settings(buck: Buck) -> None:
    if shutil.which("go") is None:
        pytest.skip("rules_go compatibility smoke test requires a Go toolchain")

    (buck.cwd / ".bazelrc").write_text(
        "common --java_runtime_version=local_jdk\n",
        encoding="utf-8",
    )

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


@buck_test()
async def test_bazel_aspect_toolchains_are_not_rule_toolchains(buck: Buck) -> None:
    await buck.build("//:aspect_toolchain_ctx")


@buck_test()
async def test_bazel_aspect_toolchains_only_required_when_aspect_applies(
    buck: Buck,
) -> None:
    await buck.build("//:missing_mandatory_aspect_toolchain_not_applicable")
    await expect_failure(
        buck.build("//:missing_mandatory_aspect_toolchain_applicable"),
        stderr_regex="mandatory toolchain type `root//:missing_aspect_toolchain_type` was not resolved",
    )


@buck_test()
async def test_bazel_aspect_rule_attrs_can_resolve_base_target_deps(buck: Buck) -> None:
    await buck.build("//:aspect_mid_go_proto")


@buck_test()
async def test_bazel_genrule_local_output_to_bindir_attrs(buck: Buck) -> None:
    await buck.build("//:local_output_to_bindir_genrule")
