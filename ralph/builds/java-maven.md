# bazel-examples/java-maven

- **Source:** bazel-examples/java-maven
- **Local path:** `~/work/bazel-examples/java-maven`
- **Ecosystem:** rules_jvm_external (Maven dep resolution via coursier), rules_java,
  rules_oci + aspect_bazel_lib (container image packaging).

## Results

| Target | Result |
| --- | --- |
| Maven resolution (coursier) | ✅ works (needs host `java` on PATH) |
| `//:java-maven-lib`, `//:java-maven`, `//:tests` (Java) | ✅ build |
| `//:image` (oci_image) | ❌ OCI path: F13✅, F15✅ fixed; now F16 (layer_mtree) |

## Status: ✅ core Maven+Java works; ⏸ OCI image packaging blocked (F16)

**rules_jvm_external (Maven) + rules_java work** — the actual app and its Maven
dependencies build and the tests compile. The OCI container-image packaging
(rules_oci/tar) is a deep separate area; surfaced and fixed two gaps here:

- **F13** `ctx.actions.run` rejected `unused_inputs_list` — **fixed**.
- **F15** `repository_ctx.download(block=False)` rejected — **fixed**.
- **F16** rules_oci/tar `layer_mtree` output not found — documented, deferred.
- (**F14** tightened the F3 coercion as a safety refinement while investigating.)

## Env note

- coursier (Maven resolver) runs during repo fetch and needs a host JDK.
  No system JDK on this VM → used bazel's embedded JDK:
  `JAVA_HOME=~/.cache/bazel/.../embedded_tools/jdk PATH=$JAVA_HOME/bin:$PATH`.
- Build java targets with `--java_runtime_version=remotejdk_21`.
