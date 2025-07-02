load("@rules_go//go:def.bzl", "go_library")

go_library(
    name = "claude",
    srcs = ["claude.go"],
    importpath = "bz.build/cli/claude",
    deps = [
        "//cli/picker",
        "//cli/textarea",
        "@org_golang_x_term//:term",
    ],
)

package(default_visibility = ["//cli:__subpackages__"])
