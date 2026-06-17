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

- `STATUS.md`   — current state, full bug tally, repos tested, environment.
- `TODO.md`     — running task list / backlog of repos and bugs.
- `FINDINGS.md` — bugs found in `bz`, root causes, fixes (F1–F17).
- `builds/`     — per-repo build logs and notes (`<repo>.md`).

## Results (see STATUS.md for the live table)

**11 `bz` bugs fixed, verified & committed; 6 documented/deferred. 9 repos/projects
across 7 rule ecosystems.** C++, Python, Java, Maven, Go, and Rust all validated
end-to-end (build + run + test where applicable).

Fixed: F1 `ctx.exec_groups`, F2 `config_setting define_values`, F3 `.lds` in cc
deps, F4 `py_internal.cc_helper`, F6 root-module override patches, F7
`repository_ctx.getenv`, F8 bare relative `Label()`, F11 `cc_common.merge_cc_infos`,
F13 `actions.run unused_inputs_list`, F14 (F3 regression guard), F15
`download(block=False)`.

Deferred (deep bz internals): F5 bare native cc rules, F9 android
`config_feature_flag`, F10 `linkstatic=0` link, F12 go `//...` shared-action
(narrow — specific targets work), F16 rules_oci `layer_mtree`, F17
`local_path_override` outside project root.

Each fix follows: pull → `bz build` → root-cause in bz source → fix → rebuild →
re-verify past the error → commit. Most fixes mirror bz's existing Bazel-compat
patterns (e.g. stub providers like `java_common.merge`).
