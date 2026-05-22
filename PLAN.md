# Hard Cutover Plan: Bazel-Aligned Analysis/Execution Overlap

## Goal

Replace Buck2's current command-level build orchestration with a single DICE
graph that can overlap execution with analysis. The required user-visible
outcome is:

- For a build with one large top-level target, action execution can begin before
  all analysis for that build has completed.
- The new path is the only path. No fallback mode, compatibility wrapper, or
  speculative "try both" implementation remains after cutover.
- The implementation stays aligned with Bazel's Skymeld model where that model
  applies: analysis and execution are requested through graph keys, not by a
  post-analysis command loop.

## Important Bazel Alignment Constraint

Bazel's Skymeld is not a general "execute during transitive analysis of one
target" mechanism. In Bazel source:

- `BuildRequestOptions.java` exposes
  `--experimental_merged_skyframe_analysis_execution`.
- `SkymeldModule.java` decides whether that mode is active.
- `SkyframeBuildView.java` maps top-level configured targets/aspects to
  `BuildDriverKey`s.
- `BuildDriverFunction.java` waits for the top-level `ActionLookupKey`, then
  requests `TargetCompletionValue` / `TestCompletionValue`.
- `AnalysisOperationWatcher.java` releases queued execution when enough
  top-level entities have concluded analysis.

That means strict Bazel Skymeld primarily overlaps execution for already
analyzed top-level entities with analysis of other top-level entities. It does
not, by itself, guarantee meaningful overlap inside one single top-level target.

To satisfy the single-top-level-target requirement, Buck2 needs to adopt the
Bazel graph-driver shape, but lower the driver granularity below Bazel's
top-level-only `BuildDriverKey`. The aligned principle is "execution is a graph
dependency of completion keys"; the Buck2-specific extension is "completion keys
exist for demanded artifacts/action owners, not only top-level targets."

## Current Buck2 Behavior

The current build command has an effective phase boundary:

- `app/buck2_server_commands/src/build.rs` wraps the whole build in
  `ctx.with_linear_recompute(...)`.
- `build_targets_for_spec(...)` schedules top-level targets with
  `FuturesUnordered`, but each target calls `build_target(...)`.
- `build_target(...)` calls `build::build_configured_label(...)`.
- `build_configured_label_inner(...)` first awaits
  `get_outputs_for_top_level_target(...)`.
- Only after top-level providers/outputs are available does it spawn
  `materialize_and_upload_artifact_group(...)`.
- Artifact materialization eventually requests `BuildKey`, which performs action
  execution.

For one large top-level target, this means execution starts only after that
target's analysis has produced its top-level output set, which usually implies
the relevant transitive analysis has drained.

## Target Architecture

### 1. DICE Build Driver Keys

Add first-class DICE keys that replace command-level build orchestration:

- `BuildDriverKey`
  - Keyed by `ConfiguredProvidersLabel`, requested provider/output set, and
    command-level build options that affect completion semantics.
  - Equivalent role to Bazel's `BuildDriverKey`.
  - Created per requested top-level target/aspect.
  - Must be created anew per command where command-specific semantics matter,
    matching Bazel's `BuildDriverKey.valueIsShareable() == false`.

- `TargetCompletionKey`
  - Equivalent role to Bazel's `TargetCompletionValue`.
  - Computes the requested providers/output groups for a configured target.
  - Requests artifact completion keys for every demanded output artifact group.
  - Produces the target build result currently assembled by
    `build_configured_label_inner(...)`.

- `ArtifactCompletionKey`
  - Keyed by `ArtifactGroup` or resolved artifact identity.
  - For source artifacts, validates/materializes as today.
  - For build artifacts, requests the producing `BuildKey`.
  - For transitive set projections and directory artifacts, delegates to the
    existing `EnsureTransitiveSetProjectionKey`, `DirArtifactValueKey`, and
    related keys.

- `BuildKey`
  - Remains the action execution key.
  - Its role becomes purely graph-driven: it is requested by completion keys,
    never by command orchestration after a separate analysis phase.

### 2. Demand-Driven Intra-Target Overlap

To overlap within one top-level target without executing unrelated actions, the
graph needs artifact demand before the entire top-level analysis result has
returned.

Hard cutover rule:

- Do not execute every action produced by an analyzed target.
- Do not execute every default output of every dependency.
- Only execute artifacts reachable from the requested top-level output groups,
  tests, run providers, or validation requirements.

Implementation approach:

1. Split "analysis finished" from "demand discovered".
   - Keep `AnalysisKey` as the owner of Starlark analysis.
   - Add a per-command `BuildDemandContext` in DICE transaction data.
   - When top-level provider/output demand is known, record demanded artifact
     groups immediately instead of waiting for the command loop.

2. Make artifact demand resumable.
   - `ArtifactCompletionKey` must be able to request an artifact whose producing
     action owner has not completed analysis yet.
   - If action lookup cannot find the action yet, it must request the owning
     `AnalysisKey` and resume through normal DICE dependency tracking.
   - This is the DICE equivalent of Bazel Skyframe returning missing values and
     restarting the requesting function.

3. Publish demands during analysis where semantically known.
   - When a top-level `DefaultInfo`, `OutputGroupInfo`, `RunInfo`, or test
     provider is constructed for a requested target, publish those artifact
     groups to `TargetCompletionKey`.
   - When analysis registers an action whose inputs are already demanded by an
     in-flight completion key, request completion for those inputs immediately.
   - If provider construction is only visible after the rule implementation
     returns, start with top-level completion after provider return, then move
     provider/output publication earlier as a separate hardening step.

4. Preserve demand-only semantics.
   - Early execution must be triggered by a DICE dependency from a completion key,
     not by an out-of-band "run all discovered actions" queue.
   - Any action executed early must also be executed by the old demand path for
     the same requested outputs.
   - Add event-log assertions to prove no extra `BuildKey`s were requested.

### 3. Execution Gate And Resource Isolation

Mirror Bazel's separation of analysis and execution scheduling without a phase
barrier:

- Keep one DICE graph transaction for the command.
- Add an execution go-ahead gate only for resource protection, not correctness.
- Default go-ahead threshold should be immediate for Buck2's single-target goal.
- If a threshold is retained, it must be an explicit DICE/build option and must
  not recreate the old "wait until analysis is done" behavior.
- Execution must continue to use existing resource accounting and dynamic local
  parallelism. Analysis tasks must not starve action execution, and action
  execution must not starve DICE analysis.

## Implementation Plan

### Phase 0: Baseline Instrumentation

Add a small event-log based measurement harness before changing behavior:

- Record timestamp of first `ActionExecutionStart`.
- Record timestamp of last `AnalysisEnd`.
- Record all `AnalysisStart`/`AnalysisEnd` and `ActionExecutionStart`/end pairs.
- Add a synthetic single-top-level target test where analysis intentionally keeps
  doing work after at least one demanded action can become known.
- Baseline assertion should currently show:
  `first_action_execution_start >= last_analysis_end` for the large single
  top-level case.

This validates the problem and gives a regression test for the cutover.

### Phase 1: Introduce Graph Driver Keys

Add a new build-driver module under `app/buck2_build_api/src/build/`:

- `driver.rs`
  - `BuildDriverKey`
  - `BuildDriverValue`
  - conversion from requested CLI target/provider options to driver keys

- `completion.rs`
  - `TargetCompletionKey`
  - `ArtifactCompletionKey`
  - helper conversion from existing `ProviderArtifacts` result structures

Move logic out of:

- `app/buck2_build_api/src/build.rs::build_configured_label_inner`
- `app/buck2_server_commands/src/build.rs::build_target`
- `app/buck2_server_commands/src/build.rs::build_targets_for_spec`

The command should request `BuildDriverKey`s from DICE and collect their values.
It should not manually sequence "configure target, get outputs, spawn builds".

### Phase 2: Remove Command-Level Phase Boundary

Delete the old orchestration path:

- Remove the build command's dependency on `build_configured_label(...)` as the
  execution entrypoint.
- Remove the command-level `FuturesUnordered` build loop that owns output
  materialization.
- Remove the broad `ctx.with_linear_recompute(...)` wrapping of the full build
  unless a smaller, proven DICE dependency-tracking boundary remains necessary.

After this phase, the command does only:

1. Parse and resolve patterns.
2. Construct configured top-level driver keys.
3. Request those keys from DICE.
4. Format results.

### Phase 3: Make Completion Demand Resumable

Refactor artifact/action lookup so completion keys can be requested before all
owners finish analysis:

- `ArtifactCompletionKey` resolves `ArtifactGroup` to producing action keys.
- `ActionCalculation::get_action(...)` must cleanly depend on the owning
  `AnalysisKey` when the action is not registered yet.
- `BuildKey` should not assume analysis has globally completed.
- Cycles and errors should surface through DICE dependency errors, not through
  command-level special cases.

Expected effect:

- If a demanded artifact's producing target has completed analysis, its action
  can execute immediately.
- If the producing target is still analyzing, completion waits on exactly that
  target's analysis, not on global analysis completion.

### Phase 4: Publish Demand Earlier Within Analysis

This is the phase that makes single-top-level overlap meaningful.

Add earlier demand publication points:

- Top-level provider/output demand:
  - Publish requested `DefaultInfo`, `OutputGroupInfo`, run, and test artifacts
    as soon as they are semantically constructed for a requested top-level
    target.

- Action input demand:
  - When an action is registered by analysis and that action is part of an
    already demanded completion path, request completion of its inputs.
  - This lets dependency actions execute while later parts of the same top-level
    target's analysis continue.

- Validation demand:
  - Move validation into a completion key so it can run alongside output
    completion without forcing a command-level barrier.

If Starlark provider construction is not observable before the rule
implementation returns, do not fake it. First land the driver/completion graph,
then split analysis result publication so demanded top-level outputs can be
published before unrelated remaining analysis work completes.

### Phase 5: Hard Cutover Result Handling

Port the existing result behavior onto driver values:

- `ConfiguredBuildEventVariant::Prepared`
- `ConfiguredBuildEventExecutionVariant::BuildOutput`
- validation errors
- skipped incompatible targets
- missing targets
- target rule type names
- provider collections needed for `buck2 run`
- streaming build report updates
- detailed aggregated metrics and action graph sketches

All events must come from driver/completion key evaluation, not from the old
command loop.

### Phase 6: Error, Cancellation, And Keep-Going Semantics

Match Bazel's important Skymeld behavior:

- In keep-going mode, analysis failures for one branch should not prevent
  independent demanded outputs from executing.
- In fail-fast mode, cancellation should stop both analysis and execution
  promptly.
- Analysis-concluded events must be deduplicated per driver key, similar to
  Bazel's `BuildDriverFunction` event deduplication.
- Execution errors should normalize back to the configured target/action owner
  so user-facing errors match current Buck2 output.

### Phase 7: UI And Metrics

Update the UI to make overlap visible:

- "Analyzing targets" and "Executing actions" should both show live non-zero
  running counts during overlapped builds.
- Add a debug summary:
  - first action start timestamp
  - last analysis end timestamp
  - overlap duration
  - number of actions that started before analysis completed
- Keep existing comma formatting and action totals.

### Phase 8: Validation Matrix

Correctness builds:

- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build :buck2`
- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build :bazelisk` in
  `/Users/siggi/Code/bazelisk`
- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build src:bazel` in
  `/Users/siggi/Code/bazel`
- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build server` in
  `/Users/siggi/Code/buildbuddy`
- `/Users/siggi/Code/buck2/bazel-bin/app/buck2/buck2_bin build enterprise/server`
  in `/Users/siggi/Code/buildbuddy`

Behavioral tests:

- Single top-level synthetic target proves:
  `first_action_execution_start < last_analysis_end`.
- Multi-top-level target proves no regression in the natural Skymeld case.
- Keep-going and fail-fast tests cover analysis errors, execution errors, and
  mixed analysis/execution failures.
- Incompatible and skipped targets preserve current behavior.
- Streaming build reports update while execution is still running.

Performance gates:

- Warm no-op `:bazelisk` remains at or below current baseline plus noise.
- Warm no-op `:buck2` does not regress.
- Cold `src:bazel` does not show a serious wall-time regression.
- Action count must not increase unless an intentional provider/output demand
  explains it.
- Peak memory must not grow materially from retaining analysis and execution
  state concurrently.

## Cutover Criteria

The hard cutover is complete only when:

- The build command no longer calls the old `build_configured_label(...)`
  orchestration as the primary execution path.
- All target completion and artifact execution requests flow through DICE keys.
- Event logs prove at least one single-top-level build starts action execution
  before all analysis ends.
- No unconditional "execute all actions discovered during analysis" behavior
  exists.
- The full validation matrix passes.
- Performance is within agreed thresholds.

## Main Risks

- Exact demand-only intra-target overlap may require earlier provider/output
  publication than Buck2 analysis currently exposes.
- Retaining analysis and execution data at the same time may increase peak
  memory.
- Execution can starve analysis unless executor/resource policy is explicit.
- Early action execution changes timing of errors and events; keep-going and
  fail-fast behavior need dedicated tests.
- If we execute actions before proving they are reachable from requested outputs,
  the implementation is not Bazel-aligned and can do unnecessary work.

## Recommended First Patch Series

1. Add overlap instrumentation and the synthetic single-target test.
2. Introduce `BuildDriverKey` and `TargetCompletionKey` without changing
   behavior.
3. Switch the command to request driver keys and remove the old command-owned
   output build loop.
4. Make `ArtifactCompletionKey` resumable on owner `AnalysisKey`.
5. Publish top-level output demand earlier and prove single-target overlap.
6. Remove dead compatibility code and update docs.
