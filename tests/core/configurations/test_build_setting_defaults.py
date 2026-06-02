# pyre-strict

import json

from buck2.tests.e2e_util.api.buck import Buck
from buck2.tests.e2e_util.buck_workspace import buck_test


@buck_test()
async def test_config_setting_flag_values_use_build_setting_defaults(buck: Buck) -> None:
    out = await buck.cquery(
        "root//:true-default-test",
        "--output-attribute=labels",
    )
    q = json.loads(out.stdout)
    assert len(q) == 1
    assert list(q.values())[0]["labels"] == ["TRUE_DEFAULT"]

    out = await buck.cquery(
        "root//:false-default-test",
        "--output-attribute=labels",
    )
    q = json.loads(out.stdout)
    assert len(q) == 1
    assert list(q.values())[0]["labels"] == ["FALSE_DEFAULT"]
