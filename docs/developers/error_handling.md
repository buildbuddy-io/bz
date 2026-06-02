---
id: error_handling
title: Error Handling
---

# Error Handling

Buck2 uses `bz_error` replacing both `anyhow` and `thiserror`.

Use of `anyhow` or `thiserror` in `buck2/app` is banned except where there are pre-existing
exceptions or when extremely strongly justified.

## Result type

```rust
fn my_function() -> bz_error::Result<String> {
    // ...
}
```

## Defining custom error types

Use `#[derive(Debug, bz_error::Error)]`, with an API similar to `thiserror`. Every error
must carry an `ErrorTag`:

```rust
#[derive(Debug, bz_error::Error)]
#[error("My error message: {field}")]
#[buck2(tag = Input)]
struct MyError {
    field: String,
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum MyErrors {
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Missing required field: {0}")]
    MissingField(String),
}
```

## Error tags

Tags are defined in `app/bz_data/error.proto`. Common generic tags:

- `Input` — user input errors (invalid arguments, malformed build files, ...)
- `Tier0` — critical infrastructure failures
- `Environment` — external environment issues (system config, external services,
  network/certs, filesystem)

Using just one of these three tags is fine for most errors. When adding additional tags other than
those, reuse existing tags only when appropriate.

## Ad-hoc errors

```rust
use bz_error::bz_error;

if some_condition {
    return Err(bz_error!(
        bz_error::ErrorTag::Input,
        "Invalid value: expected {}, got {}",
        expected,
        actual
    ));
}
```

## Internal errors (bugs in Buck2 itself)

When reaching a condition that represents an invariant violation and should never fire, panicking
via `.expect()`, `.unwrap()`, etc. is ok for file-local invariants. For non-file-local, prefer a
variant of internal error:

```rust
use bz_error::internal_error;

let value = map.get(key).internal_error("Key must exist")?;

return Err(internal_error!(
    "Unexpected state: {} should not be empty",
    collection_name
));
```

For `Option`, prefer `.internal_error(...)` / `.with_internal_error(|| ...)`
over `.context(...)` or `.expect(...)`.

## Adding context

`bz_error` supports `buck_error_context` APIs akin to anyhow's context:

```rust
use bz_error::BuckErrorContext;

result.buck_error_context("Failed to process file")?;

result.with_buck_error_context(|| format!("Failed to process file: {}", path))?;
```

Be somewhat conservative in the use of context, more is not always better.

## Conversion

`bz_error::Error` impls `From` for many common error types, including many from std and common
dependencies. When none exists, follow existing patterns, add one if semantically appropriate, and
otherwise use an ad-hoc conversion:

```rust
use bz_error::conversion::from_any_with_tag;

some_result.map_err(|e| from_any_with_tag(e, ErrorTag::Tier0))?;
```

## Worked example

```rust
fn process_artifact(&self, artifact: &Artifact) -> bz_error::Result<()> {
    let path = artifact.path()
        .buck_error_context("Failed to get artifact path")?;

    if !path.exists() {
        return Err(bz_error!(
            bz_error::ErrorTag::Input,
            "Artifact does not exist: {}",
            path
        ));
    }

    Ok(())
}
```
