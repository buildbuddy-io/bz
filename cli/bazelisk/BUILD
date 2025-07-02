load("@rules_go//go:def.bzl", "go_library")

# gazelle:resolve go github.com/bazelbuild/bazelisk/core @com_github_bazelbuild_bazelisk//core:go_default_library
# gazelle:resolve go github.com/bazelbuild/bazelisk/repositories @com_github_bazelbuild_bazelisk//repositories:go_default_library

go_library(
    name = "bazelisk",
    srcs = ["bazelisk.go"],
    importpath = "bz.build/cli/bazelisk",
    deps = [
        "@com_github_bazelbuild_bazelisk//config",
        "@com_github_bazelbuild_bazelisk//core:go_default_library",
        "@com_github_bazelbuild_bazelisk//repositories:go_default_library",
        "@com_github_creack_pty//:pty",
        "@com_github_mattn_go_isatty//:go-isatty",
    ],
)

package(default_visibility = ["//cli:__subpackages__"])
