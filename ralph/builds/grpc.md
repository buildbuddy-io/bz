# grpc/grpc — flagship real-world proto + cc

- **Source:** https://github.com/grpc/grpc (shallow clone), bzlmod (`module(name = "grpc", version = "1.83.0-dev")`).
- **Ecosystem:** rules_cc + protobuf + rules_proto + aspect_bazel_lib toolchains + (transitively) rules_android, rules_swift, etc.

## Result — ⏸ progressed past F9, now blocked on F33

| Target | Result |
| --- | --- |
| `//:gpr` (core platform-support C lib) | ⏸ analysis: F9 (fixed) → F33 `local_config_platform` alias |

## Findings

1. **F9 (`config_feature_flag`) — FIXED here.** First attempt at `//:gpr` failed at
   `Variable config_feature_flag not found` while toolchain resolution evaluated the stub
   `rules_android .../androidsdk/BUILD.bazel`. Implemented `config_feature_flag` as a
   bazel-compat native rule (see FINDINGS F9). gpr now gets past it.

2. **F33 (`local_config_platform`) — NEW, deferred.** After F9, gpr fails at
   `unknown cell alias: local_config_platform` while a generated `aspect_bazel_lib`
   toolchains BUILD loads `@local_config_platform//:constraints.bzl`.
   `local_config_platform` is a Bazel built-in always-visible repo (like `bazel_tools`)
   that bz doesn't inject. Its `constraints.bzl` content matches bz's existing
   `host_platform` generator, but it also needs a `:host` `platform()` target. Core
   bzlmod-resolution change (touches every cell's repo mapping) — documented and deferred
   (FINDINGS F33). Blocks grpc and any repo using aspect_bazel_lib toolchains.

## Significance

- Confirms **F9 is broad** (not just protobuf/android): any module graph pulling rules_android
  hits it during toolchain resolution even for pure-C targets. Now fixed.
- **F33** is the next high-value, broadly-applicable blocker — aspect_bazel_lib is an
  extremely common dependency. Worth revisiting with supervision given its blast radius.
