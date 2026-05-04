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
- The hard-coded Go/Kotlin generated-repo paths have been removed from the bzlmod resolver and external-cell materializer; those repos must now come from real `module_extension(...)` and repository-rule execution.
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
- Bazel native `toolchain_type(...)` and `toolchain(...)` rules are available from real BUILD files.
- Native `toolchain(...)` targets now emit an internal `DeclaredToolchainInfo` provider containing the toolchain type, selected implementation label, target/exec constraints, target settings, and `use_target_platform_constraints`.
- `register_toolchains(...)` declarations from root and BCR modules are collected, dependency-module `//...` patterns are qualified to their declaring module cell, and non-literal MODULE expressions are not misparsed as target patterns.
- Configured targets now resolve declared Bazel toolchain types against registered native `toolchain(...)` targets and expose resolved `platform_common.ToolchainInfo` providers through `ctx.toolchains[...]`.
- Bazel `Label(...)` Starlark values now coerce through plain `attrs.label()` values as well as dependency/source attrs.
- `cc_common.is_cc_toolchain_resolution_enabled_do_not_use(ctx = ctx)` is available.
- Native Bazel C/C++ providers needed by rules_cc (`DebugPackageInfo`, `CcToolchainConfigInfo`, `CcSharedLibraryInfo`, `CcSharedLibraryHintInfo`, and `cc_common.CcToolchainInfo`) are provider callables with native provider identity.
- `native.*` exports in the prelude now include Bazel native names that are not Buck prelude rules, while existing Buck native rules still win for concrete rule creation.
- Bazel native `cc_*` declarations are present for rules_cc wrapper calls.
- Bazel native Java declarations are present for `java_import`, `java_runtime`, `java_toolchain`, `java_package_configuration`, `java_proto_library`, and `java_lite_proto_library`.
- Bazel `genrule(toolchains = ...)` is accepted.
- Bzlmod generated repos can materialize checksum-verified `http_archive` outputs.
- `rules_java` `toolchains` extension imports for `@remote_java_tools` and platform-specific Java tools archives are generated as bzlmod external cells from the module's `use_repo(...)` imports.
- Bazel compatibility cells admit source labels for files that do not exist at package-load time, matching Bazel's source-file target behavior for generated JDK header filegroups.
- `ctx.build_setting_value`, `ctx.attr`, and `ctx.var` are available during Bazel rule analysis.
- Bazel-declared rules accept `None`, a single provider, or provider sequences from analysis and receive an implicit empty `DefaultInfo` when omitted.
- `DefaultInfo(files = depset(...))`, dependency `.files`, and source-artifact `.files` are available with Bazel depset values.
- Provider callables are hashable and compare by Bazel provider identity, so real Starlark can use providers as dict keys during load.
- Bazel `attr.string(values = ...)` and `attr.int(values = ...)` remain string/int attrs with separate allowed-value validation instead of being lowered to Buck enums; defaults outside `values` load like Bazel.
- Generated `rules_cc` `local_config_cc` repos now contain concrete host `cc_toolchain_suite`, `cc_toolchain`, wrapper scripts, and `CcToolchainConfigInfo` rules rather than empty placeholders.
- Bazel `cc_toolchain(...)` can be analyzed as a normal configured target when referenced directly by `native.toolchain(...)`.
- `cc_common.create_cc_toolchain_config_info(ctx = ctx, ...)` consumes `ctx` instead of storing the analysis context in the provider.
- Bazel toolchain target and execution constraint matching follows `alias(actual = ...)` labels.
- Bazel `glob(allow_empty = ...)` is accepted by BUILD-file glob evaluation.
- Bazel output-file package targets are modeled for `attr.output()`, `attr.output_list()`, and rule `outputs = ...`, and `ctx.outputs` exposes the predeclared artifacts during analysis.
- Bazel canonical repo label syntax such as `@@rules_go+0.57.0//go:sdk` resolves to the matching bzlmod cell.
- Downloaded `module_extension(...)` implementations are now loaded and invoked for generated bzlmod extension repos.
- `module_ctx` now exposes the real extension usage graph, tag-class-coerced attrs, label-valued attrs, `ctx.os`, `ctx.getenv`, `ctx.path`, `ctx.read`, `ctx.download`, and Bazel-compatible `extension_metadata(...)` keyword names.
- `repository_rule(...)` calls emitted by real module extensions are recorded generically with rule id, repo name, and kwargs.
- Recorded `repository_rule(...)` calls from real module extensions can now execute their downloaded implementation functions into generated external cells.
- `repository_ctx` now supports the generic APIs reached by the real rules_go SDK repository rule: `report_progress`, `delete`, `download`, and `download_and_extract`, in addition to `file`, `template`, `path`, and `read`.
- Repository-rule materialization now preserves binary/tree outputs produced in the repository_ctx working directory, not just the text file manifest.
- BCR metadata fetching uses a less fragile registry HTTP budget and lower concurrency for larger transitive module graphs.
- Bazel Starlark test and executable rules declared with `rule(test = True, ...)` / `rule(executable = True, ...)` now receive Bazel's implicit attrs (`size`, `timeout`, `flaky`, `shard_count`, `local`, `args`, and `output_licenses` as applicable).
- Bzlmod registered toolchain patterns from dependency modules now resolve `@repo//...` through that module's repo mapping after extension-generated repos are known.
- Generated module-extension repos now receive Bazel-style repo mappings: the hosting module's visible repos plus repos generated by the same extension under their internal names when those names are statically visible from `use_repo(...)` imports or extension tag repo-name attrs.
- Buck command setup now performs a two-phase bzlmod cutover: build a preliminary graph, evaluate real downloaded module extensions, then rebuild the final cell graph from the emitted repository-rule repo names before analysis starts.
- Generated bzlmod cells now use emitted module-extension repo names as the source of truth for generated cells and same-extension repo mappings, instead of relying on `use_repo(...)`/tag-name prediction.
- Bazel `module_extension(...)` accepts the implementation function positionally, private `repository_rule(...)` values emitted from module extensions can execute, label values expose Bazel `repo_name`/`workspace_name`, and `json.encode_indent(...)`/`json.indent(...)` are available.
- Bzlmod cells receive Bazel compatibility config by cell identity, avoiding marker-file probing that materializes generated repos while computing buckconfig.
- Bazel generic archive extraction in `repository_ctx.download_and_extract` now preserves SDK tar layouts reached by real rules_go repository rules.
- Bazel `@@canonical_repo//...` load labels, `select()` label keys, and Bazel label attrs with `allow_files`/`allow_single_file` now follow Bazel source-vs-target coercion closely enough for real rules_go packages.
- Bazel Starlark attribute transitions declared with `attr.label(cfg = transition(...))` execute as Bazel split transitions and expose transitioned deps through `ctx.attr` as lists.
- Bazel analysis now exposes `ctx.file`, `ctx.files`, `ctx.executable`, `ctx.configuration`, `ctx.bin_dir`, `ctx.genfiles_dir`, `ctx.features`, `ctx.disabled_features`, `ctx.coverage_instrumented()`, and `ctx.runfiles(...)` for the rules_go paths reached so far.
- Bazel `DefaultInfo` now carries `files`, `files_to_run`, `default_runfiles`, and `data_runfiles`; Bazel rule analysis accepts omitted `DefaultInfo` for positional and named Bazel `rule(...)` declarations.
- Bazel `ctx.actions.args`, `ctx.actions.declare_file`, `ctx.actions.declare_directory`, `ctx.actions.symlink`, `ctx.actions.run_shell`, and named-parameter `ctx.actions.run` are wired to native Buck actions for the direct/transitive input and output shapes reached by rules_go.
- Bazel command-line `Args` supports the `add`, `add_all`, `add_joined`, direct depset values, hidden depset/input expansion, map_each, uniquify, and param-file API surface reached by rules_go; real param-file action lowering is still pending.
- Bazel user provider instances now report Starlark type `struct`, matching Bazel `StarlarkInfo` behavior used by real rules_go provider helpers, while the abstract `Provider` type remains available for provider APIs and type checking.
- Bazel `ctx.info_file` and `ctx.version_file` now expose stable/volatile workspace-status artifacts backed by a generic Buck write action.
- Bazel `File.path`/`dirname` for generated bzlmod external source artifacts now resolve to Buck-owned external-cell materialization paths, so downloaded tools such as the Go SDK are invoked from their real execution-root locations.
- Bazel `Args.add_all`/`add_joined` with directory expansion now expands input tree artifacts to their leaf entries during Buck action command-line rendering, matching Bazel's default `expand_directories = True` behavior.
- Bazel compatibility cells now use Bazel-native `filegroup` semantics, returning only the declared source files in `DefaultInfo.files` instead of creating Buck synthetic directory outputs for empty filegroups.
- Bazel `File.path`/`dirname` for declared output artifacts now resolves through the active Buck output root/configuration path, so real rules can pass output directories to actions.
- The simple downloaded `rules_go` bzlmod smoke repo now builds `//:hello` with Buck2 actions.
- Bazelisk now advances through real Gazelle bzlmod setup: `native.package_relative_label`, Bazel rule initializers, public-by-default `exports_files(...)`, repository-rule label attrs, same-module generated-repo label prefetch, and repository-rule command/symlink path remapping are implemented generically.

Latest smoke:

```sh
BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=true \
BUCK2_HARD_ERROR=false \
bazel-bin/app/buck2/buck2_bin --isolation-dir real-rules-go-output-path-2 build //:hello
```

After the two-phase bzlmod cutover, the root smoke no longer stops at the pre-analysis cell-graph boundary for dynamically emitted repos such as `rules_go__download_0_darwin_amd64`. The simple rules_go smoke loads rules_go from its downloaded bzlmod module, invokes real module extensions and repository rules, materializes the Go SDK repository, analyzes the simple Go package, and runs the real `rules_go` local actions needed for `//:hello`.

Bazelisk is now the active validation target. The latest Bazelisk smoke gets through Gazelle's generated config repo, materializes the Gazelle cache repo from real `non_module_deps`, builds far enough to invoke the real `go_repository_tools` repository rule, and now fails while running Gazelle's helper source-list check:

```text
list_repository_tools_srcs: open internal/go_repository_tools_srcs.bzl: no such file or directory
```

The next cut is to finish repository-rule execution-root parity for `ctx.symlink(...)` and command working directories so the symlinked Gazelle source tree is visible exactly where the helper expects it.

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

Status: module dependencies, generic `use_extension(...)` bindings, extension tags, and `use_repo(...)` imports are discovered. Repo aliases are now applied through per-module bzlmod mappings: the root module sees its root-visible imports, while downloaded module cells see the deps and extension imports declared by their own `MODULE.bazel`. Extension-imported repos are represented as generated bzlmod cells using module/extension/repo canonical names, and each generated module-extension repo now carries a serialized generic module/tag usage graph for its extension, including dev-dependency tagging. The interpreter loads the downloaded `module_extension(...)` symbol, coerces tags through the extension's real `tag_class` attrs, populates `module_ctx.modules`, and invokes the real implementation. `module_ctx` currently supports the APIs reached by rules_go's `go_sdk` extension path: `ctx.os`, `ctx.getenv`, `ctx.path`, `ctx.read`, `ctx.download`, and `extension_metadata(...)`. `repository_rule(...)` calls emitted by real module extensions are recorded with exported rule id, repo name, and generic keyword values, and those downloaded repository-rule implementation functions now execute into generated external cells via `repository_ctx.file`, `repository_ctx.template`, `repository_ctx.path`, `repository_ctx.read`, `repository_ctx.report_progress`, `repository_ctx.delete`, `repository_ctx.download`, and `repository_ctx.download_and_extract`. The previous Go/Kotlin Rust-side generated repo materializers have been removed; the remaining work is to move extension evaluation early enough that dynamically emitted repos are known before the cell resolver is finalized.

Implement:

- Represent `use_extension(...)` bindings from `MODULE.bazel` without keying behavior on extension names.
- Capture extension tag calls as structured Starlark calls associated with their extension proxy.
- Preserve `use_repo(...)` imports, including aliasing syntax and imports from non-root modules.
- Preserve per-module repo mappings so aliases imported by `rules_go`, `gazelle`, and other dependencies do not collide in the root alias set.
- Carry the generic module/tag usage graph needed to populate `module_ctx.modules`.
- Execute repository rules emitted by module extensions into generated external cells using the generic recorded repository-rule invocations.
- Implement repository context APIs initially needed by rules_go:
  - `ctx.file`
  - `ctx.template`
  - `ctx.path`
  - `ctx.read`
  - `ctx.report_progress`
  - `ctx.delete`
  - `ctx.download`
  - `ctx.download_and_extract`
  - label attrs in repository rules

Immediate target:

- Persist the emitted repository-rule invocation payloads in generated-cell setup so materialization does not re-evaluate the extension to rediscover the matching invocation.
- Rerun the rules_go smoke after the generated-cell config probing fix and continue from the next concrete failure in real generated repository materialization.
- Retire the remaining module-specific generated-repo materializers as their extensions become executable.

Current validation boundary:

- A Bazel-valid rules_go smoke repo with direct `rules_proto` visibility now resolves root and downloaded-module aliases separately, loads rules_go through its real downloaded module cell, evaluates downloaded module extensions before final cell graph injection, rebuilds generated bzlmod cells from emitted repo names, and advances into materializing transitive generated repos from downloaded modules. The old `rules_go__download_0_darwin_amd64` unknown-alias boundary has been removed.
- The older smoke fixture's root-level `@rules_proto` load is intentionally not root-visible without a direct `bazel_dep`, matching Bazel 9.1.0 behavior.

Acceptance:

- The simple `rules_go` fixture gets past module-extension generated repo loading.
- `go_toolchains`, `go_host_compatible_sdk_label`, `io_bazel_rules_nogo`, Gazelle `go_deps`, and Kotlin compiler capability repos are produced by the downloaded modules' extension/repository-rule code when imported by `use_repo(...)`.

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
- Bazel native `toolchain_type(...)` and `toolchain(...)` declarations
- Internal `DeclaredToolchainInfo` provider emitted by native `toolchain(...)`
- Bazel `ctx.toolchains[...]` access shape and optional `None` result
- `cc_common.is_cc_toolchain_resolution_enabled_do_not_use(ctx = ctx)`
- Native rules_cc provider exports and `cc_common.CcToolchainInfo`
- `native.*` names bridged into Buck's prelude `native` struct
- Bazel native `cc_*` declaration surface for rules_cc wrappers
- Bazel native Java toolchain/runtime/import/proto declaration surface for rules_java wrappers
- Bazel `genrule(toolchains = ...)`
- hashable provider callables
- Bazel `attr.string(values = ...)`/`attr.int(values = ...)` allowed-value metadata

Immediate target:

- Exercise the new Bazel output-file targets once module-extension execution reaches the generated Go SDK BUILD file again.
- Replace parsed/generated module-repo materialization with real module-extension/repository-rule execution.
- Materialize transitive toolchain extension repos such as `@local_config_cc_toolchains`, `@local_config_shell`, `@local_jdk`, `@pythons_hub`, and `@rules_pkg_rpmbuild` by running their downloaded extension/repository-rule code.
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

Status: real rules_go analysis is now underway. The simple smoke reaches Go package action construction through downloaded rules_go and the materialized Go SDK, analyzes 165 targets, and starts real local actions. The former post-analysis wait was a generic action self-dependency: Bazel rules may pass outputs as command-line arguments to `ctx.actions.run/run_shell`, and those frozen output artifacts must not be visited as action inputs. Remaining work is driven by the next concrete action-execution failure.

Implement enough Bazel analysis surface for real rules_go:

- Complete `attr.*` semantics beyond load-time schema capture: configurability, providers, cfg, mandatory, allow_files/allow_single_file, executable, and label coercion edge cases.
- Complete `rule(...)` analysis semantics: `attr.output()` / `attr.output_list()` package targets, rule `outputs = ...`, executable/test flags, fragments/toolchains metadata, and analysis invocation through Buck2's configured target machinery.
- `DefaultInfo`
- provider indexing and membership behavior
- executable/test rule metadata
- implicit attrs and configurable attrs
- aspects needed by `rules_go`
- enforce `.bzl` load visibility recorded by `visibility(...)`
- transitions needed by `rules_go`, including platform label semantics for `//command_line_option:platforms`

Completed for current rules_go smoke:

- build-setting rule defaults and `ctx.build_setting_value`
- Bazel provider return shapes and implicit `DefaultInfo`, including positional Bazel `rule(_impl, ...)` declarations
- `DefaultInfo(files = depset(...))`, `files_to_run`, `default_runfiles`, and `data_runfiles`
- dependency/source `.files`
- `ctx.attr` alias, `ctx.file`, `ctx.files`, `ctx.executable`, and empty `ctx.var`
- `ctx.configuration.coverage_enabled`, `ctx.configuration.host_path_separator`, `ctx.bin_dir.path`, `ctx.genfiles_dir.path`, `ctx.features`, `ctx.disabled_features`, and `ctx.coverage_instrumented()`
- `ctx.info_file` and `ctx.version_file` as stable/volatile workspace-status artifacts
- `ctx.runfiles(files = ..., transitive_files = ...)` with merge and merge_all
- Bazel artifact `path`, `dirname`, `basename`, `extension`, and `root.path`
- Bazel Starlark attribute split transitions for `attr.label(cfg = transition(...))`
- Bazel label attrs that admit source files via `allow_files`/`allow_single_file`
- Bazel user provider instances report `struct` for `type(...)`, while provider identity remains available for indexing and membership.
- Bazel `Args.add_all`/`add_joined` accept direct depset values in addition to iterable sequences.
- Bazel action outputs embedded in command-line artifacts render with the action output path and are not collected as inputs, matching `ctx.actions.run/run_shell` output-argument usage in rules_go.

Acceptance:

- A real `rules_go` `go_library` target can be analyzed by Buck2.
- Analysis creates Buck2 actions rather than shelling out to Bazel.

## Phase 5: Actions, Toolchains, and Go Execution

Status: the simple downloaded `rules_go` bzlmod fixture builds `//:hello` with Buck2 actions and no rules_go compatibility prelude.

Implement remaining:

- Complete `ctx.actions.write` Bazel keyword compatibility as failures require.
- Complete `ctx.actions.run` metadata/tool/input-manifest/unused-inputs behavior beyond the named executable/arguments/inputs/outputs/env shape reached so far.
- Complete `ctx.actions.run_shell` parity beyond the direct command/arguments shape reached so far.
- Lower Bazel Args param files to real action param files instead of accepting the API as a no-op.
- Add remaining args/depset behavior used by rules_go and later validation repos.
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
