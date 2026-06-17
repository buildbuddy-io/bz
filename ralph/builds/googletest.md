# googletest

- **Source:** https://github.com/google/googletest (shallow clone)
- **Local path:** `~/work/googletest`
- **Rule ecosystem:** rules_cc (clean loads), bzlmod (deps: abseil, re2).

## Results

| Command | Result |
| --- | --- |
| `bz build //:gtest //:gtest_main` | ✅ build (core libs + .so) |
| `bz build //:sample9_unittest //:sample10_unittest` | ✅ build (normal cc_test) |
| `bz build //googlemock/test:gmock_all_test` | ✅ builds |
| `bz build //:gtest_samples` | ❌ F10 — link drops `gtest_sample_lib` (linkstatic=0) |
| `bz build //...` | ❌ only `gtest_samples` fails; everything else builds |

## Status: ✅ builds except one `linkstatic = 0` target (F10)

Great validation that the earlier fixes (F1–F4) compound correctly — gtest/gmock
and all normal cc_tests build cleanly with no new bugs in the common path. The
single failure is `//:gtest_samples`, which uses `linkstatic = 0`; its deps-less
`gtest_sample_lib` is dropped from the link (see FINDINGS F10).

## Bugs surfaced

- **F10** — `linkstatic = 0` drops direct deps-less cc_library deps from the link.
  Documented with diagnostic (link params), deferred (deep cc-linking internals).
