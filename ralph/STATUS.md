# Status

_Last updated: 2026-06-17 01:00 UTC_

## Summary

Built `bz` from source and ran the build-loop across 9 repos/projects spanning
rules_cc, rules_python/pybind, rules_java, rules_jvm_external, rules_go, rules_rust,
rules_oci, and a huge multi-language repo. **11 `bz` bugs found, fixed, verified, and
committed; 6 deeper ones documented and deferred.** Ecosystems validated end-to-end:
**C++, Python, Java, Maven, Go (single-pkg), Rust** (build + run + test).

## Bugs fixed & committed (11)

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
| F13 | `ctx.actions.run` `unused_inputs_list` | java-maven (tar/oci) |
| F14 | tighten F3 source-coercion to bare names | java-maven (regression guard) |
| F15 | `repository_ctx.download(block=False)` | java-maven (rules_oci) |

## Documented / deferred (5 — deeper)

| ID | Issue | Why deferred |
| --- | --- | --- |
| F5 | bare native cc rules unimplemented | autoload to rules_cc; modern repos load explicitly |
| F9 | android `config_feature_flag` undefined | android ecosystem; protobuf graph only |
| F10 | `linkstatic=0` drops cc_library deps | deep cc dynamic-linking internals |
| F12 | go multi-package shared-action conflict | config-transition output-path dedup |
| F16 | rules_oci/tar `layer_mtree` output not found | deep rules_oci/tar container-image path |
| F17 | `local_path_override` outside project root | bz path model is project-rooted; setup-specific |

## Repos tested

| Repo | Ecosystem | Result |
| --- | --- | --- |
| abseil-cpp | rules_cc | ✅ `//...` full build |
| re2 | rules_cc + pybind | ✅ core lib + Python bindings (only emscripten app blocked) |
| protobuf | multi-language | ⏸ F6/F7/F8 fixed; deferred at F9 (android) |
| googletest | rules_cc | ✅ all but 1 `linkstatic=0` target (F10) |
| bazel-examples/java-tutorial | rules_java | ✅ full build (remotejdk) |
| bazel-examples/go-tutorial | rules_go | ✅ single-package builds+runs (F11); multi-pkg F12 |
| bazel-examples/java-maven | rules_jvm_external + rules_oci | ✅ Maven+Java (F13/F15 fixed); OCI image F16 |
| bazel-examples/cpp-tutorial | rules_cc | ✅ all stages build+run (no bugs) |
| rules_rust (standalone) | rules_rust | ✅ binary+library+test build, run, pass |

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
