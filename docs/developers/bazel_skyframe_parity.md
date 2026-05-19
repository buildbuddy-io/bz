# Bazel Skyframe parity

This file tracks Buck2's Bazel-aligned DICE surface against Bazel's
`SkyFunctions` registry.

Bazel source references:

- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/SkyFunctions.java`
- `~/Code/bazel/src/main/java/com/google/devtools/build/lib/skyframe/SkyframeExecutor.java`

## Loading and package evaluation

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `FILE` | `ReadFileKey` / `PathMetadataKey` |
| `DIRECTORY_LISTING` | `ReadDirKey` |
| `DIRECTORY_LISTING_STATE` | `ReadDirForNoWatchFsKey` |
| `DIRECTORY_TREE_DIGEST` | Directory artifact value keys |
| `PACKAGE_LOOKUP` | `PackageListingKey` / package file lookup |
| `CONTAINING_PACKAGE_LOOKUP` | Package boundary and package listing keys |
| `GLOB` | `BazelPackageDataKey` |
| `GLOBS` | `BazelPackageDataKey` batched by package evaluation |
| `BZL_LOAD` | `BazelBzlLoadKey` |
| `BZL_COMPILE` | Inlined in `BazelBzlLoadKey` and interpreter prepare/eval cache |
| `STARLARK_BUILTINS` | Global interpreter state (`GisKey`) |
| `PACKAGE` | `BazelPackageKey` |
| `PACKAGE_DECLARATIONS` | `BazelPackageDeclarationsKey` |
| `NON_FINALIZER_PACKAGE_PIECES` | `BazelNonFinalizerPackagePiecesKey` |
| `PACKAGE_ERROR_MESSAGE` | `BazelPackageErrorMessageKey` |
| `PACKAGE_ERROR` | Represented by `BazelPackageKey` error values |
| `MACRO_INSTANCE` | Not separately lazy in Buck2; folded into package evaluation |
| `EVAL_MACRO` | Not separately lazy in Buck2; folded into package evaluation |

## Target pattern graph

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `TARGET_PATTERN_PHASE` | `TargetPatternPhaseKey` |
| `TARGET_PATTERN` | `TargetPatternKey` |
| `TARGET_PATTERN_ERROR` | `TargetPatternKey` error values |
| `PREPARE_DEPS_OF_PATTERNS` | `PrepareDepsOfPatternsKey` |
| `PREPARE_DEPS_OF_PATTERN` | `PrepareDepsOfPatternKey` |
| `PREPARE_DEPS_OF_TARGETS_UNDER_DIRECTORY` | `CollectPackagesUnderDirectoryKey` |
| `COLLECT_PACKAGES_UNDER_DIRECTORY` | `CollectPackagesUnderDirectoryKey` |
| `COLLECT_TARGETS_IN_PACKAGE` | `TargetPatternPhaseKey` package application step |
| `IGNORED_SUBDIRECTORIES` | `DiceFileOps::is_ignored` during package collection |
| `RECURSIVE_PKG` | `CollectPackagesUnderDirectoryKey` |
| `TESTS_IN_SUITE` | Buck2 test command expansion |
| `TEST_SUITE_EXPANSION` | Buck2 test command expansion |

## Configuration, platform, and toolchains

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `BUILD_CONFIGURATION` | `ConfigurationNodeKey` / `BazelPlatformKey` / configuration constructor DICE keys |
| `BUILD_CONFIGURATION_KEY` | Configured target labels and `ConfigurationData` keys |
| `PARSED_FLAGS` | Legacy config DICE keys and Bazel command line build setting application |
| `BASELINE_OPTIONS` | Legacy config DICE keys |
| `FLAG_SET` | Legacy config DICE keys |
| `BUILD_OPTIONS_SCOPE` | Target-platform and modifier resolution |
| `STARLARK_BUILD_SETTINGS_DETAILS` | Configuration rule analysis results |
| `PLATFORM` | `BazelPlatformKey` |
| `PLATFORM_MAPPING` | Bazel command line build setting application |
| `REGISTERED_EXECUTION_PLATFORMS` | `ExecutionPlatformsKey` |
| `REGISTERED_TOOLCHAINS` | `RegisteredBazelToolchainNodesKey` |
| `SINGLE_TOOLCHAIN_RESOLUTION` | `ToolchainExecutionPlatformCompatibilityKey` |
| `TOOLCHAIN_RESOLUTION` | `ExecutionPlatformResolutionKey` |

## Analysis and execution

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `CONFIGURED_TARGET` | `ConfiguredTargetNodeKey` and `AnalysisKey` |
| `TRANSITIVE_TARGET` | Configured graph traversal from `ConfiguredTargetNodeKey` |
| `TRANSITIVE_TRAVERSAL` | Configured graph traversal from `ConfiguredTargetNodeKey` |
| `ASPECT` | Not applicable until Buck2 exposes Bazel aspect analysis |
| `TOP_LEVEL_ASPECTS` | Not applicable until Buck2 exposes Bazel aspect analysis |
| `LOAD_ASPECTS` | Not applicable until Buck2 exposes Bazel aspect analysis |
| `ACTION_LOOKUP_CONFLICT_FINDING` | Action registry conflict checks during analysis |
| `TOP_LEVEL_ACTION_LOOKUP_CONFLICT_FINDING` | Action registry conflict checks during analysis |
| `ACTION_EXECUTION` | `BuildKey` |
| `ARTIFACT` | `EnsureArtifactGroupValuesKey` / `DirArtifactValueKey` |
| `ARTIFACT_NESTED_SET` | `EnsureArtifactGroupValuesKey` / `EnsureTransitiveSetProjectionKey` |
| `ACTION_TEMPLATE_EXPANSION` | Dynamic action analysis/execution keys |
| `RECURSIVE_FILESYSTEM_TRAVERSAL` | Directory artifact value keys |
| `TARGET_COMPLETION` | Top-level output collection plus `BuildKey` execution |
| `ASPECT_COMPLETION` | Not applicable until Buck2 exposes Bazel aspect analysis |
| `TEST_COMPLETION` | `TestExecutionKey` |
| `BUILD_INFO` | Workspace/status action support |
| `COVERAGE_REPORT` | Buck2 coverage/test reporting |
| `BUILD_DRIVER` | Command-level build orchestration over `BuildKey` |

## Inputs and project files

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `PRECOMPUTED` | Injected DICE data |
| `CLIENT_ENVIRONMENT_VARIABLE` | Injected/client DICE data and bzlmod env keys |
| `ACTION_ENVIRONMENT_VARIABLE` | Action execution environment in executor config |
| `PROJECT` | Not applicable to Buck2 project metadata today |
| `PROJECT_FILES_LOOKUP` | Not applicable to Buck2 project metadata today |

