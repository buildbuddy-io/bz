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
- Bazel-style `provider()` now supports `fields = None`/omitted schemaless providers with arbitrary keyword fields.
- `@io_bazel_rules_nogo` is generated as a bzlmod external cell from the `rules_go` module's imported repos.
- The first `@bazel_tools` builtin Starlark files needed by `rules_go` are present as Bazel tool definitions, not rules_go compatibility shims.

Latest smoke:

```sh
BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=true \
BUCK2_HARD_ERROR=false \
bazel-bin/app/buck2/buck2_bin --isolation-dir real-rules-go-... build //:hello
```

The smoke now loads real `rules_go` from bzlmod, gets past the generated `@io_bazel_rules_nogo` repository, loads the initial `@bazel_tools` builtin files, and gets past `depset(...)`. The current failure is:

```text
Variable `attr` not found, did you mean `Attr`?
```

That is the next hard cutover point into Bazel's real load-time rule API: `attr.*` and `rule(...)` must be native Bazel-compatible globals, not compatibility macros.

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

Status: first generated repo implemented; full extension evaluation still remaining.

Implement:

- Parse `use_extension(...)` bindings from `MODULE.bazel`.
- Parse extension tag calls such as `go_sdk.from_file(...)` and `go_sdk.nogo(...)`.
- Parse `use_repo(...)` imports, including aliasing syntax.
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

- Replace the current parsed `rules_go` generated-repo extraction with real `module_extension(...)` evaluation and repository rule execution.

Acceptance:

- The simple `rules_go` fixture gets past module-extension generated repo loading.
- `go_toolchains`, `go_host_compatible_sdk_label`, and `io_bazel_rules_nogo` are real generated repos/cells when imported by `use_repo(...)`.

## Phase 3: Bazel Load-Time Builtins

Status: partial.

Completed:

- `Label(...)`
- `depset(...)` and `.to_list()`
- Bazel-compatible schemaless `provider()`
- Initial `@bazel_tools//tools/cpp:toolchain_utils.bzl` and `@bazel_tools//tools/build_defs/cc:action_names.bzl` builtin files needed by `rules_go`

Implement as failures demand:

- `attr.*`
- `rule(...)`
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

Status: current blocker.

Implement enough Bazel analysis surface for real rules_go:

- `attr.*` constructors with Bazel-compatible defaults, configurability, providers, cfg, mandatory, allow_files/allow_single_file, executable, and label coercion.
- `rule(...)` with implementation binding, attrs schema capture, outputs, executable/test flags, fragments/toolchains metadata, and analysis invocation through Buck2's configured target machinery.
- `DefaultInfo`
- `OutputGroupInfo`
- provider indexing and membership behavior
- executable/test rule metadata
- implicit attrs and configurable attrs
- aspects needed by `rules_go`
- transitions needed by `rules_go`

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
