# bazel-examples/frontend

- **Source:** bazel-examples/frontend
- **Local path:** `~/work/bazel-examples/frontend`
- **Ecosystem:** JS/TS — aspect_rules_js, rules_nodejs, aspect_rules_ts,
  aspect_rules_swc, aspect_rules_jest, aspect_rules_rollup/webpack, aspect_bazel_lib.

## Results

| Command | Result |
| --- | --- |
| `bz build //...` (first) | ❌ F18 (`native.toolchain` Label coercion) — fetched full JS toolchain (~648s) |
| `bz build //...` (after F18) | ⏳ **1,632 build actions ran** (TS compile, SWC, bundling) then ❌ F19 |

## Status: ✅ JS/TS ecosystem largely works; ⏸ blocked on F19

After fixing F18, the JS/TS build executed **1,632 local actions** — TypeScript
compilation, SWC transpilation, and bundling all work under bz. The build then hit
F19 when a js_binary gathers runfiles through aspect_bazel_lib's `copy_to_bin`
(coreutils toolchain).

## Bugs surfaced

- **F18** — NODEP_LABEL string attr (`native.toolchain`'s `toolchain`) rejected a
  `Label` object. **Fixed** (unblocked the build to 1,632 actions).
- **F19** — toolchain key matching didn't resolve apparent repo aliases
  (`@bazel_lib` vs canonical `aspect_bazel_lib+`). **Fixed** (keys_match repo-relative
  fallback) → coreutils toolchain resolves, build reaches ~1,664 actions.
- **F24** — copy-to-bin double-bind in js_binary runfiles (`Attempted to bind an
  artifact which was already bound`). Documented, deferred — last gap for js_binary.

## Note

JS builds fetch a large toolchain + npm tree on first run (node, swc, npm packages
from registry.npmjs.org). Subsequent runs are cached.
