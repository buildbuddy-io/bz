load("//:rules/defs.bzl", "bz_bundle", "pagable_transition_alias")
load("//rules:ci.bzl", "ci")

oncall("build_infra")

# Need a custom transition here so that bz is always built with pagable enabled,
# even if its parent does not have pagable enabled.
pagable_transition_alias(
    name = "bz",
    actual = "//app/bz:bz-bin",
    labels = [ci.aarch64(ci.skip_test())],
)

bz_bundle(
    name = "bz_bundle",
    bz = "//:bz",
    bz_client = "//app/bz:bz_client-bin",
    bz_health_check = None,
    tpx = None,
    visibility = ["PUBLIC"],
)

# You can use this target to test custom builds of bz.
#
# Step 1: `bz build --modifier opt //:symlinked_bz_and_tpx --out ~/bz`
# Step 2: Use the bz binary from `~/bz/bz`
#
# If you're testing on macOS, use the appropriate local platform modifier.
alias(
    name = "symlinked_bz_and_tpx",
    actual = ":bz_bundle",
)
