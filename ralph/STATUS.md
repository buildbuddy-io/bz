# Status

_Last updated: 2026-06-16 23:20 UTC_

## Now

- Building `bz` itself from this repo (`bazel build //app/bz:bz`) — first time,
  Bazel is fetching deps. Long initial build expected.

## Environment

- VM: Linux x86_64, 8 cores, 31 GiB RAM, 159 GiB free disk.
- Tools present: bazel + bazelisk (`/usr/local/bin`), gcc/cc, go, python3.
- `bz` binary: not yet built. Will live at `bazel-bin/app/bz/bz`.

## Progress log

- 2026-06-16 23:20 — Set up `ralph/` docs. Kicked off initial `bz` build.
