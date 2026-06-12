# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-strict


from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.asserts import expect_failure
from buck2.tests.e2e_util.buck_workspace import buck_test, env
from buck2.tests.e2e_util.helper.utils import json_get


@buck_test()
@env("SANDCASTLE", "1")  # wait for logs to finish uploading
async def test_upload_re_logs(buck: Buck) -> None:
    # Build a trivial action
    await buck.build("root//:run")

    session_id = await extract_re_session_id(buck)
    await expect_failure(
        buck.debug("upload-re-logs", "--session-id", session_id),
        stderr_regex="No artifact upload endpoint is configured in this build",
    )


async def extract_re_session_id(buck: Buck) -> str:
    result = await buck.log("show")
    session_id = None
    for line in result.stdout.splitlines():
        session_id = json_get(
            line, "Event", "data", "Instant", "data", "ReSession", "session_id"
        )
        if session_id:
            break
    assert session_id is not None
    return session_id
