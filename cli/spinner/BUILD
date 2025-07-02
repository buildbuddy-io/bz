load("@rules_go//go:def.bzl", "go_library")

go_library(
    name = "spinner",
    srcs = ["spinner.go"],
    importpath = "bz.build/cli/spinner",
    visibility = ["//visibility:public"],
    deps = [
        "@com_github_charmbracelet_bubbles//spinner",
        "@com_github_charmbracelet_bubbletea//:bubbletea",
        "@com_github_charmbracelet_lipgloss//:lipgloss",
    ],
)