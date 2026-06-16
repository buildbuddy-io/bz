# Status

_Last updated: 2026-06-17 00:00 UTC_

## Now

- re2 ✅ core + Python bindings build. Moving to next repo: **protobuf**.

## Done

- Built `bz` binary (`bazel build //app/bz:bz`), wrapped at `~/bin/bz`.
- **abseil-cpp: `bz build //...` fully succeeds** after fixing F1, F2, F3.
- **re2: core lib + pybind extension + py_library/py_test build** after F4.
  Only `//app` (emscripten WASM) unbuildable — needs emcc + bare cc_binary (F5).

## Bugs fixed (committed): F1 exec_groups, F2 define_values, F3 .lds-in-deps, F4 cc_helper.
## Open: F5 bare native cc rules unimplemented (documented).

## Environment

- VM: Linux x86_64, 8 cores, 31 GiB RAM, 159 GiB free disk.
- Tools present: bazel + bazelisk (`/usr/local/bin`), gcc/cc, go, python3.
- `bz` binary: not yet built. Will live at `bazel-bin/app/bz/bz`.

## Progress log

- 2026-06-16 23:20 — Set up `ralph/` docs. Kicked off initial `bz` build.
- 2026-06-16 23:25 — `bz` built. Smoke-tested abseil-cpp: libs build, cc_test → F1.
- 2026-06-16 23:40 — F1 fix (exec_groups) implemented, verified, committed.
- 2026-06-16 23:45 — F2 (config_setting define_values) fixed, verified, committed.
- 2026-06-16 23:50 — F3 (source files in cc deps / .lds) root-caused in
  BazelLabelAttrType coercion; fix implemented, rebuilding to verify.

## Bugs fixed so far

1. **F1** exec_groups on AnalysisContext — committed.
2. **F2** config_setting define_values — committed.
3. **F3** source files in cc deps fail CcInfo check — pending verify.
