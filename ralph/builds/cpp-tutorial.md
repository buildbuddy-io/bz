# bazel-examples/cpp-tutorial

- **Source:** bazel-examples/cpp-tutorial (stage1/2/3)
- **Ecosystem:** rules_cc (explicit loads), multi-package C++.

## Results — ✅ CLEAN PASS (no bugs)

| Stage | Result |
| --- | --- |
| stage1 (`//main:hello-world`) | ✅ builds (3 actions) + runs → `Hello world` |
| stage2 (lib + bin) | ✅ builds (6 actions) |
| stage3 (multi-library) | ✅ builds (9 actions) |

No new bz bugs. Good validation that the normal C++ path (rules_cc, multi-package
libraries, cc_binary) is solid — the cc bugs found elsewhere (F1/F2/F3, F10) are all
edge cases (exec_groups, define_values, `.lds` in deps, `linkstatic=0`), not the
common path.
