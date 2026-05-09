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
- Bazel native `config_setting(values = ...)` now separates Bazel command-line options from dotted Buck config keys, and bundled `@bazel_tools//src/conditions` includes the Bazel 9 `host_windows` and `remote` targets reached by BuildBuddy.
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
- Bzlmod canonical repo names now follow Bazel 9 naming for the validation path: unique selected modules use `name+`, multi-version modules keep `name+version`, and module-extension repos use `module++extension+repo`.
- Repository rules emitted by module extensions now expose Bazel canonical names through `repository_ctx.name` and `repository_ctx.attr.name`, matching Bazel's generated repo behavior.
- Bazel common rule attrs now include `applicable_licenses`, and Bazel `testonly = 0/1` values are accepted for common attrs reached by downloaded rules.
- `platform_common.ConstraintValueInfo` and `ctx.target_platform_has_constraint(...)` are available with native provider identity and target-platform constraint lookup.
- Bazelisk now builds `//:bazelisk` end to end with downloaded bzlmod modules, generated Gazelle/rules_go repos, the downloaded Go SDK, and Buck2 actions.
- BuildBuddy bzlmod resolution now consumes root `use_repo_rule(...)`, `archive_override(...)`, and `single_version_override(...)` data without reintroducing module-specific generated-repo materializers.
- Module-extension generated repos are grouped and reused by extension identity, avoiding repeated generated repo mapping work across large graphs.
- Bazel tag calls inside top-level `MODULE.bazel` list comprehensions now carry all comprehension bindings, including tuple bindings from `dict.items()`, into the Starlark attr evaluator.
- Bazel `py_internal` is available for downloaded rules_python load/extension code, including `regex_match`, OS naming, and bzlmod/runfiles probes.
- Bazel `config.string_list(repeatable = ...)` and `config.string(allow_multiple = ...)` load like Bazel's build-setting descriptors.
- Bazel `Label(...)` is idempotent for existing label values, matching rules_python patterns such as `Label(cv)`.
- The current `bazel_features` globals repo shape is materialized without assuming the old `LEGACY_GLOBALS` dict.
- `cc_common.internal_DO_NOT_USE()` now exposes the Bazel `cc_internal` header-info APIs reached by rules_cc/rules_java while preserving Buck's native C++ provider callables.
- `apple_common.apple_toolchain()` exposes the Bazel toolchain path helpers reached by rules_cc ObjC support.
- Bazel rules may define a user `metadata` attr without colliding with Buck's internal metadata attr.
- `module_ctx.watch(...)`, `module_ctx.report_progress(...)`, and `module_ctx.execute(...)` are available for downloaded module-extension implementations without adding module-specific toolchain logic.
- Bazel `ProvidersLabel.same_package_label(...)` and `ConfiguredProvidersLabel.same_package_label(...)` are available for downloaded repository and extension helpers.
- Module-extension evaluation now pre-materializes only the direct generated-repo aliases visible to the extension's defining module cell, and does not force unevaluated dynamic repos from other extensions.
- Bzlmod module-extension evaluation requests are emitted in dependency-first module order so dependency modules such as `rules_go` can create their generated repos before dependent extension code loads helpers through those aliases.
- `repository_ctx.original_name` is available, repository-relative paths are lexically normalized, empty `auth`/`headers` structures are accepted by download APIs, and `repository_ctx.template(...)` accepts Bazel's positional substitutions form.
- `use_repo_rule(...)` attrs now resolve supported top-level `MODULE.bazel` string constants and string-list constants into self-contained generated repository-rule calls.
- `repository_ctx.execute(...)` now prefers existing generated external-cell paths over synthetic `.repository_ctx` siblings when rewriting command paths, so reused generated repos do not wait on non-existent repository-rule scratch dirs.
- Repository-rule execution now uses source-driven label pre-materialization for direct `Label(...)`/`ctx.path(...)` attrs and repo-name string attrs used to build `@repo//...` labels, allowing downloaded repository helpers to pull dynamic generated-repo inputs without Go/Kotlin-specific logic or broad string-attr deps.
- Bazel compatibility cells now merge root/external `.bazelignore` contents into Buck's existing ignore machinery, so package discovery, globbing, exact-case checks, and file watching all honor Bazel ignore prefixes.
- `repository_ctx.patch(...)` applies unified diff patches in generated repository working directories, matching the Bazel API reached through `bazel_tools//tools/build_defs/repo:utils.bzl`.
- `module_ctx.download(...)` and `repository_ctx.download(...)` now accept Bazel SRI integrity strings for SHA-1, SHA-256, SHA-384, and SHA-512, and return Bazel-shaped `sha256`/`integrity` result fields.
- Bzlmod download APIs now retry retryable HTTP failures using Bazel-style status handling for 5xx, 403, 408, 429, timeouts, and send failures before moving to the next mirror URL.
- BCR module graph discovery is cached in-process by root deps and override set, so large-repo cell graph rebuilds after module-extension evaluation do not repeatedly refetch the same registry graph. Generated repo mappings are still recomputed from the current extension results.
- Recursive bzlmod external-cell materialization now hardlinks regular files when possible and falls back to copying, avoiding duplicate byte copies for large generated repositories while keeping symlink and directory semantics unchanged.
- Bundled external cells now declare source artifacts lazily on metadata lookup; the full bundled cell is still materializable on explicit `materialize_all(...)`, but fresh Bazel-compatible sync no longer declares the entire prelude up front.
- `repository_ctx.execute(...)` and `module_ctx.execute(...)` now fail immediately when an external command input has not been materialized instead of polling for two minutes; exact `.repository_ctx` paths are treated as ready scratch roots rather than as generated-repo roots.
- Module-extension and repository-rule label pre-materialization now scans transitive loaded `.bzl` modules, recognizes templated apparent `Label("@repo_{}//...".format(...))` labels, and avoids broad source string scanning that can materialize unrelated generated repos.
- Repository-rule execution now records labels passed to repository_ctx path/read/watch/delete/patch/symlink/template inputs. If a downloaded repository rule fails because one of those label-backed paths is not materialized yet, Buck materializes exactly the observed label repos and retries the same repository rule from a clean repository_ctx working directory.
- The broad same-extension generated-repo compatibility expansion has been removed; dynamically referenced toolchain/runtime repos must now be discovered from the downloaded module/repository-rule code at execution time.
- `repository_ctx.watch(...)` and `repository_ctx.watch_tree(...)` are available for downloaded repository rules using the same path coercion as other repository context APIs.
- Generated module-extension repo setup now carries the resolved extension `.bzl` cell/path from bzlmod resolution, so module-root-relative labels such as `use_extension(":extensions.bzl", ...)` load through the same resolved-label path as `//pkg:file.bzl`, `@repo//...`, and `@@canonical//...` labels.
- Module-extension and repository-rule source preloading no longer materializes generated bzlmod repos from static `Label(...)` strings in downloaded `.bzl` files. Concrete generated repos still materialize from recorded repository-rule label attrs and observed runtime `repository_ctx` path labels, keeping generated repos lazy without module-specific toolchain logic.
- The `bazel_features` globals parser now accepts the current simple string version form as well as legacy version ranges, restoring generated `bazel_features_globals` provider exports such as `PackageSpecificationInfo`.
- Generated repos emitted by a module extension now inherit the repo mapping visible to the extension's defining `.bzl` module, while `use_repo(...)` still controls which aliases are imported into the consuming module. This matches Bazel's extension-generated repo mapping model and lets Gazelle-generated repos resolve helper aliases such as `@package_metadata`.
- Un-lockfiled non-root module extension usages now request pre-finalization evaluation too, so generated repos see the complete same-extension sibling repo set instead of only the repos named by that module's `use_repo(...)` imports.
- `repository_ctx.execute(...)` now records missing runtime external-cell paths as concrete bzlmod cell deps and retries the repository rule after materializing those cells. This covers downloaded tool paths discovered from generated files, such as rules_go's `GOROOT` in `go.env`, without hard-coded Go/Kotlin logic.
- `repository_ctx.execute(...)` and `module_ctx.execute(...)` now accept direct Bazel `Label` values in command argument lists, resolving them through the same external-cell path and materialization machinery as `repository_ctx.path(Label(...))`.
- Generated bzlmod file delegates no longer execute repository rules or materialize generated repository directories while the cell graph is being constructed. Generated repos now materialize only when package loading or repository code performs an actual file operation in that repo.
- Bzlmod registry and generated external cells no longer declare/hash whole repository trees during ordinary fetch/generate. Source artifacts are declared lazily per observed file or symlink metadata lookup, while explicit `materialize_all(...)` still forces full-tree declaration.
- External-cell load-file reads no longer declare source artifacts. Reading BUILD and `.bzl` content now stays separate from source-artifact materialization; labels and action inputs still declare artifacts through metadata lookup.
- The in-process BCR discovery cache now stores the discovered module graph behind an `Arc`, so repeated two-phase bzlmod resolution reuses the graph without deep-cloning every discovered module.
- The MODULE.bazel call scanner now uses a non-allocating prefix fast path for non-matching lines, avoiding repeated comment-stripping of every line for every scanned function name.
- Bazel execroot artifact-path alias materialization is idempotent and repairs stale/conflicting aliases before retrying, matching Bazel action execution's expectation that generated-path aliases can be reused across actions.
- Generated bzlmod `local_config_cc` wrappers choose the host C or C++ driver from the actual source/command language instead of assuming C++, so downloaded rules_go cgo actions compile C inputs the same way Bazel does.
- Bazel-compatible run actions render command-line artifacts with Bazel exec paths and normalize Buck-owned external/generated/root artifact paths back into Bazel-shaped `external/...` and `buck-out/bin/...` paths before execution.
- Produced directory/tree outputs are precreated at the declared output root, matching Bazel tree-artifact execution behavior for actions that expect an empty declared directory to exist.
- Bazel `attr.label` and `attr.label_list` preserve downloaded aspect declarations, and analysis now runs downloaded no-required-provider aspect implementations against the owning target. This lets rules_go produce `GoProtoImports` from its real `_go_proto_aspect` without a rules_go-specific shim. Full dependency-aspect propagation for aspects with `required_providers` remains future work.
- The Bazel main repo now uses Bazel's empty canonical repo name for root labels while preserving the apparent `module(repo_name = ...)` alias for repo mapping. This matches Bazel's `StarlarkBazelModule` tag-label coercion and lets Gazelle root-module tags see `repo_name == ""`, so generated `go_deps` config includes `go.mod` like Bazel.
- Bazel-compatible local execution now prepares the execroot `buck-out` alias before output-directory creation. Output parents are therefore created through the same execroot view that the action command sees, matching Bazel's `ActionOutputDirectoryHelper` behavior for non-tree and tree outputs.
- Buck now has a persistent local action cache for local execution. Successful local actions record an action-digest to output-fingerprint entry under the isolation cache, later commands validate the real output metadata without mutating output paths, and cache hits are reported as `LocalActionCache` command execution kinds.
- Generated bzlmod repository materialization now writes Bazel-style setup stamps beside generated repos and reuses matching already-materialized repos across daemon restarts. The stamp is outside the repo tree, is cleared before re-materialization, and is written only after successful materialization.
- Bazel-compatible roots now start buckd from a minimal startup config and defer full bzlmod/module-extension resolution to command update, so daemon startup no longer performs repository resolution before the client connects.
- Bzlmod cell-graph module-extension results are persisted under `buck-out/v2/cache` per extension, matching Bazel's independent module-extension values. Fresh daemons reuse unchanged extension results and evaluate only the extensions whose usage key changed.
- BuildBuddy's current server target now builds with Buck2 actions:

```sh
BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=true \
BUCK2_HARD_ERROR=false \
/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin \
  --isolation-dir buildbuddy-fileresolver-debug build //server:server
```

The successful run executed 2357 local commands and did not call `bazel build`.

### Benchmark Summary

All BuildBuddy measurements below were taken on `/Users/siggi/Code/buildbuddy` for
`//server:server` with local-only Buck execution unless the row explicitly says otherwise.

| Benchmark | Engine | State | Wall time | Engine/command time | Work |
| --- | --- | --- | ---: | ---: | --- |
| Full server build | Bazel 9.1.0 | cold fresh output base | 302.51s | 302.33s | 4519 processes |
| Full server build | Bazel 9.1.0 | warm same output base | 1.64-2.53s | 1.57-2.48s | 0 packages analyzed, 1 internal process |
| Full server build | Buck2 before bzlmod warm cutover | warm same isolation | 127-134s | ~2:00 | 3743 targets analyzed, 0 local actions |
| Full server build | Buck2 final cutover | cold fresh isolation | 282.66s | 4:32.0 | 3738 targets analyzed, 2357 local actions |
| Full server build | Buck2 final cutover | first warm same daemon/isolation | 5.78s | 5.8s | 0 targets analyzed, 0 actions |
| Full server build | Buck2 final cutover | steady warm same daemon/isolation | 1.18s | 1.2s | 0 targets analyzed, 0 actions |
| Leaf edit: `server/util/bytebufferpool/bytebufferpool.go` | Bazel 9.1.0 | warm leaf edit | 11.23s | - | 44 actions, 9 cache hits, 42 sandboxed processes |
| Leaf edit: `server/util/bytebufferpool/bytebufferpool.go` | Bazel 9.1.0 | cold leaf edit after seeded output base | 15.57s | - | 44 processes, 4475 action cache hits |
| Leaf edit: `server/util/bytebufferpool/bytebufferpool.go` | Buck2 before local action cache | cold leaf edit after seeded isolation | 546.75s | - | 2357 local actions, 0 cached |
| Leaf edit: `server/util/bytebufferpool/bytebufferpool.go` | Buck2 with local action cache | cold same-state rebuild after daemon restart | 126.36s | - | 2357 cached actions, 0 local |
| Leaf edit: `server/util/bytebufferpool/bytebufferpool.go` | Buck2 with local action cache | cold leaf edit after daemon restart | 131.60s | - | 2343 cached actions, 14 local |
| Leaf edit: `server/util/bytebufferpool/bytebufferpool.go` | Buck2 with local action cache | warm leaf edit in same daemon | 12.36s | - | 22 commands: 8 cached, 14 local |
| High invalidation: `server/util/status/status.go` | Bazel 9.1.0 | warm exported-const edit | 15.95s | - | 219 total actions, 217 sandboxed processes, 30 action cache hits |
| High invalidation: `server/util/status/status.go` | Bazel 9.1.0 | cold reverse edit after seeded output base | 20.20s | - | 219 total actions, 217 sandboxed processes, 4300 action cache hits |
| High invalidation: `server/util/status/status.go` | Buck2 before generated-repo/materialization caches | warm exported-const edit | 22.29s | - | 120 commands: 28 cached, 92 local |
| High invalidation: `server/util/status/status.go` | Buck2 before generated-repo/materialization caches | cold reverse edit after seeded isolation | 146.05s | - | 2357 commands: 2265 cached, 92 local |
| High invalidation: `server/util/status/status.go` | Buck2 with generated repo stamps only | cold reverse edit after seeded isolation | 89.79s | - | 2357 commands: 2265 cached, 92 local |
| High invalidation: `server/util/status/status.go` | Buck2 with generated repo stamps and full module-extension cache | warm exported-const edit | 21.85s | - | 120 commands: 28 cached, 92 local |
| High invalidation: `server/util/status/status.go` | Buck2 with generated repo stamps and full module-extension cache | cold reverse edit after seeded isolation | 55.40s | - | 2357 commands: 2265 cached, 92 local |
| External module: `github.com/pkg/errors` patch | Bazel 9.1.0 | warm external-module patch | 14.01s | - | 133 total actions, 4386 action cache hits |
| External module: `github.com/pkg/errors` patch | Bazel 9.1.0 | cold reverse external-module patch | 15.72s | - | 133 total actions, 4386 action cache hits |
| External module: `github.com/pkg/errors` patch | Buck2 before per-cell bzlmod origin keys | warm external-module patch | 59-61s | - | 3738 analyses, 1708 loads, 2353 cached actions, 4 local |
| External module: `github.com/pkg/errors` patch | Buck2 with per-cell bzlmod origin keys | warm external-module patch | 16.44-21.61s | - | 0 analyses, 1 load, 76 cached actions, 4 local |
| External module: `github.com/pkg/errors` patch | Buck2 before materializer metadata cache hits | cold reverse external-module patch | 52.30s | 44.91s | 3738 analyses, 1708 loads, 2353 cached actions, 4 local |
| External module: `github.com/pkg/errors` patch | Buck2 with materializer metadata cache hits | cold same-state daemon restart | 23.65s | 16.42s | 3738 analyses, 1708 loads, 2357 cached actions, 0 local |
| External module: `github.com/pkg/errors` patch | Buck2 with materializer metadata cache hits | cold reverse external-module patch | 39.04s | 31.69s | 3738 analyses, 1708 loads, 2353 cached actions, 4 local |
| External module: `github.com/pkg/errors` patch | Buck2 with per-extension module cache | cold external-module patch after seeded isolation | 25.21s | 24.5s | 3740 targets analyzed, 2356 cached actions, 1 local |

Latest BuildBuddy timing comparison, measured on `/Users/siggi/Code/buildbuddy` for
`//server:server` with local-only execution:

| Engine | State | Wall time | Engine duration | Work |
| --- | --- | ---: | ---: | --- |
| Bazel 9.1.0 | cold fresh output base | 302.51s | 302.33s | 4519 processes |
| Bazel 9.1.0 | warm same output base | 1.64-2.53s | 1.57-2.48s | 0 packages analyzed, 1 internal process |
| Buck2 before bzlmod warm cutover | warm same isolation | 127-134s | ~2:00 | 3743 targets analyzed, 0 local actions |
| Buck2 final cutover | cold fresh isolation | 282.66s | 4:32.0 | 3738 targets analyzed, 2357 local actions |
| Buck2 final cutover | first warm same daemon/isolation | 5.78s | 5.8s | 0 targets analyzed, 0 actions |
| Buck2 final cutover | steady warm same daemon/isolation | 1.18s | 1.2s | 0 targets analyzed, 0 actions |

The warm speedup is grounded in the event logs: Buck was publishing preliminary and partial bzlmod
cell graphs during no-op warm builds, invalidating package loading and analysis even with no file
watcher events. Buck now seeds the command cell graph from previous module-extension results whose
extension usage JSON matches the current MODULE graph, validates those results through DICE after
file-watcher sync, and only publishes a new final graph if the evaluated result actually changes.
The client immediate-config path also no longer resolves bzlmod before connecting to the daemon; it
parses only real Buck config plus a minimal root cell for Bazel repos, leaving full MODULE.bazel
resolution in the daemon command update. The cold comparison is likewise grounded in the Buck event
log: the remaining critical path is real downloaded-toolchain work and package/action execution
(`GoToolchainBinaryBuild`, `GoStdlib`, protobuf C++ actions, and downstream `GoCompilePkg`/`GoLink`
actions), not bzlmod graph churn or eager generated-repo materialization.

Latest leaf-node edit timing comparison, measured by changing
`server/util/bytebufferpool/bytebufferpool.go` in BuildBuddy:

| Engine | State | Wall time | Work |
| --- | --- | ---: | --- |
| Bazel 9.1.0 | warm leaf edit | 11.23s | 44 actions, 9 cache hits, 42 sandboxed processes |
| Bazel 9.1.0 | cold leaf edit after seeded output base | 15.57s | 44 processes, 4475 action cache hits |
| Buck2 before local action cache | cold leaf edit after seeded isolation | 546.75s | 2357 local actions, 0 cached |
| Buck2 with local action cache | cold same-state rebuild after daemon restart | 126.36s | 2357 cached actions, 0 local |
| Buck2 with local action cache | cold leaf edit after daemon restart | 131.60s | 2343 cached actions, 14 local |
| Buck2 with local action cache | warm leaf edit in same daemon | 12.36s | 22 commands: 8 cached, 14 local |

The leaf-node runs show the execution-side cutover working: cold same-state rebuilds hit the local
action cache for every action, and cold leaf edits re-execute the same small affected action set as
the warm leaf edit. Buck's warm leaf time is now comparable to Bazel's. The remaining cold gap was
above execution: fresh-daemon bzlmod/load/analysis ran before the small action delta was reused.
The generated-repo materialization stamps and module-extension cell-graph cache added for the
high-invalidation benchmark below address the largest bzlmod portions of that cold setup cost.

Latest high-invalidation edit timing comparison, measured by changing exported API in
`server/util/status/status.go` in BuildBuddy:

| Engine | State | Wall time | Work |
| --- | --- | ---: | --- |
| Bazel 9.1.0 | warm exported-const edit | 15.95s | 219 total actions, 217 sandboxed processes, 30 action cache hits |
| Bazel 9.1.0 | cold reverse edit after seeded output base | 20.20s | 219 total actions, 217 sandboxed processes, 4300 action cache hits |
| Buck2 before generated-repo/materialization caches | warm exported-const edit | 22.29s | 120 commands: 28 cached, 92 local |
| Buck2 before generated-repo/materialization caches | cold reverse edit after seeded isolation | 146.05s | 2357 commands: 2265 cached, 92 local |
| Buck2 with generated repo stamps only | cold reverse edit after seeded isolation | 89.79s | 2357 commands: 2265 cached, 92 local |
| Buck2 with generated repo stamps and full module-extension cache | warm exported-const edit | 21.85s | 120 commands: 28 cached, 92 local |
| Buck2 with generated repo stamps and full module-extension cache | cold reverse edit after seeded isolation | 55.40s | 2357 commands: 2265 cached, 92 local |

The high-invalidation profile started with two concrete cold bottlenecks: fresh-daemon generated
external repo materialization/listing and bzlmod module-extension cell-graph setup. Generated repo
listing for `rules_go++go_sdk+main___download_0//` dropped from roughly 19-33s on the critical path
to about 0.1s after materialization stamps. Command setup dropped from roughly 42s before first
analysis to 9s after the complete module-extension cache path. The remaining cold gap is now mostly
normal package/load/analysis/build-key work plus the 92 invalidated local actions, not bzlmod
repository re-generation.

Latest external-module edit timing comparison, measured by adding/removing a bzlmod
`go_deps.module_override(...)` patch for `github.com/pkg/errors` in BuildBuddy:

| Engine | State | Wall time | Work |
| --- | --- | ---: | --- |
| Bazel 9.1.0 | warm external-module patch | 14.01s | 133 total actions, 4386 action cache hits |
| Bazel 9.1.0 | cold reverse external-module patch | 15.72s | 133 total actions, 4386 action cache hits |
| Buck2 before per-cell bzlmod origin keys | warm external-module patch | 59-61s | 3738 analyses, 1708 loads, 2353 cached actions, 4 local |
| Buck2 with per-cell bzlmod origin keys | warm external-module patch | 16.44-21.61s | 0 analyses, 1 load, 76 cached actions, 4 local |
| Buck2 before materializer metadata cache hits | cold reverse external-module patch | 52.30s wall / 44.91s command | 3738 analyses, 1708 loads, 2353 cached actions, 4 local |
| Buck2 with materializer metadata cache hits | cold same-state daemon restart | 23.65s wall / 16.42s command | 3738 analyses, 1708 loads, 2357 cached actions, 0 local |
| Buck2 with materializer metadata cache hits | cold reverse external-module patch | 39.04s wall / 31.69s command | 3738 analyses, 1708 loads, 2353 cached actions, 4 local |
| Buck2 with per-extension module cache | cold external-module patch after seeded isolation | 25.21s wall / 24.5s command | 3740 targets analyzed, 2356 cached actions, 1 local |

The warm external-module regression was global DICE invalidation from treating generated repo
repository-rule setup as part of cell/config identity. Buck now compares cell resolver identity by
graph shape, keeps external generated-repo origins as per-cell injected DICE values, and updates
only the generated repo origin cells whose repository-rule inputs changed. File operations for
external cells read that per-cell origin key, so an external-module patch dirties the changed
generated repo and its actual reverse deps instead of the full BuildBuddy graph. The remaining warm
time is real local Go work, dominated by the final BuildBuddy `GoLink`, not bzlmod sync.

The cold external-module profile then exposed local action-cache hit validation as a separate
Bazel-aligned bottleneck. Bazel's persistent action cache stores output metadata and reinjects it
into output metadata state on cache hits. Buck's local action cache was re-reading and hashing real
outputs from disk on every persistent hit after daemon restart, which put multi-second cached C++ and
Go actions on the critical path. Buck now asks the persisted materializer for declared output
`ArtifactValue`s, validates those values against the local action-cache fingerprint, and only falls
back to disk output calculation when metadata is missing. The cold same-state command duration is now
comparable to Bazel's cold reverse command time.

The next cold external-module profile showed the remaining gap was no longer action execution but
monolithic bzlmod module-extension result caching: changing the `go_deps` usage key missed the full
33-extension result-set cache and spent 28.47s in command-start setup before actions could begin.
Bazel keys module extensions independently, so Buck now persists one cache entry per extension and
seeds the preliminary cell graph from the unchanged cached extensions before evaluating only the
missing requests. The same `github.com/pkg/errors` patch now spends 3.05s in command-start overhead,
runs one local `GoCompilePkg`, and completes in 25.21s wall / 24.5s command time. The remaining gap
to Bazel's 15.72s cold external-module run is mostly cold daemon startup, scheduler waiting, and
normal rules_go SDK/package analysis rather than broad bzlmod re-evaluation.

Latest smoke:

```sh
BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=true \
BUCK2_HARD_ERROR=false \
/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin \
  --isolation-dir buildbuddy-fileresolver-debug build //server:server
```

Bazelisk now builds `//:bazelisk` end to end in a fresh isolation after loading downloaded rules_go/Gazelle/protobuf modules, invoking real module extensions and generated repository rules, materializing the downloaded Go SDK from runtime repository-rule inputs, analyzing rules_go targets, and running Buck2 actions. An earlier `bazelisk-runtime-inputs-1` rerun reached the same rules_go action phase but failed because the machine had only 699 MiB free; after clearing stale generated `buck-out` isolation directories, `bazelisk-runtime-inputs-2` succeeded.

BuildBuddy now gets through the earlier rules_python `pip`, rules_java/rules_cc, Apple toolchain, rules_webtesting `metadata`, aspect_rules_js `module_ctx.watch`, rules_go generated-alias ordering, rules_nodejs empty download-auth, rules_jvm_external `use_repo_rule` constant/template, repository_ctx `.repository_ctx` path reuse, dynamic generated-repo executable input materialization, `repository_ctx.patch`, and SHA-384 SRI integrity boundaries. The `buildbuddy-download-retry-1` smoke no longer failed on unsupported `sha384-...` integrity and did not repeat the transient GitHub 502 before it returned to the known DICE synchronization park after large generated-repo expansion. The follow-up `buildbuddy-bcr-cache-1` smoke moved past repeated BCR network resolution and into generated repository materialization for rules_python, rules_go, Gazelle, googleapis, rules_oci, aspect_rules_js, Node, and Go SDK repos. The `buildbuddy-bcr-cache-hardlink-1` smoke confirmed hardlinked generated repo files with link count 2 and reached at least 233 generated repos before being stopped; the active nodes were still generated external-cell file delegates for rules_nodejs and Gazelle repos during pre-analysis sync.

The `buildbuddy-source-label-deps-2` smoke replaced broad module-extension pre-materialization with source/tag-label-driven pre-materialization. It reached only 22 generated repo directories during sync, with the 320M `rules_go++go_sdk+main___download_0` archive dominating disk usage instead of hundreds of unrelated generated repos. The remaining active boundary moved to final synchronization work in cell-graph/module parsing after the extension pass, not eager generated external-cell realization.

A follow-up rerun showed bzlmod alias registration rebuilding large per-cell maps during the same synchronization phase. Alias registration now stores the already-normalized alias vectors directly; after that change, the diagnostic boundary moved to bundled `prelude` source artifact declaration/hashing during fresh-daemon sync.

The `buildbuddy-repo-watch-1` smoke cleared the aspect_rules_js `yq` external executable boundary by pre-materializing labels found in transitively loaded helper modules. It also exposed the cost of the broad same-extension dynamic-repo compatibility expansion: rules_python host-runtime setup pulled unrelated platform implementation repos from the same extension even though the downloaded rule picks one host implementation at runtime.

The `buildbuddy-path-label-retry-1` smoke replaces that broad expansion with repository_ctx-observed path-label retries. It still clears the rules_python host-runtime and `repository_ctx.watch(...)` boundaries, but the generated tree stays small: about 87M with host Python repos, rules_go host SDK label, rules_java/rules_cc support repos, and rules_python config repos, instead of hundreds of unrelated generated repos or non-host Python platform repos. The command was stopped after returning to the known long synchronization wait; DICE and thread dumps showed no concrete missing API, missing generated repo, or active IO boundary.

The `bazelisk-resolved-extension-label-1` regression smoke cleared the `rules_jvm_external+` `use_extension(":extensions.bzl", "maven")` load failure by using the bzlmod-resolved extension cell/path instead of parsing the raw MODULE label as a normal `load()` import. It then executed module-extension work far enough to create the `rules_go` and `rules_python` extension working directories before returning to the known long synchronization wait. It was stopped after thread/DICE inspection showed the daemon parked without a concrete missing API or active external command.

The `buildbuddy-lazy-generated-delegate-1` and `buildbuddy-lazy-source-declare-1` smokes moved the slow BuildBuddy server build away from eager generated-repo expansion. During synchronization, generated repos stayed around 2 to 6 directories instead of the earlier 838 generated dirs and 3.2G external-cell footprint. The source-declaration rerun still reached about 61M of extracted registry/source bytes, but daemon samples no longer showed `sha2`, `build_file_metadata`, `declare_existing`, or CAS digest stacks, which indicates the full-tree hashing/declaration boundary has been removed from normal sync.

The `buildbuddy-load-read-lazy-1` smoke removed the next load-file materializer boundary. A sample of the previous run showed `ReadFileKey` driving bundled external-cell `declare_write`, immediate writes, and SHA-256 while loading files. After separating content reads from source-artifact declaration, the fresh sync stayed at 2 external-cell dirs and 3.3M when sampled, with no broad `declare_write` stack; later sync had 12 module/generated dirs and 9.1M. The active cost is now normal Starlark/module evaluation plus registry module extraction, not materializer writes for loaded files.

The `buildbuddy-bcr-arc-cache-1` smoke confirmed the BCR cache no longer spends the sync sample deep-cloning `DiscoveredBcrModule` trees. The follow-up `buildbuddy-module-scan-fastpath-1` smoke kept the same low external footprint at the sampled point, 2 dirs and 3.3M, while `collect_bzl_calls` dropped out as a top-level sample entry. The visible cost moved to bzlmod resolution work, Starlark import evaluation, and residual archive/hash work.

The `buildbuddy-repo-contents-cache-1`, `buildbuddy-repo-contents-cache-hit-1`, and `buildbuddy-bundled-lazy-digest-1` smokes added a Buck-owned bzlmod repository contents cache for normal registry modules. Cold runs populate extracted and patched module contents keyed by declared module/archive/patch inputs; fresh isolations then materialize those repos by hardlink/copy from the cache instead of downloading and hashing archives again. Bundled cells now compute file digests only for metadata requests instead of hashing all bundled contents while constructing the delegate. The warmed sample kept the same low external footprint, 20 dirs and 3.3M in working external cells with a 71M repository contents cache, and no longer showed `http_download`, `copy_and_hash`, `sha2`, or bundled delegate-wide `from_content` stacks. The active cost moved to cached repo copy/link materialization and Starlark parse/eval.

The `buildbuddy-cache-root-symlink-2`, `bazelisk-lazy-cache-files-1`, and `bazelisk-lazy-cache-files-2` smokes moved normal registry modules from whole-repo cache copies to cache-backed file delegates. Normal bzlmod cells now keep the external-cell root empty, read file contents/directories/metadata from the extracted repo cache, and hardlink/copy only observed source files into the external root when metadata is requested for action inputs. Generated repo helpers that still need direct filesystem reads, such as `bazel_features` globals and rules_cc toolchain templates, resolve those reads through a canonical-repo cache alias instead of requiring the full module tree in the external root. The Bazelisk rerun got past the previous `bazel_features` `private/globals.bzl` lazy-root failure, and the latest samples no longer show the cached repo copy path; the remaining visible cost is Starlark parse/eval.

The `buildbuddy-unlockfiled-ext-1` smoke confirmed that a fully lazy module-extension cutover is still blocked by Buck2's static cell graph: rules_go emits sibling repos such as `@main___download_0` that must be known before package/analysis label parsing. The eager pre-finalization extension pass remains, but Bazel-compatible evaluation now treats every file from the active Bazel prelude cell as Bazel-compatible, so downloaded Bazel rules do not pay Buck/Starlark static typechecking for `@prelude//:rules.bzl` and its transitive Buck prelude loads.

The `buildbuddy-server-cutover-2` smoke passed the previous generated NodeJS input-file boundary and the Bazel 9 `@bazel_tools//src/conditions:host_windows` boundary. The next blocker was a Bazel module-extension repo mapping mismatch: `aspect_bazel_lib++toolchains+coreutils_toolchains` referenced sibling repos such as `@coreutils_darwin_arm64`, but Buck had not evaluated the non-root `aspect_bazel_lib` toolchains extension and therefore only mapped the imported `coreutils_toolchains` repo. Buck now evaluates un-lockfiled non-root extension usages before final cell graph construction, matching Bazel's rule that extension-generated repos see all repos emitted by the same extension.

The `buildbuddy-server-cutover-3` smoke cleared that same-extension repo mapping blocker and reached aspect_rules_js npm package import materialization. The next failure was `repository_ctx.execute([Label("@bsd_tar_toolchains_darwin_arm64//:tar"), ...])` passing the label string to `execve`; Buck now resolves direct Label command arguments to materialized external-cell paths.

The `buildbuddy-fileresolver-debug` smoke cleared the remaining server action boundaries: stale execroot alias conflicts in protobuf C++ actions, C-vs-C++ host wrapper selection for rules_go cgo, Bazel exec-path rendering for generated/root artifacts in run actions, Bazel-style precreation of produced tree outputs, and rules_go `GoProtoImports` production through the downloaded `_go_proto_aspect`. The current `//server:server` target now builds successfully with Buck2 actions.

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

Status: module dependencies, generic `use_extension(...)` bindings, extension tags, and `use_repo(...)` imports are discovered. Repo aliases are now applied through per-module bzlmod mappings: the root module sees its root-visible imports, while downloaded module cells see the deps and extension imports declared by their own `MODULE.bazel`. Extension-imported repos are represented as generated bzlmod cells using Bazel canonical module/extension/repo names, and each generated module-extension repo now carries a serialized generic module/tag usage graph for its extension, including dev-dependency tagging. The interpreter loads the downloaded `module_extension(...)` symbol, coerces tags through the extension's real `tag_class` attrs, populates `module_ctx.modules`, and invokes the real implementation before the final cell graph is rebuilt. `module_ctx` currently supports the APIs reached by rules_go, Gazelle, aspect_rules_js, rules_nodejs, rules_jvm_external, and rules_js extension paths: `ctx.os`, `ctx.getenv`, `ctx.path`, `ctx.read`, `ctx.watch`, `ctx.report_progress`, `ctx.execute`, `ctx.download`, and `extension_metadata(...)`. `repository_rule(...)` calls emitted by real module extensions are recorded with exported rule id, Bazel canonical repo name, original apparent repo name, and generic keyword values, and those downloaded repository-rule implementation functions now execute into generated external cells via `repository_ctx.file`, `repository_ctx.template`, `repository_ctx.path`, `repository_ctx.read`, `repository_ctx.watch`, `repository_ctx.watch_tree`, `repository_ctx.report_progress`, `repository_ctx.delete`, `repository_ctx.download`, `repository_ctx.download_and_extract`, `repository_ctx.patch`, `repository_ctx.symlink`, and `repository_ctx.original_name`. Source-driven repository-rule input pre-materialization now handles direct label attrs and repo-name string attrs used to construct labels without hard-coded toolchain knowledge, and execution-time repository_ctx path-label observation handles dynamic labels that cannot be known statically. The previous Go/Kotlin Rust-side generated repo materializers and broad same-extension dynamic-repo expansion have been removed from this path.

Implement:

- Represent `use_extension(...)` bindings from `MODULE.bazel` without keying behavior on extension names.
- Capture extension tag calls as structured Starlark calls associated with their extension proxy.
- Preserve `use_repo(...)` imports, including aliasing syntax and imports from non-root modules.
- Preserve per-module repo mappings so aliases imported by `rules_go`, `gazelle`, and other dependencies do not collide in the root alias set.
- Carry the generic module/tag usage graph needed to populate `module_ctx.modules`.
- Execute repository rules emitted by module extensions into generated external cells using the generic recorded repository-rule invocations.
- Implement repository context APIs initially needed by downloaded module/repository rules:
  - `ctx.file`
  - `ctx.template`
  - `ctx.path`
  - `ctx.read`
  - `ctx.report_progress`
  - `ctx.delete`
  - `ctx.download`
  - `ctx.download_and_extract`
  - `ctx.patch`
  - `ctx.original_name`
  - label attrs in repository rules

Immediate target:

- Use the successful Bazelisk and BuildBuddy server builds as regression checks while expanding the same generic bzlmod/module-extension machinery to more Bazel repos and targets.
- Resume from the `buildbuddy-path-label-retry-1` boundary: BCR discovery is no longer the hot path during cell graph rebuilds, hardlink-first materialization avoids duplicated `.repository_ctx` file contents, bundled prelude files are lazy, and dynamic repository-rule label inputs are pulled on demand from repository_ctx usage instead of same-extension expansion.
- Continue tightening source-driven, load-driven, and execution-observed generated repo materialization. `evaluate_bzlmod_module_extension_repo` no longer forces `MODULE.bazel` metadata for every input module and visible generated alias before loading an extension, and repository-rule retries now use concrete labels observed while executing downloaded code rather than forcing whole extension groups.
- Keep module-extension `.bzl` resolution tied to the bzlmod resolver's resolved cell/path data. The evaluator should not reinterpret raw `MODULE.bazel` labels as normal Starlark `load()` imports or add extension-specific label fallbacks.
- Keep `.bazelignore` support wired through `CellFileIgnores`; BuildBuddy's root ignores `buck-out`, `node_modules`, and website output trees, which matters for recursive package discovery and globs in the large-repo smoke.
- Preserve Bazel's external `.bazelignore` semantics while making them lazy: Bazel's `PackageLookupFunction` checks the main repo ignore file before package lookup and checks an external repository's ignore file after fetching that repository, so Buck should not fetch unrelated generated repos solely to compute ignores during global cell setup.
- Keep the low generated-repo footprint and cache-backed module materialization behavior while hardening the successful BuildBuddy server path. Generated repository realization, full-tree source declaration, load-file materializer writes, BCR graph cloning, repeated MODULE call comment scanning, registry archive download/hash/extract for cached modules, bundled delegate-wide content hashing, whole-repo cache copy/link materialization, and Buck/Starlark static typechecking of Bazel prelude loads are no longer intended to be the main cost.
- Retire any remaining module-specific generated-repo materializers as their extensions become executable.

Current validation boundary:

- The simple rules_go smoke resolves root and downloaded-module aliases separately, loads rules_go through downloaded bzlmod module cells, evaluates downloaded module extensions before final cell graph injection, rebuilds generated bzlmod cells from emitted repo names, materializes generated repos from downloaded repository-rule implementations, and runs Buck2 actions.
- Bazelisk builds `//:bazelisk` end to end in a fresh isolation, including downloaded rules_go/Gazelle/module-extension loading, module-root-relative `use_extension(":...")` labels, generated Go SDK materialization from runtime repository-rule inputs, rules_go analysis, and Buck2 local actions.
- The older smoke fixture's root-level `@rules_proto` load is intentionally not root-visible without a direct `bazel_dep`, matching Bazel 9.1.0 behavior.
- BuildBuddy builds `//server:server` after clearing the concrete missing-API, generated-alias, repository patch, SRI checksum, BCR rediscovery, duplicated repository-output copy, broad generated-repo pre-materialization, alias-registration map rebuild, bundled prelude declaration, aspect_rules_js templated label, rules_python dynamic host runtime, repository_ctx watch, eager generated-repo delegate construction, eager full-tree source declaration, external load-file materializer-write, BCR graph clone, repeated MODULE call-scan, bzlmod registry archive re-download/re-hash, bundled delegate-wide digest, whole cached-repo materialization, Bazel-prelude static typecheck, execroot alias, C wrapper language selection, Bazel exec-path rendering, produced tree-output precreation, and rules_go proto aspect boundaries.

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
- `platform_common.ConstraintValueInfo`
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
- `ctx.target_platform_has_constraint(...)`
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

Status: accepted.

The Bazelisk validation target now builds successfully with:

```sh
/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin \
  build --isolation-dir bazelisk-target-platform-constraint-1 //:bazelisk
```

Acceptance:

- `buck2 build //:bazelisk` succeeds.
- The build uses real external modules and Buck2 actions.

## Phase 7: BuildBuddy Server

Status: accepted for the current top-level server target.

BuildBuddy now builds successfully with:

```sh
BUCK2_TEST_SKIP_DEFAULT_EXTERNAL_CONFIG=true \
BUCK2_HARD_ERROR=false \
/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin \
  --isolation-dir buildbuddy-fileresolver-debug build //server:server
```

Work failures in this order:

1. Module resolution and repo mapping.
2. Module extensions and repository rules.
3. Bazel load-time builtins.
4. Rule/provider/action semantics.
5. Go/proto/toolchain execution.
6. Remaining domain rule sets required by the server target.

Acceptance:

- `buck2 build //server:server` succeeds.
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
  build //server:server
```

## Done Definition

This work is done when Buck2 can build the BuildBuddy server target from a Bazel repo by resolving `MODULE.bazel`, loading real external module Starlark, materializing generated repositories, and executing Buck2 actions, with no rules_go-specific prelude shim and no nested `bazel build` fallback.
