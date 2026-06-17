# Findings — bugs & incompatibilities in `bz`

Format per finding:

## F<N>: <short title>
- **Repo:** <which target surfaced it>
- **Symptom:** <error / behavior>
- **Root cause:** <analysis>
- **Fix:** <commit / description, or "config workaround in repo">
- **Status:** open / fixed / workaround

---

## F34: bazelrc relative `import` that escapes the project root — ✅ FIXED
- **Repo:** rules_scala `examples/scala3` (`bz targets //...`).
- **Symptom:** `expected a normalized path but got an un-normalized path instead:
  '../../.bazelrc'` during "Parsing cells", from the example's `.bazelrc` containing
  `import ../../.bazelrc`.
- **Root cause:** the scala3 example is its own workspace root (has MODULE.bazel), so the
  relative import points two levels *above* the project root. `bazelrc_import_path`
  (`legacy_configs/cells.rs`) resolved relative imports via
  `ConfigPath::join_to_parent_normalized`, which keeps the path project-relative and errors
  when `..` segments escape the root (`ForwardRelativePathBuf::try_from` rejects the
  un-normalized `../../.bazelrc`). Bazel treats bazelrc imports as plain filesystem paths.
- **Fix:** on the escape (Err) case, fall back to resolving the import against the importing
  rc file's directory as an absolute `ConfigPath::Global` — mirroring the existing
  `%workspace%`-escapes-project branch (`resolve_project_relative_to_absolute`). Purely
  additive: in-project imports keep the original behavior.
- **Verification:** scala3 `.bazelrc` now resolves (build advances to MODULE.bazel, which
  then hits deferred F17 `local_path_override(path="../..")`). Regression-clean: abseil, re2.
- **Status:** ✅ fixed & verified (committed)

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
- **Root cause (regression from F3/F14!):** zlib's BCR overlay BUILD generates the
  prefixed headers via a genrule (`copy_public_headers`: `cp $(SRCS) $(@D)/zlib/
  include/`) and the cc_library's `hdrs = ["zlib/include/crc32.h", ...]` reference
  those **genrule outputs**. bz's source coercion has a non-existence-checking
  `coerce_path` fallback (source.rs:74), so my F14 "bare-name source-first" wrongly
  coerced the genrule output as a (non-existent) source → "Included in BUILD but does
  not exist". F3/F14's comment even wrongly claimed source coercion "only succeeds for
  a file present in the package listing".
- **The real tension (F3 ↔ F20):** in a bazel-compat `allow_files` label attr, a bare
  name can be EITHER a source file (`.lds`) OR a rule output (zlib's genrule-generated
  `zlib/include/crc32.h`). Source-first (F14) breaks genrule-outputs-in-`hdrs`;
  dep-first breaks source-files-in-cc-`deps`. The correct discriminator is "is it an
  existing source?" — but `ctx.coerce_existing_path` returns None even for the on-disk
  `flag_benchmark.lds` (bz's package listing at coerce time doesn't reliably contain
  it), so an existence-gated fix made everything dep-coerce → fixed proto/zlib but
  **regressed abseil `.lds`**. Reverted.
- **Verified:** with dep-coercion the proto-standalone build succeeds (284 actions) —
  so the genrule-output resolution is the only blocker; the fix just needs a reliable
  source-vs-output discriminator.
- **Why coerce_existing_path is unreliable (definitive):** for bazel-compat cells bz
  builds the package listing with a **`Shallow`/lazy strategy** — it starts EMPTY
  (`dice_calculation_delegate.rs:946`, `PackageListingStrategy::Shallow`) and only
  accrues files as globs/refs touch them during BUILD eval. So at attr-coerce time the
  listing is incomplete and `coerce_existing_path` can return None for an on-disk
  source. Coerce-time source-vs-output discrimination is therefore fundamentally
  unreliable in bz's current model.
- **Proper fix (architectural):** either (a) defer the source-vs-dep decision for
  `allow_files` labels to a phase where the package's full source + target listings
  are known (Bazel resolves this at loading over the whole package), or (b) make the
  bazel-compat listing eager/complete so `coerce_existing_path` is trustworthy.
- **Scope (BROADER than zlib):** any **generated source referenced by bare name in
  `srcs`/`hdrs`** hits this — confirmed on zlib (genrule headers → proto/protobuf/grpc)
  AND buildtools/buildifier (`build/parse.y.baz.go`, a **goyacc-generated Go source**
  in a go_library's `srcs`). Codegen (proto, yacc, generated headers) is everywhere,
  so this is one of the highest-impact deferred issues for real-world repos.
- **Better fix direction (than coerce-time discrimination):** the root cause is that
  Bazel exempts files matching `allow_files` from the attribute's provider requirement
  (F3's original insight). The CORRECT implementation is **dep-coerce the label** (which
  resolves generated files to their rule output AND source files to their source-file
  target) and **exempt file deps (DefaultInfo-only single-file targets) from the
  provider check when the attr allows files** — rather than the current "source-first"
  coercion (F14) that mis-treats generated files as missing sources. This needs the
  `allow_files` flag threaded into dep resolution / provider checking (a dep-or-file
  attr variant) — multi-layer, hence architectural. NOTE: dep-first is actually
  correct for the common case (generated headers in `hdrs` already worked under
  dep-coercion in the F20 experiment); the only regression was `.lds`-in-cc-`deps`,
  which the provider exemption would fix properly.
- **Status:** ✅ FIXED & verified — (1) always dep-coerce in bazel-compat (`label.rs`,
  removes the F14 source-first that mis-treated generated files); (2) the `allow_files`
  union's dep has no required providers (`attrs_global.rs`), exempting file deps from
  the provider check (Bazel's allow_files semantics). Supersedes F3/F14's coercion
  approach. Verified exhaustively: proto-standalone builds, buildifier 135→781 actions
  (now hits the separate F12), abseil `.lds` (F3) still builds, FULL regression sweep
  green (abseil //…, re2, googletest, cpp-tutorial, rust, custom-rules, java //…),
  `bz test` Pass, `bz query` works. **Highest-impact fix — unblocks codegen everywhere
  (proto/grpc/buildifier/generated headers+sources).**

## F28: `java_common_internal.check_java_toolchain_is_declared_on_rule` missing
- **Repo:** standalone rules_kotlin project (kt_jvm_library, via java_common).
- **Symptom:** `Object of type 'java_common_internal' has no attribute
  'check_java_toolchain_is_declared_on_rule'`.
- **Root cause:** rules_java/rules_kotlin call this java_common deprecation-check
  internal method, which bz's `JavaCommonInternal`
  (`.../provider/builtin/bazel/java_info.rs`) didn't implement.
- **Fix:** Add it as a no-op (bz resolves java toolchains itself).
- **Status:** ✅ fixed & verified — Kotlin build progresses past it; kt_jvm_library
  compiles. kt_jvm_binary then hits F29 (= F21, `ctx.outputs.executable`).

## F32: `bz query deps()` over cc targets — missing `third_party/def_parser`
- **Repo:** abseil-cpp (`bz query "deps(//absl/strings:strings)"`).
- **Symptom:** `package 'bazel_tools//third_party/def_parser' has no build file`. cc
  rules carry an implicit (Windows-only) reference to
  `@bazel_tools//tools/def_parser:def_parser` → `//third_party/def_parser:def_parser`,
  and `bz query deps()` traverses all select branches — so it's broken for ~all cc
  targets. (Builds are fine: the Linux build doesn't select that branch.)
- **Root cause:** bz's bundled `bazel_tools` ships `tools/def_parser` but not
  `third_party/def_parser` (the actual def_parser tool package).
- **Fix:** Add a minimal `third_party/def_parser` package (def_parser cc_library +
  cc_binary mirroring upstream, omitting the py_test that pulls in `//src/...`
  packages bz doesn't ship) + sources + `pkg_sources`, registered in
  `bazel_tools_sources`. Same class as F25.
- **Status:** fixing

## F31: `bz test` — test runfiles artifact not in action inputs
- **Repo:** abseil-cpp (`bz test //absl/types:variant_test`, a cc_test).
- **Symptom:** `Test execution request failed: Bazel test runfiles artifact was not
  present in action inputs (internal error)`. The test **builds** fine; only
  execution fails.
- **Root cause:** the test action's inputs (`Execute2RequestExpander::get_inputs`,
  `app/bz_test/src/orchestrator.rs`) were collected only from the test command line +
  env. The Bazel test's full runfiles tree (data files reachable only via runfiles)
  was not included, so `bazel_test_runfiles_inputs` couldn't find those artifacts in
  the ensured inputs.
- **Fix:** In `get_inputs`, also visit the test's runfiles artifacts
  (`bazel_info().for_each_runfiles_entry`) so the full runfiles tree is part of the
  action inputs before execution.
- **Status:** ✅ fixed & verified — `bz test` now passes across ecosystems:
  abseil variant_test (7 tests), re2 search/charclass/compile, gmock_all_test, re2
  python test, rust greeter_test — all Pass. Unblocked the whole test-execution path.

## F30: rules_python pip version-matching select fails (`_no_matching_repository`)
- **Repo:** standalone rules_python + pip project (py_binary with a PyPI dep `six`).
- **Symptom:** `None of 1 conditions matched configuration ... and no default was set:
  rules_python+//python/config_settings:is_not_matching_current_config` — via
  `pypi//six:pkg` → `pypi//six:_no_matching_repository`.
- **Analysis:** pip **resolution works** (six was fetched from PyPI). The pip package
  `:pkg` alias's `actual` is a `select()` over python-version config settings; bz's
  build configuration doesn't satisfy the expected `@rules_python//python/
  config_settings:python_version` value, so the select falls through to
  `_no_matching_repository` (which itself selects on `is_not_matching_current_config`
  with no default). Root cause: bz isn't propagating the module-extension-registered
  default `python_version` flag into the configuration that selects evaluate against.
- **Also:** bz's CLI rejects `--@repo//flag=value` build-setting syntax
  (`unexpected argument`), so the flag can't be set manually as a workaround either.
- **Scope:** pip dep RESOLUTION works; the multi-version config-flag plumbing is the
  gap. Moderately deep (config default propagation). **Broad impact:** blocks ANY repo
  with pip deps — confirmed on the synthetic pip-standalone test AND google/benchmark
  (`//tools:gbench` → scipy). Worth prioritizing for real-world Python repos.
- **Deeper analysis:** `is_not_matching_current_config` is generated by rules_python's
  `construct_config_settings` macro (per-registered-version config_settings keyed off
  the exact `python_version` value, e.g. `is_python_3.11.x`). The pip hub's `:pkg`
  alias selects on these; when none match it falls to `_no_matching_repository` →
  `is_not_matching_current_config`. bz's config doesn't carry the exact python_version
  the registered toolchain expects, so neither branch matches. Fixing it requires bz
  to resolve and apply rules_python's exact-version `python_version` flag value into
  the configuration that selects evaluate against — deep (rules_python multi-version
  machinery), not a quick flag-default tweak.
- **Status:** documented / open (deferred — high real-world impact, but architectural).

## F29: kt_jvm_binary blocked on `ctx.outputs.executable` (= F21)
- **Repo:** standalone rules_kotlin (`kt_jvm_binary`, impl.bzl:120
  `output = ctx.outputs.executable`).
- **Same root cause as F21.** kt_jvm_binary *writes* `ctx.outputs.executable`, so it
  needs the predeclared executable. Confirmed the eager-declare approach can't be used
  unconditionally: `bazel_test_info`/run handling calls `get_bound_artifact()` on the
  DefaultInfo executable, and the registry validates **all** declared artifacts — so a
  predeclared-but-unproduced executable (the `//executable` `DefaultInfo(executable=
  <other>)` case) errors "Artifact must be bound by now". The fix is a **lazy**
  `ctx.outputs.executable`: a custom `ctx.outputs` value that declares the executable
  via the registry only when accessed (memoized), so rules that write it (kt_jvm_binary,
  runfiles, test_rule) bind it while rules that don't (//executable) never declare it.
- **Scope:** rules_kotlin: kt_jvm_library ✅ compiles (F25–F28 fixed); kt_jvm_binary
  blocked here. Also unblocks custom-rules runfiles+test_rule (→ 19/19).
- **Exact blocker:** `ActionsRegistry::finalize()`
  (`app/bz_build_api/src/actions/registry.rs:421`) calls `ensure_bound()` on every
  declared artifact, so any eagerly-declared executable MUST be bound. Two viable
  fixes, both **core changes to action finalization** (affect all builds → need
  review + thorough testing):
  1. An "optional artifact" flag threaded env.rs → AnalysisRegistry → ActionsRegistry
     so `finalize()` skips unbound optional outputs (the predeclared executable).
  2. A lazy `ctx.outputs` value (custom StarlarkValue) that declares the executable
     via the registry only on access (memoized).
- **Status:** documented / open (deferred — single highest-value remaining fix.
  Intentionally NOT attempted unsupervised: a subtly-wrong change to `finalize()`
  could mask real unbound-output errors or break the action graph across all builds.
  Flagged for a maintainer with the exact fix shapes above.)

## F27: `FilesToRunProvider` (no executable) rejected in `actions.run` tools
- **Repo:** standalone rules_kotlin project (kt_jvm_library compile action).
- **Symptom:** `expected hidden action input/tool to be a command-line value,
  sequence, or depset, got 'struct'` — `tools = [kotlinbuilder.files_to_run,
  kotlin_home.files_to_run]`.
- **Root cause:** bz's hidden-input/tool handling (`add_bazel_hidden_value` in
  `cmd_args/typ.rs`) extracted only the **executable** from a FilesToRunProvider
  struct. `kotlin_home` is a filegroup with no executable, so the check returned
  None and the struct fell through to the error. Its files live in the struct's
  `runfiles`, which was ignored.
- **Fix:** Detect a FilesToRunProvider via its executable/runfiles fields and add
  both the executable (if any) and the files from its runfiles.
- **Status:** ✅ fixed & verified — Kotlin build progresses past it to F28.

## F26: `ctx.actions.run` rejects `input_manifests`
- **Repo:** standalone rules_kotlin project (kt_jvm_binary, after F25).
- **Symptom:** `Found 'input_manifests' extra named parameter(s) for call to run`.
- **Root cause:** bz's `ctx.actions.run` (`app/bz_action_impl/src/context/run.rs`)
  didn't accept Bazel's deprecated `input_manifests` param (runfiles manifests for the
  action's tools). Same class as F13 (`unused_inputs_list`).
- **Fix:** Accept `input_manifests` and ignore it — bz tracks tool runfiles
  automatically.
- **Status:** ✅ fixed & verified — Kotlin build progresses past it to F27.

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
  filegroup, mirroring upstream. (Bundled cell → rebuild required.) Note: globs don't
  cross package boundaries, so the new package also needs a `pkg_sources` filegroup
  registered in `bazel_tools_sources`.
- **Status:** ✅ fixed & verified — Kotlin build progresses past the missing package to
  compilation, then F26 (`input_manifests`).

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

## F38: `data` files absent from cc_test runfiles (tests with fixtures can't find data)
- **Repo:** boringssl (`//:crypto_test`).
- **Symptom:** crypto_test builds (78 actions) and runs but 32/32 shards fail: every test that
  reads a data fixture prints `Could not open '.../crypto_test.runfiles/boringssl/crypto/
  cipher/test/cipher_tests.txt'` (and the wycheproof `third_party/.../*.txt` vectors). Tests
  with no data pass (ssl_test: 471 tests Pass 1/Fail 0 — it has no `data`).
- **Evidence:** the materialized `crypto_test.runfiles/MANIFEST` has exactly **one** line
  (`_main/crypto_test crypto_test`) — i.e. only the executable. The `data = crypto_test_data`
  files (hundreds of `.txt` test vectors) are entirely missing from the runfiles tree/manifest.
  ssl_test's manifest, by contrast, has the binary + shared libs (6 entries) — runfiles for
  deps/`.so` work; only `data` source files are dropped.
- **Root cause (PRECISELY located via diagnostics):** a **source file target's runfiles are
  empty** in bz. `DefaultInfo::for_file_target` / `FrozenDefaultInfo::for_file_target`
  (`bz_build_api/.../provider/builtin/default_info.rs`, ~L981 and ~L1081) put the artifact in
  `files` but set `data_runfiles`/`default_runfiles` to **empty**. In Bazel a source file
  target's runfiles include the file itself. rules_cc's `cc_test` builds its runfiles by
  **merging the `default_runfiles` of its `srcs`/`data`/`deps`** (collect_default-style), so the
  empty source-`data` runfiles contribute nothing → the data files never reach the runfiles
  tree. Confirmed with minimal custom-rule repros: `ctx.files.data` = `[fixture.txt]` ✓;
  `ctx.runfiles(files=ctx.files.data).files` = `[fixture.txt]` ✓ (explicit works); but
  `ctx.runfiles(collect_data=True)` = `[]` and a source dep's `DefaultInfo.default_runfiles` =
  `[]` ✗ (the gap).
- **Impact:** broad — any test that reads fixtures / golden files / test vectors via `data`
  (an enormous fraction of real-world test suites) will run but fail to open its data.
- **ATTEMPTED & REVERTED (2026-06-17):** made `for_file_target` (both variants) put the artifact
  into `data_runfiles`/`default_runfiles`. Result: crypto_test runfiles manifest went **1 → 853
  entries and now includes the `.txt` data**; `collect_data` repro fixed; full regression sweep
  green (abseil/re2 build, abseil/rust/ssl_test pass). **But** (a) it over-collected — cc_test
  also merges `srcs` runfiles, so all `.cc` srcs leaked into the runfiles tree (every source file
  in every build now self-runfiles — broadest possible blast radius, not clearly Bazel-accurate
  for `srcs`), and (b) crypto_test **still failed** because of F39 below (runfiles prefix). Reverted
  to protect the base.
- **Correct fix shape (deferred — needs supervision):** make the `data` attribute contribute its
  files to runfiles **scoped to `data`** (not all source files), and pair with F39. Either give
  source files self-runfiles ONLY where Bazel does and ensure cc_test's `srcs` merge excludes
  them, or add `data` files to the rule's runfiles at the DefaultInfo auto-collection layer. Must
  verify no `srcs` over-collection and no runfiles symlink conflicts in large repos.
- **Status:** documented / open (deferred — blocks tests with `data` fixtures; coupled with F39)

## F39: test runfiles use `_main/` prefix but tests resolve by module name (no `_repo_mapping`)
- **Repo:** boringssl (`//:crypto_test`) — surfaced while working F38.
- **Symptom:** with data in the runfiles tree (during the F38 attempt), the test still failed:
  it opens `crypto_test.runfiles/boringssl/crypto/.../*.txt` but the manifest stages entries under
  `_main/crypto/...` (e.g. `_main/crypto/blake2/blake2b256_tests.txt crypto/blake2/...`).
- **Root cause:** under bzlmod the main repo's runfiles dir is the canonical `_main`, but the test's
  C++ runfiles library calls `Rlocation("boringssl/...")` using the **module's apparent name**
  (`module(name = "boringssl")`). Bazel bridges this with a `_repo_mapping` runfiles file (apparent
  → canonical). bz's `bazel_test_runfiles_inputs` (`bz_test/src/orchestrator.rs`) writes the
  manifest with the `_main` workspace prefix (from `executable_runfiles_path`) and emits **no
  `_repo_mapping`**, so module-name rlocation lookups don't resolve.
- **Fix shape (deferred):** emit a `<test>.runfiles/_repo_mapping` file mapping apparent repo names
  (incl. the root module name) to canonical names, OR prefix the main-repo runfiles with the
  module name. Core runfiles/test machinery — defer with F38.
- **Status:** documented / open (deferred — blocks tests that resolve runfiles by module name)

## F37: `bazel_tools//tools/cpp/runfiles` missing from bundled cell — ✅ FIXED
- **Repo:** boringssl (`//:ssl_test`, `//:crypto_test`).
- **Symptom:** `package 'bazel_tools//tools/cpp/runfiles' has no build file; expected one of
  BUILD.bazel, BUILD` during analysis of a test that depends on
  `@bazel_tools//tools/cpp/runfiles:runfiles` (the C++ runfiles library).
- **Root cause:** bz embeds a `cells/bazel_tools/` tree into the binary, but the
  `tools/cpp/runfiles` package was absent. (Same class as F25 `tools/java` and F32
  `def_parser`.)
- **Fix:** added `cells/bazel_tools/tools/cpp/runfiles/{BUILD.bazel,runfiles.h}` as the modern
  deprecated forwarder to `@rules_cc//cc/runfiles` (matches Bazel 9.x; bz compat version
  9.1.0) — a `cc_library` shim + a `pkg_sources` filegroup, registered into
  `tools/cpp:pkg_sources` (globs don't cross package boundaries).
- **Verification:** boringssl `//:ssl_test` builds (447 actions) and passes (471 tests, Pass 1/
  Fail 0); `//:crypto_internal` compiles (369 actions, incl. x86 asm). Regression-clean:
  abseil/tcmalloc/re2 build, abseil + rust tests pass.
- **Status:** ✅ fixed & verified (committed)

## F36: `hasattr()`/`dir()` on a provider instance report unset fields as present
- **Repo:** protobuf (`//:protobuf_lite`) and grpc (`//:gpr`) — both via the rules_kotlin
  `kotlin_repositories` module extension.
- **Symptom:** `Object of type 'NoneType' has no attribute 'format'` at rules_kotlin
  `repositories/versions.bzl:18`:
  `if hasattr(version, "strip_prefix_template"): rule_arguments["strip_prefix"] =
  version.strip_prefix_template.format(...)`. The `version` is a `provider()` instance built
  *without* `strip_prefix_template` (e.g. `versions.PINTEREST_KTLINT`), so Bazel's `hasattr`
  guard returns False and skips the line; bz's returns True and then `.format()` hits None.
- **Minimal repro:** `P = provider(fields = {"a": "..", "b": ".."})`; `p = P(a = "set")`;
  `hasattr(p, "b")` → bz prints `True` (Bazel: `False`); `dir(p)` → `["a","b"]` (Bazel: `["a"]`).
- **Root cause:** `UserProviderGen` (`bz_build_api/.../provider/user.rs`) stores
  `attributes: Box<[V]>` with exactly one slot per *declared* schema field (asserts
  `fields.len() == attributes.len()`). At construction (`user_provider_creator`), a field that
  isn't passed falls back to `UserProviderField.default`, which for a `provider(fields={name:
  doc})` declaration is the implicit `Some(None)` (`UserProviderField::default()` in
  `callable.rs`). So every declared field is always materialized (unset ⇒ stored as `None`),
  and `dir_attr`/`get_attr_hashed` (and thus `hasattr`) always report it present. Bazel keeps
  unset provider fields *absent* (access raises, `hasattr` is False).
- **Impact:** blocks **protobuf, grpc, AND cel-cpp** (and, transitively, ~everything that
  depends on protobuf — a huge slice of the C++/proto ecosystem), all via the rules_kotlin
  `kotlin_repositories` module extension that protobuf pulls into the graph.
- **ATTEMPTED & REVERTED (2026-06-17) — important learning:** I tried the cheap approximation
  "tag `provider(fields={...})`/list fields as `bazel_optional` and, in `get_attr`/`dir_attr`
  only, treat a `bazel_optional` field whose stored value is `None` as absent." This fixed the
  repro cleanly (`hasattr(b)→False`, `dir→["a"]`, `getattr(...,default)` works) and let
  protobuf/grpc/cel-cpp past the rules_kotlin guard — **but it regressed the whole cc base**:
  `rules_cc++cc_configure_extension+local_config_cc//:cc-compiler-k8` failed with
  `struct has no attribute '_module_map'`. rules_cc's `get_cc_toolchain_provider` accesses an
  optional provider field that is **`None`** and relies on bz's current lenient
  "unset/None optional field ⇒ returns `None`" behavior. The approximation can't distinguish a
  field explicitly set to `None` (rules_cc) from a field never set (rules_kotlin), so it broke
  rules_cc. Reverted; base re-verified (abseil/tcmalloc/rust green). (The `_module_map` and
  `cc_test size` errors seen "past F36" were artifacts of this broken state, not real findings —
  bz's cc_test does accept `size`, confirmed via abseil.)
- **Correct fix shape (deferred — needs supervision):** a true 3rd optionality state
  (Required | Optional-absent | Defaulted) with an **unset sentinel** stored in the `attributes`
  slot at construction for Optional-absent fields that weren't passed — so explicit `field=None`
  stays *present* while truly-unset is *absent*. Filter the sentinel in `get_attr_hashed`
  (⇒ hasattr False), `dir_attr`, and `iter_items` (feeds `Display`/`serialize`/`ProviderLike::
  items`); keep `equals`/`write_hash` positional over the raw array so presence is part of
  identity. Must verify rules_cc cc_toolchain still resolves (it may rely on truly-unset⇒None;
  if so, that lenient access is itself non-Bazel and may need its own handling). Full
  provider-subsystem test coverage required — not to be attempted unsupervised.
- **Status:** documented / open (deferred — blocks protobuf, grpc, cel-cpp at rules_kotlin ext)

## F35: `rule()` rejects deprecated no-op `incompatible_use_toolchain_transition` — ✅ FIXED
- **Repo:** tcmalloc (`//tcmalloc:tcmalloc`), after F33.
- **Symptom:** `Found 'incompatible_use_toolchain_transition' extra named parameter(s) for
  call to rule`.
- **Root cause:** modern Bazel removed the `incompatible_use_toolchain_transition` `rule()`
  flag (it became a no-op), but some rule sets still pass it; bz erred on the unknown kwarg.
- **Fix:** added `incompatible_use_toolchain_transition: bool` to `rule()`
  (`bz_interpreter_for_build/src/rule.rs`), collected into `_unused` like other deprecated
  flags — accepted and ignored.
- **Verification:** with F33, tcmalloc `//tcmalloc:tcmalloc` builds (232 actions).
- **Status:** ✅ fixed & verified (committed)

## F33: `local_config_platform` not a known cell alias (bzlmod well-known repo) — ✅ FIXED
- **Repo:** grpc (`//:gpr`), tcmalloc (`//tcmalloc:tcmalloc`), surfaced after F9 fixed.
- **Symptom:** `unknown cell alias: 'local_config_platform'. In cell
  'aspect_bazel_lib++toolchains+bsd_tar_toolchains', known aliases are: ...host_platform,
  platforms...` while loading `@local_config_platform//:constraints.bzl` from a generated
  aspect_bazel_lib toolchains BUILD.
- **Root cause:** `local_config_platform` is one of Bazel's *built-in always-visible*
  repos (like `bazel_tools`) — present in every repo's mapping under both WORKSPACE and
  bzlmod. bz only generates/injects `host_platform` (from the `platforms` module's
  extension), not `local_config_platform`, so cells that reference `@local_config_platform`
  (very common via aspect_bazel_lib toolchains) fail to resolve the alias.
- **Fix (turned out localized, not architectural):** in `bzlmod_cell_aliases_for_cell`
  (`bz_core/src/cells/external.rs`) — the single read path every cell-alias resolver uses —
  wherever a cell already exposes `host_platform`, also expose `local_config_platform` as an
  alias to the *same destination cell* (its `constraints.bzl`/`HOST_CONSTRAINTS` is what the
  observed repos load). Purely additive: only adds a resolvable name where `host_platform` is
  already visible; never overrides existing aliases. A dedicated `:host` `platform()` target
  is still not emitted — revisit if a repo references `@local_config_platform//:host`.
- **Verification:** grpc `//:gpr` and tcmalloc both advance past F33. Full regression sweep
  clean: abseil(163)/re2(196)/googletest/rust/cpp-tutorial/go-tutorial/custom-rules build,
  abseil variant_test Pass 1/Fail 0.
- **Status:** ✅ fixed & verified (committed)

## F9: `config_feature_flag` native rule not defined (Android) — ✅ FIXED
- **Repo:** protobuf (`//:protoc`), grpc (`//:gpr`).
- **Symptom:** `Variable 'config_feature_flag' not found` while evaluating
  `rules_android++android_sdk_repository_extension+androidsdk//:BUILD.bazel`.
- **Root cause:** Module graphs that pull in rules_android materialize a stub `androidsdk`
  repo whose BUILD.bazel declares `config_feature_flag(...)` (an Android native rule bz did
  not define). bz evaluates this repo's BUILD even for pure C++ targets during toolchain
  resolution, so the missing symbol blocked the *load*, not just Android targets.
- **Fix:** Implemented `config_feature_flag` as a bazel-compat native rule
  (name registered in `bazel/native.rs`; decl in `decls/core_rules.bzl`; impl
  `config_feature_flag_impl` in `configurations/rules.bzl` returning `DefaultInfo`, validates
  default∈allowed; wired through `native_rules.bzl` + `bazel/prelude.bzl`). bz does not model
  feature-flag propagation, so the flag analyzes to a plain target and any `config_setting`
  referencing it via `flag_values` stays inert unless set — same philosophy as `define_values`
  (F2). This unblocks loading BUILD files that merely *declare* feature flags.
- **Verification:** grpc `//:gpr` and protobuf both get past F9 (grpc → F33 below;
  protobuf → rules_kotlin module-extension issue). Regression sweep clean: abseil 163
  actions, re2 196, googletest 19 — config_setting unaffected.
- **Status:** ✅ fixed & verified (committed)

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
- **Confirmed real-world occurrence (2026-06-17):** `tcmalloc//tcmalloc:
  huge_page_aware_allocator_fuzz` (a cc_test) depends on **snappy**, whose
  `@@snappy+//:config` target uses a bare native `cc_library` →
  `fail: Unimplemented rule type 'cc_library' for target '@@snappy+//:config'`. This is a
  buildable-on-Linux real-world repro (unlike the re2 emscripten case). tcmalloc's core
  library and most targets load cc rules from rules_cc and build fine; only deps that use
  bare native cc rules (snappy here) hit F5.
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
