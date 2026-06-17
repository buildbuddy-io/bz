# rules_kotlin (standalone)

- **Source:** rules_kotlin 1.9.6 (BCR). Standalone project at `~/work/kotlin-standalone`
  (kt_jvm_library + kt_jvm_binary). Build with the embedded JDK on PATH +
  `--java_runtime_version=remotejdk_21`.
- **Ecosystem:** rules_kotlin (JVM/Kotlin) — a fresh ecosystem.

## Results — kt_jvm_library ✅ compiles; kt_jvm_binary ⏸ (F29=F21)

Surfaced **five** distinct compat issues, each peeled off in turn:

| Bug | Issue | Status |
| --- | --- | --- |
| F25 | bundled `bazel_tools` missing `tools/java` (java_stub_template) | ✅ fixed |
| F26 | `ctx.actions.run` rejected `input_manifests` | ✅ fixed |
| F27 | `FilesToRunProvider` (no executable) in `actions.run` tools | ✅ fixed |
| F28 | `java_common_internal.check_java_toolchain_is_declared_on_rule` | ✅ fixed |
| F29 | `kt_jvm_binary` writes `ctx.outputs.executable` (= F21) | ⏸ deferred |

**4 fixes landed for Kotlin** (F25–F28). kt_jvm_library now compiles end-to-end. The
binary is blocked only on `ctx.outputs.executable` (F29 = the deferred F21), which
needs a lazy predeclared-output value. This is the single hardest remaining fix.
