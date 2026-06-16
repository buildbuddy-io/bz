# Status

_Last updated: 2026-06-16 23:40 UTC_

## Now

- Rebuilding `bz` with fix for **F1** (`ctx.exec_groups` on `AnalysisContext`),
  surfaced by `bz build //...` on abseil-cpp (cc_test targets).

## Done

- Built `bz` binary (`bazel build //app/bz:bz`), wrapped at `~/bin/bz`.
- abseil-cpp: libraries build cleanly with `bz`. cc_test targets surfaced F1.

## Environment

- VM: Linux x86_64, 8 cores, 31 GiB RAM, 159 GiB free disk.
- Tools present: bazel + bazelisk (`/usr/local/bin`), gcc/cc, go, python3.
- `bz` binary: not yet built. Will live at `bazel-bin/app/bz/bz`.

## Progress log

- 2026-06-16 23:20 — Set up `ralph/` docs. Kicked off initial `bz` build.
- 2026-06-16 23:25 — `bz` built. Smoke-tested abseil-cpp: libs build, cc_test → F1.
- 2026-06-16 23:40 — Implemented F1 fix (exec_groups), rebuilding.
