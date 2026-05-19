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
| `FILE_STATE` | `PathMetadataForNoWatchFsKey` |
| `FILE_SYMLINK_CYCLE_UNIQUENESS` | `BazelSkyframeMarkerKey::FileSymlinkCycleUniqueness` plus symlink cycle detection in `resolve_read_file_metadata` |
| `FILE_SYMLINK_INFINITE_EXPANSION_UNIQUENESS` | `BazelSkyframeMarkerKey::FileSymlinkInfiniteExpansionUniqueness` plus symlink expansion detection in file-op metadata resolution |
| `DIRECTORY_LISTING` | `ReadDirKey` |
| `DIRECTORY_LISTING_STATE` | `ReadDirForNoWatchFsKey` |
| `DIRECTORY_TREE_DIGEST` | `BazelSkyframeMarkerKey::DirectoryTreeDigest` plus directory artifact value keys |
| `PACKAGE_LOOKUP` | `PackageListingKey` / package file lookup |
| `CONTAINING_PACKAGE_LOOKUP` | `BazelSkyframeMarkerKey::ContainingPackageLookup` plus package boundary and package listing keys |
| `GLOB` | `BazelPackageDataKey` for glob requests |
| `GLOBS` | `BazelPackageDataKey` for subpackage/package-data requests |
| `BZL_COMPILE` | `BazelBzlCompileKey` |
| `STARLARK_BUILTINS` | `BazelStarlarkBuiltinsKey` |
| `BZL_LOAD` | `BazelBzlLoadKey` |
| `PACKAGE` | `BazelPackageKey` |
| `PACKAGE_DECLARATIONS` | `BazelPackageDeclarationsKey` |
| `NON_FINALIZER_PACKAGE_PIECES` | `BazelNonFinalizerPackagePiecesKey` |
| `PACKAGE_ERROR` | `BazelPackageErrorKey` |
| `PACKAGE_ERROR_MESSAGE` | `BazelPackageErrorMessageKey` |
| `MACRO_INSTANCE` | `BazelSkyframeMarkerKey::MacroInstance` plus package macro expansion folded into `BazelNonFinalizerPackagePiecesKey` |
| `EVAL_MACRO` | `BazelSkyframeMarkerKey::EvalMacro` plus package macro expansion folded into `BazelPackageDeclarationsKey` |

## Target pattern graph

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `TARGET_PATTERN_PHASE` | `TargetPatternPhaseKey` |
| `TARGET_PATTERN` | `TargetPatternKey` |
| `TARGET_PATTERN_ERROR` | `TargetPatternErrorKey` |
| `PREPARE_DEPS_OF_PATTERNS` | `PrepareDepsOfPatternsKey` |
| `PREPARE_DEPS_OF_PATTERN` | `PrepareDepsOfPatternKey` |
| `PREPARE_DEPS_OF_TARGETS_UNDER_DIRECTORY` | `PrepareDepsOfTargetsUnderDirectoryKey` |
| `COLLECT_TARGETS_IN_PACKAGE` | `CollectTargetsInPackageKey` |
| `COLLECT_PACKAGES_UNDER_DIRECTORY` | `CollectPackagesUnderDirectoryKey` |
| `IGNORED_SUBDIRECTORIES` | `IgnoredSubdirectoriesKey` |
| `RECURSIVE_PKG` | `RecursivePkgKey` |
| `TEST_SUITE_EXPANSION` | `BazelSkyframeMarkerKey::TestSuiteExpansion` plus Buck2 test-suite expansion in the test command path |
| `TESTS_IN_SUITE` | `BazelSkyframeMarkerKey::TestsInSuite` plus Buck2 test-suite expansion in the test command path |
| `PREPARE_ANALYSIS_PHASE` | `BazelSkyframeMarkerKey::PrepareAnalysisPhase` plus target-pattern phase and configured graph construction |

## Configuration, platform, and toolchains

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `BUILD_CONFIGURATION` | `ConfigurationNodeKey` |
| `BUILD_CONFIGURATION_KEY` | `MatchedConfigurationSettingKeysKey` / configured label construction |
| `PARSED_FLAGS` | `BazelSkyframeMarkerKey::ParsedFlags` plus legacy config DICE keys and Bazel command-line build setting application |
| `BASELINE_OPTIONS` | `BazelSkyframeMarkerKey::BaselineOptions` plus legacy config DICE keys |
| `FLAG_SET` | `BazelSkyframeMarkerKey::FlagSet` plus legacy config DICE keys and modifier/transition application |
| `BUILD_OPTIONS_SCOPE` | `BazelSkyframeMarkerKey::BuildOptionsScope` plus target-platform and modifier resolution |
| `STARLARK_BUILD_SETTINGS_DETAILS` | `BazelSkyframeMarkerKey::StarlarkBuildSettingsDetails` plus configuration rule analysis results |
| `PLATFORM` | `BazelPlatformKey` |
| `PLATFORM_MAPPING` | `BazelSkyframeMarkerKey::PlatformMapping` plus Bazel command-line build setting application |
| `REGISTERED_EXECUTION_PLATFORMS` | `ExecutionPlatformsKey` |
| `REGISTERED_TOOLCHAINS` | `RegisteredBazelToolchainNodesKey` |
| `SINGLE_TOOLCHAIN_RESOLUTION` | `ToolchainExecutionPlatformCompatibilityKey` |
| `TOOLCHAIN_RESOLUTION` | `ExecutionPlatformResolutionKey` |

## Analysis and execution

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `CONFIGURED_TARGET` | `ConfiguredTargetNodeKey` and `AnalysisKey` |
| `TRANSITIVE_TARGET` | `BazelSkyframeMarkerKey::TransitiveTarget` plus configured graph traversal from `ConfiguredTargetNodeKey` |
| `TRANSITIVE_TRAVERSAL` | `BazelSkyframeMarkerKey::TransitiveTraversal` plus configured graph traversal from `ConfiguredTargetNodeKey` |
| `ASPECT` | `BazelSkyframeMarkerKey::Aspect` plus Bazel aspect metadata attached during package evaluation and analyzed through `AnalysisKey` |
| `TOP_LEVEL_ASPECTS` | `BazelSkyframeMarkerKey::TopLevelAspects` plus top-level aspect behavior represented by configured target analysis |
| `LOAD_ASPECTS` | `BazelSkyframeMarkerKey::LoadAspects` plus aspect definitions loaded through `BazelBzlLoadKey` |
| `ACTION_LOOKUP_CONFLICT_DETECTION` | `BazelSkyframeMarkerKey::ActionLookupConflictDetection` plus action registry conflict checks during analysis |
| `TOP_LEVEL_ACTION_LOOKUP_CONFLICT_DETECTION` | `BazelSkyframeMarkerKey::TopLevelActionLookupConflictDetection` plus action registry conflict checks during analysis |
| `ACTION_EXECUTION` | `BuildKey` |
| `ARTIFACT` | `EnsureArtifactGroupValuesKey` / `DirArtifactValueKey` |
| `ARTIFACT_NESTED_SET` | `EnsureTransitiveSetProjectionKey` |
| `ACTION_TEMPLATE_EXPANSION` | `BazelSkyframeMarkerKey::ActionTemplateExpansion` plus dynamic action analysis/execution keys |
| `RECURSIVE_FILESYSTEM_TRAVERSAL` | `DirArtifactValueKey` |
| `FILESET_ENTRY` | `BazelSkyframeMarkerKey::FilesetEntry` plus artifact/directory traversal keys |
| `TARGET_COMPLETION` | `TopLevelTargetOutputsKey` plus `BuildKey` execution |
| `ASPECT_COMPLETION` | `BazelSkyframeMarkerKey::AspectCompletion` plus configured target completion |
| `TEST_COMPLETION` | `TestExecutionKey` |
| `BUILD_INFO` | `BazelSkyframeMarkerKey::BuildInfo` plus workspace/status action support |
| `COVERAGE_REPORT` | `BazelSkyframeMarkerKey::CoverageReport` plus Buck2 coverage/test reporting |
| `BUILD_DRIVER` | `BazelSkyframeMarkerKey::BuildDriver` plus command-level build orchestration over `BuildKey` |
| `GENQUERY_SCOPE` | `BazelSkyframeMarkerKey::GenqueryScope` plus query scope evaluation through Buck2 query environments |
| `INCLUDE_HINTS` | `BazelSkyframeMarkerKey::IncludeHints` plus C/C++ include discovery represented by dep-file and action input tracking |

## Repository and bzlmod

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `REPOSITORY_ENVIRONMENT_VARIABLE` | `BazelSkyframeMarkerKey::RepositoryEnvironmentVariable` plus repository evaluation environment reads |
| `REPOSITORY_DIRECTORY` | `BzlmodRepositoryDirectoryKey`, `BzlmodGeneratedCellMaterializationKey`, and bzlmod file-op delegate keys |
| `LOCAL_REPOSITORY_LOOKUP` | `BazelSkyframeMarkerKey::LocalRepositoryLookup` plus local repository materialization in bzlmod file-op delegate keys |
| `REPOSITORY_MAPPING` | `CellAliasResolverKey` and bzlmod repo mapping keys |
| `MODULE_FILE` | `BzlmodRootModuleKey` / `BzlmodModuleFileKey` |
| `REPO_PACKAGE_ARGS` | `BazelSkyframeMarkerKey::RepositoryPackageArgs` plus repository/package args in bzlmod module resolution |
| `REPO_FILE` | `BazelSkyframeMarkerKey::RepositoryFile` plus repository file parsing during ignored-directory and repo metadata resolution |
| `BAZEL_MOD_TIDY` | `BzlmodModTidyKey` |
| `BAZEL_MODULE_RESOLUTION` | `BzlmodModuleResolutionKey` / `BzlmodResolutionKey` |
| `BAZEL_MODULE_INSPECTION` | `BzlmodModuleInspectionKey` |
| `SINGLE_EXTENSION_USAGES` | `BzlmodSingleExtensionUsagesKey` |
| `SINGLE_EXTENSION` | `BzlmodSingleExtensionKey` |
| `SINGLE_EXTENSION_EVAL` | `BzlmodSingleExtensionEvalKey` |
| `BAZEL_DEP_GRAPH` | `BzlmodDepGraphKey` |
| `BAZEL_LOCK_FILE` | `BzlmodLockFileKey` / `BzlmodHiddenLockFileKey` |
| `BAZEL_FETCH_ALL` | `BzlmodFetchAllKey` |
| `REGISTRY` | `BzlmodRegistryKey` |
| `REPO_SPEC` | `BzlmodRepoSpecKey` / `BzlmodRepoDefinitionKey` |
| `REPO_DEFINITION` | `BzlmodRepoDefinitionKey` |
| `YANKED_VERSIONS` | `BzlmodYankedVersionsKey` |
| `MODULE_EXTENSION_REPO_MAPPING_ENTRIES` | `BzlmodModuleExtensionRepoMappingEntriesKey` |
| `VENDOR_FILE` | `BzlmodVendorFileKey` |

## Inputs and project files

| Bazel SkyFunction | Buck2 DICE surface |
| --- | --- |
| `PRECOMPUTED` | `BazelSkyframeMarkerKey::Precomputed` plus injected DICE data |
| `CLIENT_ENVIRONMENT_VARIABLE` | Injected/client DICE data and bzlmod env keys |
| `ACTION_ENVIRONMENT_VARIABLE` | `BazelSkyframeMarkerKey::ActionEnvironmentVariable` plus action execution environment in executor config |
| `PROJECT` | `BazelSkyframeMarkerKey::Project` plus Buck2 project-level config represented by legacy config DICE data |
| `PROJECT_FILES_LOOKUP` | `BazelSkyframeMarkerKey::ProjectFilesLookup` plus Buck2 project-level config lookup represented by legacy config DICE data |
