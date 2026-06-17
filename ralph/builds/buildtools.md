# bazelbuild/buildtools (buildifier) — real-world Go

- **Source:** https://github.com/bazelbuild/buildtools (shallow clone)
- **Ecosystem:** rules_go + gazelle + protobuf (real Go project: buildifier/buildozer).

## Result — ⏸ blocked on F20 (generated Go source)

| Target | Result |
| --- | --- |
| `//buildifier:buildifier` (go_binary) | ❌ after 135 actions: `build/parse.y.baz.go` not found |

The Go build runs 135 actions (Go SDK + compilation), then fails: `build/parse.y.baz.go`
is a **goyacc-generated Go source** listed in a go_library's `srcs`, and bz treats it
as a missing *source* (same root cause as F20 / the zlib genrule headers).

## Significance

Confirms **F20 is broad**: it blocks any repo with generated sources in `srcs`/`hdrs`
(proto, yacc, generated headers), not just zlib/proto. A high-value, real-world
blocker. See FINDINGS F20 for the corrected fix direction (dep-coerce + `allow_files`
provider exemption).
