# Status

_Last updated: 2026-06-17 00:40 UTC_

## Summary

Built `bz` from source and ran the build-loop across 6 open-source repos spanning
rules_cc, rules_python/pybind, rules_java, rules_go, and a huge multi-language repo.
**8 `bz` bugs found, fixed, verified, and committed; 4 deeper ones documented and
deferred.**

## Bugs fixed & committed (8)

| ID | Fix | Surfaced by |
| --- | --- | --- |
| F1 | `ctx.exec_groups` on `AnalysisContext` | abseil cc_test |
| F2 | `config_setting` `define_values` attr | abseil perfcounters |
| F3 | source files (`.lds`) in cc `deps` | abseil flag_benchmark |
| F4 | `py_internal.cc_helper` | re2 pybind |
| F6 | root module self-ref in override patch labels | protobuf |
| F7 | `repository_ctx.getenv` | protobuf (rules_android) |
| F8 | bare relative `Label("foo.bzl")` | protobuf (rules_kotlin) |
| F11 | `cc_common.merge_cc_infos` | go-tutorial |

## Documented / deferred (4 — deeper)

| ID | Issue | Why deferred |
| --- | --- | --- |
| F5 | bare native cc rules unimplemented | autoload to rules_cc; modern repos load explicitly |
| F9 | android `config_feature_flag` undefined | android ecosystem; protobuf graph only |
| F10 | `linkstatic=0` drops cc_library deps | deep cc dynamic-linking internals |
| F12 | go multi-package shared-action conflict | config-transition output-path dedup |

## Repos tested

| Repo | Ecosystem | Result |
| --- | --- | --- |
| abseil-cpp | rules_cc | ✅ `//...` full build |
| re2 | rules_cc + pybind | ✅ core lib + Python bindings (only emscripten app blocked) |
| protobuf | multi-language | ⏸ F6/F7/F8 fixed; deferred at F9 (android) |
| googletest | rules_cc | ✅ all but 1 `linkstatic=0` target (F10) |
| bazel-examples/java-tutorial | rules_java | ✅ full build (remotejdk) |
| bazel-examples/go-tutorial | rules_go | ✅ single-package builds+runs (F11); multi-pkg F12 |

## Environment

- VM: Linux x86_64, 8 cores, 31 GiB RAM, ~159 GiB free disk.
- `bz` binary built via `bazel build //app/bz:bz`; wrapper at `~/bin/bz`.
- Tools: bazel/bazelisk, gcc/cc, go, python3. No system JDK (use
  `--java_runtime_version=remotejdk_21` for Java).
- Cloned repos under `~/work/`.

## Next candidates

- Deferred fixes worth revisiting: F12 (unblocks multi-package Go), F10
  (linkstatic=0), F5 (bare cc rules).
- More repos: grpc, envoy, a rules_rust project, java-maven (rules_jvm_external).
