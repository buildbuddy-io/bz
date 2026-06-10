use std::cell::Cell;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub mod archive;
pub mod integrity;
pub mod patch;

mod cell_aliases;
mod module_file;

mod lockfile;
mod validation;

pub(crate) use self::cell_aliases::BazelModuleCellAliases;
pub(crate) use self::cell_aliases::BzlmodOverlayConfig;
pub(crate) use self::cell_aliases::BzlmodPatchConfig;
pub(crate) use self::cell_aliases::bzlmod_external_module_is_configure_repo;
pub(crate) use self::cell_aliases::bzlmod_external_module_is_local;
pub(crate) use self::cell_aliases::dedup_preserve_order;
pub(crate) use self::cell_aliases::parse_bzlmod_external_cell_origin;
use self::lockfile::BzlmodModuleLockfileData;
use self::lockfile::bzlmod_hidden_lockfile_schema_matches;
use self::lockfile::bzlmod_lockfile_data;
use self::lockfile::bzlmod_lockfile_data_from_str;
use self::lockfile::bzlmod_lockfile_extension_key;
use self::lockfile::bzlmod_vendor_file_data;
use self::lockfile::empty_bzlmod_lockfile_data;

use self::module_file::BzlmodCompiledModuleFile;
use self::module_file::BzlmodEvaluatedModuleFile;
use self::module_file::BzlmodModuleEvalOptions;
use self::module_file::bzlmod_include_label_to_path;
use self::module_file::compile_bzlmod_module_file;
use self::module_file::eval_bzlmod_module_file;
use self::validation::bzlmod_version_cmp;
use self::validation::is_valid_bzlmod_module_name;
use self::validation::parse_bzlmod_version;

use allocative::Allocative;
use bz_core::cells::cell_root_path::CellRootPath;
use bz_core::cells::external::BZLMOD_BAZEL_COMPAT_VERSION;
use bz_core::cells::external::BZLMOD_EXTERNAL_CELL_KIND;
use bz_core::cells::external::BZLMOD_GENERATED_EXTERNAL_CELL_KIND;
use bz_core::cells::external::BzlmodBazelFeaturesGlobalsSetup;
use bz_core::cells::external::BzlmodBazelFeaturesVersionSetup;
use bz_core::cells::external::BzlmodCellSetup;
use bz_core::cells::external::BzlmodGeneratedCellGenerator;
use bz_core::cells::external::BzlmodGeneratedCellSetup;
use bz_core::cells::external::BzlmodHostPlatformSetup;
use bz_core::cells::external::BzlmodModuleExtensionRepoSetup;
use bz_core::cells::external::BzlmodOverlay;
use bz_core::cells::external::BzlmodPatch;
use bz_core::cells::external::BzlmodRepositoryRuleInvocationSetup;
use bz_core::cells::external::BzlmodShellConfigSetup;
use bz_core::cells::external::BzlmodXcodeConfigSetup;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::external::bzlmod_cell_name;
use bz_core::cells::external::register_bzlmod_cell_aliases_from_refs;
use bz_core::cells::external::register_bzlmod_cell_canonical_repo_name_for_cell;
use bz_core::cells::external::register_bzlmod_module_extension_usages_json;
use bz_core::cells::external::register_external_cell_origin;
use bz_core::cells::name::CellName;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_error::BuckErrorContext;
use bz_error::bz_error;
use bz_error::conversion::from_any_with_tag;
use bz_fs::paths::RelativePath;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use bz_hash::StdBuckHashMap;
use bz_http::HttpClient;
use bz_http::HttpClientBuilder;
use bz_http::retries::HttpError as RetryingHttpError;
use bz_http::retries::HttpErrorForRetry;
use bz_http::retries::IntoBuck2Error;
use bz_http::retries::http_retry;
use bz_util::late_binding::LateBinding;
use derive_more::Display;
use dice::DiceComputations;
use dice::DiceTransactionUpdater;
use dice::InjectedKey;
use dice::Key;
use dice::NoValueSerialize;
use dice::UserComputationData;
use dice::ValueSerialize;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::FutureExt;
use futures::StreamExt;
use pagable::Pagable;
use pagable::pagable_typetag;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use starlark::any::ProvidesStaticType;
use starlark::environment::Globals;
use starlark::environment::GlobalsBuilder;
use starlark::environment::LibraryExtension;
use starlark::environment::Module;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::syntax::AstModule;
use starlark::syntax::Dialect;
use starlark::syntax::DialectTypes;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::list::ListRef;
use starlark::values::none::NoneOr;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::tuple::TupleRef;
use starlark::values::tuple::UnpackTuple;
use starlark_map::small_map::SmallMap;
use starlark_syntax::syntax::ast::ArgumentP;
use starlark_syntax::syntax::ast::AssignTargetP;
use starlark_syntax::syntax::ast::AstAssignTarget;
use starlark_syntax::syntax::ast::AstExpr;
use starlark_syntax::syntax::ast::AstLiteral;
use starlark_syntax::syntax::ast::AstNoPayload;
use starlark_syntax::syntax::ast::AstStmt;
use starlark_syntax::syntax::ast::CallArgsP;
use starlark_syntax::syntax::ast::ClauseP;
use starlark_syntax::syntax::ast::ExprP;
use starlark_syntax::syntax::ast::ForClauseP;
use starlark_syntax::syntax::ast::StmtP;

use crate::bazel::bzlmod::archive::ArchiveKind;
use crate::bazel::bzlmod::archive::archive_kind_from_type_or_url;
use crate::bazel::bzlmod::archive::extract_archive;
use crate::bazel::bzlmod::integrity::parse_bzlmod_integrity;
use crate::dice::cells::HasCellResolver;
use crate::dice::data::HasIoProvider;
use crate::dice::progress::dice_state_update_stage;
use crate::legacy_configs::configs::BazelCompatCellAlias;
use crate::legacy_configs::configs::BazelCompatExternalModule;
use crate::legacy_configs::configs::BazelCompatGeneratedModule;
use crate::legacy_configs::configs::BazelCompatRegistryModule;
use crate::legacy_configs::configs::LegacyBuckConfig;
use crate::legacy_configs::dice::HasLegacyConfigs;
use crate::legacy_configs::file_ops::ConfigParserFileOps;
use crate::legacy_configs::file_ops::ConfigPath;
use crate::legacy_configs::file_ops::DiceConfigFileOps;
use crate::legacy_configs::key::BuckconfigKeyRef;

pub const BZLMOD_ALLOWED_YANKED_VERSIONS_ENV: &str = "BZLMOD_ALLOW_YANKED_VERSIONS";
pub const BZLMOD_REPOSITORY_OS_NAME_ENV: &str = "BUCK2_REPOSITORY_OS_NAME";
pub const BZLMOD_REPOSITORY_OS_ARCH_ENV: &str = "BUCK2_REPOSITORY_OS_ARCH";
pub(crate) const BAZEL_HOST_PLATFORM_CONSTRAINTS: &str = "host_platform_constraints";
pub(crate) const BAZEL_MODULE_FILE: &str = "MODULE.bazel";
const BAZEL_EXTRA_BZLMOD_DEPS: &str = "extra_bzlmod_deps";

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Hash,
    Allocative,
    Pagable
)]
struct BazelDep {
    name: String,
    version: String,
    apparent_name: Option<String>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
struct DiscoveredBcrModule {
    dep: BazelDep,
    source_json: BcrSourceJson,
    module_aliases: Vec<String>,
    use_repo_aliases: Vec<String>,
    extension_usages: Vec<BzlmodExtensionUsage>,
    use_repo_rule_invocations: Vec<BzlmodUseRepoRuleInvocation>,
    constants: Vec<(String, String)>,
    registered_toolchains: Vec<String>,
    deps: Vec<BazelDep>,
}

type DiscoveredBcrModules = BTreeMap<(String, String), DiscoveredBcrModule>;

static BZLMOD_HTTP_CLIENT: LazyLock<tokio::sync::OnceCell<HttpClient>> =
    LazyLock::new(tokio::sync::OnceCell::new);

const BAZEL_TOOLS_MODULE_TOOLS: &str = include_str!("../../../../bazel_tools/MODULE.tools");

#[derive(
    Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Allocative, Pagable
)]
struct RootBzlmodModule {
    name: String,
    version: String,
    repo_name: String,
    canonical_repo_name: String,
    lockfile_extension_generated_repos: BTreeMap<String, BTreeSet<String>>,
    lockfile_extension_facts: BTreeSet<String>,
    constants: Vec<(String, String)>,
    extension_usages: Vec<BzlmodExtensionUsage>,
    use_repo_rule_invocations: Vec<BzlmodUseRepoRuleInvocation>,
    registered_toolchains: Vec<String>,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodUseRepoImport {
    alias: String,
    repo_name: String,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodExtensionUsage {
    proxy_name: String,
    extension_bzl_file: String,
    extension_name: String,
    dev_dependency: bool,
    imports: Vec<BzlmodUseRepoImport>,
    repo_overrides: Vec<BzlmodRepoOverride>,
    tags: Vec<BzlmodExtensionTag>,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodRepoOverride {
    repo_name: String,
    overriding_repo_name: String,
    must_exist: bool,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodExtensionTag {
    tag_name: String,
    bindings: Vec<(String, String)>,
    kwargs: Vec<(String, String)>,
}

#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Allocative,
    Pagable
)]
struct BzlmodUseRepoRuleInvocation {
    rule_bzl_file: String,
    rule_name: String,
    repo_name: String,
    attrs: Vec<(String, String)>,
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Allocative, Pagable
)]
struct BzlmodExtensionId {
    bzl_cell_name: String,
    bzl_path: String,
    extension_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodResolvedExtension {
    id: BzlmodExtensionId,
    unique_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodDepGraph {
    root_module: RootBzlmodModule,
    discovered: DiscoveredBcrModules,
    selected_keys: BTreeSet<(String, String)>,
    selected_keys_in_bfs_order: Vec<(String, String)>,
    selected_keys_in_dependency_order: Vec<(String, String)>,
    canonical_repo_names_by_key: BTreeMap<(String, String), String>,
    canonical_repo_names_by_cell: BTreeMap<String, String>,
    root_aliases_by_key: BTreeMap<(String, String), BTreeSet<String>>,
    cell_aliases_by_cell: BzlmodCellAliasesByCell,
    extension_eval_cell_aliases_by_cell: BzlmodCellAliasesByCell,
    extension_unique_names: BTreeMap<BzlmodExtensionId, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub struct BzlmodModuleExtensionRepoMappingBase {
    pub host_aliases: Vec<(String, String)>,
    pub repo_overrides: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodSingleExtensionUsagesValue {
    extension_id: BzlmodExtensionId,
    unique_name: String,
    extension_usages_json: String,
}

fn bzlmod_single_extension_eval_setup(
    usage: &BzlmodSingleExtensionUsagesValue,
) -> BzlmodModuleExtensionRepoSetup {
    BzlmodModuleExtensionRepoSetup {
        parent_canonical_repo_name: Arc::from(""),
        parent_is_root: true,
        extension_bzl_file: Arc::from(format!(
            "{}//{}",
            usage.extension_id.bzl_cell_name, usage.extension_id.bzl_path
        )),
        extension_bzl_cell: Arc::from(usage.extension_id.bzl_cell_name.as_str()),
        extension_bzl_path: Arc::from(usage.extension_id.bzl_path.as_str()),
        extension_unique_name: Arc::from(usage.unique_name.as_str()),
        extension_name: Arc::from(usage.extension_id.extension_name.as_str()),
        repo_name: Arc::from(""),
        extension_usages_key: register_bzlmod_module_extension_usages_json(
            &usage.extension_usages_json,
        ),
        extension_usages_json: Arc::from(usage.extension_usages_json.as_str()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodInspectionModule {
    name: String,
    version: String,
    canonical_repo_name: String,
    selected: bool,
    deps: Vec<BazelDep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodModuleInspectionValue {
    modules: Vec<BzlmodInspectionModule>,
    modules_index: BTreeMap<String, BTreeSet<String>>,
    extension_to_repo_internal_names: BTreeMap<BzlmodExtensionId, BTreeSet<String>>,
    module_key_to_canonical_repo_name: BTreeMap<(String, String), String>,
    errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodModTidyValue {
    root_extension_usages: Vec<BzlmodSingleExtensionUsagesValue>,
    errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodFetchAllValue {
    repos_to_vendor: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRepoDefinitionValue {
    module: Option<BazelCompatExternalModule>,
    configure: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRepositoryDirectoryValue {
    found: bool,
    exclude_from_vendoring: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodVendorFileValue {
    ignored_repos: Vec<String>,
    pinned_repos: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BcrResolution {
    external_modules: Vec<BazelCompatExternalModule>,
    root_aliases: Vec<BazelCompatCellAlias>,
    cell_aliases: BTreeMap<String, Vec<BazelCompatCellAlias>>,
    registered_toolchains: Vec<String>,
}

type BzlmodCellAliasMap = StdBuckHashMap<String, String>;
type BzlmodCellAliasesByCell = StdBuckHashMap<String, BzlmodCellAliasMap>;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative, Pagable)]
struct BzlmodRootPatch {
    path: String,
    content: Arc<str>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
struct BzlmodModuleExtensionEvaluationConfig {
    root_module_has_non_dev_dependency: bool,
    modules: Vec<BzlmodModuleExtensionModuleConfig>,
    #[serde(default)]
    usages: Vec<BzlmodModuleExtensionUsageConfig>,
    #[serde(default)]
    repo_overrides: Vec<(String, String)>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
struct BzlmodModuleExtensionModuleConfig {
    name: String,
    version: String,
    canonical_repo_name: String,
    is_root: bool,
    extension_bzl_file: String,
    extension_name: String,
    cell_aliases: Vec<(String, String)>,
    constants: Vec<(String, String)>,
    tags: Vec<BzlmodModuleExtensionTagConfig>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
struct BzlmodModuleExtensionTagConfig {
    tag_name: String,
    dev_dependency: bool,
    bindings: Vec<(String, String)>,
    kwargs: Vec<(String, String)>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Allocative,
    Pagable
)]
struct BzlmodModuleExtensionUsageConfig {
    imports: Vec<BzlmodUseRepoImport>,
    repo_overrides: Vec<BzlmodRepoOverride>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRegistryValue {
    url: String,
    registry_file_hashes: BTreeMap<String, Option<String>>,
    selected_yanked_versions: BTreeMap<(String, String), String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRegistryInvalidationValue {
    epoch_hour: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodYankedVersionsValue {
    yanked_versions: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodClientEnvironmentVariableValue {
    value: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRepositoryEnvironmentVariableValue {
    value: Option<String>,
}

// Bazel exposes the full repo environment through repository_ctx.os.environ, but
// that access does not establish a Skyframe dependency. Track only explicitly
// requested repo-env variables as DICE keys.
struct BzlmodRepositoryEnvironmentData {
    vars: Arc<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodAllowedYankedVersionsValue {
    allow_all: bool,
    modules: BTreeSet<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRepoSpecValue {
    source_json: BcrSourceJson,
    registry_file_hashes: BTreeMap<String, Option<String>>,
}

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Hash,
    Allocative,
    Pagable
)]
struct BcrSourceJson {
    url: String,
    urls: Option<Vec<String>>,
    integrity: String,
    strip_prefix: Option<String>,
    archive_type: Option<String>,
    patches: Option<BTreeMap<String, String>>,
    overlay: Option<BTreeMap<String, String>>,
    patch_strip: Option<u32>,
}

fn bcr_source_urls(source_json: &BcrSourceJson) -> Vec<String> {
    source_json
        .urls
        .as_ref()
        .filter(|urls| !urls.is_empty())
        .cloned()
        .unwrap_or_else(|| vec![source_json.url.clone()])
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative, Pagable)]
struct BzlmodArchiveOverride {
    module_name: String,
    urls: Vec<String>,
    integrity: String,
    strip_prefix: Option<String>,
    archive_type: Option<String>,
    patches: Vec<BzlmodRootPatch>,
    patch_strip: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative, Pagable)]
struct BzlmodSingleVersionOverride {
    version: Option<String>,
    patches: Vec<BzlmodRootPatch>,
    patch_strip: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative, Pagable)]
struct BzlmodLocalPathOverride {
    module_name: String,
    path: String,
    module_text: String,
    included_module_texts: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodRootResolutionInput {
    aliases: BazelModuleCellAliases,
    root_deps: Vec<BazelDep>,
    root_module: RootBzlmodModule,
    builtin_bazel_tools_module: DiscoveredBcrModule,
    archive_overrides: BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: BTreeMap<String, BzlmodSingleVersionOverride>,
    local_path_overrides: BTreeMap<String, BzlmodLocalPathOverride>,
}

async fn read_bzlmod_compiled_module_file_set(
    module_root: &ProjectRelativePath,
    root_module_file: &str,
    missing_root_ok: bool,
    file_ops: &mut dyn ConfigParserFileOps,
) -> bz_error::Result<(
    Arc<BzlmodCompiledModuleFile>,
    BTreeMap<String, Arc<BzlmodCompiledModuleFile>>,
)> {
    let root_file = ForwardRelativePath::new(root_module_file)?;
    let root_path = ConfigPath::Project(module_root.join(root_file));
    let root_lines = file_ops.read_file_lines_if_exists(&root_path).await?;
    let root_text = match root_lines {
        Some(lines) => lines.join("\n"),
        None if missing_root_ok => String::new(),
        None => {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "`{}` does not exist",
                root_path
            ));
        }
    };
    let root = Arc::new(compile_bzlmod_module_file(
        root_module_file.to_owned(),
        root_text,
    )?);
    let mut included_modules = BTreeMap::<String, Arc<BzlmodCompiledModuleFile>>::new();
    let mut stack = root
        .includes
        .iter()
        .map(|label| (root.module_file.clone(), label.clone()))
        .collect::<Vec<_>>();
    while let Some((including_module_file, label)) = stack.pop() {
        if included_modules.contains_key(&label) {
            continue;
        }
        let include_file = bzlmod_include_label_to_path(&including_module_file, &label)?;
        let include_path =
            ConfigPath::Project(module_root.join(ForwardRelativePath::new(&include_file)?));
        let Some(lines) = file_ops.read_file_lines_if_exists(&include_path).await? else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "included MODULE.bazel file `{}` does not exist",
                label
            ));
        };
        let compiled = Arc::new(compile_bzlmod_module_file(include_file, lines.join("\n"))?);
        stack.extend(
            compiled
                .includes
                .iter()
                .map(|label| (compiled.module_file.clone(), label.clone())),
        );
        included_modules.insert(label, compiled);
    }
    Ok((root, included_modules))
}

async fn read_bzlmod_root_patch_contents(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
    patches: &mut [BzlmodRootPatch],
) -> bz_error::Result<()> {
    for patch in patches {
        let path = cell_path
            .as_project_relative_path()
            .join(ForwardRelativePath::new(&patch.path)?);
        let config_path = ConfigPath::Project(path);
        let Some(lines) = file_ops.read_file_lines_if_exists(&config_path).await? else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod override patch `{}` does not exist",
                patch.path
            ));
        };
        patch.content = if lines.is_empty() {
            Arc::from("")
        } else {
            Arc::from(format!("{}\n", lines.join("\n")))
        };
    }
    Ok(())
}

async fn read_bazel_module_resolution_inputs(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
    extra_root_deps: Vec<BazelDep>,
) -> bz_error::Result<BzlmodRootResolutionInput> {
    let (compiled_root, included_modules) = dice_state_update_stage(
        "reading MODULE.bazel files",
        read_bzlmod_compiled_module_file_set(
            cell_path.as_project_relative_path(),
            BAZEL_MODULE_FILE,
            true,
            file_ops,
        ),
    )
    .await?;
    let mut evaluated = eval_bzlmod_module_file(
        &compiled_root,
        BzlmodModuleEvalOptions {
            is_root: true,
            allow_include: true,
            ignore_dev_dependency: false,
            default_name: "root".to_owned(),
            default_version: String::new(),
            default_repo_name: "root".to_owned(),
            cell_project_path: Some(cell_path.as_project_relative_path().to_buf()),
            included_modules,
        },
    )?;

    for archive_override in evaluated.archive_overrides.values_mut() {
        read_bzlmod_root_patch_contents(cell_path, file_ops, &mut archive_override.patches).await?;
    }
    for single_version_override in evaluated.single_version_overrides.values_mut() {
        read_bzlmod_root_patch_contents(cell_path, file_ops, &mut single_version_override.patches)
            .await?;
    }
    for local_path_override in evaluated.local_path_overrides.values_mut() {
        let module_root = ProjectRelativePath::new(&local_path_override.path)?;
        let (compiled_local_root, included_local_modules) = read_bzlmod_compiled_module_file_set(
            module_root,
            BAZEL_MODULE_FILE,
            false,
            file_ops,
        )
        .await
        .with_buck_error_context(|| {
            format!(
                "Error reading local_path_override MODULE.bazel files for module `{}` from `{}`",
                local_path_override.module_name, local_path_override.path
            )
        })?;
        local_path_override.module_text = compiled_local_root.module_text.clone();
        local_path_override.included_module_texts = included_local_modules
            .into_iter()
            .map(|(label, compiled)| (label, compiled.module_text.clone()))
            .collect();
    }

    let mut builtin_bazel_tools_module = builtin_bazel_tools_module()?;
    let mut aliases = BazelModuleCellAliases::default();
    for alias in &evaluated.aliases {
        aliases.root_aliases.push(BazelCompatCellAlias {
            alias: alias.clone(),
            cell_name: "root".to_owned(),
        });
    }
    aliases.registered_toolchains = evaluated.registered_toolchains.clone();
    let mut root_deps = evaluated.deps.clone();
    merge_extra_bzlmod_root_deps(&mut root_deps, extra_root_deps);
    apply_bzlmod_dep_overrides(
        &mut root_deps,
        &evaluated.archive_overrides,
        &evaluated.single_version_overrides,
        &evaluated.local_path_overrides,
    );
    apply_bzlmod_dep_overrides(
        &mut builtin_bazel_tools_module.deps,
        &evaluated.archive_overrides,
        &evaluated.single_version_overrides,
        &evaluated.local_path_overrides,
    );

    let root_module = RootBzlmodModule {
        name: evaluated.name,
        version: evaluated.version,
        repo_name: evaluated.repo_name,
        canonical_repo_name: String::new(),
        lockfile_extension_generated_repos: BTreeMap::new(),
        lockfile_extension_facts: BTreeSet::new(),
        constants: Vec::new(),
        extension_usages: evaluated.extension_usages,
        use_repo_rule_invocations: evaluated.use_repo_rule_invocations,
        registered_toolchains: evaluated.registered_toolchains,
    };
    Ok(BzlmodRootResolutionInput {
        aliases,
        root_deps,
        root_module,
        builtin_bazel_tools_module,
        archive_overrides: evaluated.archive_overrides,
        single_version_overrides: evaluated.single_version_overrides,
        local_path_overrides: evaluated.local_path_overrides,
    })
}

pub(crate) async fn bzlmod_resolution_enabled_on_dice(
    ctx: &mut DiceComputations<'_>,
) -> bz_error::Result<bool> {
    let root_cell = ctx.get_cell_resolver().await?.root_cell();
    Ok(ctx
        .parse_legacy_config_property::<bool>(
            root_cell,
            BuckconfigKeyRef {
                section: "bazel",
                property: "compatibility",
            },
        )
        .await?
        .unwrap_or(false))
}

pub(crate) async fn get_bazel_module_resolution_on_dice(
    ctx: &mut DiceComputations<'_>,
) -> bz_error::Result<Arc<BazelModuleCellAliases>> {
    if !bzlmod_resolution_enabled_on_dice(ctx).await? {
        return Ok(Arc::new(BazelModuleCellAliases::default()));
    }
    let aliases = ctx.compute(&BzlmodResolutionKey).await??;
    Ok(aliases)
}

pub async fn get_bazel_module_registered_toolchains_on_dice(
    ctx: &mut DiceComputations<'_>,
) -> bz_error::Result<Vec<String>> {
    Ok(get_bazel_module_resolution_on_dice(ctx)
        .await?
        .registered_toolchains
        .clone())
}

fn parse_extra_bzlmod_root_dep(raw: &str) -> bz_error::Result<Option<BazelDep>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let Some((name, version)) = raw.split_once('@') else {
        return Err(bz_error!(
            bz_error::ErrorTag::Input,
            "Invalid bazel.{} entry `{}`; expected `<module>@<version>`",
            BAZEL_EXTRA_BZLMOD_DEPS,
            raw
        ));
    };
    let name = name.trim();
    let version = version.trim();
    if !is_valid_bzlmod_module_name(name) {
        return Err(bz_error!(
            bz_error::ErrorTag::Input,
            "Invalid module name `{}` in bazel.{}",
            name,
            BAZEL_EXTRA_BZLMOD_DEPS
        ));
    }
    parse_bzlmod_version(version).with_buck_error_context(|| {
        format!(
            "Invalid version `{}` for module `{}` in bazel.{}",
            version, name, BAZEL_EXTRA_BZLMOD_DEPS
        )
    })?;
    Ok(Some(BazelDep {
        name: name.to_owned(),
        version: version.to_owned(),
        apparent_name: Some(name.to_owned()),
    }))
}

async fn extra_bzlmod_root_deps_from_config(
    ctx: &mut DiceComputations<'_>,
    root_cell: CellName,
) -> bz_error::Result<Vec<BazelDep>> {
    let Some(entries) = ctx
        .parse_legacy_config_list_property::<String>(
            root_cell,
            BuckconfigKeyRef {
                section: "bazel",
                property: BAZEL_EXTRA_BZLMOD_DEPS,
            },
        )
        .await?
    else {
        return Ok(Vec::new());
    };

    entries
        .into_iter()
        .filter_map(|entry| parse_extra_bzlmod_root_dep(&entry).transpose())
        .collect()
}

fn merge_extra_bzlmod_root_deps(root_deps: &mut Vec<BazelDep>, extra_root_deps: Vec<BazelDep>) {
    for dep in extra_root_deps {
        let visible_alias = dep.apparent_name.as_deref();
        if root_deps.iter().any(|existing| {
            existing.name == dep.name && existing.apparent_name.as_deref() == visible_alias
        }) {
            continue;
        }
        root_deps.push(dep);
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_LOCK_FILE")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodLockFileKey;

#[async_trait::async_trait]
impl Key for BzlmodLockFileKey {
    type Value = bz_error::Result<Arc<BzlmodModuleLockfileData>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let root_path = resolver.root_cell_instance().path().to_buf();
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let mut file_ops = DiceConfigFileOps::new(ctx, project_fs, &resolver);
        Ok(Arc::new(
            dice_state_update_stage("parsing MODULE.bazel.lock", async {
                bzlmod_lockfile_data(&root_path, &mut file_ops).await
            })
            .await?,
        ))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_LOCK_FILE(hidden)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodHiddenLockFileKey;

#[async_trait::async_trait]
impl Key for BzlmodHiddenLockFileKey {
    type Value = bz_error::Result<Arc<BzlmodModuleLockfileData>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let mut file_ops = DiceConfigFileOps::new(ctx, project_fs, &resolver);
        let hidden_lockfile_path = ConfigPath::Project(ProjectRelativePathBuf::unchecked_new(
            "buck-out/v2/cache/bzlmod_hidden/MODULE.bazel.lock".to_owned(),
        ));
        let Some(lines) = file_ops
            .read_file_lines_if_exists(&hidden_lockfile_path)
            .await?
        else {
            return Ok(Arc::new(empty_bzlmod_lockfile_data()));
        };
        let contents = lines.join("\n");
        if !bzlmod_hidden_lockfile_schema_matches(&contents) {
            return Ok(Arc::new(empty_bzlmod_lockfile_data()));
        }
        Ok(Arc::new(bzlmod_lockfile_data_from_str(&contents)?))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REGISTRY({url})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRegistryKey {
    url: String,
}

fn bzlmod_default_registry_key() -> BzlmodRegistryKey {
    BzlmodRegistryKey {
        url: "https://bcr.bazel.build".to_owned(),
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REGISTRY_LAST_INVALIDATION")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRegistryInvalidationKey;

impl InjectedKey for BzlmodRegistryInvalidationKey {
    type Value = bz_error::Result<Arc<BzlmodRegistryInvalidationValue>>;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

pub trait SetBzlmodRegistryInvalidation {
    fn set_bzlmod_registry_invalidation(&mut self, epoch_hour: u64) -> bz_error::Result<()>;
}

impl SetBzlmodRegistryInvalidation for DiceTransactionUpdater {
    fn set_bzlmod_registry_invalidation(&mut self, epoch_hour: u64) -> bz_error::Result<()> {
        Ok(self.changed_to([(
            BzlmodRegistryInvalidationKey,
            Ok(Arc::new(BzlmodRegistryInvalidationValue { epoch_hour })),
        )])?)
    }
}

#[async_trait::async_trait]
impl Key for BzlmodRegistryKey {
    type Value = bz_error::Result<Arc<BzlmodRegistryValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let lockfile = ctx.compute(&BzlmodLockFileKey).await??;
        Ok(Arc::new(BzlmodRegistryValue {
            url: self.url.clone(),
            registry_file_hashes: lockfile.registry_file_hashes.clone(),
            selected_yanked_versions: lockfile.selected_yanked_versions.clone(),
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("YANKED_VERSIONS({module_name}, {registry_url})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodYankedVersionsKey {
    module_name: String,
    registry_url: String,
}

/// Yanked-version metadata is mutable in the registry, so unlike module files
/// it cannot be cached forever. Registry data is already refreshed at most
/// once per hour on a running daemon (see `set_bzlmod_registry_invalidation`),
/// so cache fetched metadata on disk with the same freshness: a cold daemon
/// start would otherwise issue a network request per module in the dep graph,
/// making startup latency hostage to registry weather.
#[derive(Serialize, Deserialize)]
struct CachedBzlmodYankedVersions {
    epoch_hour: u64,
    yanked_versions: Option<BTreeMap<String, String>>,
}

fn bzlmod_yanked_versions_cache_path(registry: &str, module_name: &str) -> ProjectRelativePathBuf {
    let mut hasher = Sha256::new();
    for field in ["buck2-bzlmod-yanked-versions-v1", registry, module_name] {
        hasher.update(field.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(field.as_bytes());
        hasher.update(b"\0");
    }
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/cache/bzlmod_yanked_versions/{}/metadata.json",
        hex::encode(hasher.finalize()),
    ))
}

#[async_trait::async_trait]
impl Key for BzlmodYankedVersionsKey {
    type Value = bz_error::Result<Arc<BzlmodYankedVersionsValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        // Match Bazel's YankedVersionsFunction: depend on RegistryKey, but
        // fail open if metadata.json cannot be read.
        let registry = ctx
            .compute(&BzlmodRegistryKey {
                url: self.registry_url.clone(),
            })
            .await??;
        // Like Bazel's RegistryFunction reading LAST_INVALIDATION in refresh
        // mode, depend on the hourly registry invalidation so a long-running
        // daemon re-fetches mutable yanked-version metadata once the epoch
        // advances. The same epoch stamps the on-disk cache below, extending
        // Bazel's in-memory refresh policy across daemon restarts.
        let epoch_hour = ctx
            .compute(&BzlmodRegistryInvalidationKey)
            .await??
            .epoch_hour;
        let project_fs = ctx.global_data().get_io_provider().project_root().dupe();
        let cache_path = bzlmod_yanked_versions_cache_path(&registry.url, &self.module_name);
        if let Ok(Some(contents)) = read_bzlmod_bcr_discovery_cache(&project_fs, &cache_path)
            && let Ok(cached) = serde_json::from_str::<CachedBzlmodYankedVersions>(&contents)
            && cached.epoch_hour == epoch_hour
        {
            return Ok(Arc::new(BzlmodYankedVersionsValue {
                yanked_versions: cached.yanked_versions,
            }));
        }
        let metadata_url = format!(
            "{}/modules/{}/metadata.json",
            registry.url, self.module_name
        );
        let yanked_versions = match http_get_text(&metadata_url).await {
            Ok(metadata) => bzlmod_yanked_versions_from_metadata_json(&metadata).ok(),
            Err(_) => None,
        };
        if let Ok(contents) = serde_json::to_string(&CachedBzlmodYankedVersions {
            epoch_hour,
            yanked_versions: yanked_versions.clone(),
        }) && let Err(error) =
            write_bzlmod_bcr_discovery_cache(&project_fs, &cache_path, &contents)
        {
            tracing::warn!("Error writing bzlmod yanked versions cache: {}", error);
        }
        Ok(Arc::new(BzlmodYankedVersionsValue { yanked_versions }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("CLIENT_ENVIRONMENT_VARIABLE({name})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodClientEnvironmentVariableKey {
    name: String,
}

impl InjectedKey for BzlmodClientEnvironmentVariableKey {
    type Value = bz_error::Result<Arc<BzlmodClientEnvironmentVariableValue>>;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

pub trait SetBzlmodClientEnvironment {
    fn set_bzlmod_client_environment(
        &mut self,
        vars: Vec<(String, Option<String>)>,
    ) -> bz_error::Result<()>;
}

impl SetBzlmodClientEnvironment for DiceTransactionUpdater {
    fn set_bzlmod_client_environment(
        &mut self,
        vars: Vec<(String, Option<String>)>,
    ) -> bz_error::Result<()> {
        let vars = vars.into_iter().map(|(name, value)| {
            (
                BzlmodClientEnvironmentVariableKey { name },
                Ok(Arc::new(BzlmodClientEnvironmentVariableValue { value })),
            )
        });
        Ok(self.changed_to(vars)?)
    }
}

pub trait SetBzlmodRepositoryEnvironment {
    fn set_bzlmod_repository_environment(
        &mut self,
        vars: BTreeMap<String, String>,
    ) -> bz_error::Result<()>;
}

pub trait SetBzlmodRepositoryEnvironmentData {
    fn set_bzlmod_repository_environment_data(&mut self, vars: BTreeMap<String, String>);
}

impl SetBzlmodRepositoryEnvironment for DiceTransactionUpdater {
    fn set_bzlmod_repository_environment(
        &mut self,
        vars: BTreeMap<String, String>,
    ) -> bz_error::Result<()> {
        let provided_vars = vars.keys().cloned().collect::<BTreeSet<_>>();
        let mut changed_vars = vars
            .into_iter()
            .map(|(name, value)| {
                (
                    BzlmodRepositoryEnvironmentVariableKey { name },
                    Ok(Arc::new(BzlmodRepositoryEnvironmentVariableValue {
                        value: Some(value),
                    })),
                )
            })
            .collect::<Vec<_>>();
        changed_vars.extend(
            self
            .existing_key_values_of_type_for_introspection::<
                BzlmodRepositoryEnvironmentVariableKey,
            >()
            .into_iter()
            .filter(|(key, _old)| !provided_vars.contains(&key.name))
            .map(|(key, _old)| {
                (
                    key,
                    Ok(Arc::new(BzlmodRepositoryEnvironmentVariableValue {
                        value: None,
                    })),
                )
            }),
        );
        Ok(self.changed_to(changed_vars)?)
    }
}

impl SetBzlmodRepositoryEnvironmentData for UserComputationData {
    fn set_bzlmod_repository_environment_data(&mut self, vars: BTreeMap<String, String>) {
        self.data.set(BzlmodRepositoryEnvironmentData {
            vars: Arc::new(vars),
        });
    }
}

#[async_trait::async_trait]
pub trait GetBzlmodRepositoryEnvironment {
    async fn get_bzlmod_repository_environment(
        &mut self,
    ) -> bz_error::Result<Arc<BTreeMap<String, String>>>;
}

#[async_trait::async_trait]
impl GetBzlmodRepositoryEnvironment for DiceComputations<'_> {
    async fn get_bzlmod_repository_environment(
        &mut self,
    ) -> bz_error::Result<Arc<BTreeMap<String, String>>> {
        Ok(self
            .per_transaction_data()
            .data
            .get::<BzlmodRepositoryEnvironmentData>()
            .map(|data| data.vars.dupe())
            .unwrap_or_else(|_| Arc::new(BTreeMap::new())))
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REPOSITORY_ENVIRONMENT_VARIABLE({name})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRepositoryEnvironmentVariableKey {
    name: String,
}

#[async_trait::async_trait]
impl Key for BzlmodRepositoryEnvironmentVariableKey {
    type Value = bz_error::Result<Arc<BzlmodRepositoryEnvironmentVariableValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let env = ctx
            .per_transaction_data()
            .data
            .get::<BzlmodRepositoryEnvironmentData>()
            .map(|data| data.vars.dupe())
            .unwrap_or_else(|_| Arc::new(BTreeMap::new()));
        Ok(Arc::new(BzlmodRepositoryEnvironmentVariableValue {
            value: env.get(&self.name).cloned(),
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[async_trait::async_trait]
pub trait GetBzlmodRepositoryEnvironmentVariable {
    async fn get_bzlmod_repository_environment_variable(
        &mut self,
        name: &str,
    ) -> bz_error::Result<Option<String>>;
}

#[async_trait::async_trait]
impl GetBzlmodRepositoryEnvironmentVariable for DiceComputations<'_> {
    async fn get_bzlmod_repository_environment_variable(
        &mut self,
        name: &str,
    ) -> bz_error::Result<Option<String>> {
        Ok(self
            .compute(&BzlmodRepositoryEnvironmentVariableKey {
                name: name.to_owned(),
            })
            .await??
            .value
            .clone())
    }
}

#[async_trait::async_trait]
pub trait GetBzlmodModuleExtensionRepoMappingBase {
    async fn get_bzlmod_module_extension_repo_mapping_base(
        &mut self,
        extension_bzl_cell: &str,
        extension_bzl_path: &str,
        extension_name: &str,
    ) -> bz_error::Result<Arc<BzlmodModuleExtensionRepoMappingBase>>;
}

#[async_trait::async_trait]
impl GetBzlmodModuleExtensionRepoMappingBase for DiceComputations<'_> {
    async fn get_bzlmod_module_extension_repo_mapping_base(
        &mut self,
        extension_bzl_cell: &str,
        extension_bzl_path: &str,
        extension_name: &str,
    ) -> bz_error::Result<Arc<BzlmodModuleExtensionRepoMappingBase>> {
        let dep_graph = self.compute(&BzlmodDepGraphKey).await??;
        let extension_id = BzlmodExtensionId {
            bzl_cell_name: extension_bzl_cell.to_owned(),
            bzl_path: extension_bzl_path.to_owned(),
            extension_name: extension_name.to_owned(),
        };
        // Bazel's ModuleExtensionRepoMappingEntriesFunction uses the hosting
        // module's full repo mapping here, not just its bazel_dep mapping.
        // That makes repos generated by one extension visible to repos
        // generated by another extension when the hosting module imported both.
        let mut host_aliases = dep_graph
            .extension_eval_cell_aliases_by_cell
            .get(extension_bzl_cell)
            .into_iter()
            .flat_map(|aliases| {
                aliases
                    .iter()
                    .map(|(alias, target)| (alias.clone(), target.clone()))
            })
            .collect::<Vec<_>>();
        let root_cell_name = self
            .get_cell_resolver()
            .await?
            .root_cell()
            .as_str()
            .to_owned();
        for (_alias, target) in &mut host_aliases {
            if target == "root" {
                *target = root_cell_name.clone();
            }
        }
        host_aliases.sort_unstable();
        host_aliases.dedup();
        let mut repo_overrides =
            bzlmod_module_extension_repo_overrides_for_extension(&dep_graph, &extension_id)?;
        for (_alias, target) in &mut repo_overrides {
            if target == "root" {
                *target = root_cell_name.clone();
            }
        }
        Ok(Arc::new(BzlmodModuleExtensionRepoMappingBase {
            host_aliases,
            repo_overrides,
        }))
    }
}

#[async_trait::async_trait]
pub trait BzlmodModuleExtensionEvaluator: Send + Sync + 'static {
    async fn evaluate_bzlmod_module_extension(
        &self,
        ctx: &mut DiceComputations<'_>,
        setup: BzlmodModuleExtensionRepoSetup,
    ) -> bz_error::Result<Vec<String>>;
}

pub static BZLMOD_MODULE_EXTENSION_EVALUATOR: LateBinding<
    &'static dyn BzlmodModuleExtensionEvaluator,
> = LateBinding::new("BZLMOD_MODULE_EXTENSION_EVALUATOR");

#[async_trait::async_trait]
pub trait EvaluateBzlmodModuleExtension {
    async fn evaluate_bzlmod_module_extension(
        &mut self,
        setup: BzlmodModuleExtensionRepoSetup,
    ) -> bz_error::Result<Vec<String>>;
}

#[async_trait::async_trait]
impl EvaluateBzlmodModuleExtension for DiceComputations<'_> {
    async fn evaluate_bzlmod_module_extension(
        &mut self,
        setup: BzlmodModuleExtensionRepoSetup,
    ) -> bz_error::Result<Vec<String>> {
        BZLMOD_MODULE_EXTENSION_EVALUATOR
            .get()?
            .evaluate_bzlmod_module_extension(self, setup)
            .await
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("CLIENT_ENVIRONMENT_VARIABLE(BZLMOD_ALLOW_YANKED_VERSIONS)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodAllowedYankedVersionsKey;

#[async_trait::async_trait]
impl Key for BzlmodAllowedYankedVersionsKey {
    type Value = bz_error::Result<Arc<BzlmodAllowedYankedVersionsValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let env = ctx
            .compute(&BzlmodClientEnvironmentVariableKey {
                name: BZLMOD_ALLOWED_YANKED_VERSIONS_ENV.to_owned(),
            })
            .await??;
        Ok(Arc::new(bzlmod_allowed_yanked_versions_from_env(
            env.value.as_deref(),
        )?))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("MODULE_FILE(root)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRootModuleKey;

#[async_trait::async_trait]
impl Key for BzlmodRootModuleKey {
    type Value = bz_error::Result<Arc<BzlmodRootResolutionInput>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let root_path = resolver.root_cell_instance().path().to_buf();
        let extra_root_deps = extra_bzlmod_root_deps_from_config(ctx, resolver.root_cell()).await?;
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let mut file_ops = DiceConfigFileOps::new(ctx, project_fs, &resolver);
        Ok(Arc::new(
            read_bazel_module_resolution_inputs(&root_path, &mut file_ops, extra_root_deps).await?,
        ))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("MODULE_FILE({}@{})", name, version)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModuleFileKey {
    name: String,
    version: String,
}

#[async_trait::async_trait]
impl Key for BzlmodModuleFileKey {
    type Value = bz_error::Result<Arc<DiscoveredBcrModule>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let registry = ctx.compute(&bzlmod_default_registry_key()).await??;
        let dep = BazelDep {
            name: self.name.clone(),
            version: self.version.clone(),
            apparent_name: None,
        };
        if let Some(local_path_override) = root.local_path_overrides.get(&dep.name).cloned() {
            return Ok(Arc::new(
                fetch_local_bzlmod_module(dep, local_path_override).await?,
            ));
        }
        let project_fs = ctx.global_data().get_io_provider().project_root().dupe();
        let archive_override = root.archive_overrides.get(&dep.name).cloned();
        let single_version_override = root.single_version_overrides.get(&dep.name).cloned();
        let repo = format!("{}@{}", dep.name, dep.version);
        Ok(Arc::new(
            bzlmod_repo_progress_span(
                repo,
                format!("modules/{}/{}/MODULE.bazel", dep.name, dep.version),
                "registry module",
                "fetching MODULE.bazel",
                fetch_bcr_module_file(
                    &project_fs,
                    &registry.url,
                    dep,
                    archive_override,
                    single_version_override,
                ),
            )
            .await?,
        ))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodDiscoveryResult {
    discovered: DiscoveredBcrModules,
}

/// Bump when the discovery walk or `DiscoveredBcrModule` parsing changes in a
/// way that affects the cached result.
const BZLMOD_DISCOVERY_RESULT_CACHE_VERSION: u32 = 1;

/// On-disk copy of a completed bzlmod discovery walk, so a fresh daemon can
/// skip re-reading and re-parsing every registry MODULE.bazel file when none
/// of the discovery inputs changed. This is the same idea as Bazel's
/// MODULE.bazel.lock: registry module files are immutable per (name, version),
/// so the walk is a pure function of the root module, overrides, and registry.
#[derive(Serialize, Deserialize)]
struct CachedBzlmodDiscoveryResult {
    fingerprint: String,
    modules: Vec<DiscoveredBcrModule>,
}

fn bzlmod_discovery_result_cache_path() -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(
        "buck-out/v2/cache/bzlmod_discovery/result.json".to_owned(),
    )
}

fn bzlmod_fingerprint_str(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn bzlmod_fingerprint_opt_str(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            bzlmod_fingerprint_str(hasher, value);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn bzlmod_fingerprint_dep(hasher: &mut blake3::Hasher, dep: &BazelDep) {
    bzlmod_fingerprint_str(hasher, &dep.name);
    bzlmod_fingerprint_str(hasher, &dep.version);
    bzlmod_fingerprint_opt_str(hasher, dep.apparent_name.as_deref());
}

fn bzlmod_fingerprint_patches(hasher: &mut blake3::Hasher, patches: &[BzlmodRootPatch]) {
    hasher.update(&(patches.len() as u64).to_le_bytes());
    for patch in patches {
        bzlmod_fingerprint_str(hasher, &patch.path);
        bzlmod_fingerprint_str(hasher, &patch.content);
    }
}

fn bzlmod_discovery_result_fingerprint(
    root: &BzlmodRootResolutionInput,
    registry: &BzlmodRegistryValue,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&BZLMOD_DISCOVERY_RESULT_CACHE_VERSION.to_le_bytes());
    bzlmod_fingerprint_str(&mut hasher, &registry.url);
    for (file, hash) in &registry.registry_file_hashes {
        bzlmod_fingerprint_str(&mut hasher, file);
        bzlmod_fingerprint_opt_str(&mut hasher, hash.as_deref());
    }
    for ((name, version), reason) in &registry.selected_yanked_versions {
        bzlmod_fingerprint_str(&mut hasher, name);
        bzlmod_fingerprint_str(&mut hasher, version);
        bzlmod_fingerprint_str(&mut hasher, reason);
    }
    bzlmod_fingerprint_str(&mut hasher, &root.root_module.name);
    hasher.update(&(root.root_deps.len() as u64).to_le_bytes());
    for dep in &root.root_deps {
        bzlmod_fingerprint_dep(&mut hasher, dep);
    }
    // Covers the deps of the builtin bazel_tools module, which seed the walk.
    bzlmod_fingerprint_str(&mut hasher, BAZEL_TOOLS_MODULE_TOOLS);
    for (name, archive_override) in &root.archive_overrides {
        bzlmod_fingerprint_str(&mut hasher, name);
        bzlmod_fingerprint_str(&mut hasher, &archive_override.module_name);
        hasher.update(&(archive_override.urls.len() as u64).to_le_bytes());
        for url in &archive_override.urls {
            bzlmod_fingerprint_str(&mut hasher, url);
        }
        bzlmod_fingerprint_str(&mut hasher, &archive_override.integrity);
        bzlmod_fingerprint_opt_str(&mut hasher, archive_override.strip_prefix.as_deref());
        bzlmod_fingerprint_opt_str(&mut hasher, archive_override.archive_type.as_deref());
        bzlmod_fingerprint_patches(&mut hasher, &archive_override.patches);
        hasher.update(&archive_override.patch_strip.unwrap_or(u32::MAX).to_le_bytes());
    }
    for (name, single_version_override) in &root.single_version_overrides {
        bzlmod_fingerprint_str(&mut hasher, name);
        bzlmod_fingerprint_opt_str(&mut hasher, single_version_override.version.as_deref());
        bzlmod_fingerprint_patches(&mut hasher, &single_version_override.patches);
        hasher.update(
            &single_version_override
                .patch_strip
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
    }
    for (name, local_path_override) in &root.local_path_overrides {
        bzlmod_fingerprint_str(&mut hasher, name);
        bzlmod_fingerprint_str(&mut hasher, &local_path_override.module_name);
        bzlmod_fingerprint_str(&mut hasher, &local_path_override.path);
        bzlmod_fingerprint_str(&mut hasher, &local_path_override.module_text);
        for (include, text) in &local_path_override.included_module_texts {
            bzlmod_fingerprint_str(&mut hasher, include);
            bzlmod_fingerprint_str(&mut hasher, text);
        }
    }
    hex::encode(blake3::Hasher::finalize(&hasher).as_bytes())
}

fn read_bzlmod_discovery_result_cache(
    project_fs: &ProjectRoot,
    fingerprint: &str,
) -> Option<DiscoveredBcrModules> {
    let contents =
        read_bzlmod_bcr_discovery_cache(project_fs, &bzlmod_discovery_result_cache_path())
            .ok()??;
    let cached: CachedBzlmodDiscoveryResult = serde_json::from_str(&contents).ok()?;
    if cached.fingerprint != fingerprint {
        return None;
    }
    Some(
        cached
            .modules
            .into_iter()
            .map(|module| ((module.dep.name.clone(), module.dep.version.clone()), module))
            .collect(),
    )
}

fn write_bzlmod_discovery_result_cache(
    project_fs: &ProjectRoot,
    fingerprint: &str,
    discovered: &DiscoveredBcrModules,
) {
    let cached = CachedBzlmodDiscoveryResult {
        fingerprint: fingerprint.to_owned(),
        modules: discovered.values().cloned().collect(),
    };
    let contents = match serde_json::to_string(&cached) {
        Ok(contents) => contents,
        Err(error) => {
            tracing::warn!("Error serializing bzlmod discovery result cache: {}", error);
            return;
        }
    };
    if let Err(error) = write_bzlmod_bcr_discovery_cache(
        project_fs,
        &bzlmod_discovery_result_cache_path(),
        &contents,
    ) {
        tracing::warn!("Error writing bzlmod discovery result cache: {}", error);
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_DEP_GRAPH(discovery)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodDiscoveryKey;

#[async_trait::async_trait]
impl Key for BzlmodDiscoveryKey {
    type Value = bz_error::Result<Arc<BzlmodDiscoveryResult>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let registry = ctx.compute(&bzlmod_default_registry_key()).await??;
        let project_fs = ctx.global_data().get_io_provider().project_root().dupe();
        let fingerprint = bzlmod_discovery_result_fingerprint(&root, &registry);
        if let Some(discovered) = read_bzlmod_discovery_result_cache(&project_fs, &fingerprint) {
            return Ok(Arc::new(BzlmodDiscoveryResult { discovered }));
        }
        let mut discovered = DiscoveredBcrModules::new();
        let mut scheduled = BTreeSet::<(String, String)>::new();
        let mut frontier = Vec::new();
        let mut discovery_roots = root.root_deps.clone();
        discovery_roots.extend(root.builtin_bazel_tools_module.deps.iter().cloned());
        for dep in discovery_roots {
            if let Some(dep) = bzlmod_discovery_dep(
                dep,
                &root.root_module.name,
                &root.archive_overrides,
                &root.single_version_overrides,
                &root.local_path_overrides,
            ) && scheduled.insert((dep.name.clone(), dep.version.clone()))
            {
                frontier.push(dep);
            }
        }

        while !frontier.is_empty() {
            let keys = frontier
                .drain(..)
                .map(|dep| BzlmodModuleFileKey {
                    name: dep.name,
                    version: dep.version,
                })
                .collect::<Vec<_>>();
            let modules: Vec<Arc<DiscoveredBcrModule>> = ctx
                .try_compute_join(keys, |ctx, key| {
                    async move { ctx.compute(&key).await? }.boxed()
                })
                .await?;
            for module in modules {
                let key = (module.dep.name.clone(), module.dep.version.clone());
                for child in &module.deps {
                    if let Some(child) = bzlmod_discovery_dep(
                        child.clone(),
                        &root.root_module.name,
                        &root.archive_overrides,
                        &root.single_version_overrides,
                        &root.local_path_overrides,
                    ) && scheduled.insert((child.name.clone(), child.version.clone()))
                    {
                        frontier.push(child);
                    }
                }
                discovered.insert(key, (*module).clone());
            }
        }
        write_bzlmod_discovery_result_cache(&project_fs, &fingerprint, &discovered);
        Ok(Arc::new(BzlmodDiscoveryResult { discovered }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
struct BzlmodSelectionResult {
    discovered: DiscoveredBcrModules,
    selected_keys: BTreeSet<(String, String)>,
    selected_keys_in_bfs_order: Vec<(String, String)>,
    selected_keys_in_dependency_order: Vec<(String, String)>,
    canonical_repo_names_by_key: BTreeMap<(String, String), String>,
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MODULE_RESOLUTION")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModuleResolutionKey;

#[async_trait::async_trait]
impl Key for BzlmodModuleResolutionKey {
    type Value = bz_error::Result<Arc<BzlmodSelectionResult>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let discovery = ctx.compute(&BzlmodDiscoveryKey).await??;
        let selection = select_bzlmod_modules(
            &root.root_deps,
            root.builtin_bazel_tools_module.clone(),
            &discovery.discovered,
        )?;
        collect_bzlmod_yanked_versions(ctx, &root, &selection).await?;
        Ok(Arc::new(selection))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MODULE_RESOLUTION(selection)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodSelectionKey;

#[async_trait::async_trait]
impl Key for BzlmodSelectionKey {
    type Value = bz_error::Result<Arc<BzlmodSelectionResult>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        ctx.compute(&BzlmodModuleResolutionKey).await?
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REPO_SPEC({}@{})", name, version)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRepoSpecKey {
    name: String,
    version: String,
}

#[async_trait::async_trait]
impl Key for BzlmodRepoSpecKey {
    type Value = bz_error::Result<Arc<BzlmodRepoSpecValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let registry = ctx.compute(&bzlmod_default_registry_key()).await??;
        let _module_file = ctx
            .compute(&BzlmodModuleFileKey {
                name: self.name.clone(),
                version: self.version.clone(),
            })
            .await??;
        let dep = BazelDep {
            name: self.name.clone(),
            version: self.version.clone(),
            apparent_name: None,
        };
        let project_fs = ctx.global_data().get_io_provider().project_root().dupe();
        let repo = format!("{}@{}", dep.name, dep.version);
        let source_json_path = format!("modules/{}/{}/source.json", dep.name, dep.version);
        let source_json_url = format!("{}/{}", registry.url, source_json_path);
        let source_json = bzlmod_repo_progress_span(
            repo,
            source_json_path,
            "registry repo spec",
            "fetching source.json",
            fetch_bcr_module_source_json(
                &project_fs,
                &registry.url,
                &dep,
                root.archive_overrides.get(&dep.name),
                root.local_path_overrides.get(&dep.name),
            ),
        )
        .await?;
        let registry_file_hashes = registry
            .registry_file_hashes
            .iter()
            .filter(|(url, _hash)| *url == &source_json_url)
            .map(|(url, hash)| (url.clone(), hash.clone()))
            .collect();
        Ok(Arc::new(BzlmodRepoSpecValue {
            source_json,
            registry_file_hashes,
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_DEP_GRAPH")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodDepGraphKey;

#[async_trait::async_trait]
impl Key for BzlmodDepGraphKey {
    type Value = bz_error::Result<Arc<BzlmodDepGraph>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let lockfile = ctx.compute(&BzlmodLockFileKey).await??;
        let hidden_lockfile = ctx.compute(&BzlmodHiddenLockFileKey).await??;
        let selection = ctx.compute(&BzlmodSelectionKey).await??;
        let repo_spec_keys = selection
            .selected_keys
            .iter()
            .filter(|(name, _version)| name != "bazel_tools")
            .map(|(name, version)| BzlmodRepoSpecKey {
                name: name.clone(),
                version: version.clone(),
            })
            .collect::<Vec<_>>();
        let repo_specs: Vec<(BzlmodRepoSpecKey, Arc<BzlmodRepoSpecValue>)> = ctx
            .try_compute_join(repo_spec_keys, |ctx, key| {
                async move {
                    let repo_spec = ctx.compute(&key).await??;
                    bz_error::Ok((key, repo_spec))
                }
                .boxed()
            })
            .await?;
        let mut discovered = selection.discovered.clone();
        for (key, repo_spec) in repo_specs {
            let _registry_file_hashes = &repo_spec.registry_file_hashes;
            if let Some(module) = discovered.get_mut(&(key.name, key.version)) {
                module.source_json = repo_spec.source_json.clone();
            }
        }
        let mut root_module = root.root_module.clone();
        root_module.lockfile_extension_generated_repos = lockfile.extension_generated_repos.clone();
        for (extension_key, repo_names) in &hidden_lockfile.extension_generated_repos {
            root_module
                .lockfile_extension_generated_repos
                .entry(extension_key.clone())
                .or_default()
                .extend(repo_names.iter().cloned());
        }
        root_module.lockfile_extension_facts = lockfile
            .extension_facts
            .union(&hidden_lockfile.extension_facts)
            .cloned()
            .collect();
        Ok(Arc::new(bzlmod_dep_graph_from_selection(
            &root.root_deps,
            root_module,
            discovered,
            &selection,
        )?))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("SINGLE_EXTENSION_USAGES({extension_id:?})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodSingleExtensionUsagesKey {
    extension_id: BzlmodExtensionId,
}

#[async_trait::async_trait]
impl Key for BzlmodSingleExtensionUsagesKey {
    type Value = bz_error::Result<Arc<BzlmodSingleExtensionUsagesValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        let unique_name = dep_graph
            .extension_unique_names
            .get(&self.extension_id)
            .ok_or_else(|| {
                bz_error!(
                    bz_error::ErrorTag::Input,
                    "bzlmod module extension `{}//{}%{}` has no usages",
                    self.extension_id.bzl_cell_name,
                    self.extension_id.bzl_path,
                    self.extension_id.extension_name
                )
            })?
            .clone();
        let extension_usages_json = bzlmod_module_extension_evaluation_config_json(
            &dep_graph.root_module,
            &dep_graph.discovered,
            &dep_graph.selected_keys_in_bfs_order,
            &dep_graph.canonical_repo_names_by_key,
            &dep_graph.canonical_repo_names_by_cell,
            &dep_graph.extension_eval_cell_aliases_by_cell,
            &self.extension_id,
            &dep_graph.extension_unique_names,
        )?;
        Ok(Arc::new(BzlmodSingleExtensionUsagesValue {
            extension_id: self.extension_id.clone(),
            unique_name,
            extension_usages_json,
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MODULE_RESOLUTION(full)")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodResolutionKey;

#[async_trait::async_trait]
impl Key for BzlmodResolutionKey {
    type Value = bz_error::Result<Arc<BazelModuleCellAliases>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let registry = ctx.compute(&bzlmod_default_registry_key()).await??;
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        let extension_usage_keys = dep_graph
            .extension_unique_names
            .keys()
            .cloned()
            .map(|extension_id| BzlmodSingleExtensionUsagesKey { extension_id })
            .collect::<Vec<_>>();
        let single_extension_usages: Vec<Arc<BzlmodSingleExtensionUsagesValue>> = ctx
            .try_compute_join(extension_usage_keys, |ctx, key| {
                async move {
                    let value = ctx.compute(&key).await??;
                    bz_error::Ok(value)
                }
                .boxed()
            })
            .await?;
        let extension_usages_json_by_id = single_extension_usages
            .iter()
            .map(|value| {
                (
                    value.extension_id.clone(),
                    value.extension_usages_json.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let host_platform_setup = bzlmod_host_platform_setup_from_config(ctx).await?;
        let BcrResolution {
            external_modules,
            root_aliases,
            cell_aliases,
            registered_toolchains,
        } = resolve_bcr_modules_from_dep_graph(
            &registry.url,
            &dep_graph,
            &root.archive_overrides,
            &root.single_version_overrides,
            &root.local_path_overrides,
            &extension_usages_json_by_id,
            &host_platform_setup,
        )?;
        let mut aliases = root.aliases.clone();
        aliases.external_modules = external_modules;
        aliases.root_aliases.extend(root_aliases);
        aliases.cell_aliases = cell_aliases;
        aliases.registered_toolchains.extend(registered_toolchains);
        aliases.normalize();
        let root_cell_name = ctx
            .get_cell_resolver()
            .await?
            .root_cell()
            .as_str()
            .to_owned();
        register_bzlmod_cell_canonical_repo_name_for_cell(&root_cell_name, "");
        // Bazel keeps repository mappings as Skyframe values and consumers reuse the computed value.
        // Register Buck2's global lookup side effects with the DICE value computation instead of
        // repeating them on every accessor of the cached resolution.
        aliases.register_for_starlark_label_resolution(&root_cell_name);
        aliases.register_external_cell_origins()?;
        Ok(Arc::new(aliases))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MODULE_INSPECTION")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModuleInspectionKey;

#[async_trait::async_trait]
impl Key for BzlmodModuleInspectionKey {
    type Value = bz_error::Result<Arc<BzlmodModuleInspectionValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let _root = ctx.compute(&BzlmodRootModuleKey).await??;
        let _module_resolution = ctx.compute(&BzlmodModuleResolutionKey).await??;
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        Ok(Arc::new(bzlmod_module_inspection_value(&dep_graph)?))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_MOD_TIDY")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModTidyKey;

#[async_trait::async_trait]
impl Key for BzlmodModTidyKey {
    type Value = bz_error::Result<Arc<BzlmodModTidyValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let root = ctx.compute(&BzlmodRootModuleKey).await??;
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        let mut keys = BTreeSet::new();
        for usage in &root.root_module.extension_usages {
            keys.insert(bzlmod_resolve_extension_id(
                "root",
                usage,
                &dep_graph.extension_eval_cell_aliases_by_cell,
            )?);
        }
        let root_extension_usages = ctx
            .try_compute_join(
                keys.into_iter()
                    .map(|extension_id| BzlmodSingleExtensionUsagesKey { extension_id })
                    .collect::<Vec<_>>(),
                |ctx, key| async move { ctx.compute(&key).await? }.boxed(),
            )
            .await?
            .into_iter()
            .map(|value| (*value).clone())
            .collect();
        let mut errors = Vec::new();
        for usage in &root_extension_usages {
            let setup = bzlmod_single_extension_eval_setup(usage);
            if let Err(error) = ctx.evaluate_bzlmod_module_extension(setup).await {
                errors.push(error.to_string());
            }
        }
        Ok(Arc::new(BzlmodModTidyValue {
            root_extension_usages,
            errors,
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REPO_DEFINITION({canonical_repo_name})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRepoDefinitionKey {
    canonical_repo_name: String,
}

#[async_trait::async_trait]
impl Key for BzlmodRepoDefinitionKey {
    type Value = bz_error::Result<Arc<BzlmodRepoDefinitionValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let aliases = ctx.compute(&BzlmodResolutionKey).await??;
        let Some(module) = aliases
            .external_modules
            .iter()
            .find(|module| module.canonical_repo_name() == self.canonical_repo_name)
            .cloned()
        else {
            return Ok(Arc::new(BzlmodRepoDefinitionValue {
                module: None,
                configure: false,
            }));
        };
        let configure = bzlmod_external_module_is_configure_repo(&module);
        Ok(Arc::new(BzlmodRepoDefinitionValue {
            module: Some(module),
            configure,
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REPOSITORY_DIRECTORY({canonical_repo_name})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodRepositoryDirectoryKey {
    canonical_repo_name: String,
}

#[async_trait::async_trait]
impl Key for BzlmodRepositoryDirectoryKey {
    type Value = bz_error::Result<Arc<BzlmodRepositoryDirectoryValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let repo_definition = ctx
            .compute(&BzlmodRepoDefinitionKey {
                canonical_repo_name: self.canonical_repo_name.clone(),
            })
            .await??;
        let Some(module) = repo_definition.module.as_ref() else {
            return Ok(Arc::new(BzlmodRepositoryDirectoryValue {
                found: false,
                exclude_from_vendoring: true,
            }));
        };
        let local = bzlmod_external_module_is_local(module);
        Ok(Arc::new(BzlmodRepositoryDirectoryValue {
            found: true,
            exclude_from_vendoring: local || repo_definition.configure,
        }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BAZEL_FETCH_ALL(configure={configure})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodFetchAllKey {
    configure: bool,
}

#[async_trait::async_trait]
impl Key for BzlmodFetchAllKey {
    type Value = bz_error::Result<Arc<BzlmodFetchAllValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let aliases = ctx.compute(&BzlmodResolutionKey).await??;
        let mut repos_to_vendor = aliases
            .external_modules
            .iter()
            .map(|module| module.canonical_repo_name().to_owned())
            .collect::<Vec<_>>();
        let dep_graph = ctx.compute(&BzlmodDepGraphKey).await??;
        let extension_usage_keys = dep_graph
            .extension_unique_names
            .keys()
            .cloned()
            .map(|extension_id| BzlmodSingleExtensionUsagesKey { extension_id })
            .collect::<Vec<_>>();
        let single_extension_usages: Vec<Arc<BzlmodSingleExtensionUsagesValue>> = ctx
            .try_compute_join(extension_usage_keys, |ctx, key| {
                async move {
                    let value = ctx.compute(&key).await??;
                    bz_error::Ok(value)
                }
                .boxed()
            })
            .await?;
        let extension_eval_setups = single_extension_usages
            .iter()
            .map(|usage| bzlmod_single_extension_eval_setup(usage))
            .collect::<Vec<_>>();
        let extension_generated_repos: Vec<Vec<String>> = ctx
            .try_compute_join(extension_eval_setups, |ctx, setup| {
                async move {
                    let unique_name = setup.extension_unique_name.to_string();
                    let repo_names = ctx.evaluate_bzlmod_module_extension(setup).await?;
                    bz_error::Ok(
                        repo_names
                            .into_iter()
                            .map(|repo_name| format!("{unique_name}+{repo_name}"))
                            .collect::<Vec<_>>(),
                    )
                }
                .boxed()
            })
            .await?;
        repos_to_vendor.extend(extension_generated_repos.into_iter().flatten());
        if self.configure {
            let repo_definitions: Vec<(BzlmodRepoDefinitionKey, Arc<BzlmodRepoDefinitionValue>)> =
                ctx.try_compute_join(
                    repos_to_vendor
                        .iter()
                        .cloned()
                        .map(|canonical_repo_name| BzlmodRepoDefinitionKey {
                            canonical_repo_name,
                        })
                        .collect::<Vec<_>>(),
                    |ctx, key| {
                        async move {
                            let value = ctx.compute(&key).await??;
                            bz_error::Ok((key, value))
                        }
                        .boxed()
                    },
                )
                .await?;
            repos_to_vendor = repo_definitions
                .into_iter()
                .filter_map(|(key, value)| value.configure.then_some(key.canonical_repo_name))
                .collect();
        }
        let repo_directories: Vec<(
            BzlmodRepositoryDirectoryKey,
            Arc<BzlmodRepositoryDirectoryValue>,
        )> = ctx
            .try_compute_join(
                repos_to_vendor
                    .iter()
                    .cloned()
                    .map(|canonical_repo_name| BzlmodRepositoryDirectoryKey {
                        canonical_repo_name,
                    })
                    .collect::<Vec<_>>(),
                |ctx, key| {
                    async move {
                        let value = ctx.compute(&key).await??;
                        bz_error::Ok((key, value))
                    }
                    .boxed()
                },
            )
            .await?;
        repos_to_vendor = repo_directories
            .into_iter()
            .filter_map(|(key, value)| {
                (value.found && !value.exclude_from_vendoring).then_some(key.canonical_repo_name)
            })
            .collect();
        repos_to_vendor.sort_unstable();
        repos_to_vendor.dedup();
        Ok(Arc::new(BzlmodFetchAllValue { repos_to_vendor }))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("VENDOR_FILE")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodVendorFileKey;

#[async_trait::async_trait]
impl Key for BzlmodVendorFileKey {
    type Value = bz_error::Result<Arc<BzlmodVendorFileValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let root_path = resolver.root_cell_instance().path().to_buf();
        let io_provider = ctx.global_data().get_io_provider();
        let project_fs = io_provider.project_root();
        let mut file_ops = DiceConfigFileOps::new(ctx, project_fs, &resolver);
        Ok(Arc::new(
            bzlmod_vendor_file_data(&root_path, &mut file_ops).await?,
        ))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn collect_bzlmod_yanked_versions(
    ctx: &mut DiceComputations<'_>,
    root: &BzlmodRootResolutionInput,
    selection: &BzlmodSelectionResult,
) -> bz_error::Result<()> {
    let registry = ctx.compute(&bzlmod_default_registry_key()).await??;
    let allowed_yanked_versions = ctx.compute(&BzlmodAllowedYankedVersionsKey).await??;
    let mut keys = Vec::new();
    for (name, version) in &selection.selected_keys {
        if name == "bazel_tools"
            || root.archive_overrides.contains_key(name)
            || root.local_path_overrides.contains_key(name)
        {
            continue;
        }
        if let Some(info) = registry
            .selected_yanked_versions
            .get(&(name.clone(), version.clone()))
        {
            if bzlmod_yanked_version_allowed(&allowed_yanked_versions, name, version) {
                continue;
            }
            return Err(bzlmod_yanked_version_error(name, version, info));
        }
        let source_json_url = format!("{}/modules/{name}/{version}/source.json", registry.url);
        if registry.registry_file_hashes.contains_key(&source_json_url) {
            continue;
        }
        keys.push(BzlmodYankedVersionsKey {
            module_name: name.clone(),
            registry_url: registry.url.clone(),
        });
    }

    let yanked_versions: Vec<(BzlmodYankedVersionsKey, Arc<BzlmodYankedVersionsValue>)> = ctx
        .try_compute_join(keys, |ctx, key| {
            async move {
                let value = ctx.compute(&key).await??;
                bz_error::Ok((key, value))
            }
            .boxed()
        })
        .await?;
    for (key, value) in yanked_versions {
        let Some(info) = value.yanked_versions.as_ref().and_then(|versions| {
            versions.get(selection.selected_versions_for_name(&key.module_name)?)
        }) else {
            continue;
        };
        let version = selection
            .selected_versions_for_name(&key.module_name)
            .unwrap_or("");
        if bzlmod_yanked_version_allowed(&allowed_yanked_versions, &key.module_name, version) {
            continue;
        }
        return Err(bzlmod_yanked_version_error(&key.module_name, version, info));
    }
    Ok(())
}

fn bzlmod_yanked_version_allowed(
    allowed_yanked_versions: &BzlmodAllowedYankedVersionsValue,
    name: &str,
    version: &str,
) -> bool {
    allowed_yanked_versions.allow_all
        || allowed_yanked_versions
            .modules
            .contains(&(name.to_owned(), version.to_owned()))
}

fn bzlmod_yanked_version_error(name: &str, version: &str, info: &str) -> bz_error::Error {
    bz_error!(
        bz_error::ErrorTag::Input,
        "Yanked version detected in bzlmod dependency graph: {}@{}, for the reason: {}. Use a newer version of this module, record the allowed yanked version in MODULE.bazel.lock with Bazel, or allow it with {}.",
        name,
        version,
        info,
        BZLMOD_ALLOWED_YANKED_VERSIONS_ENV
    )
}

fn bzlmod_allowed_yanked_versions_from_env(
    value: Option<&str>,
) -> bz_error::Result<BzlmodAllowedYankedVersionsValue> {
    let mut modules = BTreeSet::new();
    let Some(value) = value else {
        return Ok(BzlmodAllowedYankedVersionsValue {
            allow_all: false,
            modules,
        });
    };
    for module in value.split(',') {
        if module.is_empty() {
            continue;
        }
        if module == "all" {
            return Ok(BzlmodAllowedYankedVersionsValue {
                allow_all: true,
                modules: BTreeSet::new(),
            });
        }
        let Some((name, version)) = module.split_once('@') else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "Parsing environment variable {}={} failed, module versions must be of the form '<module name>@<version>'",
                BZLMOD_ALLOWED_YANKED_VERSIONS_ENV,
                value
            ));
        };
        if !is_valid_bzlmod_module_name(name) {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "Parsing environment variable {}={} failed, invalid module name `{}`",
                BZLMOD_ALLOWED_YANKED_VERSIONS_ENV,
                value,
                name
            ));
        }
        parse_bzlmod_version(version).with_buck_error_context(|| {
            format!(
                "Parsing environment variable {}={} failed, invalid version specified for module `{}`",
                BZLMOD_ALLOWED_YANKED_VERSIONS_ENV, value, name
            )
        })?;
        modules.insert((name.to_owned(), version.to_owned()));
    }
    Ok(BzlmodAllowedYankedVersionsValue {
        allow_all: false,
        modules,
    })
}

impl BzlmodSelectionResult {
    fn selected_versions_for_name(&self, name: &str) -> Option<&str> {
        self.selected_keys
            .iter()
            .find_map(|(selected_name, version)| {
                (selected_name == name).then_some(version.as_str())
            })
    }
}

fn bzlmod_discovery_dep(
    mut dep: BazelDep,
    root_module_name: &str,
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, BzlmodSingleVersionOverride>,
    local_path_overrides: &BTreeMap<String, BzlmodLocalPathOverride>,
) -> Option<BazelDep> {
    if dep.name == root_module_name {
        return None;
    }
    if archive_overrides.contains_key(&dep.name) {
        dep.version.clear();
    } else if local_path_overrides.contains_key(&dep.name) {
        dep.version.clear();
    } else if let Some(version_override) = single_version_overrides.get(&dep.name)
        && let Some(version) = &version_override.version
    {
        dep.version = version.clone();
    }
    Some(dep)
}

fn select_bzlmod_modules(
    root_deps: &[BazelDep],
    builtin_bazel_tools_module: DiscoveredBcrModule,
    discovered: &DiscoveredBcrModules,
) -> bz_error::Result<BzlmodSelectionResult> {
    let mut discovered = discovered.clone();
    discovered.insert(
        (
            builtin_bazel_tools_module.dep.name.clone(),
            builtin_bazel_tools_module.dep.version.clone(),
        ),
        builtin_bazel_tools_module,
    );
    let mut selected_versions = BTreeMap::<String, String>::new();
    for (name, version) in discovered.keys() {
        match selected_versions.get(name) {
            Some(existing)
                if bzlmod_version_cmp(version, existing).with_buck_error_context(|| {
                    format!("Invalid version for module `{name}`")
                })? != Ordering::Greater => {}
            _ => {
                selected_versions.insert(name.clone(), version.clone());
            }
        }
    }

    let mut selected_keys = BTreeSet::<(String, String)>::new();
    let mut visit = VecDeque::new();
    for dep in root_deps {
        if let Some(version) = selected_versions.get(&dep.name) {
            visit.push_back((dep.name.clone(), version.clone()));
        }
    }
    if let Some(version) = selected_versions.get("bazel_tools") {
        visit.push_back(("bazel_tools".to_owned(), version.clone()));
    }
    let mut selected_keys_in_bfs_order = Vec::new();
    while let Some(key) = visit.pop_front() {
        if !selected_keys.insert(key.clone()) {
            continue;
        }
        selected_keys_in_bfs_order.push(key.clone());
        let Some(module) = discovered.get(&key) else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "selected bzlmod module `{}@{}` was not discovered",
                key.0,
                key.1
            ));
        };
        for dep in &module.deps {
            if let Some(version) = selected_versions.get(&dep.name) {
                visit.push_back((dep.name.clone(), version.clone()));
            }
        }
    }
    let selected_keys_in_dependency_order = bzlmod_selected_keys_dependency_first(
        &discovered,
        root_deps,
        &selected_versions,
        &selected_keys,
    );
    let canonical_repo_names_by_key = bzlmod_canonical_repo_names_by_key(&selected_keys);
    Ok(BzlmodSelectionResult {
        discovered,
        selected_keys,
        selected_keys_in_bfs_order,
        selected_keys_in_dependency_order,
        canonical_repo_names_by_key,
    })
}

async fn bzlmod_repo_progress_span<T, Fut>(
    repo: String,
    path: String,
    kind: &'static str,
    progress: &'static str,
    fut: Fut,
) -> bz_error::Result<T>
where
    Fut: Future<Output = bz_error::Result<T>>,
{
    bz_events::dispatch::span_async(
        bz_data::BzlmodRepoStart {
            repo: repo.clone(),
            path: path.clone(),
            kind: kind.to_owned(),
            progress: progress.to_owned(),
        },
        async {
            (
                fut.await,
                bz_data::BzlmodRepoEnd {
                    repo,
                    path,
                    kind: kind.to_owned(),
                },
            )
        },
    )
    .await
}

async fn bzlmod_http_client() -> bz_error::Result<HttpClient> {
    let mut builder = HttpClientBuilder::oss().await?;
    builder
        .with_max_redirects(10)
        .with_http2(false)
        .with_connect_timeout(Some(Duration::from_secs(60)))
        .with_response_header_timeout(Some(Duration::from_secs(60)))
        .with_read_timeout(Some(Duration::from_secs(60)))
        .with_write_timeout(Some(Duration::from_secs(60)))
        .with_max_concurrent_requests(Some(8));
    Ok(builder.build())
}

async fn shared_bzlmod_http_client() -> bz_error::Result<HttpClient> {
    Ok(BZLMOD_HTTP_CLIENT
        .get_or_try_init(bzlmod_http_client)
        .await?
        .dupe())
}

fn apply_bzlmod_dep_overrides(
    deps: &mut [BazelDep],
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, BzlmodSingleVersionOverride>,
    local_path_overrides: &BTreeMap<String, BzlmodLocalPathOverride>,
) {
    for dep in deps {
        if archive_overrides.contains_key(&dep.name) {
            dep.version.clear();
        } else if local_path_overrides.contains_key(&dep.name) {
            dep.version.clear();
        } else if let Some(version_override) = single_version_overrides.get(&dep.name) {
            if let Some(version) = &version_override.version {
                dep.version = version.clone();
            }
        }
    }
}

fn bzlmod_dep_graph_from_selection(
    root_deps: &[BazelDep],
    root_module: RootBzlmodModule,
    discovered: DiscoveredBcrModules,
    selection: &BzlmodSelectionResult,
) -> bz_error::Result<BzlmodDepGraph> {
    let selected_keys = selection.selected_keys.clone();
    let selected_keys_in_bfs_order = selection.selected_keys_in_bfs_order.clone();
    let selected_keys_in_dependency_order = selection.selected_keys_in_dependency_order.clone();
    let canonical_repo_names_by_key = selection.canonical_repo_names_by_key.clone();
    let selected_versions = selected_keys
        .iter()
        .map(|(name, version)| (name.clone(), version.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut root_aliases_by_key = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let mut cell_aliases_by_cell = BzlmodCellAliasesByCell::default();
    if !root_module.repo_name.is_empty() {
        add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "root",
            &root_module.repo_name,
            "root",
        );
    }
    for dep in root_deps {
        add_bzlmod_dep_alias(dep, &selected_versions, &mut root_aliases_by_key);
        add_bzlmod_dep_cell_alias(
            "root",
            dep,
            &selected_versions,
            &canonical_repo_names_by_key,
            &mut cell_aliases_by_cell,
        )?;
    }
    for key in &selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            &canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            &cell_name,
            &canonical_repo_name,
            &cell_name,
        );
        if module.dep.name == "platforms" {
            root_aliases_by_key
                .entry(key.clone())
                .or_default()
                .insert("platforms".to_owned());
            add_bzlmod_cell_alias(&mut cell_aliases_by_cell, "root", "platforms", &cell_name);
        }
        for alias in &module.module_aliases {
            add_bzlmod_cell_alias(&mut cell_aliases_by_cell, &cell_name, alias, &cell_name);
            add_bzlmod_cell_alias(&mut cell_aliases_by_cell, "bazel_tools", alias, &cell_name);
        }
        for dep in &module.deps {
            if dep.name == root_module.name {
                if let Some(alias) = dep.apparent_name.as_ref() {
                    add_bzlmod_cell_alias(&mut cell_aliases_by_cell, &cell_name, alias, "root");
                }
                continue;
            }
            add_bzlmod_dep_cell_alias(
                &cell_name,
                dep,
                &selected_versions,
                &canonical_repo_names_by_key,
                &mut cell_aliases_by_cell,
            )?;
        }
    }

    let mut canonical_repo_names_by_cell = BTreeMap::<String, String>::new();
    canonical_repo_names_by_cell.insert("bazel_tools".to_owned(), "bazel_tools".to_owned());
    canonical_repo_names_by_cell.insert("root".to_owned(), root_module.canonical_repo_name.clone());
    for key in &selected_keys {
        let canonical_repo_name = canonical_repo_names_by_key
            .get(key)
            .expect("selected key should have canonical repo name")
            .clone();
        canonical_repo_names_by_cell.insert(
            bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name),
            canonical_repo_name,
        );
    }
    let extension_unique_names = bzlmod_extension_unique_names(
        &root_module,
        &discovered,
        &selected_keys,
        &canonical_repo_names_by_key,
        &cell_aliases_by_cell,
        &canonical_repo_names_by_cell,
    )?;
    let extension_eval_cell_aliases_by_cell = bzlmod_extension_eval_cell_aliases_by_cell(
        &root_module,
        &discovered,
        &selected_keys,
        &canonical_repo_names_by_key,
        &cell_aliases_by_cell,
        &extension_unique_names,
    )?;

    Ok(BzlmodDepGraph {
        root_module,
        discovered,
        selected_keys,
        selected_keys_in_bfs_order,
        selected_keys_in_dependency_order,
        canonical_repo_names_by_key,
        canonical_repo_names_by_cell,
        root_aliases_by_key,
        cell_aliases_by_cell,
        extension_eval_cell_aliases_by_cell,
        extension_unique_names,
    })
}

fn resolve_bcr_modules_from_dep_graph(
    registry: &str,
    dep_graph: &BzlmodDepGraph,
    archive_overrides: &BTreeMap<String, BzlmodArchiveOverride>,
    single_version_overrides: &BTreeMap<String, BzlmodSingleVersionOverride>,
    local_path_overrides: &BTreeMap<String, BzlmodLocalPathOverride>,
    extension_usages_json_by_id: &BTreeMap<BzlmodExtensionId, String>,
    host_platform_setup: &BzlmodHostPlatformSetup,
) -> bz_error::Result<BcrResolution> {
    let root_module = &dep_graph.root_module;
    let discovered = &dep_graph.discovered;
    let selected_keys = &dep_graph.selected_keys;
    let canonical_repo_names_by_key = &dep_graph.canonical_repo_names_by_key;
    let mut root_aliases_by_key = dep_graph.root_aliases_by_key.clone();
    let mut cell_aliases_by_cell = dep_graph.cell_aliases_by_cell.clone();

    let mut resolved = BTreeMap::<String, BazelCompatExternalModule>::new();
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let mut aliases = root_aliases_by_key
            .remove(key)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        aliases.sort_unstable();
        aliases.dedup();

        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            &canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let archive_override = archive_overrides.get(&module.dep.name);
        let single_version_override = single_version_overrides.get(&module.dep.name);
        let local_path_override = local_path_overrides.get(&module.dep.name);
        let patch_configs = bzlmod_patch_configs(
            registry,
            &module.dep,
            &module.source_json,
            archive_override,
            single_version_override,
        );
        let patches_json = serde_json::to_string(&patch_configs)
            .buck_error_context("Error serializing bzlmod patch configuration")?;
        let overlay_configs = bzlmod_overlay_configs(registry, &module.dep, &module.source_json);
        let overlays_json = serde_json::to_string(&overlay_configs)
            .buck_error_context("Error serializing bzlmod overlay configuration")?;
        let patch_strip = archive_override
            .and_then(|archive_override| archive_override.patch_strip)
            .or_else(|| {
                single_version_override.and_then(|version_override| version_override.patch_strip)
            })
            .or(module.source_json.patch_strip)
            .unwrap_or(0);
        let urls = bcr_source_urls(&module.source_json);
        let urls_json =
            serde_json::to_string(&urls).buck_error_context("Error serializing bzlmod URLs")?;
        let cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        if module.dep.name == "bazel_tools" {
            continue;
        }
        resolved.insert(
            cell_name.clone(),
            BazelCompatExternalModule::Registry(BazelCompatRegistryModule {
                cell_name,
                aliases,
                module_name: module.dep.name.clone(),
                version: module.dep.version.clone(),
                canonical_repo_name,
                local_path: local_path_override
                    .map(|local_path_override| local_path_override.path.clone()),
                url: module.source_json.url.clone(),
                urls_json,
                integrity: module.source_json.integrity.clone(),
                strip_prefix: module.source_json.strip_prefix.clone(),
                archive_type: module.source_json.archive_type.clone(),
                patches_json,
                overlays_json,
                patch_strip,
            }),
        );
    }

    let mut resolved = resolved.into_values().collect::<Vec<_>>();
    let generated_resolution = resolve_generated_bzlmod_repos(
        root_module,
        discovered,
        &dep_graph.selected_keys_in_dependency_order,
        canonical_repo_names_by_key,
        &mut cell_aliases_by_cell,
        &dep_graph.canonical_repo_names_by_cell,
        &dep_graph.extension_unique_names,
        extension_usages_json_by_id,
        host_platform_setup,
    )?;
    resolved.extend(generated_resolution.external_modules);
    let registered_toolchains = resolve_bzlmod_registered_toolchains(
        root_module,
        discovered,
        &dep_graph.selected_keys_in_bfs_order,
        canonical_repo_names_by_key,
        &cell_aliases_by_cell,
    )?;
    Ok(BcrResolution {
        external_modules: resolved,
        root_aliases: cell_aliases_by_cell
            .remove("root")
            .map(bzlmod_cell_alias_map_to_vec)
            .unwrap_or_default(),
        cell_aliases: cell_aliases_by_cell
            .into_iter()
            .map(|(cell, aliases)| (cell, bzlmod_cell_alias_map_to_vec(aliases)))
            .collect(),
        registered_toolchains,
    })
}

fn bzlmod_extension_eval_cell_aliases_by_cell(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    base_cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
) -> bz_error::Result<BzlmodCellAliasesByCell> {
    let mut cell_aliases_by_cell = base_cell_aliases_by_cell.clone();
    for usage in &root_module.extension_usages {
        add_bzlmod_extension_usage_eval_aliases(
            usage,
            "root",
            base_cell_aliases_by_cell,
            &mut cell_aliases_by_cell,
            extension_unique_names,
        )?;
    }
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        for usage in &module.extension_usages {
            add_bzlmod_extension_usage_eval_aliases(
                usage,
                &module_cell_name,
                base_cell_aliases_by_cell,
                &mut cell_aliases_by_cell,
                extension_unique_names,
            )?;
        }
    }
    Ok(cell_aliases_by_cell)
}

fn add_bzlmod_extension_usage_eval_aliases(
    usage: &BzlmodExtensionUsage,
    parent_cell_name: &str,
    base_cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
) -> bz_error::Result<()> {
    let resolved_extension = bzlmod_resolve_extension(
        parent_cell_name,
        usage,
        base_cell_aliases_by_cell,
        extension_unique_names,
    )?;
    let repo_override_targets = bzlmod_extension_repo_override_targets(
        usage,
        parent_cell_name,
        base_cell_aliases_by_cell,
        &resolved_extension,
    )?;
    for import in &usage.imports {
        if let Some(target_cell_name) = repo_override_targets.get(&import.repo_name) {
            add_bzlmod_cell_alias(
                cell_aliases_by_cell,
                parent_cell_name,
                &import.alias,
                target_cell_name,
            );
            continue;
        }
        let canonical_repo_name =
            bzlmod_extension_repo_canonical_repo_name(&resolved_extension, &import.repo_name);
        let target_cell_name = bzlmod_cell_name(&canonical_repo_name);
        add_bzlmod_cell_alias(
            cell_aliases_by_cell,
            parent_cell_name,
            &import.alias,
            &target_cell_name,
        );
    }
    Ok(())
}

fn bzlmod_module_inspection_value(
    dep_graph: &BzlmodDepGraph,
) -> bz_error::Result<BzlmodModuleInspectionValue> {
    let mut modules = Vec::new();
    let mut modules_index = BTreeMap::<String, BTreeSet<String>>::new();
    let mut extension_to_repo_internal_names =
        BTreeMap::<BzlmodExtensionId, BTreeSet<String>>::new();

    modules.push(BzlmodInspectionModule {
        name: dep_graph.root_module.name.clone(),
        version: dep_graph.root_module.version.clone(),
        canonical_repo_name: dep_graph.root_module.canonical_repo_name.clone(),
        selected: true,
        deps: Vec::new(),
    });
    modules_index
        .entry(dep_graph.root_module.name.clone())
        .or_default()
        .insert(dep_graph.root_module.version.clone());
    for usage in &dep_graph.root_module.extension_usages {
        bzlmod_add_inspection_extension_repo_names(
            usage,
            "root",
            dep_graph,
            &mut extension_to_repo_internal_names,
        )?;
    }

    for (key, module) in &dep_graph.discovered {
        let selected = dep_graph.selected_keys.contains(key);
        let canonical_repo_name = dep_graph
            .canonical_repo_names_by_key
            .get(key)
            .cloned()
            .unwrap_or_else(|| {
                bzlmod_canonical_repo_name(&module.dep.name, &module.dep.version, false)
            });
        modules.push(BzlmodInspectionModule {
            name: module.dep.name.clone(),
            version: module.dep.version.clone(),
            canonical_repo_name: canonical_repo_name.clone(),
            selected,
            deps: module.deps.clone(),
        });
        modules_index
            .entry(module.dep.name.clone())
            .or_default()
            .insert(module.dep.version.clone());

        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        for usage in &module.extension_usages {
            bzlmod_add_inspection_extension_repo_names(
                usage,
                &module_cell_name,
                dep_graph,
                &mut extension_to_repo_internal_names,
            )?;
        }
    }

    modules.sort_unstable_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.version.cmp(&b.version))
            .then_with(|| a.canonical_repo_name.cmp(&b.canonical_repo_name))
    });
    Ok(BzlmodModuleInspectionValue {
        modules,
        modules_index,
        extension_to_repo_internal_names,
        module_key_to_canonical_repo_name: dep_graph.canonical_repo_names_by_key.clone(),
        errors: Vec::new(),
    })
}

fn bzlmod_add_inspection_extension_repo_names(
    usage: &BzlmodExtensionUsage,
    parent_cell_name: &str,
    dep_graph: &BzlmodDepGraph,
    extension_to_repo_internal_names: &mut BTreeMap<BzlmodExtensionId, BTreeSet<String>>,
) -> bz_error::Result<()> {
    let resolved_extension = bzlmod_resolve_extension(
        parent_cell_name,
        usage,
        &dep_graph.cell_aliases_by_cell,
        &dep_graph.extension_unique_names,
    )?;
    let repo_names = extension_to_repo_internal_names
        .entry(resolved_extension.id.clone())
        .or_default();
    repo_names.extend(usage.imports.iter().map(|import| import.repo_name.clone()));
    repo_names.extend(bzlmod_extension_tag_repo_names(usage));
    repo_names.extend(
        usage
            .repo_overrides
            .iter()
            .filter(|repo_override| repo_override.must_exist)
            .map(|repo_override| repo_override.repo_name.clone()),
    );
    let lockfile_extension_key = bzlmod_lockfile_extension_key(
        &resolved_extension.id,
        &dep_graph.canonical_repo_names_by_cell,
    )?;
    if let Some(lockfile_repo_names) = dep_graph
        .root_module
        .lockfile_extension_generated_repos
        .get(&lockfile_extension_key)
    {
        repo_names.extend(lockfile_repo_names.iter().cloned());
    }
    Ok(())
}

struct GeneratedBzlmodReposResolution {
    external_modules: Vec<BazelCompatExternalModule>,
}

fn bzlmod_host_platform_setup_from_constraints(raw: Option<&str>) -> BzlmodHostPlatformSetup {
    let mut setup = BzlmodHostPlatformSetup::default();
    for constraint in raw.into_iter().flat_map(str::lines).map(str::trim) {
        if let Some(cpu) = constraint.strip_prefix("@platforms//cpu:") {
            setup.cpu_constraint = Some(Arc::from(cpu));
        } else if let Some(os) = constraint.strip_prefix("@platforms//os:") {
            setup.os_constraint = Some(Arc::from(os));
        }
    }
    setup
}

async fn bzlmod_host_platform_setup_from_config(
    ctx: &mut DiceComputations<'_>,
) -> bz_error::Result<BzlmodHostPlatformSetup> {
    let root_cell = ctx.get_cell_resolver().await?.root_cell();
    let constraints = ctx
        .get_legacy_config_property(
            root_cell,
            BuckconfigKeyRef {
                section: "bazel",
                property: BAZEL_HOST_PLATFORM_CONSTRAINTS,
            },
        )
        .await?;
    Ok(bzlmod_host_platform_setup_from_constraints(
        constraints.as_deref(),
    ))
}

fn resolve_generated_bzlmod_repos(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys_in_dependency_order: &[(String, String)],
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
    extension_usages_json_by_id: &BTreeMap<BzlmodExtensionId, String>,
    host_platform_setup: &BzlmodHostPlatformSetup,
) -> bz_error::Result<GeneratedBzlmodReposResolution> {
    let mut generated = Vec::new();
    let mut generated_repo_declaring_cells = Vec::new();
    let mut extension_generated_repo_groups = BTreeMap::<String, Vec<(String, String)>>::new();
    let mut extension_repo_override_groups = BTreeMap::<String, Vec<(String, String)>>::new();
    resolve_bzlmod_use_repo_rule_generated_repos(
        &root_module.use_repo_rule_invocations,
        &root_module.canonical_repo_name,
        "root",
        true,
        cell_aliases_by_cell,
        &mut generated,
        &mut generated_repo_declaring_cells,
    )?;
    let local_config_xcode_generator_json = serde_json::to_string(
        &BzlmodGeneratedCellGenerator::XcodeConfig(BzlmodXcodeConfigSetup {}),
    )
    .buck_error_context("Error serializing generated Xcode config repo configuration")?;
    let local_config_xcode_canonical_repo_name =
        "bazel_tools+xcode_configure_extension+local_config_xcode";
    let local_config_xcode_cell = add_unimported_generated_bzlmod_repo(
        &mut generated,
        &mut generated_repo_declaring_cells,
        "bazel_tools",
        local_config_xcode_canonical_repo_name,
        local_config_xcode_generator_json,
    );
    add_bzlmod_cell_alias(
        cell_aliases_by_cell,
        "root",
        "local_config_xcode",
        &local_config_xcode_cell,
    );
    add_bzlmod_cell_alias(
        cell_aliases_by_cell,
        "bazel_tools",
        "local_config_xcode",
        &local_config_xcode_cell,
    );
    for key in selected_keys_in_dependency_order {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let parent_canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let parent_cell_name =
            bzlmod_cell_name_for_canonical_repo_name(&parent_canonical_repo_name);
        resolve_bzlmod_use_repo_rule_generated_repos(
            &module.use_repo_rule_invocations,
            &parent_canonical_repo_name,
            &parent_cell_name,
            false,
            cell_aliases_by_cell,
            &mut generated,
            &mut generated_repo_declaring_cells,
        )?;
        // These built-ins are normally emitted by module extensions. Keep
        // static placeholders for known imports so the cell graph can stay
        // demand-driven and defer real extension evaluation until a generated
        // repo is materialized.
        if module.dep.name == "rules_shell" {
            for alias in &module.use_repo_aliases {
                if alias != "local_config_shell" {
                    continue;
                }
                let canonical_repo_name =
                    format!("{parent_canonical_repo_name}+sh_configure+{alias}");
                let generator_json = serde_json::to_string(
                    &BzlmodGeneratedCellGenerator::ShellConfig(BzlmodShellConfigSetup {}),
                )
                .buck_error_context(
                    "Error serializing generated rules_shell configure repo configuration",
                )?;
                add_generated_bzlmod_repo(
                    &mut generated,
                    &mut generated_repo_declaring_cells,
                    cell_aliases_by_cell,
                    &parent_cell_name,
                    alias,
                    &canonical_repo_name,
                    generator_json,
                );
            }
        }

        if module.dep.name == "platforms" {
            for usage in module
                .extension_usages
                .iter()
                .filter(|usage| usage.extension_name == "host_platform")
            {
                let resolved_extension = bzlmod_resolve_extension(
                    &parent_cell_name,
                    usage,
                    cell_aliases_by_cell,
                    extension_unique_names,
                )?;
                for import in &usage.imports {
                    if import.repo_name != "host_platform" {
                        continue;
                    }
                    let canonical_repo_name = format!(
                        "{}+host_platform+{}",
                        parent_canonical_repo_name, import.repo_name
                    );
                    let generator_json = serde_json::to_string(
                        &BzlmodGeneratedCellGenerator::HostPlatform(host_platform_setup.dupe()),
                    )
                    .buck_error_context(
                        "Error serializing generated host_platform repo configuration",
                    )?;
                    let generated_cell_name = add_generated_bzlmod_repo(
                        &mut generated,
                        &mut generated_repo_declaring_cells,
                        cell_aliases_by_cell,
                        &parent_cell_name,
                        &import.alias,
                        &canonical_repo_name,
                        generator_json,
                    );
                    extension_generated_repo_groups
                        .entry(resolved_extension.unique_name.clone())
                        .or_default()
                        .push((import.repo_name.clone(), generated_cell_name));
                }
            }
        }

        if module.dep.name == "bazel_features" {
            for import in
                bzlmod_extension_imports_from_usages(&module.extension_usages, "version_extension")
            {
                let generator = match import.repo_name.as_str() {
                    "bazel_features_globals" => {
                        Some(BzlmodGeneratedCellGenerator::BazelFeaturesGlobals(
                            BzlmodBazelFeaturesGlobalsSetup {
                                parent_canonical_repo_name: Arc::from(
                                    parent_canonical_repo_name.clone(),
                                ),
                                bazel_version: Arc::from(BZLMOD_BAZEL_COMPAT_VERSION),
                            },
                        ))
                    }
                    "bazel_features_version" => {
                        Some(BzlmodGeneratedCellGenerator::BazelFeaturesVersion(
                            BzlmodBazelFeaturesVersionSetup {
                                bazel_version: Arc::from(BZLMOD_BAZEL_COMPAT_VERSION),
                            },
                        ))
                    }
                    _ => None,
                };
                let Some(generator) = generator else {
                    continue;
                };
                let canonical_repo_name = format!(
                    "{}+version_extension+{}",
                    parent_canonical_repo_name, import.repo_name
                );
                let generator_json = serde_json::to_string(&generator).buck_error_context(
                    "Error serializing generated bazel_features repo configuration",
                )?;
                add_generated_bzlmod_repo(
                    &mut generated,
                    &mut generated_repo_declaring_cells,
                    cell_aliases_by_cell,
                    &parent_cell_name,
                    &import.alias,
                    &canonical_repo_name,
                    generator_json,
                );
            }
        }

        for usage in &module.extension_usages {
            resolve_bzlmod_extension_usage_generated_repos(
                usage,
                &parent_canonical_repo_name,
                &parent_cell_name,
                false,
                root_module,
                cell_aliases_by_cell,
                canonical_repo_names_by_cell,
                extension_unique_names,
                extension_usages_json_by_id,
                &mut generated,
                &mut generated_repo_declaring_cells,
                &mut extension_generated_repo_groups,
                &mut extension_repo_override_groups,
            )?;
        }
    }

    for usage in &root_module.extension_usages {
        resolve_bzlmod_extension_usage_generated_repos(
            usage,
            &root_module.canonical_repo_name,
            "root",
            true,
            root_module,
            cell_aliases_by_cell,
            canonical_repo_names_by_cell,
            extension_unique_names,
            extension_usages_json_by_id,
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut extension_generated_repo_groups,
            &mut extension_repo_override_groups,
        )?;
    }

    add_generated_bzlmod_repo_mappings(
        cell_aliases_by_cell,
        &generated_repo_declaring_cells,
        &extension_generated_repo_groups,
        &extension_repo_override_groups,
    );
    Ok(GeneratedBzlmodReposResolution {
        external_modules: generated,
    })
}

fn resolve_bzlmod_extension_usage_generated_repos(
    usage: &BzlmodExtensionUsage,
    parent_canonical_repo_name: &str,
    parent_cell_name: &str,
    parent_is_root: bool,
    root_module: &RootBzlmodModule,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
    extension_usages_json_by_id: &BTreeMap<BzlmodExtensionId, String>,
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    extension_generated_repo_groups: &mut BTreeMap<String, Vec<(String, String)>>,
    extension_repo_override_groups: &mut BTreeMap<String, Vec<(String, String)>>,
) -> bz_error::Result<()> {
    let resolved_extension = bzlmod_resolve_extension(
        parent_cell_name,
        usage,
        cell_aliases_by_cell,
        extension_unique_names,
    )?;
    let extension_group_key = resolved_extension.unique_name.clone();
    let extension_host_cell_name = resolved_extension.id.bzl_cell_name.clone();
    let mut existing_generated_repos = extension_generated_repo_groups
        .get(&extension_group_key)
        .map(|generated_repos| {
            generated_repos
                .iter()
                .cloned()
                .collect::<StdBuckHashMap<_, _>>()
        })
        .unwrap_or_default();

    let imports_needing_generic_repos = usage
        .imports
        .iter()
        .filter(|import| {
            bzlmod_cell_alias_target(cell_aliases_by_cell, parent_cell_name, &import.alias)
                .is_none()
        })
        .collect::<Vec<_>>();
    let mut static_repo_names = imports_needing_generic_repos
        .iter()
        .map(|import| import.repo_name.clone())
        .collect::<BTreeSet<_>>();
    static_repo_names.extend(bzlmod_extension_tag_repo_names(usage));
    static_repo_names.extend(
        usage
            .repo_overrides
            .iter()
            .filter(|repo_override| repo_override.must_exist)
            .map(|repo_override| repo_override.repo_name.clone()),
    );
    let lockfile_extension_key =
        bzlmod_lockfile_extension_key(&resolved_extension.id, canonical_repo_names_by_cell)?;
    if let Some(lockfile_repo_names) = root_module
        .lockfile_extension_generated_repos
        .get(&lockfile_extension_key)
    {
        static_repo_names.extend(lockfile_repo_names.iter().cloned());
    }
    let repo_override_targets = bzlmod_extension_repo_override_targets(
        usage,
        parent_cell_name,
        cell_aliases_by_cell,
        &resolved_extension,
    )?;
    if !repo_override_targets.is_empty() {
        extension_repo_override_groups
            .entry(extension_group_key.clone())
            .or_default()
            .extend(
                repo_override_targets
                    .iter()
                    .map(|(repo_name, target_cell_name)| {
                        (repo_name.clone(), target_cell_name.clone())
                    }),
            );
    }
    let extension_usages_json = extension_usages_json_by_id
        .get(&resolved_extension.id)
        .ok_or_else(|| {
            bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod module extension `{}//{}%{}` has no single-extension usages value",
                resolved_extension.id.bzl_cell_name,
                resolved_extension.id.bzl_path,
                resolved_extension.id.extension_name
            )
        })?;
    if static_repo_names.is_empty() {
        return Ok(());
    }
    let mut generated_repo_names = static_repo_names;

    for import in imports_needing_generic_repos {
        if let Some(target_cell_name) = repo_override_targets.get(&import.repo_name) {
            if usage.repo_overrides.iter().any(|repo_override| {
                repo_override.repo_name == import.repo_name && !repo_override.must_exist
            }) {
                return Err(bz_error!(
                    bz_error::ErrorTag::Input,
                    "bzlmod module extension `{}`%`{}` for `{}` imports injected repository `{}`; refer to `{}` directly",
                    usage.extension_bzl_file,
                    usage.extension_name,
                    parent_canonical_repo_name,
                    import.repo_name,
                    target_cell_name
                ));
            }
            add_bzlmod_cell_alias(
                cell_aliases_by_cell,
                parent_cell_name,
                &import.alias,
                target_cell_name,
            );
            continue;
        }

        if let Some(generated_cell_name) = existing_generated_repos.get(&import.repo_name) {
            add_bzlmod_cell_alias(
                cell_aliases_by_cell,
                parent_cell_name,
                &import.alias,
                generated_cell_name,
            );
            generated_repo_names.remove(&import.repo_name);
            continue;
        }

        let canonical_repo_name =
            bzlmod_extension_repo_canonical_repo_name(&resolved_extension, &import.repo_name);
        let generator_json = serde_json::to_string(&bzlmod_module_extension_repo_config(
            &resolved_extension,
            parent_canonical_repo_name,
            parent_is_root,
            usage,
            &import.repo_name,
            &extension_usages_json,
        )?)
        .buck_error_context("Error serializing generated module extension repo configuration")?;
        let generated_cell_name = add_generated_bzlmod_repo_with_mapping_cell(
            generated,
            generated_repo_declaring_cells,
            cell_aliases_by_cell,
            parent_cell_name,
            &import.alias,
            &extension_host_cell_name,
            &canonical_repo_name,
            generator_json,
        );
        extension_generated_repo_groups
            .entry(extension_group_key.clone())
            .or_default()
            .push((import.repo_name.clone(), generated_cell_name.clone()));
        existing_generated_repos.insert(import.repo_name.clone(), generated_cell_name);
        generated_repo_names.remove(&import.repo_name);
    }

    for repo_name in generated_repo_names {
        if existing_generated_repos.contains_key(&repo_name) {
            continue;
        }
        let canonical_repo_name =
            bzlmod_extension_repo_canonical_repo_name(&resolved_extension, &repo_name);
        let generator_json = serde_json::to_string(&bzlmod_module_extension_repo_config(
            &resolved_extension,
            parent_canonical_repo_name,
            parent_is_root,
            usage,
            &repo_name,
            &extension_usages_json,
        )?)
        .buck_error_context("Error serializing generated module extension repo configuration")?;
        let generated_cell_name = add_unimported_generated_bzlmod_repo(
            generated,
            generated_repo_declaring_cells,
            &extension_host_cell_name,
            &canonical_repo_name,
            generator_json,
        );
        extension_generated_repo_groups
            .entry(extension_group_key.clone())
            .or_default()
            .push((repo_name.clone(), generated_cell_name.clone()));
        existing_generated_repos.insert(repo_name, generated_cell_name);
    }

    Ok(())
}

fn bzlmod_extension_repo_override_targets(
    usage: &BzlmodExtensionUsage,
    parent_cell_name: &str,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    resolved_extension: &BzlmodResolvedExtension,
) -> bz_error::Result<BTreeMap<String, String>> {
    let mut targets = BTreeMap::new();
    for repo_override in &usage.repo_overrides {
        let Some(target_cell_name) = bzlmod_cell_alias_target(
            cell_aliases_by_cell,
            parent_cell_name,
            &repo_override.overriding_repo_name,
        ) else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod module extension `{}//{}%{}` maps repository `{}` to `{}`, but `{}` is not visible from `{}`",
                resolved_extension.id.bzl_cell_name,
                resolved_extension.id.bzl_path,
                resolved_extension.id.extension_name,
                repo_override.repo_name,
                repo_override.overriding_repo_name,
                repo_override.overriding_repo_name,
                parent_cell_name
            ));
        };
        targets.insert(repo_override.repo_name.clone(), target_cell_name.to_owned());
    }
    Ok(targets)
}

fn add_bzlmod_module_extension_repo_overrides_for_usage(
    usage: &BzlmodExtensionUsage,
    parent_cell_name: &str,
    dep_graph: &BzlmodDepGraph,
    extension_id: &BzlmodExtensionId,
    targets: &mut BTreeMap<String, String>,
) -> bz_error::Result<()> {
    let resolved_extension = bzlmod_resolve_extension(
        parent_cell_name,
        usage,
        &dep_graph.cell_aliases_by_cell,
        &dep_graph.extension_unique_names,
    )?;
    if &resolved_extension.id != extension_id {
        return Ok(());
    }
    targets.extend(bzlmod_extension_repo_override_targets(
        usage,
        parent_cell_name,
        &dep_graph.cell_aliases_by_cell,
        &resolved_extension,
    )?);
    Ok(())
}

fn bzlmod_module_extension_repo_overrides_for_extension(
    dep_graph: &BzlmodDepGraph,
    extension_id: &BzlmodExtensionId,
) -> bz_error::Result<Vec<(String, String)>> {
    let mut targets = BTreeMap::new();
    for usage in &dep_graph.root_module.extension_usages {
        add_bzlmod_module_extension_repo_overrides_for_usage(
            usage,
            "root",
            dep_graph,
            extension_id,
            &mut targets,
        )?;
    }
    for key in &dep_graph.selected_keys {
        let Some(module) = dep_graph.discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            &dep_graph.canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        for usage in &module.extension_usages {
            add_bzlmod_module_extension_repo_overrides_for_usage(
                usage,
                &module_cell_name,
                dep_graph,
                extension_id,
                &mut targets,
            )?;
        }
    }
    Ok(targets.into_iter().collect())
}

fn bzlmod_module_extension_evaluation_config_json(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys_in_bfs_order: &[(String, String)],
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    extension_id: &BzlmodExtensionId,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
) -> bz_error::Result<String> {
    let mut modules = Vec::new();
    let mut usages = Vec::new();
    let mut repo_overrides = BTreeMap::new();
    let mut root_has_usage = false;
    let mut root_module_has_non_dev_dependency = false;
    let mut root_extension_bzl_file = None;
    let mut root_extension_name = None;
    let mut root_tags = Vec::new();
    for usage in &root_module.extension_usages {
        let resolved_extension =
            bzlmod_resolve_extension("root", usage, cell_aliases_by_cell, extension_unique_names)?;
        if &resolved_extension.id != extension_id {
            continue;
        }
        root_has_usage = true;
        root_extension_bzl_file.get_or_insert_with(|| usage.extension_bzl_file.clone());
        root_extension_name.get_or_insert_with(|| usage.extension_name.clone());
        root_module_has_non_dev_dependency |= !usage.dev_dependency;
        usages.push(BzlmodModuleExtensionUsageConfig {
            imports: usage.imports.clone(),
            repo_overrides: usage.repo_overrides.clone(),
        });
        for (repo_name, target_cell_name) in bzlmod_extension_repo_override_targets(
            usage,
            "root",
            cell_aliases_by_cell,
            &resolved_extension,
        )? {
            repo_overrides.insert(
                repo_name,
                bzlmod_canonical_repo_name_for_cell_name(
                    &target_cell_name,
                    canonical_repo_names_by_cell,
                )?,
            );
        }
        root_tags.extend(usage.tags.iter().map(|tag| BzlmodModuleExtensionTagConfig {
            tag_name: tag.tag_name.clone(),
            dev_dependency: usage.dev_dependency,
            bindings: tag.bindings.clone(),
            kwargs: tag.kwargs.clone(),
        }));
    }
    if root_has_usage {
        modules.push(BzlmodModuleExtensionModuleConfig {
            name: root_module.name.clone(),
            version: root_module.version.clone(),
            canonical_repo_name: root_module.canonical_repo_name.clone(),
            is_root: true,
            extension_bzl_file: root_extension_bzl_file.unwrap_or_default(),
            extension_name: root_extension_name.unwrap_or_default(),
            cell_aliases: bzlmod_module_extension_cell_aliases(cell_aliases_by_cell, "root"),
            constants: root_module.constants.clone(),
            tags: root_tags,
        });
    }

    for key in selected_keys_in_bfs_order {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        let mut has_usage = false;
        let mut extension_bzl_file = None;
        let mut extension_name = None;
        let mut tags = Vec::new();
        for usage in &module.extension_usages {
            let resolved_extension = bzlmod_resolve_extension(
                &module_cell_name,
                usage,
                cell_aliases_by_cell,
                extension_unique_names,
            )?;
            if &resolved_extension.id != extension_id {
                continue;
            }
            has_usage = true;
            extension_bzl_file.get_or_insert_with(|| usage.extension_bzl_file.clone());
            extension_name.get_or_insert_with(|| usage.extension_name.clone());
            usages.push(BzlmodModuleExtensionUsageConfig {
                imports: usage.imports.clone(),
                repo_overrides: usage.repo_overrides.clone(),
            });
            for (repo_name, target_cell_name) in bzlmod_extension_repo_override_targets(
                usage,
                &module_cell_name,
                cell_aliases_by_cell,
                &resolved_extension,
            )? {
                repo_overrides.insert(
                    repo_name,
                    bzlmod_canonical_repo_name_for_cell_name(
                        &target_cell_name,
                        canonical_repo_names_by_cell,
                    )?,
                );
            }
            tags.extend(usage.tags.iter().map(|tag| BzlmodModuleExtensionTagConfig {
                tag_name: tag.tag_name.clone(),
                dev_dependency: usage.dev_dependency,
                bindings: tag.bindings.clone(),
                kwargs: tag.kwargs.clone(),
            }));
        }
        if !has_usage {
            continue;
        }
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        modules.push(BzlmodModuleExtensionModuleConfig {
            name: module.dep.name.clone(),
            version: module.dep.version.clone(),
            canonical_repo_name,
            is_root: false,
            extension_bzl_file: extension_bzl_file.unwrap_or_default(),
            extension_name: extension_name.unwrap_or_default(),
            cell_aliases: bzlmod_module_extension_cell_aliases(
                cell_aliases_by_cell,
                &module_cell_name,
            ),
            constants: module.constants.clone(),
            tags,
        });
    }

    serde_json::to_string(&BzlmodModuleExtensionEvaluationConfig {
        root_module_has_non_dev_dependency,
        modules,
        usages,
        repo_overrides: repo_overrides.into_iter().collect(),
    })
    .buck_error_context("Error serializing module extension evaluation configuration")
}

fn bzlmod_canonical_repo_name_for_cell_name(
    cell_name: &str,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
) -> bz_error::Result<String> {
    canonical_repo_names_by_cell
        .get(cell_name)
        .cloned()
        .ok_or_else(|| {
            bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod cell `{}` does not have a canonical repository name",
                cell_name
            )
        })
}

fn bzlmod_module_extension_cell_aliases(
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    cell_name: &str,
) -> Vec<(String, String)> {
    let mut aliases = cell_aliases_by_cell
        .get(cell_name)
        .into_iter()
        .flat_map(|aliases| {
            aliases
                .iter()
                .map(|(alias, target)| (alias.clone(), target.clone()))
        })
        .collect::<Vec<_>>();
    aliases.sort_unstable();
    aliases.dedup();
    aliases
}

fn bzlmod_module_extension_repo_config(
    resolved_extension: &BzlmodResolvedExtension,
    parent_canonical_repo_name: &str,
    parent_is_root: bool,
    usage: &BzlmodExtensionUsage,
    repo_name: &str,
    extension_usages_json: &str,
) -> bz_error::Result<BzlmodGeneratedCellGenerator> {
    let extension_usages_key = register_bzlmod_module_extension_usages_json(extension_usages_json);
    Ok(BzlmodGeneratedCellGenerator::ModuleExtensionRepo(
        BzlmodModuleExtensionRepoSetup {
            parent_canonical_repo_name: Arc::from(parent_canonical_repo_name),
            parent_is_root,
            extension_bzl_file: Arc::from(usage.extension_bzl_file.clone()),
            extension_bzl_cell: Arc::from(resolved_extension.id.bzl_cell_name.clone()),
            extension_bzl_path: Arc::from(resolved_extension.id.bzl_path.clone()),
            extension_unique_name: Arc::from(resolved_extension.unique_name.clone()),
            extension_name: Arc::from(usage.extension_name.clone()),
            repo_name: Arc::from(repo_name),
            extension_usages_key,
            extension_usages_json: Arc::from(extension_usages_json),
        },
    ))
}

fn add_generated_bzlmod_repo(
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    declaring_cell_name: &str,
    alias: &str,
    canonical_repo_name: &str,
    generator_json: String,
) -> String {
    add_generated_bzlmod_repo_with_mapping_cell(
        generated,
        generated_repo_declaring_cells,
        cell_aliases_by_cell,
        declaring_cell_name,
        alias,
        declaring_cell_name,
        canonical_repo_name,
        generator_json,
    )
}

fn add_generated_bzlmod_repo_with_mapping_cell(
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    importing_cell_name: &str,
    alias: &str,
    mapping_cell_name: &str,
    canonical_repo_name: &str,
    generator_json: String,
) -> String {
    let cell_name = bzlmod_cell_name(canonical_repo_name);
    add_bzlmod_cell_alias(cell_aliases_by_cell, importing_cell_name, alias, &cell_name);
    add_unimported_generated_bzlmod_repo(
        generated,
        generated_repo_declaring_cells,
        mapping_cell_name,
        canonical_repo_name,
        generator_json,
    )
}

fn add_unimported_generated_bzlmod_repo(
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
    declaring_cell_name: &str,
    canonical_repo_name: &str,
    generator_json: String,
) -> String {
    let cell_name = bzlmod_cell_name(canonical_repo_name);
    generated_repo_declaring_cells.push((cell_name.clone(), declaring_cell_name.to_owned()));
    generated.push(BazelCompatExternalModule::Generated(
        BazelCompatGeneratedModule {
            cell_name: cell_name.clone(),
            aliases: Vec::new(),
            canonical_repo_name: canonical_repo_name.to_owned(),
            generator_json,
        },
    ));
    cell_name
}

fn add_generated_bzlmod_repo_mappings(
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    generated_repo_declaring_cells: &[(String, String)],
    extension_generated_repo_groups: &BTreeMap<String, Vec<(String, String)>>,
    extension_repo_override_groups: &BTreeMap<String, Vec<(String, String)>>,
) {
    let mut mapped_declaring_cells = BTreeSet::new();
    for (generated_cell_name, declaring_cell_name) in generated_repo_declaring_cells {
        if !mapped_declaring_cells
            .insert((generated_cell_name.clone(), declaring_cell_name.clone()))
        {
            continue;
        }
        let Some(declaring_aliases) = cell_aliases_by_cell.get(declaring_cell_name).cloned() else {
            continue;
        };
        cell_aliases_by_cell
            .entry(generated_cell_name.clone())
            .or_default()
            .extend(declaring_aliases);
    }

    for (extension_group_key, generated_repos) in extension_generated_repo_groups {
        let generated_repos = generated_repos
            .iter()
            .cloned()
            .collect::<StdBuckHashMap<_, _>>();
        let mut visible_repos = generated_repos.clone();
        if let Some(repo_overrides) = extension_repo_override_groups.get(extension_group_key) {
            visible_repos.extend(repo_overrides.iter().cloned());
        }
        for generated_cell_name in generated_repos.values() {
            for (repo_name, target_cell_name) in &visible_repos {
                add_bzlmod_cell_alias(
                    cell_aliases_by_cell,
                    generated_cell_name,
                    repo_name,
                    target_cell_name,
                );
            }
        }
    }
}

fn resolve_bzlmod_use_repo_rule_generated_repos(
    invocations: &[BzlmodUseRepoRuleInvocation],
    parent_canonical_repo_name: &str,
    parent_cell_name: &str,
    parent_is_root: bool,
    cell_aliases_by_cell: &mut BzlmodCellAliasesByCell,
    generated: &mut Vec<BazelCompatExternalModule>,
    generated_repo_declaring_cells: &mut Vec<(String, String)>,
) -> bz_error::Result<()> {
    for invocation in invocations {
        let (rule_bzl_cell, rule_bzl_path) = bzlmod_resolve_extension_bzl_label(
            parent_cell_name,
            &invocation.rule_bzl_file,
            cell_aliases_by_cell,
        )?;
        let canonical_repo_name = bzlmod_use_repo_rule_canonical_repo_name(
            parent_canonical_repo_name,
            parent_is_root,
            &invocation.rule_name,
            &invocation.repo_name,
        );
        let generator_json =
            serde_json::to_string(&BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(
                BzlmodRepositoryRuleInvocationSetup {
                    repo_name: Arc::from(invocation.repo_name.clone()),
                    rule_bzl_cell: Arc::from(rule_bzl_cell),
                    rule_bzl_path: Arc::from(rule_bzl_path),
                    rule_bzl_build_file_cell: Arc::from(parent_cell_name),
                    rule_bzl_build_file_package: Some(Arc::from("")),
                    rule_name: Arc::from(invocation.rule_name.clone()),
                    attrs: Arc::new(
                        invocation
                            .attrs
                            .iter()
                            .map(|(key, value)| (Arc::from(key.clone()), Arc::from(value.clone())))
                            .collect(),
                    ),
                },
            ))
            .buck_error_context("Error serializing use_repo_rule repository configuration")?;
        add_generated_bzlmod_repo(
            generated,
            generated_repo_declaring_cells,
            cell_aliases_by_cell,
            parent_cell_name,
            &invocation.repo_name,
            &canonical_repo_name,
            generator_json,
        );
    }
    Ok(())
}

fn bzlmod_use_repo_rule_canonical_repo_name(
    parent_canonical_repo_name: &str,
    parent_is_root: bool,
    rule_name: &str,
    repo_name: &str,
) -> String {
    let extension_unique_name = if parent_is_root {
        format!("+{rule_name}")
    } else {
        format!("{parent_canonical_repo_name}+{rule_name}")
    };
    bzlmod_extension_unique_repo_canonical_repo_name(&extension_unique_name, repo_name)
}

fn bzlmod_extension_tag_repo_names(usage: &BzlmodExtensionUsage) -> Vec<String> {
    let mut repo_names = usage
        .tags
        .iter()
        .flat_map(|tag| tag.kwargs.iter())
        .filter_map(|(name, value)| {
            if name == "name" || name == "repo_name" {
                bzlmod_string_literal_prefix(value.trim())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    repo_names.sort_unstable();
    repo_names.dedup();
    repo_names
}

fn bzlmod_string_literal_prefix(value: &str) -> Option<String> {
    let mut chars = value.chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    let mut result = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            result.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some(result);
        } else {
            result.push(ch);
        }
    }

    None
}

fn bzlmod_extension_unique_names(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &BTreeSet<(String, String)>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
) -> bz_error::Result<BTreeMap<BzlmodExtensionId, String>> {
    let mut extension_ids = BTreeSet::new();
    for usage in &root_module.extension_usages {
        extension_ids.insert(bzlmod_resolve_extension_id(
            "root",
            usage,
            cell_aliases_by_cell,
        )?);
    }
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let module_cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        for usage in &module.extension_usages {
            extension_ids.insert(bzlmod_resolve_extension_id(
                &module_cell_name,
                usage,
                cell_aliases_by_cell,
            )?);
        }
    }

    let mut used_names = BTreeSet::new();
    let mut unique_names = BTreeMap::new();
    for extension_id in extension_ids {
        let Some(extension_repo_name) =
            canonical_repo_names_by_cell.get(&extension_id.bzl_cell_name)
        else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod module extension `{}//{}%{}` resolves to unknown cell `{}`",
                extension_id.bzl_cell_name,
                extension_id.bzl_path,
                extension_id.extension_name,
                extension_id.bzl_cell_name
            ));
        };
        let mut attempt = 1;
        loop {
            let disambiguator = if attempt == 1 {
                String::new()
            } else {
                attempt.to_string()
            };
            let candidate = format!(
                "{}+{}{}",
                extension_repo_name, extension_id.extension_name, disambiguator
            );
            if used_names.insert(candidate.clone()) {
                unique_names.insert(extension_id, candidate);
                break;
            }
            attempt += 1;
        }
    }
    Ok(unique_names)
}

fn bzlmod_resolve_extension(
    current_cell_name: &str,
    usage: &BzlmodExtensionUsage,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
    extension_unique_names: &BTreeMap<BzlmodExtensionId, String>,
) -> bz_error::Result<BzlmodResolvedExtension> {
    let id = bzlmod_resolve_extension_id(current_cell_name, usage, cell_aliases_by_cell)?;
    let Some(unique_name) = extension_unique_names.get(&id) else {
        return Err(bz_error!(
            bz_error::ErrorTag::Input,
            "bzlmod module extension `{}`%`{}` in cell `{}` was not assigned a unique name",
            usage.extension_bzl_file,
            usage.extension_name,
            current_cell_name
        ));
    };
    Ok(BzlmodResolvedExtension {
        id,
        unique_name: unique_name.clone(),
    })
}

fn bzlmod_resolve_extension_id(
    current_cell_name: &str,
    usage: &BzlmodExtensionUsage,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
) -> bz_error::Result<BzlmodExtensionId> {
    let (bzl_cell_name, bzl_path) = bzlmod_resolve_extension_bzl_label(
        current_cell_name,
        &usage.extension_bzl_file,
        cell_aliases_by_cell,
    )?;
    Ok(BzlmodExtensionId {
        bzl_cell_name,
        bzl_path,
        extension_name: usage.extension_name.clone(),
    })
}

fn bzlmod_resolve_extension_bzl_label(
    current_cell_name: &str,
    label: &str,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
) -> bz_error::Result<(String, String)> {
    if let Some(rest) = label.strip_prefix("@@") {
        let Some((canonical_repo_name, package_and_target)) = rest.split_once("//") else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod module extension label `{}` is not an absolute label",
                label
            ));
        };
        let cell_name = if canonical_repo_name == "bazel_tools" {
            "bazel_tools".to_owned()
        } else {
            bzlmod_cell_name(canonical_repo_name)
        };
        return Ok((
            cell_name,
            bzlmod_label_package_target_to_path(label, package_and_target)?,
        ));
    }
    if let Some(rest) = label.strip_prefix('@') {
        let Some((alias, package_and_target)) = rest.split_once("//") else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod module extension label `{}` is not an absolute label",
                label
            ));
        };
        let cell_name = if alias == "bazel_tools" {
            "bazel_tools"
        } else {
            bzlmod_cell_alias_target(cell_aliases_by_cell, current_cell_name, alias).ok_or_else(
                || {
                    bz_error!(
                        bz_error::ErrorTag::Input,
                        "bzlmod module extension label `{}` in cell `{}` references unknown repo `{}`",
                        label,
                        current_cell_name,
                        alias
                    )
                },
            )?
        };
        return Ok((
            cell_name.to_owned(),
            bzlmod_label_package_target_to_path(label, package_and_target)?,
        ));
    }
    if let Some(package_and_target) = label.strip_prefix("//") {
        return Ok((
            current_cell_name.to_owned(),
            bzlmod_label_package_target_to_path(label, package_and_target)?,
        ));
    }
    if let Some(target) = label.strip_prefix(':') {
        return Ok((
            current_cell_name.to_owned(),
            bzlmod_label_package_target_to_path(label, &format!(":{target}"))?,
        ));
    }
    Err(bz_error!(
        bz_error::ErrorTag::Input,
        "bzlmod module extension label `{}` is not an absolute or module-root-relative label",
        label
    ))
}

fn bzlmod_label_package_target_to_path(
    label: &str,
    package_and_target: &str,
) -> bz_error::Result<String> {
    let (package, target) = match package_and_target.split_once(':') {
        Some((package, target)) => (package, target),
        None => {
            let target = package_and_target
                .rsplit('/')
                .next()
                .unwrap_or(package_and_target);
            (package_and_target, target)
        }
    };
    if target.is_empty() {
        return Err(bz_error!(
            bz_error::ErrorTag::Input,
            "bzlmod module extension label `{}` has an empty target name",
            label
        ));
    }
    if package.is_empty() {
        Ok(target.to_owned())
    } else {
        Ok(format!("{package}/{target}"))
    }
}

fn bzlmod_extension_repo_canonical_repo_name(
    extension: &BzlmodResolvedExtension,
    repo_name: &str,
) -> String {
    bzlmod_extension_unique_repo_canonical_repo_name(&extension.unique_name, repo_name)
}

fn bzlmod_extension_unique_repo_canonical_repo_name(
    extension_unique_name: &str,
    repo_name: &str,
) -> String {
    format!("{extension_unique_name}+{repo_name}")
}

fn resolve_bzlmod_registered_toolchains(
    root_module: &RootBzlmodModule,
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    selected_keys: &[(String, String)],
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
) -> bz_error::Result<Vec<String>> {
    let mut registered_toolchains = Vec::new();
    for pattern in &root_module.registered_toolchains {
        registered_toolchains.push(qualify_bzlmod_registered_toolchain(
            pattern,
            "root",
            cell_aliases_by_cell,
        )?);
    }
    for key in selected_keys {
        let Some(module) = discovered.get(key) else {
            continue;
        };
        let canonical_repo_name = bzlmod_selected_canonical_repo_name(
            canonical_repo_names_by_key,
            &module.dep.name,
            &module.dep.version,
        )?;
        let cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
        for pattern in &module.registered_toolchains {
            registered_toolchains.push(qualify_bzlmod_registered_toolchain(
                pattern,
                &cell_name,
                cell_aliases_by_cell,
            )?);
        }
    }
    dedup_preserve_order(&mut registered_toolchains);
    Ok(registered_toolchains)
}

fn qualify_bzlmod_registered_toolchain(
    pattern: &str,
    module_cell_name: &str,
    cell_aliases_by_cell: &BzlmodCellAliasesByCell,
) -> bz_error::Result<String> {
    let pattern = pattern.trim();
    if let Some(rest) = pattern.strip_prefix("//") {
        return Ok(format!("{module_cell_name}//{rest}"));
    }
    if pattern.starts_with("@@") {
        return Ok(pattern.to_owned());
    }
    if let Some(rest) = pattern.strip_prefix('@') {
        let Some((alias, package_and_target)) = rest.split_once("//") else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod registered toolchain pattern `{}` in cell `{}` is not an absolute target pattern",
                pattern,
                module_cell_name
            ));
        };
        if alias.is_empty() {
            return Ok(format!("{module_cell_name}//{package_and_target}"));
        }
        if alias == "bazel_tools" {
            return Ok(format!("bazel_tools//{package_and_target}"));
        }
        let Some(target_cell_name) =
            bzlmod_cell_alias_target(cell_aliases_by_cell, module_cell_name, alias)
        else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod registered toolchain pattern `{}` in cell `{}` references unknown repo `{}`",
                pattern,
                module_cell_name,
                alias
            ));
        };
        return Ok(format!("{target_cell_name}//{package_and_target}"));
    }
    if pattern.contains("//") {
        Ok(pattern.to_owned())
    } else {
        Err(bz_error!(
            bz_error::ErrorTag::Input,
            "bzlmod registered toolchain pattern `{}` in cell `{}` is not an absolute target pattern",
            pattern,
            module_cell_name
        ))
    }
}

fn add_bzlmod_dep_alias(
    dep: &BazelDep,
    selected_versions: &BTreeMap<String, String>,
    aliases_by_key: &mut BTreeMap<(String, String), BTreeSet<String>>,
) {
    let Some(alias) = dep.apparent_name.as_ref() else {
        return;
    };
    let Some(version) = selected_versions.get(&dep.name) else {
        return;
    };
    aliases_by_key
        .entry((dep.name.clone(), version.clone()))
        .or_default()
        .insert(alias.clone());
}

fn add_bzlmod_dep_cell_alias(
    current_cell_name: &str,
    dep: &BazelDep,
    selected_versions: &BTreeMap<String, String>,
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    aliases_by_cell: &mut BzlmodCellAliasesByCell,
) -> bz_error::Result<()> {
    let Some(alias) = dep.apparent_name.as_ref() else {
        return Ok(());
    };
    let Some(version) = selected_versions.get(&dep.name) else {
        return Ok(());
    };
    let canonical_repo_name =
        bzlmod_selected_canonical_repo_name(canonical_repo_names_by_key, &dep.name, version)?;
    let cell_name = bzlmod_cell_name_for_canonical_repo_name(&canonical_repo_name);
    add_bzlmod_cell_alias(aliases_by_cell, current_cell_name, alias, &cell_name);
    Ok(())
}

fn add_bzlmod_cell_alias(
    aliases_by_cell: &mut BzlmodCellAliasesByCell,
    current_cell_name: &str,
    alias: &str,
    target_cell_name: &str,
) {
    aliases_by_cell
        .entry(current_cell_name.to_owned())
        .or_default()
        .insert(alias.to_owned(), target_cell_name.to_owned());
}

fn bzlmod_cell_alias_target<'a>(
    aliases_by_cell: &'a BzlmodCellAliasesByCell,
    current_cell_name: &str,
    alias: &str,
) -> Option<&'a str> {
    aliases_by_cell
        .get(current_cell_name)
        .and_then(|aliases| aliases.get(alias))
        .map(String::as_str)
}

fn bzlmod_cell_alias_map_to_vec(aliases: BzlmodCellAliasMap) -> Vec<BazelCompatCellAlias> {
    aliases
        .into_iter()
        .map(|(alias, cell_name)| BazelCompatCellAlias { alias, cell_name })
        .collect()
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Http)]
enum BcrHttpGetError {
    #[error("Error fetching `{url}`")]
    Fetch {
        url: String,
        #[source]
        source: RetryingHttpError,
    },
}

impl HttpErrorForRetry for BcrHttpGetError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Fetch { source, .. } => source.is_retryable(),
        }
    }
}

impl IntoBuck2Error for BcrHttpGetError {
    fn into_bz_error(self) -> bz_error::Error {
        bz_error::Error::from(self)
    }
}

fn empty_bcr_source_json() -> BcrSourceJson {
    BcrSourceJson {
        url: String::new(),
        urls: None,
        integrity: String::new(),
        strip_prefix: None,
        archive_type: None,
        patches: None,
        overlay: None,
        patch_strip: None,
    }
}

fn bzlmod_archive_override_source_json(archive_override: &BzlmodArchiveOverride) -> BcrSourceJson {
    BcrSourceJson {
        url: bzlmod_archive_override_primary_url(archive_override).to_owned(),
        urls: Some(archive_override.urls.clone()),
        integrity: archive_override.integrity.clone(),
        strip_prefix: archive_override.strip_prefix.clone(),
        archive_type: archive_override.archive_type.clone(),
        patches: None,
        overlay: None,
        patch_strip: archive_override.patch_strip,
    }
}

fn parse_discovered_bzlmod_module(
    dep: BazelDep,
    source_json: BcrSourceJson,
    module_text: String,
) -> bz_error::Result<DiscoveredBcrModule> {
    let compiled =
        compile_bzlmod_module_file(format!("{}@{}", dep.name, dep.version), module_text)?;
    if !compiled.includes.is_empty() {
        return Err(bz_error!(
            bz_error::ErrorTag::Input,
            "registry module `{}@{}` uses include(), but Bazel only allows include() in the root module and non-registry overrides",
            dep.name,
            dep.version
        ));
    }
    let evaluated = eval_bzlmod_module_file(
        &compiled,
        BzlmodModuleEvalOptions {
            is_root: false,
            allow_include: false,
            ignore_dev_dependency: true,
            default_name: dep.name.clone(),
            default_version: dep.version.clone(),
            default_repo_name: dep.name.clone(),
            cell_project_path: None,
            included_modules: BTreeMap::new(),
        },
    )?;
    discovered_bzlmod_module_from_eval(dep, source_json, evaluated)
}

async fn fetch_local_bzlmod_module(
    dep: BazelDep,
    local_path_override: BzlmodLocalPathOverride,
) -> bz_error::Result<DiscoveredBcrModule> {
    let compiled = compile_bzlmod_module_file(
        BAZEL_MODULE_FILE.to_owned(),
        local_path_override.module_text,
    )?;
    let included_modules = local_path_override
        .included_module_texts
        .into_iter()
        .map(|(label, module_text)| {
            let module_file = bzlmod_include_label_to_path(BAZEL_MODULE_FILE, &label)?;
            let compiled = compile_bzlmod_module_file(module_file, module_text)?;
            bz_error::Ok((label, Arc::new(compiled)))
        })
        .collect::<bz_error::Result<BTreeMap<_, _>>>()?;
    let evaluated = eval_bzlmod_module_file(
        &compiled,
        BzlmodModuleEvalOptions {
            is_root: false,
            allow_include: true,
            ignore_dev_dependency: true,
            default_name: dep.name.clone(),
            default_version: dep.version.clone(),
            default_repo_name: dep.name.clone(),
            cell_project_path: None,
            included_modules,
        },
    )?;
    discovered_bzlmod_module_from_eval(dep, empty_bcr_source_json(), evaluated)
}

fn builtin_bazel_tools_module() -> bz_error::Result<DiscoveredBcrModule> {
    let dep = BazelDep {
        name: "bazel_tools".to_owned(),
        version: String::new(),
        apparent_name: Some("bazel_tools".to_owned()),
    };
    parse_discovered_bzlmod_module(
        dep,
        BcrSourceJson {
            url: String::new(),
            urls: None,
            integrity: String::new(),
            strip_prefix: None,
            archive_type: None,
            patches: None,
            overlay: None,
            patch_strip: None,
        },
        BAZEL_TOOLS_MODULE_TOOLS.to_owned(),
    )
}

fn discovered_bzlmod_module_from_eval(
    dep: BazelDep,
    source_json: BcrSourceJson,
    evaluated: BzlmodEvaluatedModuleFile,
) -> bz_error::Result<DiscoveredBcrModule> {
    let extension_usages = evaluated.extension_usages;
    Ok(DiscoveredBcrModule {
        dep,
        source_json,
        module_aliases: evaluated.aliases,
        use_repo_aliases: bzlmod_use_repo_aliases_from_usages(&extension_usages),
        extension_usages,
        use_repo_rule_invocations: evaluated.use_repo_rule_invocations,
        constants: Vec::new(),
        registered_toolchains: evaluated.registered_toolchains,
        deps: evaluated.deps,
    })
}

fn bzlmod_bcr_discovery_cache_key(registry: &str, dep: &BazelDep, kind: &str) -> String {
    let mut hasher = Sha256::new();
    for field in [
        "buck2-bzlmod-bcr-discovery-v1",
        kind,
        registry,
        dep.name.as_str(),
        dep.version.as_str(),
    ] {
        hasher.update(field.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(field.as_bytes());
        hasher.update(b"\0");
    }
    hex::encode(hasher.finalize())
}

fn bzlmod_bcr_discovery_cache_path(
    registry: &str,
    dep: &BazelDep,
    kind: &str,
) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/cache/bzlmod_bcr_discovery/{}/{}",
        bzlmod_bcr_discovery_cache_key(registry, dep, kind),
        kind,
    ))
}

fn read_bzlmod_bcr_discovery_cache(
    project_fs: &ProjectRoot,
    path: &ProjectRelativePathBuf,
) -> bz_error::Result<Option<String>> {
    let path = project_fs.resolve(path);
    match fs::read_to_string(path.as_path()) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_buck_error_context(|| {
            format!("Error reading bzlmod registry cache `{}`", path.display())
        }),
    }
}

fn write_bzlmod_bcr_discovery_cache(
    project_fs: &ProjectRoot,
    path: &ProjectRelativePathBuf,
    contents: &str,
) -> bz_error::Result<()> {
    let path = project_fs.resolve(path);
    if let Some(parent) = path.as_path().parent() {
        fs::create_dir_all(parent).with_buck_error_context(|| {
            format!(
                "Error creating parent directory for bzlmod registry cache `{}`",
                path.display()
            )
        })?;
    }
    let temp = path
        .as_path()
        .with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&temp, contents).with_buck_error_context(|| {
        format!(
            "Error writing temporary bzlmod registry cache `{}`",
            temp.display()
        )
    })?;
    fs::rename(&temp, path.as_path()).with_buck_error_context(|| {
        format!(
            "Error committing bzlmod registry cache `{}`",
            path.display()
        )
    })?;
    Ok(())
}

async fn fetch_bcr_module_file(
    project_fs: &ProjectRoot,
    registry: &str,
    dep: BazelDep,
    archive_override: Option<BzlmodArchiveOverride>,
    single_version_override: Option<BzlmodSingleVersionOverride>,
) -> bz_error::Result<DiscoveredBcrModule> {
    let (source_json, mut module_text) = if let Some(archive_override) = archive_override.as_ref() {
        let source_json = bzlmod_archive_override_source_json(archive_override);
        let module_text = fetch_archive_override_module_file(archive_override)
            .await
            .with_buck_error_context(|| {
                format!(
                    "Error reading MODULE.bazel from archive_override for module `{}`",
                    dep.name
                )
            })?;
        (source_json, module_text)
    } else {
        let module_url = format!(
            "{registry}/modules/{}/{}/MODULE.bazel",
            dep.name, dep.version
        );
        let cache_path = bzlmod_bcr_discovery_cache_path(registry, &dep, "MODULE.bazel");
        let module_text =
            if let Some(module_text) = read_bzlmod_bcr_discovery_cache(project_fs, &cache_path)? {
                module_text
            } else {
                let module_text = http_get_text(&module_url).await?;
                write_bzlmod_bcr_discovery_cache(project_fs, &cache_path, &module_text)?;
                module_text
            };
        (empty_bcr_source_json(), module_text)
    };
    if archive_override.is_none()
        && let Some(single_version_override) = single_version_override.as_ref()
    {
        module_text = apply_bzlmod_single_version_module_patches(
            &dep.name,
            &module_text,
            single_version_override,
        )?;
    }
    parse_discovered_bzlmod_module(dep, source_json, module_text)
}

async fn fetch_bcr_module_source_json(
    project_fs: &ProjectRoot,
    registry: &str,
    dep: &BazelDep,
    archive_override: Option<&BzlmodArchiveOverride>,
    local_path_override: Option<&BzlmodLocalPathOverride>,
) -> bz_error::Result<BcrSourceJson> {
    if local_path_override.is_some() {
        return Ok(empty_bcr_source_json());
    }
    if let Some(archive_override) = archive_override {
        return Ok(bzlmod_archive_override_source_json(archive_override));
    }
    let source_url = format!(
        "{registry}/modules/{}/{}/source.json",
        dep.name, dep.version
    );
    let cache_path = bzlmod_bcr_discovery_cache_path(registry, dep, "source.json");
    let source_json =
        if let Some(source_json) = read_bzlmod_bcr_discovery_cache(project_fs, &cache_path)? {
            source_json
        } else {
            let source_json = http_get_text(&source_url).await?;
            write_bzlmod_bcr_discovery_cache(project_fs, &cache_path, &source_json)?;
            source_json
        };
    serde_json::from_str(&source_json)
        .with_buck_error_context(|| format!("Invalid BCR source metadata at `{source_url}`"))
}

fn bzlmod_yanked_versions_from_metadata_json(
    metadata: &str,
) -> bz_error::Result<BTreeMap<String, String>> {
    #[derive(Deserialize)]
    struct MetadataJson {
        #[serde(default, rename = "yanked_versions")]
        yanked_versions: BTreeMap<String, String>,
    }

    let metadata: MetadataJson = serde_json::from_str(metadata)
        .buck_error_context("Invalid bzlmod registry metadata.json")?;
    Ok(metadata.yanked_versions)
}

async fn http_get_text(url: &str) -> bz_error::Result<String> {
    let bytes = http_get_bytes(url).await?;
    String::from_utf8(bytes)
        .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Input))
        .with_buck_error_context(|| format!("Invalid UTF-8 response from `{url}`"))
}

async fn http_get_bytes(url: &str) -> bz_error::Result<Vec<u8>> {
    // The shared client is created lazily here, at the point of the first real
    // request: loading system TLS roots is slow, and fully disk-cached bzlmod
    // operations should not pay for it.
    let client = shared_bzlmod_http_client().await?;
    http_retry(
        || async {
            let response = client
                .get(url)
                .await
                .map_err(|error| BcrHttpGetError::Fetch {
                    url: url.to_owned(),
                    source: RetryingHttpError::Client(error),
                })?;
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|error| BcrHttpGetError::Fetch {
                    url: url.to_owned(),
                    source: RetryingHttpError::Transfer {
                        received: bytes.len() as u64,
                        url: url.to_owned(),
                        source: error,
                    },
                })?;
                bytes.extend_from_slice(&chunk);
            }
            Result::<_, BcrHttpGetError>::Ok(bytes)
        },
        vec![2, 4, 8].into_iter().map(Duration::from_secs).collect(),
    )
    .await
    .map_err(bz_error::Error::from)
}

async fn http_get_bytes_from_urls(urls: &[String]) -> bz_error::Result<(String, Vec<u8>)> {
    let mut last_error = None;
    for url in urls {
        match http_get_bytes(url).await {
            Ok(bytes) => return Ok((url.clone(), bytes)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(bz_error!(
        bz_error::ErrorTag::Input,
        "failed to download from any archive_override URL {:?}: {}",
        urls,
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no URL provided".to_owned())
    ))
}

async fn fetch_archive_override_module_file(
    archive_override: &BzlmodArchiveOverride,
) -> bz_error::Result<String> {
    let (url, bytes) = http_get_bytes_from_urls(&archive_override.urls).await?;
    verify_bzlmod_archive_integrity(&url, &archive_override.integrity, &bytes)?;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Tier0))?
        .as_nanos();
    let temp = std::env::temp_dir().join(format!(
        "buck2-bzlmod-archive-{}-{unique}",
        sanitize_bzlmod_temp_name(&archive_override.module_name)
    ));
    let archive = temp.join("source.archive");
    let extract_dir = temp.join("extract");
    fs::create_dir_all(&extract_dir)
        .with_buck_error_context(|| format!("Error creating `{}`", extract_dir.display()))?;
    fs::write(&archive, bytes)
        .with_buck_error_context(|| format!("Error writing `{}`", archive.display()))?;
    extract_bzlmod_archive_override(archive_override, &archive, &extract_dir)?;

    let module_root = archive_override
        .strip_prefix
        .as_ref()
        .map(|strip_prefix| extract_dir.join(strip_prefix))
        .unwrap_or_else(|| extract_dir.clone());
    for patch in &archive_override.patches {
        apply_bzlmod_root_patch_to_directory(
            &module_root,
            patch,
            archive_override.patch_strip.unwrap_or(0),
        )
        .with_buck_error_context(|| {
            format!(
                "Error applying archive_override patch `{}` to module `{}` for MODULE.bazel discovery",
                patch.path, archive_override.module_name
            )
        })?;
    }

    let module_file = module_root.join("MODULE.bazel");
    let module_text = fs::read_to_string(&module_file)
        .with_buck_error_context(|| format!("Error reading `{}`", module_file.display()))?;
    let _ = fs::remove_dir_all(&temp);
    Ok(module_text)
}

fn extract_bzlmod_archive_override(
    archive_override: &BzlmodArchiveOverride,
    archive: &Path,
    extract_dir: &Path,
) -> bz_error::Result<()> {
    let primary_url = bzlmod_archive_override_primary_url(archive_override);
    let kind = bzlmod_archive_override_kind(archive_override).ok_or_else(|| {
        bz_error!(
            bz_error::ErrorTag::Input,
            "unsupported archive_override archive type for `{}`",
            primary_url
        )
    })?;
    extract_archive(archive, extract_dir, kind, "", 0, &[]).with_buck_error_context(|| {
        format!("archive_override extraction failed for `{}`", primary_url)
    })
}

fn bzlmod_archive_override_kind(archive_override: &BzlmodArchiveOverride) -> Option<ArchiveKind> {
    archive_kind_from_type_or_url(
        archive_override.archive_type.as_deref(),
        bzlmod_archive_override_primary_url(archive_override),
    )
}

fn bzlmod_archive_override_primary_url(archive_override: &BzlmodArchiveOverride) -> &str {
    archive_override
        .urls
        .first()
        .expect("archive_override URLs should be non-empty")
}

fn verify_bzlmod_archive_integrity(
    url: &str,
    integrity: &str,
    bytes: &[u8],
) -> bz_error::Result<()> {
    let Some(expected) = parse_bzlmod_integrity(integrity)? else {
        return Ok(());
    };
    let got = expected.kind().digest(bytes);
    if got.as_slice() != expected.bytes() {
        return Err(bz_error!(
            bz_error::ErrorTag::Input,
            "archive_override integrity mismatch for `{}`: expected {}, got {}",
            url,
            hex::encode(expected.bytes()),
            hex::encode(got)
        ));
    }
    Ok(())
}

fn apply_bzlmod_single_version_module_patches(
    module_name: &str,
    module_text: &str,
    single_version_override: &BzlmodSingleVersionOverride,
) -> bz_error::Result<String> {
    let mut module_text = module_text.to_owned();
    let patch_strip = single_version_override.patch_strip.unwrap_or(0);
    for patch in &single_version_override.patches {
        let Some(filtered_patch) = filter_bzlmod_module_file_patch(&patch.content, patch_strip)
        else {
            continue;
        };
        module_text = apply_bzlmod_module_file_patch(
            module_name,
            &module_text,
            patch,
            &filtered_patch,
            patch_strip,
        )?;
    }
    Ok(module_text)
}

fn filter_bzlmod_module_file_patch(patch_content: &str, patch_strip: u32) -> Option<String> {
    let mut chunks = Vec::<Vec<&str>>::new();
    let mut current = Vec::<&str>::new();
    for line in patch_content.lines() {
        let starts_file_chunk = line.starts_with("diff --git ")
            || (line.starts_with("--- ") && current.iter().any(|line| line.starts_with("+++ ")));
        if starts_file_chunk && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    let mut filtered = String::new();
    for chunk in chunks {
        if !bzlmod_patch_chunk_touches_module_file(&chunk, patch_strip) {
            continue;
        }
        for line in chunk {
            filtered.push_str(line);
            filtered.push('\n');
        }
    }
    (!filtered.is_empty()).then_some(filtered)
}

fn bzlmod_patch_chunk_touches_module_file(chunk: &[&str], patch_strip: u32) -> bool {
    chunk.iter().any(|line| {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            return rest.split_whitespace().any(|path| {
                bzlmod_patch_path_after_strip(path, patch_strip).as_deref() == Some("MODULE.bazel")
            });
        }
        if let Some(rest) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            return bzlmod_patch_path_after_strip(rest, patch_strip).as_deref()
                == Some("MODULE.bazel");
        }
        false
    })
}

fn bzlmod_patch_path_after_strip(path: &str, patch_strip: u32) -> Option<String> {
    crate::bazel::bzlmod::patch::patch_path_after_strip(path, patch_strip)
}

fn apply_bzlmod_module_file_patch(
    module_name: &str,
    module_text: &str,
    patch: &BzlmodRootPatch,
    filtered_patch: &str,
    patch_strip: u32,
) -> bz_error::Result<String> {
    let temp = bzlmod_temp_dir(&format!(
        "module-patch-{}",
        sanitize_bzlmod_temp_name(module_name)
    ))?;
    let module_file = temp.join("MODULE.bazel");
    let patch_file = temp.join("module.patch");
    fs::create_dir_all(&temp)
        .with_buck_error_context(|| format!("Error creating `{}`", temp.display()))?;
    fs::write(&module_file, module_text)
        .with_buck_error_context(|| format!("Error writing `{}`", module_file.display()))?;
    fs::write(&patch_file, filtered_patch)
        .with_buck_error_context(|| format!("Error writing `{}`", patch_file.display()))?;

    let result = run_bzlmod_patch(&temp, &patch_file, patch_strip).with_buck_error_context(|| {
        format!(
            "Error applying single_version_override patch `{}` to MODULE.bazel for module `{}`",
            patch.path, module_name
        )
    });
    let module_text = result.and_then(|()| {
        fs::read_to_string(&module_file)
            .with_buck_error_context(|| format!("Error reading `{}`", module_file.display()))
    });
    let _ = fs::remove_dir_all(&temp);
    module_text
}

fn apply_bzlmod_root_patch_to_directory(
    directory: &Path,
    patch: &BzlmodRootPatch,
    patch_strip: u32,
) -> bz_error::Result<()> {
    if patch.content.is_empty() {
        return Ok(());
    }
    let temp = bzlmod_temp_dir("root-patch")?;
    fs::create_dir_all(&temp)
        .with_buck_error_context(|| format!("Error creating `{}`", temp.display()))?;
    let patch_file = temp.join("root.patch");
    fs::write(&patch_file, patch.content.as_bytes())
        .with_buck_error_context(|| format!("Error writing `{}`", patch_file.display()))?;
    let result = run_bzlmod_patch(directory, &patch_file, patch_strip);
    let _ = fs::remove_dir_all(&temp);
    result
}

fn run_bzlmod_patch(
    directory: &Path,
    patch_file: &Path,
    patch_strip: u32,
) -> bz_error::Result<()> {
    crate::bazel::bzlmod::patch::apply_unified_patch_file(directory, patch_file, patch_strip)
}

fn bzlmod_temp_dir(prefix: &str) -> bz_error::Result<std::path::PathBuf> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Tier0))?
        .as_nanos();
    Ok(std::env::temp_dir().join(format!("buck2-bzlmod-{prefix}-{unique}")))
}

fn sanitize_bzlmod_temp_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn bzlmod_use_repo_aliases_from_usages(usages: &[BzlmodExtensionUsage]) -> Vec<String> {
    usages
        .iter()
        .flat_map(|usage| usage.imports.iter())
        .map(|import| import.alias.clone())
        .collect()
}

fn bzlmod_extension_imports_from_usages(
    usages: &[BzlmodExtensionUsage],
    extension: &str,
) -> Vec<BzlmodUseRepoImport> {
    usages
        .iter()
        .filter(|usage| usage.extension_name == extension)
        .flat_map(|usage| usage.imports.iter().cloned())
        .collect()
}

fn bzlmod_repository_rule_string_attr_expression(value: &str) -> bz_error::Result<String> {
    serde_json::to_string(value)
        .buck_error_context("Error serializing use_repo_rule string attribute")
}

fn bzlmod_patch_configs(
    registry: &str,
    dep: &BazelDep,
    source_json: &BcrSourceJson,
    archive_override: Option<&BzlmodArchiveOverride>,
    single_version_override: Option<&BzlmodSingleVersionOverride>,
) -> Vec<BzlmodPatchConfig> {
    let mut patches = source_json
        .patches
        .as_ref()
        .into_iter()
        .flat_map(|patches| patches.iter())
        .map(|(file, integrity)| BzlmodPatchConfig {
            url: format!(
                "{registry}/modules/{}/{}/patches/{}",
                dep.name, dep.version, file
            ),
            integrity: integrity.clone(),
            path: None,
            content_sha256: None,
            patch_strip: source_json.patch_strip,
        })
        .collect::<Vec<_>>();
    if let Some(archive_override) = archive_override {
        patches.extend(
            archive_override
                .patches
                .iter()
                .map(|path| BzlmodPatchConfig {
                    url: String::new(),
                    integrity: String::new(),
                    path: Some(path.path.clone()),
                    content_sha256: Some(hex::encode(Sha256::digest(path.content.as_bytes()))),
                    patch_strip: archive_override.patch_strip,
                }),
        );
    }
    if let Some(single_version_override) = single_version_override {
        patches.extend(
            single_version_override
                .patches
                .iter()
                .map(|path| BzlmodPatchConfig {
                    url: String::new(),
                    integrity: String::new(),
                    path: Some(path.path.clone()),
                    content_sha256: Some(hex::encode(Sha256::digest(path.content.as_bytes()))),
                    patch_strip: single_version_override.patch_strip,
                }),
        );
    }
    patches
}

fn bzlmod_overlay_configs(
    registry: &str,
    dep: &BazelDep,
    source_json: &BcrSourceJson,
) -> Vec<BzlmodOverlayConfig> {
    source_json
        .overlay
        .as_ref()
        .into_iter()
        .flat_map(|overlays| overlays.iter())
        .map(|(path, integrity)| BzlmodOverlayConfig {
            path: path.clone(),
            url: format!(
                "{registry}/modules/{}/{}/overlay/{}",
                dep.name, dep.version, path
            ),
            integrity: integrity.clone(),
        })
        .collect()
}

fn bzlmod_selected_keys_dependency_first(
    discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
    root_deps: &[BazelDep],
    selected_versions: &BTreeMap<String, String>,
    selected_keys: &BTreeSet<(String, String)>,
) -> Vec<(String, String)> {
    fn visit(
        key: &(String, String),
        discovered: &BTreeMap<(String, String), DiscoveredBcrModule>,
        selected_versions: &BTreeMap<String, String>,
        selected_keys: &BTreeSet<(String, String)>,
        seen: &mut BTreeSet<(String, String)>,
        ordered: &mut Vec<(String, String)>,
    ) {
        if !selected_keys.contains(key) || !seen.insert(key.clone()) {
            return;
        }
        if let Some(module) = discovered.get(key) {
            for dep in &module.deps {
                let Some(version) = selected_versions.get(&dep.name) else {
                    continue;
                };
                visit(
                    &(dep.name.clone(), version.clone()),
                    discovered,
                    selected_versions,
                    selected_keys,
                    seen,
                    ordered,
                );
            }
        }
        ordered.push(key.clone());
    }

    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    for dep in root_deps {
        let Some(version) = selected_versions.get(&dep.name) else {
            continue;
        };
        visit(
            &(dep.name.clone(), version.clone()),
            discovered,
            selected_versions,
            selected_keys,
            &mut seen,
            &mut ordered,
        );
    }
    for key in selected_keys {
        visit(
            key,
            discovered,
            selected_versions,
            selected_keys,
            &mut seen,
            &mut ordered,
        );
    }
    ordered
}

fn bzlmod_canonical_repo_names_by_key(
    selected_keys: &BTreeSet<(String, String)>,
) -> BTreeMap<(String, String), String> {
    let mut selected_versions_by_name = BTreeMap::<&str, BTreeSet<&str>>::new();
    for (module_name, version) in selected_keys {
        selected_versions_by_name
            .entry(module_name.as_str())
            .or_default()
            .insert(version.as_str());
    }

    selected_keys
        .iter()
        .map(|(module_name, version)| {
            let multiple_versions = selected_versions_by_name
                .get(module_name.as_str())
                .map_or(false, |versions| versions.len() > 1);
            (
                (module_name.clone(), version.clone()),
                bzlmod_canonical_repo_name(module_name, version, multiple_versions),
            )
        })
        .collect()
}

fn bzlmod_selected_canonical_repo_name(
    canonical_repo_names_by_key: &BTreeMap<(String, String), String>,
    module_name: &str,
    version: &str,
) -> bz_error::Result<String> {
    canonical_repo_names_by_key
        .get(&(module_name.to_owned(), version.to_owned()))
        .cloned()
        .ok_or_else(|| {
            bz_error!(
                bz_error::ErrorTag::Input,
                "selected bzlmod module `{}@{}` does not have a canonical repository name",
                module_name,
                version
            )
        })
}

fn bzlmod_canonical_repo_name(module_name: &str, version: &str, multiple_versions: bool) -> String {
    match module_name {
        "bazel_tools" => "bazel_tools".to_owned(),
        "platforms" => "platforms".to_owned(),
        _ if multiple_versions => format!("{module_name}+{version}"),
        _ => format!("{module_name}+"),
    }
}

fn bzlmod_cell_name_for_canonical_repo_name(canonical_repo_name: &str) -> String {
    bzlmod_cell_name(canonical_repo_name)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bz_core::cells::external::bzlmod_cell_name;
    use bz_error::BuckErrorContext;
    use indoc::indoc;

    use crate::legacy_configs::cells::BuckConfigBasedCells;
    use crate::legacy_configs::configs::testing::TestConfigParserFileOps;

    fn insert_test_extension_usages_json(
        extension_usages_json_by_id: &mut std::collections::BTreeMap<
            super::BzlmodExtensionId,
            String,
        >,
        extension_id: super::BzlmodExtensionId,
    ) -> bz_error::Result<()> {
        extension_usages_json_by_id.insert(
            extension_id,
            serde_json::to_string(&super::BzlmodModuleExtensionEvaluationConfig {
                root_module_has_non_dev_dependency: false,
                modules: Vec::new(),
                usages: Vec::new(),
                repo_overrides: Vec::new(),
            })
            .buck_error_context("Error serializing test extension usages")?,
        );
        Ok(())
    }

    fn eval_bzlmod_module_for_test(
        module_text: &str,
        ignore_dev_dependency: bool,
    ) -> bz_error::Result<super::BzlmodEvaluatedModuleFile> {
        let compiled =
            super::compile_bzlmod_module_file("MODULE.bazel".to_owned(), module_text.to_owned())?;
        super::eval_bzlmod_module_file(
            &compiled,
            super::BzlmodModuleEvalOptions {
                is_root: true,
                allow_include: false,
                ignore_dev_dependency,
                default_name: "root".to_owned(),
                default_version: String::new(),
                default_repo_name: "root".to_owned(),
                cell_project_path: Some(
                    bz_core::fs::project_rel_path::ProjectRelativePathBuf::testing_new(""),
                ),
                included_modules: std::collections::BTreeMap::new(),
            },
        )
    }

    fn eval_bzlmod_module(
        module_text: &str,
    ) -> bz_error::Result<super::BzlmodEvaluatedModuleFile> {
        eval_bzlmod_module_for_test(module_text, false)
    }

    #[test]
    fn test_bzlmod_module_validation_rejects_load() {
        let error = eval_bzlmod_module(indoc!(
            r#"
            module(name = "demo")
            load("//:defs.bzl", "dep")
            "#
        ))
        .unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("`load` statements may not be used"),
            "error: {error}"
        );
    }

    #[test]
    fn test_bzlmod_module_accepts_starlark_intrinsic_constants() -> bz_error::Result<()> {
        let evaluated = eval_bzlmod_module(indoc!(
            r#"
            print("loading", "module", sep = " ")
            module(name = "demo")
            bazel_dep(name = "visible", version = "1.0", dev_dependency = False)
            bazel_dep(name = "hidden", version = "1.0", repo_name = None)
            register_toolchains("//:toolchain", dev_dependency = True)
            "#
        ))?;

        assert_eq!(evaluated.deps.len(), 2);
        assert_eq!(evaluated.deps[0].apparent_name.as_deref(), Some("visible"));
        assert_eq!(evaluated.deps[1].apparent_name, None);
        assert_eq!(evaluated.registered_toolchains, vec!["//:toolchain"]);
        Ok(())
    }

    #[test]
    fn test_bzlmod_extra_root_dep_parser_makes_visible_alias() -> bz_error::Result<()> {
        let dep = super::parse_extra_bzlmod_root_dep(" llvm@0.8.4 ")?.unwrap();

        assert_eq!(dep.name, "llvm");
        assert_eq!(dep.version, "0.8.4");
        assert_eq!(dep.apparent_name.as_deref(), Some("llvm"));
        Ok(())
    }

    #[test]
    fn test_bzlmod_extra_root_dep_merge_adds_missing_visible_alias() -> bz_error::Result<()> {
        let mut root_deps = vec![super::BazelDep {
            name: "llvm".to_owned(),
            version: "0.8.4".to_owned(),
            apparent_name: None,
        }];
        let extra = vec![super::parse_extra_bzlmod_root_dep("llvm@0.8.4")?.unwrap()];

        super::merge_extra_bzlmod_root_deps(&mut root_deps, extra);

        assert_eq!(root_deps.len(), 2);
        assert_eq!(root_deps[1].apparent_name.as_deref(), Some("llvm"));
        Ok(())
    }

    #[test]
    fn test_bzlmod_extra_root_dep_merge_keeps_existing_visible_alias() -> bz_error::Result<()> {
        let mut root_deps = vec![super::BazelDep {
            name: "llvm".to_owned(),
            version: "0.8.4".to_owned(),
            apparent_name: Some("llvm".to_owned()),
        }];
        let extra = vec![super::parse_extra_bzlmod_root_dep("llvm@0.8.4")?.unwrap()];

        super::merge_extra_bzlmod_root_deps(&mut root_deps, extra);

        assert_eq!(root_deps.len(), 1);
        assert_eq!(root_deps[0].apparent_name.as_deref(), Some("llvm"));
        Ok(())
    }

    #[test]
    fn test_bzlmod_configure_repo_detection_uses_structured_generator_kind()
    -> bz_error::Result<()> {
        fn generated(
            canonical_repo_name: &str,
            generator_json: String,
        ) -> super::BazelCompatExternalModule {
            super::BazelCompatExternalModule::Generated(super::BazelCompatGeneratedModule {
                cell_name: super::bzlmod_cell_name(canonical_repo_name),
                aliases: Vec::new(),
                canonical_repo_name: canonical_repo_name.to_owned(),
                generator_json,
            })
        }

        let host_platform_json =
            serde_json::to_string(&super::BzlmodGeneratedCellGenerator::HostPlatform(
                super::BzlmodHostPlatformSetup::default(),
            ))?;
        assert!(super::bzlmod_external_module_is_configure_repo(&generated(
            "platforms+host_platform+host_platform",
            host_platform_json
        )));

        let non_configure_json =
            serde_json::to_string(&super::BzlmodGeneratedCellGenerator::BazelFeaturesVersion(
                super::BzlmodBazelFeaturesVersionSetup {
                    bazel_version: Arc::from("9.1.0"),
                },
            ))?;
        assert!(!super::bzlmod_external_module_is_configure_repo(
            &generated("rules_example+toolchain_config_repo", non_configure_json)
        ));
        assert!(!super::bzlmod_external_module_is_configure_repo(
            &generated("rules_example+configure_repo", "{".to_owned())
        ));
        Ok(())
    }

    #[test]
    fn test_bzlmod_host_platform_setup_from_constraints() {
        let setup = super::bzlmod_host_platform_setup_from_constraints(Some(
            "@platforms//cpu:x86_64\n@platforms//os:linux",
        ));

        assert_eq!(setup.cpu_constraint.as_deref(), Some("x86_64"));
        assert_eq!(setup.os_constraint.as_deref(), Some("linux"));
    }

    #[test]
    fn test_bzlmod_single_version_override_allows_patches_without_version()
    -> bz_error::Result<()> {
        let evaluated = eval_bzlmod_module(indoc!(
            r#"
            single_version_override(
                module_name = "grpc-java",
                patch_strip = 1,
                patches = [
                    "//third_party:grpc-java.patch",
                    "//third_party:grpc-java-addloads.patch",
                ],
            )
            "#
        ))?;
        let override_config = evaluated.single_version_overrides.get("grpc-java").unwrap();

        assert_eq!(override_config.version, None);
        assert_eq!(override_config.patch_strip, Some(1));
        assert_eq!(
            override_config
                .patches
                .iter()
                .map(|patch| patch.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "third_party/grpc-java.patch",
                "third_party/grpc-java-addloads.patch",
            ]
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_archive_override_preserves_url_mirror_order() -> bz_error::Result<()> {
        let evaluated = eval_bzlmod_module(indoc!(
            r#"
            archive_override(
                module_name = "example",
                url = "https://primary.example.com/source.tar.gz",
                urls = [
                    "https://mirror1.example.com/source.tar.gz",
                    "https://mirror2.example.com/source.tar.gz",
                ],
            )
            "#
        ))?;
        let archive_override = evaluated.archive_overrides.get("example").unwrap();

        assert_eq!(
            archive_override.urls,
            vec![
                "https://primary.example.com/source.tar.gz",
                "https://mirror1.example.com/source.tar.gz",
                "https://mirror2.example.com/source.tar.gz",
            ]
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_archive_override_infers_kind_from_url_without_type() -> bz_error::Result<()> {
        let evaluated = eval_bzlmod_module(indoc!(
            r#"
            archive_override(
                module_name = "rules_webtesting",
                url = "https://github.com/bazelbuild/rules_webtesting/archive/e09c04b7d4d1e91ac1cd6f08283246d350c65379.tar.gz",
            )
            "#
        ))?;
        let archive_override = evaluated.archive_overrides.get("rules_webtesting").unwrap();

        assert_eq!(
            super::bzlmod_archive_override_kind(&archive_override),
            Some(crate::bazel::bzlmod::archive::ArchiveKind::TarGz)
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_archive_integrity_accepts_sri_algorithms() {
        for integrity in [
            "sha1-iEPX+SQWIR3p67lj/0zigSWTKHg=",
            "sha256-w6uP8Tcg6K2QR905Rms8iXTlksL6OD1KOWBxTK7wxPI=",
            "sha384-PJww2fZl501RXIQpYNSkUcg6ASX9Pec5LXs3IxrxDHLqWK7fzfiaV2W/kCr5Ps8G",
            "sha512-ClAmHr0aOQ/tK/Mm8mc8FFWCpjQtUjIElz0CGTN/gWFqgGmwElh89WNfaSXxtWw2AjDBmyc1AO4BPgMGAb8kJQ==",
        ] {
            super::verify_bzlmod_archive_integrity(
                "https://example.com/archive.tar.gz",
                integrity,
                b"foobar",
            )
            .unwrap();
        }
    }

    #[test]
    fn test_bzlmod_archive_override_resolves_module_constants() -> bz_error::Result<()> {
        let evaluated = eval_bzlmod_module(indoc!(
            r#"
            COMMIT = "abc123"

            archive_override(
                module_name = "example",
                strip_prefix = "example-{}".format(COMMIT),
                urls = ["https://example.com/{}.tar.gz".format(COMMIT)],
            )
            "#
        ))?;
        let archive_override = evaluated.archive_overrides.get("example").unwrap();

        assert_eq!(
            archive_override.strip_prefix.as_deref(),
            Some("example-abc123")
        );
        assert_eq!(
            archive_override.urls,
            vec!["https://example.com/abc123.tar.gz"]
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_single_version_override_rejects_patch_cmds() {
        let error = eval_bzlmod_module(indoc!(
            r#"
            single_version_override(
                module_name = "example",
                patch_cmds = ["echo patched"],
            )
            "#
        ))
        .unwrap_err();

        let error = format!("{error:?}");
        assert!(error.contains("unsupported `patch_cmds`"), "error: {error}");
    }

    #[test]
    fn test_bzlmod_single_version_override_rejects_registry() {
        let error = eval_bzlmod_module(indoc!(
            r#"
            single_version_override(
                module_name = "example",
                registry = "https://registry.example.com",
            )
            "#
        ))
        .unwrap_err();

        let error = format!("{error:?}");
        assert!(error.contains("unsupported `registry`"), "error: {error}");
    }

    #[tokio::test]
    async fn test_bzlmod_multiple_version_override_rejected() -> bz_error::Result<()> {
        let mut file_ops = TestConfigParserFileOps::new(&[(
            "MODULE.bazel",
            indoc!(
                r#"
                module(name = "root")
                multiple_version_override(
                    module_name = "example",
                    versions = ["1.0.0", "2.0.0"],
                )
                "#
            ),
        )])?;

        let error =
            match BuckConfigBasedCells::testing_parse_with_file_ops(&mut file_ops, &[]).await {
                Ok(_) => panic!("expected multiple_version_override to be rejected"),
                Err(error) => error,
            };
        let error = format!("{error:?}");
        assert!(
            error.contains("multiple_version_override is not implemented"),
            "error: {error}"
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_extension_usages_are_parsed_without_extension_name_special_cases()
    -> bz_error::Result<()> {
        let usages = eval_bzlmod_module(indoc!(
            r#"
            sdk = use_extension("//go:extensions.bzl", "go_sdk")
            _GO_MOD = "//:go.mod"
            SUPPORTED_PYTHON_VERSIONS = [
                "3.11",
                "3.12",
            ]
            sdk.from_file(name = "go_default_sdk", go_mod = "//:go.mod")
            [sdk.from_file(name = name, go_mod = "//:go.mod") for name in ("ignored",)]
            use_repo(
                sdk,
                "go_toolchains",
                alias_name = "actual_repo",
                system_python = "python_{}".format(SUPPORTED_PYTHON_VERSIONS[-1].replace(".", "_")),
            )
            INJECTED_REPOS = {
                "non_identifier.repo": "actual_repo",
            }
            inject_repo(
                sdk,
                "com_github_buildbuddy_io_buildbuddy",
                googleapis_alias = "googleapis",
                **INJECTED_REPOS,
            )
            override_repo(sdk, go_toolchains = "actual_repo")

            features = use_extension("@bazel_features//:extensions.bzl", "version_extension")
            use_repo(features, "bazel_features_globals")

            rules_kotlin_extensions = use_extension(
                "//src/main/starlark/core/repositories:bzlmod_setup.bzl",
                "rules_kotlin_extensions",
            )
            use_repo(
                rules_kotlin_extensions,
                "com_github_jetbrains_kotlin",
            )
            "#
        ))?
        .extension_usages;
        assert_eq!(usages.len(), 3);
        assert_eq!(usages[0].proxy_name, "sdk");
        assert_eq!(usages[0].extension_bzl_file, "//go:extensions.bzl");
        assert_eq!(usages[0].extension_name, "go_sdk");
        assert_eq!(usages[0].tags.len(), 2);
        assert_eq!(usages[0].tags[0].tag_name, "from_file");
        assert_eq!(
            usages[0].tags[0].kwargs,
            vec![
                ("go_mod".to_owned(), "\"//:go.mod\"".to_owned()),
                ("name".to_owned(), "\"go_default_sdk\"".to_owned()),
            ]
        );
        assert_eq!(
            usages[0].tags[1].kwargs,
            vec![
                ("go_mod".to_owned(), "\"//:go.mod\"".to_owned()),
                ("name".to_owned(), "\"ignored\"".to_owned()),
            ]
        );
        assert_eq!(usages[0].imports.len(), 3);
        assert_eq!(usages[0].imports[0].alias, "go_toolchains");
        assert_eq!(usages[0].imports[0].repo_name, "go_toolchains");
        assert_eq!(usages[0].imports[1].alias, "alias_name");
        assert_eq!(usages[0].imports[1].repo_name, "actual_repo");
        assert_eq!(usages[0].imports[2].alias, "system_python");
        assert_eq!(usages[0].imports[2].repo_name, "python_3_12");
        assert_eq!(usages[0].repo_overrides.len(), 4);
        assert_eq!(
            usages[0].repo_overrides[0].repo_name,
            "com_github_buildbuddy_io_buildbuddy"
        );
        assert_eq!(
            usages[0].repo_overrides[0].overriding_repo_name,
            "com_github_buildbuddy_io_buildbuddy"
        );
        assert!(!usages[0].repo_overrides[0].must_exist);
        assert_eq!(usages[0].repo_overrides[1].repo_name, "go_toolchains");
        assert_eq!(
            usages[0].repo_overrides[1].overriding_repo_name,
            "actual_repo"
        );
        assert!(usages[0].repo_overrides[1].must_exist);
        assert_eq!(usages[0].repo_overrides[2].repo_name, "googleapis_alias");
        assert_eq!(
            usages[0].repo_overrides[2].overriding_repo_name,
            "googleapis"
        );
        assert!(!usages[0].repo_overrides[2].must_exist);
        assert_eq!(usages[0].repo_overrides[3].repo_name, "non_identifier.repo");
        assert_eq!(
            usages[0].repo_overrides[3].overriding_repo_name,
            "actual_repo"
        );
        assert!(!usages[0].repo_overrides[3].must_exist);
        assert_eq!(
            super::bzlmod_extension_tag_repo_names(&usages[0]),
            vec!["go_default_sdk".to_owned(), "ignored".to_owned()]
        );

        assert_eq!(usages[1].proxy_name, "features");
        assert_eq!(
            usages[1].extension_bzl_file,
            "@bazel_features//:extensions.bzl"
        );
        assert_eq!(usages[1].extension_name, "version_extension");
        assert_eq!(usages[1].imports.len(), 1);
        assert_eq!(usages[1].imports[0].alias, "bazel_features_globals");
        assert_eq!(usages[1].imports[0].repo_name, "bazel_features_globals");

        assert_eq!(usages[2].proxy_name, "rules_kotlin_extensions");
        assert_eq!(
            usages[2].extension_bzl_file,
            "//src/main/starlark/core/repositories:bzlmod_setup.bzl"
        );
        assert_eq!(usages[2].extension_name, "rules_kotlin_extensions");
        assert_eq!(usages[2].imports.len(), 1);
        assert_eq!(usages[2].imports[0].alias, "com_github_jetbrains_kotlin");
        assert_eq!(
            usages[2].imports[0].repo_name,
            "com_github_jetbrains_kotlin"
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_extension_tags_expand_simple_list_comprehensions() -> bz_error::Result<()> {
        let usages = eval_bzlmod_module(indoc!(
            r#"
            SUPPORTED_PYTHON_VERSIONS = [
                "3.11",
                "3.12",
            ]

            python = use_extension("@rules_python//python/extensions:python.bzl", "python")

            [
                python.toolchain(
                    is_default = python_version == SUPPORTED_PYTHON_VERSIONS[-1],
                    python_version = python_version,
                )
                for python_version in SUPPORTED_PYTHON_VERSIONS
            ]
            "#
        ))?
        .extension_usages;
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].tags.len(), 2);
        assert_eq!(
            usages[0].tags[0].kwargs,
            vec![
                ("is_default".to_owned(), "False".to_owned()),
                ("python_version".to_owned(), "\"3.11\"".to_owned()),
            ]
        );
        assert_eq!(
            usages[0].tags[1].kwargs,
            vec![
                ("is_default".to_owned(), "True".to_owned()),
                ("python_version".to_owned(), "\"3.12\"".to_owned()),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_extension_tags_preserve_bazel_evaluation_order() -> bz_error::Result<()> {
        let usages = eval_bzlmod_module(indoc!(
            r#"
            ext = use_extension("//:extensions.bzl", "ext")

            ext.zeta(second = "2", first = "1")

            [
                ext.alpha(version = version)
                for version in ["3.11", "3.12"]
            ]

            ext.beta(name = "last")
            "#
        ))?
        .extension_usages;
        assert_eq!(usages.len(), 1);
        assert_eq!(
            usages[0]
                .tags
                .iter()
                .map(|tag| tag.tag_name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha", "alpha", "beta"]
        );
        assert_eq!(
            usages[0].tags[0].kwargs,
            vec![
                ("second".to_owned(), "\"2\"".to_owned()),
                ("first".to_owned(), "\"1\"".to_owned()),
            ]
        );
        assert_eq!(usages[0].tags[1].bindings, Vec::<(String, String)>::new());
        assert_eq!(
            usages[0].tags[1].kwargs,
            vec![("version".to_owned(), "\"3.11\"".to_owned())]
        );
        assert_eq!(
            usages[0].tags[2].kwargs,
            vec![("version".to_owned(), "\"3.12\"".to_owned())]
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_extension_tags_expand_split_tuple_list_comprehensions() -> bz_error::Result<()>
    {
        let usages = eval_bzlmod_module(indoc!(
            r#"
            maven = use_extension("@rules_jvm_external//:extensions.bzl", "maven")

            [
                maven.artifact(
                    artifact = artifact,
                    group = group,
                    version = version,
                )
                for group, artifact, version in [coord.split(":") for coord in [
                    "com.google.guava:guava-testlib:33.2.1-jre",
                    "com.google.truth:truth:1.4.2",
                ]]
            ]
            "#
        ))?
        .extension_usages;
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].tags.len(), 2);
        assert!(usages[0].tags.iter().any(|tag| {
            tag.kwargs
                .contains(&("group".to_owned(), "\"com.google.guava\"".to_owned()))
                && tag
                    .kwargs
                    .contains(&("artifact".to_owned(), "\"guava-testlib\"".to_owned()))
                && tag
                    .kwargs
                    .contains(&("version".to_owned(), "\"33.2.1-jre\"".to_owned()))
        }));
        Ok(())
    }

    #[test]
    fn test_bzlmod_extension_list_comprehensions_do_not_match_suffix_proxy_names()
    -> bz_error::Result<()> {
        let usages = eval_bzlmod_module_for_test(
            indoc!(
                r#"
            pip = use_extension("//python/extensions:pip.bzl", "pip")

            [pip.parse(hub_name = "prod_pip", python_version = version) for version in ["3.11"]]

            dev_pip = use_extension("//python/extensions:pip.bzl", "pip", dev_dependency = True)

            [dev_pip.parse(hub_name = "dev_pip", python_version = version) for version in ["3.14"]]
            "#
            ),
            true,
        )?
        .extension_usages;
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].proxy_name, "pip");
        assert_eq!(usages[0].tags.len(), 1);
        assert_eq!(
            usages[0].tags[0].kwargs,
            vec![
                ("hub_name".to_owned(), "\"prod_pip\"".to_owned()),
                ("python_version".to_owned(), "\"3.11\"".to_owned()),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_rules_java_repo_list_comprehensions() -> bz_error::Result<()> {
        let evaluated = eval_bzlmod_module(indoc!(
            r#"
            JDKS = {
                "8": ["linux"],
                "25": ["macos_aarch64"],
            }
            REMOTE_JDK_REPOS = [
                (("remote_jdk" if version == "8" else "remotejdk") + version + "_" + platform)
                for version in JDKS
                for platform in JDKS[version]
            ]

            toolchains = use_extension("//toolchains:extensions.bzl", "toolchains")

            [use_repo(toolchains, repo + "_toolchain_config_repo") for repo in REMOTE_JDK_REPOS]
            [register_toolchains("@" + name + "_toolchain_config_repo//:all") for name in REMOTE_JDK_REPOS]
            "#
        ))?;
        let usages = evaluated.extension_usages;
        assert_eq!(usages.len(), 1);
        let mut imports = usages[0]
            .imports
            .iter()
            .map(|import| (import.alias.clone(), import.repo_name.clone()))
            .collect::<Vec<_>>();
        imports.sort();
        assert_eq!(
            imports,
            vec![
                (
                    "remote_jdk8_linux_toolchain_config_repo".to_owned(),
                    "remote_jdk8_linux_toolchain_config_repo".to_owned()
                ),
                (
                    "remotejdk25_macos_aarch64_toolchain_config_repo".to_owned(),
                    "remotejdk25_macos_aarch64_toolchain_config_repo".to_owned()
                ),
            ]
        );

        assert_eq!(
            evaluated.registered_toolchains,
            vec![
                "@remote_jdk8_linux_toolchain_config_repo//:all".to_owned(),
                "@remotejdk25_macos_aarch64_toolchain_config_repo//:all".to_owned(),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_use_repo_rule_attrs_resolve_module_constants() -> bz_error::Result<()> {
        let invocations = eval_bzlmod_module(indoc!(
            r#"
            http_file = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_file")

            _VERSION = "v1.2.3"
            FILE_NAME = ("tool_" + _VERSION).replace(".", "_")
            URL = "https://example.com/{version}/tool".format(version = _VERSION)
            SHA256 = "abc123"

            http_file(
                name = "tool",
                sha256 = SHA256,
                urls = [URL],
            )
            "#
        ))?
        .use_repo_rule_invocations;
        assert_eq!(invocations.len(), 1);
        assert_eq!(
            invocations[0].attrs,
            vec![
                ("sha256".to_owned(), "\"abc123\"".to_owned()),
                (
                    "urls".to_owned(),
                    "[\"https://example.com/v1.2.3/tool\"]".to_owned()
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_use_repo_rule_expands_list_comprehensions() -> bz_error::Result<()> {
        let invocations = eval_bzlmod_module(indoc!(
            r#"
            http_archive = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")

            PREBUILT_LLVM_VERSION = "22.1.3"
            PREBUILT_LLVM_SUFFIX = "-2"
            LLVM_TOOLCHAIN_MINIMAL_SHA256 = {
                "linux-amd64-musl": "abc123",
                "windows-amd64": "def456",
            }
            TOOLCHAIN_EXTRAS_SHA256 = {
                "linux-amd64-musl": "ghi789",
            }

            [
                http_archive(
                    name = "llvm-toolchain-minimal-{llvm_version}-{target}".format(
                        llvm_version = PREBUILT_LLVM_VERSION,
                        target = target.replace("-musl", ""),
                    ),
                    build_file = "//toolchain/llvm:llvm_release.BUILD.bazel" if "windows" not in target else "//toolchain/llvm:llvm_release_windows.BUILD.bazel",
                    sha256 = sha256,
                    urls = ["https://example.com/llvm-{llvm_version}{prebuilt_llvm_suffix}/llvm-toolchain-minimal-{llvm_version}-{target}.tar.zst".format(
                        llvm_version = PREBUILT_LLVM_VERSION,
                        prebuilt_llvm_suffix = PREBUILT_LLVM_SUFFIX,
                        target = target,
                    )],
                )
                for (target, sha256) in LLVM_TOOLCHAIN_MINIMAL_SHA256.items()
            ]

            [
                http_archive(
                    name = "toolchain-extra-prebuilts-{target}".format(target = target.replace("-musl", "").replace("-gnu", "")),
                    build_file = "//prebuilt/extras:extras.BUILD.bazel",
                    sha256 = sha256,
                    urls = ["https://example.com/toolchain-extra-prebuilts-{target}.tar.zst".format(target = target)],
                )
                for (target, sha256) in TOOLCHAIN_EXTRAS_SHA256.items()
            ]
            "#
        ))?
        .use_repo_rule_invocations;

        assert_eq!(
            invocations
                .iter()
                .map(|invocation| invocation.repo_name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "llvm-toolchain-minimal-22.1.3-linux-amd64",
                "llvm-toolchain-minimal-22.1.3-windows-amd64",
                "toolchain-extra-prebuilts-linux-amd64",
            ]
        );
        assert_eq!(
            invocations[0].attrs,
            vec![
                (
                    "build_file".to_owned(),
                    "\"//toolchain/llvm:llvm_release.BUILD.bazel\"".to_owned()
                ),
                ("sha256".to_owned(), "\"abc123\"".to_owned()),
                (
                    "urls".to_owned(),
                    "[\"https://example.com/llvm-22.1.3-2/llvm-toolchain-minimal-22.1.3-linux-amd64-musl.tar.zst\"]".to_owned()
                ),
            ]
        );
        assert_eq!(
            invocations[1]
                .attrs
                .iter()
                .find(|(name, _)| name == "build_file")
                .map(|(_, value)| value.as_str()),
            Some("\"//toolchain/llvm:llvm_release_windows.BUILD.bazel\"")
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_extension_usages_ignore_dev_dependency_when_requested() -> bz_error::Result<()>
    {
        let usages = eval_bzlmod_module_for_test(
            indoc!(
                r#"
            dev_ext = use_extension(
                "@dev_repo//:extensions.bzl",
                "dev",
                dev_dependency = True,
            )
            use_repo(dev_ext, "dev_repo")

            prod_ext = use_extension("//:extensions.bzl", "prod")
            use_repo(prod_ext, "prod_repo")
            "#
            ),
            true,
        )?
        .extension_usages;
        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].proxy_name, "prod_ext");
        assert_eq!(usages[0].imports[0].repo_name, "prod_repo");
        Ok(())
    }

    #[test]
    fn test_bzlmod_extension_generated_repos_inherit_extension_host_repo_mapping() {
        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        let gazelle_cell = bzlmod_cell_name("gazelle+");
        let package_metadata_cell = bzlmod_cell_name("package_metadata+");
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            &gazelle_cell,
            "package_metadata",
            &package_metadata_cell,
        );

        let mut generated = Vec::new();
        let mut generated_repo_declaring_cells = Vec::new();
        let generated_cell_name = super::add_generated_bzlmod_repo_with_mapping_cell(
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut cell_aliases_by_cell,
            "root",
            "com_github_example_dep",
            &gazelle_cell,
            "gazelle++go_deps+com_github_example_dep",
            "{}".to_owned(),
        );

        let mut extension_generated_repo_groups = std::collections::BTreeMap::new();
        extension_generated_repo_groups.insert(
            "gazelle++go_deps".to_owned(),
            vec![(
                "com_github_example_dep".to_owned(),
                generated_cell_name.clone(),
            )],
        );
        let mut extension_repo_override_groups = std::collections::BTreeMap::new();
        extension_repo_override_groups.insert(
            "gazelle++go_deps".to_owned(),
            vec![(
                "com_github_buildbuddy_io_buildbuddy".to_owned(),
                "root".to_owned(),
            )],
        );

        super::add_generated_bzlmod_repo_mappings(
            &mut cell_aliases_by_cell,
            &generated_repo_declaring_cells,
            &extension_generated_repo_groups,
            &extension_repo_override_groups,
        );

        assert_eq!(
            super::bzlmod_cell_alias_target(
                &cell_aliases_by_cell,
                "root",
                "com_github_example_dep"
            ),
            Some(generated_cell_name.as_str())
        );
        assert_eq!(
            super::bzlmod_cell_alias_target(
                &cell_aliases_by_cell,
                &generated_cell_name,
                "package_metadata"
            ),
            Some(package_metadata_cell.as_str())
        );
        assert_eq!(
            super::bzlmod_cell_alias_target(
                &cell_aliases_by_cell,
                &generated_cell_name,
                "com_github_buildbuddy_io_buildbuddy"
            ),
            Some("root")
        );
        assert_eq!(
            super::bzlmod_cell_alias_target(
                &cell_aliases_by_cell,
                &generated_cell_name,
                "com_github_example_dep"
            ),
            Some(generated_cell_name.as_str())
        );
    }

    #[test]
    fn test_bzlmod_extension_without_lockfile_uses_static_repos() -> bz_error::Result<()> {
        let usage = super::BzlmodExtensionUsage {
            proxy_name: "npm".to_owned(),
            extension_bzl_file: "@aspect_rules_js//npm:extensions.bzl".to_owned(),
            extension_name: "npm".to_owned(),
            dev_dependency: false,
            imports: vec![super::BzlmodUseRepoImport {
                alias: "npm".to_owned(),
                repo_name: "npm".to_owned(),
            }],
            repo_overrides: Vec::new(),
            tags: vec![super::BzlmodExtensionTag {
                tag_name: "npm_translate_lock".to_owned(),
                bindings: Vec::new(),
                kwargs: vec![("name".to_owned(), "\"npm\"".to_owned())],
            }],
        };
        let root_module = super::RootBzlmodModule {
            name: "buildbuddy".to_owned(),
            version: String::new(),
            repo_name: "buildbuddy".to_owned(),
            canonical_repo_name: String::new(),
            lockfile_extension_generated_repos: std::collections::BTreeMap::new(),
            lockfile_extension_facts: std::collections::BTreeSet::new(),
            constants: Vec::new(),
            extension_usages: vec![usage.clone()],
            use_repo_rule_invocations: Vec::new(),
            registered_toolchains: Vec::new(),
        };

        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "root",
            "aspect_rules_js",
            "bzlmod_aspect_rules_js_",
        );

        let mut extension_unique_names = std::collections::BTreeMap::new();
        extension_unique_names.insert(
            super::BzlmodExtensionId {
                bzl_cell_name: "bzlmod_aspect_rules_js_".to_owned(),
                bzl_path: "npm/extensions.bzl".to_owned(),
                extension_name: "npm".to_owned(),
            },
            "aspect_rules_js++npm".to_owned(),
        );

        let mut canonical_repo_names_by_cell = std::collections::BTreeMap::new();
        canonical_repo_names_by_cell.insert(
            "bzlmod_aspect_rules_js_".to_owned(),
            "aspect_rules_js+".to_owned(),
        );

        let mut generated = Vec::new();
        let mut generated_repo_declaring_cells = Vec::new();
        let mut extension_generated_repo_groups = std::collections::BTreeMap::new();
        let mut extension_repo_override_groups = std::collections::BTreeMap::new();
        let mut extension_usages_json_by_id = std::collections::BTreeMap::new();
        insert_test_extension_usages_json(
            &mut extension_usages_json_by_id,
            super::BzlmodExtensionId {
                bzl_cell_name: "bzlmod_aspect_rules_js_".to_owned(),
                bzl_path: "npm/extensions.bzl".to_owned(),
                extension_name: "npm".to_owned(),
            },
        )?;

        super::resolve_bzlmod_extension_usage_generated_repos(
            &usage,
            "",
            "root",
            true,
            &root_module,
            &mut cell_aliases_by_cell,
            &canonical_repo_names_by_cell,
            &extension_unique_names,
            &mut extension_usages_json_by_id,
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut extension_generated_repo_groups,
            &mut extension_repo_override_groups,
        )?;

        assert_eq!(
            super::bzlmod_cell_alias_target(&cell_aliases_by_cell, "root", "npm"),
            Some("bzlmod_aspect_rules_js__npm_npm")
        );

        Ok(())
    }

    #[test]
    fn test_bzlmod_root_tag_only_extension_without_lockfile_stays_demand_driven()
    -> bz_error::Result<()> {
        let usage = super::BzlmodExtensionUsage {
            proxy_name: "go_sdk".to_owned(),
            extension_bzl_file: "@io_bazel_rules_go//go:extensions.bzl".to_owned(),
            extension_name: "go_sdk".to_owned(),
            dev_dependency: false,
            imports: Vec::new(),
            repo_overrides: Vec::new(),
            tags: vec![super::BzlmodExtensionTag {
                tag_name: "download".to_owned(),
                bindings: Vec::new(),
                kwargs: vec![("version".to_owned(), "\"1.24.0\"".to_owned())],
            }],
        };
        let root_module = super::RootBzlmodModule {
            name: "bazelisk".to_owned(),
            version: String::new(),
            repo_name: "bazelisk".to_owned(),
            canonical_repo_name: String::new(),
            lockfile_extension_generated_repos: std::collections::BTreeMap::new(),
            lockfile_extension_facts: std::collections::BTreeSet::new(),
            constants: Vec::new(),
            extension_usages: vec![usage.clone()],
            use_repo_rule_invocations: Vec::new(),
            registered_toolchains: Vec::new(),
        };

        let rules_go_cell = bzlmod_cell_name("rules_go+");
        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "root",
            "io_bazel_rules_go",
            &rules_go_cell,
        );

        let mut extension_unique_names = std::collections::BTreeMap::new();
        extension_unique_names.insert(
            super::BzlmodExtensionId {
                bzl_cell_name: rules_go_cell.clone(),
                bzl_path: "go/extensions.bzl".to_owned(),
                extension_name: "go_sdk".to_owned(),
            },
            "rules_go++go_sdk".to_owned(),
        );

        let mut canonical_repo_names_by_cell = std::collections::BTreeMap::new();
        canonical_repo_names_by_cell.insert(rules_go_cell.clone(), "rules_go+".to_owned());

        let mut generated = Vec::new();
        let mut generated_repo_declaring_cells = Vec::new();
        let mut extension_generated_repo_groups = std::collections::BTreeMap::new();
        let mut extension_repo_override_groups = std::collections::BTreeMap::new();
        let mut extension_usages_json_by_id = std::collections::BTreeMap::new();
        insert_test_extension_usages_json(
            &mut extension_usages_json_by_id,
            super::BzlmodExtensionId {
                bzl_cell_name: rules_go_cell,
                bzl_path: "go/extensions.bzl".to_owned(),
                extension_name: "go_sdk".to_owned(),
            },
        )?;

        super::resolve_bzlmod_extension_usage_generated_repos(
            &usage,
            "",
            "root",
            true,
            &root_module,
            &mut cell_aliases_by_cell,
            &canonical_repo_names_by_cell,
            &extension_unique_names,
            &mut extension_usages_json_by_id,
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut extension_generated_repo_groups,
            &mut extension_repo_override_groups,
        )?;

        assert!(generated.is_empty());

        Ok(())
    }

    #[test]
    fn test_bzlmod_non_root_extension_without_lockfile_uses_static_repos() -> bz_error::Result<()>
    {
        let usage = super::BzlmodExtensionUsage {
            proxy_name: "npm".to_owned(),
            extension_bzl_file: "@aspect_rules_js//npm:extensions.bzl".to_owned(),
            extension_name: "npm".to_owned(),
            dev_dependency: false,
            imports: vec![super::BzlmodUseRepoImport {
                alias: "npm".to_owned(),
                repo_name: "npm".to_owned(),
            }],
            repo_overrides: Vec::new(),
            tags: Vec::new(),
        };
        let root_module = super::RootBzlmodModule {
            name: "buildbuddy".to_owned(),
            version: String::new(),
            repo_name: "buildbuddy".to_owned(),
            canonical_repo_name: String::new(),
            lockfile_extension_generated_repos: std::collections::BTreeMap::new(),
            lockfile_extension_facts: std::collections::BTreeSet::new(),
            constants: Vec::new(),
            extension_usages: Vec::new(),
            use_repo_rule_invocations: Vec::new(),
            registered_toolchains: Vec::new(),
        };

        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "bzlmod_dep_",
            "aspect_rules_js",
            "bzlmod_aspect_rules_js_",
        );

        let mut extension_unique_names = std::collections::BTreeMap::new();
        extension_unique_names.insert(
            super::BzlmodExtensionId {
                bzl_cell_name: "bzlmod_aspect_rules_js_".to_owned(),
                bzl_path: "npm/extensions.bzl".to_owned(),
                extension_name: "npm".to_owned(),
            },
            "aspect_rules_js++npm".to_owned(),
        );

        let mut canonical_repo_names_by_cell = std::collections::BTreeMap::new();
        canonical_repo_names_by_cell.insert(
            "bzlmod_aspect_rules_js_".to_owned(),
            "aspect_rules_js+".to_owned(),
        );

        let mut generated = Vec::new();
        let mut generated_repo_declaring_cells = Vec::new();
        let mut extension_generated_repo_groups = std::collections::BTreeMap::new();
        let mut extension_repo_override_groups = std::collections::BTreeMap::new();
        let mut extension_usages_json_by_id = std::collections::BTreeMap::new();
        insert_test_extension_usages_json(
            &mut extension_usages_json_by_id,
            super::BzlmodExtensionId {
                bzl_cell_name: "bzlmod_aspect_rules_js_".to_owned(),
                bzl_path: "npm/extensions.bzl".to_owned(),
                extension_name: "npm".to_owned(),
            },
        )?;

        super::resolve_bzlmod_extension_usage_generated_repos(
            &usage,
            "dep+",
            "bzlmod_dep_",
            false,
            &root_module,
            &mut cell_aliases_by_cell,
            &canonical_repo_names_by_cell,
            &extension_unique_names,
            &mut extension_usages_json_by_id,
            &mut generated,
            &mut generated_repo_declaring_cells,
            &mut extension_generated_repo_groups,
            &mut extension_repo_override_groups,
        )?;

        assert_eq!(
            super::bzlmod_cell_alias_target(&cell_aliases_by_cell, "bzlmod_dep_", "npm"),
            Some("bzlmod_aspect_rules_js__npm_npm")
        );

        Ok(())
    }

    #[test]
    fn test_bzlmod_lockfile_extension_generated_repos() -> bz_error::Result<()> {
        let lockfile_data = super::bzlmod_lockfile_data_from_str(indoc!(
            r#"
            {
              "facts": {
                "@@rules_go+//go:extensions.bzl%go_sdk": {
                  "1.23.5": {}
                }
              },
              "moduleExtensions": {
                "@@rules_go+//go:extensions.bzl%go_sdk": {
                  "general": {
                    "generatedRepoSpecs": {
                      "go_toolchains": {},
                      "main___download_0": {}
                    }
                  },
                  "os:darwin": {
                    "generatedRepoSpecs": {
                      "darwin_only": {}
                    }
                  }
                },
                "@@googleapis+//:extensions.bzl%switched_rules": {
                  "general": {
                    "generatedRepoSpecs": {}
                  }
                }
              }
            }
            "#
        ))?;
        let repos = lockfile_data.extension_generated_repos;

        assert_eq!(
            repos
                .get("@@rules_go+//go:extensions.bzl%go_sdk")
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                "darwin_only".to_owned(),
                "go_toolchains".to_owned(),
                "main___download_0".to_owned(),
            ]
        );
        assert!(
            lockfile_data
                .extension_facts
                .contains("@@rules_go+//go:extensions.bzl%go_sdk")
        );
        assert_eq!(
            repos
                .get("@@googleapis+//:extensions.bzl%switched_rules")
                .unwrap(),
            &std::collections::BTreeSet::new()
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_lockfile_extension_key() -> bz_error::Result<()> {
        let mut canonical_repo_names_by_cell = std::collections::BTreeMap::new();
        let rules_go_cell = bzlmod_cell_name("rules_go+");
        canonical_repo_names_by_cell.insert(rules_go_cell.clone(), "rules_go+".to_owned());

        assert_eq!(
            super::bzlmod_lockfile_extension_key(
                &super::BzlmodExtensionId {
                    bzl_cell_name: "root".to_owned(),
                    bzl_path: "repositories.bzl".to_owned(),
                    extension_name: "async_profiler_repos".to_owned(),
                },
                &canonical_repo_names_by_cell,
            )?,
            "//:repositories.bzl%async_profiler_repos"
        );
        assert_eq!(
            super::bzlmod_lockfile_extension_key(
                &super::BzlmodExtensionId {
                    bzl_cell_name: rules_go_cell,
                    bzl_path: "go/extensions.bzl".to_owned(),
                    extension_name: "go_sdk".to_owned(),
                },
                &canonical_repo_names_by_cell,
            )?,
            "@@rules_go+//go:extensions.bzl%go_sdk"
        );
        Ok(())
    }

    #[test]
    fn test_bzlmod_registered_toolchains_resolve_declaring_repo_mapping() -> bz_error::Result<()>
    {
        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        let rules_go_cell = bzlmod_cell_name("rules_go+0.57.0");
        let go_toolchains_cell = bzlmod_cell_name("rules_go+0.57.0+go_sdk+go_toolchains");
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            &rules_go_cell,
            "go_toolchains",
            &go_toolchains_cell,
        );

        assert_eq!(
            super::qualify_bzlmod_registered_toolchain(
                "@go_toolchains//:all",
                &rules_go_cell,
                &cell_aliases_by_cell,
            )?,
            format!("{go_toolchains_cell}//:all")
        );
        assert_eq!(
            super::qualify_bzlmod_registered_toolchain(
                "//:all",
                &rules_go_cell,
                &cell_aliases_by_cell,
            )?,
            format!("{rules_go_cell}//:all")
        );

        Ok(())
    }

    #[test]
    fn test_bzlmod_registered_toolchains_include_root_module() -> bz_error::Result<()> {
        let root_module = super::RootBzlmodModule {
            name: "root".to_owned(),
            version: String::new(),
            repo_name: "root".to_owned(),
            canonical_repo_name: String::new(),
            lockfile_extension_generated_repos: std::collections::BTreeMap::new(),
            lockfile_extension_facts: std::collections::BTreeSet::new(),
            constants: Vec::new(),
            extension_usages: Vec::new(),
            use_repo_rule_invocations: Vec::new(),
            registered_toolchains: vec![
                "@rust_toolchains//:all".to_owned(),
                "//tools:toolchain".to_owned(),
            ],
        };
        let mut cell_aliases_by_cell = super::BzlmodCellAliasesByCell::default();
        super::add_bzlmod_cell_alias(
            &mut cell_aliases_by_cell,
            "root",
            "rust_toolchains",
            "bzlmod_rules_rust__rust_rust_toolchains",
        );

        assert_eq!(
            super::resolve_bzlmod_registered_toolchains(
                &root_module,
                &std::collections::BTreeMap::new(),
                &[],
                &std::collections::BTreeMap::new(),
                &cell_aliases_by_cell,
            )?,
            vec![
                "bzlmod_rules_rust__rust_rust_toolchains//:all",
                "root//tools:toolchain",
            ]
        );

        Ok(())
    }
}
