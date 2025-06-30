load("@rules_go//go:def.bzl", "go_library")

go_library(
    name = "claude",
    srcs = ["claude.go"],
    importpath = "bz.build/cli/claude",
    deps = [
        "@com_github_anthropics_anthropic_sdk_go//:go_default_library",
    ],
)

package(default_visibility = ["//cli:__subpackages__"])
