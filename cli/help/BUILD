load("@rules_go//go:def.bzl", "go_library")

go_library(
    name = "help",
    srcs = ["help.go"],
    importpath = "bz.build/cli/help",
    deps = [
        "//cli/arg",
        "//cli/bazelisk",
        "//cli/command",
    ],
)

package(default_visibility = ["//cli:__subpackages__"])
