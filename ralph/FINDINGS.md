# Findings — bugs & incompatibilities in `bz`

Format per finding:

## F<N>: <short title>
- **Repo:** <which target surfaced it>
- **Symptom:** <error / behavior>
- **Root cause:** <analysis>
- **Fix:** <commit / description, or "config workaround in repo">
- **Status:** open / fixed / workaround

---

## F3: source files (e.g. `.lds`) in cc `deps` fail provider check
- **Repo:** abseil-cpp (`//absl/flags:flag_benchmark`, a cc_binary)
- **Symptom:** `Attribute requires a dep that provides 'CcInfo', but it was not
  found on '...flag_benchmark.lds'. Found these providers: DefaultInfo`.
- **Root cause:** rules_cc declares `deps = attr.label_list(allow_files =
  ALLOWED_FILES_IN_DEPS, providers = [CcInfo])` (`.lds`/`.ld` linker scripts are in
  ALLOWED_FILES_IN_DEPS). Bazel exempts source files matching `allow_files` from the
  provider requirement. bz models this attr as `AttrType::bazel_label(dep, source)`
  (a union), but `BazelLabelAttrType::coerce_item`
  (`app/bz_interpreter_for_build/src/bazel/attrs/label.rs`) **unconditionally**
  coerced every string as `dep` in bazel-compat cells, so a source file ran through
  dep-resolution and failed `check_providers`.
- **Fix:** In bazel-compat cells, for same-cell references (not `//`/`@`), try
  `source` coercion first (succeeds only for a real source file in the package
  listing) and fall back to `dep`. Matches Bazel's allow_files exemption.
- **Status:** fixing

---

## F2: `config_setting` rejects `define_values`
- **Repo:** abseil-cpp (`bz build //...`, e.g. `//absl/.../perfcounters`)
- **Symptom:** `Found 'define_values' extra named parameter(s) for call to config_setting`.
- **Root cause:** bz's `config_setting` (prelude `core_rules.bzl` decl +
  `configurations/rules.bzl` impl) supports `values`/`constraint_values`/`flag_values`
  but not Bazel's `define_values` attr (sugar for matching `--define K=V`).
- **Fix:** Added `define_values` dict attr; impl folds entries into per-define
  command-line build settings. bz doesn't model `--define`, so these conditions are
  inert (never match unless set) — correct default. Prelude is bundled into the bz
  binary (`//:prelude_sources`), so a rebuild is required.
- **Status:** fixing

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
