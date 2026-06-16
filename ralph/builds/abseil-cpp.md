# abseil-cpp

- **Source:** https://github.com/abseil/abseil-cpp (shallow clone, `head`)
- **Local path:** `~/work/abseil-cpp`
- **Rule ecosystem:** rules_cc / C++ (bzlmod)

## Results

| Command | Result |
| --- | --- |
| `bz targets //absl/strings:strings` | ✅ resolves |
| `bz build //absl/strings:strings` | ✅ builds `libstrings.a` + `.so` (41 actions, ~13s first run) |
| `bz build //...` | ✅ **entire repo builds** (1152 actions, exit 0) after F1+F2+F3 |

## Status: ✅ FULLY BUILDS with bz

`bz build //...` completes successfully (libraries, cc_test, cc_binary benchmarks
incl. `@google_benchmark` external dep, linker scripts). Required three bz fixes.

## Bugs surfaced & fixed

- **F1** — `ctx.exec_groups` missing on `AnalysisContext` (cc_test). Fixed.
- **F2** — `config_setting` rejected `define_values` attr (perfcounters). Fixed.
- **F3** — source files (`.lds`) in cc `deps` failed `CcInfo` provider check. Fixed.

## Notes

- bz materializes bzlmod external cells (rules_cc, bazel_skylib, platforms, etc.)
  and downloads them from the Bazel registry on first build. Works out of the box.
- Observed it fetching `remote_java_tools_linux_aarch64` even on x86_64 — harmless
  but worth a follow-up (over-fetching platform variants?).
