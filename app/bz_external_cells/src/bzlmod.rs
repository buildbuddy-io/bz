use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::future::Future;
use std::io::ErrorKind;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use bz_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use bz_common::bazel::bzlmod::BZLMOD_REPOSITORY_OS_ARCH_ENV;
use bz_common::bazel::bzlmod::BZLMOD_REPOSITORY_OS_NAME_ENV;
use bz_common::bazel::bzlmod::BzlmodModuleExtensionRepoMappingBase;
use bz_common::bazel::bzlmod::GetBzlmodModuleExtensionRepoMappingBase;
use bz_common::bazel::bzlmod::GetBzlmodRepositoryEnvironmentVariable;
use bz_common::bazel::bzlmod::archive::archive_kind_from_type_or_url;
use bz_common::bazel::bzlmod::archive::extract_archive as extract_bazel_archive;
use bz_common::bazel::bzlmod::integrity::BzlmodIntegrityKind;
use bz_common::bazel::bzlmod::integrity::parse_bzlmod_integrity;
use bz_common::bazel::bzlmod::patch::apply_unified_patch_file;
use bz_common::cas_digest::DigestAlgorithm;
use bz_common::dice::data::HasIoProvider;
use bz_common::file_ops::delegate::FileOpsDelegate;
use bz_common::file_ops::dice::ReadFileProxy;
use bz_common::file_ops::metadata::FileDigest;
use bz_common::file_ops::metadata::FileDigestConfig;
use bz_common::file_ops::metadata::FileType;
use bz_common::file_ops::metadata::RawDirEntry;
use bz_common::file_ops::metadata::RawPathMetadata;
use bz_common::file_ops::metadata::RawPathMetadataForNoWatchFs;
use bz_common::file_ops::metadata::RawSymlink;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_common::init::BUILDBUDDY_API_KEY_HEADER;
use bz_common::init::RemoteExecutionStartupConfig;
use bz_common::io::IoProvider;
use bz_common::io::NoWatchFsMetadataCache;
use bz_common::io::fs::FsIoProvider;
use bz_core::cells::cell_path::CellPath;
use bz_core::cells::external::BZLMOD_EXTERNAL_CELL_KIND;
use bz_core::cells::external::BZLMOD_GENERATED_EXTERNAL_CELL_PATH_MARKER;
use bz_core::cells::external::BzlmodBazelFeaturesGlobalsSetup;
use bz_core::cells::external::BzlmodBazelFeaturesVersionSetup;
use bz_core::cells::external::BzlmodCcAutoconfSetup;
use bz_core::cells::external::BzlmodCcAutoconfToolchainsSetup;
use bz_core::cells::external::BzlmodCellSetup;
use bz_core::cells::external::BzlmodGeneratedCellGenerator;
use bz_core::cells::external::BzlmodGeneratedCellSetup;
use bz_core::cells::external::BzlmodHostPlatformSetup;
use bz_core::cells::external::BzlmodHttpArchiveSetup;
use bz_core::cells::external::BzlmodModuleExtensionRepoSetup;
use bz_core::cells::external::BzlmodPythonHubSetup;
use bz_core::cells::external::BzlmodRepositoryRuleFile;
use bz_core::cells::external::BzlmodRepositoryRuleInvocationSetup;
use bz_core::cells::external::BzlmodRepositoryRuleSetup;
use bz_core::cells::external::BzlmodShellConfigSetup;
use bz_core::cells::external::BzlmodXcodeConfigSetup;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use bz_core::cells::external::bzlmod_cell_aliases_for_cell;
use bz_core::cells::external::bzlmod_cell_name;
use bz_core::cells::external::external_cell_source_path;
use bz_core::cells::external::register_bzlmod_cell_aliases;
use bz_core::cells::external::register_bzlmod_cell_canonical_repo_name_for_cell;
use bz_core::cells::external::register_external_cell_origin;
use bz_core::cells::name::CellName;
use bz_core::cells::paths::CellRelativePath;
use bz_core::fs::buck_out_path::BuckOutPathResolver;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_directory::directory::directory::Directory;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_events::dispatch::span_async_simple;
use bz_events::dispatch::span_simple;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest_config::HasDigestConfig;
use bz_execute::directory::ActionDirectoryEntry;
use bz_execute::directory::ActionDirectoryMember;
use bz_execute::directory::INTERNER;
use bz_execute::entry::build_entry_from_disk;
use bz_execute::execute::blocking::HasBlockingExecutor;
use bz_execute::execute::blocking::IoRequest;
use bz_execute::materialize::http::Checksum;
use bz_execute::materialize::http::bazel_repository_download_headers;
use bz_execute::materialize::http::http_download_with_headers;
use bz_execute::materialize::materializer::DeclareArtifactPayload;
use bz_execute::materialize::materializer::HasMaterializer;
use bz_execute::materialize::materializer::Materializer;
use bz_fs::error::IoResultExt;
use bz_fs::fs_util;
use bz_fs::paths::abs_norm_path::AbsNormPath;
use bz_fs::paths::abs_path::AbsPath;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use bz_http::HttpClient;
use bz_http::HttpClientBuilder;
use bz_interpreter_for_build::bazel::repository::BazelRepositoryRuleCacheInfo;
use bz_interpreter_for_build::bazel::repository::BazelRepositoryRuleProgress;
use bz_interpreter_for_build::bazel::repository::bzlmod_module_extension_bazel_bzl_transitive_digest;
use bz_interpreter_for_build::bazel::repository::bzlmod_module_extension_bazel_usages_digest;
use bz_interpreter_for_build::bazel::repository::bzlmod_module_extension_eval_factor_deps;
use bz_interpreter_for_build::bazel::repository::bzlmod_repository_rule_cache_info;
use bz_interpreter_for_build::bazel::repository::bzlmod_repository_rule_invocation_from_setup;
use bz_interpreter_for_build::bazel::repository::bzlmod_repository_rule_invocation_to_setup;
use bz_interpreter_for_build::bazel::repository::evaluate_bzlmod_module_extension_repo;
use bz_interpreter_for_build::bazel::repository::evaluate_bzlmod_repository_rule_with_recorded_inputs;
use bz_interpreter_for_build::interpreter::build_context::BazelModuleExtensionEvaluationResult;
use bz_interpreter_for_build::interpreter::build_context::BazelRepositoryRecordedInput;
use bz_interpreter_for_build::interpreter::build_context::BazelRepositoryRuleInvocation;
use cmp_any::PartialEqAny;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::Key;
use dice::NoValueSerialize;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use pagable::Pagable;
use pagable::pagable_typetag;
use prost::Message;
use re_grpc_proto::build::bazel::remote::execution::v2::Digest as RemoteExecutionDigest;
use re_grpc_proto::google::bytestream::ReadRequest;
use re_grpc_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use re_grpc_proto::google::rpc::Status as RemoteAssetStatus;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tonic::metadata::MetadataValue;
use tonic::transport::ClientTlsConfig;
use tonic::transport::Endpoint;

mod generated_repos;

use self::generated_repos::host_macos_sdk_version;
use self::generated_repos::translate_host_platform_cpu_constraint;
use self::generated_repos::translate_host_platform_os_constraint;
use self::generated_repos::write_bazel_features_globals_repo;
use self::generated_repos::write_bazel_features_version_repo;
use self::generated_repos::write_cc_autoconf_repo;
use self::generated_repos::write_cc_autoconf_toolchains_repo;
use self::generated_repos::write_host_platform_repo;
use self::generated_repos::write_python_hub_repo;
use self::generated_repos::write_repository_rule_repo;
use self::generated_repos::write_shell_config_repo;
use self::generated_repos::write_xcode_config_repo;

#[cfg(test)]
use self::generated_repos::host_cc_cpu_value;
#[cfg(test)]
use self::generated_repos::host_platform_cpu_constraint;
#[cfg(test)]
use self::generated_repos::host_platform_os_constraint;
#[cfg(test)]
use self::generated_repos::local_config_cc_toolchains_build_file;
#[cfg(test)]
use self::generated_repos::parse_bazel_features_version_pair;

static BZLMOD_MATERIALIZATION_LOCKS: OnceLock<
    Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
> = OnceLock::new();
static BZLMOD_HIDDEN_LOCKFILE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static BZLMOD_MODULE_EXTENSION_REPO_MAPPING_REGISTRATIONS: OnceLock<
    Mutex<BTreeMap<String, [u8; 32]>>,
> = OnceLock::new();
static BZLMOD_DOWNLOAD_HTTP_CLIENT: LazyLock<tokio::sync::OnceCell<HttpClient>> =
    LazyLock::new(tokio::sync::OnceCell::new);
static BZLMOD_DOWNLOAD_SEMAPHORE: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(BZLMOD_DOWNLOAD_MAX_PARALLEL_DOWNLOADS));
static BZLMOD_GENERATED_CACHE_ENTRY_COUNTER: AtomicU64 = AtomicU64::new(0);
static BZLMOD_CACHE_ALIAS_COUNTER: AtomicU64 = AtomicU64::new(0);
static BZLMOD_GENERATED_MATERIALIZATION_VALUE_COUNTER: AtomicU64 = AtomicU64::new(0);

const BZLMOD_DOWNLOAD_MAX_PARALLEL_DOWNLOADS: usize = 8;
const BZLMOD_DOWNLOAD_MAX_REDIRECTS: usize = 40;
const BZLMOD_DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const BZLMOD_DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(20);
const BZLMOD_DOWNLOAD_WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const BZLMOD_DOWNLOAD_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const BZLMOD_REPOSITORY_MATERIALIZATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const BZLMOD_GENERATED_RECORDED_INPUTS_SUFFIX: &str = ".recorded_inputs.json";
const BZLMOD_GENERATED_LATEST_ENTRY: &str = "latest";

type BzlmodProgressState = Arc<Mutex<String>>;

#[derive(bz_error::Error, Debug)]
#[buck2(tag = Tier0)]
enum BzlmodError {
    #[error("Unsupported bzlmod archive type for `{0}`")]
    UnsupportedArchiveType(String),
    #[error("Expected extracted bzlmod module directory at `{0}`")]
    MissingExtractedDirectory(String),
    #[error("Expected bzlmod materialization to create a directory")]
    NoDirectory,
    #[error("Invalid generated bzlmod repo path `{0}`")]
    InvalidGeneratedRepoPath(String),
    #[error("Could not find `{dict}` in bazel_features globals at `{path}`")]
    MissingBazelFeaturesGlobalsDict { path: String, dict: &'static str },
    #[error("Could not download bzlmod archive from any URL {urls:?}: {error}")]
    DownloadFailed { urls: Vec<String>, error: String },
    #[error("Invalid remote downloader endpoint `{endpoint}`: {error}")]
    InvalidRemoteDownloaderEndpoint { endpoint: String, error: String },
    #[error("Remote downloader failed for URLs {urls:?}: {error}")]
    RemoteDownloaderFailed { urls: Vec<String>, error: String },
    #[error("Remote downloader did not return a CAS blob digest for URLs {urls:?}")]
    RemoteDownloaderMissingDigest { urls: Vec<String> },
    #[error("Remote downloader returned non-OK status for URLs {urls:?}: code {code}: {message}")]
    RemoteDownloaderStatus {
        urls: Vec<String>,
        code: i32,
        message: String,
    },
    #[error(
        "Timed out materializing bzlmod repository `{repo}` at `{path}` after {timeout_secs}s; last progress: {progress}"
    )]
    RepositoryMaterializationTimedOut {
        repo: String,
        path: String,
        timeout_secs: u64,
        progress: String,
    },
    #[error(
        "bzlmod module extension repo `{repo_name}` from `{parent_canonical_repo_name}` extension `{extension_bzl_file}%{extension_name}` cannot be materialized until module_extension evaluation is wired to repository_rule execution"
    )]
    ModuleExtensionRepoNotMaterialized {
        parent_canonical_repo_name: String,
        extension_bzl_file: String,
        extension_name: String,
        repo_name: String,
    },
    #[error(
        "bzlmod repository_rule invocation for `{repo_name}` cannot be materialized without repository_rule execution"
    )]
    RepositoryRuleInvocationNotMaterialized { repo_name: String },
    #[error(
        "bzlmod module extension `{extension_bzl_file}%{extension_name}` did not emit repository `{repo_name}`; emitted repositories: {}",
        emitted.join(", ")
    )]
    ModuleExtensionRepoNotEmitted {
        extension_bzl_file: String,
        extension_name: String,
        repo_name: String,
        emitted: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionValidationConfig {
    #[serde(default)]
    usages: Vec<BzlmodModuleExtensionValidationUsage>,
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionValidationUsage {
    #[serde(default)]
    imports: Vec<BzlmodModuleExtensionValidationImport>,
    #[serde(default)]
    repo_overrides: Vec<BzlmodModuleExtensionValidationRepoOverride>,
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionValidationImport {
    alias: String,
    repo_name: String,
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionValidationRepoOverride {
    repo_name: String,
    must_exist: bool,
}

fn bzlmod_materialization_lock(path: &ProjectRelativePath) -> Arc<tokio::sync::Mutex<()>> {
    let locks = BZLMOD_MATERIALIZATION_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    locks
        .lock()
        .expect("bzlmod materialization locks poisoned")
        .entry(path.as_str().to_owned())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .dupe()
}

fn bzlmod_report_progress(progress_state: &BzlmodProgressState, progress: impl Into<String>) {
    let progress = progress.into();
    *progress_state
        .lock()
        .expect("bzlmod progress state poisoned") = progress.clone();
    bz_events::dispatch::instant_event(bz_data::BzlmodProgress { progress });
}

fn bzlmod_current_progress(progress_state: &BzlmodProgressState) -> String {
    progress_state
        .lock()
        .expect("bzlmod progress state poisoned")
        .clone()
}

async fn bzlmod_progress_step<T>(
    progress_state: &BzlmodProgressState,
    progress: impl Into<String>,
    fut: impl Future<Output = T>,
) -> T {
    bzlmod_report_progress(progress_state, progress);
    fut.await
}

#[derive(Clone)]
struct BzlmodRemoteDownloaderConfig {
    endpoint: String,
    api_key: Option<String>,
}

#[derive(Clone)]
struct ParsedRemoteAssetEndpoint {
    uri: String,
    tls: bool,
}

impl ParsedRemoteAssetEndpoint {
    fn parse(endpoint: &str) -> bz_error::Result<Self> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return Err(BzlmodError::InvalidRemoteDownloaderEndpoint {
                endpoint: endpoint.to_owned(),
                error: "empty endpoint".to_owned(),
            }
            .into());
        }
        if let Some(rest) = endpoint.strip_prefix("grpc://") {
            return Ok(Self {
                uri: format!("http://{rest}"),
                tls: false,
            });
        }
        if let Some(rest) = endpoint.strip_prefix("grpcs://") {
            return Ok(Self {
                uri: format!("https://{rest}"),
                tls: true,
            });
        }
        if endpoint.starts_with("http://") {
            return Ok(Self {
                uri: endpoint.to_owned(),
                tls: false,
            });
        }
        if endpoint.starts_with("https://") {
            return Ok(Self {
                uri: endpoint.to_owned(),
                tls: true,
            });
        }
        Ok(Self {
            uri: format!("https://{endpoint}"),
            tls: true,
        })
    }
}

#[derive(Clone, PartialEq, Message)]
struct RemoteAssetQualifier {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, Message)]
struct FetchBlobRequest {
    #[prost(string, tag = "1")]
    instance_name: String,
    #[prost(message, optional, tag = "2")]
    timeout: Option<prost_types::Duration>,
    #[prost(message, optional, tag = "3")]
    oldest_content_accepted: Option<prost_types::Timestamp>,
    #[prost(string, repeated, tag = "4")]
    uris: Vec<String>,
    #[prost(message, repeated, tag = "5")]
    qualifiers: Vec<RemoteAssetQualifier>,
    #[prost(enumeration = "RemoteExecutionDigestFunction", tag = "6")]
    digest_function: i32,
}

#[derive(Clone, PartialEq, Message)]
struct FetchBlobResponse {
    #[prost(message, optional, tag = "1")]
    status: Option<RemoteAssetStatus>,
    #[prost(string, tag = "2")]
    uri: String,
    #[prost(message, repeated, tag = "3")]
    qualifiers: Vec<RemoteAssetQualifier>,
    #[prost(message, optional, tag = "4")]
    expires_at: Option<prost_types::Timestamp>,
    #[prost(message, optional, tag = "5")]
    blob_digest: Option<RemoteExecutionDigest>,
    #[prost(enumeration = "RemoteExecutionDigestFunction", tag = "6")]
    digest_function: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
enum RemoteExecutionDigestFunction {
    Unknown = 0,
    Sha256 = 1,
}

fn bzlmod_remote_downloader_config(
    ctx: &DiceComputations<'_>,
) -> Option<BzlmodRemoteDownloaderConfig> {
    let startup_config = ctx
        .per_transaction_data()
        .data
        .get::<RemoteExecutionStartupConfig>()
        .ok()?;
    let endpoint = startup_config
        .remote_downloader
        .as_ref()
        .filter(|endpoint| !endpoint.trim().is_empty())?;
    Some(BzlmodRemoteDownloaderConfig {
        endpoint: endpoint.clone(),
        api_key: startup_config.buildbuddy_api_key.clone(),
    })
}

fn bzlmod_generated_repo_kind(setup: &BzlmodGeneratedCellSetup) -> &'static str {
    match &setup.generator {
        BzlmodGeneratedCellGenerator::BazelFeaturesGlobals(_) => "bazel_features globals",
        BzlmodGeneratedCellGenerator::BazelFeaturesVersion(_) => "bazel_features version",
        BzlmodGeneratedCellGenerator::HostPlatform(_) => "host platform",
        BzlmodGeneratedCellGenerator::CcAutoconfToolchains(_) => "cc autoconf toolchains",
        BzlmodGeneratedCellGenerator::CcAutoconf(_) => "cc autoconf",
        BzlmodGeneratedCellGenerator::XcodeConfig(_) => "xcode config",
        BzlmodGeneratedCellGenerator::ShellConfig(_) => "shell config",
        BzlmodGeneratedCellGenerator::HttpArchive(_) => "http archive",
        BzlmodGeneratedCellGenerator::PythonHub(_) => "python hub",
        BzlmodGeneratedCellGenerator::RepositoryRule(_) => "repository rule",
        BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(_) => "repository rule invocation",
        BzlmodGeneratedCellGenerator::ModuleExtensionRepo(_) => "module extension repo",
    }
}

#[derive(Debug)]
struct BzlmodGeneratedRepoContentsCacheCandidate {
    entry_name: String,
    repo: ProjectRelativePathBuf,
    recorded_inputs_path: ProjectRelativePathBuf,
    recorded_inputs: Vec<BazelRepositoryRecordedInput>,
}

#[derive(Debug)]
struct BzlmodGeneratedMaterializationResult {
    cacheable: bool,
}

#[derive(Debug)]
struct BzlmodRepositoryRuleMaterializationResult {
    recorded_inputs: Vec<BazelRepositoryRecordedInput>,
    reproducible: bool,
}

async fn run_bzlmod_cache_io<T>(
    op: impl FnOnce() -> bz_error::Result<T> + Send + 'static,
) -> bz_error::Result<T>
where
    T: Send + 'static,
{
    // Cache-hit preparation can run during command setup before per-command Dice
    // data has installed a BlockingExecutor.
    tokio::task::spawn_blocking(op)
        .await
        .buck_error_context("Failed to spawn bzlmod cache IO")?
}

struct BzlmodExtractIoRequest {
    setup: BzlmodCellSetup,
    archive: ProjectRelativePathBuf,
    patch_files: Vec<BzlmodPatchFile>,
    overlay_files: Vec<BzlmodOverlayFile>,
    temp: ProjectRelativePathBuf,
    cache_repo: ProjectRelativePathBuf,
    cache_tmp: ProjectRelativePathBuf,
    cache_alias: ProjectRelativePathBuf,
    dest: ProjectRelativePathBuf,
}

struct BzlmodPatchFile {
    path: ProjectRelativePathBuf,
    patch_strip: u32,
}

struct BzlmodOverlayFile {
    path: String,
    file: ProjectRelativePathBuf,
}

impl IoRequest for BzlmodExtractIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> bz_error::Result<()> {
        if bzlmod_repo_contents_cache_exists(project_fs, &self.cache_repo)? {
            record_bzlmod_repo_contents_cache_alias(
                project_fs,
                &self.cache_alias,
                &self.cache_repo,
            )?;
            prepare_bzlmod_external_cell_root(project_fs, &self.cache_repo, &self.dest)?;
            return Ok(());
        }

        let archive = project_fs.resolve(&self.archive);
        let temp = project_fs.resolve(&self.temp);
        let cache_tmp = project_fs.resolve(&self.cache_tmp);
        let cache_repo = project_fs.resolve(&self.cache_repo);

        fs_util::remove_all(&cache_tmp).categorize_internal()?;
        fs_util::remove_all(&temp).categorize_internal()?;
        fs_util::create_dir_all(temp.clone())?;
        fs_util::create_dir_all(cache_tmp.clone())?;

        extract_archive(&self.setup, &archive, &temp)?;
        copy_dir_contents(&temp, &cache_tmp)?;

        for patch in &self.patch_files {
            apply_patch(project_fs, &cache_tmp, &patch.path, patch.patch_strip)?;
        }

        for overlay in &self.overlay_files {
            let overlay_path = ForwardRelativePath::new(&overlay.path)?;
            let dest = cache_tmp.join(overlay_path);
            if let Some(parent) = dest.parent() {
                fs_util::create_dir_all(parent)?;
            }
            fs_util::remove_all(&dest).categorize_internal()?;
            link_or_copy_file(&project_fs.resolve(&overlay.file), &dest)?;
        }

        if let Some(parent) = cache_repo.parent() {
            fs_util::create_dir_all(parent)?;
        }
        if !cache_repo.exists() {
            match fs_util::rename(&cache_tmp, &cache_repo) {
                Ok(()) => {}
                Err(error) if cache_repo.exists() => {
                    fs_util::remove_all(&cache_tmp).categorize_internal()?;
                    drop(error);
                }
                Err(error) => return Err(error.categorize_internal()),
            }
        } else {
            fs_util::remove_all(&cache_tmp).categorize_internal()?;
        }

        record_bzlmod_repo_contents_cache_alias(project_fs, &self.cache_alias, &self.cache_repo)?;
        prepare_bzlmod_external_cell_root(project_fs, &self.cache_repo, &self.dest)?;

        Ok(())
    }
}

struct BzlmodPrepareExternalCellRootIoRequest {
    cache_repo: ProjectRelativePathBuf,
    cache_alias: Option<ProjectRelativePathBuf>,
    dest: ProjectRelativePathBuf,
}

impl IoRequest for BzlmodPrepareExternalCellRootIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> bz_error::Result<()> {
        if let Some(cache_alias) = &self.cache_alias {
            record_bzlmod_repo_contents_cache_alias(project_fs, cache_alias, &self.cache_repo)?;
        }
        prepare_bzlmod_external_cell_root(project_fs, &self.cache_repo, &self.dest)
    }
}

struct BzlmodGeneratedIoRequest {
    setup: BzlmodGeneratedCellSetup,
    dest: ProjectRelativePathBuf,
}

struct BzlmodGeneratedPublishIoRequest {
    src: ProjectRelativePathBuf,
    dest: ProjectRelativePathBuf,
    cleanup: Vec<ProjectRelativePathBuf>,
}

struct BzlmodGeneratedHttpArchiveIoRequest {
    setup: BzlmodHttpArchiveSetup,
    archive: ProjectRelativePathBuf,
    temp: ProjectRelativePathBuf,
    dest: ProjectRelativePathBuf,
}

struct BzlmodGeneratedNormalizeIoRequest {
    canonical_repo_name: Arc<str>,
    dest: ProjectRelativePathBuf,
}

impl IoRequest for BzlmodGeneratedHttpArchiveIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> bz_error::Result<()> {
        let archive = project_fs.resolve(&self.archive);
        let temp = project_fs.resolve(&self.temp);
        let dest = project_fs.resolve(&self.dest);

        fs_util::create_dir_all(temp.clone())?;
        fs_util::create_dir_all(dest.clone())?;

        let extract_setup = BzlmodCellSetup {
            module_name: self.setup.repo_name.dupe(),
            version: Arc::from(""),
            canonical_repo_name: self.setup.repo_name.dupe(),
            local_path: None,
            url: self.setup.url.dupe(),
            urls: Arc::new(vec![self.setup.url.dupe()]),
            integrity: Arc::from(""),
            strip_prefix: self.setup.strip_prefix.dupe(),
            archive_type: self.setup.archive_type.dupe(),
            patches: Arc::new(Vec::new()),
            overlays: Arc::new(Vec::new()),
            patch_strip: 0,
        };
        extract_archive(&extract_setup, &archive, &temp)?;
        copy_dir_contents(&temp, &dest)?;
        normalize_generated_repository(&self.setup.repo_name, &dest)?;
        write_generated_module_file(&dest, &self.setup.repo_name)?;
        Ok(())
    }
}

impl IoRequest for BzlmodGeneratedNormalizeIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> bz_error::Result<()> {
        let dest = project_fs.resolve(&self.dest);
        normalize_generated_repository(&self.canonical_repo_name, &dest)
    }
}

impl IoRequest for BzlmodGeneratedPublishIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> bz_error::Result<()> {
        let src = project_fs.resolve(&self.src);
        let dest = project_fs.resolve(&self.dest);
        if let Some(parent) = dest.parent() {
            fs_util::create_dir_all(parent)?;
        }
        fs_util::remove_all(&dest).categorize_internal()?;
        fs_util::rename(&src, &dest).categorize_internal()?;
        for path in self.cleanup {
            fs_util::remove_all(project_fs.resolve(path)).categorize_internal()?;
        }
        Ok(())
    }
}

impl IoRequest for BzlmodGeneratedIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> bz_error::Result<()> {
        let dest = project_fs.resolve(&self.dest);
        fs_util::create_dir_all(dest.clone())?;
        match &self.setup.generator {
            BzlmodGeneratedCellGenerator::BazelFeaturesGlobals(setup) => {
                write_bazel_features_globals_repo(project_fs, &self.dest, &dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::BazelFeaturesVersion(setup) => {
                write_bazel_features_version_repo(&dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::HostPlatform(setup) => {
                write_host_platform_repo(&dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::CcAutoconfToolchains(setup) => {
                write_cc_autoconf_toolchains_repo(project_fs, &self.dest, &dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::CcAutoconf(setup) => {
                write_cc_autoconf_repo(&dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::XcodeConfig(setup) => {
                write_xcode_config_repo(&dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::ShellConfig(setup) => {
                write_shell_config_repo(&dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::HttpArchive(setup) => {
                write_generated_module_file(&dest, &setup.repo_name)?;
            }
            BzlmodGeneratedCellGenerator::PythonHub(setup) => {
                write_python_hub_repo(&dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::RepositoryRule(setup) => {
                write_repository_rule_repo(
                    project_fs,
                    &dest,
                    &self.setup.canonical_repo_name,
                    setup,
                )?;
            }
            BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(setup) => {
                return Err(repository_rule_invocation_not_materialized(setup));
            }
            BzlmodGeneratedCellGenerator::ModuleExtensionRepo(setup) => {
                return Err(module_extension_repo_not_materialized(setup));
            }
        }
        Ok(())
    }
}

fn module_extension_repo_not_materialized(
    setup: &BzlmodModuleExtensionRepoSetup,
) -> bz_error::Error {
    BzlmodError::ModuleExtensionRepoNotMaterialized {
        parent_canonical_repo_name: setup.parent_canonical_repo_name.to_string(),
        extension_bzl_file: setup.extension_bzl_file.to_string(),
        extension_name: setup.extension_name.to_string(),
        repo_name: setup.repo_name.to_string(),
    }
    .into()
}

fn repository_rule_invocation_not_materialized(
    setup: &BzlmodRepositoryRuleInvocationSetup,
) -> bz_error::Error {
    BzlmodError::RepositoryRuleInvocationNotMaterialized {
        repo_name: setup.repo_name.to_string(),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use bz_core::cells::external::BzlmodPatch;
    use bz_core::fs::project::ProjectRootTemp;

    use super::*;

    #[test]
    fn parses_bazel_features_simple_global_version() {
        assert_eq!(
            parse_bazel_features_version_pair("\"6.4.0\""),
            Some(("6.4.0".to_owned(), String::new()))
        );
    }

    #[test]
    fn parses_bazel_features_version_range() {
        assert_eq!(
            parse_bazel_features_version_pair("(\"1.0.0\", \"2.0.0\")"),
            Some(("1.0.0".to_owned(), "2.0.0".to_owned()))
        );
    }

    #[test]
    fn bzlmod_repo_contents_cache_key_includes_local_patch_content_sha256() {
        fn setup(content_sha256: &str) -> BzlmodCellSetup {
            BzlmodCellSetup {
                module_name: Arc::from("module"),
                version: Arc::from("1.0.0"),
                canonical_repo_name: Arc::from("module~1.0.0"),
                local_path: None,
                url: Arc::from("https://example.com/source.tar.gz"),
                urls: Arc::new(vec![Arc::from("https://example.com/source.tar.gz")]),
                integrity: Arc::from("sha256-YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXowMTIzNDU="),
                strip_prefix: None,
                archive_type: None,
                patches: Arc::new(vec![BzlmodPatch {
                    url: Arc::from(""),
                    integrity: Arc::from(""),
                    path: Some(Arc::from("patches/fix.patch")),
                    content_sha256: Some(Arc::from(content_sha256)),
                    patch_strip: 1,
                }]),
                overlays: Arc::new(Vec::new()),
                patch_strip: 0,
            }
        }

        assert_ne!(
            bzlmod_repo_contents_cache_key(&setup(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )),
            bzlmod_repo_contents_cache_key(&setup(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ))
        );
    }

    #[test]
    fn bzlmod_generated_repo_contents_cache_key_matches_stamp() {
        let setup = BzlmodGeneratedCellSetup {
            canonical_repo_name: Arc::from("gazelle++go_deps+example"),
            generator: BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(
                BzlmodRepositoryRuleInvocationSetup {
                    repo_name: Arc::from("example"),
                    rule_bzl_cell: Arc::from("gazelle"),
                    rule_bzl_path: Arc::from("internal/go_repository.bzl"),
                    rule_bzl_build_file_cell: Arc::from("gazelle"),
                    rule_bzl_build_file_package: None,
                    rule_name: Arc::from("go_repository"),
                    attrs: Arc::new(vec![(
                        Arc::from("importpath"),
                        Arc::from("\"example.com/lib\""),
                    )]),
                },
            ),
        };

        let key = bzlmod_generated_repo_contents_cache_key(&setup);
        assert_eq!(
            format!("{key}\n"),
            bzlmod_generated_materialization_stamp_content(&setup)
        );
        assert_eq!(64, key.len());
    }

    #[test]
    fn bzlmod_host_platform_translates_bazel_constraints() {
        assert_eq!(
            translate_host_platform_cpu_constraint("amd64"),
            Some("x86_64")
        );
        assert_eq!(
            translate_host_platform_cpu_constraint("armv7l"),
            Some("arm")
        );
        assert_eq!(
            translate_host_platform_os_constraint("mac os x"),
            Some("osx")
        );
        assert_eq!(
            translate_host_platform_os_constraint("linux"),
            Some("linux")
        );
        assert_eq!(translate_host_platform_os_constraint("plan9"), None);
    }

    #[test]
    fn bzlmod_host_platform_cache_key_includes_constraints() {
        fn setup(cpu_constraint: &str, os_constraint: &str) -> BzlmodGeneratedCellSetup {
            BzlmodGeneratedCellSetup {
                canonical_repo_name: Arc::from("platforms+host_platform+host_platform"),
                generator: BzlmodGeneratedCellGenerator::HostPlatform(BzlmodHostPlatformSetup {
                    cpu_constraint: Some(Arc::from(cpu_constraint)),
                    os_constraint: Some(Arc::from(os_constraint)),
                }),
            }
        }

        assert_ne!(
            bzlmod_generated_repo_contents_cache_key(&setup("aarch64", "osx")),
            bzlmod_generated_repo_contents_cache_key(&setup("x86_64", "linux"))
        );
    }

    #[test]
    fn bzlmod_host_platform_repo_writes_effective_constraints() -> bz_error::Result<()> {
        let project_root = ProjectRootTemp::new()?;
        let dest_rel = ProjectRelativePathBuf::testing_new("host_platform");
        let dest = project_root.path().resolve(&dest_rel);
        fs_util::create_dir_all(dest.clone()).categorize_internal()?;

        write_host_platform_repo(
            &dest,
            &BzlmodHostPlatformSetup {
                cpu_constraint: Some(Arc::from("x86_64")),
                os_constraint: Some(Arc::from("linux")),
            },
        )?;

        let constraints =
            fs_util::read_to_string(dest.join(ForwardRelativePath::new("constraints.bzl")?))
                .categorize_internal()?;
        assert!(constraints.contains("'@platforms//cpu:x86_64'"));
        assert!(constraints.contains("'@platforms//os:linux'"));
        assert!(!constraints.contains("'@platforms//os:osx'"));
        Ok(())
    }

    #[test]
    fn local_config_cc_toolchains_use_actual_host_constraints() {
        let build = local_config_cc_toolchains_build_file(
            r#"load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

toolchain(
    name = "cc-toolchain-%{name}",
    exec_compatible_with = HOST_CONSTRAINTS,
    target_compatible_with = HOST_CONSTRAINTS,
)
"#,
        );

        assert!(!build.contains("load(\"@platforms//host:constraints.bzl\""));
        assert!(build.contains(&format!(
            "\"@platforms//cpu:{}\"",
            host_platform_cpu_constraint().unwrap()
        )));
        assert!(build.contains(&format!(
            "\"@platforms//os:{}\"",
            host_platform_os_constraint().unwrap()
        )));
        assert!(build.contains(&format!("cc-toolchain-{}", host_cc_cpu_value())));
    }

    #[test]
    fn bzlmod_repo_contents_cache_alias_is_published_atomically() -> bz_error::Result<()> {
        let project_root = ProjectRootTemp::new()?;
        let cache_repo_a =
            ProjectRelativePathBuf::testing_new("buck-out/v2/cache/bzlmod_repo_contents/a");
        let cache_repo_b =
            ProjectRelativePathBuf::testing_new("buck-out/v2/cache/bzlmod_repo_contents/b");
        let cache_alias = ProjectRelativePathBuf::testing_new(
            "buck-out/v2/cache/bzlmod_repo_contents/by_canonical/repo",
        );

        fs_util::create_dir_all(project_root.path().resolve(&cache_repo_a))?;
        fs_util::create_dir_all(project_root.path().resolve(&cache_repo_b))?;

        record_bzlmod_repo_contents_cache_alias(project_root.path(), &cache_alias, &cache_repo_a)?;
        record_bzlmod_repo_contents_cache_alias(project_root.path(), &cache_alias, &cache_repo_b)?;

        assert_eq!(
            cache_repo_b.as_str(),
            fs::read_to_string(project_root.path().resolve(&cache_alias)).with_buck_error_context(
                || {
                    format!(
                        "Error reading bzlmod cache alias `{}`",
                        cache_alias.as_str()
                    )
                }
            )?
        );

        Ok(())
    }

    #[test]
    fn bzlmod_repo_contents_cache_alias_rejects_missing_repo() -> bz_error::Result<()> {
        let project_root = ProjectRootTemp::new()?;
        let missing_cache_repo =
            ProjectRelativePathBuf::testing_new("buck-out/v2/cache/bzlmod_repo_contents/missing");
        let cache_alias = ProjectRelativePathBuf::testing_new(
            "buck-out/v2/cache/bzlmod_repo_contents/by_canonical/repo",
        );

        assert!(
            record_bzlmod_repo_contents_cache_alias(
                project_root.path(),
                &cache_alias,
                &missing_cache_repo,
            )
            .is_err()
        );
        assert!(
            fs_util::symlink_metadata_if_exists(project_root.path().resolve(&cache_alias))?
                .is_none()
        );

        Ok(())
    }

    #[test]
    fn repository_rule_build_directory_is_not_mirrored_as_build_bazel() -> bz_error::Result<()> {
        let project_root = ProjectRootTemp::new()?;
        let dest_rel = ProjectRelativePath::new("repo")?;
        let dest = project_root.path().resolve(dest_rel);
        fs_util::create_dir_all(dest.join(ForwardRelativePath::new("BUILD")?))?;
        let setup = BzlmodRepositoryRuleSetup {
            files: Arc::new(Vec::new()),
            source_dir: None,
        };

        write_repository_rule_repo(project_root.path(), &dest, "repo", &setup)?;

        assert!(
            fs_util::symlink_metadata_if_exists(&dest.join(ForwardRelativePath::new("BUILD")?))?
                .is_some_and(|metadata| metadata.is_dir())
        );
        assert!(
            fs_util::symlink_metadata_if_exists(
                &dest.join(ForwardRelativePath::new("BUILD.bazel")?)
            )?
            .is_none()
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn copy_dir_contents_rewrites_in_tree_absolute_symlink() -> bz_error::Result<()> {
        let project_root = ProjectRootTemp::new()?;
        let from = project_root
            .path()
            .resolve(ProjectRelativePath::new("from")?);
        let to = project_root.path().resolve(ProjectRelativePath::new("to")?);
        let target_rel = ForwardRelativePath::new(".tmp_git_root/shed/pkg/Cargo.toml")?;
        let target = from.join(target_rel);
        fs_util::create_dir_all(target.parent().expect("target has parent"))?;
        fs_util::write(&target, "[package]\n").categorize_internal()?;
        let link = from.join(ForwardRelativePath::new("Cargo.toml")?);
        fs_util::symlink(&target, &link).categorize_internal()?;

        copy_dir_contents(&from, &to)?;

        let copied_link = to.join(ForwardRelativePath::new("Cargo.toml")?);
        assert!(
            fs_util::symlink_metadata_if_exists(&copied_link)?
                .is_some_and(|metadata| metadata.file_type().is_symlink())
        );
        assert_eq!(
            PathBuf::from(".tmp_git_root/shed/pkg/Cargo.toml"),
            fs_util::read_link(&copied_link).categorize_internal()?
        );
        assert_eq!(
            "[package]\n",
            fs_util::read_to_string(&copied_link).categorize_internal()?
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn replant_generated_repo_symlinks_rewrites_same_repo_absolute_link() -> bz_error::Result<()> {
        let project_root = ProjectRootTemp::new()?;
        let repo = project_root.path().resolve(ProjectRelativePath::new(
            "buck-out/v2/external_cells/bzlmod_generated/repo",
        )?);
        let external_cell_root = project_root
            .path()
            .resolve(ProjectRelativePath::new("buck-out/v2/external_cells")?);
        let target = repo.join(ForwardRelativePath::new("tools/tool")?);
        let link = repo.join(ForwardRelativePath::new("bin/tool")?);
        fs_util::create_dir_all(target.parent().expect("target has parent"))?;
        fs_util::create_dir_all(link.parent().expect("link has parent"))?;
        fs_util::write(&target, "tool").categorize_internal()?;
        fs_util::symlink(&target, &link).categorize_internal()?;

        assert!(replant_bzlmod_generated_repo_symlinks(
            &repo,
            &external_cell_root
        )?);

        assert_eq!(
            PathBuf::from("../tools/tool"),
            fs_util::read_link(&link).categorize_internal()?
        );
        assert_eq!(
            "tool",
            fs_util::read_to_string(&link).categorize_internal()?
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn replant_generated_repo_symlinks_keeps_cross_repo_absolute_link() -> bz_error::Result<()> {
        let project_root = ProjectRootTemp::new()?;
        let repo = project_root.path().resolve(ProjectRelativePath::new(
            "buck-out/v2/external_cells/bzlmod_generated/repo",
        )?);
        let external_cell_root = project_root
            .path()
            .resolve(ProjectRelativePath::new("buck-out/v2/external_cells")?);
        let target = project_root.path().resolve(ProjectRelativePath::new(
            "buck-out/v2/external_cells/bzlmod/other/file",
        )?);
        let link = repo.join(ForwardRelativePath::new("linked_file")?);
        fs_util::create_dir_all(target.parent().expect("target has parent"))?;
        fs_util::create_dir_all(&repo)?;
        fs_util::write(&target, "other").categorize_internal()?;
        fs_util::symlink(&target, &link).categorize_internal()?;

        assert!(!replant_bzlmod_generated_repo_symlinks(
            &repo,
            &external_cell_root
        )?);

        assert_eq!(target, fs_util::read_link(&link).categorize_internal()?);
        assert_eq!(
            "other",
            fs_util::read_to_string(&link).categorize_internal()?
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn generated_repo_symlink_check_rejects_broken_link() -> bz_error::Result<()> {
        let project_root = ProjectRootTemp::new()?;
        let repo = project_root
            .path()
            .resolve(ProjectRelativePath::new("repo")?);
        fs_util::create_dir_all(&repo)?;
        let link = repo.join(ForwardRelativePath::new("Cargo.toml")?);
        let missing = project_root
            .path()
            .resolve(ProjectRelativePath::new("missing")?);
        fs_util::symlink(&missing, &link).categorize_internal()?;

        assert!(!bzlmod_generated_repo_symlink_targets_exist(&repo)?);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn generated_repo_symlink_check_rejects_link_loop() -> bz_error::Result<()> {
        let project_root = ProjectRootTemp::new()?;
        let repo = project_root
            .path()
            .resolve(ProjectRelativePath::new("repo")?);
        fs_util::create_dir_all(&repo)?;
        let link = repo.join(ForwardRelativePath::new("JavaBuilder_deploy.jar")?);
        fs_util::symlink(&link, &link).categorize_internal()?;

        assert!(!bzlmod_generated_repo_symlink_targets_exist(&repo)?);
        Ok(())
    }

    #[test]
    fn checksum_from_integrity_allows_empty_integrity() {
        assert!(matches!(
            checksum_from_integrity("").unwrap(),
            Checksum::None
        ));
    }

    #[test]
    fn checksum_from_integrity_accepts_sri_algorithms() {
        let cases = [
            (
                "sha1-iEPX+SQWIR3p67lj/0zigSWTKHg=",
                Some("8843d7f92416211de9ebb963ff4ce28125932878"),
                None,
                None,
                None,
            ),
            (
                "sha256-w6uP8Tcg6K2QR905Rms8iXTlksL6OD1KOWBxTK7wxPI=",
                None,
                Some("c3ab8ff13720e8ad9047dd39466b3c8974e592c2fa383d4a3960714caef0c4f2"),
                None,
                None,
            ),
            (
                "sha384-PJww2fZl501RXIQpYNSkUcg6ASX9Pec5LXs3IxrxDHLqWK7fzfiaV2W/kCr5Ps8G",
                None,
                None,
                Some(
                    "3c9c30d9f665e74d515c842960d4a451c83a0125fd3de7392d7b37231af10c72ea58aedfcdf89a5765bf902af93ecf06",
                ),
                None,
            ),
            (
                "sha512-ClAmHr0aOQ/tK/Mm8mc8FFWCpjQtUjIElz0CGTN/gWFqgGmwElh89WNfaSXxtWw2AjDBmyc1AO4BPgMGAb8kJQ==",
                None,
                None,
                None,
                Some(
                    "0a50261ebd1a390fed2bf326f2673c145582a6342d523204973d0219337f81616a8069b012587cf5635f6925f1b56c360230c19b273500ee013e030601bf2425",
                ),
            ),
        ];

        for (integrity, sha1, sha256, sha384, sha512) in cases {
            let checksum = checksum_from_integrity(integrity).unwrap();
            assert_eq!(checksum.sha1(), sha1);
            assert_eq!(checksum.sha256(), sha256);
            assert_eq!(checksum.sha384(), sha384);
            assert_eq!(checksum.sha512(), sha512);
        }
    }

    fn hidden_lockfile_evaluation(
        repo_name: &str,
    ) -> BzlmodHiddenLockfileModuleExtensionEvaluation {
        BzlmodHiddenLockfileModuleExtensionEvaluation {
            bzl_transitive_digest: "bzl-digest".to_owned(),
            usages_digest: "usages-digest".to_owned(),
            recorded_inputs: Vec::new(),
            generated_repo_specs: BTreeMap::from([(
                repo_name.to_owned(),
                BzlmodRepositoryRuleInvocationSetup {
                    repo_name: Arc::from(repo_name),
                    rule_bzl_cell: Arc::from("root"),
                    rule_bzl_path: Arc::from("repo.bzl"),
                    rule_bzl_build_file_cell: Arc::from("root"),
                    rule_bzl_build_file_package: None,
                    rule_name: Arc::from("repo_rule"),
                    attrs: Arc::new(Vec::new()),
                },
            )]),
            module_extension_metadata: Some(BzlmodHiddenLockfileModuleExtensionMetadata {
                reproducible: true,
            }),
        }
    }

    #[test]
    fn hidden_lockfile_update_skips_unchanged_logical_value() -> bz_error::Result<()> {
        let extension_key = "//:extensions.bzl%extension";
        let contents = bzlmod_update_hidden_lockfile_json(
            None,
            extension_key,
            BZLMOD_LOCKFILE_GENERAL_EXTENSION,
            Some(hidden_lockfile_evaluation("repo")),
        )?
        .expect("new reproducible extension should write hidden lockfile");

        assert!(
            bzlmod_update_hidden_lockfile_json(
                Some(contents),
                extension_key,
                BZLMOD_LOCKFILE_GENERAL_EXTENSION,
                Some(hidden_lockfile_evaluation("repo")),
            )?
            .is_none()
        );

        Ok(())
    }

    #[test]
    fn hidden_lockfile_update_skips_empty_non_reproducible_extension() -> bz_error::Result<()> {
        assert!(
            bzlmod_update_hidden_lockfile_json(
                None,
                "//:extensions.bzl%extension",
                BZLMOD_LOCKFILE_GENERAL_EXTENSION,
                None
            )?
            .is_none()
        );

        Ok(())
    }
}

fn bazel_feature_global_is_available(current: &str, min_version: &str, max_version: &str) -> bool {
    (min_version.is_empty() || bazel_version_ge(current, min_version))
        && (max_version.is_empty() || bazel_version_lt(current, max_version))
}

fn bazel_version_ge(current: &str, required: &str) -> bool {
    bazel_version_cmp(current, required) != Ordering::Less
}

fn bazel_version_lt(current: &str, required: &str) -> bool {
    bazel_version_cmp(current, required) == Ordering::Less
}

fn bazel_version_cmp(a: &str, b: &str) -> Ordering {
    let a = bazel_version_numbers(a);
    let b = bazel_version_numbers(b);
    for (a, b) in a.iter().zip(b.iter()) {
        match a.cmp(b) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    a.len().cmp(&b.len())
}

fn bazel_version_numbers(version: &str) -> Vec<u64> {
    version
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

fn write_generated_module_file(dest: &AbsNormPath, name: &str) -> bz_error::Result<()> {
    fs_util::write(
        dest.join(ForwardRelativePath::new("MODULE.bazel")?),
        format!("module(name = {name:?})\n"),
    )
    .categorize_internal()?;
    Ok(())
}

fn extract_archive(
    setup: &BzlmodCellSetup,
    archive: &AbsNormPath,
    temp: &AbsNormPath,
) -> bz_error::Result<()> {
    let primary_url = bzlmod_cell_setup_primary_url(setup);
    let archive_type = setup.archive_type.as_deref();
    let kind = archive_kind_from_type_or_url(archive_type, primary_url)
        .ok_or_else(|| BzlmodError::UnsupportedArchiveType(primary_url.to_owned()))?;
    extract_bazel_archive(
        archive.as_path(),
        temp.as_path(),
        kind,
        setup.strip_prefix.as_deref().unwrap_or(""),
        0,
        &[],
    )
    .buck_error_context("Could not extract archive for bzlmod external cell")
}

fn bzlmod_cell_setup_primary_url(setup: &BzlmodCellSetup) -> &str {
    setup
        .urls
        .first()
        .map(|url| url.as_ref())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| setup.url.as_ref())
}

fn bzlmod_cell_setup_urls(setup: &BzlmodCellSetup) -> Vec<String> {
    let urls = setup
        .urls
        .iter()
        .map(|url| url.to_string())
        .filter(|url| !url.is_empty())
        .collect::<Vec<_>>();
    if urls.is_empty() && !setup.url.is_empty() {
        vec![setup.url.to_string()]
    } else {
        urls
    }
}

fn apply_patch(
    project_fs: &ProjectRoot,
    dest: &AbsNormPath,
    patch: &ProjectRelativePath,
    patch_strip: u32,
) -> bz_error::Result<()> {
    let patch = project_fs.resolve(patch);
    apply_unified_patch_file(dest.as_path(), patch.as_path(), patch_strip)
        .buck_error_context("Could not apply patch for bzlmod external cell")
}

fn copy_dir_contents(from: &AbsNormPath, to: &AbsNormPath) -> bz_error::Result<()> {
    copy_dir_contents_impl(from, to, from, to)
}

fn copy_dir_contents_impl(
    root_from: &AbsNormPath,
    root_to: &AbsNormPath,
    from: &AbsNormPath,
    to: &AbsNormPath,
) -> bz_error::Result<()> {
    for entry in fs_util::read_dir(from).categorize_internal()? {
        let entry = entry?;
        let from_path = entry.path();
        let to_path = to.join(ForwardRelativePath::new(&entry.file_name())?);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs_util::create_dir_all(&to_path)?;
            copy_dir_contents_impl(root_from, root_to, &from_path, &to_path)?;
        } else if file_type.is_file() {
            link_or_copy_file(&from_path, &to_path)?;
        } else if file_type.is_symlink() {
            let target = fs_util::read_link(&from_path).categorize_internal()?;
            let target = copy_dir_symlink_target(root_from, root_to, &to_path, target);
            fs_util::symlink(target, &to_path).categorize_internal()?;
        }
    }
    Ok(())
}

fn copy_dir_symlink_target(
    root_from: &AbsNormPath,
    root_to: &AbsNormPath,
    link: &AbsNormPath,
    target: PathBuf,
) -> PathBuf {
    if !target.is_absolute() {
        return target;
    }
    let Ok(target_relative) = target.strip_prefix(root_from.as_path()) else {
        return target;
    };
    let copied_target = root_to.as_path().join(target_relative);
    path_relative_to_link(&copied_target, link.as_path())
}

fn replant_bzlmod_generated_repo_symlinks(
    repo_dir: &AbsNormPath,
    external_cell_root: &AbsNormPath,
) -> bz_error::Result<bool> {
    let mut safe_for_repo_contents_cache = true;
    replant_bzlmod_generated_repo_symlinks_impl(
        repo_dir,
        repo_dir,
        external_cell_root,
        &mut safe_for_repo_contents_cache,
    )?;
    Ok(safe_for_repo_contents_cache)
}

fn replant_bzlmod_generated_repo_symlinks_impl(
    repo_dir: &AbsNormPath,
    dir: &AbsNormPath,
    external_cell_root: &AbsNormPath,
    safe_for_repo_contents_cache: &mut bool,
) -> bz_error::Result<()> {
    for entry in fs_util::read_dir(dir).categorize_internal()? {
        let entry = entry?;
        let entry_path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            let target = fs_util::read_link(&entry_path).categorize_internal()?;
            if !target.is_absolute() {
                continue;
            }
            let new_target = if target.starts_with(repo_dir.as_path()) {
                Some(path_relative_to_link(&target, entry_path.as_path()))
            } else if target.starts_with(external_cell_root.as_path()) {
                *safe_for_repo_contents_cache = false;
                None
            } else {
                None
            };
            if let Some(new_target) = new_target {
                fs_util::remove_file(&entry_path).categorize_internal()?;
                fs_util::symlink(new_target, &entry_path).categorize_internal()?;
            }
        } else if file_type.is_dir() {
            replant_bzlmod_generated_repo_symlinks_impl(
                repo_dir,
                &entry_path,
                external_cell_root,
                safe_for_repo_contents_cache,
            )?;
        }
    }
    Ok(())
}

fn path_relative_to_link(target: &Path, link: &Path) -> PathBuf {
    let Some(link_parent) = link.parent() else {
        return target.to_path_buf();
    };
    let target_components = target.components().collect::<Vec<_>>();
    let parent_components = link_parent.components().collect::<Vec<_>>();
    let mut shared = 0;
    while target_components.get(shared) == parent_components.get(shared) {
        shared += 1;
    }

    let mut relative = PathBuf::new();
    for _ in shared..parent_components.len() {
        relative.push("..");
    }
    for component in target_components.iter().skip(shared) {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    relative
}

fn link_or_copy_file(from: &AbsNormPath, to: &AbsNormPath) -> bz_error::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs_util::copy(from, to).categorize_internal()?;
            Ok(())
        }
    }
}

fn normalize_generated_repository(repo_name: &str, dest: &AbsNormPath) -> bz_error::Result<()> {
    if repo_name.contains("macos_sdk") {
        normalize_macos_sdk_archive(dest)?;
    }
    Ok(())
}

fn normalize_macos_sdk_archive(dest: &AbsNormPath) -> bz_error::Result<()> {
    let usr_lib = dest.join(ForwardRelativePath::new("sysroot/usr/lib")?);
    let libproc = usr_lib.join(ForwardRelativePath::new("libproc.tbd")?);
    if libproc.exists() {
        return Ok(());
    }

    let libsystem = usr_lib.join(ForwardRelativePath::new("libSystem.B.tbd")?);
    if libsystem.exists() {
        link_or_copy_file(&libsystem, &libproc)?;
    }
    Ok(())
}

fn checksum_from_integrity(integrity: &str) -> bz_error::Result<Checksum> {
    let Some(integrity) = parse_bzlmod_integrity(integrity)? else {
        return Ok(Checksum::none());
    };
    match integrity.kind() {
        BzlmodIntegrityKind::Sha1 => Checksum::new(Some(&hex::encode(integrity.bytes())), None),
        BzlmodIntegrityKind::Sha256 => Checksum::new(None, Some(&hex::encode(integrity.bytes()))),
        BzlmodIntegrityKind::Sha384 => Checksum::new_sha384(&hex::encode(integrity.bytes())),
        BzlmodIntegrityKind::Sha512 => Checksum::new_sha512(&hex::encode(integrity.bytes())),
    }
}

fn bzlmod_path(setup: &BzlmodCellSetup, suffix: &str) -> ProjectRelativePathBuf {
    bzlmod_repo_contents_cache_path(&bzlmod_repo_contents_cache_key(setup), suffix)
}

fn update_bzlmod_repo_contents_cache_key(hasher: &mut blake3::Hasher, field: &str) {
    hasher.update(field.len().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(field.as_bytes());
    hasher.update(b"\0");
}

fn bzlmod_repo_contents_cache_key(setup: &BzlmodCellSetup) -> String {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(&mut hasher, "buck2-bzlmod-repo-contents-v2");
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.module_name);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.version);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.canonical_repo_name);
    update_bzlmod_repo_contents_cache_key_opt(&mut hasher, setup.local_path.as_deref());
    let urls = bzlmod_cell_setup_urls(setup);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &urls.len().to_string());
    for url in urls {
        update_bzlmod_repo_contents_cache_key(&mut hasher, &url);
    }
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.integrity);
    update_bzlmod_repo_contents_cache_key(&mut hasher, setup.strip_prefix.as_deref().unwrap_or(""));
    update_bzlmod_repo_contents_cache_key(&mut hasher, setup.archive_type.as_deref().unwrap_or(""));
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.patch_strip.to_string());
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.patches.len().to_string());
    for patch in setup.patches.iter() {
        update_bzlmod_repo_contents_cache_key(&mut hasher, &patch.url);
        update_bzlmod_repo_contents_cache_key(&mut hasher, &patch.integrity);
        update_bzlmod_repo_contents_cache_key(&mut hasher, patch.path.as_deref().unwrap_or(""));
        if let Some(content_sha256) = patch.content_sha256.as_deref() {
            update_bzlmod_repo_contents_cache_key(&mut hasher, content_sha256);
        }
        update_bzlmod_repo_contents_cache_key(&mut hasher, &patch.patch_strip.to_string());
    }
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.overlays.len().to_string());
    for overlay in setup.overlays.iter() {
        update_bzlmod_repo_contents_cache_key(&mut hasher, &overlay.path);
        update_bzlmod_repo_contents_cache_key(&mut hasher, &overlay.url);
        update_bzlmod_repo_contents_cache_key(&mut hasher, &overlay.integrity);
    }
    hasher.finalize().to_hex().to_string()
}

fn bzlmod_repo_contents_cache_path(cache_key: &str, suffix: &str) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/cache/bzlmod_repo_contents/{cache_key}/{suffix}",
    ))
}

fn bzlmod_generated_repo_contents_cache_entry_dir(
    cache_info: &BazelRepositoryRuleCacheInfo,
) -> ProjectRelativePathBuf {
    bzlmod_repo_contents_cache_path(
        &bzlmod_generated_repo_contents_cache_storage_key(cache_info),
        "generated",
    )
}

fn bzlmod_generated_repo_contents_cache_storage_key(
    cache_info: &BazelRepositoryRuleCacheInfo,
) -> String {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(
        &mut hasher,
        "buck2-bzlmod-generated-repository-rule-contents-cache-v2",
    );
    update_bzlmod_repo_contents_cache_key(&mut hasher, &cache_info.predeclared_input_hash);
    hasher.finalize().to_hex().to_string()
}

fn bzlmod_generated_external_cell_root_path(
    path: &ProjectRelativePath,
) -> Option<ProjectRelativePathBuf> {
    let (buck_out_root, _) = path.as_str().split_once("/external_cells/")?;
    Some(ProjectRelativePathBuf::unchecked_new(format!(
        "{buck_out_root}/external_cells"
    )))
}

fn bzlmod_generated_repo_contents_cache_entry_path(
    cache_info: &BazelRepositoryRuleCacheInfo,
    entry_name: &str,
) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "{}/{}",
        bzlmod_generated_repo_contents_cache_entry_dir(cache_info).as_str(),
        entry_name,
    ))
}

fn bzlmod_generated_repo_contents_cache_recorded_inputs_path(
    cache_info: &BazelRepositoryRuleCacheInfo,
    entry_name: &str,
) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "{}{}",
        bzlmod_generated_repo_contents_cache_entry_path(cache_info, entry_name).as_str(),
        BZLMOD_GENERATED_RECORDED_INPUTS_SUFFIX,
    ))
}

fn bzlmod_generated_repo_contents_cache_latest_path(
    cache_info: &BazelRepositoryRuleCacheInfo,
) -> ProjectRelativePathBuf {
    bzlmod_generated_repo_contents_cache_entry_path(cache_info, BZLMOD_GENERATED_LATEST_ENTRY)
}

fn bzlmod_repo_contents_cache_alias_path(canonical_repo_name: &str) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/cache/bzlmod_repo_contents/by_canonical/{canonical_repo_name}",
    ))
}

fn bzlmod_repo_contents_cache_exists(
    project_fs: &ProjectRoot,
    cache_repo: &ProjectRelativePath,
) -> bz_error::Result<bool> {
    let cache_repo = project_fs.resolve(cache_repo);
    Ok(matches!(
        fs_util::symlink_metadata_if_exists(&cache_repo)?,
        Some(metadata) if metadata.is_dir()
    ))
}

fn bzlmod_generated_repo_contents_cache_new_entry_name() -> String {
    let counter =
        BZLMOD_GENERATED_CACHE_ENTRY_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("repo.{}.{}.{}", std::process::id(), nanos, counter)
}

fn write_bzlmod_generated_repo_contents_cache_latest(
    project_fs: &ProjectRoot,
    cache_info: &BazelRepositoryRuleCacheInfo,
    entry_name: &str,
) -> bz_error::Result<()> {
    let latest_path =
        project_fs.resolve(bzlmod_generated_repo_contents_cache_latest_path(cache_info));
    let latest_parent = latest_path.parent().ok_or_else(|| {
        internal_error!(
            "bzlmod generated cache latest path has no parent: `{}`",
            latest_path.display()
        )
    })?;
    fs_util::create_dir_all(latest_parent)?;
    let tmp_counter = BZLMOD_CACHE_ALIAS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_latest_path = latest_parent.join(ForwardRelativePath::new(&format!(
        ".{}.tmp.{}.{}",
        BZLMOD_GENERATED_LATEST_ENTRY,
        std::process::id(),
        tmp_counter
    ))?);
    fs_util::write(&tmp_latest_path, format!("{entry_name}\n")).categorize_internal()?;
    match fs_util::rename(&tmp_latest_path, &latest_path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ignored = fs_util::remove_file(&tmp_latest_path);
            Err(error.categorize_internal())
        }
    }
}

fn write_bzlmod_generated_recorded_inputs_json(
    project_fs: &ProjectRoot,
    recorded_inputs_path: &ProjectRelativePath,
    recorded_inputs_json: &str,
) -> bz_error::Result<()> {
    let recorded_inputs_path = project_fs.resolve(recorded_inputs_path);
    if let Some(parent) = recorded_inputs_path.parent() {
        fs_util::create_dir_all(parent)?;
    }
    fs_util::write(recorded_inputs_path, recorded_inputs_json).categorize_internal()
}

fn record_bzlmod_repo_contents_cache_alias(
    project_fs: &ProjectRoot,
    cache_alias: &ProjectRelativePath,
    cache_repo: &ProjectRelativePath,
) -> bz_error::Result<()> {
    let cache_repo_abs = project_fs.resolve(cache_repo);
    let cache_repo_metadata = fs_util::metadata(&cache_repo_abs)
        .categorize_internal()
        .with_buck_error_context(|| {
            format!(
                "Error checking bzlmod cache repo `{}` before publishing alias `{}`",
                cache_repo.as_str(),
                cache_alias.as_str()
            )
        })?;
    if !cache_repo_metadata.is_dir() {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Tier0,
            "Cannot publish bzlmod cache alias `{}` to non-directory cache repo `{}`",
            cache_alias.as_str(),
            cache_repo.as_str()
        ));
    }

    let cache_alias = project_fs.resolve(cache_alias);
    let cache_alias_parent = cache_alias.parent().ok_or_else(|| {
        internal_error!(
            "bzlmod cache alias path has no parent: `{}`",
            cache_alias.display()
        )
    })?;
    fs_util::create_dir_all(cache_alias_parent)?;

    let alias_file_name = cache_alias
        .as_path()
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .ok_or_else(|| {
            internal_error!(
                "bzlmod cache alias path has no UTF-8 filename: `{}`",
                cache_alias.display()
            )
        })?;
    let tmp_counter = BZLMOD_CACHE_ALIAS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_file_name = format!(
        ".{}.tmp.{}.{}",
        alias_file_name,
        std::process::id(),
        tmp_counter
    );
    let tmp_cache_alias = cache_alias_parent.join(ForwardRelativePath::new(&tmp_file_name)?);

    fs_util::write(&tmp_cache_alias, cache_repo.as_str()).categorize_internal()?;
    match fs_util::rename(&tmp_cache_alias, &cache_alias) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ignored = fs_util::remove_file(&tmp_cache_alias);
            Err(error.categorize_internal())
        }
    }
}

fn prepare_bzlmod_external_cell_root(
    project_fs: &ProjectRoot,
    cache_repo: &ProjectRelativePath,
    dest: &ProjectRelativePath,
) -> bz_error::Result<()> {
    let stamp_path = bzlmod_external_cell_root_stamp_path(dest);
    let stamp_content = bzlmod_external_cell_root_stamp_content(cache_repo);
    prepare_bzlmod_external_cell_root_with_stamp(
        project_fs,
        cache_repo,
        dest,
        stamp_path.as_ref(),
        &stamp_content,
    )
}

fn prepare_bzlmod_generated_external_cell_root_with_repository_rule_stamp(
    project_fs: &ProjectRoot,
    cache_repo: &ProjectRelativePath,
    dest: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cache_info: &BazelRepositoryRuleCacheInfo,
) -> bz_error::Result<()> {
    let stamp_path = bzlmod_generated_materialization_stamp_path(setup, dest);
    let stamp_content =
        bzlmod_generated_repository_rule_materialization_stamp_content(setup, cache_info);
    prepare_bzlmod_external_cell_root_with_stamp(
        project_fs,
        cache_repo,
        dest,
        stamp_path.as_ref(),
        &stamp_content,
    )
}

async fn bzlmod_generated_repo_contents_cache_candidates(
    ctx: &mut DiceComputations<'_>,
    cache_info: &BazelRepositoryRuleCacheInfo,
) -> bz_error::Result<Vec<BzlmodGeneratedRepoContentsCacheCandidate>> {
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let entry_dir = bzlmod_generated_repo_contents_cache_entry_dir(cache_info);
    let cache_info = cache_info.clone();
    run_bzlmod_cache_io(move || {
        let candidate_for_entry = |entry_name: &str| -> bz_error::Result<
            Option<(u128, BzlmodGeneratedRepoContentsCacheCandidate)>,
        > {
            let recorded_inputs_path =
                bzlmod_generated_repo_contents_cache_recorded_inputs_path(&cache_info, entry_name);
            let recorded_inputs_abs = project_root.resolve(&recorded_inputs_path);
            let recorded_inputs_json = match fs_util::read_to_string(&recorded_inputs_abs) {
                Ok(recorded_inputs_json) => recorded_inputs_json,
                Err(error)
                    if matches!(
                        error.io_error_kind(),
                        Some(ErrorKind::NotFound | ErrorKind::NotADirectory)
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error.categorize_internal()),
            };
            let Ok(recorded_inputs) =
                serde_json::from_str::<Vec<BazelRepositoryRecordedInput>>(&recorded_inputs_json)
            else {
                return Ok(None);
            };
            let repo = ProjectRelativePathBuf::unchecked_new(format!(
                "{}/{}",
                entry_dir.as_str(),
                entry_name
            ));
            if !bzlmod_repo_contents_cache_exists(&project_root, &repo)? {
                return Ok(None);
            }
            let repo_abs = project_root.resolve(&repo);
            if !bzlmod_generated_repo_symlink_targets_exist(&repo_abs)? {
                return Ok(None);
            }
            let modified = fs_util::metadata(recorded_inputs_abs)
                .categorize_internal()?
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            Ok(Some((
                modified,
                BzlmodGeneratedRepoContentsCacheCandidate {
                    entry_name: entry_name.to_owned(),
                    repo,
                    recorded_inputs_path,
                    recorded_inputs,
                },
            )))
        };

        let mut candidates = Vec::new();

        let latest_path = bzlmod_generated_repo_contents_cache_latest_path(&cache_info);
        let latest_entry_name = match fs_util::read_to_string(project_root.resolve(&latest_path)) {
            Ok(latest_entry_name) => {
                let latest_entry_name = latest_entry_name.trim().to_owned();
                if latest_entry_name.is_empty() {
                    None
                } else {
                    if let Some((_, candidate)) = candidate_for_entry(&latest_entry_name)? {
                        candidates.push((u128::MAX, candidate));
                    }
                    Some(latest_entry_name)
                }
            }
            Err(error)
                if matches!(
                    error.io_error_kind(),
                    Some(ErrorKind::NotFound | ErrorKind::NotADirectory)
                ) =>
            {
                None
            }
            Err(error) => return Err(error.categorize_internal()),
        };

        let entry_dir_abs = project_root.resolve(&entry_dir);
        let entries = match fs_util::read_dir(&entry_dir_abs) {
            Ok(entries) => entries,
            Err(error)
                if matches!(
                    error.io_error_kind(),
                    Some(ErrorKind::NotFound | ErrorKind::NotADirectory)
                ) =>
            {
                return Ok(candidates
                    .into_iter()
                    .map(|(_, candidate)| candidate)
                    .collect());
            }
            Err(error) => return Err(error.categorize_internal()),
        };
        for entry in entries {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let Some(entry_name) = file_name.strip_suffix(BZLMOD_GENERATED_RECORDED_INPUTS_SUFFIX)
            else {
                continue;
            };
            if latest_entry_name.as_deref() == Some(entry_name) {
                continue;
            }
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Some(candidate) = candidate_for_entry(entry_name)? {
                candidates.push(candidate);
            }
        }
        candidates.sort_by(|(a, _), (b, _)| b.cmp(a));
        Ok(candidates
            .into_iter()
            .map(|(_, candidate)| candidate)
            .collect())
    })
    .await
}

fn touch_bzlmod_generated_repo_contents_cache_recorded_inputs(
    project_fs: &ProjectRoot,
    recorded_inputs_path: &ProjectRelativePath,
) -> bz_error::Result<()> {
    let recorded_inputs_path = project_fs.resolve(recorded_inputs_path);
    let file = fs::File::options().write(true).open(recorded_inputs_path)?;
    file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
        .map_err(Into::into)
}

async fn prepare_bzlmod_generated_external_cell_root_from_cache_candidate(
    ctx: &mut DiceComputations<'_>,
    candidate: BzlmodGeneratedRepoContentsCacheCandidate,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cache_info: &BazelRepositoryRuleCacheInfo,
) -> bz_error::Result<()> {
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let path = path.to_owned();
    let setup = setup.dupe();
    let cache_info = cache_info.clone();
    let cache_entry_name = candidate.entry_name;
    let cache_recorded_inputs_path = candidate.recorded_inputs_path;
    let recorded_inputs_json = serde_json::to_string(&candidate.recorded_inputs)
        .buck_error_context("Error serializing bzlmod repository recorded inputs")?;
    run_bzlmod_cache_io(move || {
        prepare_bzlmod_generated_external_cell_root_with_repository_rule_stamp(
            &project_root,
            &candidate.repo,
            &path,
            &setup,
            &cache_info,
        )?;
        let recorded_inputs_path = bzlmod_generated_recorded_inputs_path(&setup, &path);
        write_bzlmod_generated_recorded_inputs_json(
            &project_root,
            &recorded_inputs_path,
            &recorded_inputs_json,
        )?;
        let _ = touch_bzlmod_generated_repo_contents_cache_recorded_inputs(
            &project_root,
            &cache_recorded_inputs_path,
        );
        let _ = write_bzlmod_generated_repo_contents_cache_latest(
            &project_root,
            &cache_info,
            &cache_entry_name,
        );
        Ok(())
    })
    .await
}

async fn prepare_bzlmod_generated_external_cell_root_from_repo_contents_cache(
    ctx: &mut DiceComputations<'_>,
    cache_info: &BazelRepositoryRuleCacheInfo,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
) -> bz_error::Result<bool> {
    let candidates = bzlmod_generated_repo_contents_cache_candidates(ctx, cache_info).await?;
    for candidate in candidates {
        if bzlmod_recorded_inputs_are_current(ctx, &candidate.recorded_inputs).await? {
            prepare_bzlmod_generated_external_cell_root_from_cache_candidate(
                ctx, candidate, path, setup, cache_info,
            )
            .await?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn prepare_bzlmod_external_cell_root_with_stamp(
    project_fs: &ProjectRoot,
    cache_repo: &ProjectRelativePath,
    dest: &ProjectRelativePath,
    stamp_path: &ProjectRelativePath,
    stamp_content: &str,
) -> bz_error::Result<()> {
    if bzlmod_external_cell_root_is_current_with_stamp(
        project_fs,
        cache_repo,
        dest,
        stamp_path,
        stamp_content,
    )? {
        return Ok(());
    }

    let stamp_path = project_fs.resolve(stamp_path);
    let cache_repo = project_fs.resolve(cache_repo);
    let dest = project_fs.resolve(dest);
    fs_util::remove_all(&stamp_path).categorize_internal()?;
    fs_util::remove_all(&dest).categorize_internal()?;
    if let Some(parent) = dest.parent() {
        fs_util::create_dir_all(parent)?;
    }
    fs_util::symlink(&cache_repo, &dest).categorize_internal()?;
    fs_util::write(stamp_path, stamp_content).categorize_internal()
}

fn bzlmod_generated_sibling_path(
    setup: &BzlmodGeneratedCellSetup,
    dest: &ProjectRelativePath,
    suffix: &str,
) -> ProjectRelativePathBuf {
    bzlmod_generated_sibling_path_for_canonical(&setup.canonical_repo_name, dest, suffix)
}

fn bzlmod_generated_sibling_path_for_canonical(
    canonical_repo_name: &str,
    dest: &ProjectRelativePath,
    suffix: &str,
) -> ProjectRelativePathBuf {
    let parent = dest
        .as_str()
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    ProjectRelativePathBuf::unchecked_new(format!("{}/{}.{}", parent, canonical_repo_name, suffix))
}

fn bzlmod_generated_scratch_path_for_canonical(
    canonical_repo_name: &str,
    suffix: &str,
) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/cache/bzlmod_generated_scratch/{canonical_repo_name}/{suffix}",
    ))
}

fn bzlmod_generated_scratch_path(
    setup: &BzlmodGeneratedCellSetup,
    suffix: &str,
) -> ProjectRelativePathBuf {
    bzlmod_generated_scratch_path_for_canonical(&setup.canonical_repo_name, suffix)
}

fn bzlmod_external_cell_root_stamp_path(dest: &ProjectRelativePath) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!("{}.materialization_stamp", dest.as_str()))
}

fn bzlmod_external_cell_root_stamp_content(cache_repo: &ProjectRelativePath) -> String {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(&mut hasher, "buck2-bzlmod-external-cell-root-v3");
    update_bzlmod_repo_contents_cache_key(&mut hasher, cache_repo.as_str());
    format!("{}\n", hasher.finalize().to_hex())
}

fn bzlmod_external_cell_root_is_current_with_stamp(
    project_fs: &ProjectRoot,
    cache_repo: &ProjectRelativePath,
    dest: &ProjectRelativePath,
    stamp_path: &ProjectRelativePath,
    stamp_content: &str,
) -> bz_error::Result<bool> {
    let cache_repo_abs = project_fs.resolve(cache_repo);
    if !matches!(
        fs_util::symlink_metadata_if_exists(&cache_repo_abs)?,
        Some(metadata) if metadata.is_dir()
    ) {
        return Ok(false);
    }

    let dest_abs = project_fs.resolve(dest);
    let Some(dest_metadata) = fs_util::symlink_metadata_if_exists(&dest_abs)? else {
        return Ok(false);
    };
    if !dest_metadata.file_type().is_symlink() {
        return Ok(false);
    }
    match fs_util::metadata(&dest_abs) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.io_error_kind() == Some(ErrorKind::NotFound) => return Ok(false),
        Err(error) => return Err(error.categorize_internal()),
    }

    let dest_target = fs_util::read_link(&dest_abs).categorize_internal()?;
    if dest_target != cache_repo_abs.as_path() {
        return Ok(false);
    }

    let stamp_path = project_fs.resolve(stamp_path);
    Ok(matches!(
        fs_util::read_to_string_if_exists(&stamp_path)?,
        Some(content) if content == stamp_content
    ))
}

fn bzlmod_generated_repo_symlink_targets_exist(path: &AbsNormPath) -> bz_error::Result<bool> {
    for entry in fs_util::read_dir(path).categorize_internal()? {
        let entry = entry?;
        let entry_path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if !bzlmod_generated_repo_symlink_targets_exist(&entry_path)? {
                return Ok(false);
            }
        } else if file_type.is_symlink() {
            let target = fs_util::read_link(&entry_path).categorize_internal()?;
            let target_path = if target.is_absolute() {
                target
            } else {
                entry_path
                    .as_path()
                    .parent()
                    .map(|parent| parent.join(&target))
                    .unwrap_or(target)
            };
            match fs::metadata(&target_path) {
                Ok(_) => {}
                Err(_) => return Ok(false),
            }
        }
    }
    Ok(true)
}

fn update_bzlmod_repo_contents_cache_key_opt(hasher: &mut blake3::Hasher, field: Option<&str>) {
    match field {
        Some(field) => {
            update_bzlmod_repo_contents_cache_key(hasher, "some");
            update_bzlmod_repo_contents_cache_key(hasher, field);
        }
        None => update_bzlmod_repo_contents_cache_key(hasher, "none"),
    }
}

fn bzlmod_generated_materialization_stamp_path(
    setup: &BzlmodGeneratedCellSetup,
    dest: &ProjectRelativePath,
) -> ProjectRelativePathBuf {
    bzlmod_generated_sibling_path(setup, dest, "materialization_stamp")
}

fn bzlmod_generated_recorded_inputs_path(
    setup: &BzlmodGeneratedCellSetup,
    dest: &ProjectRelativePath,
) -> ProjectRelativePathBuf {
    bzlmod_generated_sibling_path(setup, dest, "recorded_inputs.json")
}

fn bzlmod_generated_materialization_value_path(
    setup: &BzlmodGeneratedCellSetup,
    dest: &ProjectRelativePath,
) -> ProjectRelativePathBuf {
    bzlmod_generated_sibling_path(setup, dest, "materialization_value")
}

fn bzlmod_generated_repo_requires_recorded_inputs(setup: &BzlmodGeneratedCellSetup) -> bool {
    matches!(
        setup.generator,
        BzlmodGeneratedCellGenerator::ModuleExtensionRepo(_)
            | BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(_)
    )
}

fn bzlmod_generated_materialization_stamp_content(setup: &BzlmodGeneratedCellSetup) -> String {
    format!("{}\n", bzlmod_generated_repo_contents_cache_key(setup))
}

fn bzlmod_generated_repository_rule_materialization_stamp_content(
    setup: &BzlmodGeneratedCellSetup,
    cache_info: &BazelRepositoryRuleCacheInfo,
) -> String {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(
        &mut hasher,
        "buck2-bzlmod-generated-repository-rule-materialization-v4",
    );
    update_bzlmod_repo_contents_cache_key(
        &mut hasher,
        &bzlmod_generated_repo_contents_cache_key(setup),
    );
    update_bzlmod_repo_contents_cache_key(&mut hasher, &cache_info.predeclared_input_hash);
    format!("{}\n", hasher.finalize().to_hex())
}

fn bzlmod_generated_repo_contents_cache_key(setup: &BzlmodGeneratedCellSetup) -> String {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(
        &mut hasher,
        "buck2-bzlmod-generated-materialization-v10",
    );
    update_bzlmod_repo_contents_cache_key(&mut hasher, std::env::consts::OS);
    update_bzlmod_repo_contents_cache_key(&mut hasher, std::env::consts::ARCH);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.canonical_repo_name);
    match &setup.generator {
        BzlmodGeneratedCellGenerator::BazelFeaturesGlobals(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "bazel_features_globals");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_canonical_repo_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.bazel_version);
        }
        BzlmodGeneratedCellGenerator::BazelFeaturesVersion(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "bazel_features_version");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.bazel_version);
        }
        BzlmodGeneratedCellGenerator::HostPlatform(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "host_platform");
            update_bzlmod_repo_contents_cache_key_opt(&mut hasher, setup.cpu_constraint.as_deref());
            update_bzlmod_repo_contents_cache_key_opt(&mut hasher, setup.os_constraint.as_deref());
        }
        BzlmodGeneratedCellGenerator::CcAutoconfToolchains(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "cc_autoconf_toolchains");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_canonical_repo_name);
        }
        BzlmodGeneratedCellGenerator::CcAutoconf(_) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "cc_autoconf");
        }
        BzlmodGeneratedCellGenerator::XcodeConfig(_) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "xcode_config");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &host_macos_sdk_version());
        }
        BzlmodGeneratedCellGenerator::ShellConfig(_) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "shell_config");
        }
        BzlmodGeneratedCellGenerator::HttpArchive(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "http_archive");
            update_bzlmod_repo_contents_cache_key(&mut hasher, "normalization_v2");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.repo_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.url);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.sha256);
            update_bzlmod_repo_contents_cache_key_opt(&mut hasher, setup.strip_prefix.as_deref());
            update_bzlmod_repo_contents_cache_key_opt(&mut hasher, setup.archive_type.as_deref());
        }
        BzlmodGeneratedCellGenerator::PythonHub(_) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "python_hub");
        }
        BzlmodGeneratedCellGenerator::RepositoryRule(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "repository_rule");
            update_bzlmod_repo_contents_cache_key(&mut hasher, "build_bazel_mirror_v2");
            let files_json = serde_json::to_string(&setup.files)
                .expect("serializing repository_rule file manifest cannot fail");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &files_json);
            update_bzlmod_repo_contents_cache_key_opt(&mut hasher, setup.source_dir.as_deref());
        }
        BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "repository_rule_invocation");
            update_bzlmod_repo_contents_cache_key(&mut hasher, "build_bazel_mirror_v2");
            update_bzlmod_repo_contents_cache_key(&mut hasher, "normalization_v2");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.repo_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.rule_bzl_cell);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.rule_bzl_path);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.rule_bzl_build_file_cell);
            update_bzlmod_repo_contents_cache_key_opt(
                &mut hasher,
                setup.rule_bzl_build_file_package.as_deref(),
            );
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.rule_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.attrs.len().to_string());
            for (key, value) in setup.attrs.iter() {
                update_bzlmod_repo_contents_cache_key(&mut hasher, key);
                update_bzlmod_repo_contents_cache_key(&mut hasher, value);
            }
        }
        BzlmodGeneratedCellGenerator::ModuleExtensionRepo(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "module_extension_repo");
            update_bzlmod_repo_contents_cache_key(&mut hasher, "build_bazel_mirror_v2");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_canonical_repo_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_is_root.to_string());
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_file);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_cell);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_path);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_unique_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.repo_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_usages_key);
        }
    }
    hasher.finalize().to_hex().to_string()
}

async fn bzlmod_generated_materialization_is_current(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
) -> bz_error::Result<bool> {
    bzlmod_generated_materialization_is_current_with_stamp_content(
        ctx,
        path,
        setup,
        bzlmod_generated_materialization_stamp_content(setup),
    )
    .await
}

async fn bzlmod_generated_materialization_is_current_with_stamp_content(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    stamp_content: String,
) -> bz_error::Result<bool> {
    let io = ctx.global_data().get_io_provider().dupe();
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let repo_path = project_root.resolve(path);
    let repo_exists = run_bzlmod_cache_io(move || match fs_util::metadata(&repo_path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error)
            if matches!(
                error.io_error_kind(),
                Some(ErrorKind::NotFound | ErrorKind::NotADirectory)
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error.categorize_internal()),
    })
    .await?;
    if !repo_exists {
        return Ok(false);
    }
    let stamp_path = bzlmod_generated_materialization_stamp_path(setup, path);
    let stamp_matches = matches!(
        (&*io).read_file_if_exists(stamp_path).await?,
        Some(content) if content == stamp_content
    );
    if !stamp_matches {
        return Ok(false);
    }
    if bzlmod_generated_repo_requires_recorded_inputs(setup) {
        let recorded_inputs_path = bzlmod_generated_recorded_inputs_path(setup, path);
        let Some(recorded_inputs_json) = (&*io).read_file_if_exists(recorded_inputs_path).await?
        else {
            return Ok(false);
        };
        let recorded_inputs: Vec<BazelRepositoryRecordedInput> =
            match serde_json::from_str(&recorded_inputs_json) {
                Ok(recorded_inputs) => recorded_inputs,
                Err(_) => return Ok(false),
            };
        if !bzlmod_recorded_inputs_are_current(ctx, &recorded_inputs).await? {
            return Ok(false);
        }
    }
    let path = project_root.resolve(path);
    run_bzlmod_cache_io(move || bzlmod_generated_repo_symlink_targets_exist(&path)).await
}

async fn write_bzlmod_generated_recorded_inputs(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    recorded_inputs: &[BazelRepositoryRecordedInput],
) -> bz_error::Result<()> {
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let recorded_inputs_path = bzlmod_generated_recorded_inputs_path(setup, path);
    let recorded_inputs_json = serde_json::to_string(recorded_inputs)
        .buck_error_context("Error serializing bzlmod repository recorded inputs")?;
    ctx.get_blocking_executor()
        .execute_io_inline(move || {
            write_bzlmod_generated_recorded_inputs_json(
                &project_root,
                &recorded_inputs_path,
                &recorded_inputs_json,
            )
        })
        .await
}

async fn write_bzlmod_generated_materialization_stamp(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
) -> bz_error::Result<()> {
    write_bzlmod_generated_materialization_stamp_content(
        ctx,
        path,
        setup,
        bzlmod_generated_materialization_stamp_content(setup),
    )
    .await
}

async fn write_bzlmod_generated_materialization_stamp_content(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    stamp_content: String,
) -> bz_error::Result<()> {
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let stamp_path =
        project_root.resolve(&bzlmod_generated_materialization_stamp_path(setup, path));
    ctx.get_blocking_executor()
        .execute_io_inline(move || {
            if let Some(parent) = stamp_path.parent() {
                fs_util::create_dir_all(parent)?;
            }
            fs_util::write(stamp_path, stamp_content).categorize_internal()
        })
        .await
}

fn write_bzlmod_generated_materialization_stamp_content_sync(
    project_root: &ProjectRoot,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cache_info: &BazelRepositoryRuleCacheInfo,
) -> bz_error::Result<()> {
    let stamp_path =
        project_root.resolve(&bzlmod_generated_materialization_stamp_path(setup, path));
    if let Some(parent) = stamp_path.parent() {
        fs_util::create_dir_all(parent)?;
    }
    fs_util::write(
        stamp_path,
        bzlmod_generated_repository_rule_materialization_stamp_content(setup, cache_info),
    )
    .categorize_internal()
}

fn new_bzlmod_generated_materialization_value_stamp_content() -> String {
    let counter = BZLMOD_GENERATED_MATERIALIZATION_VALUE_COUNTER
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}.{}.{}\n", std::process::id(), nanos, counter)
}

async fn write_new_bzlmod_generated_materialization_value_stamp(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
) -> bz_error::Result<()> {
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let value_path =
        project_root.resolve(&bzlmod_generated_materialization_value_path(setup, path));
    let value_content = new_bzlmod_generated_materialization_value_stamp_content();
    ctx.get_blocking_executor()
        .execute_io_inline(move || {
            if let Some(parent) = value_path.parent() {
                fs_util::create_dir_all(parent)?;
            }
            fs_util::write(value_path, value_content).categorize_internal()
        })
        .await
}

#[derive(Clone, Debug, PartialEq, Eq, allocative::Allocative, Pagable)]
struct BzlmodGeneratedCellMaterializationValue {
    fingerprint: [u8; 32],
}

async fn bzlmod_generated_materialization_value(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    stamp_content: &str,
) -> bz_error::Result<Arc<BzlmodGeneratedCellMaterializationValue>> {
    let io = ctx.global_data().get_io_provider().dupe();
    let value_stamp_path = bzlmod_generated_materialization_value_path(setup, path);
    let value_stamp_content = (&*io)
        .read_file_if_exists(value_stamp_path)
        .await?
        .unwrap_or_default();
    let recorded_inputs_json = if bzlmod_generated_repo_requires_recorded_inputs(setup) {
        let recorded_inputs_path = bzlmod_generated_recorded_inputs_path(setup, path);
        (&*io)
            .read_file_if_exists(recorded_inputs_path)
            .await?
            .unwrap_or_default()
    } else {
        String::new()
    };

    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(
        &mut hasher,
        "buck2-bzlmod-generated-materialization-value-v2",
    );
    update_bzlmod_repo_contents_cache_key(&mut hasher, path.as_str());
    update_bzlmod_repo_contents_cache_key(&mut hasher, stamp_content);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &value_stamp_content);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &recorded_inputs_json);
    Ok(Arc::new(BzlmodGeneratedCellMaterializationValue {
        fingerprint: *hasher.finalize().as_bytes(),
    }))
}

fn bzlmod_module_extension_evaluation_cache_key(setup: &BzlmodModuleExtensionRepoSetup) -> String {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(&mut hasher, "buck2-bzlmod-module-extension-v4");
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_canonical_repo_name);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_is_root.to_string());
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_file);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_cell);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_path);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_unique_name);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_name);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_usages_key);
    hasher.finalize().to_hex().to_string()
}

fn bzlmod_recorded_input_path(project_root: &ProjectRoot, path: &str) -> bz_error::Result<PathBuf> {
    if Path::new(path).is_absolute() {
        return Ok(PathBuf::from(path));
    }
    Ok(project_root
        .resolve(ProjectRelativePath::new(path)?)
        .as_path()
        .to_owned())
}

fn bzlmod_recorded_file_value(path: &Path) -> bz_error::Result<String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok("ENOENT".to_owned()),
        Err(error) => {
            return Err(error).with_buck_error_context(|| {
                format!("Error statting bzlmod recorded input `{}`", path.display())
            });
        }
    };
    if metadata.is_dir() {
        return Ok("DIR".to_owned());
    }
    if metadata.is_file() {
        let mut file = fs::File::open(path).with_buck_error_context(|| {
            format!("Error opening bzlmod recorded input `{}`", path.display())
        })?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; 8192];
        loop {
            let len = file.read(&mut buf).with_buck_error_context(|| {
                format!("Error reading bzlmod recorded input `{}`", path.display())
            })?;
            if len == 0 {
                break;
            }
            hasher.update(&buf[..len]);
        }
        return Ok(format!("FILE:{}", hasher.finalize().to_hex()));
    }
    Ok("OTHER".to_owned())
}

fn bzlmod_recorded_dirents_value(path: &Path) -> bz_error::Result<String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok("ENOENT".to_owned()),
        Err(error) => {
            return Err(error).with_buck_error_context(|| {
                format!("Error statting bzlmod recorded input `{}`", path.display())
            });
        }
    };
    if !metadata.is_dir() {
        return bzlmod_recorded_file_value(path);
    }
    let mut entries = fs::read_dir(path)
        .with_buck_error_context(|| {
            format!(
                "Error reading bzlmod recorded directory `{}`",
                path.display()
            )
        })?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()
        .with_buck_error_context(|| {
            format!(
                "Error reading bzlmod recorded directory `{}`",
                path.display()
            )
        })?;
    entries.sort();
    let mut hasher = blake3::Hasher::new();
    for entry in entries {
        hasher.update(entry.as_bytes());
        hasher.update(&[0]);
    }
    Ok(format!("DIRENTS:{}", hasher.finalize().to_hex()))
}

fn bzlmod_recorded_dir_tree_value(path: &Path) -> bz_error::Result<String> {
    fn visit(base: &Path, path: &Path, hasher: &mut blake3::Hasher) -> bz_error::Result<()> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                hasher.update(b"ENOENT");
                return Ok(());
            }
            Err(error) => {
                return Err(error).with_buck_error_context(|| {
                    format!("Error statting bzlmod recorded input `{}`", path.display())
                });
            }
        };
        let relative = path.strip_prefix(base).unwrap_or(path);
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        if metadata.is_dir() {
            hasher.update(b"DIR");
            let mut entries = fs::read_dir(path)
                .with_buck_error_context(|| {
                    format!(
                        "Error reading bzlmod recorded directory `{}`",
                        path.display()
                    )
                })?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()
                .with_buck_error_context(|| {
                    format!(
                        "Error reading bzlmod recorded directory `{}`",
                        path.display()
                    )
                })?;
            entries.sort();
            for entry in entries {
                visit(base, &entry, hasher)?;
            }
        } else if metadata.is_file() {
            hasher.update(bzlmod_recorded_file_value(path)?.as_bytes());
        } else {
            hasher.update(b"OTHER");
        }
        hasher.update(&[0]);
        Ok(())
    }

    let mut hasher = blake3::Hasher::new();
    visit(path, path, &mut hasher)?;
    Ok(format!("DIRTREE:{}", hasher.finalize().to_hex()))
}

fn bzlmod_repo_name_for_cell(cell_name: &str) -> String {
    if cell_name == "root" {
        return String::new();
    }
    bzlmod_canonical_repo_name_for_cell(cell_name).unwrap_or_else(|| cell_name.to_owned())
}

fn bzlmod_current_repo_mapping(source_cell_name: &str, apparent_name: &str) -> Option<String> {
    if apparent_name.is_empty() {
        return Some(String::new());
    }
    bzlmod_cell_aliases_for_cell(source_cell_name)
        .into_iter()
        .find_map(|(alias, target_cell_name)| {
            (alias == apparent_name).then(|| bzlmod_repo_name_for_cell(&target_cell_name))
        })
}

async fn bzlmod_recorded_inputs_are_current(
    ctx: &mut DiceComputations<'_>,
    recorded_inputs: &[BazelRepositoryRecordedInput],
) -> bz_error::Result<bool> {
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    for input in recorded_inputs {
        match input {
            BazelRepositoryRecordedInput::EnvVar { name, value } => {
                if ctx
                    .get_bzlmod_repository_environment_variable(name)
                    .await?
                    .as_ref()
                    != value.as_ref()
                {
                    return Ok(false);
                }
            }
            BazelRepositoryRecordedInput::File { path, value } => {
                let path = bzlmod_recorded_input_path(&project_root, path)?;
                let current = ctx
                    .get_blocking_executor()
                    .execute_io_inline(move || bzlmod_recorded_file_value(&path))
                    .await?;
                if &current != value {
                    return Ok(false);
                }
            }
            BazelRepositoryRecordedInput::Dirents { path, value } => {
                let path = bzlmod_recorded_input_path(&project_root, path)?;
                let current = ctx
                    .get_blocking_executor()
                    .execute_io_inline(move || bzlmod_recorded_dirents_value(&path))
                    .await?;
                if &current != value {
                    return Ok(false);
                }
            }
            BazelRepositoryRecordedInput::DirTree { path, value } => {
                let path = bzlmod_recorded_input_path(&project_root, path)?;
                let current = ctx
                    .get_blocking_executor()
                    .execute_io_inline(move || bzlmod_recorded_dir_tree_value(&path))
                    .await?;
                if &current != value {
                    return Ok(false);
                }
            }
            BazelRepositoryRecordedInput::RepoMapping {
                source_repo: _,
                source_cell_name,
                apparent_name,
                canonical_name,
            } => {
                if &bzlmod_current_repo_mapping(source_cell_name, apparent_name) != canonical_name {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn bzlmod_hidden_lockfile_path() -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(
        "buck-out/v2/cache/bzlmod_hidden/MODULE.bazel.lock".to_owned(),
    )
}

const BZLMOD_HIDDEN_LOCKFILE_SCHEMA_FIELD: &str = "buck2HiddenLockfileSchemaVersion";
const BZLMOD_HIDDEN_LOCKFILE_SCHEMA_VERSION: u64 = 5;

fn bzlmod_workspace_lockfile_path() -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new("MODULE.bazel.lock".to_owned())
}

#[derive(
    Clone,
    Dupe,
    derive_more::Display,
    Debug,
    Eq,
    Hash,
    PartialEq,
    allocative::Allocative,
    Pagable
)]
#[display("BZLMOD_WORKSPACE_LOCKFILE_JSON")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodWorkspaceLockfileJsonKey;

#[async_trait::async_trait]
impl Key for BzlmodWorkspaceLockfileJsonKey {
    type Value = bz_error::Result<Arc<serde_json::Value>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let project_root = ctx.global_data().get_io_provider().project_root().dupe();
        ctx.get_blocking_executor()
            .execute_io_inline(move || {
                let lockfile_path = project_root.resolve(&bzlmod_workspace_lockfile_path());
                let Some(contents) = fs_util::read_to_string_if_exists(lockfile_path)? else {
                    return Ok(Arc::new(serde_json::json!({})));
                };
                serde_json::from_str(&contents)
                    .map(Arc::new)
                    .buck_error_context("Error parsing bzlmod workspace lockfile")
            })
            .await
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

#[derive(
    Clone,
    Dupe,
    derive_more::Display,
    Debug,
    Eq,
    Hash,
    PartialEq,
    allocative::Allocative,
    Pagable
)]
#[display("BZLMOD_HIDDEN_LOCKFILE_JSON")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodHiddenLockfileJsonKey;

#[async_trait::async_trait]
impl Key for BzlmodHiddenLockfileJsonKey {
    type Value = bz_error::Result<Arc<serde_json::Value>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let project_root = ctx.global_data().get_io_provider().project_root().dupe();
        ctx.get_blocking_executor()
            .execute_io_inline(move || {
                let lockfile_path = project_root.resolve(&bzlmod_hidden_lockfile_path());
                let Some(contents) = fs_util::read_to_string_if_exists(&lockfile_path)? else {
                    return Ok(Arc::new(serde_json::json!({})));
                };
                match serde_json::from_str(&contents) {
                    Ok(lockfile) if bzlmod_hidden_lockfile_schema_matches(&lockfile) => {
                        Ok(Arc::new(lockfile))
                    }
                    Err(_) => Ok(Arc::new(serde_json::json!({}))),
                    _ => Ok(Arc::new(serde_json::json!({}))),
                }
            })
            .await
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

fn bzlmod_hidden_lockfile_schema_matches(lockfile: &serde_json::Value) -> bool {
    lockfile
        .get(BZLMOD_HIDDEN_LOCKFILE_SCHEMA_FIELD)
        .and_then(|value| value.as_u64())
        == Some(BZLMOD_HIDDEN_LOCKFILE_SCHEMA_VERSION)
}

const BZLMOD_LOCKFILE_GENERAL_EXTENSION: &str = "general";

#[derive(Clone, Debug, Eq, PartialEq)]
struct BzlmodModuleExtensionEvalFactors {
    os: String,
    arch: String,
}

impl BzlmodModuleExtensionEvalFactors {
    async fn from_setup(
        ctx: &mut DiceComputations<'_>,
        setup: &BzlmodModuleExtensionRepoSetup,
    ) -> bz_error::Result<Self> {
        let deps = bzlmod_module_extension_eval_factor_deps(ctx, setup).await?;
        Ok(Self {
            os: if deps.os_dependent {
                bzlmod_bazel_current_os_name(ctx).await?
            } else {
                String::new()
            },
            arch: if deps.arch_dependent {
                bzlmod_bazel_current_arch_name(ctx).await?
            } else {
                String::new()
            },
        })
    }

    fn lockfile_key(&self) -> String {
        if self.os.is_empty() && self.arch.is_empty() {
            return BZLMOD_LOCKFILE_GENERAL_EXTENSION.to_owned();
        }
        let mut parts = Vec::new();
        if !self.os.is_empty() {
            parts.push(format!("os:{}", self.os));
        }
        if !self.arch.is_empty() {
            parts.push(format!("arch:{}", self.arch));
        }
        parts.join(",")
    }
}

async fn bzlmod_bazel_current_os_name(ctx: &mut DiceComputations<'_>) -> bz_error::Result<String> {
    Ok(
        match ctx
            .get_bzlmod_repository_environment_variable(BZLMOD_REPOSITORY_OS_NAME_ENV)
            .await?
            .as_deref()
        {
            Some("mac os x" | "macos" | "osx" | "darwin") => "osx".to_owned(),
            Some("linux") => "linux".to_owned(),
            Some("windows") => "windows".to_owned(),
            Some("freebsd") => "freebsd".to_owned(),
            Some("openbsd") => "openbsd".to_owned(),
            Some(other) => other.to_owned(),
            None if cfg!(target_os = "macos") => "osx".to_owned(),
            None if cfg!(target_os = "linux") => "linux".to_owned(),
            None if cfg!(target_os = "windows") => "windows".to_owned(),
            None if cfg!(target_os = "freebsd") => "freebsd".to_owned(),
            None if cfg!(target_os = "openbsd") => "openbsd".to_owned(),
            None => "unknown".to_owned(),
        },
    )
}

async fn bzlmod_bazel_current_arch_name(
    ctx: &mut DiceComputations<'_>,
) -> bz_error::Result<String> {
    let arch = ctx
        .get_bzlmod_repository_environment_variable(BZLMOD_REPOSITORY_OS_ARCH_ENV)
        .await?
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned());
    Ok(match arch.as_str() {
        "x86_64" => "x86_64".to_owned(),
        "aarch64" => "aarch64".to_owned(),
        "arm" => "arm".to_owned(),
        arch => arch.to_owned(),
    })
}

fn bzlmod_bzl_path_to_label_path(path: &str) -> String {
    if let Some((package, target)) = path.rsplit_once('/') {
        format!("{package}:{target}")
    } else {
        format!(":{path}")
    }
}

fn bzlmod_lockfile_extension_key_from_setup(
    setup: &BzlmodModuleExtensionRepoSetup,
) -> bz_error::Result<String> {
    let canonical_repo_name = if setup.extension_bzl_cell.as_ref() == "root" {
        String::new()
    } else {
        bzlmod_canonical_repo_name_for_cell(&setup.extension_bzl_cell).ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod module extension `{}//{}%{}` resolves to unknown cell `{}`",
                setup.extension_bzl_cell,
                setup.extension_bzl_path,
                setup.extension_name,
                setup.extension_bzl_cell
            )
        })?
    };
    if canonical_repo_name.is_empty() {
        return Ok(format!(
            "//{}%{}",
            bzlmod_bzl_path_to_label_path(&setup.extension_bzl_path),
            setup.extension_name
        ));
    }
    Ok(format!(
        "@@{}//{}%{}",
        canonical_repo_name,
        bzlmod_bzl_path_to_label_path(&setup.extension_bzl_path),
        setup.extension_name
    ))
}

#[derive(Debug, Serialize, Deserialize)]
struct BzlmodHiddenLockfileModuleExtensionMetadata {
    #[serde(default)]
    reproducible: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct BzlmodHiddenLockfileModuleExtensionEvaluation {
    #[serde(default, rename = "bzlTransitiveDigest")]
    bzl_transitive_digest: String,
    #[serde(default, rename = "usagesDigest")]
    usages_digest: String,
    #[serde(default, rename = "recordedInputs")]
    recorded_inputs: Vec<BazelRepositoryRecordedInput>,
    #[serde(default, rename = "generatedRepoSpecs")]
    generated_repo_specs: BTreeMap<String, BzlmodRepositoryRuleInvocationSetup>,
    #[serde(
        default,
        rename = "moduleExtensionMetadata",
        skip_serializing_if = "Option::is_none"
    )]
    module_extension_metadata: Option<BzlmodHiddenLockfileModuleExtensionMetadata>,
}

#[derive(Debug, Deserialize)]
struct BzlmodWorkspaceLockfileModuleExtensionEvaluation {
    #[serde(default, rename = "bzlTransitiveDigest")]
    bzl_transitive_digest: String,
    #[serde(default, rename = "usagesDigest")]
    usages_digest: String,
    #[serde(default, rename = "recordedInputs")]
    recorded_inputs: Vec<String>,
    #[serde(default, rename = "generatedRepoSpecs")]
    generated_repo_specs: BTreeMap<String, BzlmodWorkspaceLockfileGeneratedRepoSpec>,
    #[serde(default, rename = "moduleExtensionMetadata")]
    module_extension_metadata: Option<BzlmodHiddenLockfileModuleExtensionMetadata>,
}

#[derive(Debug, Deserialize)]
struct BzlmodWorkspaceLockfileGeneratedRepoSpec {
    #[serde(rename = "repoRuleId")]
    repo_rule_id: String,
    #[serde(default)]
    attributes: BTreeMap<String, serde_json::Value>,
}

impl BzlmodHiddenLockfileModuleExtensionEvaluation {
    fn reproducible(&self) -> bool {
        self.module_extension_metadata
            .as_ref()
            .map(|metadata| metadata.reproducible)
            .unwrap_or(false)
    }
}

impl BzlmodWorkspaceLockfileModuleExtensionEvaluation {
    fn reproducible(&self) -> bool {
        self.module_extension_metadata
            .as_ref()
            .map(|metadata| metadata.reproducible)
            .unwrap_or(false)
    }
}

fn bzlmod_hidden_lockfile_extension_evaluation_from_result(
    evaluation: &BazelModuleExtensionEvaluationResult,
    bzl_transitive_digest: String,
    usages_digest: String,
) -> bz_error::Result<BzlmodHiddenLockfileModuleExtensionEvaluation> {
    let mut generated_repo_specs = BTreeMap::new();
    for invocation in &evaluation.repository_rule_invocations {
        generated_repo_specs.insert(
            invocation.name.clone(),
            bzlmod_repository_rule_invocation_to_setup(invocation)?,
        );
    }
    Ok(BzlmodHiddenLockfileModuleExtensionEvaluation {
        bzl_transitive_digest,
        usages_digest,
        recorded_inputs: evaluation.recorded_inputs.clone(),
        generated_repo_specs,
        module_extension_metadata: Some(BzlmodHiddenLockfileModuleExtensionMetadata {
            reproducible: evaluation.reproducible,
        }),
    })
}

fn bzlmod_hidden_lockfile_extension_evaluation_to_result(
    evaluation: BzlmodHiddenLockfileModuleExtensionEvaluation,
) -> bz_error::Result<Arc<BazelModuleExtensionEvaluationResult>> {
    let reproducible = evaluation.reproducible();
    let mut repository_rule_invocations = Vec::new();
    for (repo_name, setup) in evaluation.generated_repo_specs {
        repository_rule_invocations.push(bzlmod_repository_rule_invocation_from_setup(
            &setup, &repo_name,
        )?);
    }
    Ok(Arc::new(BazelModuleExtensionEvaluationResult {
        repository_rule_invocations,
        recorded_inputs: evaluation.recorded_inputs,
        path_label_deps: Vec::new(),
        reproducible,
    }))
}

fn bzlmod_workspace_lockfile_extension_evaluation_to_result(
    evaluation: BzlmodWorkspaceLockfileModuleExtensionEvaluation,
) -> bz_error::Result<Arc<BazelModuleExtensionEvaluationResult>> {
    let reproducible = evaluation.reproducible();
    let mut repository_rule_invocations = Vec::new();
    for (repo_name, spec) in evaluation.generated_repo_specs {
        let setup = bzlmod_workspace_lockfile_generated_repo_spec_to_setup(&repo_name, spec)?;
        repository_rule_invocations.push(bzlmod_repository_rule_invocation_from_setup(
            &setup, &repo_name,
        )?);
    }
    Ok(Arc::new(BazelModuleExtensionEvaluationResult {
        repository_rule_invocations,
        recorded_inputs: Vec::new(),
        path_label_deps: Vec::new(),
        reproducible,
    }))
}

fn bzlmod_hidden_lockfile_extension_evaluation_from_lockfile(
    lockfile: &serde_json::Value,
    extension_key: &str,
    factor_key: &str,
) -> bz_error::Result<Option<BzlmodHiddenLockfileModuleExtensionEvaluation>> {
    let Some(evaluation) = lockfile
        .get("moduleExtensions")
        .and_then(|module_extensions| module_extensions.get(extension_key))
        .and_then(|extension| extension.get(factor_key))
    else {
        return Ok(None);
    };
    match serde_json::from_value(evaluation.clone()) {
        Ok(evaluation) => Ok(Some(evaluation)),
        Err(_) => Ok(None),
    }
}

fn bzlmod_workspace_lockfile_extension_evaluation_from_lockfile(
    lockfile: &serde_json::Value,
    extension_key: &str,
    factor_key: &str,
) -> bz_error::Result<Option<BzlmodWorkspaceLockfileModuleExtensionEvaluation>> {
    let Some(evaluation) = lockfile
        .get("moduleExtensions")
        .and_then(|module_extensions| module_extensions.get(extension_key))
        .and_then(|extension| extension.get(factor_key))
    else {
        return Ok(None);
    };
    serde_json::from_value(evaluation.clone())
        .map(Some)
        .buck_error_context("Error parsing bzlmod workspace lockfile extension value")
}

fn bzlmod_cell_name_for_lockfile_repo_name(canonical_repo_name: &str) -> String {
    if canonical_repo_name.is_empty() {
        "root".to_owned()
    } else {
        bzlmod_cell_name(canonical_repo_name)
    }
}

fn bzlmod_label_package_target_to_path(package_target: &str) -> bz_error::Result<String> {
    if let Some((package, target)) = package_target.split_once(':') {
        if package.is_empty() {
            Ok(target.to_owned())
        } else {
            Ok(format!("{package}/{target}"))
        }
    } else {
        Ok(package_target.to_owned())
    }
}

fn bzlmod_workspace_lockfile_repo_rule_id_parts(
    repo_rule_id: &str,
) -> bz_error::Result<(String, String, String)> {
    let (label, rule_name) = repo_rule_id.rsplit_once('%').ok_or_else(|| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "bzlmod lockfile repoRuleId `{}` is missing `%<rule>`",
            repo_rule_id
        )
    })?;
    let (canonical_repo_name, package_target) = if let Some(label) = label.strip_prefix("@@") {
        label.split_once("//").ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod lockfile repoRuleId `{}` is missing `//`",
                repo_rule_id
            )
        })?
    } else if let Some(label) = label.strip_prefix('@') {
        label.split_once("//").ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod lockfile repoRuleId `{}` is missing `//`",
                repo_rule_id
            )
        })?
    } else if let Some(package_target) = label.strip_prefix("//") {
        ("", package_target)
    } else {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "bzlmod lockfile repoRuleId `{}` is not an absolute label",
            repo_rule_id
        ));
    };
    Ok((
        bzlmod_cell_name_for_lockfile_repo_name(canonical_repo_name),
        bzlmod_label_package_target_to_path(package_target)?,
        rule_name.to_owned(),
    ))
}

fn bzlmod_workspace_lockfile_generated_repo_spec_to_setup(
    repo_name: &str,
    spec: BzlmodWorkspaceLockfileGeneratedRepoSpec,
) -> bz_error::Result<BzlmodRepositoryRuleInvocationSetup> {
    let (rule_bzl_cell, rule_bzl_path, rule_name) =
        bzlmod_workspace_lockfile_repo_rule_id_parts(&spec.repo_rule_id)?;
    let mut attrs = spec
        .attributes
        .iter()
        .map(|(key, value)| {
            bz_error::Ok((
                Arc::from(key.as_str()),
                Arc::from(bzlmod_workspace_lockfile_attr_expression(value)?.as_str()),
            ))
        })
        .collect::<bz_error::Result<Vec<(Arc<str>, Arc<str>)>>>()?;
    attrs.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(BzlmodRepositoryRuleInvocationSetup {
        repo_name: Arc::from(repo_name),
        rule_bzl_build_file_cell: Arc::from(rule_bzl_cell.as_str()),
        rule_bzl_build_file_package: None,
        rule_bzl_cell: Arc::from(rule_bzl_cell.as_str()),
        rule_bzl_path: Arc::from(rule_bzl_path.as_str()),
        rule_name: Arc::from(rule_name.as_str()),
        attrs: Arc::new(attrs),
    })
}

fn bzlmod_workspace_lockfile_attr_expression(
    value: &serde_json::Value,
) -> bz_error::Result<String> {
    match value {
        serde_json::Value::Null => Ok("None".to_owned()),
        serde_json::Value::Bool(value) => Ok(if *value { "True" } else { "False" }.to_owned()),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        serde_json::Value::String(value) => bzlmod_workspace_lockfile_string_attr_expression(value),
        serde_json::Value::Array(values) => {
            let values = values
                .iter()
                .map(bzlmod_workspace_lockfile_attr_expression)
                .collect::<bz_error::Result<Vec<_>>>()?;
            Ok(format!("[{}]", values.join(", ")))
        }
        serde_json::Value::Object(values) => {
            let mut entries = values
                .iter()
                .map(|(key, value)| {
                    let key = serde_json::to_string(key)
                        .buck_error_context("Error serializing bzlmod lockfile attr key")?;
                    let value = bzlmod_workspace_lockfile_attr_expression(value)?;
                    bz_error::Ok(format!("{key}: {value}"))
                })
                .collect::<bz_error::Result<Vec<_>>>()?;
            entries.sort();
            Ok(format!("{{{}}}", entries.join(", ")))
        }
    }
}

fn bzlmod_workspace_lockfile_string_attr_expression(value: &str) -> bz_error::Result<String> {
    serde_json::to_string(value)
        .buck_error_context("Error serializing bzlmod lockfile string repository-rule attribute")
}

#[derive(Debug)]
enum BzlmodWorkspaceLockfileRecordedInput {
    EnvVar {
        name: String,
        value: Option<String>,
    },
    File {
        path: String,
        value: String,
    },
    RepoMapping {
        source_repo: String,
        apparent_name: String,
        canonical_name: Option<String>,
    },
    Unsupported,
}

fn bzlmod_bazel_lockfile_unescape(value: &str) -> Option<String> {
    if value == "\\0" {
        return None;
    }
    let mut result = String::new();
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            if c == 'n' {
                result.push('\n');
            } else if c == 's' {
                result.push(' ');
            } else {
                result.push(c);
            }
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            result.push(c);
        }
    }
    Some(result)
}

fn bzlmod_workspace_lockfile_recorded_input_from_str(
    value: &str,
) -> Option<BzlmodWorkspaceLockfileRecordedInput> {
    let separator = value.find(' ')?;
    if separator == 0 {
        return None;
    }
    let input = bzlmod_bazel_lockfile_unescape(&value[..separator])?;
    let value = bzlmod_bazel_lockfile_unescape(&value[separator + 1..]);
    let (kind, id) = input.split_once(':')?;
    match kind {
        "ENV" => Some(BzlmodWorkspaceLockfileRecordedInput::EnvVar {
            name: id.to_owned(),
            value,
        }),
        "FILE" => Some(BzlmodWorkspaceLockfileRecordedInput::File {
            path: id.to_owned(),
            value: value?,
        }),
        "REPO_MAPPING" => {
            let (source_repo, apparent_name) = id.split_once(',')?;
            Some(BzlmodWorkspaceLockfileRecordedInput::RepoMapping {
                source_repo: source_repo.to_owned(),
                apparent_name: apparent_name.to_owned(),
                canonical_name: value,
            })
        }
        "DIRENTS" | "DIRTREE" => Some(BzlmodWorkspaceLockfileRecordedInput::Unsupported),
        _ => None,
    }
}

fn bzlmod_workspace_recorded_input_path(
    project_root: &ProjectRoot,
    path: &str,
) -> bz_error::Result<PathBuf> {
    if Path::new(path).is_absolute() {
        return Ok(PathBuf::from(path));
    }
    let label = path.strip_prefix("@@").or_else(|| path.strip_prefix('@'));
    if let Some(label) = label {
        let (canonical_repo_name, repo_path) = label.split_once("//").ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod lockfile recorded input path `{}` is not a repository path",
                path
            )
        })?;
        let relative = if canonical_repo_name.is_empty() {
            repo_path.to_owned()
        } else if repo_path.is_empty() {
            external_cell_source_path(BZLMOD_EXTERNAL_CELL_KIND, canonical_repo_name)
        } else {
            format!(
                "{}/{}",
                external_cell_source_path(BZLMOD_EXTERNAL_CELL_KIND, canonical_repo_name),
                repo_path
            )
        };
        return Ok(project_root
            .resolve(ProjectRelativePath::new(&relative)?)
            .as_path()
            .to_owned());
    }
    Ok(project_root
        .resolve(ProjectRelativePath::new(path)?)
        .as_path()
        .to_owned())
}

fn bzlmod_workspace_recorded_file_value(path: &Path) -> bz_error::Result<String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok("ENOENT".to_owned()),
        Err(error) => {
            return Err(error).with_buck_error_context(|| {
                format!(
                    "Error statting bzlmod workspace lockfile input `{}`",
                    path.display()
                )
            });
        }
    };
    if metadata.is_dir() {
        return Ok("DIR".to_owned());
    }
    if metadata.is_file() {
        let file = fs::File::open(path).with_buck_error_context(|| {
            format!(
                "Error opening bzlmod workspace lockfile input `{}`",
                path.display()
            )
        })?;
        let digest = FileDigest::from_reader_for_algorithm(file, DigestAlgorithm::Sha256)
            .with_buck_error_context(|| {
                format!(
                    "Error digesting bzlmod workspace lockfile input `{}`",
                    path.display()
                )
            })?;
        return Ok(hex::encode(digest.raw_digest().as_bytes()));
    }
    Ok("OTHER".to_owned())
}

async fn bzlmod_workspace_lockfile_recorded_inputs_are_current(
    ctx: &mut DiceComputations<'_>,
    recorded_inputs: &[String],
) -> bz_error::Result<bool> {
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    for input in recorded_inputs {
        let Some(input) = bzlmod_workspace_lockfile_recorded_input_from_str(input) else {
            return Ok(false);
        };
        match input {
            BzlmodWorkspaceLockfileRecordedInput::EnvVar { name, value } => {
                if ctx
                    .get_bzlmod_repository_environment_variable(&name)
                    .await?
                    .as_ref()
                    != value.as_ref()
                {
                    return Ok(false);
                }
            }
            BzlmodWorkspaceLockfileRecordedInput::File { path, value } => {
                let path = bzlmod_workspace_recorded_input_path(&project_root, &path)?;
                let current = ctx
                    .get_blocking_executor()
                    .execute_io_inline(move || bzlmod_workspace_recorded_file_value(&path))
                    .await?;
                if current != value {
                    return Ok(false);
                }
            }
            BzlmodWorkspaceLockfileRecordedInput::RepoMapping {
                source_repo,
                apparent_name,
                canonical_name,
            } => {
                let source_cell_name = bzlmod_cell_name_for_lockfile_repo_name(&source_repo);
                let current = bzlmod_current_repo_mapping(&source_cell_name, &apparent_name);
                // Buck registers some generated-repo alias maps demand-driven. A present mapping
                // must match the lockfile, but an absent mapping here only means it has not been
                // registered yet.
                if current.is_some() && current != canonical_name {
                    return Ok(false);
                }
            }
            BzlmodWorkspaceLockfileRecordedInput::Unsupported => return Ok(false),
        }
    }
    Ok(true)
}

async fn read_bzlmod_workspace_lockfile_extension_candidate(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
    eval_factors: &BzlmodModuleExtensionEvalFactors,
) -> bz_error::Result<Option<BzlmodWorkspaceLockfileModuleExtensionEvaluation>> {
    let extension_key = bzlmod_lockfile_extension_key_from_setup(setup)?;
    let factor_key = eval_factors.lockfile_key();
    let lockfile = ctx.compute(&BzlmodWorkspaceLockfileJsonKey).await??;
    bzlmod_workspace_lockfile_extension_evaluation_from_lockfile(
        lockfile.as_ref(),
        &extension_key,
        &factor_key,
    )
}

async fn validate_bzlmod_workspace_lockfile_extension(
    ctx: &mut DiceComputations<'_>,
    evaluation: BzlmodWorkspaceLockfileModuleExtensionEvaluation,
    bzl_transitive_digest: &str,
    usages_digest: &str,
) -> bz_error::Result<Option<Arc<BazelModuleExtensionEvaluationResult>>> {
    if evaluation.bzl_transitive_digest != bzl_transitive_digest {
        return Ok(None);
    }
    if evaluation.usages_digest != usages_digest {
        return Ok(None);
    }
    if !bzlmod_workspace_lockfile_recorded_inputs_are_current(ctx, &evaluation.recorded_inputs)
        .await?
    {
        return Ok(None);
    }
    bzlmod_workspace_lockfile_extension_evaluation_to_result(evaluation).map(Some)
}

async fn read_bzlmod_hidden_lockfile_extension(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
    eval_factors: &BzlmodModuleExtensionEvalFactors,
    bzl_transitive_digest: &str,
    usages_digest: &str,
) -> bz_error::Result<Option<Arc<BazelModuleExtensionEvaluationResult>>> {
    let extension_key = bzlmod_lockfile_extension_key_from_setup(setup)?;
    let factor_key = eval_factors.lockfile_key();
    let lockfile = ctx.compute(&BzlmodHiddenLockfileJsonKey).await??;
    let evaluation = bzlmod_hidden_lockfile_extension_evaluation_from_lockfile(
        lockfile.as_ref(),
        &extension_key,
        &factor_key,
    )?;
    let Some(evaluation) = evaluation else {
        return Ok(None);
    };
    if evaluation.bzl_transitive_digest != bzl_transitive_digest {
        return Ok(None);
    }
    if evaluation.usages_digest != usages_digest {
        return Ok(None);
    }
    if !bzlmod_recorded_inputs_are_current(ctx, &evaluation.recorded_inputs).await? {
        return Ok(None);
    }
    bzlmod_hidden_lockfile_extension_evaluation_to_result(evaluation).map(Some)
}

fn bzlmod_update_hidden_lockfile_json(
    contents: Option<String>,
    extension_key: &str,
    factor_key: &str,
    evaluation: Option<BzlmodHiddenLockfileModuleExtensionEvaluation>,
) -> bz_error::Result<Option<String>> {
    let mut lockfile: serde_json::Value = match contents {
        Some(contents) => serde_json::from_str(&contents)
            .buck_error_context("Error parsing hidden bzlmod lockfile")?,
        None => serde_json::json!({}),
    };
    if !lockfile.is_object() {
        lockfile = serde_json::json!({});
    }
    if !bzlmod_hidden_lockfile_schema_matches(&lockfile) {
        lockfile = serde_json::json!({});
    }
    let old_lockfile = lockfile.clone();
    let lockfile_object = lockfile.as_object_mut().expect("checked object");
    lockfile_object.insert(
        BZLMOD_HIDDEN_LOCKFILE_SCHEMA_FIELD.to_owned(),
        serde_json::Value::from(BZLMOD_HIDDEN_LOCKFILE_SCHEMA_VERSION),
    );
    if let Some(evaluation) = evaluation {
        let module_extensions = lockfile_object
            .entry("moduleExtensions".to_owned())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if !module_extensions.is_object() {
            *module_extensions = serde_json::Value::Object(serde_json::Map::new());
        }
        let module_extensions = module_extensions.as_object_mut().expect("checked object");
        let extension = module_extensions
            .entry(extension_key.to_owned())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if !extension.is_object() {
            *extension = serde_json::Value::Object(serde_json::Map::new());
        }
        let extension = extension.as_object_mut().expect("checked object");
        extension.insert(
            factor_key.to_owned(),
            serde_json::to_value(evaluation)
                .buck_error_context("Error serializing hidden bzlmod extension value")?,
        );
    } else if let Some(module_extensions) = lockfile_object.get_mut("moduleExtensions") {
        if !module_extensions.is_object() {
            *module_extensions = serde_json::Value::Object(serde_json::Map::new());
        }
        let module_extensions = module_extensions.as_object_mut().expect("checked object");
        let remove_extension = if let Some(extension) = module_extensions.get_mut(extension_key) {
            if let Some(extension) = extension.as_object_mut() {
                extension.remove(factor_key);
                extension.is_empty()
            } else {
                true
            }
        } else {
            false
        };
        if remove_extension {
            module_extensions.remove(extension_key);
        }
    }
    if lockfile == old_lockfile {
        return Ok(None);
    }
    serde_json::to_string_pretty(&lockfile)
        .map(Some)
        .buck_error_context("Error serializing hidden bzlmod lockfile")
}

async fn write_bzlmod_hidden_lockfile_extension(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
    eval_factors: &BzlmodModuleExtensionEvalFactors,
    evaluation: &BazelModuleExtensionEvaluationResult,
    bzl_transitive_digest: String,
    usages_digest: String,
) -> bz_error::Result<()> {
    let extension_key = bzlmod_lockfile_extension_key_from_setup(setup)?;
    let factor_key = eval_factors.lockfile_key();
    let lockfile_evaluation = bzlmod_hidden_lockfile_extension_evaluation_from_result(
        evaluation,
        bzl_transitive_digest,
        usages_digest,
    )?;
    let lockfile_evaluation = lockfile_evaluation
        .reproducible()
        .then_some(lockfile_evaluation);
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let _guard = BZLMOD_HIDDEN_LOCKFILE_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    ctx.get_blocking_executor()
        .execute_io_inline(move || {
            let lockfile_path = project_root.resolve(&bzlmod_hidden_lockfile_path());
            let contents = fs_util::read_to_string_if_exists(&lockfile_path)?;
            let Some(lockfile_json) = bzlmod_update_hidden_lockfile_json(
                contents,
                &extension_key,
                &factor_key,
                lockfile_evaluation,
            )?
            else {
                return Ok(());
            };
            if let Some(parent) = lockfile_path.parent() {
                fs::create_dir_all(parent).with_buck_error_context(|| {
                    format!(
                        "Error creating parent directory for hidden bzlmod lockfile `{}`",
                        lockfile_path.display()
                    )
                })?;
            }
            let temp_path = lockfile_path
                .as_path()
                .with_extension(format!("tmp.{}", std::process::id()));
            fs::write(&temp_path, lockfile_json).with_buck_error_context(|| {
                format!(
                    "Error writing temporary hidden bzlmod lockfile `{}`",
                    temp_path.display()
                )
            })?;
            fs::rename(&temp_path, lockfile_path.as_path()).with_buck_error_context(|| {
                format!(
                    "Error committing hidden bzlmod lockfile `{}`",
                    lockfile_path.display()
                )
            })?;
            Ok(())
        })
        .await
}

fn bzlmod_module_extension_evaluation_usages_key(
    setup: &BzlmodModuleExtensionRepoSetup,
) -> bz_error::Result<Arc<str>> {
    let mut usages: serde_json::Value = serde_json::from_str(&setup.extension_usages_json)
        .buck_error_context("Error parsing bzlmod module extension usages")?;
    if let Some(usages) = usages.as_object_mut() {
        usages.remove("usages");
    }
    let usages = serde_json::to_string(&usages)
        .buck_error_context("Error serializing bzlmod module extension evaluation usages")?;
    Ok(Arc::from(
        BzlmodModuleExtensionRepoSetup::extension_usages_key_from_json(&usages),
    ))
}

fn bzlmod_module_extension_evaluation_setup(
    setup: &BzlmodModuleExtensionRepoSetup,
) -> bz_error::Result<BzlmodModuleExtensionRepoSetup> {
    Ok(BzlmodModuleExtensionRepoSetup {
        parent_canonical_repo_name: setup.parent_canonical_repo_name.dupe(),
        parent_is_root: setup.parent_is_root,
        extension_bzl_file: setup.extension_bzl_file.dupe(),
        extension_bzl_cell: setup.extension_bzl_cell.dupe(),
        extension_bzl_path: setup.extension_bzl_path.dupe(),
        extension_unique_name: setup.extension_unique_name.dupe(),
        extension_name: setup.extension_name.dupe(),
        repo_name: Arc::from(""),
        extension_usages_key: bzlmod_module_extension_evaluation_usages_key(setup)?,
        extension_usages_json: setup.extension_usages_json.dupe(),
    })
}

fn bzlmod_module_extension_evaluation_working_dir(
    setup: &BzlmodModuleExtensionRepoSetup,
) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/cache/bzlmod_module_extensions/{}",
        bzlmod_module_extension_evaluation_cache_key(setup)
    ))
}

#[derive(
    Clone,
    Debug,
    derive_more::Display,
    PartialEq,
    Eq,
    Hash,
    allocative::Allocative,
    Pagable
)]
#[display("SINGLE_EXTENSION_EVAL({setup:?})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodSingleExtensionEvalKey {
    setup: BzlmodModuleExtensionRepoSetup,
    working_dir: ProjectRelativePathBuf,
}

#[async_trait::async_trait]
impl Key for BzlmodSingleExtensionEvalKey {
    type Value = bz_error::Result<Arc<BazelModuleExtensionEvaluationResult>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        cancellations: &CancellationContext,
    ) -> Self::Value {
        let eval_factors = BzlmodModuleExtensionEvalFactors::from_setup(ctx, &self.setup).await?;
        let bzl_transitive_digest =
            bzlmod_module_extension_bazel_bzl_transitive_digest(ctx, &self.setup).await?;
        let usages_digest =
            bzlmod_module_extension_bazel_usages_digest(ctx, &self.setup, cancellations).await?;
        if let Some(evaluation) =
            read_bzlmod_workspace_lockfile_extension_candidate(ctx, &self.setup, &eval_factors)
                .await?
        {
            if let Some(evaluation) = validate_bzlmod_workspace_lockfile_extension(
                ctx,
                evaluation,
                &bzl_transitive_digest,
                &usages_digest,
            )
            .await?
            {
                return Ok(evaluation);
            }
        }
        if let Some(evaluation) = read_bzlmod_hidden_lockfile_extension(
            ctx,
            &self.setup,
            &eval_factors,
            &bzl_transitive_digest,
            &usages_digest,
        )
        .await?
        {
            return Ok(evaluation);
        }
        let extension_bzl_file = self.setup.extension_bzl_file.to_string();
        let extension_name = self.setup.extension_name.to_string();
        let repo = self.setup.repo_name.to_string();
        let working_dir = self.working_dir.to_string();
        let evaluation = span_async_simple(
            bz_data::BzlmodModuleExtensionStart {
                extension_bzl_file: extension_bzl_file.clone(),
                extension_name: extension_name.clone(),
                repo: repo.clone(),
                working_dir: working_dir.clone(),
                progress: "starting".to_owned(),
            },
            async {
                ctx.get_blocking_executor()
                    .execute_io(
                        Box::new(bz_execute::execute::clean_output_paths::CleanOutputPaths {
                            paths: vec![self.working_dir.clone()],
                        }),
                        cancellations,
                    )
                    .await?;
                bz_error::Ok(Arc::new(
                    evaluate_bzlmod_module_extension_repo(
                        ctx,
                        &self.setup,
                        self.working_dir.as_str(),
                        None,
                        cancellations,
                    )
                    .await?,
                ))
            },
            bz_data::BzlmodModuleExtensionEnd {
                extension_bzl_file,
                extension_name,
                repo,
                working_dir,
            },
        )
        .await?;
        write_bzlmod_hidden_lockfile_extension(
            ctx,
            &self.setup,
            &eval_factors,
            &evaluation,
            bzl_transitive_digest,
            usages_digest,
        )
        .await?;
        Ok(evaluation)
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

#[derive(
    Clone,
    Debug,
    derive_more::Display,
    PartialEq,
    Eq,
    Hash,
    allocative::Allocative,
    Pagable
)]
#[display("SINGLE_EXTENSION({setup:?})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodSingleExtensionKey {
    setup: BzlmodModuleExtensionRepoSetup,
    working_dir: ProjectRelativePathBuf,
}

fn validate_bzlmod_single_extension(
    setup: &BzlmodModuleExtensionRepoSetup,
    evaluation: &BazelModuleExtensionEvaluationResult,
) -> bz_error::Result<()> {
    let config: BzlmodModuleExtensionValidationConfig =
        serde_json::from_str(&setup.extension_usages_json)
            .buck_error_context("Error parsing bzlmod module extension usages for validation")?;
    let emitted = evaluation
        .repository_rule_invocations
        .iter()
        .map(|invocation| invocation.name.clone())
        .collect::<Vec<_>>();
    let emitted_set = emitted.iter().cloned().collect::<BTreeSet<_>>();
    let repo_overrides = config
        .usages
        .iter()
        .flat_map(|usage| usage.repo_overrides.iter())
        .map(|repo_override| repo_override.repo_name.clone())
        .collect::<BTreeSet<_>>();

    for usage in &config.usages {
        for import in &usage.imports {
            if emitted_set.contains(&import.repo_name) || repo_overrides.contains(&import.repo_name)
            {
                continue;
            }
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "bzlmod module extension `{}%{}` does not generate repository `{}`, yet it is imported as `{}`; emitted repositories: {}",
                setup.extension_bzl_file,
                setup.extension_name,
                import.repo_name,
                import.alias,
                emitted.join(", ")
            ));
        }
    }

    for usage in &config.usages {
        for repo_override in &usage.repo_overrides {
            let repo_exists = emitted_set.contains(&repo_override.repo_name);
            if repo_exists && !repo_override.must_exist {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "bzlmod module extension `{}%{}` generates repository `{}`, yet it is injected via inject_repo(); use override_repo() instead",
                    setup.extension_bzl_file,
                    setup.extension_name,
                    repo_override.repo_name
                ));
            }
            if !repo_exists && repo_override.must_exist {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "bzlmod module extension `{}%{}` does not generate repository `{}`, yet it is overridden via override_repo(); use inject_repo() instead",
                    setup.extension_bzl_file,
                    setup.extension_name,
                    repo_override.repo_name
                ));
            }
        }
    }

    Ok(())
}

#[async_trait::async_trait]
impl Key for BzlmodSingleExtensionKey {
    type Value = bz_error::Result<Arc<BazelModuleExtensionEvaluationResult>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let eval_setup = bzlmod_module_extension_evaluation_setup(&self.setup)?;
        let evaluation = ctx
            .compute(&BzlmodSingleExtensionEvalKey {
                setup: eval_setup,
                working_dir: self.working_dir.clone(),
            })
            .await??;
        validate_bzlmod_single_extension(&self.setup, &evaluation)?;
        if !self.setup.repo_name.is_empty()
            && !evaluation
                .repository_rule_invocations
                .iter()
                .any(|invocation| invocation.name == self.setup.repo_name.as_ref())
        {
            let emitted = evaluation
                .repository_rule_invocations
                .iter()
                .map(|invocation| invocation.name.clone())
                .collect();
            return Err(BzlmodError::ModuleExtensionRepoNotEmitted {
                extension_bzl_file: self.setup.extension_bzl_file.to_string(),
                extension_name: self.setup.extension_name.to_string(),
                repo_name: self.setup.repo_name.to_string(),
                emitted,
            }
            .into());
        }
        Ok(evaluation)
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

async fn evaluate_cached_bzlmod_module_extension(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
    _generated_repo_path: &ProjectRelativePath,
) -> bz_error::Result<Arc<BazelModuleExtensionEvaluationResult>> {
    let working_dir = bzlmod_module_extension_evaluation_working_dir(setup);
    ctx.compute(&BzlmodSingleExtensionKey {
        setup: setup.dupe(),
        working_dir,
    })
    .await?
}

pub(crate) async fn evaluate_module_extension(
    ctx: &mut DiceComputations<'_>,
    setup: BzlmodModuleExtensionRepoSetup,
) -> bz_error::Result<Vec<String>> {
    let eval_setup = bzlmod_module_extension_evaluation_setup(&setup)?;
    let working_dir = bzlmod_module_extension_evaluation_working_dir(&eval_setup);
    let evaluation = ctx
        .compute(&BzlmodSingleExtensionEvalKey {
            setup: eval_setup,
            working_dir,
        })
        .await??;
    Ok(evaluation
        .repository_rule_invocations
        .iter()
        .map(|invocation| invocation.name.clone())
        .collect())
}

async fn evaluate_and_materialize_bzlmod_repository_rule(
    ctx: &mut DiceComputations<'_>,
    canonical_repo_name: &str,
    path: &ProjectRelativePath,
    kind: &'static str,
    invocation: &BazelRepositoryRuleInvocation,
    cancellations: &CancellationContext,
) -> bz_error::Result<BzlmodRepositoryRuleMaterializationResult> {
    let working_dir =
        bzlmod_generated_scratch_path_for_canonical(canonical_repo_name, "repository_ctx");
    let materialized_dir =
        bzlmod_generated_scratch_path_for_canonical(canonical_repo_name, "materialization");
    ctx.get_blocking_executor()
        .execute_io(
            Box::new(bz_execute::execute::clean_output_paths::CleanOutputPaths {
                paths: vec![working_dir.clone(), materialized_dir.clone()],
            }),
            cancellations,
        )
        .await?;
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let working_dir_abs = project_root
        .resolve(&working_dir)
        .as_path()
        .to_string_lossy()
        .into_owned();
    let working_dir_to_create = working_dir.clone();
    let working_dir_abs_for_eval = working_dir_abs.clone();
    ctx.get_blocking_executor()
        .execute_io_inline(move || {
            fs_util::create_dir_all(project_root.resolve(&working_dir_to_create))?;
            Ok(())
        })
        .await?;
    let result = evaluate_bzlmod_repository_rule_with_recorded_inputs(
        ctx,
        invocation,
        &working_dir_abs_for_eval,
        Some(BazelRepositoryRuleProgress {
            repo: canonical_repo_name.to_owned(),
            path: path.to_string(),
            kind: kind.to_owned(),
        }),
        cancellations,
    )
    .await?;
    let files = result
        .files
        .into_iter()
        .map(|file| BzlmodRepositoryRuleFile {
            path: Arc::from(file.path),
            content: Arc::from(file.content),
            executable: file.executable,
        })
        .collect();
    ctx.get_blocking_executor()
        .execute_io(
            Box::new(BzlmodGeneratedIoRequest {
                setup: BzlmodGeneratedCellSetup {
                    canonical_repo_name: Arc::from(canonical_repo_name),
                    generator: BzlmodGeneratedCellGenerator::RepositoryRule(
                        BzlmodRepositoryRuleSetup {
                            files: Arc::new(files),
                            source_dir: Some(Arc::from(working_dir.as_str())),
                        },
                    ),
                },
                dest: materialized_dir.clone(),
            }),
            cancellations,
        )
        .await?;
    ctx.get_blocking_executor()
        .execute_io(
            Box::new(BzlmodGeneratedPublishIoRequest {
                src: materialized_dir,
                dest: path.to_owned(),
                cleanup: vec![working_dir],
            }),
            cancellations,
        )
        .await?;
    Ok(BzlmodRepositoryRuleMaterializationResult {
        recorded_inputs: result.recorded_inputs,
        reproducible: result.reproducible,
    })
}

async fn download_impl(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodCellSetup,
    dest: &ProjectRelativePath,
    cache_repo: &ProjectRelativePath,
    cache_tmp: &ProjectRelativePath,
    cache_alias: &ProjectRelativePath,
    cancellations: &CancellationContext,
    progress_state: &BzlmodProgressState,
) -> bz_error::Result<()> {
    if bzlmod_progress_step(
        progress_state,
        "checking repository contents cache",
        prepare_bzlmod_external_cell_root_if_cache_exists(
            ctx,
            cache_repo,
            cache_alias,
            dest,
            cancellations,
        ),
    )
    .await?
    {
        bzlmod_report_progress(progress_state, "using repository contents cache");
        return Ok(());
    }

    let io = ctx.get_blocking_executor();
    let archive = bzlmod_path(setup, "source.archive");
    let temp = bzlmod_path(setup, "extract-tmp");
    let patch_dir = bzlmod_path(setup, "patches");
    let overlay_dir = bzlmod_path(setup, "overlays");
    let stamp = bzlmod_external_cell_root_stamp_path(dest);
    let patch_files: Vec<_> = setup
        .patches
        .iter()
        .enumerate()
        .map(|(idx, patch)| BzlmodPatchFile {
            path: match patch.path.as_deref() {
                Some(path) => ProjectRelativePathBuf::unchecked_new(path.to_owned()),
                None => patch_dir.join(ForwardRelativePath::new(&format!("{idx}.patch")).unwrap()),
            },
            patch_strip: patch.patch_strip,
        })
        .collect();
    let overlay_files: Vec<_> = setup
        .overlays
        .iter()
        .enumerate()
        .map(|(idx, overlay)| BzlmodOverlayFile {
            path: overlay.path.to_string(),
            file: overlay_dir.join(ForwardRelativePath::new(&format!("{idx}.overlay")).unwrap()),
        })
        .collect();

    bzlmod_progress_step(
        progress_state,
        "cleaning repository materialization paths",
        io.execute_io(
            Box::new(bz_execute::execute::clean_output_paths::CleanOutputPaths {
                paths: vec![
                    stamp,
                    dest.to_owned(),
                    archive.clone(),
                    temp.clone(),
                    patch_dir.clone(),
                    overlay_dir.clone(),
                    cache_tmp.to_owned(),
                ],
            }),
            cancellations,
        ),
    )
    .await?;

    let io_provider = ctx.global_data().get_io_provider();
    let project_root = io_provider.project_root();
    let digest_config = ctx.global_data().get_digest_config();
    let remote_downloader = bzlmod_remote_downloader_config(ctx);
    bzlmod_report_progress(progress_state, "initializing repository download client");
    let client = shared_bzlmod_download_http_client().await?;
    let bazel_download_headers = bazel_repository_download_headers(std::iter::empty());
    let archive_checksum = checksum_from_integrity(&setup.integrity)?;
    let archive_urls = bzlmod_cell_setup_urls(setup);
    bzlmod_download_any_with_headers(
        remote_downloader.as_ref(),
        &client,
        project_root,
        digest_config.dupe(),
        &archive,
        &archive_urls,
        &archive_checksum,
        false,
        &bazel_download_headers,
        Some(progress_state),
        "module archive",
    )
    .await?;

    let patch_count = setup
        .patches
        .iter()
        .filter(|patch| patch.path.is_none())
        .count();
    let mut patch_idx = 0;
    for (patch, output) in setup.patches.iter().zip(&patch_files) {
        if patch.path.is_some() {
            continue;
        }
        patch_idx += 1;
        let checksum = checksum_from_integrity(&patch.integrity)?;
        bzlmod_download_with_headers(
            remote_downloader.as_ref(),
            &client,
            project_root,
            digest_config.dupe(),
            &output.path,
            &patch.url,
            &checksum,
            false,
            &bazel_download_headers,
            Some(progress_state),
            &format!("patch {patch_idx}/{patch_count}"),
        )
        .await?;
    }

    let overlay_count = setup.overlays.len();
    for (overlay_idx, (overlay, output)) in setup.overlays.iter().zip(&overlay_files).enumerate() {
        let checksum = checksum_from_integrity(&overlay.integrity)?;
        bzlmod_download_with_headers(
            remote_downloader.as_ref(),
            &client,
            project_root,
            digest_config.dupe(),
            &output.file,
            &overlay.url,
            &checksum,
            false,
            &bazel_download_headers,
            Some(progress_state),
            &format!("overlay {}/{}", overlay_idx + 1, overlay_count),
        )
        .await?;
    }

    bzlmod_progress_step(
        progress_state,
        "extracting and publishing module archive",
        io.execute_io(
            Box::new(BzlmodExtractIoRequest {
                setup: setup.dupe(),
                archive,
                patch_files,
                overlay_files,
                temp,
                cache_repo: cache_repo.to_owned(),
                cache_tmp: cache_tmp.to_owned(),
                cache_alias: cache_alias.to_owned(),
                dest: dest.to_owned(),
            }),
            cancellations,
        ),
    )
    .await?;

    Ok(())
}

async fn prepare_bzlmod_external_cell_root_if_cache_exists(
    ctx: &mut DiceComputations<'_>,
    cache_repo: &ProjectRelativePath,
    cache_alias: &ProjectRelativePath,
    dest: &ProjectRelativePath,
    _cancellations: &CancellationContext,
) -> bz_error::Result<bool> {
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let cache_repo = cache_repo.to_owned();
    let cache_alias = cache_alias.to_owned();
    let dest = dest.to_owned();
    run_bzlmod_cache_io(move || {
        if !bzlmod_repo_contents_cache_exists(&project_root, &cache_repo)? {
            return Ok(false);
        }

        record_bzlmod_repo_contents_cache_alias(&project_root, &cache_alias, &cache_repo)?;
        prepare_bzlmod_external_cell_root(&project_root, &cache_repo, &dest)?;
        Ok(true)
    })
    .await
}

async fn prepare_bzlmod_external_cell_root_from_source(
    ctx: &mut DiceComputations<'_>,
    source: &ProjectRelativePath,
    dest: &ProjectRelativePath,
    cancellations: &CancellationContext,
) -> bz_error::Result<()> {
    ctx.get_blocking_executor()
        .execute_io(
            Box::new(BzlmodPrepareExternalCellRootIoRequest {
                cache_repo: source.to_owned(),
                cache_alias: None,
                dest: dest.to_owned(),
            }),
            cancellations,
        )
        .await
}

async fn bzlmod_download_any_with_headers(
    remote_downloader: Option<&BzlmodRemoteDownloaderConfig>,
    client: &HttpClient,
    fs: &ProjectRoot,
    digest_config: bz_execute::digest_config::DigestConfig,
    path: &ProjectRelativePath,
    urls: &[String],
    checksum: &Checksum,
    executable: bool,
    headers: &[(String, String)],
    progress_state: Option<&BzlmodProgressState>,
    progress_subject: &str,
) -> bz_error::Result<()> {
    if let Some(remote_downloader) = remote_downloader {
        return remote_asset_download_any_with_headers(
            remote_downloader,
            fs,
            digest_config,
            path,
            urls,
            checksum,
            executable,
            headers,
            progress_state,
            progress_subject,
        )
        .await;
    }

    let mut last_error = None;
    for (idx, url) in urls.iter().enumerate() {
        match bzlmod_http_download_with_headers(
            client,
            fs,
            digest_config.dupe(),
            path,
            url,
            checksum,
            executable,
            headers,
            progress_state,
            &format!("{progress_subject} from URL {}/{}", idx + 1, urls.len()),
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(BzlmodError::DownloadFailed {
        urls: urls.to_owned(),
        error: last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no URL provided".to_owned()),
    }
    .into())
}

async fn bzlmod_download_with_headers(
    remote_downloader: Option<&BzlmodRemoteDownloaderConfig>,
    client: &HttpClient,
    fs: &ProjectRoot,
    digest_config: bz_execute::digest_config::DigestConfig,
    path: &ProjectRelativePath,
    url: &str,
    checksum: &Checksum,
    executable: bool,
    headers: &[(String, String)],
    progress_state: Option<&BzlmodProgressState>,
    progress_subject: &str,
) -> bz_error::Result<()> {
    if let Some(remote_downloader) = remote_downloader {
        return remote_asset_download_any_with_headers(
            remote_downloader,
            fs,
            digest_config,
            path,
            &[url.to_owned()],
            checksum,
            executable,
            headers,
            progress_state,
            progress_subject,
        )
        .await;
    }

    bzlmod_http_download_with_headers(
        client,
        fs,
        digest_config,
        path,
        url,
        checksum,
        executable,
        headers,
        progress_state,
        progress_subject,
    )
    .await
}

async fn bzlmod_http_download_with_headers(
    client: &HttpClient,
    fs: &ProjectRoot,
    digest_config: bz_execute::digest_config::DigestConfig,
    path: &ProjectRelativePath,
    url: &str,
    checksum: &Checksum,
    executable: bool,
    headers: &[(String, String)],
    progress_state: Option<&BzlmodProgressState>,
    progress_subject: &str,
) -> bz_error::Result<()> {
    if let Some(progress_state) = progress_state {
        bzlmod_report_progress(
            progress_state,
            format!("waiting for download slot for {progress_subject}"),
        );
    }
    let _permit = BZLMOD_DOWNLOAD_SEMAPHORE
        .acquire()
        .await
        .expect("bzlmod download semaphore should not be closed");
    if let Some(progress_state) = progress_state {
        bzlmod_report_progress(progress_state, format!("downloading {progress_subject}"));
    }
    http_download_with_headers(
        client,
        fs,
        digest_config,
        path,
        url,
        checksum,
        executable,
        headers,
    )
    .await?;
    Ok(())
}

async fn remote_asset_download_any_with_headers(
    config: &BzlmodRemoteDownloaderConfig,
    fs: &ProjectRoot,
    digest_config: bz_execute::digest_config::DigestConfig,
    path: &ProjectRelativePath,
    urls: &[String],
    checksum: &Checksum,
    executable: bool,
    headers: &[(String, String)],
    progress_state: Option<&BzlmodProgressState>,
    progress_subject: &str,
) -> bz_error::Result<()> {
    if let Some(progress_state) = progress_state {
        bzlmod_report_progress(
            progress_state,
            format!("waiting for remote downloader slot for {progress_subject}"),
        );
    }
    let _permit = BZLMOD_DOWNLOAD_SEMAPHORE
        .acquire()
        .await
        .expect("bzlmod download semaphore should not be closed");
    if let Some(progress_state) = progress_state {
        bzlmod_report_progress(
            progress_state,
            format!(
                "fetching {progress_subject} with remote downloader from {} URL{}",
                urls.len(),
                if urls.len() == 1 { "" } else { "s" }
            ),
        );
    }

    let digest = remote_asset_fetch_blob(config, urls, checksum, headers).await?;
    if let Some(progress_state) = progress_state {
        bzlmod_report_progress(
            progress_state,
            format!("downloading {progress_subject} from remote CAS"),
        );
    }
    remote_asset_download_blob(config, fs, digest_config, path, &digest).await?;
    if executable {
        fs.set_executable(path)?;
    }
    Ok(())
}

async fn remote_asset_fetch_blob(
    config: &BzlmodRemoteDownloaderConfig,
    urls: &[String],
    checksum: &Checksum,
    headers: &[(String, String)],
) -> bz_error::Result<RemoteExecutionDigest> {
    let endpoint = ParsedRemoteAssetEndpoint::parse(&config.endpoint)?;
    let mut endpoint_builder = Endpoint::from_shared(endpoint.uri.clone()).map_err(|error| {
        BzlmodError::InvalidRemoteDownloaderEndpoint {
            endpoint: config.endpoint.clone(),
            error: error.to_string(),
        }
    })?;
    if endpoint.tls {
        endpoint_builder = endpoint_builder
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|error| BzlmodError::InvalidRemoteDownloaderEndpoint {
                endpoint: config.endpoint.clone(),
                error: error.to_string(),
            })?;
    }
    let channel =
        endpoint_builder
            .connect()
            .await
            .map_err(|error| BzlmodError::RemoteDownloaderFailed {
                urls: urls.to_owned(),
                error: error.to_string(),
            })?;
    let mut client = tonic::client::Grpc::new(channel);

    let mut qualifiers = Vec::new();
    if let Some(integrity) = checksum_to_subresource_integrity(checksum)? {
        qualifiers.push(RemoteAssetQualifier {
            name: "checksum.sri".to_owned(),
            value: integrity,
        });
    }
    for (name, value) in headers {
        qualifiers.push(RemoteAssetQualifier {
            name: format!("http_header:{name}"),
            value: value.clone(),
        });
    }

    let request = FetchBlobRequest {
        instance_name: String::new(),
        timeout: Some(prost_types::Duration {
            seconds: BZLMOD_REPOSITORY_MATERIALIZATION_TIMEOUT.as_secs() as i64,
            nanos: 0,
        }),
        oldest_content_accepted: remote_asset_oldest_content_accepted(checksum)?,
        uris: urls.to_owned(),
        qualifiers,
        digest_function: RemoteExecutionDigestFunction::Sha256 as i32,
    };
    let mut request = tonic::Request::new(request);
    add_remote_asset_metadata(request.metadata_mut(), config)?;

    let path = tonic::codegen::http::uri::PathAndQuery::from_static(
        "/build.bazel.remote.asset.v1.Fetch/FetchBlob",
    );
    let codec = tonic_prost::ProstCodec::<FetchBlobRequest, FetchBlobResponse>::default();
    client
        .ready()
        .await
        .map_err(|error| BzlmodError::RemoteDownloaderFailed {
            urls: urls.to_owned(),
            error: error.to_string(),
        })?;
    let response = client
        .unary(request, path, codec)
        .await
        .map_err(|error| BzlmodError::RemoteDownloaderFailed {
            urls: urls.to_owned(),
            error: error.to_string(),
        })?
        .into_inner();

    if let Some(status) = &response.status
        && status.code != 0
    {
        return Err(BzlmodError::RemoteDownloaderStatus {
            urls: urls.to_owned(),
            code: status.code,
            message: status.message.clone(),
        }
        .into());
    }
    response.blob_digest.ok_or_else(|| {
        BzlmodError::RemoteDownloaderMissingDigest {
            urls: urls.to_owned(),
        }
        .into()
    })
}

fn remote_asset_oldest_content_accepted(
    checksum: &Checksum,
) -> bz_error::Result<Option<prost_types::Timestamp>> {
    if !matches!(checksum, Checksum::None) {
        return Ok(None);
    }

    // Match Bazel's GrpcRemoteDownloader: checksumless downloads are mutable, so never accept
    // cached Remote Asset content. The hour offset allows for clock skew.
    let timestamp = SystemTime::now()
        .checked_add(Duration::from_secs(60 * 60))
        .ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "system time overflow computing remote downloader cache cutoff"
            )
        })?
        .duration_since(UNIX_EPOCH)
        .buck_error_context(
            "system time is before Unix epoch computing remote downloader cache cutoff",
        )?;
    Ok(Some(prost_types::Timestamp {
        seconds: timestamp.as_secs() as i64,
        nanos: timestamp.subsec_nanos() as i32,
    }))
}

async fn remote_asset_download_blob(
    config: &BzlmodRemoteDownloaderConfig,
    fs: &ProjectRoot,
    digest_config: bz_execute::digest_config::DigestConfig,
    path: &ProjectRelativePath,
    digest: &RemoteExecutionDigest,
) -> bz_error::Result<TrackedFileDigest> {
    let endpoint = ParsedRemoteAssetEndpoint::parse(&config.endpoint)?;
    let mut endpoint_builder = Endpoint::from_shared(endpoint.uri.clone()).map_err(|error| {
        BzlmodError::InvalidRemoteDownloaderEndpoint {
            endpoint: config.endpoint.clone(),
            error: error.to_string(),
        }
    })?;
    if endpoint.tls {
        endpoint_builder = endpoint_builder
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|error| BzlmodError::InvalidRemoteDownloaderEndpoint {
                endpoint: config.endpoint.clone(),
                error: error.to_string(),
            })?;
    }
    let channel =
        endpoint_builder
            .connect()
            .await
            .map_err(|error| BzlmodError::RemoteDownloaderFailed {
                urls: Vec::new(),
                error: error.to_string(),
            })?;
    let mut client = ByteStreamClient::new(channel);
    let mut request = tonic::Request::new(ReadRequest {
        resource_name: bytestream_download_resource_name("", digest),
        read_offset: 0,
        read_limit: 0,
    });
    add_remote_asset_metadata(request.metadata_mut(), config)?;
    let mut stream = client
        .read(request)
        .await
        .map_err(|error| BzlmodError::RemoteDownloaderFailed {
            urls: Vec::new(),
            error: error.to_string(),
        })?
        .into_inner();

    let abs_path = fs.resolve(path);
    if let Some(dir) = abs_path.parent() {
        fs_util::create_dir_all(dir)?;
    }
    let mut file = tokio::fs::File::create(abs_path.as_path())
        .await
        .with_buck_error_context(|| format!("create({abs_path})"))?;
    while let Some(response) =
        stream
            .message()
            .await
            .map_err(|error| BzlmodError::RemoteDownloaderFailed {
                urls: Vec::new(),
                error: error.to_string(),
            })?
    {
        file.write_all(&response.data)
            .await
            .with_buck_error_context(|| format!("write({abs_path})"))?;
    }
    file.flush()
        .await
        .with_buck_error_context(|| format!("flush({abs_path})"))?;

    let file_digest_config = FileDigestConfig::build(digest_config.cas_digest_config());
    let file_digest = FileDigest::from_file(&abs_path, file_digest_config)?;
    Ok(TrackedFileDigest::new(
        file_digest,
        digest_config.cas_digest_config(),
    ))
}

fn add_remote_asset_metadata(
    metadata: &mut tonic::metadata::MetadataMap,
    config: &BzlmodRemoteDownloaderConfig,
) -> bz_error::Result<()> {
    if let Some(api_key) = config
        .api_key
        .as_ref()
        .filter(|api_key| !api_key.trim().is_empty())
    {
        metadata.insert(
            BUILDBUDDY_API_KEY_HEADER,
            MetadataValue::try_from(api_key.as_str()).map_err(|error| {
                bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "invalid `{BUILDBUDDY_API_KEY_HEADER}` metadata value: {error}"
                )
            })?,
        );
    }
    Ok(())
}

fn bytestream_download_resource_name(
    instance_name: &str,
    digest: &RemoteExecutionDigest,
) -> String {
    let blob = format!("blobs/{}/{}", digest.hash, digest.size_bytes);
    if instance_name.is_empty() {
        blob
    } else {
        format!("{instance_name}/{blob}")
    }
}

fn checksum_to_subresource_integrity(checksum: &Checksum) -> bz_error::Result<Option<String>> {
    let Some((algorithm, hex_digest)) = (match checksum {
        Checksum::None => None,
        Checksum::Sha1(sha1) => Some(("sha1", sha1.as_ref())),
        Checksum::Sha256(sha256) => Some(("sha256", sha256.as_ref())),
        Checksum::Sha384(sha384) => Some(("sha384", sha384.as_ref())),
        Checksum::Sha512(sha512) => Some(("sha512", sha512.as_ref())),
        Checksum::Both { sha256, .. } => Some(("sha256", sha256.as_ref())),
    }) else {
        return Ok(None);
    };
    let bytes = hex::decode(hex_digest)?;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(Some(format!("{algorithm}-{encoded}")))
}

async fn bzlmod_download_http_client() -> bz_error::Result<HttpClient> {
    let mut builder = HttpClientBuilder::oss().await?;
    builder
        .with_max_redirects(BZLMOD_DOWNLOAD_MAX_REDIRECTS)
        .with_http2(false)
        .with_connect_timeout(Some(BZLMOD_DOWNLOAD_CONNECT_TIMEOUT))
        .with_response_header_timeout(Some(BZLMOD_DOWNLOAD_RESPONSE_HEADER_TIMEOUT))
        .with_read_timeout(Some(BZLMOD_DOWNLOAD_READ_TIMEOUT))
        .with_write_timeout(Some(BZLMOD_DOWNLOAD_WRITE_TIMEOUT));
    Ok(builder.build())
}

async fn shared_bzlmod_download_http_client() -> bz_error::Result<HttpClient> {
    Ok(BZLMOD_DOWNLOAD_HTTP_CLIENT
        .get_or_try_init(bzlmod_download_http_client)
        .await?
        .dupe())
}

async fn declare_existing_directory(
    ctx: &mut DiceComputations<'_>,
    dest: &ProjectRelativePath,
    materializer: &dyn Materializer,
) -> bz_error::Result<()> {
    let io = ctx.get_blocking_executor();
    let io_provider = ctx.global_data().get_io_provider();
    let project_root = io_provider.project_root();
    let digest_config = ctx.global_data().get_digest_config();
    let proj_root = project_root.root();
    let abs_path = proj_root.join(dest);
    let file_digest_config = FileDigestConfig::build(digest_config.cas_digest_config());
    let entry = build_entry_from_disk(abs_path, file_digest_config, &*io, proj_root)
        .await?
        .0
        .ok_or(BzlmodError::NoDirectory)?;
    let entry = entry.map_dir(|d| {
        d.to_builder()
            .fingerprint(digest_config.as_directory_serializer())
            .shared(&*INTERNER)
    });

    materializer
        .declare_existing(vec![DeclareArtifactPayload {
            path: dest.to_owned(),
            artifact: ArtifactValue::new(entry, None),
            configuration_path: None,
        }])
        .await?;

    Ok(())
}

async fn declare_observed_source_artifact(
    ctx: &mut DiceComputations<'_>,
    path: ProjectRelativePathBuf,
    metadata: &RawPathMetadata<ProjectRelativePathBuf>,
) -> bz_error::Result<()> {
    let member = match metadata {
        RawPathMetadata::File(metadata) => ActionDirectoryMember::File(metadata.clone()),
        RawPathMetadata::Symlink {
            at: _,
            to: RawSymlink::Relative(_, symlink),
        } => ActionDirectoryMember::Symlink(symlink.dupe()),
        RawPathMetadata::Symlink {
            at: _,
            to: RawSymlink::External(symlink),
        } => ActionDirectoryMember::ExternalSymlink(symlink.dupe()),
        RawPathMetadata::Directory => return Ok(()),
    };

    ctx.per_transaction_data()
        .get_materializer()
        .declare_existing(vec![DeclareArtifactPayload {
            path,
            artifact: ArtifactValue::new(ActionDirectoryEntry::Leaf(member), None),
            configuration_path: None,
        }])
        .await?;
    Ok(())
}

async fn materialize_observed_bzlmod_source_path(
    _ctx: &mut DiceComputations<'_>,
    _source_path: ProjectRelativePathBuf,
    _dest_path: ProjectRelativePathBuf,
    metadata: &RawPathMetadata<ProjectRelativePathBuf>,
) -> bz_error::Result<()> {
    match metadata {
        RawPathMetadata::File(_) | RawPathMetadata::Symlink { .. } => {}
        RawPathMetadata::Directory => return Ok(()),
    }

    // Bzlmod roots are symlinks to their content cache. The source path under
    // the external cell root already exists, so declaring it to the materializer
    // is enough and avoids copying whole repositories into every isolation.
    Ok(())
}

async fn download_and_materialize(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodCellSetup,
    cancellations: &CancellationContext,
) -> bz_error::Result<()> {
    let repo = setup.canonical_repo_name.to_string();
    let path_str = path.to_string();
    let kind = "module archive".to_owned();
    let progress_state = Arc::new(Mutex::new("starting".to_owned()));
    span_async_simple(
        bz_data::BzlmodRepoStart {
            repo: repo.clone(),
            path: path_str.clone(),
            kind: kind.clone(),
            progress: "starting".to_owned(),
        },
        {
            let progress_state = progress_state.dupe();
            let timeout_repo = repo.clone();
            let timeout_path = path_str.clone();
            async move {
                let materialization_progress = progress_state.dupe();
                let materialization = async {
                    bzlmod_report_progress(
                        &materialization_progress,
                        "waiting for materialization lock",
                    );
                    let lock = bzlmod_materialization_lock(path);
                    let _guard = lock.lock().await;
                    let cache_key = bzlmod_repo_contents_cache_key(setup);
                    let cache_repo = bzlmod_repo_contents_cache_path(&cache_key, "repo");
                    let cache_tmp = bzlmod_repo_contents_cache_path(
                        &cache_key,
                        &format!("repo.tmp.{}", std::process::id()),
                    );
                    let cache_alias =
                        bzlmod_repo_contents_cache_alias_path(&setup.canonical_repo_name);
                    bzlmod_report_progress(
                        &materialization_progress,
                        "waiting for repository contents cache lock",
                    );
                    let cache_lock = bzlmod_materialization_lock(&cache_repo);
                    let _cache_guard = cache_lock.lock().await;

                    cancellations
                        .critical_section(|| {
                            download_impl(
                                ctx,
                                setup,
                                path,
                                &cache_repo,
                                &cache_tmp,
                                &cache_alias,
                                cancellations,
                                &materialization_progress,
                            )
                        })
                        .await
                };

                match tokio::time::timeout(
                    BZLMOD_REPOSITORY_MATERIALIZATION_TIMEOUT,
                    materialization,
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(BzlmodError::RepositoryMaterializationTimedOut {
                        repo: timeout_repo,
                        path: timeout_path,
                        timeout_secs: BZLMOD_REPOSITORY_MATERIALIZATION_TIMEOUT.as_secs(),
                        progress: bzlmod_current_progress(&progress_state),
                    }
                    .into()),
                }
            }
        },
        bz_data::BzlmodRepoEnd {
            repo,
            path: path_str,
            kind,
        },
    )
    .await
}

async fn bzlmod_generated_repo_contents_cache_info(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
) -> bz_error::Result<Option<BazelRepositoryRuleCacheInfo>> {
    let invocation = match &setup.generator {
        BzlmodGeneratedCellGenerator::ModuleExtensionRepo(module_extension) => {
            let evaluation =
                evaluate_cached_bzlmod_module_extension(ctx, module_extension, path).await?;
            let entries = compute_bzlmod_module_extension_repo_mapping_entries(
                ctx,
                setup,
                module_extension,
                path,
            )
            .await?;
            register_bzlmod_module_extension_repo_mapping_entries(&entries)?;
            let Some(invocation) = evaluation
                .repository_rule_invocations
                .iter()
                .find(|invocation| invocation.name == module_extension.repo_name.as_ref())
            else {
                let emitted = evaluation
                    .repository_rule_invocations
                    .iter()
                    .map(|invocation| invocation.name.clone())
                    .collect();
                return Err(BzlmodError::ModuleExtensionRepoNotEmitted {
                    extension_bzl_file: module_extension.extension_bzl_file.to_string(),
                    extension_name: module_extension.extension_name.to_string(),
                    repo_name: module_extension.repo_name.to_string(),
                    emitted,
                }
                .into());
            };
            let mut invocation = invocation.clone();
            invocation.name = setup.canonical_repo_name.to_string();
            invocation
        }
        BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(invocation_setup) => {
            bzlmod_repository_rule_invocation_from_setup(
                invocation_setup,
                &setup.canonical_repo_name,
            )?
        }
        _ => return Ok(None),
    };

    bzlmod_repository_rule_cache_info(ctx, &invocation)
        .await
        .map(Some)
}

async fn materialize_generated(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cancellations: &CancellationContext,
) -> bz_error::Result<Arc<BzlmodGeneratedCellMaterializationValue>> {
    let lock = bzlmod_materialization_lock(path);
    let _guard = lock.lock().await;
    if let Some(cache_info) = bzlmod_generated_repo_contents_cache_info(ctx, path, setup).await? {
        return materialize_generated_with_repo_contents_cache(
            ctx,
            path,
            setup,
            &cache_info,
            cancellations,
        )
        .await;
    }

    let stamp_content = bzlmod_generated_materialization_stamp_content(setup);
    if bzlmod_generated_materialization_is_current(ctx, path, setup).await? {
        return bzlmod_generated_materialization_value(ctx, path, setup, &stamp_content).await;
    }

    cancellations
        .critical_section(|| async move {
            let stamp_path = bzlmod_generated_materialization_stamp_path(setup, path);
            let value_path = bzlmod_generated_materialization_value_path(setup, path);
            ctx.get_blocking_executor()
                .execute_io(
                    Box::new(bz_execute::execute::clean_output_paths::CleanOutputPaths {
                        paths: vec![stamp_path, value_path],
                    }),
                    cancellations,
                )
                .await?;
            materialize_generated_contents(ctx, path, setup, cancellations).await?;
            normalize_materialized_generated_repo(ctx, path, setup, cancellations).await?;
            write_bzlmod_generated_materialization_stamp(ctx, path, setup).await?;
            write_new_bzlmod_generated_materialization_value_stamp(ctx, path, setup).await?;
            bzlmod_generated_materialization_value(ctx, path, setup, &stamp_content).await
        })
        .await
}

async fn normalize_materialized_generated_repo(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cancellations: &CancellationContext,
) -> bz_error::Result<()> {
    ctx.get_blocking_executor()
        .execute_io(
            Box::new(BzlmodGeneratedNormalizeIoRequest {
                canonical_repo_name: setup.canonical_repo_name.dupe(),
                dest: path.to_owned(),
            }),
            cancellations,
        )
        .await
}

async fn promote_current_bzlmod_generated_repo_to_cache(
    ctx: &mut DiceComputations<'_>,
    cache_info: &BazelRepositoryRuleCacheInfo,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
) -> bz_error::Result<bool> {
    let recorded_inputs_path = bzlmod_generated_recorded_inputs_path(setup, path);
    let Some(recorded_inputs_json) = (&*ctx.global_data().get_io_provider())
        .read_file_if_exists(recorded_inputs_path)
        .await?
    else {
        return Ok(false);
    };
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let entry_name = bzlmod_generated_repo_contents_cache_new_entry_name();
    let cache_repo = bzlmod_generated_repo_contents_cache_entry_path(cache_info, &entry_name);
    let cache_recorded_inputs =
        bzlmod_generated_repo_contents_cache_recorded_inputs_path(cache_info, &entry_name);
    let path = path.to_owned();
    let setup = setup.dupe();
    let cache_info = cache_info.clone();
    run_bzlmod_cache_io(move || {
        let path_abs = project_root.resolve(&path);
        let Some(path_metadata) = fs_util::symlink_metadata_if_exists(&path_abs)? else {
            return Ok(false);
        };
        if !path_metadata.is_dir() {
            return Ok(false);
        }

        let cache_repo_abs = project_root.resolve(&cache_repo);
        let safe_for_repo_contents_cache =
            if let Some(external_cell_root) = bzlmod_generated_external_cell_root_path(&path) {
                let external_cell_root = project_root.resolve(&external_cell_root);
                replant_bzlmod_generated_repo_symlinks(&path_abs, &external_cell_root)?
            } else {
                true
            };
        if !safe_for_repo_contents_cache {
            write_bzlmod_generated_materialization_stamp_content_sync(
                &project_root,
                &path,
                &setup,
                &cache_info,
            )?;
            return Ok(true);
        }
        if let Some(parent) = cache_repo_abs.parent() {
            fs_util::create_dir_all(parent)?;
        }
        fs_util::remove_all(&cache_repo_abs).categorize_internal()?;
        match fs_util::rename(&path_abs, &cache_repo_abs) {
            Ok(()) => {}
            Err(error) if cache_repo_abs.exists() => {
                fs_util::remove_all(&path_abs).categorize_internal()?;
                drop(error);
            }
            Err(error) => return Err(error.categorize_internal()),
        }
        write_bzlmod_generated_recorded_inputs_json(
            &project_root,
            &cache_recorded_inputs,
            &recorded_inputs_json,
        )?;
        write_bzlmod_generated_repo_contents_cache_latest(&project_root, &cache_info, &entry_name)?;
        prepare_bzlmod_generated_external_cell_root_with_repository_rule_stamp(
            &project_root,
            &cache_repo,
            &path,
            &setup,
            &cache_info,
        )?;
        let recorded_inputs_path = bzlmod_generated_recorded_inputs_path(&setup, &path);
        write_bzlmod_generated_recorded_inputs_json(
            &project_root,
            &recorded_inputs_path,
            &recorded_inputs_json,
        )?;
        Ok(true)
    })
    .await
}

async fn materialize_generated_with_repo_contents_cache(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cache_info: &BazelRepositoryRuleCacheInfo,
    cancellations: &CancellationContext,
) -> bz_error::Result<Arc<BzlmodGeneratedCellMaterializationValue>> {
    let cache_dir = bzlmod_generated_repo_contents_cache_entry_dir(cache_info);
    let cache_lock = bzlmod_materialization_lock(&cache_dir);
    let _cache_guard = cache_lock.lock().await;
    let stamp_content =
        bzlmod_generated_repository_rule_materialization_stamp_content(setup, cache_info);

    if bzlmod_generated_materialization_is_current_with_stamp_content(
        ctx,
        path,
        setup,
        stamp_content.clone(),
    )
    .await?
    {
        if !cache_info.local {
            let _ = promote_current_bzlmod_generated_repo_to_cache(ctx, cache_info, path, setup)
                .await?;
        }
        return bzlmod_generated_materialization_value(ctx, path, setup, &stamp_content).await;
    }

    if !cache_info.local
        && prepare_bzlmod_generated_external_cell_root_from_repo_contents_cache(
            ctx, cache_info, path, setup,
        )
        .await?
    {
        normalize_materialized_generated_repo(ctx, path, setup, cancellations).await?;
        write_new_bzlmod_generated_materialization_value_stamp(ctx, path, setup).await?;
        return bzlmod_generated_materialization_value(ctx, path, setup, &stamp_content).await;
    }

    cancellations
        .critical_section(|| async move {
            let stamp_path = bzlmod_generated_materialization_stamp_path(setup, path);
            let value_path = bzlmod_generated_materialization_value_path(setup, path);
            ctx.get_blocking_executor()
                .execute_io(
                    Box::new(bz_execute::execute::clean_output_paths::CleanOutputPaths {
                        paths: vec![stamp_path, value_path],
                    }),
                    cancellations,
                )
                .await?;

            let result = materialize_generated_contents(ctx, path, setup, cancellations).await?;
            normalize_materialized_generated_repo(ctx, path, setup, cancellations).await?;
            if result.cacheable && !cache_info.local {
                if !promote_current_bzlmod_generated_repo_to_cache(ctx, cache_info, path, setup)
                    .await?
                {
                    return Err(BzlmodError::MissingExtractedDirectory(path.to_string()).into());
                }
            } else {
                write_bzlmod_generated_materialization_stamp_content(
                    ctx,
                    path,
                    setup,
                    stamp_content.clone(),
                )
                .await?;
            }
            write_new_bzlmod_generated_materialization_value_stamp(ctx, path, setup).await?;
            bzlmod_generated_materialization_value(ctx, path, setup, &stamp_content).await
        })
        .await
}

fn bzlmod_module_extension_unique_name(
    generated_setup: &BzlmodGeneratedCellSetup,
    module_extension: &BzlmodModuleExtensionRepoSetup,
) -> Option<String> {
    if !module_extension.extension_unique_name.is_empty() {
        return Some(module_extension.extension_unique_name.to_string());
    }
    generated_setup
        .canonical_repo_name
        .strip_suffix(&format!("+{}", module_extension.repo_name))
        .map(str::to_owned)
}

#[derive(Clone, Debug, PartialEq, Eq, allocative::Allocative, Pagable)]
struct BzlmodModuleExtensionRepoMappingEntries {
    registration_key: Option<String>,
    sibling_origins: Vec<(String, String, BzlmodGeneratedCellSetup)>,
    cell_aliases: Vec<(String, Vec<(String, String)>)>,
    fingerprint: [u8; 32],
}

impl BzlmodModuleExtensionRepoMappingEntries {
    fn new(
        registration_key: Option<String>,
        sibling_origins: Vec<(String, String, BzlmodGeneratedCellSetup)>,
        cell_aliases: Vec<(String, Vec<(String, String)>)>,
    ) -> Self {
        let fingerprint = bzlmod_module_extension_repo_mapping_entries_fingerprint(
            &registration_key,
            &sibling_origins,
            &cell_aliases,
        );
        Self {
            registration_key,
            sibling_origins,
            cell_aliases,
            fingerprint,
        }
    }
}

#[derive(
    Clone,
    Debug,
    derive_more::Display,
    PartialEq,
    Eq,
    Hash,
    allocative::Allocative,
    Pagable
)]
#[display("MODULE_EXTENSION_REPO_MAPPING_ENTRIES({module_extension:?})")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModuleExtensionRepoMappingEntriesKey {
    extension_unique_name: String,
    module_extension: BzlmodModuleExtensionRepoSetup,
    working_dir: ProjectRelativePathBuf,
}

#[async_trait::async_trait]
impl Key for BzlmodModuleExtensionRepoMappingEntriesKey {
    type Value = bz_error::Result<Arc<BzlmodModuleExtensionRepoMappingEntries>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let evaluation = ctx
            .compute(&BzlmodSingleExtensionKey {
                setup: self.module_extension.dupe(),
                working_dir: self.working_dir.clone(),
            })
            .await??;
        let mapping_base = ctx
            .get_bzlmod_module_extension_repo_mapping_base(
                &self.module_extension.extension_bzl_cell,
                &self.module_extension.extension_bzl_path,
                &self.module_extension.extension_name,
            )
            .await?;
        Ok(Arc::new(bzlmod_module_extension_repo_mapping_entries(
            &self.extension_unique_name,
            &self.module_extension,
            &evaluation,
            &mapping_base,
        )?))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x.fingerprint == y.fingerprint,
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

fn bzlmod_module_extension_repo_mapping_entries(
    extension_unique_name: &str,
    module_extension: &BzlmodModuleExtensionRepoSetup,
    evaluation: &BazelModuleExtensionEvaluationResult,
    mapping_base: &BzlmodModuleExtensionRepoMappingBase,
) -> bz_error::Result<BzlmodModuleExtensionRepoMappingEntries> {
    if extension_unique_name.is_empty() {
        return Ok(BzlmodModuleExtensionRepoMappingEntries::new(
            None,
            Vec::new(),
            Vec::new(),
        ));
    }

    let mut visible_aliases = mapping_base
        .host_aliases
        .iter()
        .cloned()
        .collect::<BTreeMap<_, _>>();

    let mut sibling_cells = BTreeMap::new();
    for invocation in &evaluation.repository_rule_invocations {
        let canonical_repo_name = format!("{extension_unique_name}+{}", invocation.name);
        let cell_name = bzlmod_cell_name(&canonical_repo_name);
        sibling_cells.insert(invocation.name.clone(), (cell_name, canonical_repo_name));
    }
    visible_aliases.extend(sibling_cells.iter().map(
        |(repo_name, (cell_name, _canonical_repo_name))| (repo_name.clone(), cell_name.clone()),
    ));
    visible_aliases.extend(mapping_base.repo_overrides.iter().cloned());

    let visible_aliases = visible_aliases.into_iter().collect::<Vec<_>>();
    let mut sibling_origins = Vec::new();
    let mut cell_aliases = Vec::new();
    for (repo_name, (cell_name, canonical_repo_name)) in sibling_cells {
        let mut sibling_module_extension = module_extension.dupe();
        sibling_module_extension.extension_unique_name = Arc::from(extension_unique_name);
        sibling_module_extension.repo_name = Arc::from(repo_name);
        let sibling_setup = BzlmodGeneratedCellSetup {
            canonical_repo_name: Arc::from(canonical_repo_name.clone()),
            generator: BzlmodGeneratedCellGenerator::ModuleExtensionRepo(sibling_module_extension),
        };
        sibling_origins.push((cell_name.clone(), canonical_repo_name, sibling_setup));
        cell_aliases.push((cell_name, visible_aliases.clone()));
    }

    Ok(BzlmodModuleExtensionRepoMappingEntries::new(
        Some(extension_unique_name.to_owned()),
        sibling_origins,
        cell_aliases,
    ))
}

fn bzlmod_module_extension_repo_mapping_entries_fingerprint(
    registration_key: &Option<String>,
    sibling_origins: &[(String, String, BzlmodGeneratedCellSetup)],
    cell_aliases: &[(String, Vec<(String, String)>)],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(
        &mut hasher,
        "buck2-bzlmod-module-extension-repo-mapping-entries-v1",
    );
    update_bzlmod_repo_contents_cache_key_opt(&mut hasher, registration_key.as_deref());
    update_bzlmod_repo_contents_cache_key(&mut hasher, &sibling_origins.len().to_string());
    for (cell_name, canonical_repo_name, setup) in sibling_origins {
        update_bzlmod_repo_contents_cache_key(&mut hasher, cell_name);
        update_bzlmod_repo_contents_cache_key(&mut hasher, canonical_repo_name);
        update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.canonical_repo_name);
        match &setup.generator {
            BzlmodGeneratedCellGenerator::ModuleExtensionRepo(module_extension) => {
                update_bzlmod_repo_contents_cache_key(&mut hasher, "module_extension_repo");
                update_bzlmod_repo_contents_cache_key(
                    &mut hasher,
                    &module_extension.parent_canonical_repo_name,
                );
                update_bzlmod_repo_contents_cache_key(
                    &mut hasher,
                    &module_extension.parent_is_root.to_string(),
                );
                update_bzlmod_repo_contents_cache_key(
                    &mut hasher,
                    &module_extension.extension_bzl_file,
                );
                update_bzlmod_repo_contents_cache_key(
                    &mut hasher,
                    &module_extension.extension_bzl_cell,
                );
                update_bzlmod_repo_contents_cache_key(
                    &mut hasher,
                    &module_extension.extension_bzl_path,
                );
                update_bzlmod_repo_contents_cache_key(
                    &mut hasher,
                    &module_extension.extension_unique_name,
                );
                update_bzlmod_repo_contents_cache_key(
                    &mut hasher,
                    &module_extension.extension_name,
                );
                update_bzlmod_repo_contents_cache_key(&mut hasher, &module_extension.repo_name);
                update_bzlmod_repo_contents_cache_key(
                    &mut hasher,
                    &module_extension.extension_usages_key,
                );
            }
            generator => {
                update_bzlmod_repo_contents_cache_key(&mut hasher, &format!("{generator:?}"));
            }
        }
    }
    update_bzlmod_repo_contents_cache_key(&mut hasher, &cell_aliases.len().to_string());
    for (cell_name, aliases) in cell_aliases {
        update_bzlmod_repo_contents_cache_key(&mut hasher, cell_name);
        update_bzlmod_repo_contents_cache_key(&mut hasher, &aliases.len().to_string());
        for (alias, cell_name) in aliases {
            update_bzlmod_repo_contents_cache_key(&mut hasher, alias);
            update_bzlmod_repo_contents_cache_key(&mut hasher, cell_name);
        }
    }
    *hasher.finalize().as_bytes()
}

fn register_bzlmod_module_extension_repo_mapping_entries(
    entries: &BzlmodModuleExtensionRepoMappingEntries,
) -> bz_error::Result<()> {
    if let Some(registration_key) = &entries.registration_key {
        let registrations = BZLMOD_MODULE_EXTENSION_REPO_MAPPING_REGISTRATIONS
            .get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut registrations = registrations
            .lock()
            .expect("bzlmod module extension repo mapping registrations poisoned");
        if matches!(
            registrations.get(registration_key),
            Some(existing) if existing == &entries.fingerprint
        ) {
            return Ok(());
        }
        register_bzlmod_module_extension_repo_mapping_entries_uncached(entries)?;
        registrations.insert(registration_key.clone(), entries.fingerprint);
        return Ok(());
    }

    register_bzlmod_module_extension_repo_mapping_entries_uncached(entries)
}

fn register_bzlmod_module_extension_repo_mapping_entries_uncached(
    entries: &BzlmodModuleExtensionRepoMappingEntries,
) -> bz_error::Result<()> {
    span_simple(
        bz_data::DiceStateUpdateStageStart {
            stage: format!(
                "registering bzlmod repo mappings ({} repos, {} alias maps)",
                entries.sibling_origins.len(),
                entries.cell_aliases.len()
            ),
        },
        || {
            for (cell_name, canonical_repo_name, setup) in &entries.sibling_origins {
                register_bzlmod_cell_canonical_repo_name_for_cell(cell_name, canonical_repo_name);
                register_external_cell_origin(
                    CellName::unchecked_new(cell_name)?,
                    ExternalCellOrigin::BzlmodGenerated(setup.dupe()),
                );
            }
            for (cell_name, aliases) in &entries.cell_aliases {
                register_bzlmod_cell_aliases(cell_name, aliases.iter().cloned());
            }
            Ok(())
        },
        bz_data::DiceStateUpdateStageEnd {},
    )
}

async fn compute_bzlmod_module_extension_repo_mapping_entries(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodGeneratedCellSetup,
    module_extension: &BzlmodModuleExtensionRepoSetup,
    _generated_repo_path: &ProjectRelativePath,
) -> bz_error::Result<Arc<BzlmodModuleExtensionRepoMappingEntries>> {
    let Some(extension_unique_name) = bzlmod_module_extension_unique_name(setup, module_extension)
    else {
        return Ok(Arc::new(BzlmodModuleExtensionRepoMappingEntries::new(
            None,
            Vec::new(),
            Vec::new(),
        )));
    };
    let mut module_extension = module_extension.dupe();
    module_extension.repo_name = Arc::from("");
    let working_dir = bzlmod_module_extension_evaluation_working_dir(&module_extension);
    ctx.compute(&BzlmodModuleExtensionRepoMappingEntriesKey {
        extension_unique_name,
        module_extension,
        working_dir,
    })
    .await?
}

pub(crate) async fn ensure_generated_cell_alias_resolver_ready(
    ctx: &mut DiceComputations<'_>,
    _cell: CellName,
    setup: BzlmodGeneratedCellSetup,
) -> bz_error::Result<()> {
    let BzlmodGeneratedCellGenerator::ModuleExtensionRepo(module_extension) = &setup.generator
    else {
        return Ok(());
    };

    let artifact_fs = ctx.get_artifact_fs().await?;
    let generated_repo_path = artifact_fs
        .buck_out_path_resolver()
        .resolve_external_cell_source(
            CellRelativePath::empty(),
            ExternalCellOrigin::BzlmodGenerated(setup.dupe()),
        );
    let entries = compute_bzlmod_module_extension_repo_mapping_entries(
        ctx,
        &setup,
        module_extension,
        &generated_repo_path,
    )
    .await?;
    register_bzlmod_module_extension_repo_mapping_entries(&entries)
}

async fn materialize_generated_contents(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cancellations: &CancellationContext,
) -> bz_error::Result<BzlmodGeneratedMaterializationResult> {
    match &setup.generator {
        BzlmodGeneratedCellGenerator::ModuleExtensionRepo(module_extension) => {
            let evaluation =
                evaluate_cached_bzlmod_module_extension(ctx, module_extension, path).await?;
            let entries = compute_bzlmod_module_extension_repo_mapping_entries(
                ctx,
                setup,
                module_extension,
                path,
            )
            .await?;
            register_bzlmod_module_extension_repo_mapping_entries(&entries)?;
            if let Some(invocation) = evaluation
                .repository_rule_invocations
                .iter()
                .find(|invocation| invocation.name == module_extension.repo_name.as_ref())
            {
                let mut invocation = invocation.clone();
                invocation.name = setup.canonical_repo_name.to_string();
                let mut recorded_inputs = evaluation.recorded_inputs.clone();
                let repository_rule_result = evaluate_and_materialize_bzlmod_repository_rule(
                    ctx,
                    &setup.canonical_repo_name,
                    path,
                    bzlmod_generated_repo_kind(setup),
                    &invocation,
                    cancellations,
                )
                .await?;
                recorded_inputs.extend(repository_rule_result.recorded_inputs);
                write_bzlmod_generated_recorded_inputs(ctx, path, setup, &recorded_inputs).await?;
                Ok(BzlmodGeneratedMaterializationResult {
                    cacheable: repository_rule_result.reproducible,
                })
            } else {
                let emitted = evaluation
                    .repository_rule_invocations
                    .iter()
                    .map(|invocation| invocation.name.clone())
                    .collect();
                Err(BzlmodError::ModuleExtensionRepoNotEmitted {
                    extension_bzl_file: module_extension.extension_bzl_file.to_string(),
                    extension_name: module_extension.extension_name.to_string(),
                    repo_name: module_extension.repo_name.to_string(),
                    emitted,
                }
                .into())
            }
        }
        BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(invocation_setup) => {
            let invocation = bzlmod_repository_rule_invocation_from_setup(
                invocation_setup,
                &setup.canonical_repo_name,
            )?;
            let repository_rule_result = evaluate_and_materialize_bzlmod_repository_rule(
                ctx,
                &setup.canonical_repo_name,
                path,
                bzlmod_generated_repo_kind(setup),
                &invocation,
                cancellations,
            )
            .await?;
            write_bzlmod_generated_recorded_inputs(
                ctx,
                path,
                setup,
                &repository_rule_result.recorded_inputs,
            )
            .await?;
            Ok(BzlmodGeneratedMaterializationResult {
                cacheable: repository_rule_result.reproducible,
            })
        }
        BzlmodGeneratedCellGenerator::HttpArchive(http_archive) => {
            let archive = bzlmod_generated_scratch_path(setup, "source.archive");
            let temp = bzlmod_generated_scratch_path(setup, "extract-tmp");
            ctx.get_blocking_executor()
                .execute_io(
                    Box::new(bz_execute::execute::clean_output_paths::CleanOutputPaths {
                        paths: vec![path.to_owned(), archive.clone(), temp.clone()],
                    }),
                    cancellations,
                )
                .await?;
            let io_provider = ctx.global_data().get_io_provider();
            let project_root = io_provider.project_root();
            let digest_config = ctx.global_data().get_digest_config();
            let remote_downloader = bzlmod_remote_downloader_config(ctx);
            let client = shared_bzlmod_download_http_client().await?;
            let bazel_download_headers = bazel_repository_download_headers(std::iter::empty());
            let archive_checksum = Checksum::new(None, Some(&*http_archive.sha256))?;
            bzlmod_download_with_headers(
                remote_downloader.as_ref(),
                &client,
                project_root,
                digest_config.dupe(),
                &archive,
                &http_archive.url,
                &archive_checksum,
                false,
                &bazel_download_headers,
                None,
                "http_archive",
            )
            .await?;
            ctx.get_blocking_executor()
                .execute_io(
                    Box::new(BzlmodGeneratedHttpArchiveIoRequest {
                        setup: http_archive.dupe(),
                        archive,
                        temp,
                        dest: path.to_owned(),
                    }),
                    cancellations,
                )
                .await?;
            Ok(BzlmodGeneratedMaterializationResult { cacheable: false })
        }
        _ => {
            ctx.get_blocking_executor()
                .execute_io(
                    Box::new(bz_execute::execute::clean_output_paths::CleanOutputPaths {
                        paths: vec![path.to_owned()],
                    }),
                    cancellations,
                )
                .await?;
            ctx.get_blocking_executor()
                .execute_io(
                    Box::new(BzlmodGeneratedIoRequest {
                        setup: setup.dupe(),
                        dest: path.to_owned(),
                    }),
                    cancellations,
                )
                .await?;
            Ok(BzlmodGeneratedMaterializationResult { cacheable: false })
        }
    }
}

#[derive(
    Clone,
    Debug,
    derive_more::Display,
    PartialEq,
    Eq,
    Hash,
    allocative::Allocative,
    Pagable
)]
#[display("REPOSITORY_DIRECTORY({}, {})", path, setup)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodGeneratedCellMaterializationKey {
    path: ProjectRelativePathBuf,
    setup: BzlmodGeneratedCellSetup,
}

#[async_trait::async_trait]
impl Key for BzlmodGeneratedCellMaterializationKey {
    type Value = bz_error::Result<Arc<BzlmodGeneratedCellMaterializationValue>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        cancellations: &CancellationContext,
    ) -> Self::Value {
        materialize_generated(ctx, &self.path, &self.setup, cancellations).await
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x.fingerprint == y.fingerprint,
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

async fn ensure_generated_materialized(
    ctx: &mut DiceComputations<'_>,
    path: ProjectRelativePathBuf,
    setup: BzlmodGeneratedCellSetup,
) -> bz_error::Result<()> {
    let setup = bzlmod_generated_effective_setup(ctx, setup).await?;
    ctx.compute(&BzlmodGeneratedCellMaterializationKey { path, setup })
        .await??;
    Ok(())
}

async fn bzlmod_generated_effective_setup(
    ctx: &mut DiceComputations<'_>,
    setup: BzlmodGeneratedCellSetup,
) -> bz_error::Result<BzlmodGeneratedCellSetup> {
    let BzlmodGeneratedCellSetup {
        canonical_repo_name,
        generator,
    } = setup;
    match generator {
        BzlmodGeneratedCellGenerator::HostPlatform(mut host_platform) => {
            if host_platform.cpu_constraint.is_none() {
                let arch = bzlmod_bazel_current_arch_name(ctx).await?;
                host_platform.cpu_constraint =
                    translate_host_platform_cpu_constraint(&arch).map(Arc::<str>::from);
            }
            if host_platform.os_constraint.is_none() {
                let os = bzlmod_bazel_current_os_name(ctx).await?;
                host_platform.os_constraint =
                    translate_host_platform_os_constraint(&os).map(Arc::<str>::from);
            }
            Ok(BzlmodGeneratedCellSetup {
                canonical_repo_name,
                generator: BzlmodGeneratedCellGenerator::HostPlatform(host_platform),
            })
        }
        generator => Ok(BzlmodGeneratedCellSetup {
            canonical_repo_name,
            generator,
        }),
    }
}

#[derive(allocative::Allocative, Pagable)]
pub(crate) struct BzlmodFileOpsDelegate {
    buck_out_resolver: BuckOutPathResolver,
    cell: CellName,
    setup: BzlmodCellSetup,
    backing_base_path: ProjectRelativePathBuf,
    io: FsIoProvider,
}

impl BzlmodFileOpsDelegate {
    fn resolve(&self, path: &CellRelativePath) -> ProjectRelativePathBuf {
        self.buck_out_resolver
            .resolve_external_cell_source(path, ExternalCellOrigin::Bzlmod(self.setup.dupe()))
    }

    fn get_base_path(&self) -> ProjectRelativePathBuf {
        self.resolve(CellRelativePath::empty())
    }

    fn resolve_backing(&self, path: &CellRelativePath) -> ProjectRelativePathBuf {
        self.backing_base_path.join(path.as_forward_relative_path())
    }
}

#[pagable_typetag]
#[async_trait::async_trait]
impl FileOpsDelegate for BzlmodFileOpsDelegate {
    async fn read_file_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<ReadFileProxy> {
        Ok(ReadFileProxy::new_with_captures(
            (self.resolve_backing(path), self.io.dupe()),
            |(project_path, io)| async move {
                (&io as &dyn IoProvider)
                    .read_file_if_exists(project_path)
                    .await
            },
        ))
    }

    async fn read_dir(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir_without_dice(path).await
    }

    async fn read_dir_for_no_watchfs_without_dice(
        &self,
        _io_provider: Arc<dyn IoProvider>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir_without_dice(path).await
    }

    async fn read_path_metadata_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadata>> {
        let project_path = self.resolve_backing(path);
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists(project_path.clone())
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        let materialized_path = self.resolve(path);
        materialize_observed_bzlmod_source_path(
            ctx,
            project_path.clone(),
            materialized_path.clone(),
            &metadata,
        )
        .await?;
        declare_observed_source_artifact(ctx, materialized_path, &metadata).await?;
        Ok(Some(metadata.try_map(|project_path| {
            match project_path
                .strip_prefix_opt(self.backing_base_path.as_ref() as &ProjectRelativePath)
            {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    project_path,
                    self.cell
                )),
            }
        })?))
    }

    async fn read_path_metadata_if_exists_for_no_watchfs(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadata>> {
        let project_path = self.resolve_backing(path);
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists(project_path)
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        Ok(Some(metadata.try_map(|project_path| {
            match project_path
                .strip_prefix_opt(self.backing_base_path.as_ref() as &ProjectRelativePath)
            {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    project_path,
                    self.cell
                )),
            }
        })?))
    }

    async fn read_path_metadata_for_no_watchfs_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(None, path)
            .await
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_with_cache(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(cache, path)
            .await
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_without_dice(
        &self,
        _io_provider: Arc<dyn IoProvider>,
        path: &'async_trait CellRelativePath,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(cache, path)
            .await
    }

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
    }
}

impl BzlmodFileOpsDelegate {
    async fn read_dir_without_dice(
        &self,
        path: &CellRelativePath,
    ) -> bz_error::Result<Arc<[RawDirEntry]>> {
        let project_path = self.resolve_backing(path);
        let mut entries = (&self.io as &dyn IoProvider)
            .read_dir(project_path)
            .await
            .with_buck_error_context(|| format!("Error listing dir `{path}`"))?;
        follow_bzlmod_symlinked_directory_entries(
            self.io.project_root(),
            self.resolve_backing(path).as_ref(),
            &mut entries,
        )?;

        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(entries.into())
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(
        &self,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
        path: &CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        let project_path = self.resolve_backing(path);
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists_for_no_watchfs_with_cache(project_path, cache)
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        Ok(Some(metadata.try_map(|project_path| {
            match project_path
                .strip_prefix_opt(self.backing_base_path.as_ref() as &ProjectRelativePath)
            {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    project_path,
                    self.cell
                )),
            }
        })?))
    }
}

#[derive(allocative::Allocative, Pagable)]
pub(crate) struct BzlmodGeneratedFileOpsDelegate {
    buck_out_resolver: BuckOutPathResolver,
    cell: CellName,
    setup: BzlmodGeneratedCellSetup,
    io: FsIoProvider,
}

impl BzlmodGeneratedFileOpsDelegate {
    fn resolve(&self, path: &CellRelativePath) -> ProjectRelativePathBuf {
        self.buck_out_resolver.resolve_external_cell_source(
            path,
            ExternalCellOrigin::BzlmodGenerated(self.setup.dupe()),
        )
    }

    fn get_base_path(&self) -> ProjectRelativePathBuf {
        self.resolve(CellRelativePath::empty())
    }

    fn get_backing_base_path(&self) -> bz_error::Result<ProjectRelativePathBuf> {
        let base_path = self.get_base_path();
        let base_abs = self.io.project_root().resolve(&base_path);
        let Some(metadata) = fs_util::symlink_metadata_if_exists(&base_abs)? else {
            return Ok(base_path);
        };
        if !metadata.file_type().is_symlink() {
            return Ok(base_path);
        }

        let target = fs_util::read_link(&base_abs).categorize_internal()?;
        let target = if target.has_root() {
            target
        } else if let Some(parent) = base_abs.as_abs_path().parent() {
            parent.join(&target).into_path_buf()
        } else {
            target
        };
        Ok(self
            .io
            .project_root()
            .relativize_any(AbsPath::new(&target)?)?)
    }
}

fn follow_bzlmod_symlinked_directory_entries(
    project_root: &ProjectRoot,
    project_path: &ProjectRelativePath,
    entries: &mut [RawDirEntry],
) -> bz_error::Result<()> {
    for entry in entries {
        if !entry.file_type.is_symlink() {
            continue;
        }

        let child_path = project_path.join(ForwardRelativePath::new(entry.file_name.as_str())?);
        match fs_util::metadata(project_root.resolve(&child_path)) {
            Ok(metadata) if metadata.is_dir() => {
                entry.file_type = FileType::Directory;
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.io_error_kind(),
                    Some(ErrorKind::NotFound | ErrorKind::NotADirectory)
                ) => {}
            Err(error) => return Err(error.categorize_internal()),
        }
    }

    Ok(())
}

async fn wait_for_bzlmod_generated_materialization_if_in_progress(
    project_root: &ProjectRoot,
    setup: &BzlmodGeneratedCellSetup,
    base_path: &ProjectRelativePath,
) -> bz_error::Result<bool> {
    let stamp_path = bzlmod_generated_materialization_stamp_path(setup, base_path);
    if fs_util::symlink_metadata_if_exists(project_root.resolve(&stamp_path))?.is_some() {
        return Ok(false);
    }
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if fs_util::symlink_metadata_if_exists(project_root.resolve(&stamp_path))?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[pagable_typetag]
#[async_trait::async_trait]
impl FileOpsDelegate for BzlmodGeneratedFileOpsDelegate {
    async fn read_file_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<ReadFileProxy> {
        ensure_generated_materialized(ctx, self.get_base_path(), self.setup.dupe()).await?;
        Ok(ReadFileProxy::new_with_captures(
            (
                self.resolve(path),
                self.io.dupe(),
                self.setup.dupe(),
                self.get_base_path(),
            ),
            |(project_path, io, setup, base_path)| async move {
                loop {
                    let contents = (&io as &dyn IoProvider)
                        .read_file_if_exists(project_path.clone())
                        .await?;
                    if contents.is_some()
                        || !wait_for_bzlmod_generated_materialization_if_in_progress(
                            io.project_root(),
                            &setup,
                            &base_path,
                        )
                        .await?
                    {
                        return Ok(contents);
                    }
                }
            },
        ))
    }

    async fn read_dir(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Arc<[RawDirEntry]>> {
        ensure_generated_materialized(ctx, self.get_base_path(), self.setup.dupe()).await?;
        let project_path = self.resolve(path);
        let mut entries = (&self.io as &dyn IoProvider)
            .read_dir(project_path)
            .await
            .with_buck_error_context(|| format!("Error listing dir `{path}`"))?;
        follow_bzlmod_symlinked_directory_entries(
            self.io.project_root(),
            self.resolve(path).as_ref(),
            &mut entries,
        )?;

        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(entries.into())
    }

    async fn read_dir_for_no_watchfs(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Arc<[RawDirEntry]>> {
        ensure_generated_materialized(ctx, self.get_base_path(), self.setup.dupe()).await?;
        self.read_dir_for_no_watchfs_without_dice_impl(path).await
    }

    async fn read_dir_for_no_watchfs_without_dice(
        &self,
        _io_provider: Arc<dyn IoProvider>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir_for_no_watchfs_without_dice_impl(path).await
    }

    async fn read_path_metadata_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadata>> {
        ensure_generated_materialized(ctx, self.get_base_path(), self.setup.dupe()).await?;
        let backing_base_path = self.get_backing_base_path()?;
        let project_path = backing_base_path.join(path.as_forward_relative_path());
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists(project_path.clone())
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        declare_observed_source_artifact(ctx, self.resolve(path), &metadata).await?;
        Ok(Some(metadata.try_map(|path| {
            match path.strip_prefix_opt(backing_base_path.as_ref() as &ProjectRelativePath) {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    path,
                    self.cell
                )),
            }
        })?))
    }

    async fn read_path_metadata_if_exists_for_no_watchfs(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadata>> {
        ensure_generated_materialized(ctx, self.get_base_path(), self.setup.dupe()).await?;
        let backing_base_path = self.get_backing_base_path()?;
        let project_path = backing_base_path.join(path.as_forward_relative_path());
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists(project_path)
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        Ok(Some(metadata.try_map(|path| {
            match path.strip_prefix_opt(backing_base_path.as_ref() as &ProjectRelativePath) {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    path,
                    self.cell
                )),
            }
        })?))
    }

    async fn read_path_metadata_for_no_watchfs_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        ensure_generated_materialized(ctx, self.get_base_path(), self.setup.dupe()).await?;
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(None, path)
            .await
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_with_cache(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
        _cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        ensure_generated_materialized(ctx, self.get_base_path(), self.setup.dupe()).await?;
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(None, path)
            .await
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_without_dice(
        &self,
        _io_provider: Arc<dyn IoProvider>,
        path: &'async_trait CellRelativePath,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(cache, path)
            .await
    }

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
    }
}

impl BzlmodGeneratedFileOpsDelegate {
    async fn wait_for_materialization_if_in_progress(&self) -> bz_error::Result<bool> {
        wait_for_bzlmod_generated_materialization_if_in_progress(
            self.io.project_root(),
            &self.setup,
            &self.get_base_path(),
        )
        .await
    }

    async fn read_dir_for_no_watchfs_without_dice_impl(
        &self,
        path: &CellRelativePath,
    ) -> bz_error::Result<Arc<[RawDirEntry]>> {
        let project_path = self.resolve(path);
        let mut entries = loop {
            match (&self.io as &dyn IoProvider)
                .read_dir(project_path.clone())
                .await
            {
                Ok(entries) => break entries,
                Err(error) if self.wait_for_materialization_if_in_progress().await? => {
                    drop(error);
                    continue;
                }
                Err(error) => {
                    return Err(error)
                        .with_buck_error_context(|| format!("Error listing dir `{path}`"));
                }
            }
        };
        follow_bzlmod_symlinked_directory_entries(
            self.io.project_root(),
            self.resolve(path).as_ref(),
            &mut entries,
        )?;

        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(entries.into())
    }
    async fn read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(
        &self,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
        path: &CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        let (backing_base_path, metadata) = loop {
            let backing_base_path = self.get_backing_base_path()?;
            let project_path = backing_base_path.join(path.as_forward_relative_path());
            let metadata = (&self.io as &dyn IoProvider)
                .read_path_metadata_if_exists_for_no_watchfs_with_cache(
                    project_path.clone(),
                    cache.dupe(),
                )
                .await
                .with_buck_error_context(|| {
                    format!("Error accessing metadata for path `{path}`")
                })?;
            if metadata.is_some() || !self.wait_for_materialization_if_in_progress().await? {
                break (backing_base_path, metadata);
            }
        };
        let Some(metadata) = metadata else {
            return Ok(None);
        };
        Ok(Some(metadata.try_map(|path| {
            match path.strip_prefix_opt(backing_base_path.as_ref() as &ProjectRelativePath) {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    path,
                    self.cell
                )),
            }
        })?))
    }
}

pub(crate) async fn get_file_ops_delegate(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodCellSetup,
) -> bz_error::Result<Arc<BzlmodFileOpsDelegate>> {
    #[derive(
        dupe::Dupe,
        Clone,
        Debug,
        derive_more::Display,
        PartialEq,
        Eq,
        Hash,
        allocative::Allocative,
        Pagable
    )]
    #[display("REPOSITORY_DIRECTORY({}, {})", _0, _1)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct BzlmodFileOpsDelegateKey(CellName, BzlmodCellSetup);

    #[async_trait::async_trait]
    impl Key for BzlmodFileOpsDelegateKey {
        type Value = bz_error::Result<Arc<BzlmodFileOpsDelegate>>;

        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            cancellations: &CancellationContext,
        ) -> Self::Value {
            let artifact_fs = ctx.get_artifact_fs().await?;
            let backing_base_path = match &self.1.local_path {
                Some(local_path) => ProjectRelativePath::new(local_path.as_ref())?.to_buf(),
                None => bzlmod_repo_contents_cache_path(
                    &bzlmod_repo_contents_cache_key(&self.1),
                    "repo",
                ),
            };
            let ops = BzlmodFileOpsDelegate {
                buck_out_resolver: artifact_fs.buck_out_path_resolver().clone(),
                cell: self.0,
                setup: self.1.dupe(),
                backing_base_path,
                io: FsIoProvider::new(
                    artifact_fs.fs().dupe(),
                    ctx.global_data().get_digest_config().cas_digest_config(),
                ),
            };
            if self.1.local_path.is_none() {
                download_and_materialize(ctx, &ops.get_base_path(), &self.1, cancellations).await?;
            } else {
                prepare_bzlmod_external_cell_root_from_source(
                    ctx,
                    &ops.backing_base_path,
                    &ops.get_base_path(),
                    cancellations,
                )
                .await?;
            }
            Ok(Arc::new(ops))
        }

        fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
            false
        }

        fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
            OkPagableValueSerialize::<Self::Value>::new()
        }
    }

    ctx.compute(&BzlmodFileOpsDelegateKey(cell, setup)).await?
}

pub(crate) async fn prepare_cached_cell_root(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodCellSetup,
    _cancellations: &CancellationContext,
) -> bz_error::Result<()> {
    if setup.local_path.is_some() {
        return Ok(());
    }

    let cache_key = bzlmod_repo_contents_cache_key(&setup);
    let cache_repo = bzlmod_repo_contents_cache_path(&cache_key, "repo");
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let cache_repo_for_check = cache_repo.clone();
    if !run_bzlmod_cache_io(move || {
        bzlmod_repo_contents_cache_exists(&project_root, &cache_repo_for_check)
    })
    .await?
    {
        return Ok(());
    }

    get_file_ops_delegate(ctx, cell, setup).await?;
    Ok(())
}

pub(crate) async fn get_generated_file_ops_delegate(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodGeneratedCellSetup,
) -> bz_error::Result<Arc<BzlmodGeneratedFileOpsDelegate>> {
    #[derive(
        dupe::Dupe,
        Clone,
        Debug,
        derive_more::Display,
        PartialEq,
        Eq,
        Hash,
        allocative::Allocative,
        Pagable
    )]
    #[display("REPOSITORY_DIRECTORY({}, {})", _0, _1)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct BzlmodGeneratedFileOpsDelegateKey(CellName, BzlmodGeneratedCellSetup);

    #[async_trait::async_trait]
    impl Key for BzlmodGeneratedFileOpsDelegateKey {
        type Value = bz_error::Result<Arc<BzlmodGeneratedFileOpsDelegate>>;

        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            _cancellations: &CancellationContext,
        ) -> Self::Value {
            let artifact_fs = ctx.get_artifact_fs().await?;
            let ops = BzlmodGeneratedFileOpsDelegate {
                buck_out_resolver: artifact_fs.buck_out_path_resolver().clone(),
                cell: self.0,
                setup: self.1.dupe(),
                io: FsIoProvider::new(
                    artifact_fs.fs().dupe(),
                    ctx.global_data().get_digest_config().cas_digest_config(),
                ),
            };
            Ok(Arc::new(ops))
        }

        fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
            false
        }

        fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
            OkPagableValueSerialize::<Self::Value>::new()
        }
    }

    ctx.compute(&BzlmodGeneratedFileOpsDelegateKey(cell, setup))
        .await?
}

pub(crate) async fn prepare_cached_generated_cell_root(
    _ctx: &mut DiceComputations<'_>,
    _cell: CellName,
    _setup: BzlmodGeneratedCellSetup,
    _cancellations: &CancellationContext,
) -> bz_error::Result<()> {
    Ok(())
}

pub(crate) async fn materialize_all(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodCellSetup,
) -> bz_error::Result<ProjectRelativePathBuf> {
    let ops = get_file_ops_delegate(ctx, cell, setup.dupe()).await?;
    let materializer = ctx.per_transaction_data().get_materializer();
    declare_existing_directory(ctx, &ops.backing_base_path, &*materializer).await?;
    Ok(ops.backing_base_path.clone())
}

pub(crate) async fn materialize_generated_all(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodGeneratedCellSetup,
) -> bz_error::Result<ProjectRelativePathBuf> {
    let ops = get_generated_file_ops_delegate(ctx, cell, setup.dupe()).await?;
    ensure_generated_materialized(ctx, ops.get_base_path(), setup).await?;
    let materializer = ctx.per_transaction_data().get_materializer();
    declare_existing_directory(ctx, &ops.get_base_path(), &*materializer).await?;
    Ok(ops.get_base_path())
}
