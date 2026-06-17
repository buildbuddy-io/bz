# Findings — bugs & incompatibilities in `bz`

Format per finding:

## F<N>: <short title>
- **Repo:** <which target surfaced it>
- **Symptom:** <error / behavior>
- **Root cause:** <analysis>
- **Fix:** <commit / description, or "config workaround in repo">
- **Status:** open / fixed / workaround

---

## F9: `config_feature_flag` native rule not defined (Android)
- **Repo:** protobuf (`//:protoc`).
- **Symptom:** `Variable 'config_feature_flag' not found` while evaluating
  `rules_android++android_sdk_repository_extension+androidsdk//:BUILD.bazel`.
- **Root cause:** protobuf's module graph pulls in rules_android, whose generated
  `androidsdk` repo BUILD.bazel uses the Android native rule `config_feature_flag`,
  which bz does not define. bz appears to evaluate this repo's BUILD even for a pure
  C++ `protoc` build (toolchain enumeration), so the gap blocks protoc.
- **Impact / decision:** Android-specific + deep. protobuf is a huge multi-language
  repo (cc/java/python/kotlin/ruby/rust/android); its full graph surfaces many
  peripheral-ecosystem gaps (F6, F7, F8 already fixed; F9 = android). Documented and
  deferred — pivoting to cleaner repos for breadth; revisit protobuf/android later.
- **Status:** documented / open (deferred)

## F8: `Label()` rejects bare relative labels
- **Repo:** protobuf (`//:protoc`; transitively evaluates rules_kotlin bzlmod setup).
- **Symptom:** `Error parsing target pattern 'capabilities_2.3.bzl', expected an
  absolute pattern` at rules_kotlin `templates.bzl:17`:
  `Label("capabilities_2.3.bzl")`.
- **Root cause:** bz's `Label()` (`parse_providers_label` in
  `app/bz_interpreter_for_build/src/label.rs`) handled `:target` (current-package
  relative) but not a bare relative name (no `@`, no `//`, no `:`). Bazel resolves
  `Label("foo.bzl")` as a target in the calling file's package, like `:foo.bzl`.
- **Fix:** Add a bare-relative branch: treat `Label("foo")` as `<current_package>:foo`.
- **Status:** fixing

## F7: `repository_ctx.getenv` missing (only on module_ctx)
- **Repo:** protobuf (`//:protoc`; transitively evaluates rules_android's
  `android_sdk_repository`).
- **Symptom:** `Object of type 'repository_ctx' has no attribute 'getenv'` at
  `rules_android/.../android_sdk_repository/rule.bzl:97`:
  `android_sdk_path = repo_ctx.getenv("ANDROID_HOME")`.
- **Root cause:** bz implemented `getenv` on the module-extension ctx
  (`StarlarkModuleExtensionContext`) but not on the repository-rule ctx
  (`StarlarkRepositoryContext`) in `app/bz_interpreter_for_build/src/bazel/
  repository.rs`.
- **Fix:** Add `getenv(name, default=None)` to `repository_context_methods`,
  mirroring the module_ctx version (reads + records the env var via
  `record_repository_env_var` so the repo refetches on change).
- **Status:** fixing

## F6: override patch label by root module's own repo_name rejected
- **Repo:** protobuf (`bz build //:protoc`, fails during MODULE.bazel eval)
- **Symptom:** `single_version_override patch must be a root-module label, got
  '@com_google_protobuf//:Disable_bundle_install.patch'`.
- **Root cause:** protobuf's `module(... repo_name = "com_google_protobuf")`, so
  `@com_google_protobuf//:...` is the root module referring to itself by its own
  apparent repo name. bz's `module_include_to_path`
  (`app/bz_common/src/bazel/bzlmod/module_file.rs`) only accepted `//`, `@//`, `@@//`
  for override patch labels, not `@<root_repo_name>//`.
- **Fix:** Thread the root module's apparent repo names (module `name` +
  `repo_name`) into `module_include_to_path` and accept `@<name>//` / `@@<name>//`
  as root-module labels. Applied to single_version_override + archive_override.
- **Status:** fixing

## F5: bare native cc rules (no `@rules_cc` load) are unimplemented
- **Repo:** re2 (`//app:_re2.js`, a bare `cc_binary` with no load statement)
- **Symptom:** `fail: Unimplemented rule type 'cc_binary' for target '//app:_re2.js'`.
- **Root cause:** bz wires bare `cc_binary`/`cc_library`/`cc_test` globals
  (`cells/prelude/bazel/native_rules.bzl`) to buck2-style prelude impls that are NOT
  in `_implemented_rules`, so they resolve to `_unimplemented_impl`. bz expects cc
  rules to be loaded from `@rules_cc` (which works well). Bazel 7's autoloads rewrite
  bare cc rules to `@rules_cc//cc:*.bzl`, so bare usage builds there.
- **Impact:** Affects projects with old-style BUILD files that use cc rules without
  loading from rules_cc. Modern projects (incl. re2's main targets) load from
  rules_cc and are unaffected.
- **Fix:** Not implemented. Proper fix = autoload bare cc rules to rules_cc (or
  implement buck2-style cc rule impls) — a substantial change with cell-bootstrap
  complications. Documented for upstream. The re2 case also needs Emscripten, so it
  can't be fully verified on this VM anyway.
- **Status:** documented / open

---

## F4: `py_internal.cc_helper` is None (rules_python + cc)
- **Repo:** re2 (`//python:re2_test`, pybind11 Python bindings)
- **Symptom:** `Object of type 'NoneType' has no attribute
  'is_valid_shared_library_artifact'` at rules_python `common.bzl:454`.
- **Root cause:** rules_python does `cc_helper = getattr(py_internal, "cc_helper",
  None)` and calls `cc_helper.is_valid_shared_library_artifact(f)` for files in a
  py_library's cc deps. bz's `BazelPyInternal` (`app/bz_interpreter_for_build/src/
  bazel/python.rs`) exposed no `cc_helper`, so the getattr default `None` was used.
- **Fix:** Add a `cc_helper` attribute on `py_internal` returning a `BazelCcHelper`
  value implementing `is_valid_shared_library_artifact` (checks shared-library
  extensions + versioned `.so.N`). Other cc_helper methods (find_cpp_toolchain,
  is_stamping_enabled, get_static_mode_params...) used by py_binary/py_executable
  are not yet implemented — add if a later target needs them.
- **Status:** fixing

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
