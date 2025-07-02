load("@rules_go//go:def.bzl", "go_library")

go_library(
    name = "textarea",
    srcs = ["textarea.go"],
    importpath = "bz.build/cli/textarea",
    visibility = ["//visibility:public"],
    deps = [
        "@com_github_charmbracelet_bubbles//help",
        "@com_github_charmbracelet_bubbles//key",
        "@com_github_charmbracelet_bubbles//textarea",
        "@com_github_charmbracelet_bubbletea//:bubbletea",
        "@com_github_charmbracelet_lipgloss//:lipgloss",
    ],
)