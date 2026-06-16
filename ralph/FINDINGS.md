# Findings — bugs & incompatibilities in `bz`

Format per finding:

## F<N>: <short title>
- **Repo:** <which target surfaced it>
- **Symptom:** <error / behavior>
- **Root cause:** <analysis>
- **Fix:** <commit / description, or "config workaround in repo">
- **Status:** open / fixed / workaround

---

## F1: `ctx.exec_groups` missing on `AnalysisContext`
- **Repo:** abseil-cpp (`bz build //...`)
- **Symptom:** `cc_test` targets fail analysis with
  `Object of type 'AnalysisContext' has no attribute 'exec_groups'` at
  `rules_cc/.../cc_test_impl.bzl:56`:
  `cc_test_toolchain = ctx.exec_groups["test"].toolchains[_CC_TEST_TOOLCHAIN_TYPE]`.
  Library targets build fine; only test targets break.
- **Root cause:** bz's native `AnalysisContext` (`analysis_context_methods` in
  `app/bz_build_api/src/interpreter/rule_defs/context.rs`) implements many Bazel
  ctx attributes but not `exec_groups`. (`AnalysisActions` had a stub returning an
  empty dict, but the rules_cc delegation path passes the native `AnalysisContext`.)
  bz also entirely ignores `exec_groups=` in `rule()` (it's in `_unused`).
- **Key behavior:** rules_cc's cc_test does
  `tc = ctx.exec_groups["test"].toolchains[TYPE]; if tc: <new flow> else: legacy`.
  So returning `None` for an unresolved toolchain routes to the legacy cc_test
  flow, which bz fully supports.
- **Fix:** Add `exec_groups` to `AnalysisContext` returning a `BazelExecGroups`
  collection. `[name]` yields a context whose `.toolchains[type]` returns the
  resolved toolchain or `None` (no error). Mirror on `AnalysisActions` + the
  synthesized bazel ctx struct for consistency.
- **Iter 2:** After adding `exec_groups`, rules_cc's `finalize_link_action.bzl:369`
  does `if "cpp_link" in ctx.exec_groups:` — the `in`/membership operator, which
  errored (`Operation 'in' not supported ... ExecGroupCollection`). Added `is_in`
  on `BazelExecGroups` returning `False` (bz has no named exec groups), which routes
  the rule to its `ctx.toolchains`-based fallback (the `elif` branch).
- **Status:** ✅ fixed & verified — `bz build //absl/types:variant_test` (a cc_test)
  builds successfully.
