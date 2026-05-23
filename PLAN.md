# Hard Cutover Plan: Bazel-Aligned Bzlmod Module Extensions

## Goal

Remove Buck2's remaining module-extension startup bottlenecks by cutting over to
Bazel's module-extension evaluation model.

The target behavior is:

- A fully cached new-daemon build reuses valid module-extension evaluations from
  `MODULE.bazel.lock` or Buck2's hidden lockfile before running the extension.
- Extension lockfile factor keys use Bazel's representation, including
  `general` for platform-independent evaluations.
- Per-extension DICE keys remain the only unit of invalidation and evaluation.
- Independent demanded extension evaluations can run concurrently, but a single
  extension implementation is not split internally.
- No compatibility wrapper, heuristic fallback, or extension-specific shortcut is
  left in the final path.

The immediate performance target is to stop rerunning
`@rules_rust//crate_universe:extensions.bzl%crate` during fully cached
new-daemon `:buck2` builds when `MODULE.bazel.lock` already contains a valid
entry.

## Bazel Model

Bazel's relevant shape in `/Users/siggi/Code/bazel`:

- `src/main/java/com/google/devtools/build/lib/bazel/bzlmod/SingleExtensionEvalFunction.java`
  - Loads one module extension per SkyKey.
  - Checks workspace and hidden lockfiles before running the extension.
  - Reuses a lockfile result when bzl transitive digest, usage digest, and
    recorded inputs are current.
  - Writes lockfile info for update/refresh modes after successful evaluation.
- `src/main/java/com/google/devtools/build/lib/bazel/bzlmod/SingleExtensionUsagesFunction.java`
  - Extracts only the usage data needed by one extension.
  - Avoids rerunning all extensions when unrelated module graph data changes.
- `src/main/java/com/google/devtools/build/lib/bazel/bzlmod/ModuleExtensionEvalFactors.java`
  - Uses `general` for platform-independent extension evaluations.
  - Uses strings like `os:mac os x,arch:aarch64` when the extension declares OS
    or arch dependence.
- `src/main/java/com/google/devtools/build/lib/bazel/bzlmod/RegularRunnableExtension.java`
  - Runs one extension implementation as one Skyframe worker task.
  - Defers repository-rule calls during Starlark evaluation and converts them to
    `RepoSpec`s after the extension returns.
- `src/main/java/com/google/devtools/build/lib/bazel/bzlmod/ModuleExtensionEvalStarlarkThreadContext.java`
  - Records repository-rule calls in a deterministic map.
  - Does not parallelize inside one extension implementation.

## Current Buck2 State

Buck2 already has most of the graph shape:

- `app/buck2_common/src/legacy_configs/cells.rs`
  - `BzlmodSingleExtensionUsagesKey` computes per-extension usage data.
  - `BzlmodResolutionKey` joins usage keys before constructing the cell graph.
  - `bzlmod_lockfile_data_from_str` parses `MODULE.bazel.lock`, but today only
    extracts generated repo names and facts for cell graph discovery.
- `app/buck2_external_cells/src/bzlmod.rs`
  - `BzlmodSingleExtensionEvalKey` computes one extension evaluation.
  - It checks Buck2's hidden lockfile before running the extension.
  - Hidden lockfile reads currently expect the empty-string eval-factor key.
  - Hidden lockfile writes only persist reproducible extension evaluations.
  - `BzlmodSingleExtensionKey` validates generated repos separately from raw
    evaluation, matching Bazel's split between eval and validation.

The gap is that Buck2 does not consume Bazel-compatible workspace lockfile
entries for extension evaluation. In the `:buck2` build, `MODULE.bazel.lock`
contains a large `@@rules_rust+//crate_universe:extensions.bzl%crate` entry, but
Buck2 still reruns the extension and spends roughly 25-30s splicing the Cargo
workspace.

## Cutover 1: Workspace Lockfile Reuse Before Evaluation

### Target Behavior

`BzlmodSingleExtensionEvalKey` must check lockfile evaluations in Bazel order:

1. Workspace `MODULE.bazel.lock`
2. Buck2 hidden lockfile
3. Run the module extension

If a lockfile entry is valid, return the lockfile evaluation and do not execute
the extension implementation.

This is a hard cutover to Bazel semantics, not a special case for
`rules_rust`.

### Implementation Steps

1. Move lockfile extension-entry parsing into a shared representation in
   `app/buck2_external_cells/src/bzlmod.rs`.
   - Parse the Bazel schema:
     - `moduleExtensions`
     - extension key
     - eval-factor key
     - `bzlTransitiveDigest`
     - `usagesDigest`
     - `recordedInputs`
     - `generatedRepoSpecs`
     - `moduleExtensionMetadata`
   - Reuse the same conversion path that hidden lockfile entries use to produce
     `BazelModuleExtensionEvaluationResult`.

2. Read the workspace lockfile from the project root before hidden lockfile.
   - Use the same project-relative lookup as
     `bzlmod_lockfile_data_from_str` in `cells.rs`.
   - Keep IO under the blocking executor, as hidden lockfile reads do today.
   - Do not route this through legacy config parsing; this is evaluation-cache
     data for a DICE key, not only cell graph discovery data.

3. Validate the workspace entry exactly like Bazel.
   - Match extension key.
   - Match eval factors.
   - Match bzl transitive digest.
   - Match usage digest.
   - Validate recorded inputs through `bzlmod_recorded_inputs_are_current`.
   - If any check fails, continue to the next Bazel-ordered source.

4. If workspace lockfile and hidden lockfile miss, run the extension and then
   write the hidden lockfile as today.
   - The final path still has no wrapper: the DICE key owns all lookup and
     evaluation.
   - Workspace lockfile is read-only during normal build.

5. Remove duplicate or partial lockfile parsers.
   - `cells.rs` may keep a lightweight generated-repo-name parser for cell graph
     bootstrapping only if it cannot depend on external-cell code.
   - The authoritative evaluation parser should live with
     `BzlmodSingleExtensionEvalKey`.

### Tests

- Unit test parsing a Bazel `MODULE.bazel.lock` extension entry with
  `general`.
- Unit test converting Bazel `generatedRepoSpecs` to
  `BazelModuleExtensionEvaluationResult`.
- Unit test that stale bzl digest misses.
- Unit test that stale usage digest misses.
- Unit test that stale recorded input misses.
- Integration test with a synthetic reproducible extension:
  - First build runs extension.
  - Second build after `buck2 kill` reuses workspace lockfile and does not emit
    an extension-evaluation span.
- Regression test for `@@rules_rust+//crate_universe:extensions.bzl%crate`
  using the existing `MODULE.bazel.lock` shape.

### Validation

- `buck2 kill`
- `BUCKD_STARTUP_INIT_TIMEOUT=90 buck2 build :buck2`
- Confirm no repeated `Splicing Cargo workspace` status when the lockfile entry
  is current.
- Confirm cache stats remain 100% for the fully cached new-daemon case.

### Risks

- Bazel workspace lockfile uses base64 digests while Buck2 hidden lockfile uses
  hex strings. The parser must normalize both forms before comparison.
- Bazel's `generatedRepoSpecs` uses `repoRuleId` and `attributes`; Buck2 hidden
  lockfile uses the internal repository-rule invocation setup. Conversion must
  be exact, not stringly typed.
- Recorded input formats must be interpreted with Bazel-compatible semantics, or
  stale lockfile entries could be incorrectly reused.

## Cutover 2: Bazel Eval-Factor Keys

### Target Behavior

Buck2 lockfile lookup and writing must use Bazel eval-factor keys:

- `general` for extension evaluations with no OS or arch dependency.
- `os:<os>` when the extension is OS-dependent.
- `arch:<arch>` when the extension is arch-dependent.
- `os:<os>,arch:<arch>` when both apply.

The empty-string factor key should not be written by Buck2 after the cutover.

### Implementation Steps

1. Add a first-class `BzlmodModuleExtensionEvalFactors` type.
   - Store `os` and `arch` as strings.
   - Implement `Display`/parse matching Bazel's
     `ModuleExtensionEvalFactors`.
   - Use `general` for the empty factor set.

2. Determine factors from the loaded extension metadata.
   - Bazel derives factors from `module_extension(os_dependent=...,
     arch_dependent=...)`.
   - Buck2's Starlark module extension object already records this data during
     evaluation loading; expose it before lockfile lookup so the key can select
     the correct entry.

3. Thread factors through `BzlmodSingleExtensionEvalKey`.
   - Include factors in workspace and hidden lockfile lookup.
   - Include factors in hidden lockfile writes.
   - Keep the DICE key stable for the same effective factors.

4. Remove empty-string lockfile writes.
   - Existing hidden lockfiles with empty-string keys may be ignored after the
     cutover.
   - The next successful reproducible evaluation writes the Bazel-aligned key.

### Tests

- Parse/display tests for:
  - `general`
  - `os:mac os x`
  - `arch:aarch64`
  - `os:mac os x,arch:aarch64`
- Lockfile read test selecting `general`.
- Lockfile read test selecting an OS/arch-specific entry.
- Hidden lockfile write test proving Buck2 writes `general`, not `""`.

### Validation

- Build `:buck2` twice with `buck2 kill` between builds.
- Inspect `buck-out/v2/cache/bzlmod_hidden/MODULE.bazel.lock`.
- Confirm new entries use `general` where Bazel would.

### Risks

- OS string spelling must match Bazel. On macOS, Bazel uses Java's `OS` enum
  string; Buck2 must map to the same lockfile string when the extension is
  OS-dependent.
- Changing hidden lockfile key spelling invalidates existing Buck2 hidden
  entries once. That is acceptable for a hard cutover.

## Cutover 3: Preserve Per-Extension Change Pruning

### Target Behavior

Extension evaluation remains keyed by the data for one extension, not by the
entire module graph.

Unrelated module graph changes must not invalidate all extension evaluations.
This mirrors Bazel's `SingleExtensionUsagesFunction`.

### Implementation Steps

1. Keep `BzlmodSingleExtensionUsagesKey` as the only producer of
   per-extension usage JSON.
   - Do not make `BzlmodSingleExtensionEvalKey` depend directly on
     `BzlmodDepGraphKey` except through usage data, bzl load data, lockfile
     values, recorded inputs, and environment.

2. Make the usage digest match Bazel's evaluation hash.
   - Buck2 currently removes `usages` for evaluation setup.
   - Verify this matches Bazel's `SingleExtensionUsagesValue.hashForEvaluation`
     behavior for import-only changes.
   - If not, change the digest input to Bazel's semantic subset.

3. Split validation from raw evaluation everywhere.
   - Raw evaluation should not rerun because an import alias changes.
   - `BzlmodSingleExtensionKey` should validate imports and repo overrides
     against the raw result.
   - This matches Bazel's `SingleExtensionEvalFunction` plus
     `SingleExtensionFunction` split.

4. Add invalidation tests.
   - Changing an unrelated module does not rerun an extension.
   - Changing `use_repo` imports only reruns validation, not raw evaluation.
   - Changing extension tags reruns the relevant extension only.
   - Changing the extension `.bzl` transitive digest reruns the relevant
     extension only.

### Tests

- DICE invalidation unit tests around `BzlmodSingleExtensionUsagesKey`.
- Event-log integration test counting extension-evaluation spans before and
  after unrelated module graph edits.
- Import-only change test proving lockfile reuse still happens.

### Validation

- Compare event logs for two builds with a controlled import-only edit.
- Confirm only validation changes, not raw extension evaluation.

### Risks

- Overly broad usage JSON hashing will keep the build correct but lose Bazel's
  pruning benefit.
- Overly narrow usage JSON hashing can reuse stale extension outputs. The digest
  definition must be explicitly tested against tags, root/dev dependency flags,
  repo overrides, extension identity, and module repo mappings.

## Cutover 4: Demand-Driven Parallel Extension Evaluation

### Target Behavior

Independent module extensions should evaluate in parallel when their generated
repos are demanded by analysis or materialization.

The implementation must remain demand-driven:

- Do not evaluate every extension during `BzlmodResolutionKey`.
- Do not prefetch every generated repo.
- Do not parallelize inside one extension implementation.
- Do not add extension-specific eager evaluation.

### Implementation Steps

1. Keep cell graph discovery static and cheap.
   - `BzlmodResolutionKey` may use workspace/hidden lockfile repo names to
     create placeholders for generated repos.
   - It should not run extension implementations.

2. Start extension eval at the earliest demanded repo boundary.
   - When a generated extension repo enters package listing, repo mapping, or
     materialization, request `BzlmodSingleExtensionKey` immediately.
   - If multiple independent generated repos are demanded, allow DICE to run
     their extension keys concurrently.

3. Remove duplicated waits.
   - Today materialization can compute repo mapping entries and then compute the
     same extension evaluation again through shared keys.
   - Keep the shared key but structure callers so a demanded repo creates one
     in-flight evaluation reused by mapping and materialization.

4. Keep repository-rule materialization separate.
   - Bazel's extension eval returns repo specs; repo fetch/materialization is
     still demand-driven by repository definition/directory requests.
   - Buck2 should continue to evaluate repository rules only for the generated
     repo being materialized, except where Bazel's repo mapping requires sibling
     specs from the same extension evaluation.

5. Instrument concurrency.
   - Emit per-extension start/end events with extension key and factor key.
   - Add a summary count for concurrently running module-extension evaluations.
   - This proves independent extension parallelism without changing semantics.

### Tests

- Synthetic project with two independent slow reproducible extensions.
  - One target demands repos from both.
  - Event log proves extension evaluations overlap.
- Synthetic project with two repos from the same extension.
  - Event log proves one extension evaluation is shared, not duplicated.
- Synthetic project with unused extension.
  - Event log proves it is not evaluated.

### Validation

- `:buck2` should no longer wait on crate-universe extension evaluation when the
  workspace lockfile is current.
- BuildBuddy and Bazel source builds should not evaluate unrelated extensions
  merely because they are present in the module graph.

### Risks

- Registering repo mappings has global side effects in Buck2. Those effects must
  stay tied to DICE value computation and fingerprints so concurrent callers do
  not race or repeatedly mutate global state.
- Starting extension eval earlier can shift error timing. Error messages must
  still point to the same extension usage and generated repo.
- DICE concurrency is only useful when more than one independent extension is
  demanded. It will not reduce the runtime of a single `rules_rust` crate
  extension if that extension must actually run.

## Patch Series

1. Add Bazel eval-factor type and tests.
2. Teach workspace lockfile parser to deserialize Bazel extension entries.
3. Convert Bazel `RepoSpec` entries into Buck2 repository-rule invocations.
4. Read workspace lockfile before hidden lockfile in
   `BzlmodSingleExtensionEvalKey`.
5. Write hidden lockfile using Bazel factor keys.
6. Add stale-digest, stale-usage, and stale-recorded-input tests.
7. Add event-log tests proving workspace lockfile reuse skips extension
   execution.
8. Add DICE invalidation tests for per-extension pruning.
9. Add demand-driven parallel extension evaluation instrumentation.
10. Add synthetic independent-extension overlap test.
11. Run the full validation matrix and compare fully cached new-daemon timing.

## Validation Matrix

Correctness:

- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build :buck2`
- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build :bazelisk`
  in `/Users/siggi/Code/bazelisk`
- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build src:bazel`
  in `/Users/siggi/Code/bazel`
- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build server`
  in `/Users/siggi/Code/buildbuddy`
- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build enterprise/server`
  in `/Users/siggi/Code/buildbuddy`

Performance:

- `buck2 kill && BUCKD_STARTUP_INIT_TIMEOUT=90 buck2 build :buck2`
- Compare against the measured current average of about `47.4s` for fully
  cached new-daemon `:buck2`.
- Expected result after workspace lockfile reuse: the visible
  `Splicing Cargo workspace` wait disappears when the lockfile is current.
- Warm daemon no-op builds should not regress.
- `:bazelisk` warm cached builds should stay within noise of the current
  baseline.

Event-log checks:

- Current workspace lockfile hit emits no `BzlmodModuleExtensionStart` for the
  reused extension.
- Hidden lockfile hit emits no `BzlmodModuleExtensionStart`.
- Workspace miss followed by hidden miss emits exactly one
  `BzlmodModuleExtensionStart`.
- Independent demanded extensions can overlap.
- Unused extensions are not evaluated.

## Cutover Criteria

The cutover is complete only when:

- Buck2 can consume Bazel workspace lockfile module-extension entries.
- Buck2 writes hidden lockfile entries with Bazel eval-factor keys.
- Empty-string hidden lockfile factor keys are no longer produced.
- `BzlmodSingleExtensionEvalKey` is the single path for raw extension
  evaluation and cache reuse.
- `BzlmodSingleExtensionKey` remains the single path for validation.
- Event logs prove lockfile reuse skips `rules_rust` crate-universe evaluation
  for a fully cached new-daemon `:buck2` build.
- The validation matrix passes.
- No extension-specific workaround remains.
