# Status

_Last updated: 2026-06-17 04:50 UTC_

## Summary

Built `bz` from source and ran the build-loop across 12 repos/projects spanning
rules_cc, rules_python/pybind, rules_java, rules_jvm_external, rules_go, rules_rust,
rules_js (JS/TS), rules_oci, rules_proto, rules_kotlin, custom Starlark rules, and a
huge multi-language repo. **20 `bz` bugs found, fixed, verified, and committed; 9
deeper ones documented and deferred.** Ecosystems validated end-to-end: **C++, Python, Java,
Maven, Go, Rust** (build + run + test — `bz test` now works after F31 across cc/python/
rust); **JS/TS** largely works (~1,664 actions before a deferred copy-to-bin gap). Custom Starlark rule-authoring APIs: **17/19
bazel-examples/rules examples build**.

## Bugs fixed & committed (20)

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
| F18 | NODEP_LABEL string attr accepts `Label` | frontend (rules_js) |
| F19 | toolchain key apparent↔canonical repo alias | frontend (rules_js) |
| F22 | `ctx.rule.files`/`file`/`executable` for aspects | rules aspect example |
| F23 | `File` artifacts comparable (`sorted([files])`) | rules custom-rule examples |
| F25 | bundled `bazel_tools//tools/java` (java_stub_template) | rules_kotlin |
| F26 | `ctx.actions.run` `input_manifests` | rules_kotlin |
| F27 | `FilesToRunProvider` (no exe) in `actions.run` tools | rules_kotlin |
| F28 | `java_common_internal.check_java_toolchain_is_declared_on_rule` | rules_kotlin |
| F31 | test runfiles tree missing from test action inputs (`bz test`) | abseil cc_test execution |

## Documented / deferred (9 — deeper)

| ID | Issue | Why deferred |
| --- | --- | --- |
| F5 | bare native cc rules unimplemented | autoload to rules_cc; modern repos load explicitly |
| F9 | android `config_feature_flag` undefined | android ecosystem; protobuf graph only |
| F10 | `linkstatic=0` drops cc_library deps | deep cc dynamic-linking internals |
| F12 | go `//...` shared-action conflict (narrow) | config-transition output-path dedup; specific targets work |
| F16 | rules_oci/tar `layer_mtree` output not found | deep rules_oci/tar container-image path |
| F17 | `local_path_override` outside project root | bz path model is project-rooted; setup-specific |
| F20 | zlib header path in proto/protobuf transitive build | deep transitive-dep materialization |
| F21 | `ctx.outputs.executable` (executable/test rules; kt_jvm_binary — F29) | needs lazy predeclared-output value; single hardest remaining fix |
| F24 | copy-to-bin double-bind in js_binary runfiles | aspect_bazel_lib copy dedup; JS/TS ~1,664 actions |

## Repos tested

| Repo | Ecosystem | Result |
| --- | --- | --- |
| abseil-cpp | rules_cc | ✅ `//...` full build |
| re2 | rules_cc + pybind | ✅ core lib + Python bindings (only emscripten app blocked) |
| protobuf | multi-language | ⏸ F6/F7/F8 fixed; deferred at F9 (android) |
| googletest | rules_cc | ✅ all but 1 `linkstatic=0` target (F10) |
| bazel-examples/java-tutorial | rules_java | ✅ full build (remotejdk) |
| bazel-examples/go-tutorial | rules_go | ✅ single + multi-package build+run (specific targets); `//...` hits F12 |
| bazel-examples/java-maven | rules_jvm_external + rules_oci | ✅ Maven+Java (F13/F15 fixed); OCI image F16 |
| bazel-examples/cpp-tutorial | rules_cc | ✅ all stages build+run (no bugs) |
| rules_rust (standalone) | rules_rust | ✅ binary+library+test build, run, pass |
| bazel-examples/frontend | rules_js (JS/TS) | ⏳ ~1,664 actions build (F18/F19 fixed); F24 copy-to-bin gap |
| bazel-examples/rules | custom Starlark rules | ✅ 17/19 examples build (only runfiles/test_rule fail — F21) |
| proto-standalone | rules_proto/protobuf | ⏸ F20 (zlib header, transitive) |
| kotlin-standalone | rules_kotlin (JVM) | ✅ kt_jvm_library compiles (F25–F28); kt_jvm_binary F29 (=F21) |

## Regression sweep (2026-06-17 03:50)

With all 19 fixes in one binary, re-ran a cross-ecosystem sweep — **all pass, no
regressions**: abseil (`//absl/strings`, `//absl/flags:flag_benchmark`), re2 (`//:re2`),
googletest (`//:gtest`,`//:gtest_main`), cpp-tutorial stage3 `//...`, go-tutorial
stage2 `//:print_fortune`, rust-standalone `//...`, custom rules
(`//aspect/...`,`//predeclared_outputs/...`,`//depsets/...`). The fixes are mutually
compatible and stable.

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
