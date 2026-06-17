# rules_rust

- **Source:** rules_rust 0.70.0 (BCR). Tested via a standalone minimal project at
  `~/work/rust-standalone` (rust_library + rust_binary + rust_test).
- **Ecosystem:** rules_rust (Rust). Note: bz itself is built with rules_rust.

## Results — ✅ FULLY WORKS

| Target | Result |
| --- | --- |
| `rust_library` (greeter) | ✅ builds |
| `rust_binary` (hello, deps=[:greeter]) | ✅ builds + runs → `Hello from rules_rust via bz!` |
| `rust_test` (greeter_test) | ✅ builds + **passes** (`1 passed; 0 failed`) |

`bz build //...` + `bz run` + `bz test` all succeed. The rust toolchain is fetched
via the rules_rust module extension; library→binary deps and unit tests work
end-to-end. Strong validation of a new ecosystem.

## Bug surfaced (setup-specific)

- **F17** — the rules_rust *in-tree* example `hello_world_no_cargo` uses
  `local_path_override(path="../..")` pointing outside the project root, which bz's
  project-rooted path model rejects (`expected a normalized path ... '../..'`).
  Documented/deferred (deep path-model issue). Standalone projects (registry deps)
  are unaffected — hence this standalone test.
