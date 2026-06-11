load("//:rules/defs.bzl", "buck2_bundle", "pagable_transition_alias")
load("//rules:ci.bzl", "ci")

oncall("build_infra")

# Need a custom transition here so that buck2 is always built with pagable enabled,
# even if its parent does not have pagable enabled.
pagable_transition_alias(
    name = "bz",
    actual = "//app/bz:bz-bin",
    labels = [ci.aarch64(ci.skip_test())],
)

buck2_bundle(
    name = "buck2_bundle",
    buck2 = "//:bz",
    buck2_client = "//app/bz:bz_client-bin",
    buck2_health_check = None,
    tpx = None,
    visibility = ["PUBLIC"],
)

# For backcompat with bash aliases and so forth
# You can use this target to test custom builds of buck2.
#
# Step 1: `buck2 build --modifier opt //:symlinked_buck2_and_tpx --out ~/buck2`
# Step 2: Use the buck2 binary from `~/buck2/buck2`
#
# If you're testing on macOS, use the appropriate local platform modifier.
alias(
    name = "symlinked_buck2_and_tpx",
    actual = ":buck2_bundle",
)
