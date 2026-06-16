# re2

- **Source:** https://github.com/google/re2 (shallow clone, `2025-11-05`)
- **Local path:** `~/work/re2`
- **Rule ecosystem:** rules_cc (C++), rules_python + pybind11 (Python bindings),
  bzlmod with abseil-cpp as a dep.

## Results

| Command | Result |
| --- | --- |
| `bz build //:re2` | ✅ builds `libre2.a` + `.so` (193 actions; pulls abseil via bzlmod) |
| `bz build //python:_re2` (pybind ext) | ✅ builds |
| `bz build //python:re2` (py_library) | ✅ builds (after F4) |
| `bz build //python:re2_test` (py_test) | ✅ builds |
| `bz build //app:_re2.js` (emscripten) | ❌ F5 + needs emcc (not installed) |

## Status: ✅ core + Python bindings BUILD with bz

Everything builds on this Linux VM except the WebAssembly demo `//app:_re2.js`,
which requires an Emscripten toolchain (not present) and also uses a bare
`cc_binary` (F5).

## Bugs surfaced

- **F4** — `py_internal.cc_helper` was None (rules_python py_library cc interop).
  **Fixed & verified** (Python targets now build).
- **F5** — bare native `cc_binary`/`cc_library`/`cc_test` (used without a
  `load(@rules_cc...)`) are unimplemented in bz; Bazel autoloads them from rules_cc.
  Surfaced by `//app:_re2.js`. Documented (not fixed — see FINDINGS). That target
  also needs Emscripten, so it's unbuildable here regardless.

## Minor observations

- `bz` does not support negative target patterns (`-//app/...` → parsed as cell
  alias `-`). Bazel supports these.
- `bz build //:all` treats `:all` as a literal target named `all` rather than the
  Bazel "all targets in package" wildcard. (`//...` works.)
- `//python` has Windows-only `_re2_copy_so_to_pyd` (.pyd) targets; building the
  whole `//python/...` tries to analyze them on Linux. Building the concrete
  Linux targets works.
