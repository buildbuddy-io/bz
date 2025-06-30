load("@rules_go//go:def.bzl", "go_library")

go_library(
    name = "register",
    srcs = ["register.go"],
    importpath = "bz.build/cli/command/register",
    visibility = ["//visibility:public"],
    deps = [
        "//cli/command",
        "//cli/please",
    ],
)

package(default_visibility = ["//cli:__subpackages__"])
