# check_mutually_exclusive_dependencies_test

This test is particularly useful for enforcing rules like:

1. **Preventing mixing of library variants**: Ensure targets use only one of: `//third-party/volk:volk`, `//third-party/volk:volk-header`, or `//third-party/toolchains:vulkan`
2. **Any scenario** where multiple dependencies should never coexist in the same transitive dependency tree

## How to Use

### Basic Example

Add this to your BUCK file:

```python
load(
    "@root//tools/build_defs:check_dependencies_test.bzl",
    "check_mutually_exclusive_dependencies_test",
)

check_mutually_exclusive_dependencies_test(
    name = "no_conflicting_volk_deps",
    target = "root//your/target:name",
    contacts = ["your-oncall@example.com"],
    mutually_exclusive_group = [
        # Only one of these should be present in the dependency tree
        "third_party//volk:volk",
        "third_party//volk:volk-header",
        "third_party//toolchains:vulkan",
    ],
)
```

### Using Regex Patterns

Each pattern in the group can be a specific target or a regex pattern:

```python
check_mutually_exclusive_dependencies_test(
    name = "no_mixed_dependencies",
    target = "root//your/target:name",
    contacts = ["your-oncall@example.com"],
    mutually_exclusive_group = [
        # Match specific targets
        "third_party//lib-v1:specific_target",
        # Use regex to match multiple targets
        "third_party//lib-v2:.*",
        # Another regex pattern
        "third_party//lib-v3/.*",
    ],
)
```

## Parameters

- **name** (required): Name of the test target
- **target** (required): The target whose dependencies should be checked
- **contacts** (required): List of contacts responsible for the test
- **mutually_exclusive_group** (required): List of dependency patterns where only one should be present. Each pattern can be a specific target (e.g., "//foo/bar:baz") or a regex pattern (e.g., "//foo/.*")
- **labels** (optional): Additional labels for the test (default: [])
- **target_deps** (optional): If True, only check target_deps() (default: True)
- **expect_failure_msg** (optional): Regex pattern for expected failure message (for testing)
- **deps** (optional): Additional dependencies for the test

## CI Integration

Like other `check_dependencies_test` variants, this test can be configured to run in CI using the `labels` parameter:

```python
load("@root//tools/target_determinator/macros:ci.bzl", "ci")

check_mutually_exclusive_dependencies_test(
    name = "no_conflicting_deps",
    target = "root//your/target:name",
    contacts = [read_oncall()],
    mutually_exclusive_group = [
        "third_party//volk:volk",
        "third_party//volk:volk-header",
        "third_party//toolchains:vulkan",
    ],
    labels = ci.labels(
        [
            ci.mode("root//arvr/mode/fb-linux-nh/cuda12_5/opt"),
        ],
        overwrite = True,
    ),
)
```
