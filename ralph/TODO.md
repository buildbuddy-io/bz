# TODO / Backlog

## Done ✅
- [x] Build `bz` binary; wrapper at `~/bin/bz`.
- [x] abseil-cpp, re2, googletest, cpp-tutorial (rules_cc)
- [x] re2 python (rules_python/pybind)
- [x] java-tutorial, java-maven (rules_java, rules_jvm_external)
- [x] go-tutorial (rules_go) — single + multi-package (specific targets)
- [x] rules_rust (standalone) — build + run + test
- [x] protobuf (rules_proto/multi-lang) — partial, android-deferred

## Next candidate ecosystems (likely to surface fresh fixable bugs)
- [ ] frontend (rules_js / aspect_rules_js — JS/TS) — bazel-examples/frontend
- [ ] rules_scala / rules_kotlin (standalone)
- [ ] rules_proto standalone (proto_library + a language proto)
- [ ] grpc / envoy (large C++ — likely re-hits deferred cc bugs)

## Deferred deep fixes (documented in FINDINGS) — revisit if high-value
- [ ] F12 go `//...` shared-action conflict (config-encoded output paths)
- [ ] F10 `linkstatic=0` link drops deps (cc dynamic linking)
- [ ] F16 rules_oci `layer_mtree` (container-image path)
- [ ] F5 bare native cc rules (autoload to rules_cc)
- [ ] F9 android `config_feature_flag`
- [ ] F17 `local_path_override` outside project root (path model)

## Bugs
_(tracked in FINDINGS.md — F1–F17)_
