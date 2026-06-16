# TODO / Backlog

## Setup
- [ ] Build `bz` binary from this repo.
- [ ] Confirm `bz --help` / `bz build` invocation works.
- [ ] Wrap `bz` in a stable `bin/bz` symlink for the loop.

## Candidate repos to build with `bz` (avoid teammate's mac-tested set)

Teammate already tested: bazelisk, bazel, buildbuddy, hermetic-llvm, bz.

Prioritize variety of rule ecosystems:
- [ ] abseil-cpp (rules_cc, C++)
- [ ] grpc (large C++, many deps)
- [ ] protobuf (C++/bzlmod)
- [ ] envoy (huge C++)
- [ ] rules_go / a Go project (gazelle)
- [ ] rules_rust example
- [ ] bazel-examples / examples from bazelbuild
- [ ] tensorflow / pytorch (very large — stretch)
- [ ] a small well-behaved bzlmod project first as smoke test

## Bugs
_(tracked in FINDINGS.md)_
