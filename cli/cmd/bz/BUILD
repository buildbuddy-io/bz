load("@rules_go//go:def.bzl", "go_binary", "go_library")

go_library(
    name = "bz_lib",
    srcs = ["bz.go"],
    data = [
    ],
    importpath = "bz.build/cli/cmd/bz",
    visibility = ["//visibility:private"],
    deps = [
        "//cli/arg",
        "//cli/bazelisk",
        "//cli/claude",
        "//cli/command",
        "//cli/command/register",
        "//cli/help",
        "//cli/log",
        "@com_github_creack_pty//:go_default_library",
        "@com_github_mattn_go_isatty//:go_default_library",
    ],
)

go_binary(
    name = "bz",
    embed = [":bz_lib"],
    visibility = ["//visibility:public"],
)

go_binary(
    name = "bz-darwin-amd64",
    out = "bz-darwin-amd64",
    embed = [":bz_lib"],
    gc_linkopts = [
        "-s",
        "-w",
    ],
    goarch = "amd64",
    goos = "darwin",
    pure = "on",
    tags = ["manual"],
    visibility = ["//visibility:public"],
)

go_binary(
    name = "bz-darwin-arm64",
    out = "bz-darwin-arm64",
    embed = [":bz_lib"],
    gc_linkopts = [
        "-s",
        "-w",
    ],
    goarch = "arm64",
    goos = "darwin",
    pure = "on",
    tags = ["manual"],
    visibility = ["//visibility:public"],
)

go_binary(
    name = "bz-linux-amd64",
    out = "bz-linux-amd64",
    embed = [":bz_lib"],
    gc_linkopts = [
        "-s",
        "-w",
    ],
    goarch = "amd64",
    goos = "linux",
    pure = "on",
    tags = ["manual"],
    visibility = ["//visibility:public"],
)

go_binary(
    name = "bz-linux-arm64",
    out = "bz-linux-arm64",
    embed = [":bz_lib"],
    gc_linkopts = [
        "-s",
        "-w",
    ],
    goarch = "arm64",
    goos = "linux",
    pure = "on",
    tags = ["manual"],
    visibility = ["//visibility:public"],
)

go_binary(
    name = "bz-windows-amd64",
    out = "bz-windows-amd64.exe",
    embed = [":bz_lib"],
    goarch = "amd64",
    goos = "windows",
    pure = "on",
    tags = ["manual"],
    visibility = ["//visibility:public"],
)

package(default_visibility = ["//cli:__subpackages__"])
