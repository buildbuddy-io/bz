# Ralph Wiggum Loop — Building Open-Source Bazel Projects with `bz`

This directory documents an autonomous, persistent effort to build a variety of
open-source Bazel-based projects using **`bz`** (BuildBuddy's Buck2-derived build
tool, this repo) instead of Bazel, in order to surface and fix bugs in `bz`.

## What is `bz`?

`bz` is a Buck2-derived build tool with Bazel-compatibility features: it reads
`MODULE.bazel` / bzlmod, understands Bazel cells, and aims to build Bazel
projects. It is written in Rust and built with Bazel (`bazel build //app/bz:bz`).

## The loop

For each target repository:

1. **Pull** the repo (clone locally on this VM).
2. **Build** it with `bz`.
3. **Document** any bug / incompatibility uncovered in `bz`.
4. **Fix** the bug in `bz` (config changes to the target repos are also fine).
5. **Commit** often (we're in a fork: `altdansalt/bz`, upstream `buildbuddy-io/bz`).

## Ground rules / context

- We're in a fork. Commit frequently; upstream later.
- Running inside tmux — be persistent, keep learning, keep trying.
- Build everything locally on this VM (8 cores, 31 GiB RAM, Linux x86_64).
- Teammate has already tested (on mac): bazelisk, bazel, buildbuddy,
  hermetic-llvm, bz. **Pick different repos** where possible.

## Files

- `STATUS.md`   — current state, what's building now, high-level progress.
- `TODO.md`     — running task list / backlog of repos and bugs.
- `FINDINGS.md` — bugs found in `bz`, root causes, fixes.
- `builds/`     — per-repo build logs and notes (`<repo>.md`).
