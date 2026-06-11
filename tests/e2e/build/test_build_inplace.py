# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-strict


import json
import sys
from pathlib import Path

from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.asserts import expect_failure
from buck2.tests.e2e_util.buck_workspace import buck_test, get_mode_from_platform
from buck2.tests.e2e_util.helper.utils import json_get


@buck_test(inplace=True)
async def test_sh_binary_no_append_extension(buck: Buck) -> None:
    target = "//tests/targets/rules/shell:no_extension"
    args = [target, "--show-full-output", get_mode_from_platform()]
    result = await buck.build(*args)
    output_dict = result.get_target_to_build_output()
    output = Path(output_dict[target])

    # Verify that we created the script symlink without an extension
    assert (output.parent / "resources" / "no_extension").is_symlink()

    # And that we're calling it without an extension as well
    last_script_line = output.read_text().splitlines()[-1]
    if sys.platform == "win32":
        assert "%BUCK_PROJECT_ROOT%\\no_extension %*" in last_script_line
    else:
        assert '"$BUCK_PROJECT_ROOT/no_extension" "$@"' in last_script_line


@buck_test(inplace=True)
async def test_build_test_dependencies(buck: Buck) -> None:
    target = "//tests/targets/rules/sh_test:test_with_env"
    build = await buck.build(
        target,
        "--build-test-info",
        "--build-report",
        "-",
    )
    report = build.get_build_report().build_report

    path = ["results", target, "other_outputs"]
    for p in path:
        report = report[p]

    has_file = False
    for artifact in report:
        if "__file__" in artifact:
            has_file = True

    assert not has_file


@buck_test(inplace=True)
async def test_missing_outputs_error(buck: Buck) -> None:
    # Check that we a) say what went wrong, b) show the command
    await expect_failure(
        buck.build(
            "//tests/targets/rules/genrule/bad:my_genrule_bad",
            # We really should make this an isolated test to avoid having to set this.
            "-c",
            "build.use_limited_hybrid=True",
            "--remote-only",
        ),
        stderr_regex="(Action failed to produce output.*frecli|frecli.*OUTMISS)",
    )

    # Same, but locally.
    await expect_failure(
        buck.build(
            "//tests/targets/rules/genrule/bad:my_genrule_bad_local"
        ),
        stderr_regex="Action failed to produce outputs.*Stdout:\nHELLO_STDOUT.*Stderr:\nHELLO_STDERR",
    )


@buck_test(inplace=True)
async def test_local_execution(buck: Buck) -> None:
    target = "//tests/targets/rules/genrule:echo_pythonpath"

    await buck.kill()
    res = await buck.build(target, env={"PYTHONPATH": "foobar"})

    build_report = res.get_build_report()
    output = build_report.output_for_target(target)
    assert output.read_text().rstrip() == ""


# In case of timeouts and failures, best would be to just disable this test.
@buck_test(inplace=True, skip_for_os=["windows"])
async def test_asic_platforms(buck: Buck) -> None:
    target = "//tests/targets/asic_platforms:uses_asic_tool"
    result = await buck.build(
        target,
        "--show-full-output",
    )
    output = result.get_target_to_build_output()[target]
    with open(output) as output:
        s = output.read()
        assert "example.com" in s, "expected 'example.com' in output: `{}`".format(
            output
        )


@buck_test(inplace=True)
async def test_genrule_with_remote_execution_dependencies(buck: Buck) -> None:
    result = await buck.build(
        get_mode_from_platform(),
        "//tests/targets/rules/genrule/re_dependencies:remote_execution_dependencies",
        "--config",
        "build.default_remote_execution_use_case=buck2-testing",
        "--no-remote-cache",
        "--remote-only",
        "--show-full-output",
    )
    output_dict = result.get_target_to_build_output()
    for _target, output in output_dict.items():
        with Path(output).open() as f:
            deps = json.load(f)
        assert len(deps) == 1
        assert deps[0]["smc_tier"] == "noop"
        assert deps[0]["id"] == "foo"
        # reservation_id is a random string which is 20 characters long
        assert len(deps[0]["reservation_id"]) == 20


async def read_io_provider_for_last_build(buck: Buck) -> None:
    log = (await buck.log("show")).stdout
    for line in log.splitlines():
        io_provider = json_get(
            line,
            "Event",
            "data",
            "SpanStart",
            "data",
            "Command",
            "metadata",
            "io_provider",
        )
        if io_provider:
            return io_provider

    raise Exception("Could not find io_provider")
