# protobuf

- **Source:** https://github.com/protocolbuffers/protobuf (shallow clone, `36.0-dev`)
- **Local path:** `~/work/protobuf`
- **Rule ecosystem:** huge multi-language — cc, java, python, kotlin, ruby, rust,
  android, proto; bzlmod with many deps (abseil, re2, zlib, rules_*).

## Results (`bz build //:protoc`)

Progressively surfaced compat bugs, each fixed in turn:

| Bug | Issue | Status |
| --- | --- | --- |
| F6 | override patch label `@com_google_protobuf//:...` (root self-ref) | ✅ fixed |
| F7 | `repository_ctx.getenv` missing (rules_android) | ✅ fixed |
| F8 | `Label("foo.bzl")` bare relative label (rules_kotlin) | ✅ fixed |
| F9 | `config_feature_flag` native rule undefined (rules_android) | ⛔ deferred |

## Status: ⏸ blocked on Android-ecosystem gap (F9), deferred

`//:protoc` is pure C++ but protobuf's full module graph forces bz to evaluate
rules_android's `androidsdk` repo BUILD, which uses Android native rules bz lacks
(`config_feature_flag`). Three real bzlmod/Starlark compat bugs (F6/F7/F8) were
found and fixed here before hitting the Android wall. Deferred in favor of breadth;
worth returning to (either implement `config_feature_flag` or investigate why bz
eagerly evaluates the android toolchain repo for a C++ build).
