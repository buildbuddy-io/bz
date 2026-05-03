# Plan: Native Bazel Repo Support in Buck2

## Goal

Make Buck2 build Bazel repos by reading `MODULE.bazel`, resolving bzlmod modules, loading real external Starlark, and executing Buck2 actions. This is a hard cutover: no compatibility preludes for rules_go/proto/container/npm, no wrapper repos, and no nested `bazel build` fallback.

Validation ladder:

1. A simple `rules_go` repo.
2. Bazelisk.
3. BuildBuddy server target.

## Current Status

Completed:

- `bazel build //:buck2` succeeds.
- Bazel roots without `.buckconfig` get synthesized Buck config from `MODULE.bazel`, `WORKSPACE.bazel`, or `WORKSPACE`.
- Root `module(...)`, `bazel_dep(...)`, and `include(...)` are parsed.
- BCR module metadata is fetched with bounded concurrent HTTP.
- Minimal version selection is implemented for transitive bzlmod deps.
- Registry archives are downloaded, integrity-checked, patched, and materialized as external cells.
- Root and external module repo aliases are emitted into `[cell_aliases]`.
- External bzlmod cells use stable canonical names and Buck-owned output paths.
- The old prelude compatibility files for rules_go/proto/container/npm/shell are removed.
- Bazel-style `Label(...)` is available at load time.
- Bazel-style `depset(...)` is available at load time with order validation, transitive depset validation, element type checks, and `.to_list()`.
- Buck's `Label` type references in prelude were renamed to `ConfiguredProvidersLabel`.
- Bazel-style `provider()` now supports `fields = None`/omitted schemaless providers with arbitrary keyword fields and Bazel `init = ...` raw constructor tuples.
- `@io_bazel_rules_nogo` is generated as a bzlmod external cell from the `rules_go` module's imported repos.
- The first `@bazel_tools` builtin Starlark files needed by `rules_go` are present as Bazel tool definitions, not rules_go compatibility shims.
- Bazel `attr.*` constructors are native load-time globals.
- Bazel `rule(...)` accepts Bazel's implementation/metadata signature and records Buck rule specs directly.
- Bazel `repository_rule(...)`, `tag_class(...)`, and `module_extension(...)` are native first-class load-time values.
- Bazel `native`, `apple_common`, `config`, `config_common`, `cc_common`, `platform_common.TemplateVariableInfo`, `OutputGroupInfo`, and `CcInfo` load-time surfaces are present where real rules_go/rules_cc/bazel_skylib currently require them.
- Bazel `Label(...)` defaults now coerce through label/dependency/source attrs.
- Bazel `transition(...)` accepts `implementation`, `inputs`, and `outputs`.
- Bazel `coverage_common.instrumented_files_info` and `InstrumentedFilesInfo` are native load-time values with Bazel depset fields.
- Bazel `aspect(...)`, `RunEnvironmentInfo`, `platform_common.ToolchainInfo`, `configuration_field(...)`, and `proto_common_do_not_use` are native load-time values needed by real rules_go/protobuf.
- Bazel proto fragment `configuration_field(...)` label defaults now resolve to Bazel's `ProtoConfiguration` default labels.
- Bazel `aspect(...)` accepts the implementation function positionally, matching real rules_go call sites.
- Bazel current-package label shorthand such as `plugin = "name"` is coerced as `:name` for BUILD-file label attrs.
- Bazel roots now default to the bzlmod `@platforms//host:host` host platform instead of Buck's unspecified parser platform.
- `@platforms` `host_platform` extension imports are materialized as generated external cells with host OS/CPU constraints.
- BUILD-file `package(default_visibility = ...)`, Bazel special visibility labels, and top-level `licenses(...)` are native load-time APIs.
- `@bazel_tools//tools/build_defs/repo:utils.bzl` is present as a Bazel builtin tool definition.
- Gazelle `go_deps.from_file(go_mod = ...)` imports from external bzlmod modules are parsed into generated external cells.
- Generated `go_deps` Go module repos read the parent module's `go.mod`, download the selected module with `go mod download`, copy the module source, and emit Bazel `go_library` BUILD files.
- Generated `go_deps` repository config repos emit `config.json` and Bazel buildfile markers.
- The `bazel_features` `version_extension` generated repos are materialized as real Buck external cells from their bzlmod `use_repo(...)` imports.
- Bazel `rule(..., build_setting = config.*(...))` injects native `build_setting_default` attrs and build setting values participate in configuration identity.
- Bazel incoming transitions now invoke implementations as `implementation(settings, attr)`, preserve bool/string/string-list outputs, read default values from build-setting targets, store command-line option outputs in the configuration, and converge when the resulting `BuildOptions` equivalent is unchanged.
- `select({"//conditions:default": ...})` is treated as Bazel's default arm.
- Bazel common implicit rule attrs such as `deprecation`, `tags`, `testonly`, and `features` are present when rules omit them.
- Bazel `.bzl` `visibility(...)` is recorded at load time for later enforcement.
- `@bazel_tools` is a first-class bundled external cell with real Bazel package targets for the bundled tool definitions currently reached by rules_go/rules_cc.
- Bazel native command-line `config_setting.values` keys such as `compilation_mode`, `stamp`, and `strip` are normalized to `//command_line_option:*` and read Bazel defaults from configuration.
- Bazel rule transitions apply from the pre-rule configuration without Buck's post-transition attr equality invariant.
- `ctx.toolchains[...]` exists as a Bazel `ToolchainContext`, enforces declared toolchain access, and returns `None` for declared optional misses.
- `cc_common.is_cc_toolchain_resolution_enabled_do_not_use(ctx = ctx)` is available.
- `ctx.build_setting_value`, `ctx.attr`, and `ctx.var` are available during Bazel rule analysis.
- Bazel-declared rules accept `None`, a single provider, or provider sequences from analysis and receive an implicit empty `DefaultInfo` when omitted.
- `DefaultInfo(files = depset(...))`, dependency `.files`, and source-artifact `.files` are available with Bazel depset values.

Latest smoke:

```sh
BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=true \
BUCK2_HARD_ERROR=false \
bazel-bin/app/buck2/buck2_bin --isolation-dir real-rules-go-... build //:hello
```

The smoke now loads real `rules_go`, `rules_cc`, `rules_proto`, `protobuf`, `bazel_skylib`, `bazel_features`, and `gazelle` load-time Starlark from bzlmod, gets past the generated `@io_bazel_rules_nogo` repository, Gazelle `go_deps` aliases, `bazel_features` generated globals, Bazel build-setting defaults, rules_go's incoming Go transitions, bundled `@bazel_tools` package targets, Bazel provider return semantics, and source `.files` access. The current failure is in rules_go Go configuration analysis when a declared Go toolchain type is accepted by `ctx.toolchains` but no registered toolchain has been resolved:

```text
root//:hello
-> @rules_go//:go_context_data
-> @rules_go//:go_config

Object of type `NoneType` has no attribute `default_goos`
```

The direct `//:hello` smoke now reaches configured target analysis through the real `go_binary`, `go_library`, Go configuration transition, standard-library transition, and Go/CGo context setup. The next gap is replacing the placeholder `ToolchainContext` result with real Bazel registered-toolchain collection, module-extension generated `@go_toolchains` repos, `native.register_toolchains(...)`, and toolchain target resolution.

## Constraints

- `.buckconfig` continues to win over Bazel compatibility defaults.
- Resolution and generated repository materialization must be deterministic and cacheable.
- Apparent repo names are scoped by the loading module's repo mapping.
- Generated repos live under Buck-owned external-cell/cache paths, not in the user's source tree.
- Unsupported bzlmod or Bazel APIs fail explicitly at the point they are requested.

## Phase 1: Bzlmod Graph and Materialization

Status: mostly complete for BCR registry modules.

Remaining:

- Support `archive_override`, `git_override`, and `local_path_override` as real module sources.
- Preserve full per-module repo mappings instead of the current global alias superset.
- Persist resolved module metadata in a Buck-owned cache key rather than recomputing from BCR for each fresh daemon.

Acceptance:

- `@io_bazel_rules_go//go:def.bzl` resolves to the materialized `rules_go` module.
- Transitive module aliases are visible from the module that declared them.
- Version selection matches Bazel for the validation ladder.

## Phase 2: Module Extensions and Generated Repos

Status: generated repo imports for external-module `go_deps.from_file(...)` and `bazel_features` `version_extension` are parsed and materialized; full extension evaluation and scoped repo mappings still remaining.

Implement:

- Parse `use_extension(...)` bindings from `MODULE.bazel`.
- Parse extension tag calls such as `go_sdk.from_file(...)` and `go_sdk.nogo(...)`.
- Parse `use_repo(...)` imports, including aliasing syntax.
- Preserve `use_repo(...)` imports from non-root modules such as `bazel_features_globals`.
- Load and evaluate real `module_extension(...)` definitions.
- Provide `module_ctx`, `tag_class`, `extension_metadata`, and the module/tag data model needed by rules_go and Gazelle.
- Execute repository rules emitted by module extensions into generated external cells.
- Implement repository context APIs initially needed by rules_go:
  - `ctx.file`
  - `ctx.template`
  - `ctx.path`
  - `ctx.read`
  - label attrs in repository rules

Immediate target:

- Replace the current parsed generated-repo extraction with real `module_extension(...)` evaluation and repository rule execution.
- Preserve per-module repo mappings so aliases imported by `rules_go` and `gazelle` no longer collide in the global alias superset.
- Replace parsed `go_deps` materialization with real Starlark `module_extension(...)` evaluation.

Acceptance:

- The simple `rules_go` fixture gets past module-extension generated repo loading.
- `go_toolchains`, `go_host_compatible_sdk_label`, `io_bazel_rules_nogo`, and Gazelle `go_deps` repos are real generated repos/cells when imported by `use_repo(...)`.

## Phase 3: Bazel Load-Time Builtins

Status: native load-time API cutover is underway; the current rules_go smoke no longer fails on missing `attr`, `rule`, `repository_rule`, `tag_class`, `module_extension`, `native`, `apple_common`, `config`, `config_common`, `cc_common`, `coverage_common`, `platform_common.TemplateVariableInfo`, `platform_common.ToolchainInfo`, `OutputGroupInfo`, `CcInfo`, `RunEnvironmentInfo`, `aspect`, `configuration_field`, `proto_common_do_not_use`, `provider(init = ...)`, `package(default_visibility = ...)`, `licenses(...)`, `@bazel_tools` package targets, or Bazel transition globals.

Completed:

- `Label(...)`
- `depset(...)` and `.to_list()`
- Bazel-compatible schemaless `provider()`
- Bazel-compatible `provider(init = ...)`
- `attr.*`
- Bazel-compatible load-time `rule(...)` signature
- `repository_rule(...)`, `tag_class(...)`, and `module_extension(...)` load-time values
- `native.bazel_version`, `native.existing_rule`, and `native.existing_rules`
- `apple_common.platform` and `apple_common.platform_type`
- `config.*` build-setting descriptors and `config_common.toolchain_type`
- `cc_common.CcToolchainInfo`
- `coverage_common.instrumented_files_info`
- `InstrumentedFilesInfo`
- `platform_common.TemplateVariableInfo`
- `platform_common.ToolchainInfo`
- `OutputGroupInfo`
- `CcInfo`
- `RunEnvironmentInfo`
- Bazel-compatible `transition(...)` signature
- Bazel transition invocation with `settings` and `attr`
- `aspect(...)`
- `configuration_field(...)` for `coverage.output_generator` label defaults
- `configuration_field(...)` for Bazel proto fragment label defaults
- `proto_common_do_not_use`
- BUILD-file `package(default_visibility = ...)`
- Bazel special visibility labels
- `licenses(...)`
- Initial `@bazel_tools` builtin files needed by `rules_go`
- Common implicit rule attrs
- `//conditions:default` select key handling
- load-time `.bzl` `visibility(...)` recording
- first-class bundled `@bazel_tools` cell and package targets reached by rules_go/rules_cc
- Bazel-native command-line `config_setting.values` normalization
- Bazel rule transition invocation without Buck post-transition attr checks
- Bazel `ctx.toolchains[...]` access shape and optional `None` result
- `cc_common.is_cc_toolchain_resolution_enabled_do_not_use(ctx = ctx)`

Immediate target:

- Implement `native.register_toolchains(...)` for MODULE and BUILD loading.
- Materialize `go_sdk` extension repos including `@go_toolchains`, `@go_host_compatible_sdk_label`, and SDK repos from real module-extension/repository-rule execution.
- Resolve declared Bazel toolchain types to registered toolchain implementation targets and expose their `platform_common.ToolchainInfo` providers from `ctx.toolchains[...]`.
- Add Apple/Xcode provider constructors exposed by Bazel's `apple_common` only when real rules request them.

Implement as failures demand:

- `module_name`
- `module_version`
- `repo_name`
- `package`
- `exports_files`
- `glob`
- `select`
- `visibility` constants
- missing `native.*` load-time functions

Acceptance:

- Loading `@io_bazel_rules_go//go:def.bzl` reaches exported symbols without any `prelude/go/def.bzl`.
- Remaining failures are rule-analysis or repository/toolchain issues, not basic load-time missing builtins.

## Phase 4: Bazel Rule API Compatibility

Status: load-time rule declaration is partially implemented; rule analysis semantics are still ahead.

Implement enough Bazel analysis surface for real rules_go:

- Complete `attr.*` semantics beyond load-time schema capture: configurability, providers, cfg, mandatory, allow_files/allow_single_file, executable, and label coercion edge cases.
- Complete `rule(...)` analysis semantics: outputs, executable/test flags, fragments/toolchains metadata, and analysis invocation through Buck2's configured target machinery.
- `DefaultInfo`
- provider indexing and membership behavior
- executable/test rule metadata
- implicit attrs and configurable attrs
- aspects needed by `rules_go`
- enforce `.bzl` load visibility recorded by `visibility(...)`
- transitions needed by `rules_go`, including platform label semantics for `//command_line_option:platforms`

Completed for current rules_go smoke:

- build-setting rule defaults and `ctx.build_setting_value`
- Bazel provider return shapes and implicit `DefaultInfo`
- `DefaultInfo(files = depset(...))`
- dependency/source `.files`
- `ctx.attr` alias and empty `ctx.var`

Acceptance:

- A real `rules_go` `go_library` target can be analyzed by Buck2.
- Analysis creates Buck2 actions rather than shelling out to Bazel.

## Phase 5: Actions, Toolchains, and Go Execution

Implement:

- `ctx.actions.declare_file`
- `ctx.actions.declare_directory`
- `ctx.actions.write`
- `ctx.actions.run`
- `ctx.actions.run_shell`
- args/depset behavior used by rules_go
- `register_toolchains`
- toolchain target resolution
- host Go SDK/toolchain repository generation
- exec platform selection sufficient for host builds

Acceptance:

- The simple `rules_go` repo builds `//:hello`.
- No `rules_go` path loads from `prelude`.
- No nested `bazel build` process runs.

## Phase 6: Bazelisk

Run Bazelisk after the simple rules_go fixture passes.

Work failures in this order:

1. Missing bzlmod/module-extension generated repos.
2. Missing load-time builtins.
3. Missing rule/provider/action semantics.
4. Missing Go/toolchain execution support.

Acceptance:

- `buck2 build //:bazelisk` succeeds.
- The build uses real external modules and Buck2 actions.

## Phase 7: BuildBuddy Server

Run BuildBuddy after Bazelisk passes.

Work failures in this order:

1. Module resolution and repo mapping.
2. Module extensions and repository rules.
3. Bazel load-time builtins.
4. Rule/provider/action semantics.
5. Go/proto/toolchain execution.
6. Remaining domain rule sets required by the server target.

Acceptance:

- `buck2 build //server/cmd/buildbuddy:buildbuddy` succeeds.
- The build does not call `bazel build`.
- The build does not depend on rules_go/proto/container/npm prelude shims.

## Validation Commands

Use fresh isolation dirs for smoke tests:

```sh
bazel build //:buck2
```

```sh
BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=true \
BUCK2_HARD_ERROR=false \
bazel-bin/app/buck2/buck2_bin --isolation-dir bazel-compat-small \
  build //:hello //:combined //:root_alias_hello
```

```sh
BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=true \
BUCK2_HARD_ERROR=false \
bazel-bin/app/buck2/buck2_bin --isolation-dir bazelisk-smoke \
  build //:bazelisk
```

```sh
BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=true \
BUCK2_HARD_ERROR=false \
bazel-bin/app/buck2/buck2_bin --isolation-dir buildbuddy-smoke \
  build //server/cmd/buildbuddy:buildbuddy
```

## Done Definition

This work is done when Buck2 can build the BuildBuddy server target from a Bazel repo by resolving `MODULE.bazel`, loading real external module Starlark, materializing generated repositories, and executing Buck2 actions, with no rules_go-specific prelude shim and no nested `bazel build` fallback.
