# google/benchmark (real-world C++ library)

- **Source:** https://github.com/google/benchmark (shallow clone)
- **Ecosystem:** rules_cc (C++), rules_python (python tools using pip/scipy).
- A widely-used real-world dependency — good breadth validation beyond examples.

## Results — ✅ C++ builds + tests pass

| Target | Result |
| --- | --- |
| `//:benchmark`, `//:benchmark_main` (cc lib) | ✅ build (20 actions) |
| `//test:basic_test` (cc_test) | ✅ build + **`bz test` Pass 1/Fail 0** |
| `//tools:gbench` (py_binary, scipy pip dep) | ❌ F30 (pip version-matching select) |

## Status: ✅ real-world C++ lib validates (build + test); python tools hit F30

The benchmark C++ library and its cc_tests build and run under bz. Only the
python tooling (`//tools`, which depends on `scipy` via pip) hits **F30** — the
rules_python pip version-matching select. Confirms F30 blocks **any repo with pip
deps**, not just the synthetic pip-standalone test. No new bugs in the C++ path.
