# abseil-cpp

- **Source:** https://github.com/abseil/abseil-cpp (shallow clone, `head`)
- **Local path:** `~/work/abseil-cpp`
- **Rule ecosystem:** rules_cc / C++ (bzlmod)

## Results

| Command | Result |
| --- | --- |
| `bz targets //absl/strings:strings` | ✅ resolves |
| `bz build //absl/strings:strings` | ✅ builds `libstrings.a` + `.so` (41 actions, ~13s first run) |
| `bz build //...` | ❌ then ✅ — library targets all build; `cc_test` targets hit F1 |

## Bugs surfaced

- **F1** — `ctx.exec_groups` missing on `AnalysisContext` (cc_test). See FINDINGS.md.

## Notes

- bz materializes bzlmod external cells (rules_cc, bazel_skylib, platforms, etc.)
  and downloads them from the Bazel registry on first build. Works out of the box.
- Observed it fetching `remote_java_tools_linux_aarch64` even on x86_64 — harmless
  but worth a follow-up (over-fetching platform variants?).
