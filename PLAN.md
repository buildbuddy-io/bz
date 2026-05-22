# Remaining Bazel Compatibility Hard Cutover Plan

This plan covers the remaining live or partially live items from
`~/Desktop/HACKS.md` after the merged compatibility fixes. The goal is a hard
cutover to Bazel-aligned behavior, not incremental best-effort compatibility.

Hard cutover means:

- Delete or bypass the shortcut implementation once the replacement lands.
- Preserve Bazel label, repository, toolchain, platform, and repository-rule
  semantics as the source of truth.
- Fail closed when a Bazel surface is still unsupported. Do not accept an attr,
  directive, flag, or API and silently ignore it.
- Add focused conformance tests that compare Buck2 behavior with Bazel behavior
  for the same small workspace when practical.
- Keep the final validation gate green:
  - `bazel build //:buck2` in `~/Code/buck2`
  - `buck2 build :buck2` or the locally produced Buck2 binary build in
    `~/Code/buck2`
  - `buck2 build server` and `buck2 build enterprise/server` in
    `~/Code/buildbuddy`
  - `buck2 build :bazelisk` in `~/Code/bazelisk`
  - `buck2 build src:bazel` in `~/Code/bazel`

## Workstream 1: Repository Identity And Bzlmod State

### #1 Bzlmod cell-name encoding is non-injective

Buck pointers:

- `app/buck2_core/src/cells/external.rs`
- `app/buck2_common/src/legacy_configs/cells.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/cmdline/RepositoryName.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/cmdline/RepositoryMapping.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/ModuleKey.java`

Problem to cut over:

`bzlmod_cell_name` sanitizes canonical repository names by replacing every
non-identifier byte with `_`. This is not injective, so distinct Bazel
canonical repository names can collide in Buck cell space.

Hard cutover:

1. Introduce a typed `BzlmodCellName` or equivalent wrapper that stores both:
   - The exact Bazel canonical repository name.
   - The reversible Buck cell spelling used in internal cell maps.
2. Replace lossy underscore sanitization with a reversible ASCII encoding.
   Acceptable examples are percent/hex escaping per byte or a fixed prefix plus
   lowercase hex/base32 of the canonical name. The encoded form must be stable
   across daemon restarts and must never collide.
3. Move all call sites from stringly cell-name construction to the typed helper.
   The canonical Bazel repository name stays the primary key. The Buck cell
   spelling becomes only the storage/display adapter.
4. Add a startup/resolution diagnostic that rejects duplicate encoded cell names
   before registering external cells. The diagnostic should print both canonical
   repository names and their apparent names.
5. Remove any fallback that derives a canonical repository name from the encoded
   cell name without decoding through the typed helper.

Tests:

- Unit test the encoder with `foo-bar+`, `foo.bar+`, `foo_bar+`, module
  extension repos, and apparent repo aliases.
- Integration test a MODULE graph containing colliding names under the old
  sanitizer. Both repositories must be addressable and must map to distinct
  cells.
- Warm daemon test: resolve, change MODULE to add a colliding repo, resolve
  again, and verify aliases/origins remain distinct.

Acceptance gate:

- No code path compares bzlmod cell names as sanitized strings without a
  canonical repository name in hand.

### #2 Process-global bzlmod alias/origin maps retain stale state

Buck pointers:

- `app/buck2_core/src/cells/external.rs`
- `app/buck2_common/src/legacy_configs/cells.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/RepositoryMappingFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/RepositoryMappingValue.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/BazelModuleResolutionValue.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/SingleExtensionUsagesFunction.java`

Problem to cut over:

Bzlmod resolution is DICE-computed, but aliases, canonical names, extension
usages, and external origins still live in process-global maps. A warm daemon
can retain aliases or origins from an older resolution.

Hard cutover:

1. Define a resolution-scoped value, for example `BzlmodResolutionState`, that
   contains:
   - Canonical repo name -> Buck cell name.
   - Apparent repo mapping for each context repo.
   - Cell origin metadata.
   - Module extension usage metadata.
   - Generated repository metadata.
2. Return this state from the bzlmod DICE computation and thread it through cell
   resolver construction. Do not mutate process-global maps from resolution.
3. Delete global alias/origin/module-extension maps. If a temporary bridge is
   unavoidable, make it an immutable snapshot keyed by a resolution epoch and
   replace the entire snapshot atomically.
4. Keep dynamic sibling aliases in a separate explicit overlay with lifetime and
   invalidation rules. Do not merge them into the base bzlmod snapshot.
5. Make all lookup APIs require the current resolution state or a cell resolver
   that owns that state.

Tests:

- Warm daemon test where MODULE removes a repo. The removed repo must stop
  resolving without restarting buckd.
- Warm daemon test where an apparent repo name is remapped to a different
  canonical repo. The old mapping must not survive.
- Test module-extension usage pruning when an extension or tag is deleted.

Acceptance gate:

- `rg` should not find mutable process-global bzlmod alias/origin maps in
  production code.

### #28 Default registry and registry invalidation are too coarse

Buck pointers:

- `app/buck2_common/src/legacy_configs/cells.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/BazelRepositoryModule.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/ModuleFileFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/RegistryFactoryImpl.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/RepositoryOptions.java`

Problem to cut over:

The default registry is hard-coded and registry DICE equality is deliberately
coarse. Registry inputs are not modeled with enough precision.

Hard cutover:

1. Parse the active registry list from Bazel-compatible sources:
   - `.bazelrc` command/config expansion.
   - Command-line flags that affect module registries.
   - Environment inputs Bazel treats as relevant.
2. Replace the hard-coded default registry with a computed `RegistrySet` value.
3. Key registry DICE values by the full registry set plus relevant registry
   file digests, selected yanked-version policy, and lockfile state.
4. Remove always-false equality for registry keys. Equality must reflect the
   modeled inputs.
5. Add explicit invalidation for user-requested cache busting instead of using
   permanent recomputation as a shortcut.

Tests:

- Changing `.bazelrc` registry order invalidates module resolution.
- Reusing the same registry inputs does not recompute.
- Root module with custom registry behaves the same under Bazel and Buck2 for a
  small fixture.

Acceptance gate:

- Registry recomputation is input-driven and no longer forced by unconditional
  false equality.

### #30 Configure-repo detection uses name substrings

Buck pointers:

- `app/buck2_common/src/legacy_configs/cells.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/RepoMetadata.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/RepoSpec.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/RepoDefinition.java`

Problem to cut over:

Repository behavior is inferred from generator type or substrings like
`configure`, `config`, or `toolchain` in the canonical repo name.

Hard cutover:

1. Add explicit repository metadata to the bzlmod repository definition model.
   It should carry whether a repo is local/configure/non-reproducible and why.
2. Plumb this metadata from:
   - Native generated repositories.
   - Module extensions.
   - Repository rules returning metadata.
3. Replace all name-substring classification with the metadata field.
4. Reject missing metadata for built-in generated repos that must be classified.
5. Add debug output that explains classification from metadata, not name.

Tests:

- Repo named `not_a_config_repo` is not classified as configure unless metadata
  says so.
- Repo named without `config` but generated by a configure rule is classified
  correctly.
- Metadata changes invalidate downstream generated repository state.

Acceptance gate:

- No production code classifies configure/local/toolchain repositories by
  substring matching canonical repository names.

### #31 `multiple_version_override` and `git_override` are not implemented

Buck pointers:

- `app/buck2_common/src/legacy_configs/cells.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/ModuleFileGlobals.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/ModuleFileFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/BazelModuleResolutionFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/MultipleVersionOverride.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/GitOverride.java`

Problem to cut over:

Buck explicitly rejects these MODULE directives. That is safer than ignoring
them, but it is still a compatibility gap.

Hard cutover:

1. Parse/evaluate both directives through the MODULE evaluator cutover in #11.
2. Add `MultipleVersionOverride` and `GitOverride` variants to Buck's module
   override model, matching Bazel fields and validation.
3. Implement multiple-version selection in module resolution:
   - Preserve allowed version sets.
   - Apply compatibility-level constraints.
   - Report Bazel-like errors for missing/invalid versions.
4. Implement git override fetching as a repository source:
   - URL, commit, patches, patch args, strip prefix, remote patches, integrity.
   - Cache key includes commit and fetch parameters.
5. Add lockfile/registry invalidation inputs for both override kinds.

Tests:

- A module graph with two allowed versions resolves both versions.
- Invalid multiple-version override errors match Bazel shape.
- A fixture git override fetches from a local bare repository and is cached.
- Patch and strip-prefix behavior matches Bazel.

Acceptance gate:

- These directives are no longer rejected in normal supported configurations.

## Workstream 2: Toolchains, Execution Platforms, And Exec Groups

### #3 Bazel toolchains do not drive execution platform selection

Buck pointers:

- `app/buck2_configured/src/nodes.rs`
- `app/buck2_configured/src/execution.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/toolchains/SingleToolchainResolutionFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/toolchains/ToolchainResolutionFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/toolchains/ToolchainContextKey.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/toolchains/ToolchainContextUtil.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/toolchains/RegisteredExecutionPlatformsFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/toolchains/RegisteredToolchainsFunction.java`

Problem to cut over:

Buck chooses an execution platform before resolving Bazel rule toolchains. Bazel
selects an execution platform and matching toolchains as one resolution result.

Hard cutover:

1. Introduce a Bazel toolchain context value that contains:
   - Requested toolchain types.
   - Mandatory/optional bits.
   - Target platform constraints.
   - Execution platform constraints.
   - Candidate registered execution platforms.
   - Candidate registered toolchains.
2. Move Bazel registered-toolchain resolution into execution platform selection.
   The resolver returns `(selected_exec_platform, resolved_toolchains)`.
3. Resolve toolchains against every candidate execution platform before choosing
   the platform. Do not resolve toolchains only after a Buck exec platform has
   already been picked.
4. Make missing mandatory toolchains fail during analysis with a Bazel-like
   diagnostic that includes the requested type and candidate platforms.
5. Thread the selected toolchain context into configured nodes and action
   analysis.
6. Delete the old post-selection `resolve_bazel_toolchain_deps` path once the
   unified resolver is in place.

Tests:

- Fixture with two execution platforms where only one has the requested
  toolchain. Buck must select the matching platform.
- Fixture where a rule has no matching mandatory toolchain. Buck must fail
  before action execution.
- Fixture where optional toolchain is absent. Buck must continue and expose
  absent optional state consistently.

Acceptance gate:

- There is one Bazel toolchain/platform selection path. Platform selection no
  longer happens before Bazel toolchain resolution.

### #5 Toolchain type matching drops repository identity

Buck pointers:

- `app/buck2_configured/src/nodes.rs`
- `app/buck2_build_api/src/interpreter/rule_defs/context.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/cmdline/Label.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/cmdline/RepositoryMapping.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/toolchains/ToolchainContextKey.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/analysis/platform/ToolchainInfo.java`

Problem to cut over:

Toolchain type matching compares the label suffix after `//`, so different
repositories can collide.

Hard cutover:

1. Replace string keys for toolchain type identity with normalized
   `ProvidersLabel` or equivalent canonical label identity.
2. Normalize requested toolchain types through repository mapping before
   inserting them into the toolchain context.
3. Normalize provided toolchain type labels from toolchain targets the same way.
4. Delete suffix fallback matching.
5. Preserve apparent label display only for diagnostics.

Tests:

- `@repo_a//pkg:tc_type` and `@repo_b//pkg:tc_type` must not match each other.
- Repository mapping alias to the same canonical repo must match.
- Diagnostics should display useful apparent labels while comparing canonical
  labels internally.

Acceptance gate:

- No code compares Bazel toolchain type labels by string suffix.

### #6 `rule(exec_compatible_with = ...)` and `exec_groups` are accepted but ignored

Buck pointers:

- `app/buck2_interpreter_for_build/src/rule.rs`
- `app/buck2_build_api/src/interpreter/rule_defs/context.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/starlarkbuildapi/StarlarkRuleFunctionsApi.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/packages/DeclaredExecGroup.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/analysis/ExecGroupCollection.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkExecGroupCollection.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/toolchains/ToolchainContextUtil.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/starlarkbuildapi/StarlarkActionFactoryApi.java`

Problem to cut over:

Rule-level execution constraints and exec groups affect Bazel platform and
toolchain resolution, but Buck currently accepts and drops them.

Hard cutover:

1. Parse `exec_compatible_with` into the rule spec as canonical constraint
   labels.
2. Parse `exec_groups` into a declared exec-group model with:
   - Name.
   - Toolchain types.
   - Execution constraints.
   - Exec properties.
   - Validation metadata.
3. Include rule-level `exec_compatible_with` in default exec group platform
   selection.
4. Resolve each declared exec group through the unified toolchain/platform
   resolver from #3.
5. Populate `ctx.exec_groups` with per-group toolchain and execution platform
   context.
6. Implement `ctx.actions.run`, `run_shell`, and related APIs'
   `exec_group = ...` validation:
   - Unknown exec group fails.
   - `toolchain` plus `exec_group` follows Bazel precedence and validation.
   - Action gets the group's selected platform/properties.
7. If any Starlark surface cannot be implemented in the same patch, reject
   non-default values instead of accepting them.

Tests:

- Rule with `exec_compatible_with` selects only compatible execution platforms.
- Rule with named exec group uses that group's toolchain, not the default group.
- Unknown `exec_group` in action creation fails during analysis.
- `ctx.exec_groups` exposes the same group names Bazel exposes for fixtures.

Acceptance gate:

- `exec_compatible_with` and `exec_groups` are either implemented or rejected.
  There is no `_unused` drop path for these attrs.

## Workstream 3: Parser And Configuration Cutovers

### #11 MODULE.bazel parsing is a custom partial parser

Buck pointers:

- `app/buck2_common/src/legacy_configs/cells.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/ModuleFileFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/CompiledModuleFile.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/ModuleFileGlobals.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/packages/BazelStarlarkEnvironment.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/packages/DotBazelFileSyntaxChecker.java`

Problem to cut over:

MODULE files are parsed with line scanning and helper string extraction. Legal
Starlark shapes can be missed or misinterpreted.

Hard cutover:

1. Delete the line-scanning parser for MODULE.bazel and included module files.
2. Add a restricted MODULE evaluator using the existing Starlark parser/runtime
   in Buck or a direct port of Bazel's module-file execution model.
3. Define predeclared MODULE globals matching Bazel:
   - `module`
   - `bazel_dep`
   - `use_extension`
   - `use_repo`
   - `use_repo_rule`
   - `register_toolchains`
   - `register_execution_platforms`
   - overrides
   - `include`
   - `flag_alias`
4. Store structured directive values from evaluation, not source text matches.
5. Execute included module files with Bazel's include ordering and syntax
   restrictions.
6. Preserve Bazel-like errors for illegal statements, repeated directives, bad
   attrs, and unsupported constructs.

Tests:

- Multiline calls, constants, comments, nested dict/list values, keyword order,
  and included `.MODULE.bazel` files.
- Invalid syntax and invalid directive usage must fail with useful source spans.
- Compare a representative MODULE fixture against Bazel's module graph.

Acceptance gate:

- `collect_bzl_calls`, `bzl_string_arg`, and top-level assignment scanning no
  longer drive MODULE semantics.

### #12 `.bazelrc` parsing and option support are ad hoc

Buck pointers:

- `app/buck2_common/src/legacy_configs/cells.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/runtime/BlazeOptionHandler.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/runtime/ConfigExpander.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/runtime/RcChunkOfArgs.java`
- `~/Code/bazel/src/main/java/com/google/devtools/common/options/OptionsParser.java`
- `~/Code/bazel/src/main/java/com/google/devtools/common/options/OptionsParserImpl.java`

Problem to cut over:

Buck supports only a narrow, hand-written subset of Bazel rc semantics and can
silently ignore build-affecting options.

Hard cutover:

1. Replace the ad hoc tokenizer/collector with a Bazel-compatible rc parser:
   - Import order.
   - `try-import` behavior.
   - Command sections.
   - OS-specific config sections.
   - `--config` expansion ordering and recursion errors.
   - Startup vs command option separation where relevant.
2. Define a typed table of Buck-supported Bazel options with converters.
3. For unsupported build-affecting options, fail with an explicit diagnostic.
   Do not silently ignore them.
4. Feed parsed options into bzlmod registry, module resolution, platform, and
   build-setting keys as structured values.
5. Add a debug command or trace mode that prints the effective Bazel options
   after expansion for troubleshooting.

Tests:

- Nested import and try-import fixtures.
- `build:foo --config=bar` expansion order.
- Unknown build-affecting option rejects.
- Non-build-affecting option either maps to no-op with explicit documentation or
  rejects consistently.

Acceptance gate:

- No bzlmod/build-affecting rc option is dropped without an explicit modeled
  decision.

### #33 Bazel features globals parsing and version comparison are fragile

Buck pointers:

- `app/buck2_external_cells/src/bzlmod.rs`

Bazel/rules source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/Version.java`
- The active `bazel_features` repository consumed by bzlmod.

Problem to cut over:

`bazel_features` globals are parsed by source scanning and versions are compared
by extracting numeric components.

Hard cutover:

1. Stop scanning `GLOBALS = { ... }` by lines.
2. Evaluate the relevant `bazel_features` Starlark file or consume a generated
   data artifact from the repository.
3. Represent feature availability as structured data keyed by feature namespace
   and Bazel version.
4. Use Bazel-compatible version parsing/comparison, including prerelease,
   release candidate, dev, suffix, and empty version semantics where applicable.
5. Add a single parser/comparator shared by bzlmod, registry, and feature logic.

Tests:

- Feature globals with reordered dicts, comments, multiline values, and aliases.
- Version comparison for `7.0.0rc1`, `7.0.0`, `7.0.1`, dev/suffix forms, and
  empty versions.
- Fixture where a feature gate changes generated repository output.

Acceptance gate:

- Feature selection does not depend on source formatting.

## Workstream 4: Generated Repository Cutovers

### #13 Generated `local_config_xcode` hard-codes most Apple state to `0.0`

Buck pointers:

- `app/buck2_external_cells/src/bzlmod.rs`

Bazel source to align with:

- `~/Code/bazel/tools/osx/xcode_configure.bzl`
- `~/Code/bazel/tools/osx/xcode_version_flag.bzl`
- `~/Code/bazel/src/main/starlark/builtins_bzl/common/objc/apple_common.bzl`
- `~/Code/bazel/src/main/starlark/builtins_bzl/common/xcode/providers.bzl`

Problem to cut over:

Buck generates a stub `local_config_xcode` with most SDK/Xcode values set to
`0.0`.

Hard cutover:

1. Delete the Rust string-template implementation for `local_config_xcode`.
2. Generate `local_config_xcode` by evaluating or faithfully porting Bazel's
   `xcode_configure.bzl`.
3. Use `xcode-locator` and `xcrun xcodebuild -version -sdk` behavior aligned
   with Bazel.
4. Populate:
   - Installed Xcode versions.
   - Default Xcode.
   - iOS, tvOS, watchOS, visionOS, and macOS SDK versions.
   - Remote/local Xcode config fields.
5. Key the generated repo by all relevant environment and command outputs:
   `DEVELOPER_DIR`, `xcode-select`, `xcrun`, Xcode install list, and rc/options
   that affect Apple config.
6. If Xcode discovery is unavailable, emit Bazel-like stub/error behavior. Do
   not fabricate `0.0` values for successful configs.

Tests:

- On Darwin, compare generated `local_config_xcode` labels and provider values
  to Bazel for the host machine.
- On non-Darwin, verify Bazel-compatible stub behavior.
- Analysis fixture consuming `apple_common.XcodeVersionConfig` sees real SDK
  versions or a clear unavailable error.

Acceptance gate:

- No generated successful Apple config contains synthetic `0.0` placeholder
  versions.

### #14 Generated `local_config_cc` is a stub toolchain

Buck pointers:

- `app/buck2_external_cells/src/bzlmod.rs`

Bazel/rules source to align with:

- `~/Code/bazel/tools/cpp/cc_configure.bzl`
- `~/Code/bazel/src/main/res/winsdk_configure.bzl`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/rules/cpp/CcToolchainConfigInfo.java`
- Active `rules_cc` `cc/private/toolchain/*` files fetched by bzlmod.

Problem to cut over:

Buck generates a partial C/C++ toolchain with empty include dirs, empty
features/action configs, no-op validation, and stub tools.

Hard cutover:

1. Delete the hand-written Rust local C/C++ template as the default path.
2. Use the same `@bazel_tools//tools/cpp:cc_configure.bzl` indirection Bazel
   uses, backed by `rules_cc`.
3. Evaluate or port the actual host C/C++ autoconfiguration logic for:
   - Tool paths.
   - Built-in include directories.
   - Sysroot and SDK paths.
   - Feature and action config definitions.
   - Static library validation.
   - MSVC/Winsdk configuration on Windows.
4. Fail explicitly for host/platform combinations that Buck cannot configure.
   Do not generate `/bin/false` tools as if the toolchain is valid.
5. Key generated repo contents by compiler path, compiler version output, SDK
   paths, relevant env vars, and rc/options.

Tests:

- Small `cc_library` and `cc_binary` fixture builds on the host.
- Query provider fixture verifies `CcToolchainConfigInfo` has non-empty built-in
  include dirs and expected action configs.
- Unsupported host configuration fails during repository generation with a clear
  error.

Acceptance gate:

- The generated local C/C++ config is either a real Bazel-compatible toolchain
  or an explicit failure.

### #15 Generated rules_python hub is intentionally empty

Buck pointers:

- `app/buck2_external_cells/src/bzlmod.rs`

Bazel/rules source to align with:

- Active `rules_python` module extension and hub-generation implementation.
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/SkyframeExecutor.java`
  for Bazel's Python flag-alias integration behavior.

Problem to cut over:

Buck writes empty interpreter labels, default versions, and version lists for
the rules_python hub.

Hard cutover:

1. Delete the empty hub generator.
2. Evaluate the rules_python toolchain/hub module extension, or port the exact
   generated repository structure from the active rules_python version.
3. Populate interpreter labels, default version, available versions, toolchains,
   and flags from structured extension data.
4. Key the hub by MODULE usage, selected Python versions, host platform, and
   relevant rc/options.
5. If rules_python hub generation cannot be supported for a graph, fail at repo
   generation with the unsupported field and requesting module.

Tests:

- Fixture using `python.toolchain()` and consuming the generated hub.
- Fixture using Bazel Python flag aliases.
- Empty/unconfigured Python hub must fail clearly instead of analyzing to empty
  labels.

Acceptance gate:

- Generated rules_python hub repos never contain placeholder empty labels for
  successful generation.

### #16 Generated shell config hard-codes shell paths

Buck pointers:

- `app/buck2_external_cells/src/bzlmod.rs`

Bazel/rules source to align with:

- Active `rules_shell` local shell toolchain setup.
- Bazel repository rule behavior in
  `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkBaseExternalContext.java`.

Problem to cut over:

Shell toolchain paths are hard-coded by OS with limited `BAZEL_SH`/PATH lookup.

Hard cutover:

1. Replace the hard-coded shell repo template with rules_shell-compatible
   repository generation.
2. Model all environment inputs used to select shells, including `BAZEL_SH`,
   PATH lookup, host OS, and any rc/config flags.
3. Generate platform-specific toolchain targets matching Bazel/rules_shell.
4. Fail clearly when a requested shell cannot be found or does not satisfy
   required semantics.

Tests:

- Host shell toolchain fixture selects expected shell.
- `BAZEL_SH` override changes the generated repo and invalidates cache.
- Missing shell errors during repository generation.

Acceptance gate:

- No generated successful shell toolchain depends on unmodeled hard-coded host
  paths.

### #52 `java_plugins_flag_alias` silently drops configured Java plugins

Buck pointers:

- `bazel_tools/tools/jdk/java_plugins_flag_alias.bzl`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/rules/java/JavaPluginsFlagAliasRule.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/rules/java/JavaPluginInfo.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/rules/java/JavaOptions.java`

Problem to cut over:

Buck's Starlark shim returns an empty `JavaPluginInfo` instead of reading the
configured Java plugins.

Hard cutover:

1. Replace the shim with a native compatibility rule or an equivalent Starlark
   rule wired to Buck's Java configuration.
2. Add a hidden/configuration-backed `:java_plugins` label-list attr matching
   Bazel's rule definition.
3. Merge configured plugin providers using Bazel's provider semantics:
   `JavaPluginInfo` and the rules_java provider where applicable.
4. If Buck cannot read Java plugin configuration, reject configured
   `--plugins` instead of returning empty providers.
5. Ensure Java compilation consumes this provider consistently with other plugin
   inputs.

Tests:

- `--plugins=//:plugin` fixture exposes plugin provider through
  `java_plugins_flag_alias`.
- Java compile action includes processor path/classes.
- No-plugin config returns empty provider.

Acceptance gate:

- Configured Java plugins are either propagated or explicitly rejected. They are
  not silently dropped.

### #54 `bazel_tools` BUILD files are context-sensitive between main repo and bundled `@bazel_tools`

Buck pointers:

- `bazel_tools/tools/BUILD.bazel`
- `bazel_tools/tools/zip/BUILD.bazel`
- `app/buck2_external_cells_bundled/build.rs`

Bazel source to align with:

- `~/Code/bazel/tools/**`
- Buck's bundled `@bazel_tools` generator and manifest.

Problem to cut over:

Some checked-in BUILD metadata only works after being bundled as `@bazel_tools`,
while the same files are also visible in the main repo context.

Hard cutover:

1. Split ownership of metadata:
   - Main-repo packaging/build metadata.
   - Embedded `@bazel_tools` metadata.
2. Generate bundled `@bazel_tools` BUILD files from an explicit manifest instead
   of relying on context-sensitive checked-in files.
3. Use `BUILD.tools` or a dedicated embedded metadata source for labels intended
   to evaluate under `@bazel_tools`.
4. Add validation that embedded-only absolute labels do not appear in main-repo
   `BUILD.bazel` files.
5. Add validation that every bundled file referenced by generated metadata
   exists in the bundle.

Tests:

- `bazel build //bazel_tools/tools/...` from the main repo does not depend on
  embedded-only labels.
- Bundled `@bazel_tools//tools/...` resolves all labels under a Buck/Bazel
  compatibility fixture.
- Manifest validation fails on missing files or wrong-context labels.

Acceptance gate:

- A BUILD file is no longer expected to mean different things depending on
  whether it is evaluated in the main repo or embedded cell.

### #55 Bazel snippets are duplicated instead of generated or single-sourced

Buck pointers:

- `prelude/tools/build_defs/repo/utils.bzl`
- `bazel_tools/tools/build_defs/repo/utils.bzl`
- `prelude/tools/build_defs/cc/action_names.bzl`
- `bazel_tools/tools/build_defs/cc/action_names.bzl`

Bazel source to align with:

- `~/Code/bazel/tools/build_defs/repo/utils.bzl`
- `~/Code/bazel/tools/build_defs/cc/action_names.bzl`

Problem to cut over:

Bazel snippets exist as duplicate copies in multiple Buck paths, with no
single source of truth or upstream sync check.

Hard cutover:

1. Choose one source of truth:
   - Prefer a checked-in vendored Bazel source manifest plus generated copies.
   - If label semantics allow it, re-export from one local path to the other.
2. Add a generator script or build action that copies/rewrites snippets from the
   source manifest.
3. Record upstream Bazel path and expected hash in the manifest.
4. Add a CI/test check that duplicated generated outputs match the manifest.
5. Remove hand-maintained duplicate files from review ownership.

Tests:

- Manifest check fails if either copy drifts.
- Generator updates both locations deterministically.
- Labels that load the snippets from prelude and `@bazel_tools` both still work.

Acceptance gate:

- There are no hand-maintained byte-for-byte Bazel snippet duplicates.

## Workstream 5: Repository Rule Filesystem And Execution Semantics

### #17 Dynamic repository labels are detected by source text scanning

Buck pointers:

- `app/buck2_interpreter_for_build/src/bazel_repository.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/AttributeValuesAdapter.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/RunnableExtension.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/RegularRunnableExtension.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/cmdline/Label.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/cmdline/RepositoryMapping.java`

Problem to cut over:

Buck scans repository rule source text for narrow `Label("...")` patterns to
infer dynamic labels.

Hard cutover:

1. Delete source-text dynamic label detection.
2. Track label creation and repository mapping during Starlark evaluation.
3. When a label cannot be resolved at definition time because it is dynamic,
   store a structured unresolved-label dependency with evaluation context.
4. Resolve dynamic labels through repository mappings at the point Bazel would.
5. Record dynamic label dependencies as DICE inputs.

Tests:

- Dynamic label via string concatenation.
- Dynamic label via variable.
- Dynamic label via `.format`.
- Static label still resolves immediately.

Acceptance gate:

- Dynamic label behavior is based on evaluated label objects/dependencies, not
  source text patterns.

### #18 Repository rule invocation attrs are serialized back into Starlark text

Buck pointers:

- `app/buck2_interpreter_for_build/src/bazel_repository.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/AttributeValuesAdapter.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/RepoSpec.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/bzlmod/RepoDefinition.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/packages/Attribute.java`

Problem to cut over:

Repository rule attrs are replayed by converting Starlark values back to text.
This loses canonical ordering and label identity.

Hard cutover:

1. Store repository rule invocations as structured coerced attr values.
2. Preserve label attrs as canonical labels plus display mapping, never as
   re-evaluable source strings.
3. Canonicalize dict/list attrs using deterministic structured serialization.
4. Reject unsupported attr value types during invocation capture.
5. Use the structured representation for cache keys, replay, diagnostics, and
   repository rule execution.

Tests:

- Dict attr order does not affect cache key.
- Label attr in a mapped repository replays to the same canonical label.
- Unsupported attr value fails at capture with source location.

Acceptance gate:

- Repository rules are not replayed from `repr()` or generated Starlark text.

### #20 `repository_ctx` generated output manifest only supports UTF-8 regular files

Buck pointers:

- `app/buck2_interpreter_for_build/src/bazel_repository.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/actions/FileArtifactValue.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/TreeArtifactValue.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/vfs/FileStatus.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/vfs/Path.java`

Problem to cut over:

Buck's generated repository output manifest tracks valid UTF-8 regular files
only. Binary files and symlinks can be omitted.

Hard cutover:

1. Replace the text-file manifest with a filesystem tree manifest.
2. Represent each entry as one of:
   - Regular file with bytes digest, executable bit, and size.
   - Symlink with raw link target bytes/string.
   - Directory.
3. Materialize generated outputs from this tree, not from UTF-8 file contents.
4. Include binary files, symlinks, modes, and deletions in repository cache keys.
5. Make refresh/update logic compare manifests structurally.

Tests:

- `repository_ctx.execute` writes a binary file.
- `repository_ctx.symlink` writes a symlink and later changes the target.
- Generated executable mode is preserved.
- Deleting a generated output is reflected in the manifest.

Acceptance gate:

- Generated repository output state is no longer limited to UTF-8 regular files.

### #21 Repository path resolution guesses project roots from `PWD`, `cwd`, and `buck-out`

Buck pointers:

- `app/buck2_interpreter_for_build/src/bazel_repository.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkBaseExternalContext.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkRepositoryContext.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/runtime/CommandEnvironment.java`

Problem to cut over:

Repository path resolution infers roots from process state and filesystem
markers instead of receiving the roots from the caller.

Hard cutover:

1. Define an explicit `RepositoryExecutionRoots` value:
   - Project/workspace root.
   - Bazel execroot.
   - External repository root.
   - Generated repository output root.
   - Buck output root.
2. Pass this value from DICE/project/cell resolution into repository rule
   execution.
3. Remove path scans under `buck-out` and marker-file ancestor guessing.
4. Make `repository_ctx.path`, `workspace_root`, command working directory, and
   external path validation use only explicit roots.
5. Add assertions that repository rule execution never reads `PWD` for semantic
   root decisions.

Tests:

- Run the same repository rule from different client working directories. Output
  must be identical.
- `PWD` spoofing does not change `repository_ctx.path` behavior.
- Nested `buck-out` path strings do not affect root discovery.

Acceptance gate:

- Repository rule roots come from typed execution context, not process cwd or
  filesystem guessing.

### #22 Repository command external input detection is string-based

Buck pointers:

- `app/buck2_interpreter_for_build/src/bazel_repository.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkBaseExternalContext.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkPath.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/vfs/Path.java`

Problem to cut over:

`repository_ctx.execute` scans strings for path-looking substrings and rewrites
or records dependencies heuristically.

Hard cutover:

1. Track repository path values as structured objects from `repository_ctx.path`
   and label/path APIs.
2. When rendering command args, record dependencies from structured path args
   directly.
3. For plain strings, do not mutate based on substring matches. Treat them as
   opaque unless the API explicitly says they are paths.
4. Add an explicit escape hatch for known tool-generated depfiles or manifests
   if needed, with structured parsing.
5. Include recorded path dependencies in repository cache keys with symlink-aware
   metadata.

Tests:

- Path object argument records dependency.
- Plain string containing `/external_cells/` but not a path is not rewritten.
- Path embedded in a flag is handled only when supplied as structured path or
  through an explicit depfile parser.

Acceptance gate:

- Repository command input tracking does not rely on scanning arbitrary command
  strings.

### #25 Archive format support omits valid Bazel formats and misclassifies `.gz`

Buck pointers:

- `app/buck2_common/src/bzlmod_archive.rs`
- `app/buck2_interpreter_for_build/src/bazel_repository.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/decompressor/DecompressorValue.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/decompressor/DecompressorDescriptor.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/decompressor/CompressedFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/decompressor/CompressedTarFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/decompressor/ArFunction.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/decompressor/SevenZDecompressor.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkBaseExternalContext.java`

Problem to cut over:

Archive support is narrower than Bazel's table, and plain `.gz` has historically
been treated as tar gzip instead of a single compressed file.

Hard cutover:

1. Replace Buck's archive type table with a Bazel-mirrored decompressor table.
2. Model separate categories:
   - Archive formats: zip, jar/war, tar, tar.gz/tgz, tar.xz, tar.zst, tar.bz2,
     tar.br, 7z, ar, deb.
   - Single compressed files: gz, xz, zst, bz2, br.
3. Preserve `strip_prefix` and `strip_components` behavior only for archive
   formats where Bazel applies them.
4. Share this decompressor table between bzlmod registry downloads and
   `repository_ctx.download_and_extract` / `extract`.
5. Add Bazel-like forced type handling for `type = ...`.

Tests:

- Plain `.gz` extracts to a single file.
- `tar.gz`, `tar.xz`, `tar.zst`, `tar.br`, `ar`, `deb`, and `7z` fixtures.
- Forced `type` overrides extension inference.
- Strip-prefix errors match Bazel for missing prefix and non-archive formats.

Acceptance gate:

- Archive behavior is table-driven from Bazel-compatible decompressor semantics,
  not extension shortcuts.

### #42 `repository_ctx.execute` uses a polling timeout loop and synthetic code 256

Buck pointers:

- `app/buck2_interpreter_for_build/src/bazel_repository.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkExecutionResult.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkBaseExternalContext.java`
- Bazel process-wrapper execution code used by repository commands.

Problem to cut over:

Buck polls every 10 ms, kills on timeout, discards useful output, and reports a
synthetic return code.

Hard cutover:

1. Replace the polling loop with an async process abstraction that supports:
   - Timeout.
   - Cancellation.
   - Controlled stdout/stderr capture.
   - Stable termination status.
2. Match Bazel's timeout result semantics as closely as the host process API
   allows.
3. Preserve captured stdout/stderr on timeout when Bazel would expose them.
4. Record execution environment, working directory, and path dependencies before
   spawn.
5. Add cancellation on repository rule restart/drop.

Tests:

- Command succeeds and captures stdout/stderr.
- Command exits non-zero and returns that code.
- Command times out and reports Bazel-compatible result fields.
- Repository rule cancellation terminates child process.

Acceptance gate:

- There is no manual sleep/poll timeout loop and no synthetic 256 timeout code
  unless Bazel-compatible for the platform.

### #43 Windows repository symlink type is guessed from target existence

Buck pointers:

- `app/buck2_interpreter_for_build/src/bazel_repository.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/vfs/Path.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/vfs/SymlinkTargetType.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/vfs/JavaIoFileSystem.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkRepositoryContext.java`

Problem to cut over:

Windows symlink creation guesses file vs directory by checking whether the
target currently exists.

Hard cutover:

1. Carry explicit symlink target type where the API or source entry knows it.
2. Add symlink target type to generated output manifests from #20.
3. For `repository_ctx.symlink`, match Bazel's target-type behavior:
   - Use unspecified type when Bazel does.
   - Use explicit file/directory type where Bazel has that metadata.
4. Avoid existence-based guesses for not-yet-created targets.
5. Add Windows-specific fallback/error behavior consistent with Bazel.

Tests:

- Windows fixture symlinks to a not-yet-existing file target.
- Windows fixture symlinks to a not-yet-existing directory target where type is
  known.
- Non-Windows behavior remains unchanged.

Acceptance gate:

- Windows symlink type decisions are metadata-driven or Bazel-unspecified, not
  target-existence guesses.

## Workstream 6: Local Execution And Clean Semantics

### #34 Local action-cache output rows are positional and not self-validating

Buck pointers:

- `app/buck2_execute_impl/src/executors/local.rs`
- `app/buck2_execute_impl/src/executors/local_action_cache.rs`

Bazel concept to align with:

- Bazel action cache stores output identity together with metadata and validates
  action cache hits against the requested output set before use.

Problem to cut over:

SQLite stores only output values and reconstructs them by zipping with the
current output set. Count matching is not enough to prove identity.

Hard cutover:

1. Version the local action cache schema.
2. Store each output row as `(output_key, output_kind, output_value)` rather than
   relying on order.
3. On cache hit, reconstruct a map by output key and compare against the current
   requested output set.
4. Recompute and compare the stored `outputs_fingerprint` before accepting a
   hit.
5. Invalidate or migrate old positional entries. Prefer invalidate-on-read for a
   hard cutover unless migration is trivial and safe.

Tests:

- Reordered output declaration does not map old values to wrong outputs.
- Missing output key rejects cache hit.
- Extra stored output rejects or invalidates cache hit.
- Old schema entries are ignored or migrated deterministically.

Acceptance gate:

- Local action cache hits are keyed by output identity, not output position.

### #35 Bazel execroot source forest leaves stale top-level links

Buck pointers:

- `app/buck2_execute_impl/src/executors/local.rs`
- `app/buck2_action_impl/src/actions/impls/run.rs`

Bazel concept to align with:

- Bazel owns and maintains its execroot/source forest so stale generated links do
  not survive source graph changes.

Problem to cut over:

Durable execroots can retain Buck-owned source symlinks after source entries are
deleted or excluded.

Hard cutover:

1. Add a source-forest manifest for each durable Bazel execroot.
2. Record every Buck-owned top-level symlink planted during preparation.
3. On each prepare, compute the desired symlink set and delete obsolete
   Buck-owned symlinks before action execution.
4. Preserve private execroot state:
   - `buck-out`
   - worker state
   - action-local directories not owned by source forest
5. Make symlink deletion robust to user-created real files by checking ownership
   markers/manifest entries before removal.

Tests:

- Source file removed from project disappears from durable execroot.
- Source excluded by new config disappears from durable execroot.
- User/action-created non-owned file is preserved.
- Repeated prepare is idempotent.

Acceptance gate:

- Durable Bazel execroots have a manifest-backed source forest cleanup pass.

### #46 `buck2 clean --background` still waits for deletion

Buck pointers:

- `app/buck2_client/src/commands/clean.rs`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/runtime/commands/CleanCommand.java`

Problem to cut over:

`--background` moves `buck-out` to trash but still awaits deletion.

Hard cutover:

1. Split clean into two phases:
   - Synchronous atomic move/rename out of the active output path.
   - Detached deletion of the moved trash path.
2. For background mode, return after the move succeeds and the deletion task has
   been detached.
3. Ensure detached cleanup survives client exit where possible:
   - Prefer a daemon-owned cleanup request if buckd remains alive.
   - Otherwise spawn a detached helper with bounded logging.
4. Report the trash path and cleanup status without blocking.
5. Preserve synchronous behavior for non-background clean.

Tests:

- Background clean returns quickly with a large fake tree.
- New builds can proceed after the move while deletion continues.
- Failed detached deletion is logged/reported without claiming synchronous
  success for deletion.
- Non-background clean still waits.

Acceptance gate:

- The background path does not await recursive deletion after the active output
  path has been moved.

## Workstream 7: Genrule Surface Recheck

### #56 Bazel `genrule` shim accepts important attrs but ignores or rejects them

Buck pointers:

- `prelude/bazel/genrule.bzl`

Bazel source to align with:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/rules/genrule/GenRuleBaseRule.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/rules/genrule/GenRuleBase.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/analysis/actions/SpawnAction.java`

Problem to cut over:

Some genrule attrs have been improved or rejected, but the remaining surface
needs a hard compatibility recheck. Important attrs must not be declared and
ignored.

Hard cutover:

1. Build a complete attr matrix from Bazel's `genrule` rule class:
   - `cmd`, `cmd_bash`, `cmd_bat`, `cmd_ps`
   - `exec_properties`
   - `local`
   - `message`
   - `output_to_bindir`
   - `heuristic_label_expansion`
   - execution requirements and tool handling
2. For each attr, choose exactly one state:
   - Fully implemented with Bazel-compatible semantics.
   - Rejected when non-default with a clear diagnostic.
   - Accepted only if proven to be an inert default.
3. Implement Windows command selection for `cmd_bat`/`cmd_ps` or reject them
   only on unsupported host/exec platforms with Bazel-like messaging.
4. Thread `exec_properties` and `local` into action execution when supported.
5. Add tests that assert every declared attr is either implemented or rejected.

Tests:

- `cmd_bash` selected on Unix.
- `cmd_bat`/`cmd_ps` behavior on Windows or explicit unsupported-platform
  rejection.
- Non-default `exec_properties`, `local`, `message`, and `output_to_bindir`
  each have expected behavior.
- Default-valued attrs do not fail.

Acceptance gate:

- `prelude/bazel/genrule.bzl` has no declared attr whose non-default value is
  silently ignored.

## Cross-Workstream Order

1. Repository identity foundation:
   - #1, #2
2. Parser/config foundation:
   - #11, #12, #28, #31, #33
3. Toolchain and execution platform foundation:
   - #5, #3, #6
4. Repository rule structured state:
   - #17, #18, #20, #21, #22
5. Generated repository cutovers:
   - #13, #14, #15, #16, #52
6. Filesystem/archive/execution cleanup:
   - #25, #42, #43
7. Local execution and user-visible clean behavior:
   - #34, #35, #46
8. Bazel tools/genrule hygiene:
   - #54, #55, #56

The first three workstreams are prerequisites for the rest. They establish the
correct identity, parser, option, and toolchain semantics that later fixes
should depend on.

## PR Slicing

Use small PRs with one hard cutover or one tightly coupled pair per PR:

1. Reversible bzlmod cell names.
2. Resolution-scoped bzlmod state.
3. MODULE evaluator cutover.
4. Bazelrc parser/typed option cutover.
5. Registry key/input cutover.
6. Override support.
7. Canonical toolchain label identity.
8. Unified toolchain/platform selection.
9. Exec groups and rule execution constraints.
10. Structured repository rule attrs and dynamic labels.
11. Repository output tree manifest.
12. Explicit repository roots and structured command input tracking.
13. Archive decompressor table.
14. Repository execute process semantics.
15. Generated Xcode config.
16. Generated C/C++ config.
17. rules_python hub.
18. Shell config.
19. Java plugins flag alias.
20. Local action cache output schema.
21. Execroot source forest manifest.
22. Background clean detach.
23. bazel_tools metadata split and snippet generation.
24. Genrule attr matrix recheck.

Each PR should include:

- A note naming the Bazel source files consulted.
- At least one regression fixture for the removed shortcut.
- The standard build gate when the blast radius touches analysis, execution, or
  generated repositories.

