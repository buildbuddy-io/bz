# Findings — bugs & incompatibilities in `bz`

Format per finding:

## F<N>: <short title>
- **Repo:** <which target surfaced it>
- **Symptom:** <error / behavior>
- **Root cause:** <analysis>
- **Fix:** <commit / description, or "config workaround in repo">
- **Status:** open / fixed / workaround

---

## F21: `ctx.outputs.executable` missing for executable/test rules
- **Repo:** bazel-examples/rules (`//runfiles:tool`, `//test_rule`).
- **Symptom:** `Object of type 'struct' has no attribute 'executable'` at
  `runfiles/tool.bzl:43` (`output = ctx.outputs.executable`). Same on `//test_rule`.
- **Root cause:** for a rule declared `rule(executable=True)` (or `test=True`), Bazel
  predeclares an output artifact named after the target, exposed as
  `ctx.outputs.executable`. bz tracks `is_bazel_executable_rule` but does not add an
  `executable` entry to the `ctx.outputs` struct.
- **Scope:** affects executable/test custom rules that write to the predeclared
  executable (2 of the bazel-examples/rules examples). Executable rules that instead
  `declare_file` + `DefaultInfo(executable=...)` work (`//executable` passes).
- **Fix shape (non-trivial):** `declare_bazel_predeclared_outputs`
  (`app/bz_analysis/src/analysis/env.rs:662`) builds `ctx.outputs` from output attrs +
  implicit outputs; bz tracks `is_bazel_executable_rule()`. The catch: `//executable`
  sets `DefaultInfo(executable=<a different declared file>)` and never writes
  `ctx.outputs.executable`. Bazel makes the predeclared executable **optional**
  (produce it OR set DefaultInfo.executable), but bz's registry requires declared
  outputs to be bound — so naively predeclaring it would break that case. Needs
  optional-output-binding semantics. Documented; deferred.
- **Empirically confirmed:** eagerly predeclaring `ctx.outputs.executable` for
  executable/test rules made `//test_rule` pass but **regressed `//executable`** with
  `Artifact must be bound by now` (it sets `DefaultInfo(executable=<other file>)` and
  never produces the predeclared one). Reverted. The correct fix is a **lazy**
  `ctx.outputs.executable` — declared only when the rule accesses it — which requires
  making `ctx.outputs` a lazy value rather than a pre-built struct. (`runfiles` has a
  second, separate issue beyond `ctx.outputs.executable`.)
- **Status:** documented / open (deferred — needs lazy predeclared-output support).

## F23: `File` artifacts not comparable — `sorted([files])` fails
- **Repo:** bazel-examples/rules (`//predeclared_outputs`).
- **Symptom:** `Operation 'compare' not supported for types 'File' and 'File'` at
  `hash.bzl:44` (`sorted(ctx.outputs.hashes)`).
- **Root cause:** bz's `File` artifact types (`StarlarkDeclaredArtifact`,
  `StarlarkArtifact`) implemented `equals` but not `compare`, so `sorted()` on a list
  of File objects failed. Bazel's `File` is comparable (orders by path).
- **Fix:** Add a `compare` method on the `StarlarkArtifactLike` trait that orders by
  the artifact's bazel path (derived from the fingerprint), and wire it into both
  `File` StarlarkValue impls. `app/bz_build_api/.../artifact/`.
- **Status:** ✅ fixed & verified — `//predeclared_outputs` now builds. Custom-rules
  coverage 15/19 → 16/19.

## F22: `ctx.rule.files` missing in aspect context
- **Repo:** bazel-examples/rules (`//aspect`).
- **Symptom:** `Object of type 'struct' has no attribute 'files'` at
  `aspect/file_collector.bzl:18` (`for f in ctx.rule.files.srcs`).
- **Root cause:** bz's `ctx.rule` struct (`analysis_context_rule` in
  `.../rule_defs/context.rs`) exposed only `attr` and `kind`. Aspects read the
  attached rule's file views via `ctx.rule.files` / `ctx.rule.file` (mirroring
  `ctx.files`/`ctx.file` of a normal rule).
- **Fix:** Add `files`, `file`, `executable` to the `ctx.rule` struct. The aspect ctx
  is built by `analysis_actions_to_bazel_ctx_with_overrides` (the synthesized bazel
  ctx struct), not `analysis_context_rule` — derive the file views from the dep rule's
  attrs via `analysis_context_bazel_file_structs_from_attrs`. (Also added to the native
  `analysis_context_rule` for consistency.)
- **Status:** ✅ fixed & verified — `//aspect` now builds. Custom-rules 16/19 → 17/19.

## Coverage note (bazel-examples/rules custom Starlark rules)
**16/19 examples build** with bz (after F23). Failing: runfiles (F21),
test_rule (F21), aspect (F22). Good breadth of Starlark rule-authoring API support.

## F20: zlib header `zlib/include/crc32.h` not found (proto/protobuf transitive)
- **Repo:** standalone proto project (`proto_library` → needs protoc → protobuf →
  zlib). MODULE: rules_proto 7.1.0 + protobuf 29.0 + rules_cc.
- **Symptom:** after 49 build actions (protoc + deps compiling),
  `File not found: 'zlib+//zlib/include/crc32.h'. Included in BUILD.bazel but does
  not exist`, failing the proto descriptor-set action.
- **Analysis:** a BUILD in the zlib BCR module references the header at
  `zlib/include/crc32.h`, but bz's materialized zlib repo doesn't have it at that
  path (upstream zlib keeps `crc32.h` at the repo root). Likely a transitive-dep
  materialization detail (module `strip_prefix` / overlay path / include layout) in
  how bz lays out the zlib BCR module. Couldn't inspect further — external cells are
  virtual/bundled, not on disk.
- **Scope:** Blocks proto compilation (proto_library descriptor set) because it pulls
  protoc → protobuf → zlib. Consistent with protobuf-as-root being deferred (F9).
- **Status:** documented / open (deferred — deep transitive materialization).

## F25: bundled `bazel_tools` missing `tools/java` (java_stub_template)
- **Repo:** standalone rules_kotlin project (`kt_jvm_binary`).
- **Symptom:** `package 'bazel_tools//tools/java' has no build file` when building a
  `kt_jvm_binary` (needs `@bazel_tools//tools/java:java_stub_template.txt`).
- **Root cause:** bz's bundled `bazel_tools` cell (`cells/bazel_tools/`) has
  `tools/jdk` but not `tools/java`. In upstream Bazel,
  `@bazel_tools//tools/java:java_stub_template.txt` is a filegroup forwarding to
  `@rules_java//java/bazel/rules:java_stub_template.txt` (the stub moved to
  rules_java). rules_kotlin's kt_jvm_binary references the bazel_tools path.
- **Fix:** Add `cells/bazel_tools/tools/java/BUILD.bazel` with the forwarding
  filegroup, mirroring upstream. (Bundled cell → rebuild required.)
- **Status:** fixing

## F24: copy-to-bin double-bind in js_binary runfiles
- **Repo:** bazel-examples/frontend (rules_js js_binary → aspect_bazel_lib copy_to_bin).
- **Symptom:** `Attempted to bind an artifact which was already bound` at
  `copy_file.bzl:80` (`ctx.actions.run(outputs=[dst], ...)`), reached via
  `gather_runfiles` → `copy_js_file_to_bin_action` → `copy_file_to_bin_action`.
- **Analysis:** the same destination artifact is produced by two copy actions —
  either the same source file is copied to bin more than once (runfiles not deduped
  the way aspect_bazel_lib expects under bz) or two sources collapse to the same bin
  path. bz binds an output by exactly one action, so the second bind fails. Needs the
  copy_to_bin dedup mechanism + actual runfiles content to pin down (external cells
  are virtual, not on disk).
- **Scope:** JS/TS otherwise runs **~1,664 build actions** (TS compile, SWC,
  bundling) — only final js_binary runfiles gathering fails. Reached only after F19.
- **Status:** documented / open (deferred).

## F19: toolchain key matching doesn't resolve apparent repo aliases
- **Repo:** bazel-examples/frontend (rules_js → aspect_bazel_lib `copy_to_bin`).
- **Symptom:** `toolchain "@bazel_lib//lib:coreutils_toolchain_type" was not declared
  by this rule (internal error)` during js_binary runfiles gathering (after 1,632
  build actions succeeded).
- **Root cause:** `ctx.toolchains[key]` matching (`AnalysisToolchains` in
  `.../rule_defs/context.rs`) compares keys after only stripping a leading `@`
  (`normalize_key`). The access uses the **apparent** repo name `@bazel_lib//...`,
  while the declared toolchain is canonicalized (via `bazel_canonical_label_key`) to
  `aspect_bazel_lib+//...`. (The build cache shows both `bazel_lib++...` and
  `aspect_bazel_lib++...` repos.) Apparent ≠ canonical → no match.
- **Fix:** Make `keys_match` fall back (after exact match fails) to comparing the
  repo-relative `//package:name` parts when both keys carry an explicit non-empty
  repo prefix. This matches an apparent alias (`bazel_lib//lib:x`) against the
  canonical declaration (`aspect_bazel_lib+//lib:x`) without threading the cell
  resolver, and without conflating a root-cell `//pkg:name` with an external repo's
  same-named target. Low regression risk (only adds matches when exact fails).
- **Scope:** The JS/TS ecosystem otherwise **largely works** (1,632 actions: TS
  compile, SWC, bundling). This blocked targets that gather runfiles through
  copy_to_bin's coreutils toolchain.
- **Status:** ✅ fixed & verified — coreutils toolchain now resolves; build progresses
  past F19 (~1,664 actions) to a separate copy-to-bin issue (F24).

## F18: string attr (NODEP_LABEL) rejects a `Label` value
- **Repo:** bazel-examples/frontend (rules_js; aspect_bazel_lib `ape` toolchain regn).
- **Symptom:** `Error coercing attribute 'toolchain' of type 'attrs.string()' ...
  Expected 'str', but got 'Label (repr: @@ape+//ape/toolchain/info:diff)'` at
  `native.toolchain(...)`.
- **Root cause:** bz models Bazel's NODEP_LABEL (label-shaped, no dependency — e.g.
  the `toolchain` rule's `toolchain` attr) as `attrs.string()`. Its coercer only
  accepted strings (`value.unpack_str_err()`), but Bazel's NODEP_LABEL also accepts a
  `Label` object (which the aspect rule passes).
- **Fix:** Extend the string attr coercer (`.../coerce/attr_type/string.rs`): if the
  value is a `Label` (StarlarkProvidersLabel / StarlarkTargetLabel), store its
  canonical string form (round-trips for bz's resolution), mirroring the visibility
  coercer's pattern. Falls back to the original error otherwise.
- **Status:** ✅ fixed & verified — frontend build went from instant failure to 1,632
  build actions (JS/TS compile + bundle). Next gap is F19 (toolchain key alias).

## F17: `local_path_override` can't point outside the project root
- **Repo:** rules_rust in-tree example hello_world_no_cargo
  (`local_path_override(module_name="rules_rust", path="../..")`).
- **Symptom:** `expected a normalized path but got an un-normalized path instead:
  '../..'` at MODULE.bazel.
- **Root cause:** bz resolves the override path via
  `cell_project_path.join_normalized("../..")` into a project-relative
  `ForwardRelativePath`, which cannot represent a path that escapes the project root
  (no `..`). The example points two levels up (to the rules_rust source), i.e.
  outside the bz project (the example dir). Bazel allows local overrides to reference
  directories outside the workspace.
- **Scope:** Affects in-tree examples / monorepo setups using `local_path_override`
  to a parent/sibling dir. Standalone projects using registry deps are unaffected.
  Deep (bz's path model is project-rooted). Documented; deferred. Tested rules_rust
  via a standalone project instead (see builds/rules_rust.md).
- **Status:** documented / open (deferred)

## F16: rules_oci/tar `layer_mtree` output not found (OCI image path)
- **Repo:** bazel-examples/java-maven (`//:image`, rules_oci + aspect_bazel_lib tar).
- **Symptom:** `File not found: root//layer_mtree. Included in BUILD but does not
  exist` when completing `//:layer` (layer.tar) / `//:image`.
- **Analysis:** the tar/oci layer rule declares an `*_mtree` manifest intermediate
  that bz isn't materializing/tracking as a declared output (looked up as a missing
  source). Another gap in the rules_oci/tar (container-image) path, after F13 and
  F15 were fixed there. Deep + peripheral.
- **Status:** documented / open (deferred). Core java-maven (rules_jvm_external Maven
  resolution + rules_java) builds fine; only the OCI image packaging is affected.

## F15: `repository_ctx.download(block=False)` rejected (async download)
- **Repo:** bazel-examples/java-maven (rules_oci toolchain fetch).
- **Symptom:** `repository_ctx.download(block = False) is not supported because
  downloads are currently executed synchronously`.
- **Root cause:** `repository_ctx.download` / `download_and_extract` explicitly
  rejected `block=False` via `repository_ctx_reject_nonblocking_download`, even though
  the pending-download token infrastructure (`StarlarkPendingDownload` with `.wait()`,
  and `module_ctx_pending_download`) already exists and handles `block=False`.
- **Fix:** Remove the rejection from both repository_ctx download methods. bz still
  downloads synchronously; `block=False` now returns a pending-download token that
  resolves immediately (and `.wait()` returns the result), matching the existing
  token path. (The helper remains, still used by module_ctx.download + a unit test.)
- **Status:** ✅ fixed & verified — OCI toolchain fetch proceeds past the download;
  next OCI gap is F16 (`layer_mtree`).

## F14: F3 refinement — restrict source-first coercion to bare names
- **Repo:** bazel-examples/java-maven (rules_oci / aspect_bazel_lib `directory_path`).
- **Symptom:** `Expected directory to be a TreeArtifact ... but <source artifact
  image> is either a source file or does not exist` — `directory = ":image"` (an
  oci_image rule target) was resolved to a *source* artifact, so `.is_directory`
  was false.
- **Root cause:** the F3 fix tried `source` coercion first for ANY same-cell
  reference (anything not starting with `//`/`@`), which over-broadly included
  explicit `:label` deps. The oci `:image` target got coerced as a source instead
  of a dependency. (abseil's `:flag` deps survived only because no source file named
  `flag` exists; `:image` hit a path where source coercion succeeded.)
- **Fix:** Restrict F3's source-first attempt to **bare** relative names
  (`!looks_like_label` — no `:`, `@`, `//`), matching the non-bazel-compat branch.
  Keeps the abseil `.lds` fix (bare filename) and stops mis-coercing explicit labels.
- **Status:** ✅ refined & verified safe — abseil `.lds` (flag_benchmark) still builds.
  NOTE: this was first hypothesized as the OCI TreeArtifact cause, but clearing the
  cache exposed F15 *before* the TreeArtifact check, so whether the original OCI error
  was stale cache or a distinct issue is still unconfirmed. The F14 change is kept as
  a safe, conservative refinement of F3 regardless.
- **Lesson:** broadening coercion in bazel-compat cells is risky — keep source-first
  to bare names only.

## F13: `ctx.actions.run` rejects `unused_inputs_list`
- **Repo:** bazel-examples/java-maven (aspect_bazel_lib `tar.bzl`, via rules_oci).
- **Symptom:** `Found 'unused_inputs_list' extra named parameter(s) for call to run`
  at `tar.bzl:395` (`ctx.actions.run(..., unused_inputs_list = unused_inputs_file)`).
- **Root cause:** bz's `ctx.actions.run` (`app/bz_action_impl/src/context/run.rs`)
  did not accept Bazel's `unused_inputs_list` param (an input-pruning hint: a file
  the action writes listing unused inputs, for incremental pruning).
- **Fix:** Accept `unused_inputs_list` as a named param and ignore it — bz does not
  perform input pruning; the action still runs and produces its real outputs.
- **Status:** ✅ fixed & verified — build proceeds past the tar action.
- **Env note:** java-maven also needs a host `java` for coursier (the Maven
  resolver, run during repo fetch). No system JDK on this VM; ran with the bazel
  embedded JDK on PATH (`JAVA_HOME=.../embedded_tools/jdk PATH=$JAVA_HOME/bin:$PATH`).
  Not a bz bug — `--java_runtime_version` only sets the build toolchain, not repo-rule
  execution.

## F12: Bazel shared-action conflict for go_library deps (multi-package Go)
- **Repo:** bazel-examples/go-tutorial/stage2 & stage3 (`//:print_fortune` go_binary
  with `deps = ["//fortune"]`, a go_library).
- **Symptom:** `Internal error (stage: bazel_shared_action_conflict): Conflicting
  Bazel shared actions for output set 'buck-out/bin/cfg/fortune/fortune.a
  buck-out/bin/cfg/fortune/fortune.x'`.
- **Analysis:** go_binary applies a configuration transition to its deps; `//fortune`
  is compiled (rules_go `compilepkg`) into `buck-out/bin/cfg/fortune/fortune.a`. bz's
  Bazel-shared-action dedup sees two actions targeting the same output paths with
  non-equivalent keys (the `cfg` output dir collapses distinct configurations to one
  path, or the lib is analyzed in two near-identical configs). Single-package Go
  (stage1) has no dep transition and works fine.
- **Scope (refined):** Multi-package Go actually **works for specific targets** —
  `bz build //:print_fortune` builds and runs (`Your tests will pass.`) in stage2/3.
  The conflict only occurs with `bz build //...`, which builds `//fortune` in BOTH
  the default config (directly) AND `print_fortune`'s transitioned config; both
  collapse to `buck-out/bin/cfg/fortune/fortune.a` with different action keys. So the
  bug is narrow: a go_library built under two configs collides because bz's Bazel
  output path uses a generic `cfg` dir that doesn't encode the configuration.
- **Fix:** Not implemented — the real fix (config-encoded output paths, or keying the
  shared-action output set by config) is deep + risky (output-path layout). But Go is
  usable in practice via specific targets. Documented; deferred.
- **Status:** documented / open (deferred) — **narrower than first thought; Go
  multi-package works for specific-target builds.**

## F11: `cc_common.merge_cc_infos` missing — blocks rules_go
- **Repo:** bazel-examples/go-tutorial/stage1 (`//:hello`, a go_binary).
- **Symptom:** `Object of type 'namespace' has no attribute 'merge_cc_infos'` at
  rules_go `context.bzl:350`: `cc_common.merge_cc_infos(cc_infos = cc_infos)` —
  during analysis of `rules_go+//:stdlib` (hit by EVERY Go target).
- **Root cause:** bz's `cc_common` namespace
  (`bazel_cc_common_module` in `.../bazel/cc_info.rs`) implements the low-level
  toolchain/feature helpers but NOT the high-level CcInfo API: `merge_cc_infos`,
  `create_compilation_context`, `create_linking_context`, `compile`, `link`. bz
  models cc rules in buck2 style (`cells/prelude/cxx/`) and delegates Bazel cc rules
  to native impls, so it never needed these — but rules_go calls `merge_cc_infos`
  directly, outside the cc-rule delegation path.
- **Impact:** Blocks the entire rules_go ecosystem (stdlib analysis fails before any
  Go target builds). For pure-Go (no cgo) the merged list is empty, so an
  empty/single-aware `merge_cc_infos` would likely unblock it; a fully correct merge
  needs compilation/linking-context merge machinery bz doesn't currently have.
- **Fix:** Add `merge_cc_infos` to `bazel_cc_common_module` returning an **empty
  CcInfo** — the same stub strategy bz already uses for `java_common.merge` (the real
  cc info flows through bz's native cc rule delegation). Low-risk; unblocks rules_go
  analysis for the pure-Go (no cgo) case.
- **Status:** ✅ fixed & verified — go-tutorial/stage1 `//:hello` builds AND runs
  (`Hello, Bazel! 💚`). Multi-package Go now reaches F12.

## F10: `linkstatic = 0` drops direct cc_library deps from the link
- **Repo:** googletest (`//:gtest_samples`, a cc_test with `linkstatic = 0`)
- **Symptom:** link fails with `undefined reference to 'Factorial(int)'`,
  `'IsPrime(int)'`, `'MyString::Set(...)'`, `'Counter::Increment()'` — all symbols
  from `gtest_sample_lib` (sample1/2/4.cc).
- **Diagnostic:** the generated link params (`gtest_samples-0.params`) contain
  `-llibgtest_Umain` and `-llibgtest` (gtest/gtest_main linked dynamically) but **no
  reference at all to `gtest_sample_lib`** — and no `CppLink libgtest_sample_lib.so`
  action runs. The direct dep is silently dropped.
- **Correlation:** `gtest`/`gtest_main` have `deps`; the dropped `gtest_sample_lib`
  has **no `deps`** (only srcs/hdrs). Under `linkstatic = 0` (dynamic mode), bz/
  rules_cc builds & links a `.so` for libraries with deps but omits a deps-less
  cc_library entirely (neither a dynamic `.so` nor its static pic archive lands on
  the link line).
- **Scope:** googletest otherwise builds **fully** (core gtest/gmock libs + all
  normal `linkstatic`-default tests incl. sample9/sample10, gmock_all_test). Only
  this one `linkstatic = 0` target fails.
- **Fix:** Not implemented — lives in bz's cc dynamic-linking / LibraryToLink
  construction (one of the most complex compat areas). Documented with diagnostic;
  deferred. Workaround: `linkstatic = 1` on the affected target.
- **Status:** documented / open (deferred)

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
- **Status:** ✅ fixed & verified (committed)

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
- **Status:** ✅ fixed & verified (committed)

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
- **Status:** ✅ fixed & verified (committed)

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
- **Status:** ✅ fixed & verified (committed)

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
- **Status:** ✅ fixed & verified (committed)

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
- **Status:** ✅ fixed & verified (committed)

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
