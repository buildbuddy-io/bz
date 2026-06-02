//! Bazel Skyframe compatibility DICE keys.
//!
//! Some Bazel SkyFunctions are true first-class computations in Buck2. Others
//! are intentionally folded into broader Buck2 computations while the external
//! behavior is still Bazel-shaped. `BazelSkyframeMarkerKey` gives those folded
//! surfaces an explicit DICE node and a Bazel-aligned display name without
//! changing the owning computation's value.

use std::fmt;

use allocative::Allocative;
use async_trait::async_trait;
use dice::DiceComputations;
use dice::Key;
use dice::NoValueSerialize;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use pagable::Pagable;
use pagable::pagable_typetag;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
pub enum BazelSkyframeFunction {
    ActionEnvironmentVariable,
    ActionLookupConflictDetection,
    ActionTemplateExpansion,
    Aspect,
    AspectCompletion,
    BaselineOptions,
    BuildDriver,
    BuildInfo,
    BuildOptionsScope,
    ContainingPackageLookup,
    CoverageReport,
    DirectoryTreeDigest,
    EvalMacro,
    FilesetEntry,
    FileSymlinkCycleUniqueness,
    FileSymlinkInfiniteExpansionUniqueness,
    FlagSet,
    GenqueryScope,
    IncludeHints,
    LoadAspects,
    LocalRepositoryLookup,
    MacroInstance,
    ParsedFlags,
    PlatformMapping,
    Precomputed,
    PrepareAnalysisPhase,
    Project,
    ProjectFilesLookup,
    RepositoryEnvironmentVariable,
    RepositoryFile,
    RepositoryPackageArgs,
    StarlarkBuildSettingsDetails,
    TestsInSuite,
    TestSuiteExpansion,
    TopLevelActionLookupConflictDetection,
    TopLevelAspects,
    TransitiveTarget,
    TransitiveTraversal,
}

impl BazelSkyframeFunction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ActionEnvironmentVariable => "ACTION_ENVIRONMENT_VARIABLE",
            Self::ActionLookupConflictDetection => "ACTION_LOOKUP_CONFLICT_DETECTION",
            Self::ActionTemplateExpansion => "ACTION_TEMPLATE_EXPANSION",
            Self::Aspect => "ASPECT",
            Self::AspectCompletion => "ASPECT_COMPLETION",
            Self::BaselineOptions => "BASELINE_OPTIONS",
            Self::BuildDriver => "BUILD_DRIVER",
            Self::BuildInfo => "BUILD_INFO",
            Self::BuildOptionsScope => "BUILD_OPTIONS_SCOPE",
            Self::ContainingPackageLookup => "CONTAINING_PACKAGE_LOOKUP",
            Self::CoverageReport => "COVERAGE_REPORT",
            Self::DirectoryTreeDigest => "DIRECTORY_TREE_DIGEST",
            Self::EvalMacro => "EVAL_MACRO",
            Self::FilesetEntry => "FILESET_ENTRY",
            Self::FileSymlinkCycleUniqueness => "FILE_SYMLINK_CYCLE_UNIQUENESS",
            Self::FileSymlinkInfiniteExpansionUniqueness => {
                "FILE_SYMLINK_INFINITE_EXPANSION_UNIQUENESS"
            }
            Self::FlagSet => "FLAG_SET",
            Self::GenqueryScope => "GENQUERY_SCOPE",
            Self::IncludeHints => "INCLUDE_HINTS",
            Self::LoadAspects => "LOAD_ASPECTS",
            Self::LocalRepositoryLookup => "LOCAL_REPOSITORY_LOOKUP",
            Self::MacroInstance => "MACRO_INSTANCE",
            Self::ParsedFlags => "PARSED_FLAGS",
            Self::PlatformMapping => "PLATFORM_MAPPING",
            Self::Precomputed => "PRECOMPUTED",
            Self::PrepareAnalysisPhase => "PREPARE_ANALYSIS_PHASE",
            Self::Project => "PROJECT",
            Self::ProjectFilesLookup => "PROJECT_FILES_LOOKUP",
            Self::RepositoryEnvironmentVariable => "REPOSITORY_ENVIRONMENT_VARIABLE",
            Self::RepositoryFile => "REPO_FILE",
            Self::RepositoryPackageArgs => "REPO_PACKAGE_ARGS",
            Self::StarlarkBuildSettingsDetails => "STARLARK_BUILD_SETTINGS_DETAILS",
            Self::TestsInSuite => "TESTS_IN_SUITE",
            Self::TestSuiteExpansion => "TEST_SUITE_EXPANSION",
            Self::TopLevelActionLookupConflictDetection => {
                "TOP_LEVEL_ACTION_LOOKUP_CONFLICT_DETECTION"
            }
            Self::TopLevelAspects => "TOP_LEVEL_ASPECTS",
            Self::TransitiveTarget => "TRANSITIVE_TARGET",
            Self::TransitiveTraversal => "TRANSITIVE_TRAVERSAL",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct BazelSkyframeMarkerKey {
    function: BazelSkyframeFunction,
    detail: Option<String>,
}

impl BazelSkyframeMarkerKey {
    pub fn new(function: BazelSkyframeFunction) -> Self {
        Self {
            function,
            detail: None,
        }
    }

    pub fn with_detail(function: BazelSkyframeFunction, detail: impl Into<String>) -> Self {
        Self {
            function,
            detail: Some(detail.into()),
        }
    }
}

impl fmt::Display for BazelSkyframeMarkerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{}({})", self.function.as_str(), detail),
            None => f.write_str(self.function.as_str()),
        }
    }
}

#[async_trait]
impl Key for BazelSkyframeMarkerKey {
    type Value = ();

    async fn compute(
        &self,
        _ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
    }

    fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
        true
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

pub async fn mark_bazel_skyframe_key(
    ctx: &mut DiceComputations<'_>,
    function: BazelSkyframeFunction,
) -> bz_error::Result<()> {
    ctx.compute(&BazelSkyframeMarkerKey::new(function)).await?;
    Ok(())
}

pub async fn mark_bazel_skyframe_key_with_detail(
    ctx: &mut DiceComputations<'_>,
    function: BazelSkyframeFunction,
    detail: impl Into<String>,
) -> bz_error::Result<()> {
    ctx.compute(&BazelSkyframeMarkerKey::with_detail(function, detail))
        .await?;
    Ok(())
}
