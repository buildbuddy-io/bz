load("@rules_go//go:def.bzl", "go_library")

go_library(
    name = "picker",
    srcs = ["picker.go"],
    importpath = "bz.build/cli/picker",
    visibility = ["//visibility:public"],
    deps = [
        "@com_github_charmbracelet_bubbles//help",
        "@com_github_charmbracelet_bubbles//key",
        "@com_github_charmbracelet_bubbletea//:bubbletea",
        "@com_github_charmbracelet_lipgloss//:lipgloss",
    ],
)
