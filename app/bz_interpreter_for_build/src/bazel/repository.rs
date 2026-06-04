use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use allocative::Allocative;
use async_compression::tokio::bufread::GzipDecoder;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bz_build_api::actions::execute::dice_data::HasFallbackExecutorConfig;
use bz_common::bazel::bzlmod::BZLMOD_REPOSITORY_OS_ARCH_ENV;
use bz_common::bazel::bzlmod::BZLMOD_REPOSITORY_OS_NAME_ENV;
use bz_common::bazel::bzlmod::GetBzlmodRepositoryEnvironment;
use bz_common::bazel::bzlmod::archive::archive_kind_from_type_or_url;
use bz_common::bazel::bzlmod::archive::extract_archive;
use bz_common::bazel::bzlmod::patch::apply_unified_patch_file;
use bz_common::dice::cells::HasCellResolver;
use bz_common::file_ops::dice::DiceFileComputations;
use bz_common::file_ops::error::FileReadErrorContext;
use bz_common::file_ops::metadata::FileDigestConfig;
use bz_common::file_ops::metadata::RawPathMetadata;
use bz_common::init::BUILDBUDDY_API_KEY_HEADER;
use bz_common::init::RemoteExecutionStartupConfig;
use bz_common::liveliness_observer::NoopLivelinessObserver;
use bz_common::package_listing::listing::PackageListing;
use bz_core::bzl::ImportPath;
use bz_core::cells::CellAliasResolver;
use bz_core::cells::alias::NonEmptyCellAlias;
use bz_core::cells::build_file_cell::BuildFileCell;
use bz_core::cells::cell_path::CellPath;
use bz_core::cells::cell_path_with_allowed_relative_dir::CellPathWithAllowedRelativeDir;
use bz_core::cells::external::BAZEL_REPOSITORY_ACCEPT_ENCODING;
use bz_core::cells::external::BAZEL_REPOSITORY_ACCEPT_ENCODING_HEADER;
use bz_core::cells::external::BAZEL_REPOSITORY_USER_AGENT_HEADER;
use bz_core::cells::external::BzlmodModuleExtensionRepoSetup;
use bz_core::cells::external::BzlmodRepositoryRuleInvocationSetup;
use bz_core::cells::external::bazel_repository_user_agent;
use bz_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use bz_core::cells::external::bzlmod_cell_aliases_for_cell;
use bz_core::cells::external::bzlmod_cell_name;
use bz_core::cells::name::CellName;
use bz_core::cells::paths::CellRelativePath;
use bz_core::cells::paths::CellRelativePathBuf;
use bz_core::execution_types::executor_config::CommandExecutorConfig;
use bz_core::execution_types::executor_config::Executor;
use bz_core::execution_types::executor_config::RemoteEnabledExecutor;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::buck_out_path::BuckOutTestPath;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_core::package::PackageLabel;
use bz_core::target::label::interner::ConcurrentTargetLabelInterner;
use bz_error::BuckErrorContext;
use bz_execute::digest_config::DigestConfig;
use bz_execute::directory::ActionDirectoryBuilder;
use bz_execute::directory::ActionDirectoryEntry;
use bz_execute::directory::ActionSharedDirectory;
use bz_execute::directory::INTERNER;
use bz_execute::entry::build_entry_from_disk;
use bz_execute::execute::blocking::BlockingExecutor;
use bz_execute::execute::claim::MutexClaimManager;
use bz_execute::execute::command_executor::CommandExecutor;
use bz_execute::execute::manager::CommandExecutionManager;
use bz_execute::execute::prepared::PreparedCommand;
use bz_execute::execute::request::CommandExecutionInput;
use bz_execute::execute::request::CommandExecutionOutput;
use bz_execute::execute::request::CommandExecutionPaths;
use bz_execute::execute::request::ExecutorPreference;
use bz_execute::execute::request::OutputCreationBehavior;
use bz_execute::execute::result::CommandExecutionStatus;
use bz_execute::execute::target::CommandExecutionTarget;
use bz_execute::materialize::materializer::Materializer;
use bz_execute_local::CommandEvent;
use bz_execute_local::DefaultKillProcess;
use bz_execute_local::GatherOutputStatus;
use bz_execute_local::spawn_command_and_stream_events;
use bz_execute_local::status_decoder::DefaultStatusDecoder;
use bz_fs::paths::abs_norm_path::AbsNormPathBuf;
use bz_fs::paths::file_name::FileNameBuf;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_hash::BuckIndexSet;
use bz_hash::StdBuckHashMap;
use bz_interpreter::file_loader::LoadedModule;
use bz_interpreter::load_module::InterpreterCalculation;
use bz_interpreter::paths::module::StarlarkModulePath;
use bz_interpreter::paths::path::OwnedStarlarkPath;
use bz_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use bz_node::attrs::attr::Attribute;
use bz_node::attrs::attr::CoercedValue;
use bz_node::attrs::coerced_attr::CoercedAttr;
use bz_node::attrs::configurable::AttrIsConfigurable;
use bz_node::attrs::fmt_context::AttrFmtContext;
use bz_node::bzl_or_bxl_path::BzlOrBxlPath;
use bz_node::rule_type::StarlarkRuleType;
use bz_resource_control::ActionFreezeEvent;
use derive_more::Display;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::Key;
use dice::NoValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use futures::TryStreamExt;
use itertools::Itertools;
use pagable::Pagable;
use pagable::pagable_typetag;
use prost::Message;
use re_grpc_proto::build::bazel::remote::execution::v2::Digest as RemoteExecutionDigest;
use re_grpc_proto::google::bytestream::ReadRequest;
use re_grpc_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use re_grpc_proto::google::rpc::Status as RemoteAssetStatus;
use serde::Deserialize;
use serde::Serialize;
use sha1::Sha1;
use sha2::Digest;
use sha2::Sha256;
use sha2::Sha384;
use sha2::Sha512;
use starlark::any::ProvidesStaticType;
use starlark::docs::DocFunction;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::DocStringKind;
use starlark::environment::Globals;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::eval::ParametersSpec;
use starlark::eval::ParametersSpecParam;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::syntax::AstModule;
use starlark::syntax::Dialect;
use starlark::typing::ParamSpec;
use starlark::typing::Ty;
use starlark::values::AllocValue;
use starlark::values::Freeze;
use starlark::values::FreezeResult;
use starlark::values::Freezer;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::ValueTypedComplex;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneOr;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::TupleRef;
use starlark::values::typing::StarlarkCallable;
use starlark_map::small_map::SmallMap;
use tar::Archive;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio_util::io::StreamReader;
use tonic::metadata::MetadataValue;
use tonic::transport::ClientTlsConfig;
use tonic::transport::Endpoint;

use crate::attrs::AttributeCoerceExt;
use crate::attrs::coerce::ctx::BuildAttrCoercionContext;
use crate::attrs::starlark_attribute::StarlarkAttribute;
use crate::interpreter::build_context::BazelModuleExtensionEvaluationResult;
use crate::interpreter::build_context::BazelRepositoryRecordedInput;
use crate::interpreter::build_context::BazelRepositoryRuleInvocation;
use crate::interpreter::build_context::BuildContext;
use crate::interpreter::build_context::PerFileTypeContext;
use crate::interpreter::dice_calculation_delegate::HasCalculationDelegate;
use crate::rule::NAME_ATTRIBUTE_FIELD;

mod bzlmod_usages;
mod command_executor;
mod download;
mod recorded_inputs;
mod repository_path;
mod starlark_types;

pub use self::bzlmod_usages::bzlmod_module_extension_bazel_usages_digest;
pub(crate) use self::bzlmod_usages::bzlmod_module_extension_bazel_usages_digest_in_eval;
pub(crate) use self::command_executor::BazelRemoteRepositoryCommandExecutor;
pub(crate) use self::command_executor::BazelRepositoryCommandExecutor;
pub(crate) use self::command_executor::BazelRepositoryRemoteDownloaderConfig;
use self::command_executor::RepositoryCommandOutput;
pub(crate) use self::command_executor::bazel_repository_remote_downloader_config;
use self::download::ModuleCtxDownloadResult;
use self::download::module_ctx_download_auth_headers_from_entries;
use self::download::module_ctx_download_error_with_block;
use self::download::module_ctx_download_headers_from_entries;
use self::download::module_ctx_download_to_path_blocking;
use self::download::module_ctx_expected_checksum;
use self::download::module_ctx_pending_download;
use self::download::module_ctx_urls_from_value;
use self::download::repository_ctx_download_error_result;
use self::download::repository_ctx_download_to_path;
use self::download::repository_ctx_extract_archive;
use self::download::repository_ctx_rename_files_from_entries;
use self::download::repository_ctx_renamed_strip_prefix;
pub use self::recorded_inputs::RepositoryPathLabelDep;
use self::recorded_inputs::record_repository_dir_tree_input;
use self::recorded_inputs::record_repository_dirents_input;
use self::recorded_inputs::record_repository_env_var;
use self::recorded_inputs::record_repository_file_input;
use self::recorded_inputs::repository_should_record_watch;
use self::repository_path::BazelRepositoryPathRemoteContext;
pub(crate) use self::repository_path::StarlarkRepositoryPath;
use self::repository_path::repository_external_cell_existing_path_relative_to;
use self::repository_path::repository_external_cell_path_relative_to;
use self::repository_path::repository_external_cell_suffix;
use self::repository_path::repository_path_and_dep_from_value_relative_to;
use self::repository_path::repository_path_for_read;
use self::repository_path::repository_path_for_read_abs_relative_to;
use self::repository_path::repository_path_for_write;
use self::repository_path::repository_path_from_value_relative_to;
use self::repository_path::repository_remote_shell;
use self::starlark_types::BazelRepositoryAttrValues;
pub(crate) use self::starlark_types::FrozenStarlarkModuleExtension;
use self::starlark_types::FrozenStarlarkRepositoryOs;
pub(crate) use self::starlark_types::FrozenStarlarkRepositoryRule;
use self::starlark_types::FrozenStarlarkTagClass;
use self::starlark_types::StarlarkBazelModule;
use self::starlark_types::StarlarkBazelModuleTag;
use self::starlark_types::StarlarkBazelModuleTags;
pub(crate) use self::starlark_types::StarlarkModuleExtension;
pub(crate) use self::starlark_types::StarlarkModuleExtensionContext;
pub(crate) use self::starlark_types::StarlarkModuleExtensionMetadata;
pub(crate) use self::starlark_types::StarlarkRepositoryMetadata;
use self::starlark_types::StarlarkRepositoryOs;
pub(crate) use self::starlark_types::StarlarkRepositoryRule;
pub(crate) use self::starlark_types::StarlarkTagClass;
use self::starlark_types::bazel_module_tag_dev_dependency;
use self::starlark_types::bazel_module_tag_sort_key;
use self::starlark_types::repository_ctx_workspace_root;

#[cfg(test)]
use self::command_executor::remote_path_relative_to_working_dir;
#[cfg(test)]
use self::command_executor::repository_ctx_embedded_project_paths;
#[cfg(test)]
use self::command_executor::repository_ctx_rewrite_embedded_project_paths;
#[cfg(test)]
use self::download::MODULE_CTX_DOWNLOAD_CACHE_LOCKS;
#[cfg(test)]
use self::download::ModuleCtxChecksum;
#[cfg(test)]
use self::download::ModuleCtxChecksumKind;
#[cfg(test)]
use self::download::module_ctx_checksum_from_integrity;
#[cfg(test)]
use self::download::module_ctx_checksum_hex;
#[cfg(test)]
use self::download::module_ctx_copy_download_file;
#[cfg(test)]
use self::download::module_ctx_download_cache_import_entry;
#[cfg(test)]
use self::download::module_ctx_download_cache_lock;
#[cfg(test)]
use self::download::module_ctx_download_cache_release_lock;
#[cfg(test)]
use self::download::module_ctx_download_request_headers_for_url;
#[cfg(test)]
use self::download::module_ctx_integrity_from_checksum;
#[cfg(test)]
use self::download::module_ctx_repository_cache_id_path;
#[cfg(test)]
use self::recorded_inputs::repository_recorded_dir_tree_value;
#[cfg(test)]
use self::recorded_inputs::repository_recorded_dirents_value;
#[cfg(test)]
use self::recorded_inputs::repository_recorded_file_value;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
pub(crate) enum BazelRepositoryError {
    #[error("`{0}` is not a valid repository rule attribute name")]
    InvalidRepositoryRuleAttributeName(String),
    #[error("`repository_rule` requires an implementation function")]
    MissingRepositoryRuleImplementation,
    #[error("`{0}` can only be declared in bzl files")]
    NotInBzl(&'static str),
    #[error(
        "repository rules can only be called from within module extension implementation functions"
    )]
    RepositoryRuleCalledOutsideModuleExtension,
    #[error("repository rule calls require a `name` argument")]
    RepositoryRuleMissingName,
    #[error("repository rule `name` argument must be a string, got `{0}`")]
    RepositoryRuleNameMustBeString(String),
    #[error("attempting to instantiate a non-exported repository rule")]
    RepositoryRuleNotExported,
    #[error(
        "repository_rule `{0}` was defined in a BXL file; bzlmod repository execution only supports .bzl repository rules"
    )]
    RepositoryRuleBxlUnsupported(String),
    #[error("repository_rule `{rule}` was not found in `{path}`")]
    RepositoryRuleSymbolMissing { path: String, rule: String },
    #[error("`{path}` export `{rule}` must be a repository_rule, got `{got}`")]
    RepositoryRuleSymbolWrongType {
        path: String,
        rule: String,
        got: String,
    },
    #[error("repository_rule `{rule}` has no attribute `{attr}`")]
    RepositoryRuleUnknownAttribute { rule: String, attr: String },
    #[error("repository_ctx output path expected string or path, got `{0}`")]
    RepositoryCtxOutputPathUnsupportedValue(String),
    #[error("repository_ctx output path `{path}` is outside repository directory `{working_dir}`")]
    RepositoryCtxOutputPathOutsideRepository { path: String, working_dir: String },
    #[error("repository_ctx.template could not read `{path}`: {error}")]
    RepositoryCtxTemplateReadFile { path: String, error: String },
    #[error("repository_ctx could not write `{path}`: {error}")]
    RepositoryCtxWriteFile { path: String, error: String },
    #[error("repository_ctx could not delete `{path}`: {error}")]
    RepositoryCtxDeletePath { path: String, error: String },
    #[error("repository_ctx.patch could not apply `{patch}`: {error}")]
    RepositoryCtxPatch { patch: String, error: String },
    #[error("repository_ctx could not symlink `{link}` to `{target}`: {error}")]
    RepositoryCtxSymlink {
        target: String,
        link: String,
        error: String,
    },
    #[error("repository_ctx.download_and_extract could not extract `{archive}`: {error}")]
    RepositoryCtxExtractArchive { archive: String, error: String },
    #[error(
        "{function}(block = False) is not supported because downloads are currently executed synchronously"
    )]
    RepositoryCtxNonblockingDownloadUnsupported { function: &'static str },
    #[error("repository_ctx.download_and_extract rename_files key must be a string, got `{0}`")]
    RepositoryCtxRenameFilesKeyUnsupportedValue(String),
    #[error(
        "repository_ctx.download_and_extract rename_files value for `{path}` must be a string, got `{got}`"
    )]
    RepositoryCtxRenameFilesValueUnsupportedValue { path: String, got: String },
    #[error("Program argument of repository_ctx.which may not contain a / or a \\ (`{0}` given)")]
    RepositoryCtxWhichInvalidProgram(String),
    #[error("Program argument of repository_ctx.which may not be empty")]
    RepositoryCtxWhichEmptyProgram,
    #[error("repository_ctx.which failed to look up `{program}`: {error}")]
    RepositoryCtxWhichFailed { program: String, error: String },
    #[error("repository_ctx.execute requires at least one argument")]
    RepositoryCtxExecuteEmptyArguments,
    #[error("repository_ctx.execute failed to run `{program}`: {error}")]
    RepositoryCtxExecuteFailed { program: String, error: String },
    #[error("repository_path.get_child expected string arguments, got `{0}`")]
    RepositoryPathGetChildNonString(String),
    #[error("repository_path.readdir could not read `{path}`: {error}")]
    RepositoryPathReaddir { path: String, error: String },
    #[error("repository_path.realpath could not canonicalize `{path}`: {error}")]
    RepositoryPathRealpath { path: String, error: String },
    #[error("attempting to instantiate a non-exported module extension")]
    ModuleExtensionNotExported,
    #[error("expected module extension `{0}` to return None or extension_metadata, got `{1}`")]
    InvalidModuleExtensionReturn(String, String),
    #[error("`tag_classes[{0}]` must be a tag_class object, got `{1}`")]
    InvalidTagClass(String, String),
    #[error("module extension `{extension}` was not found in `{path}`")]
    ModuleExtensionSymbolMissing { path: String, extension: String },
    #[error("`{path}` export `{extension}` must be a module_extension, got `{got}`")]
    ModuleExtensionSymbolWrongType {
        path: String,
        extension: String,
        got: String,
    },
    #[error("invalid bzlmod module extension usage data")]
    InvalidModuleExtensionUsageData,
    #[error("module extension `{extension}` has no tag class `{tag}`")]
    UnknownModuleExtensionTag { extension: String, tag: String },
    #[error("`tag_classes[{tag}]` must be a frozen tag_class object, got `{got}`")]
    InvalidFrozenTagClass { tag: String, got: String },
    #[error("module extension tag `{tag}` is missing required attribute `{attr}`")]
    MissingModuleExtensionTagAttribute { tag: String, attr: String },
    #[error("could not read evaluated bzlmod tag expression `{0}`")]
    MissingEvaluatedTagExpression(String),
    #[error("module_ctx.path expected string, Label, or path, got `{0}`")]
    ModuleCtxPathUnsupportedValue(String),
    #[error("error reading `{path}`: {error}")]
    ModuleCtxReadFile { path: String, error: String },
    #[error("module_ctx.download expected string or iterable of strings for `url`, got `{0}`")]
    ModuleCtxDownloadUrlUnsupportedValue(String),
    #[error("module_ctx.download requires at least one URL")]
    ModuleCtxDownloadNoUrls,
    #[error("module_ctx.download auth key must be a string, got `{0}`")]
    ModuleCtxDownloadAuthKeyUnsupportedValue(String),
    #[error("module_ctx.download auth value for `{url}` must be a dict, got `{got}`")]
    ModuleCtxDownloadAuthValueUnsupportedValue { url: String, got: String },
    #[error("module_ctx.download auth field `{field}` for `{url}` must be a string, got `{got}`")]
    ModuleCtxDownloadAuthFieldUnsupportedValue {
        url: String,
        field: &'static str,
        got: String,
    },
    #[error(
        "Found request to do basic auth for {url} without 'login' and 'password' being provided."
    )]
    ModuleCtxDownloadAuthBasicMissingCredentials { url: String },
    #[error("Found request to do pattern auth for {url} without a pattern being provided")]
    ModuleCtxDownloadAuthPatternMissingPattern { url: String },
    #[error("Auth pattern contains {component} but it was not provided in auth dict.")]
    ModuleCtxDownloadAuthPatternMissingComponent { component: String },
    #[error("module_ctx.download `headers` keys must be strings, got `{0}`")]
    ModuleCtxDownloadHeaderKeyUnsupportedValue(String),
    #[error(
        "module_ctx.download `headers[{header}]` must be a string or iterable of strings, got `{got}`"
    )]
    ModuleCtxDownloadHeaderValueUnsupportedValue { header: String, got: String },
    #[error("module_ctx.download failed for {urls:?}: {error}")]
    ModuleCtxDownloadFailed { urls: Vec<String>, error: String },
    #[error("module_ctx.download expected either `sha256` or `integrity`, but not both")]
    ModuleCtxDownloadConflictingChecksums,
    #[error("module_ctx.download unsupported integrity `{0}`")]
    ModuleCtxDownloadUnsupportedIntegrity(String),
    #[error("module_ctx.download checksum mismatch for `{path}`: expected {expected}, got {got}")]
    ModuleCtxDownloadChecksumMismatch {
        path: String,
        expected: String,
        got: String,
    },
    #[error("module_ctx.download could not write `{path}`: {error}")]
    ModuleCtxDownloadWriteFile { path: String, error: String },
}

fn current_bzl_path<'v>(
    eval: &Evaluator<'v, '_, '_>,
    symbol: &'static str,
) -> bz_error::Result<BzlOrBxlPath> {
    let build_context = BuildContext::from_context(eval)?;
    match &build_context.additional {
        PerFileTypeContext::Bzl(bzl_path) => Ok(BzlOrBxlPath::Bzl(bzl_path.bzl_path.clone())),
        _ => Err(BazelRepositoryError::NotInBzl(symbol).into()),
    }
}

fn doc_string(doc: NoneOr<&str>) -> Option<String> {
    doc.into_option().map(|doc| doc.trim().to_owned())
}

fn record_repository_rule_invocation<'v>(
    rule_id: &StarlarkRuleType,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let build_context = BuildContext::from_context(eval)?;
    let recorder = build_context
        .bazel_repository_rule_recorder
        .ok_or_else(|| {
            bz_error::Error::from(
                BazelRepositoryError::RepositoryRuleCalledOutsideModuleExtension,
            )
        })?;

    args.no_positional_args(eval.heap())?;

    let mut name = None;
    let mut attrs = Vec::new();
    for (attr_name, attr_value) in args.names_map()? {
        let attr_name = attr_name.as_str();
        if attr_name == NAME_ATTRIBUTE_FIELD {
            let Some(name_value) = attr_value.unpack_str() else {
                return Err(bz_error::Error::from(
                    BazelRepositoryError::RepositoryRuleNameMustBeString(
                        attr_value.get_type().to_owned(),
                    ),
                )
                .into());
            };
            name = Some(name_value.to_owned());
        } else {
            attrs.push((
                attr_name.to_owned(),
                repository_rule_attr_expression(attr_value)?,
            ));
        }
    }
    let name = name
        .ok_or_else(|| bz_error::Error::from(BazelRepositoryError::RepositoryRuleMissingName))?;
    attrs.sort_by(|(left, _), (right, _)| left.cmp(right));
    let attr_build_file_callsite = repository_rule_attr_build_file_callsite(eval, build_context);

    recorder.record(BazelRepositoryRuleInvocation {
        rule_id: rule_id.clone(),
        original_name: name.clone(),
        name,
        attr_build_file_cell: attr_build_file_callsite.0.as_str().to_owned(),
        attr_build_file_package: attr_build_file_callsite.1,
        attrs,
    });

    Ok(Value::new_none())
}

fn repository_rule_attr_build_file_callsite(
    eval: &Evaluator<'_, '_, '_>,
    build_context: &BuildContext<'_>,
) -> (CellName, Option<String>) {
    let Some(location) = eval.call_stack_top_location() else {
        return (build_context.build_file_cell().name(), None);
    };
    let Ok(project_relative_path) = ProjectRelativePath::new(location.filename()) else {
        return (build_context.build_file_cell().name(), None);
    };
    let callsite_path = build_context
        .cell_info()
        .cell_resolver()
        .get_cell_path(project_relative_path);
    let package = callsite_path
        .parent()
        .map(|package| package.path().as_str().to_owned());
    (callsite_path.cell(), package)
}

fn repository_rule_attr_expression(value: Value<'_>) -> starlark::Result<String> {
    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
        return repository_rule_label_attr_expression(&label);
    }
    if let Some(string) = value.unpack_str() {
        return serde_json::to_string(string).map_err(|e| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "failed to serialize repository_rule string attr: {e}"
            )
            .into()
        });
    }
    if let Some(dict) = DictRef::from_value(value) {
        let mut entries = Vec::new();
        for (key, value) in dict.iter() {
            entries.push(format!(
                "{}: {}",
                repository_rule_attr_expression(key)?,
                repository_rule_attr_expression(value)?
            ));
        }
        return Ok(format!("{{{}}}", entries.join(", ")));
    }
    if let Some(list) = ListRef::from_value(value) {
        let values = list
            .iter()
            .map(repository_rule_attr_expression)
            .collect::<starlark::Result<Vec<_>>>()?;
        return Ok(format!("[{}]", values.join(", ")));
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        let values = tuple
            .iter()
            .map(repository_rule_attr_expression)
            .collect::<starlark::Result<Vec<_>>>()?;
        if values.len() == 1 {
            return Ok(format!("({},)", values[0]));
        }
        return Ok(format!("({})", values.join(", ")));
    }
    Ok(value.to_repr())
}

fn repository_rule_label_attr_expression(
    label: &StarlarkProvidersLabel,
) -> starlark::Result<String> {
    let target = label.label().target();
    let cell_name = target.pkg().cell_name();
    let cell_name = cell_name.as_str();
    let repo_name = if cell_name == "root" {
        String::new()
    } else if cell_name == "bazel_tools" {
        "bazel_tools".to_owned()
    } else {
        bzlmod_canonical_repo_name_for_cell(cell_name).unwrap_or_else(|| cell_name.to_owned())
    };
    let package = target.pkg().cell_relative_path().as_str();
    let name = target.name().as_str();
    let label = if repo_name.is_empty() {
        format!("//{package}:{name}")
    } else {
        format!("@@{repo_name}//{package}:{name}")
    };
    Ok(format!(
        "Label({})",
        serde_json::to_string(&label).map_err(|e| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "failed to serialize repository_rule label attr: {e}"
            )
        })?
    ))
}

fn bazel_canonical_starlark_label_string(
    label: &StarlarkProvidersLabel,
) -> starlark::Result<String> {
    let target = label.label().target();
    let cell_name = target.pkg().cell_name();
    let cell_name = cell_name.as_str();
    let repo_name = if cell_name == "root" {
        String::new()
    } else if cell_name == "bazel_tools" {
        "bazel_tools".to_owned()
    } else {
        bzlmod_canonical_repo_name_for_cell(cell_name).unwrap_or_else(|| cell_name.to_owned())
    };
    let package = target.pkg().cell_relative_path().as_str();
    let name = target.name().as_str();
    if repo_name.is_empty() {
        Ok(format!("@@//{package}:{name}"))
    } else {
        Ok(format!("@@{repo_name}//{package}:{name}"))
    }
}

fn repository_rule_source_uses_unresolved_dynamic_label(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut offset = 0usize;
    while let Some(found) = source[offset..].find("Label") {
        let mut index = offset + found + "Label".len();
        repository_rule_skip_ascii_whitespace(bytes, &mut index);
        if !repository_rule_consume_byte(bytes, &mut index, b'(') {
            offset = index;
            continue;
        }
        repository_rule_skip_ascii_whitespace(bytes, &mut index);
        let Some(quote @ (b'"' | b'\'')) = bytes.get(index).copied() else {
            offset = index;
            continue;
        };
        index += 1;
        if bytes.get(index) == Some(&b'{') {
            return true;
        }
        while index < bytes.len() && bytes[index] != quote {
            index += 1;
        }
        offset = index;
    }
    false
}

fn repository_rule_skip_ascii_whitespace(bytes: &[u8], index: &mut usize) {
    while *index < bytes.len() && bytes[*index].is_ascii_whitespace() {
        *index += 1;
    }
}

fn repository_rule_consume_byte(bytes: &[u8], index: &mut usize, expected: u8) -> bool {
    if bytes.get(*index) == Some(&expected) {
        *index += 1;
        true
    } else {
        false
    }
}

fn empty_dict_value<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocDict(Vec::<(Value<'v>, Value<'v>)>::new()))
}

fn bazel_host_os_name() -> &'static str {
    match env::consts::OS {
        "macos" => "mac os x",
        "windows" => "windows",
        other => other,
    }
}

fn repository_os_name_value(repo_env: &BTreeMap<String, String>) -> String {
    repo_env
        .get(BZLMOD_REPOSITORY_OS_NAME_ENV)
        .cloned()
        .unwrap_or_else(|| bazel_host_os_name().to_owned())
}

fn canonical_bazel_os_name(os_name: &str) -> String {
    let os_name = os_name
        .trim()
        .to_ascii_lowercase()
        .replace('_', " ")
        .replace('-', " ");
    match os_name.as_str() {
        "darwin" | "macos" | "mac os x" => "mac os x".to_owned(),
        "win" | "windows" => "windows".to_owned(),
        "linux" => "linux".to_owned(),
        other => other.to_owned(),
    }
}

fn repository_os_name_matches_host(os_name: &str) -> bool {
    canonical_bazel_os_name(os_name) == canonical_bazel_os_name(bazel_host_os_name())
}

fn repository_ctx_should_search_local_path(repo_env: &BTreeMap<String, String>) -> bool {
    match repo_env.get(BZLMOD_REPOSITORY_OS_NAME_ENV) {
        Some(os_name) => repository_os_name_matches_host(os_name),
        None => true,
    }
}

fn repository_os_arch_value(repo_env: &BTreeMap<String, String>) -> String {
    repo_env
        .get(BZLMOD_REPOSITORY_OS_ARCH_ENV)
        .cloned()
        .unwrap_or_else(|| env::consts::ARCH.to_owned())
}

fn repository_rule_should_use_remote_command_executor(
    repo_env: &BTreeMap<String, String>,
    remotable: bool,
) -> bool {
    remotable || !repository_ctx_should_search_local_path(repo_env)
}

fn repository_rule_command_executor(
    repo_env: &BTreeMap<String, String>,
    remotable: bool,
    eval: &Evaluator<'_, '_, '_>,
) -> starlark::Result<BazelRepositoryCommandExecutor> {
    if repository_rule_should_use_remote_command_executor(repo_env, remotable) {
        return Ok(BuildContext::from_context(eval)?
            .bazel_repository_context
            .as_ref()
            .map(|context| context.command_executor.clone())
            .unwrap_or(BazelRepositoryCommandExecutor::Local));
    }
    Ok(BazelRepositoryCommandExecutor::Local)
}

fn repository_os_name(
    repo_env: &BTreeMap<String, String>,
    _recorded_inputs: &Mutex<Vec<BazelRepositoryRecordedInput>>,
) -> String {
    repository_os_name_value(repo_env)
}

fn repository_os_arch(
    repo_env: &BTreeMap<String, String>,
    _recorded_inputs: &Mutex<Vec<BazelRepositoryRecordedInput>>,
) -> String {
    repository_os_arch_value(repo_env)
}

fn host_environ<'v>(
    heap: Heap<'v>,
    repo_env: &BTreeMap<String, String>,
    _recorded_inputs: &Mutex<Vec<BazelRepositoryRecordedInput>>,
) -> Value<'v> {
    heap.alloc(AllocDict(
        repo_env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repository_rule_source_uses_unresolved_dynamic_label() {
        assert!(repository_rule_source_uses_unresolved_dynamic_label(
            r#"Label("{repo}//:BUILD.bazel".format(repo = repo))"#
        ));
        assert!(repository_rule_source_uses_unresolved_dynamic_label(
            r#"Label ( "{repo}//:BUILD.bazel".format(repo = repo))"#
        ));
        assert!(!repository_rule_source_uses_unresolved_dynamic_label(
            r#"Label("@yq_{}//:yq{}".format(platform, extension))"#
        ));
    }

    #[test]
    fn test_repository_rule_loaded_module_scan_skips_prelude() {
        assert!(!repository_rule_should_scan_loaded_module_cell("prelude"));
        assert!(repository_rule_should_scan_loaded_module_cell(
            "bazel_tools"
        ));
        assert!(repository_rule_should_scan_loaded_module_cell(
            &bzlmod_cell_name("rules_go+")
        ));
    }

    #[test]
    fn test_repository_os_uses_fake_values_without_recording_inputs() {
        let repo_env = BTreeMap::from([
            (BZLMOD_REPOSITORY_OS_NAME_ENV.to_owned(), "linux".to_owned()),
            (
                BZLMOD_REPOSITORY_OS_ARCH_ENV.to_owned(),
                "x86_64".to_owned(),
            ),
        ]);
        let recorded_inputs = Mutex::new(Vec::new());

        assert_eq!("linux", repository_os_name(&repo_env, &recorded_inputs));
        assert_eq!("x86_64", repository_os_arch(&repo_env, &recorded_inputs));

        assert!(recorded_inputs.into_inner().unwrap().is_empty());
    }

    #[test]
    fn test_repository_os_defaults_to_host_without_recording_inputs() {
        let repo_env = BTreeMap::new();
        let recorded_inputs = Mutex::new(Vec::new());

        assert_eq!(
            bazel_host_os_name(),
            repository_os_name(&repo_env, &recorded_inputs)
        );
        assert_eq!(
            env::consts::ARCH,
            repository_os_arch(&repo_env, &recorded_inputs)
        );

        assert!(recorded_inputs.into_inner().unwrap().is_empty());
    }

    #[test]
    fn test_repository_ctx_should_search_local_path_without_fake_os() {
        assert!(repository_ctx_should_search_local_path(&BTreeMap::new()));
    }

    #[test]
    fn test_repository_ctx_should_search_local_path_when_fake_os_matches_host() {
        let repo_env = BTreeMap::from([(
            BZLMOD_REPOSITORY_OS_NAME_ENV.to_owned(),
            bazel_host_os_name().to_owned(),
        )]);

        assert!(repository_ctx_should_search_local_path(&repo_env));
    }

    #[test]
    fn test_repository_ctx_should_not_search_local_path_when_fake_os_differs() {
        let different_os = if repository_os_name_matches_host("linux") {
            "windows"
        } else {
            "linux"
        };
        let repo_env = BTreeMap::from([(
            BZLMOD_REPOSITORY_OS_NAME_ENV.to_owned(),
            different_os.to_owned(),
        )]);

        assert!(!repository_ctx_should_search_local_path(&repo_env));
    }

    #[test]
    fn test_repository_ctx_remote_which_output() {
        assert_eq!(
            Some("/usr/bin/chmod".to_owned()),
            parse_repository_ctx_remote_which_output(b"0\n/usr/bin/chmod\n").unwrap()
        );
        assert_eq!(
            None,
            parse_repository_ctx_remote_which_output(b"1\n").unwrap()
        );
        assert!(parse_repository_ctx_remote_which_output(b"0\n").is_err());
        assert!(parse_repository_ctx_remote_which_output(b"garbage\n").is_err());
    }

    #[test]
    fn test_repository_ctx_command_progress_unwraps_env() {
        let args = vec![
            "-i".to_owned(),
            "TMPDIR=/tmp".to_owned(),
            "/repo/buck-out/gazelle".to_owned(),
            "-go_repository_mode".to_owned(),
        ];

        assert_eq!(
            "running `/repo/buck-out/gazelle`",
            repository_ctx_command_progress("env", &args)
        );
        assert_eq!(
            "running `/repo/buck-out/gazelle`",
            repository_ctx_command_progress("/usr/bin/env", &args)
        );
    }

    #[test]
    fn test_repository_ctx_command_progress_keeps_plain_program() {
        let args = vec!["-c".to_owned()];

        assert_eq!(
            "running `/usr/bin/gcc`",
            repository_ctx_command_progress("/usr/bin/gcc", &args)
        );
    }

    #[test]
    fn test_repository_os_name_match_accepts_common_macos_spellings() {
        assert_eq!(
            canonical_bazel_os_name("macos"),
            canonical_bazel_os_name("mac os x")
        );
        assert_eq!(
            canonical_bazel_os_name("darwin"),
            canonical_bazel_os_name("mac os x")
        );
    }

    #[test]
    fn test_repository_ctx_rejects_nonblocking_downloads() {
        repository_ctx_reject_nonblocking_download(true, "repository_ctx.download").unwrap();

        let error = repository_ctx_reject_nonblocking_download(false, "repository_ctx.download")
            .unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("repository_ctx.download(block = False) is not supported"),
            "error: {error}"
        );
    }

    #[test]
    fn test_remote_path_relative_to_repository_working_dir() {
        assert_eq!(
            "../buck-out/v2/external_cells/repo/bin/tool",
            remote_path_relative_to_working_dir(
                "__bz_repository_ctx_work",
                "buck-out/v2/external_cells/repo/bin/tool",
            )
        );
        assert_eq!(
            "subdir/file.txt",
            remote_path_relative_to_working_dir(
                "__bz_repository_ctx_work",
                "__bz_repository_ctx_work/subdir/file.txt",
            )
        );
        assert_eq!(
            "../../buck-out/v2/out",
            remote_path_relative_to_working_dir(
                "__bz_repository_ctx_work/nested/pkg",
                "buck-out/v2/out",
            )
        );
    }

    #[test]
    fn test_repository_ctx_rewrites_embedded_project_paths_for_remote_execution() {
        let project_root = Path::new("/Users/siggi/Code/buildbuddy");
        let repository_working_dir = Path::new(
            "/Users/siggi/Code/buildbuddy/buck-out/v2/cache/bzlmod_generated_scratch/repo/repository_ctx",
        );
        let command =
            "patch -p0 < /Users/siggi/Code/buildbuddy/buildpatches/protobuf.js_inquire.patch";

        assert_eq!(
            repository_ctx_embedded_project_paths(command, project_root),
            vec![PathBuf::from(
                "/Users/siggi/Code/buildbuddy/buildpatches/protobuf.js_inquire.patch"
            )],
        );
        assert_eq!(
            repository_ctx_rewrite_embedded_project_paths(
                command,
                project_root,
                repository_working_dir,
            )
            .as_deref(),
            Some("patch -p0 < __BUCK2_REMOTE_EXEC_ROOT__/buildpatches/protobuf.js_inquire.patch"),
        );
    }

    #[test]
    fn test_repository_ctx_rewrites_embedded_repository_working_dir_first() {
        let project_root = Path::new("/Users/siggi/Code/buildbuddy");
        let repository_working_dir = Path::new(
            "/Users/siggi/Code/buildbuddy/buck-out/v2/cache/bzlmod_generated_scratch/repo/repository_ctx",
        );
        let command = "cat /Users/siggi/Code/buildbuddy/buck-out/v2/cache/bzlmod_generated_scratch/repo/repository_ctx/package.json";

        assert_eq!(
            repository_ctx_rewrite_embedded_project_paths(
                command,
                project_root,
                repository_working_dir,
            )
            .as_deref(),
            Some("cat __BUCK2_REMOTE_EXEC_ROOT__/__bz_repository_ctx_work/package.json"),
        );
    }

    #[test]
    fn test_repository_ctx_external_repo_root_project_path() {
        assert_eq!(
            Some(ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0"
                    .to_owned(),
            )),
            repository_ctx_external_repo_root_project_path(ProjectRelativePath::unchecked_new(
                "buck-out/v2/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0/bin/go",
            ))
        );
        assert_eq!(
            Some(ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2/external_cells/bzlmod/gazelle+".to_owned(),
            )),
            repository_ctx_external_repo_root_project_path(ProjectRelativePath::unchecked_new(
                "buck-out/v2/external_cells/bzlmod/gazelle+/internal/common.bzl",
            ))
        );
        assert_eq!(
            None,
            repository_ctx_external_repo_root_project_path(ProjectRelativePath::unchecked_new(
                "buck-out/v2/cache/repository/foo",
            ))
        );
    }

    #[test]
    fn test_repository_rule_execution_cache_key_distinguishes_remote_execution() {
        use bz_core::execution_types::executor_config::CacheUploadBehavior;
        use bz_core::execution_types::executor_config::CommandGenerationOptions;
        use bz_core::execution_types::executor_config::MetaInternalExtraParams;
        use bz_core::execution_types::executor_config::OutputPathsBehavior;
        use bz_core::execution_types::executor_config::PathSeparatorKind;
        use bz_core::execution_types::executor_config::RePlatformFields;
        use bz_core::execution_types::executor_config::RemoteEnabledExecutorOptions;
        use bz_core::execution_types::executor_config::RemoteExecutorOptions;
        use bz_core::execution_types::executor_config::RemoteExecutorUseCase;

        let local = CommandExecutorConfig::testing_local();
        let remote = CommandExecutorConfig {
            executor: Executor::RemoteEnabled(RemoteEnabledExecutorOptions {
                executor: RemoteEnabledExecutor::Remote(RemoteExecutorOptions::default()),
                re_properties: RePlatformFields::default(),
                re_use_case: RemoteExecutorUseCase::bz_default(),
                re_action_key: None,
                cache_upload_behavior: CacheUploadBehavior::Disabled,
                remote_cache_enabled: true,
                remote_dep_file_cache_enabled: false,
                dependencies: Vec::new(),
                gang_workers: Vec::new(),
                custom_image: None,
                meta_internal_extra_params: MetaInternalExtraParams::default_arc(),
                priority: None,
            }),
            options: CommandGenerationOptions {
                path_separator: PathSeparatorKind::Unix,
                output_paths_behavior: OutputPathsBehavior::OutputPaths,
                use_bazel_protocol_remote_persistent_workers: false,
            },
        };

        assert_ne!(
            bzlmod_repository_rule_execution_cache_key(&local),
            bzlmod_repository_rule_execution_cache_key(&remote),
        );
    }

    #[test]
    fn test_repository_ctx_external_input_dep_includes_path() {
        assert_eq!(
            repository_ctx_external_input_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod/gazelle+/internal/list_repository_tools_srcs.go",
            )),
            Some(RepositoryPathLabelDep::cell_path(
                bzlmod_cell_name("gazelle+"),
                "internal/list_repository_tools_srcs.go".to_owned(),
            ))
        );
        assert_eq!(
            repository_ctx_external_input_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0/bin/go",
            )),
            Some(RepositoryPathLabelDep::cell_path(
                bzlmod_cell_name("rules_go++go_sdk+main___download_0"),
                "bin/go".to_owned(),
            ))
        );
        assert_eq!(
            repository_ctx_external_input_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod_generated/repo.repository_ctx/file",
            )),
            None
        );
        assert_eq!(
            repository_ctx_external_input_tree_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod/gazelle+",
            )),
            Some(RepositoryPathLabelDep::tree(
                bzlmod_cell_name("gazelle+"),
                None,
            ))
        );
        assert_eq!(
            repository_ctx_external_input_tree_dep(Path::new(
                "/repo/buck-out/v2/external_cells/bzlmod/gazelle+/internal",
            )),
            Some(RepositoryPathLabelDep::tree(
                bzlmod_cell_name("gazelle+"),
                Some("internal".to_owned()),
            ))
        );
    }

    #[test]
    fn test_repository_ctx_command_path_preserves_external_assignment_prefix() {
        let working_dir =
            "buck-out/v2/external_cells/bzlmod_generated/gazelle++deps+tools.repository_ctx";
        let rewritten = repository_ctx_command_path(
            "GOROOT=/repo/buck-out/buildbuddy-source-file-1/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0",
            working_dir,
        );
        assert!(rewritten.starts_with("GOROOT="));
        assert!(rewritten.contains(
            "/buck-out/v2/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0"
        ));

        let rewritten = repository_ctx_command_path(
            "/repo/buck-out/buildbuddy-source-file-1/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0/bin/go",
            working_dir,
        );
        assert!(rewritten.ends_with(
            "/buck-out/v2/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0/bin/go"
        ));

        let rewritten = repository_ctx_command_path(
            "all=-trimpath=/repo/buck-out/buildbuddy-source-file-1/external_cells/bzlmod_generated/gazelle++deps+tools",
            working_dir,
        );
        assert!(rewritten.starts_with("all=-trimpath="));
    }

    #[test]
    fn test_repository_ctx_command_path_resolves_external_cells_from_cache_scratch_dir() {
        let working_dir =
            "/repo/buck-out/v2/cache/bzlmod_generated_scratch/gazelle++deps+tools/repository_ctx";
        let rewritten = repository_ctx_command_path(
            "GOROOT=/repo/buck-out/buildbuddy-source-file-1/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0",
            working_dir,
        );

        assert_eq!(
            rewritten,
            "GOROOT=/repo/buck-out/v2/external_cells/bzlmod_generated/rules_go++go_sdk+main___download_0"
        );
        assert_eq!(
            repository_ctx_workspace_root(working_dir),
            "/repo".to_owned()
        );
    }

    #[test]
    fn test_repository_ctx_patch_strip_rejects_negative() {
        assert_eq!(repository_ctx_patch_strip(0, "patch.diff").unwrap(), 0);
        assert_eq!(repository_ctx_patch_strip(2, "patch.diff").unwrap(), 2);

        let error = repository_ctx_patch_strip(-1, "patch.diff")
            .unwrap_err()
            .to_string();
        assert!(error.contains("strip must be non-negative"));
        assert!(error.contains("patch.diff"));
    }

    #[test]
    fn test_repository_ctx_renamed_strip_prefix() {
        assert_eq!(
            repository_ctx_renamed_strip_prefix("download_and_extract", "new", "").unwrap(),
            "new"
        );
        assert_eq!(
            repository_ctx_renamed_strip_prefix("download_and_extract", "", "old").unwrap(),
            "old"
        );

        let error = repository_ctx_renamed_strip_prefix("download_and_extract", "new", "old")
            .unwrap_err()
            .to_string();
        assert!(error.contains("download_and_extract() got multiple values"));
        assert!(error.contains("stripPrefix"));
    }

    #[test]
    fn test_repository_path_display_is_absolute() {
        let path = StarlarkRepositoryPath::new("buck-out/v2/external_cells/repo/file".to_owned());
        assert!(Path::new(&path.to_string()).is_absolute());
    }

    #[test]
    fn test_repository_path_get_child_normalizes_relative_paths() {
        assert_eq!(
            repository_path_get_child("repo/root", ["./.bazelignore"]),
            "repo/root/.bazelignore"
        );
        assert_eq!(
            repository_path_get_child("repo/root", ["a/./b", "../c"]),
            "repo/root/a/c"
        );
        assert_eq!(
            repository_path_get_child("repo/root", ["/tmp/./file"]),
            "/tmp/file"
        );
    }

    #[test]
    fn test_repository_ctx_output_path_rejects_escapes() {
        let working_dir = "buck-out/v2/external_cells/bzlmod_generated/repo.repository_ctx";

        assert_eq!(
            repository_ctx_output_path_from_relative_path("dir/../file", working_dir).unwrap(),
            "file"
        );
        assert_eq!(
            repository_ctx_output_path_from_resolved_path(
                "buck-out/v2/external_cells/bzlmod_generated/repo.repository_ctx/dir/file",
                working_dir,
            )
            .unwrap(),
            "dir/file"
        );
        assert!(
            repository_ctx_output_path_from_relative_path("../file", working_dir).is_err(),
            "relative output paths must not escape the repository root"
        );
        assert!(
            repository_ctx_output_path_from_relative_path("/tmp/file", working_dir).is_err(),
            "absolute output paths must not be accepted as repository-relative strings"
        );
        assert!(
            repository_ctx_output_path_from_resolved_path(
                "buck-out/v2/external_cells/bzlmod_generated/other.repository_ctx/file",
                working_dir,
            )
            .is_err(),
            "path objects must point inside the current repository root"
        );
    }

    #[test]
    fn test_module_ctx_checksum_from_sha384_integrity() {
        let integrity = "sha384-ZoEgzfCLmDk7eoKdJSoq/nny1iX3Cq9mMJ3gnPZ2ejhKMxSgHUQIa7MREToxYl6Z";
        let checksum = module_ctx_checksum_from_integrity(integrity)
            .unwrap()
            .unwrap();
        assert_eq!(checksum.kind, ModuleCtxChecksumKind::Sha384);
        assert_eq!(checksum.hex.len(), 96);
        assert_eq!(
            module_ctx_integrity_from_checksum(&checksum).unwrap(),
            integrity
        );
    }

    #[test]
    fn test_module_ctx_download_auth_headers_match_url() {
        let heap = Heap::new();
        let url = "https://example.com/archive.zip";
        let auth = heap.alloc(AllocDict([
            ("type", "basic"),
            ("login", "user"),
            ("password", "pass"),
        ]));
        let entries = UnpackDictEntries {
            entries: vec![(heap.alloc(url), auth)],
        };
        let auth_headers = module_ctx_download_auth_headers_from_entries(&entries).unwrap();

        assert_eq!(
            module_ctx_download_request_headers_for_url(
                url,
                &[("x-test".to_owned(), "1".to_owned())],
                &auth_headers,
            ),
            vec![("x-test", "1"), ("Authorization", "Basic dXNlcjpwYXNz"),],
        );
        assert_eq!(
            module_ctx_download_request_headers_for_url(
                "https://example.com/other.zip",
                &[],
                &auth_headers,
            ),
            Vec::<(&str, &str)>::new(),
        );
    }

    #[test]
    fn test_module_ctx_download_pattern_auth() {
        let heap = Heap::new();
        let url = "https://example.com/archive.zip";
        let auth = heap.alloc(AllocDict([
            ("type", "pattern"),
            ("pattern", "Bearer <login>:<password>"),
            ("login", "user"),
            ("password", "pass"),
        ]));
        let entries = UnpackDictEntries {
            entries: vec![(heap.alloc(url), auth)],
        };
        let auth_headers = module_ctx_download_auth_headers_from_entries(&entries).unwrap();

        assert_eq!(
            module_ctx_download_request_headers_for_url(url, &[], &auth_headers),
            vec![("Authorization", "Bearer user:pass")],
        );
    }

    fn module_ctx_download_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        std::env::temp_dir().join(format!(
            "buck2-module-ctx-download-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn module_ctx_download_tmp_entries(dir: &Path, destination_name: &str) -> Vec<String> {
        let mut entries = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|entry| entry.starts_with(&format!(".{destination_name}.tmp.")))
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    #[test]
    fn test_module_ctx_copy_download_file_publishes_without_tmp_leftovers() {
        let dir = module_ctx_download_test_dir("success");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source");
        let destination = dir.join("dest");
        fs::write(&source, b"new").unwrap();
        fs::write(&destination, b"old").unwrap();

        module_ctx_copy_download_file(&source, &destination, false).unwrap();

        assert_eq!(b"new", fs::read(&destination).unwrap().as_slice());
        assert_eq!(
            Vec::<String>::new(),
            module_ctx_download_tmp_entries(&dir, "dest")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_module_ctx_copy_download_file_failure_preserves_destination() {
        let dir = module_ctx_download_test_dir("failure");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("missing");
        let destination = dir.join("dest");
        fs::write(&destination, b"old").unwrap();

        module_ctx_copy_download_file(&source, &destination, false).unwrap_err();

        assert_eq!(b"old", fs::read(&destination).unwrap().as_slice());
        assert_eq!(
            Vec::<String>::new(),
            module_ctx_download_tmp_entries(&dir, "dest")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    fn module_ctx_download_test_key(name: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        format!(
            "test-module-ctx-download-cache-lock-{name}-{}-{nanos}",
            std::process::id()
        )
    }

    fn module_ctx_download_cache_lock_exists(key: &str) -> bool {
        MODULE_CTX_DOWNLOAD_CACHE_LOCKS.get().is_some_and(|locks| {
            locks
                .lock()
                .expect("module ctx download cache lock map is poisoned")
                .contains_key(key)
        })
    }

    #[test]
    fn test_module_ctx_download_cache_lock_release_prunes_unused_lock() {
        let key = module_ctx_download_test_key("unused");
        let lock = module_ctx_download_cache_lock(&key);
        assert!(module_ctx_download_cache_lock_exists(&key));

        module_ctx_download_cache_release_lock(&key, &lock);

        assert!(!module_ctx_download_cache_lock_exists(&key));
    }

    #[test]
    fn test_module_ctx_download_cache_lock_release_keeps_shared_lock() {
        let key = module_ctx_download_test_key("shared");
        let lock = module_ctx_download_cache_lock(&key);
        let other = module_ctx_download_cache_lock(&key);

        module_ctx_download_cache_release_lock(&key, &lock);

        assert!(module_ctx_download_cache_lock_exists(&key));
        drop(other);
        module_ctx_download_cache_release_lock(&key, &lock);
        assert!(!module_ctx_download_cache_lock_exists(&key));
    }

    #[test]
    fn test_module_ctx_download_cache_import_entry_copies_file_and_canonical_id() {
        let dir = module_ctx_download_test_dir("cache-import");
        fs::create_dir_all(&dir).unwrap();
        let source_entry = dir.join("source");
        let destination_entry = dir.join("destination");
        fs::create_dir_all(&source_entry).unwrap();

        let contents = b"cached archive";
        let checksum = ModuleCtxChecksum {
            kind: ModuleCtxChecksumKind::Sha256,
            hex: module_ctx_checksum_hex(ModuleCtxChecksumKind::Sha256, contents),
        };
        let canonical_id = "https://example.com/archive.pkg";
        fs::write(source_entry.join("file"), contents).unwrap();
        fs::write(
            module_ctx_repository_cache_id_path(&source_entry, &checksum, canonical_id).unwrap(),
            b"",
        )
        .unwrap();

        module_ctx_download_cache_import_entry(
            &checksum,
            canonical_id,
            &source_entry,
            &destination_entry,
        )
        .unwrap();

        assert_eq!(
            contents,
            fs::read(destination_entry.join("file")).unwrap().as_slice()
        );
        assert!(
            module_ctx_repository_cache_id_path(&destination_entry, &checksum, canonical_id)
                .unwrap()
                .exists()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    fn repository_recorded_input_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "buck2-repository-recorded-input-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn test_repository_recorded_file_value_records_symlink_text() {
        let dir = repository_recorded_input_test_dir("file-symlink");
        let target = dir.join("target");
        let other = dir.join("other");
        let link = dir.join("link");
        fs::write(&target, "old").unwrap();
        fs::write(&other, "other").unwrap();
        std::os::unix::fs::symlink("target", &link).unwrap();

        let initial = repository_recorded_file_value(&link).unwrap();
        fs::write(&target, "new").unwrap();
        assert_eq!(initial, repository_recorded_file_value(&link).unwrap());

        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("other", &link).unwrap();
        assert_ne!(initial, repository_recorded_file_value(&link).unwrap());

        assert_eq!("SYMLINK:target", initial);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_repository_recorded_dirents_value_records_symlink_text() {
        let dir = repository_recorded_input_test_dir("dirents-symlink");
        let target = dir.join("target");
        let link = dir.join("link");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("file"), "old").unwrap();
        std::os::unix::fs::symlink("target", &link).unwrap();

        let initial = repository_recorded_dirents_value(&link).unwrap();
        fs::write(target.join("file"), "new").unwrap();

        assert_eq!(initial, repository_recorded_dirents_value(&link).unwrap());
        assert_eq!("SYMLINK:target", initial);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_repository_recorded_dir_tree_value_records_symlink_text() {
        let dir = repository_recorded_input_test_dir("tree-symlink");
        let tree = dir.join("tree");
        fs::create_dir_all(&tree).unwrap();
        let target = dir.join("target");
        let other = dir.join("other");
        let link = tree.join("link");
        fs::write(&target, "old").unwrap();
        fs::write(&other, "other").unwrap();
        std::os::unix::fs::symlink("../target", &link).unwrap();

        let initial = repository_recorded_dir_tree_value(&tree).unwrap();
        fs::write(&target, "new").unwrap();
        assert_eq!(initial, repository_recorded_dir_tree_value(&tree).unwrap());

        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("../other", &link).unwrap();
        assert_ne!(initial, repository_recorded_dir_tree_value(&tree).unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }
}

fn validate_module_extension_return<'v>(
    extension_id: &StarlarkRuleType,
    value: Value<'v>,
) -> starlark::Result<Value<'v>> {
    if value.is_none()
        || value
            .downcast_ref::<StarlarkModuleExtensionMetadata>()
            .is_some()
    {
        return Ok(value);
    }
    Err(
        bz_error::Error::from(BazelRepositoryError::InvalidModuleExtensionReturn(
            extension_id.to_string(),
            value.get_type().to_owned(),
        ))
        .into(),
    )
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionEvaluationConfig {
    root_module_has_non_dev_dependency: bool,
    modules: Vec<BzlmodModuleExtensionModuleConfig>,
    #[serde(default)]
    repo_overrides: Vec<(String, String)>,
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionModuleConfig {
    name: String,
    version: String,
    canonical_repo_name: String,
    is_root: bool,
    #[serde(default)]
    extension_bzl_file: String,
    #[serde(default)]
    extension_name: String,
    cell_aliases: Vec<(String, String)>,
    constants: Vec<(String, String)>,
    tags: Vec<BzlmodModuleExtensionTagConfig>,
}

#[derive(Debug, Deserialize)]
struct BzlmodModuleExtensionTagConfig {
    tag_name: String,
    dev_dependency: bool,
    bindings: Vec<(String, String)>,
    kwargs: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BazelRepositoryGeneratedFile {
    pub path: String,
    pub content: String,
    pub executable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BazelRepositoryRuleProgress {
    pub repo: String,
    pub path: String,
    pub kind: String,
}

pub(crate) enum BazelRepositoryRuleEvaluation {
    Success(BazelRepositoryRuleEvaluationResult),
    NeedsPathLabelDeps {
        label_deps: Vec<RepositoryPathLabelDep>,
        error: String,
    },
}

pub enum BazelModuleExtensionEvaluation {
    Success(BazelModuleExtensionEvaluationResult),
    NeedsPathLabelDeps {
        label_deps: Vec<RepositoryPathLabelDep>,
        error: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BazelRepositoryRuleEvaluationResult {
    pub files: Vec<BazelRepositoryGeneratedFile>,
    pub recorded_inputs: Vec<BazelRepositoryRecordedInput>,
    pub path_label_deps: Vec<RepositoryPathLabelDep>,
    pub reproducible: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BazelRepositoryRuleCacheInfo {
    pub predeclared_input_hash: String,
    pub local: bool,
}

pub async fn evaluate_bzlmod_module_extension_repo(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
    module_ctx_working_dir: &str,
    _current_canonical_repo_name: Option<&str>,
    cancellation: &CancellationContext,
) -> bz_error::Result<BazelModuleExtensionEvaluationResult> {
    let extension_cell_path = CellPath::new(
        CellName::unchecked_new(&setup.extension_bzl_cell)?,
        CellRelativePathBuf::try_from(setup.extension_bzl_path.to_string())?,
    );
    let extension_path = ImportPath::new_same_cell(extension_cell_path)?;
    let extension_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(&extension_path))
        .await?;
    let repo_env = ctx.get_bzlmod_repository_environment().await?;
    let mut materialized_path_label_deps = BTreeSet::new();
    loop {
        let mut interpreter = ctx
            .get_interpreter_calculator(OwnedStarlarkPath::LoadFile(extension_path.clone()))
            .await?;
        match interpreter
            .eval_bzlmod_module_extension(
                &extension_path,
                &extension_module,
                &setup.extension_name,
                &setup.extension_usages_json,
                module_ctx_working_dir,
                repo_env.clone(),
                cancellation,
            )
            .await?
        {
            BazelModuleExtensionEvaluation::Success(result) => {
                let new_label_deps = result
                    .path_label_deps
                    .iter()
                    .filter_map(|dep| {
                        materialized_path_label_deps
                            .insert(dep.clone())
                            .then(|| dep.clone())
                    })
                    .collect::<Vec<_>>();
                if new_label_deps.is_empty() {
                    return Ok(result);
                }
                materialize_repository_rule_path_label_deps(ctx, &new_label_deps).await?;
                repository_ctx_clean_working_dir(module_ctx_working_dir)?;
            }
            BazelModuleExtensionEvaluation::NeedsPathLabelDeps { label_deps, error } => {
                let new_label_deps = label_deps
                    .into_iter()
                    .filter(|dep| materialized_path_label_deps.insert(dep.clone()))
                    .collect::<Vec<_>>();
                if new_label_deps.is_empty() {
                    return Err(bz_error::bz_error!(
                        bz_error::ErrorTag::Input,
                        "module_extension `{}%{}` failed after materializing module_ctx path labels: {}",
                        extension_path,
                        setup.extension_name,
                        error
                    ));
                }
                materialize_repository_rule_path_label_deps(ctx, &new_label_deps).await?;
                repository_ctx_clean_working_dir(module_ctx_working_dir)?;
            }
        }
    }
}

fn collect_loaded_module_load_paths(
    module: &LoadedModule,
    seen: &mut BTreeSet<String>,
    paths: &mut Vec<ImportPath>,
) {
    for loaded in module.loaded_modules().map.values() {
        let key = loaded.path().to_string();
        if !seen.insert(key) {
            continue;
        }
        if let StarlarkModulePath::LoadFile(path) = loaded.path() {
            if !repository_rule_should_scan_loaded_module_cell(path.path().cell().as_str()) {
                continue;
            }
            paths.push(path.clone());
        }
        collect_loaded_module_load_paths(loaded, seen, paths);
    }
}

fn repository_rule_should_scan_loaded_module_cell(cell_name: &str) -> bool {
    cell_name != "prelude"
}

pub async fn evaluate_bzlmod_repository_rule(
    ctx: &mut DiceComputations<'_>,
    invocation: &BazelRepositoryRuleInvocation,
    repository_ctx_working_dir: &str,
    progress: Option<BazelRepositoryRuleProgress>,
    cancellation: &CancellationContext,
) -> bz_error::Result<Vec<BazelRepositoryGeneratedFile>> {
    Ok(evaluate_bzlmod_repository_rule_with_recorded_inputs(
        ctx,
        invocation,
        repository_ctx_working_dir,
        progress,
        cancellation,
    )
    .await?
    .files)
}

pub async fn evaluate_bzlmod_repository_rule_with_recorded_inputs(
    ctx: &mut DiceComputations<'_>,
    invocation: &BazelRepositoryRuleInvocation,
    repository_ctx_working_dir: &str,
    progress: Option<BazelRepositoryRuleProgress>,
    cancellation: &CancellationContext,
) -> bz_error::Result<BazelRepositoryRuleEvaluationResult> {
    let rule_path = match &invocation.rule_id.path {
        BzlOrBxlPath::Bzl(path) => path,
        BzlOrBxlPath::Bxl(_) => {
            return Err(BazelRepositoryError::RepositoryRuleBxlUnsupported(
                invocation.rule_id.to_string(),
            )
            .into());
        }
    };
    let rule_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(rule_path))
        .await?;
    let repo_env = ctx.get_bzlmod_repository_environment().await?;
    let mut materialized_path_label_deps = BTreeSet::new();
    loop {
        let mut interpreter = ctx
            .get_interpreter_calculator(OwnedStarlarkPath::LoadFile(rule_path.clone()))
            .await?;
        let evaluation = interpreter.eval_bzlmod_repository_rule(
            rule_path,
            &rule_module,
            invocation,
            repository_ctx_working_dir,
            repo_env.clone(),
            cancellation,
        );
        let evaluation = if let Some(progress) = progress.as_ref() {
            bz_events::dispatch::span_async_simple(
                bz_data::BzlmodRepoStart {
                    repo: progress.repo.clone(),
                    path: progress.path.clone(),
                    kind: progress.kind.clone(),
                    progress: "starting".to_owned(),
                },
                evaluation,
                bz_data::BzlmodRepoEnd {
                    repo: progress.repo.clone(),
                    path: progress.path.clone(),
                    kind: progress.kind.clone(),
                },
            )
            .await?
        } else {
            evaluation.await?
        };
        match evaluation {
            BazelRepositoryRuleEvaluation::Success(result) => {
                let new_label_deps = result
                    .path_label_deps
                    .iter()
                    .filter_map(|dep| {
                        materialized_path_label_deps
                            .insert(dep.clone())
                            .then(|| dep.clone())
                    })
                    .collect::<Vec<_>>();
                if new_label_deps.is_empty() {
                    return Ok(result);
                }
                materialize_repository_rule_path_label_deps(ctx, &new_label_deps).await?;
                repository_ctx_clean_working_dir(repository_ctx_working_dir)?;
            }
            BazelRepositoryRuleEvaluation::NeedsPathLabelDeps { label_deps, error } => {
                let new_label_deps = label_deps
                    .into_iter()
                    .filter(|dep| materialized_path_label_deps.insert(dep.clone()))
                    .collect::<Vec<_>>();
                if new_label_deps.is_empty() {
                    return Err(bz_error::bz_error!(
                        bz_error::ErrorTag::Input,
                        "repository_rule `{}` failed after materializing repository_ctx path labels: {}",
                        invocation.rule_id,
                        error
                    ));
                }
                materialize_repository_rule_path_label_deps(ctx, &new_label_deps).await?;
                repository_ctx_clean_working_dir(repository_ctx_working_dir)?;
            }
        }
    }
}

pub async fn repository_rule_uses_unresolved_dynamic_label(
    ctx: &mut DiceComputations<'_>,
    invocation: &BazelRepositoryRuleInvocation,
) -> bz_error::Result<bool> {
    let rule_path = match &invocation.rule_id.path {
        BzlOrBxlPath::Bzl(path) => path,
        BzlOrBxlPath::Bxl(_) => return Ok(false),
    };
    let source = DiceFileComputations::read_file(ctx, rule_path.path().as_ref())
        .await
        .with_package_context_information(rule_path.path().path().to_string())?;
    if repository_rule_source_uses_unresolved_dynamic_label(&source) {
        return Ok(true);
    }

    let loaded_paths = repository_rule_loaded_module_load_paths(ctx, rule_path).await?;
    for path in loaded_paths.iter() {
        let source = DiceFileComputations::read_file(ctx, path.path().as_ref())
            .await
            .with_package_context_information(path.path().path().to_string())?;
        if repository_rule_source_uses_unresolved_dynamic_label(&source) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("repository rule loaded module load paths for {}", path)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct RepositoryRuleLoadedModuleLoadPathsKey {
    path: ImportPath,
}

#[async_trait::async_trait]
impl Key for RepositoryRuleLoadedModuleLoadPathsKey {
    type Value = bz_error::Result<Arc<Vec<ImportPath>>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let module = ctx
            .get_loaded_module(StarlarkModulePath::LoadFile(&self.path))
            .await?;
        let mut paths = Vec::new();
        collect_loaded_module_load_paths(&module, &mut BTreeSet::new(), &mut paths);
        Ok(Arc::new(paths))
    }

    fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
        false
    }

    fn validity(x: &Self::Value) -> bool {
        x.is_ok()
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn repository_rule_loaded_module_load_paths(
    ctx: &mut DiceComputations<'_>,
    path: &ImportPath,
) -> bz_error::Result<Arc<Vec<ImportPath>>> {
    ctx.compute(&RepositoryRuleLoadedModuleLoadPathsKey { path: path.clone() })
        .await?
}

async fn bzlmod_bzl_transitive_digest(
    ctx: &mut DiceComputations<'_>,
    bzl_path: ImportPath,
) -> bz_error::Result<String> {
    let bzl_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(&bzl_path))
        .await?;
    let mut paths = vec![bzl_path];
    collect_loaded_module_load_paths(&bzl_module, &mut BTreeSet::new(), &mut paths);
    paths.sort_by_key(|path| path.to_string());
    paths.dedup_by_key(|path| path.to_string());

    let mut hasher = blake3::Hasher::new();
    for path in paths {
        let path_string = path.to_string();
        hasher.update(path_string.as_bytes());
        hasher.update(&[0]);
        let source = DiceFileComputations::read_file(ctx, path.path().as_ref())
            .await
            .with_package_context_information(path.path().path().to_string())?;
        hasher.update(source.as_bytes());
        hasher.update(&[0]);
    }
    Ok(blake3::Hasher::finalize(&hasher).to_hex().to_string())
}

fn collect_loaded_module_bazel_digest_postorder(
    module: &LoadedModule,
    seen: &mut BTreeSet<String>,
    modules: &mut Vec<LoadedModule>,
) {
    let key = module.path().to_string();
    if !seen.insert(key) {
        return;
    }
    for loaded in module.loaded_modules().ordered_modules() {
        if loaded_module_bazel_digest_path(loaded).is_some() {
            collect_loaded_module_bazel_digest_postorder(loaded, seen, modules);
        }
    }
    if loaded_module_bazel_digest_path(module).is_some() {
        modules.push(module.dupe());
    }
}

fn loaded_module_bazel_digest_path(module: &LoadedModule) -> Option<ImportPath> {
    match module.path() {
        StarlarkModulePath::LoadFile(path)
            if repository_rule_should_scan_loaded_module_cell(path.path().cell().as_str()) =>
        {
            Some(path.clone())
        }
        _ => None,
    }
}

async fn bzlmod_bazel_bzl_transitive_digest(
    ctx: &mut DiceComputations<'_>,
    bzl_path: ImportPath,
) -> bz_error::Result<String> {
    let bzl_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(&bzl_path))
        .await?;
    let mut modules = Vec::new();
    collect_loaded_module_bazel_digest_postorder(&bzl_module, &mut BTreeSet::new(), &mut modules);

    let mut digests = HashMap::<String, Vec<u8>>::new();
    for module in modules {
        let path = loaded_module_bazel_digest_path(&module).ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "loaded module `{}` cannot be included in a Bazel bzl transitive digest",
                module.path()
            )
        })?;
        let source = DiceFileComputations::read_file(ctx, path.path().as_ref())
            .await
            .with_package_context_information(path.path().path().to_string())?;
        let compile_digest = Sha256::digest(source.as_bytes());

        let mut transitive = Sha256::new();
        transitive.update(compile_digest.as_slice());
        for loaded in module.loaded_modules().ordered_modules() {
            if loaded_module_bazel_digest_path(loaded).is_none() {
                continue;
            }
            let key = loaded.path().to_string();
            let digest = digests.get(&key).ok_or_else(|| {
                bz_error::bz_error!(
                    bz_error::ErrorTag::Tier0,
                    "missing Bazel bzl transitive digest for loaded module `{}`",
                    key
                )
            })?;
            transitive.update(digest);
        }
        let digest = transitive.finalize().to_vec();
        digests.insert(module.path().to_string(), digest);
    }
    let root_digest = digests
        .get(&StarlarkModulePath::LoadFile(&bzl_path).to_string())
        .ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "missing Bazel bzl transitive digest for root module `{}`",
                bzl_path
            )
        })?;
    Ok(BASE64_STANDARD.encode(root_digest))
}

pub async fn bzlmod_module_extension_bzl_transitive_digest(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
) -> bz_error::Result<String> {
    let extension_cell_path = CellPath::new(
        CellName::unchecked_new(&setup.extension_bzl_cell)?,
        CellRelativePathBuf::try_from(setup.extension_bzl_path.to_string())?,
    );
    let extension_path = ImportPath::new_same_cell(extension_cell_path)?;
    bzlmod_bzl_transitive_digest(ctx, extension_path).await
}

pub async fn bzlmod_module_extension_bazel_bzl_transitive_digest(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
) -> bz_error::Result<String> {
    let extension_cell_path = CellPath::new(
        CellName::unchecked_new(&setup.extension_bzl_cell)?,
        CellRelativePathBuf::try_from(setup.extension_bzl_path.to_string())?,
    );
    let extension_path = ImportPath::new_same_cell(extension_cell_path)?;
    bzlmod_bazel_bzl_transitive_digest(ctx, extension_path).await
}

#[derive(Debug, Clone, Copy)]
pub struct BzlmodModuleExtensionEvalFactorDeps {
    pub os_dependent: bool,
    pub arch_dependent: bool,
}

pub async fn bzlmod_module_extension_eval_factor_deps(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
) -> bz_error::Result<BzlmodModuleExtensionEvalFactorDeps> {
    let extension_cell_path = CellPath::new(
        CellName::unchecked_new(&setup.extension_bzl_cell)?,
        CellRelativePathBuf::try_from(setup.extension_bzl_path.to_string())?,
    );
    let extension_path = ImportPath::new_same_cell(extension_cell_path)?;
    let extension_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(&extension_path))
        .await?;
    let extension_value = extension_module
        .env()
        .get_option(&setup.extension_name)
        .map_err(|e| bz_error::conversion::from_any_with_tag(e, bz_error::ErrorTag::Input))?
        .ok_or_else(|| {
            bz_error::Error::from(BazelRepositoryError::ModuleExtensionSymbolMissing {
                path: extension_path.to_string(),
                extension: setup.extension_name.to_string(),
            })
        })?;
    let extension = module_extension_from_loaded_module(
        &extension_path,
        &setup.extension_name,
        extension_value,
    )?;
    Ok(BzlmodModuleExtensionEvalFactorDeps {
        os_dependent: extension.os_dependent,
        arch_dependent: extension.arch_dependent,
    })
}

fn update_repository_rule_cache_key(hasher: &mut blake3::Hasher, field: &str) {
    hasher.update(field.len().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(field.as_bytes());
    hasher.update(b"\0");
}

fn update_repository_rule_cache_key_opt(hasher: &mut blake3::Hasher, field: Option<&str>) {
    match field {
        Some(field) => {
            update_repository_rule_cache_key(hasher, "some");
            update_repository_rule_cache_key(hasher, field);
        }
        None => update_repository_rule_cache_key(hasher, "none"),
    }
}

fn bzlmod_repository_rule_execution_cache_key(config: &CommandExecutorConfig) -> String {
    let Executor::RemoteEnabled(options) = &config.executor else {
        return "local".to_owned();
    };
    if !matches!(
        options.executor,
        RemoteEnabledExecutor::Remote(_) | RemoteEnabledExecutor::Hybrid { .. }
    ) {
        return "local".to_owned();
    }

    let mut hasher = blake3::Hasher::new();
    update_repository_rule_cache_key(&mut hasher, "remote-v1");
    update_repository_rule_cache_key(&mut hasher, options.re_use_case.as_str());
    update_repository_rule_cache_key(&mut hasher, &format!("{:?}", config.options.path_separator));
    update_repository_rule_cache_key(
        &mut hasher,
        &format!("{:?}", config.options.output_paths_behavior),
    );
    update_repository_rule_cache_key(
        &mut hasher,
        &config
            .options
            .use_bazel_protocol_remote_persistent_workers
            .to_string(),
    );
    update_repository_rule_cache_key(
        &mut hasher,
        &options.re_properties.properties.len().to_string(),
    );
    for (name, value) in options.re_properties.properties.iter() {
        update_repository_rule_cache_key(&mut hasher, name);
        update_repository_rule_cache_key(&mut hasher, value);
    }
    update_repository_rule_cache_key(&mut hasher, &options.dependencies.len().to_string());
    for dependency in &options.dependencies {
        update_repository_rule_cache_key(&mut hasher, &dependency.smc_tier);
        update_repository_rule_cache_key(&mut hasher, &dependency.id);
    }
    update_repository_rule_cache_key(&mut hasher, &options.gang_workers.len().to_string());
    for worker in &options.gang_workers {
        update_repository_rule_cache_key(&mut hasher, &worker.capabilities.len().to_string());
        for (name, value) in worker.capabilities.iter() {
            update_repository_rule_cache_key(&mut hasher, name);
            update_repository_rule_cache_key(&mut hasher, value);
        }
    }
    match &options.custom_image {
        Some(image) => {
            update_repository_rule_cache_key(&mut hasher, "custom_image");
            update_repository_rule_cache_key(&mut hasher, &image.identifier.name);
            update_repository_rule_cache_key(&mut hasher, &image.identifier.uuid);
            update_repository_rule_cache_key(
                &mut hasher,
                &image.drop_host_mount_globs.len().to_string(),
            );
            for glob in &image.drop_host_mount_globs {
                update_repository_rule_cache_key(&mut hasher, glob);
            }
        }
        None => update_repository_rule_cache_key(&mut hasher, "no_custom_image"),
    }
    format!("remote:{}", blake3::Hasher::finalize(&hasher).to_hex())
}

pub async fn bzlmod_repository_rule_cache_info(
    ctx: &mut DiceComputations<'_>,
    invocation: &BazelRepositoryRuleInvocation,
) -> bz_error::Result<BazelRepositoryRuleCacheInfo> {
    let rule_path = match &invocation.rule_id.path {
        BzlOrBxlPath::Bzl(path) => path,
        BzlOrBxlPath::Bxl(_) => {
            return Err(BazelRepositoryError::RepositoryRuleBxlUnsupported(
                invocation.rule_id.to_string(),
            )
            .into());
        }
    };
    let rule_module = ctx
        .get_loaded_module(StarlarkModulePath::LoadFile(rule_path))
        .await?;
    let rule_value = rule_module
        .env()
        .get_any_visibility(&invocation.rule_id.name)
        .map(|(value, _)| value)
        .map_err(|e| bz_error::conversion::from_any_with_tag(e, bz_error::ErrorTag::Input))
        .or_else(|_| {
            Err(bz_error::Error::from(
                BazelRepositoryError::RepositoryRuleSymbolMissing {
                    path: rule_path.to_string(),
                    rule: invocation.rule_id.name.clone(),
                },
            ))
        })?;
    let repository_rule =
        repository_rule_from_loaded_module(rule_path, &invocation.rule_id.name, rule_value)?;
    let bzl_transitive_digest = bzlmod_bzl_transitive_digest(ctx, rule_path.clone()).await?;
    let repo_env = ctx.get_bzlmod_repository_environment().await?;
    let execution_cache_key =
        if repository_rule_should_use_remote_command_executor(&repo_env, repository_rule.remotable)
        {
            bzlmod_repository_rule_execution_cache_key(ctx.get_fallback_executor_config())
        } else {
            "local".to_owned()
        };

    let mut hasher = blake3::Hasher::new();
    update_repository_rule_cache_key(&mut hasher, "buck2-bzlmod-repository-rule-cache-v11");
    update_repository_rule_cache_key(&mut hasher, &execution_cache_key);
    update_repository_rule_cache_key(&mut hasher, &repository_os_name_value(&repo_env));
    update_repository_rule_cache_key(&mut hasher, &repository_os_arch_value(&repo_env));
    update_repository_rule_cache_key(&mut hasher, &invocation.rule_id.path.to_string());
    update_repository_rule_cache_key(&mut hasher, &invocation.rule_id.name);
    update_repository_rule_cache_key(&mut hasher, &bzl_transitive_digest);
    update_repository_rule_cache_key(&mut hasher, &invocation.name);
    update_repository_rule_cache_key(&mut hasher, &invocation.original_name);
    update_repository_rule_cache_key(&mut hasher, &invocation.attr_build_file_cell);
    update_repository_rule_cache_key_opt(
        &mut hasher,
        invocation.attr_build_file_package.as_deref(),
    );
    update_repository_rule_cache_key(&mut hasher, &invocation.attrs.len().to_string());
    for (name, value) in &invocation.attrs {
        update_repository_rule_cache_key(&mut hasher, name);
        update_repository_rule_cache_key(&mut hasher, value);
    }
    let environ = repository_rule.environ.iter().collect::<BTreeSet<_>>();
    update_repository_rule_cache_key(&mut hasher, &environ.len().to_string());
    for name in environ {
        update_repository_rule_cache_key(&mut hasher, name);
        update_repository_rule_cache_key_opt(&mut hasher, repo_env.get(name).map(|value| &**value));
    }

    Ok(BazelRepositoryRuleCacheInfo {
        predeclared_input_hash: blake3::Hasher::finalize(&hasher).to_hex().to_string(),
        local: repository_rule.local,
    })
}

async fn materialize_repository_rule_path_label_deps(
    ctx: &mut DiceComputations<'_>,
    label_deps: &[RepositoryPathLabelDep],
) -> bz_error::Result<()> {
    let mut seen = BTreeSet::new();
    for dep in label_deps {
        if !seen.insert(dep) {
            continue;
        }
        let cell_name = CellName::unchecked_new(&dep.cell_name)?;
        let should_materialize = {
            let cell_resolver = ctx.get_cell_resolver().await?;
            match cell_resolver.get(cell_name) {
                Ok(cell) => cell.external().is_some(),
                Err(_) => false,
            }
        };
        if !should_materialize {
            continue;
        }
        match &dep.path {
            Some(path) if dep.recursive => {
                materialize_repository_rule_path_label_dep_tree(ctx, cell_name, path).await?;
            }
            Some(path) => {
                let cell_path =
                    CellPath::new(cell_name, CellRelativePathBuf::try_from(path.to_owned())?);
                DiceFileComputations::read_path_metadata_if_exists(ctx, cell_path.as_ref()).await?;
            }
            None if dep.recursive => {
                materialize_repository_rule_path_label_dep_tree(ctx, cell_name, "").await?;
            }
            None => {
                let cell_root =
                    CellPath::new(cell_name, CellRelativePathBuf::unchecked_new(String::new()));
                DiceFileComputations::read_dir(ctx, cell_root.as_ref()).await?;
            }
        }
    }
    Ok(())
}

async fn materialize_repository_rule_path_label_dep_tree(
    ctx: &mut DiceComputations<'_>,
    cell_name: CellName,
    path: &str,
) -> bz_error::Result<()> {
    let root = CellPath::new(cell_name, CellRelativePathBuf::try_from(path.to_owned())?);
    let Some(metadata) =
        DiceFileComputations::read_path_metadata_if_exists(ctx, root.as_ref()).await?
    else {
        return Ok(());
    };
    if !matches!(metadata, RawPathMetadata::Directory) {
        return Ok(());
    }

    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        let entries = DiceFileComputations::read_dir(ctx, dir.as_ref()).await?;
        for entry in entries.included.iter() {
            let child = dir.join(&entry.file_name);
            if entry.file_type.is_dir() {
                dirs.push(child);
            } else {
                DiceFileComputations::read_path_metadata_if_exists(ctx, child.as_ref()).await?;
            }
        }
    }
    Ok(())
}

pub async fn evaluate_bzlmod_repository_rule_invocation(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodRepositoryRuleInvocationSetup,
    canonical_repo_name: &str,
    repository_ctx_working_dir: &str,
    cancellation: &CancellationContext,
) -> bz_error::Result<Vec<BazelRepositoryGeneratedFile>> {
    let invocation = bzlmod_repository_rule_invocation_from_setup(setup, canonical_repo_name)?;
    evaluate_bzlmod_repository_rule(
        ctx,
        &invocation,
        repository_ctx_working_dir,
        None,
        cancellation,
    )
    .await
}

pub fn bzlmod_repository_rule_invocation_from_setup(
    setup: &BzlmodRepositoryRuleInvocationSetup,
    canonical_repo_name: &str,
) -> bz_error::Result<BazelRepositoryRuleInvocation> {
    let rule_cell = CellName::unchecked_new(&setup.rule_bzl_cell)?;
    let rule_path = CellPath::new(
        rule_cell,
        CellRelativePathBuf::try_from(setup.rule_bzl_path.to_string())?,
    );
    let build_file_cell =
        BuildFileCell::new(CellName::unchecked_new(&setup.rule_bzl_build_file_cell)?);
    let rule_path = ImportPath::new_with_build_file_cells(rule_path, build_file_cell)?;
    Ok(BazelRepositoryRuleInvocation {
        rule_id: StarlarkRuleType {
            path: BzlOrBxlPath::Bzl(rule_path),
            name: setup.rule_name.to_string(),
        },
        name: canonical_repo_name.to_owned(),
        original_name: setup.repo_name.to_string(),
        attr_build_file_cell: setup.rule_bzl_build_file_cell.to_string(),
        attr_build_file_package: setup
            .rule_bzl_build_file_package
            .as_ref()
            .map(|package| package.to_string()),
        attrs: setup
            .attrs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    })
}

pub fn bzlmod_repository_rule_invocation_to_setup(
    invocation: &BazelRepositoryRuleInvocation,
) -> bz_error::Result<BzlmodRepositoryRuleInvocationSetup> {
    let BzlOrBxlPath::Bzl(rule_path) = &invocation.rule_id.path else {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "bzlmod repository rule invocation `{}` is not backed by a .bzl file",
            invocation.rule_id
        ));
    };
    Ok(BzlmodRepositoryRuleInvocationSetup {
        repo_name: Arc::from(invocation.original_name.as_str()),
        rule_bzl_cell: Arc::from(rule_path.path().cell().as_str()),
        rule_bzl_path: Arc::from(rule_path.path().path().as_str()),
        rule_bzl_build_file_cell: Arc::from(invocation.attr_build_file_cell.as_str()),
        rule_bzl_build_file_package: invocation
            .attr_build_file_package
            .as_ref()
            .map(|package| Arc::from(package.as_str())),
        rule_name: Arc::from(invocation.rule_id.name.as_str()),
        attrs: Arc::new(
            invocation
                .attrs
                .iter()
                .map(|(key, value)| (Arc::from(key.as_str()), Arc::from(value.as_str())))
                .collect(),
        ),
    })
}

pub(crate) fn module_extension_from_loaded_module(
    extension_module_path: &ImportPath,
    extension_name: &str,
    extension_value: starlark::values::OwnedFrozenValue,
) -> bz_error::Result<starlark::values::OwnedFrozenValueTyped<FrozenStarlarkModuleExtension>> {
    extension_value.downcast_starlark().map_err(|err| {
        let got = err.to_string();
        BazelRepositoryError::ModuleExtensionSymbolWrongType {
            path: extension_module_path.to_string(),
            extension: extension_name.to_owned(),
            got,
        }
        .into()
    })
}

pub(crate) fn repository_rule_from_loaded_module(
    rule_module_path: &ImportPath,
    rule_name: &str,
    rule_value: starlark::values::OwnedFrozenValue,
) -> bz_error::Result<starlark::values::OwnedFrozenValueTyped<FrozenStarlarkRepositoryRule>> {
    rule_value.downcast_starlark().map_err(|err| {
        let got = err.to_string();
        BazelRepositoryError::RepositoryRuleSymbolWrongType {
            path: rule_module_path.to_string(),
            rule: rule_name.to_owned(),
            got,
        }
        .into()
    })
}

fn eval_bzlmod_tag_expression<'v>(
    expression: &str,
    constants: &[(String, String)],
    value_name: &str,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let mut source = String::new();
    for (name, value) in constants {
        source.push_str(name);
        source.push_str(" = (");
        source.push_str(value);
        source.push_str(")\n");
    }
    source.push_str(&format!("{value_name} = ({expression})"));
    let filename = format!("<bzlmod module extension tag expression {value_name}>");
    let ast = AstModule::parse(&filename, source, &Dialect::AllOptionsInternal)?;
    eval.eval_module(ast, globals)?;
    eval.module()
        .get(value_name)
        .ok_or_else(|| {
            bz_error::Error::from(BazelRepositoryError::MissingEvaluatedTagExpression(
                value_name.to_owned(),
            ))
        })
        .map_err(Into::into)
}

fn alloc_coerced_attr_value_on_heap<'v>(
    value: &CoercedAttr,
    heap: Heap<'v>,
) -> starlark::Result<Value<'v>> {
    match value {
        CoercedAttr::Label(label)
        | CoercedAttr::SourceLabel(label)
        | CoercedAttr::Dep(label)
        | CoercedAttr::ConfigurationDep(label)
        | CoercedAttr::SplitTransitionDep(label) => {
            return Ok(heap.alloc(StarlarkProvidersLabel::new(label.clone())));
        }
        CoercedAttr::List(list) => {
            let values = list
                .iter()
                .map(|item| alloc_coerced_attr_value_on_heap(item, heap))
                .collect::<starlark::Result<Vec<_>>>()?;
            return Ok(heap.alloc(AllocList(values)));
        }
        CoercedAttr::Tuple(tuple) => {
            let values = tuple
                .iter()
                .map(|item| alloc_coerced_attr_value_on_heap(item, heap))
                .collect::<starlark::Result<Vec<_>>>()?;
            return Ok(heap.alloc(AllocList(values)));
        }
        CoercedAttr::Dict(dict) => {
            let values = dict
                .iter()
                .map(|(key, value)| {
                    Ok((
                        alloc_coerced_attr_value_on_heap(key, heap)?,
                        alloc_coerced_attr_value_on_heap(value, heap)?,
                    ))
                })
                .collect::<starlark::Result<Vec<_>>>()?;
            return Ok(heap.alloc(AllocDict(values)));
        }
        CoercedAttr::OneOf(value, _) => return alloc_coerced_attr_value_on_heap(value, heap),
        CoercedAttr::None => return Ok(Value::new_none()),
        _ => {}
    }
    let json = value
        .to_json(&AttrFmtContext::NO_CONTEXT)
        .map_err(starlark::Error::from)?;
    Ok(heap.alloc(json))
}

fn ensure_coerced_attr_value_allocable(value: &CoercedAttr) -> starlark::Result<()> {
    match value {
        CoercedAttr::Label(_)
        | CoercedAttr::SourceLabel(_)
        | CoercedAttr::Dep(_)
        | CoercedAttr::ConfigurationDep(_)
        | CoercedAttr::SplitTransitionDep(_)
        | CoercedAttr::None => Ok(()),
        CoercedAttr::List(list) => {
            for item in list.iter() {
                ensure_coerced_attr_value_allocable(item)?;
            }
            Ok(())
        }
        CoercedAttr::Tuple(tuple) => {
            for item in tuple.iter() {
                ensure_coerced_attr_value_allocable(item)?;
            }
            Ok(())
        }
        CoercedAttr::Dict(dict) => {
            for (key, value) in dict.iter() {
                ensure_coerced_attr_value_allocable(key)?;
                ensure_coerced_attr_value_allocable(value)?;
            }
            Ok(())
        }
        CoercedAttr::OneOf(value, _) => ensure_coerced_attr_value_allocable(value),
        _ => {
            value
                .to_json(&AttrFmtContext::NO_CONTEXT)
                .map_err(starlark::Error::from)?;
            Ok(())
        }
    }
}

fn alloc_coerced_attr_value<'v>(
    value: &CoercedAttr,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    alloc_coerced_attr_value_on_heap(value, eval.heap())
}

fn coerce_attr_value<'v>(
    attr_name: &str,
    attr: &Attribute,
    attr_coercion_ctx: &BuildAttrCoercionContext,
    raw_value: Value<'v>,
) -> starlark::Result<CoercedAttr> {
    let value = match attr
        .coerce(
            attr_name,
            AttrIsConfigurable::No,
            attr_coercion_ctx,
            raw_value,
        )
        .map_err(starlark::Error::from)?
    {
        CoercedValue::Custom(value) => value,
        CoercedValue::Default => {
            let default = attr.default().ok_or_else(|| {
                bz_error::bz_error!(
                    bz_error::ErrorTag::Tier0,
                    "attribute `{}` selected a default but has no default value",
                    attr_name
                )
            })?;
            default.as_ref().clone()
        }
    };
    ensure_coerced_attr_value_allocable(&value)?;
    Ok(value)
}

fn alloc_attr_value<'v>(
    attr_name: &str,
    attr: &Attribute,
    attr_coercion_ctx: &BuildAttrCoercionContext,
    raw_value: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let value = coerce_attr_value(attr_name, attr, attr_coercion_ctx, raw_value)?;
    alloc_coerced_attr_value(&value, eval)
}

fn bzlmod_module_cell_name(
    canonical_repo_name: &str,
    is_root: bool,
    eval: &Evaluator<'_, '_, '_>,
) -> bz_error::Result<CellName> {
    let cell_resolver = BuildContext::from_context(eval)?
        .cell_info()
        .cell_resolver();
    if is_root {
        return Ok(cell_resolver.root_cell());
    }
    if canonical_repo_name == "bazel_tools" {
        return CellName::unchecked_new("bazel_tools");
    }
    CellName::unchecked_new(&bzlmod_cell_name(canonical_repo_name))
}

fn bzlmod_module_attr_coercion_context(
    module_config: &BzlmodModuleExtensionModuleConfig,
    eval: &Evaluator<'_, '_, '_>,
) -> bz_error::Result<BuildAttrCoercionContext> {
    let build_context = BuildContext::from_context(eval)?;
    let cell_resolver = build_context.cell_info().cell_resolver().dupe();
    let cell_name = bzlmod_module_cell_name(
        &module_config.canonical_repo_name,
        module_config.is_root,
        eval,
    )?;
    let mut aliases = StdBuckHashMap::default();
    for alias in ["root", "prelude", "bazel_tools"] {
        let alias = NonEmptyCellAlias::new(alias.to_owned())?;
        let destination = CellName::unchecked_new(alias.as_str())?;
        cell_resolver.get(destination)?;
        aliases.insert(alias, destination);
    }
    for (alias, target_cell_name) in &module_config.cell_aliases {
        let alias = NonEmptyCellAlias::new(alias.to_owned())?;
        let destination = CellName::unchecked_new(target_cell_name.as_str())?;
        cell_resolver.get(destination)?;
        aliases.insert(alias, destination);
    }
    let cell_alias_resolver = CellAliasResolver::new(cell_name, aliases)?;
    Ok(BuildAttrCoercionContext::new_no_package(
        cell_resolver,
        cell_name,
        cell_alias_resolver,
        Arc::new(ConcurrentTargetLabelInterner::default()),
    ))
}

fn bzlmod_current_attr_coercion_context(
    eval: &Evaluator<'_, '_, '_>,
) -> bz_error::Result<BuildAttrCoercionContext> {
    let build_context = BuildContext::from_context(eval)?;
    Ok(BuildAttrCoercionContext::new_no_package(
        build_context.cell_info().cell_resolver().dupe(),
        build_context.cell_info().name().name(),
        build_context.cell_info().cell_alias_resolver().dupe(),
        Arc::new(ConcurrentTargetLabelInterner::default()),
    ))
}

fn bzlmod_repository_rule_attr_coercion_context(
    invocation: &BazelRepositoryRuleInvocation,
    eval: &Evaluator<'_, '_, '_>,
) -> bz_error::Result<BuildAttrCoercionContext> {
    let build_context = BuildContext::from_context(eval)?;
    let cell_resolver = build_context.cell_info().cell_resolver().dupe();
    let BzlOrBxlPath::Bzl(rule_path) = &invocation.rule_id.path else {
        return bzlmod_current_attr_coercion_context(eval);
    };
    let cell_name = CellName::unchecked_new(&invocation.attr_build_file_cell)
        .unwrap_or_else(|_| rule_path.build_file_cell().name());
    let cell_alias_resolver =
        bzlmod_repository_rule_cell_alias_resolver(&cell_resolver, cell_name)?;
    if let Some(package) = &invocation.attr_build_file_package {
        let package = PackageLabel::new(cell_name, CellRelativePath::from_path(package)?)?;
        return Ok(BuildAttrCoercionContext::new_with_package(
            cell_resolver,
            cell_alias_resolver,
            (
                package.dupe(),
                PackageListing::empty(FileNameBuf::unchecked_new("BUILD.bazel")),
            ),
            false,
            Arc::new(ConcurrentTargetLabelInterner::default()),
            CellPathWithAllowedRelativeDir::backwards_relative_not_supported(
                package.to_cell_path(),
            ),
        ));
    }
    Ok(BuildAttrCoercionContext::new_no_package(
        cell_resolver,
        cell_name,
        cell_alias_resolver,
        Arc::new(ConcurrentTargetLabelInterner::default()),
    ))
}

fn bzlmod_repository_rule_cell_alias_resolver(
    cell_resolver: &bz_core::cells::CellResolver,
    cell_name: CellName,
) -> bz_error::Result<CellAliasResolver> {
    if cell_name == cell_resolver.root_cell() {
        return Ok(cell_resolver.root_cell_cell_alias_resolver().dupe());
    }

    let mut aliases = StdBuckHashMap::default();
    for alias in ["root", "prelude", "bazel_tools"] {
        let alias = NonEmptyCellAlias::new(alias.to_owned())?;
        let destination = if alias.as_str() == "root" {
            cell_resolver.root_cell()
        } else {
            CellName::unchecked_new(alias.as_str())?
        };
        if cell_resolver.get(destination).is_ok() {
            aliases.insert(alias, destination);
        }
    }
    for (alias, destination) in bzlmod_cell_aliases_for_cell(cell_name.as_str()) {
        aliases.insert(
            NonEmptyCellAlias::new(alias)?,
            CellName::unchecked_new(destination.as_str())?,
        );
    }
    CellAliasResolver::new(cell_name, aliases)
}

pub(crate) fn alloc_bzlmod_module_extension_context<'v>(
    extension: &FrozenStarlarkModuleExtension,
    extension_usages_json: &str,
    working_dir: &str,
    repo_env: Arc<BTreeMap<String, String>>,
    recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>>,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let config: BzlmodModuleExtensionEvaluationConfig = serde_json::from_str(extension_usages_json)
        .map_err(|e| {
            bz_error::Error::from(BazelRepositoryError::InvalidModuleExtensionUsageData)
                .context(format!("JSON parse error: {e}"))
        })?;
    let extension_id = extension.id()?;
    let tag_classes = extension.tag_classes();
    let tag_class_names = tag_classes.keys().cloned().collect::<Vec<_>>();

    let mut expression_index = 0usize;
    let mut sort_key = 0i32;
    let mut modules = Vec::new();
    for module_config in config.modules {
        let attr_coercion_ctx = bzlmod_module_attr_coercion_context(&module_config, eval)
            .map_err(starlark::Error::from)?;
        let mut tags = SmallMap::new();
        for tag_class_name in &tag_class_names {
            tags.insert(tag_class_name.clone(), Vec::new());
        }

        for tag_config in module_config.tags {
            let mut expression_bindings = module_config.constants.clone();
            expression_bindings.extend(tag_config.bindings.iter().cloned());
            let tag_class_value = tag_classes.get(&tag_config.tag_name).ok_or_else(|| {
                bz_error::Error::from(BazelRepositoryError::UnknownModuleExtensionTag {
                    extension: extension_id.to_string(),
                    tag: tag_config.tag_name.clone(),
                })
            })?;
            let tag_class = tag_class_value
                .to_value()
                .downcast_ref::<FrozenStarlarkTagClass>()
                .ok_or_else(|| {
                    bz_error::Error::from(BazelRepositoryError::InvalidFrozenTagClass {
                        tag: tag_config.tag_name.clone(),
                        got: tag_class_value.to_value().get_type().to_owned(),
                    })
                })?;
            let mut explicit_attrs = tag_config
                .kwargs
                .into_iter()
                .collect::<BTreeMap<String, String>>();
            let mut attrs = SmallMap::new();
            for (attr_name, attr) in tag_class.attributes() {
                let value = match explicit_attrs.remove(attr_name) {
                    Some(expression) => {
                        let value_name = format!("buck_bzlmod_tag_value_{expression_index}");
                        expression_index += 1;
                        let raw_value = eval_bzlmod_tag_expression(
                            &expression,
                            &expression_bindings,
                            &value_name,
                            globals,
                            eval,
                        )?;
                        alloc_attr_value(attr_name, attr, &attr_coercion_ctx, raw_value, eval)?
                    }
                    None => match attr.default() {
                        Some(default) => alloc_coerced_attr_value(default, eval)?,
                        None => {
                            return Err(bz_error::Error::from(
                                BazelRepositoryError::MissingModuleExtensionTagAttribute {
                                    tag: tag_config.tag_name.clone(),
                                    attr: attr_name.clone(),
                                },
                            )
                            .into());
                        }
                    },
                };
                attrs.insert(attr_name.clone(), value);
            }
            if let Some((attr, _)) = explicit_attrs.into_iter().next() {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "module extension tag `{}` has unknown attribute `{}`",
                    tag_config.tag_name,
                    attr
                )
                .into());
            }

            let tag_value = eval.heap().alloc(StarlarkBazelModuleTag::new(
                tag_config.tag_name.clone(),
                tag_config.dev_dependency,
                sort_key,
                attrs,
            ));
            sort_key += 1;
            tags.entry(tag_config.tag_name)
                .or_insert_with(Vec::new)
                .push(tag_value);
        }

        let tags = tags
            .into_iter()
            .map(|(name, values)| (name, eval.heap().alloc(AllocList(values))))
            .collect();
        let tags_value = eval.heap().alloc(StarlarkBazelModuleTags::new(tags));
        let module_value = eval.heap().alloc(StarlarkBazelModule::new(
            module_config.name,
            module_config.version,
            tags_value,
            module_config.is_root,
        ));
        modules.push(module_value);
    }
    let modules = eval.heap().alloc(AllocList(modules));

    let module_ctx = StarlarkModuleExtensionContext::new(
        modules,
        working_dir.to_owned(),
        config.root_module_has_non_dev_dependency,
        repo_env.clone(),
        recorded_inputs,
        if repository_ctx_should_search_local_path(&repo_env) {
            BazelRepositoryCommandExecutor::Local
        } else {
            BuildContext::from_context(eval)?
                .bazel_repository_context
                .as_ref()
                .map(|context| context.command_executor.clone())
                .unwrap_or(BazelRepositoryCommandExecutor::Local)
        },
        BuildContext::from_context(eval)?
            .bazel_repository_context
            .as_ref()
            .and_then(|context| context.remote_downloader.clone()),
    );
    for name in &extension.environ {
        record_repository_env_var(&module_ctx.repo_env, &module_ctx.recorded_inputs, name);
    }
    Ok(eval.heap().alloc(module_ctx))
}

pub(crate) fn alloc_bzlmod_repository_context<'v>(
    repository_rule: &FrozenStarlarkRepositoryRule,
    invocation: &BazelRepositoryRuleInvocation,
    working_dir: &str,
    repo_env: Arc<BTreeMap<String, String>>,
    recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>>,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let mut expression_index = 0usize;
    let mut explicit_attrs = invocation
        .attrs
        .iter()
        .cloned()
        .collect::<BTreeMap<String, String>>();
    let attr_coercion_ctx = bzlmod_repository_rule_attr_coercion_context(invocation, eval)
        .map_err(starlark::Error::from)?;
    let mut attrs = SmallMap::new();
    for (attr_name, attr) in repository_rule.attributes.attributes() {
        let value = match explicit_attrs.remove(attr_name) {
            Some(expression) => {
                let value_name = format!("buck_repository_rule_attr_{expression_index}");
                expression_index += 1;
                let raw_value =
                    eval_bzlmod_tag_expression(&expression, &[], &value_name, globals, eval)?;
                coerce_attr_value(attr_name, attr, &attr_coercion_ctx, raw_value)?
            }
            None => match attr.default() {
                Some(default) => {
                    let value = default.as_ref().clone();
                    ensure_coerced_attr_value_allocable(&value)?;
                    value
                }
                None => {
                    return Err(bz_error::bz_error!(
                        bz_error::ErrorTag::Input,
                        "repository_rule `{}` invocation `{}` is missing required attribute `{}`",
                        invocation.rule_id,
                        invocation.name,
                        attr_name
                    )
                    .into());
                }
            },
        };
        attrs.insert(attr_name.clone(), value);
    }
    if let Some((attr, _)) = explicit_attrs.into_iter().next() {
        return Err(bz_error::Error::from(
            BazelRepositoryError::RepositoryRuleUnknownAttribute {
                rule: invocation.rule_id.to_string(),
                attr,
            },
        )
        .into());
    }
    let attrs = BazelRepositoryAttrValues {
        attrs,
        name: invocation.name.clone(),
    };
    let command_executor =
        repository_rule_command_executor(&repo_env, repository_rule.remotable, eval)?;
    let repository_ctx = StarlarkRepositoryContext::new(
        invocation.name.clone(),
        invocation.original_name.clone(),
        attrs,
        working_dir.to_owned(),
        repository_ctx_workspace_root(working_dir),
        repo_env,
        recorded_inputs,
        command_executor,
        BuildContext::from_context(eval)?
            .bazel_repository_context
            .as_ref()
            .and_then(|context| context.remote_downloader.clone()),
    );
    for name in &repository_rule.environ {
        record_repository_env_var(
            &repository_ctx.repo_env,
            &repository_ctx.recorded_inputs,
            name,
        );
    }
    Ok(eval.heap().alloc(repository_ctx))
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkRepositoryContext<'v> {
    name: String,
    original_name: String,
    attrs: BazelRepositoryAttrValues,
    working_dir: String,
    workspace_root: String,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    repo_env: Arc<BTreeMap<String, String>>,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    files: Mutex<Vec<BazelRepositoryGeneratedFile>>,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    path_label_deps: Mutex<Vec<RepositoryPathLabelDep>>,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>>,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    command_executor: BazelRepositoryCommandExecutor,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    remote_downloader: Option<BazelRepositoryRemoteDownloaderConfig>,
    _marker: std::marker::PhantomData<&'v ()>,
}

impl<'v> StarlarkRepositoryContext<'v> {
    fn new(
        name: String,
        original_name: String,
        attrs: BazelRepositoryAttrValues,
        working_dir: String,
        workspace_root: String,
        repo_env: Arc<BTreeMap<String, String>>,
        recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>>,
        command_executor: BazelRepositoryCommandExecutor,
        remote_downloader: Option<BazelRepositoryRemoteDownloaderConfig>,
    ) -> Self {
        Self {
            name,
            original_name,
            attrs,
            working_dir,
            workspace_root,
            repo_env,
            files: Mutex::new(Vec::new()),
            path_label_deps: Mutex::new(Vec::new()),
            recorded_inputs,
            command_executor,
            remote_downloader,
            _marker: std::marker::PhantomData,
        }
    }

    fn take_files(&self) -> Vec<BazelRepositoryGeneratedFile> {
        std::mem::take(&mut *self.files.lock().expect("repository_ctx files poisoned"))
    }

    fn take_path_label_deps(&self) -> Vec<RepositoryPathLabelDep> {
        std::mem::take(
            &mut *self
                .path_label_deps
                .lock()
                .expect("repository_ctx path label deps poisoned"),
        )
    }

    fn take_recorded_inputs(&self) -> Vec<BazelRepositoryRecordedInput> {
        std::mem::take(
            &mut *self
                .recorded_inputs
                .lock()
                .expect("repository_ctx recorded inputs poisoned"),
        )
    }
}

impl<'v> Display for StarlarkRepositoryContext<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository_ctx {}>", self.name)
    }
}

impl<'v> AllocValue<'v> for StarlarkRepositoryContext<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "repository_ctx")]
impl<'v> StarlarkValue<'v> for StarlarkRepositoryContext<'v> {
    fn dir_attr(&self) -> Vec<String> {
        vec![
            "attr".to_owned(),
            "name".to_owned(),
            "original_name".to_owned(),
            "os".to_owned(),
            "workspace_root".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "attr" => Some(self.attrs.alloc(heap)),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "original_name" => Some(heap.alloc_str(&self.original_name).to_value()),
            "os" => Some(heap.alloc(StarlarkRepositoryOs::new(
                self.repo_env.clone(),
                self.recorded_inputs.clone(),
            ))),
            "workspace_root" => {
                Some(heap.alloc(StarlarkRepositoryPath::new(self.workspace_root.clone())))
            }
            _ => None,
        }
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self>(repository_context_methods)
    }
}

impl<'v> Freeze for StarlarkRepositoryContext<'v> {
    type Frozen = FrozenStarlarkRepositoryContext;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkRepositoryContext {
            name: self.name,
            original_name: self.original_name,
            attrs: self.attrs,
            working_dir: self.working_dir,
            workspace_root: self.workspace_root,
            repo_env: self.repo_env,
            files: Mutex::new(
                self.files
                    .into_inner()
                    .expect("repository_ctx files poisoned"),
            ),
            path_label_deps: Mutex::new(
                self.path_label_deps
                    .into_inner()
                    .expect("repository_ctx path label deps poisoned"),
            ),
            recorded_inputs: self.recorded_inputs,
            command_executor: self.command_executor,
            remote_downloader: self.remote_downloader,
        })
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct FrozenStarlarkRepositoryContext {
    name: String,
    original_name: String,
    attrs: BazelRepositoryAttrValues,
    working_dir: String,
    workspace_root: String,
    #[allocative(skip)]
    repo_env: Arc<BTreeMap<String, String>>,
    #[allocative(skip)]
    files: Mutex<Vec<BazelRepositoryGeneratedFile>>,
    #[allocative(skip)]
    path_label_deps: Mutex<Vec<RepositoryPathLabelDep>>,
    #[allocative(skip)]
    recorded_inputs: Arc<Mutex<Vec<BazelRepositoryRecordedInput>>>,
    #[allocative(skip)]
    command_executor: BazelRepositoryCommandExecutor,
    #[allocative(skip)]
    remote_downloader: Option<BazelRepositoryRemoteDownloaderConfig>,
}

impl Display for FrozenStarlarkRepositoryContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository_ctx {}>", self.name)
    }
}

starlark_simple_value!(FrozenStarlarkRepositoryContext);

#[starlark_value(type = "repository_ctx")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkRepositoryContext {
    type Canonical = StarlarkRepositoryContext<'v>;

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "attr".to_owned(),
            "name".to_owned(),
            "original_name".to_owned(),
            "os".to_owned(),
            "workspace_root".to_owned(),
        ]
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "attr" => Some(self.attrs.alloc(heap)),
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "original_name" => Some(heap.alloc_str(&self.original_name).to_value()),
            "os" => Some(heap.alloc(FrozenStarlarkRepositoryOs::new(
                self.repo_env.clone(),
                self.recorded_inputs.clone(),
            ))),
            "workspace_root" => {
                Some(heap.alloc(StarlarkRepositoryPath::new(self.workspace_root.clone())))
            }
            _ => None,
        }
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(repository_context_methods)
    }
}

fn repository_ctx_output_path_from_value(
    value: Value<'_>,
    working_dir: &str,
) -> starlark::Result<String> {
    if let Some(path) = value.downcast_ref::<StarlarkRepositoryPath>() {
        return repository_ctx_output_path_from_resolved_path(&path.path, working_dir);
    }
    if let Some(path) = value.unpack_str() {
        return repository_ctx_output_path_from_relative_path(path, working_dir);
    }
    Err(bz_error::Error::from(
        BazelRepositoryError::RepositoryCtxOutputPathUnsupportedValue(value.get_type().to_owned()),
    )
    .into())
}

fn repository_ctx_output_path_from_value_relative_to(
    value: Value<'_>,
    eval: &Evaluator<'_, '_, '_>,
    working_dir: &str,
) -> starlark::Result<String> {
    if let Some(path) = value.unpack_str() {
        return repository_ctx_output_path_from_relative_path(path, working_dir);
    }
    let path = repository_path_from_value_relative_to(value, eval, Some(working_dir))?;
    repository_ctx_output_path_from_resolved_path(&path, working_dir)
}

fn repository_ctx_output_abs_path_from_value_relative_to(
    value: Value<'_>,
    eval: &Evaluator<'_, '_, '_>,
    working_dir: &str,
) -> starlark::Result<String> {
    let relative_path =
        repository_ctx_output_path_from_value_relative_to(value, eval, working_dir)?;
    Ok(Path::new(working_dir)
        .join(relative_path)
        .to_string_lossy()
        .into_owned())
}

fn repository_ctx_output_path_from_resolved_path(
    path: &str,
    working_dir: &str,
) -> starlark::Result<String> {
    if path == working_dir {
        return Ok(String::new());
    }

    let prefix = format!("{working_dir}/");
    if let Some(path) = path.strip_prefix(&prefix) {
        return repository_ctx_output_path_from_relative_path(path, working_dir);
    }

    let path_buf = Path::new(path);
    let working_dir_buf = Path::new(working_dir);
    if path_buf.is_absolute()
        && working_dir_buf.is_absolute()
        && path_buf.starts_with(working_dir_buf)
    {
        let relative = path_buf.strip_prefix(working_dir_buf).map_err(|_| {
            bz_error::Error::from(
                BazelRepositoryError::RepositoryCtxOutputPathOutsideRepository {
                    path: path.to_owned(),
                    working_dir: working_dir.to_owned(),
                },
            )
        })?;
        let relative = relative.to_string_lossy();
        return repository_ctx_output_path_from_relative_path(relative.as_ref(), working_dir);
    }

    Err(bz_error::Error::from(
        BazelRepositoryError::RepositoryCtxOutputPathOutsideRepository {
            path: path.to_owned(),
            working_dir: working_dir.to_owned(),
        },
    )
    .into())
}

fn repository_ctx_output_path_from_relative_path(
    path: &str,
    working_dir: &str,
) -> starlark::Result<String> {
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(bz_error::Error::from(
                        BazelRepositoryError::RepositoryCtxOutputPathOutsideRepository {
                            path: path.to_owned(),
                            working_dir: working_dir.to_owned(),
                        },
                    )
                    .into());
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(bz_error::Error::from(
                    BazelRepositoryError::RepositoryCtxOutputPathOutsideRepository {
                        path: path.to_owned(),
                        working_dir: working_dir.to_owned(),
                    },
                )
                .into());
            }
        }
    }
    Ok(normalized.to_string_lossy().into_owned())
}

pub(crate) fn take_repository_ctx_files<'v>(
    repository_ctx: Value<'v>,
) -> starlark::Result<Vec<BazelRepositoryGeneratedFile>> {
    let repository_ctx = repository_ctx
        .downcast_ref::<StarlarkRepositoryContext<'v>>()
        .ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "expected repository_ctx, got `{}`",
                repository_ctx.get_type()
            )
        })?;
    repository_ctx_current_generated_files(&repository_ctx.working_dir, repository_ctx.take_files())
}

fn repository_ctx_current_generated_files(
    working_dir: &str,
    files: Vec<BazelRepositoryGeneratedFile>,
) -> starlark::Result<Vec<BazelRepositoryGeneratedFile>> {
    let mut seen = BTreeSet::new();
    let mut refreshed = Vec::new();
    for file in files.into_iter().rev() {
        if !seen.insert(file.path.clone()) {
            continue;
        }
        let path = Path::new(working_dir).join(&file.path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Tier0,
                    "repository_ctx could not stat generated file `{}`: {}",
                    path.to_string_lossy(),
                    error
                )
                .into());
            }
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let content = match fs::read(&path) {
            Ok(content) => content,
            Err(error) => {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Tier0,
                    "repository_ctx could not read generated file `{}`: {}",
                    path.to_string_lossy(),
                    error
                )
                .into());
            }
        };
        let Ok(content) = String::from_utf8(content) else {
            continue;
        };
        refreshed.push(BazelRepositoryGeneratedFile {
            path: file.path,
            content,
            executable: repository_path_is_executable(&path),
        });
    }
    refreshed.reverse();
    Ok(refreshed)
}

pub(crate) fn take_repository_ctx_path_label_deps<'v>(
    repository_ctx: Value<'v>,
) -> starlark::Result<Vec<RepositoryPathLabelDep>> {
    let repository_ctx = repository_ctx
        .downcast_ref::<StarlarkRepositoryContext>()
        .ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "expected repository_ctx, got `{}`",
                repository_ctx.get_type()
            )
        })?;
    Ok(repository_ctx.take_path_label_deps())
}

pub(crate) fn take_repository_ctx_recorded_inputs<'v>(
    repository_ctx: Value<'v>,
) -> starlark::Result<Vec<BazelRepositoryRecordedInput>> {
    let repository_ctx = repository_ctx
        .downcast_ref::<StarlarkRepositoryContext>()
        .ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "expected repository_ctx, got `{}`",
                repository_ctx.get_type()
            )
        })?;
    Ok(repository_ctx.take_recorded_inputs())
}

pub(crate) fn take_module_ctx_path_label_deps<'v>(
    module_ctx: Value<'v>,
) -> starlark::Result<Vec<RepositoryPathLabelDep>> {
    let module_ctx = module_ctx
        .downcast_ref::<StarlarkModuleExtensionContext>()
        .ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "expected module_ctx, got `{}`",
                module_ctx.get_type()
            )
        })?;
    Ok(module_ctx.take_path_label_deps())
}

pub(crate) fn take_module_ctx_recorded_inputs<'v>(
    module_ctx: Value<'v>,
) -> starlark::Result<Vec<BazelRepositoryRecordedInput>> {
    let module_ctx = module_ctx
        .downcast_ref::<StarlarkModuleExtensionContext>()
        .ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "expected module_ctx, got `{}`",
                module_ctx.get_type()
            )
        })?;
    Ok(module_ctx.take_recorded_inputs())
}

fn repository_ctx_working_dir<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
) -> &'v str {
    match this.unpack() {
        either::Either::Left(ctx) => &ctx.working_dir,
        either::Either::Right(ctx) => &ctx.working_dir,
    }
}

fn repository_ctx_repo_env<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
) -> Arc<BTreeMap<String, String>> {
    match this.unpack() {
        either::Either::Left(ctx) => ctx.repo_env.clone(),
        either::Either::Right(ctx) => ctx.repo_env.clone(),
    }
}

fn repository_ctx_recorded_inputs<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
) -> Arc<Mutex<Vec<BazelRepositoryRecordedInput>>> {
    match this.unpack() {
        either::Either::Left(ctx) => ctx.recorded_inputs.clone(),
        either::Either::Right(ctx) => ctx.recorded_inputs.clone(),
    }
}

fn repository_ctx_command_executor<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
) -> BazelRepositoryCommandExecutor {
    match this.unpack() {
        either::Either::Left(ctx) => ctx.command_executor.clone(),
        either::Either::Right(ctx) => ctx.command_executor.clone(),
    }
}

fn repository_ctx_remote_downloader<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
) -> Option<BazelRepositoryRemoteDownloaderConfig> {
    match this.unpack() {
        either::Either::Left(ctx) => ctx.remote_downloader.clone(),
        either::Either::Right(ctx) => ctx.remote_downloader.clone(),
    }
}

fn repository_ctx_record_path_dep<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
    dep: RepositoryPathLabelDep,
) {
    match this.unpack() {
        either::Either::Left(ctx) => {
            let mut deps = ctx
                .path_label_deps
                .lock()
                .expect("repository_ctx path label deps poisoned");
            if !deps.iter().any(|existing| existing == &dep) {
                deps.push(dep);
            }
        }
        either::Either::Right(ctx) => {
            let mut deps = ctx
                .path_label_deps
                .lock()
                .expect("repository_ctx path label deps poisoned");
            if !deps.iter().any(|existing| existing == &dep) {
                deps.push(dep);
            }
        }
    }
}

fn repository_ctx_path_from_value_relative_to<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
    path: Value<'v>,
    eval: &Evaluator<'v, '_, '_>,
) -> starlark::Result<String> {
    let (path, dep) = repository_path_and_dep_from_value_relative_to(
        path,
        eval,
        Some(repository_ctx_working_dir(this)),
    )?;
    if let Some(dep) = dep {
        repository_ctx_record_path_dep(this, dep);
    }
    Ok(path)
}

fn module_ctx_record_path_dep<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
    dep: RepositoryPathLabelDep,
) {
    match this.unpack() {
        either::Either::Left(ctx) => {
            let mut deps = ctx
                .path_label_deps
                .lock()
                .expect("module_ctx path label deps poisoned");
            if !deps.iter().any(|existing| existing == &dep) {
                deps.push(dep);
            }
        }
        either::Either::Right(ctx) => {
            let mut deps = ctx
                .path_label_deps
                .lock()
                .expect("module_ctx path label deps poisoned");
            if !deps.iter().any(|existing| existing == &dep) {
                deps.push(dep);
            }
        }
    }
}

fn module_ctx_path_from_value_relative_to<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
    path: Value<'v>,
    eval: &Evaluator<'v, '_, '_>,
) -> starlark::Result<String> {
    let (path, dep) = repository_path_and_dep_from_value_relative_to(
        path,
        eval,
        Some(module_ctx_working_dir(this)),
    )?;
    if let Some(dep) = dep {
        module_ctx_record_path_dep(this, dep);
    }
    Ok(path)
}

fn repository_ctx_clean_working_dir(working_dir: &str) -> bz_error::Result<()> {
    let working_dir = repository_path_for_write(working_dir)?;
    if working_dir.exists() {
        fs::remove_dir_all(&working_dir).map_err(|error| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "repository_ctx could not clean `{}` before retry: {}",
                working_dir.to_string_lossy(),
                error
            )
        })?;
    }
    fs::create_dir_all(&working_dir).map_err(|error| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Tier0,
            "repository_ctx could not create `{}` before retry: {}",
            working_dir.to_string_lossy(),
            error
        )
    })
}

fn repository_ctx_patch_strip(strip: i32, patch: &str) -> bz_error::Result<u32> {
    u32::try_from(strip).map_err(|_| {
        bz_error::Error::from(BazelRepositoryError::RepositoryCtxPatch {
            patch: patch.to_owned(),
            error: format!("strip must be non-negative, got `{strip}`"),
        })
    })
}

fn repository_ctx_push_file<'v>(
    this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
    file: BazelRepositoryGeneratedFile,
) {
    match this.unpack() {
        either::Either::Left(ctx) => ctx
            .files
            .lock()
            .expect("repository_ctx files poisoned")
            .push(file),
        either::Either::Right(ctx) => ctx
            .files
            .lock()
            .expect("repository_ctx files poisoned")
            .push(file),
    }
}

fn repository_ctx_write_bytes(path: &str, bytes: &[u8], executable: bool) -> starlark::Result<()> {
    let write_path = repository_path_for_write(path)?;
    if let Some(parent) = write_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            bz_error::Error::from(BazelRepositoryError::RepositoryCtxWriteFile {
                path: write_path.to_string_lossy().into_owned(),
                error: e.to_string(),
            })
        })?;
    }
    fs::write(&write_path, bytes).map_err(|e| {
        bz_error::Error::from(BazelRepositoryError::RepositoryCtxWriteFile {
            path: write_path.to_string_lossy().into_owned(),
            error: e.to_string(),
        })
    })?;
    if executable {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&write_path, fs::Permissions::from_mode(0o755)).map_err(|e| {
                bz_error::Error::from(BazelRepositoryError::RepositoryCtxWriteFile {
                    path: write_path.to_string_lossy().into_owned(),
                    error: e.to_string(),
                })
            })?;
        }
    }
    Ok(())
}

fn repository_path_is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn repository_ctx_which_local_path(path: &str, program: &str) -> Option<String> {
    for dir in env::split_paths(std::ffi::OsStr::new(path)) {
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(program);
        if repository_path_is_executable(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(unix)]
fn repository_ctx_create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn repository_ctx_create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

fn repository_ctx_command_arg(
    value: Value<'_>,
    working_dir: &str,
    eval: &Evaluator<'_, '_, '_>,
) -> starlark::Result<String> {
    if let Some(path) = value.downcast_ref::<StarlarkRepositoryPath>() {
        return Ok(repository_ctx_command_path_object(&path.path, working_dir));
    }
    if StarlarkProvidersLabel::from_value(value).is_some() {
        let (path, _dep) =
            repository_path_and_dep_from_value_relative_to(value, eval, Some(working_dir))?;
        return Ok(repository_ctx_command_path_object(&path, working_dir));
    }
    if let Some(path) = value.unpack_str() {
        return Ok(repository_ctx_command_path(path, working_dir));
    }
    Ok(value.to_string())
}

fn repository_ctx_command_progress(program: &str, args: &[String]) -> String {
    let display_program = repository_ctx_unwrapped_env_command(program, args).unwrap_or(program);
    format!("running `{display_program}`")
}

fn repository_ctx_unwrapped_env_command<'a>(
    program: &'a str,
    args: &'a [String],
) -> Option<&'a str> {
    if !repository_ctx_program_is_env(program) {
        return None;
    }

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "-i" | "-" | "--ignore-environment" => {
                index += 1;
            }
            "--" => {
                index += 1;
                break;
            }
            "-u" | "--unset" => {
                index += 2;
            }
            _ if repository_ctx_env_assignment(arg) => {
                index += 1;
            }
            _ => break,
        }
    }

    args.get(index)
        .map(String::as_str)
        .filter(|arg| !arg.is_empty())
}

fn repository_ctx_program_is_env(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        == Some("env")
}

fn repository_ctx_env_assignment(arg: &str) -> bool {
    let Some((key, _value)) = arg.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn repository_ctx_command_path_object(path: &str, working_dir: &str) -> String {
    if let Some(path) = repository_ctx_command_external_path(path, working_dir) {
        return path;
    }
    repository_path_for_read_abs_relative_to(path, working_dir)
        .to_string_lossy()
        .into_owned()
}

fn repository_ctx_command_env(value: &str, working_dir: &str) -> String {
    repository_ctx_command_path(value, working_dir)
}

fn repository_ctx_command_path(path: &str, working_dir: &str) -> String {
    if let Some(path) = repository_ctx_command_assignment_path(path, working_dir) {
        return path;
    }
    if let Some(path) = repository_ctx_command_external_path(path, working_dir) {
        return path;
    }
    path.to_owned()
}

fn repository_ctx_command_assignment_path(path: &str, working_dir: &str) -> Option<String> {
    if let Some(path) =
        repository_ctx_command_assignment_path_with_split(working_dir, path.split_once('='))
    {
        return Some(path);
    }
    repository_ctx_command_assignment_path_with_split(working_dir, path.rsplit_once('='))
}

fn repository_ctx_command_assignment_path_with_split(
    working_dir: &str,
    split: Option<(&str, &str)>,
) -> Option<String> {
    let (prefix, value) = split?;
    if prefix.is_empty() || prefix.contains('/') || prefix.contains('\\') {
        return None;
    }
    if !repository_ctx_command_assignment_value_is_plain_path(value) {
        return None;
    }
    let value = repository_ctx_command_external_path(value, working_dir)?;
    Some(format!("{prefix}={value}"))
}

fn repository_ctx_command_assignment_value_is_plain_path(value: &str) -> bool {
    Path::new(value).is_absolute() || value.starts_with("buck-out/")
}

fn repository_ctx_command_external_path(path: &str, working_dir: &str) -> Option<String> {
    let suffix = repository_external_cell_suffix(path)?;
    let path = repository_external_cell_existing_path_relative_to(suffix, working_dir)
        .or_else(|| repository_external_cell_path_relative_to(suffix, working_dir))?;
    Some(path.to_string_lossy().into_owned())
}

fn repository_ctx_command_external_input_path(
    value: &str,
    repository_working_dir: &Path,
) -> Option<PathBuf> {
    if !Path::new(value).is_absolute() {
        return None;
    }
    if !value.contains("/external_cells/") {
        return None;
    }
    let path = PathBuf::from(value);
    if path == repository_working_dir || path.starts_with(repository_working_dir) {
        return None;
    }
    Some(path)
}

fn repository_ctx_validate_external_inputs_ready(
    values: impl IntoIterator<Item = String>,
    repository_working_dir: &Path,
    program: &str,
    mut record_dep: impl FnMut(RepositoryPathLabelDep),
) -> starlark::Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        let Some(path) = repository_ctx_command_external_input_path(&value, repository_working_dir)
        else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        if !repository_ctx_external_input_ready(&path) {
            if let Some(dep) = repository_ctx_external_input_dep(&path) {
                record_dep(dep);
            }
            return Err(bz_error::Error::from(
                BazelRepositoryError::RepositoryCtxExecuteFailed {
                    program: program.to_owned(),
                    error: format!(
                        "external input `{}` was not materialized",
                        path.to_string_lossy()
                    ),
                },
            )
            .into());
        }
    }
    Ok(())
}

fn repository_ctx_external_input_dep(path: &Path) -> Option<RepositoryPathLabelDep> {
    repository_ctx_external_input_dep_impl(path, false)
}

fn repository_ctx_external_input_tree_dep(path: &Path) -> Option<RepositoryPathLabelDep> {
    repository_ctx_external_input_dep_impl(path, true)
}

fn repository_ctx_external_input_dep_impl(
    path: &Path,
    recursive: bool,
) -> Option<RepositoryPathLabelDep> {
    let path = path.to_string_lossy();
    let suffix = path
        .split_once("/external_cells/bzlmod_generated/")
        .map(|(_, suffix)| suffix)
        .or_else(|| {
            path.split_once("/external_cells/bzlmod/")
                .map(|(_, suffix)| suffix)
        })?;
    let (canonical_repo_name, repo_path) = suffix.split_once('/').unwrap_or((suffix, ""));
    if canonical_repo_name.ends_with(".repository_ctx") {
        return None;
    }
    let cell_name = bzlmod_cell_name(canonical_repo_name);
    if recursive {
        Some(RepositoryPathLabelDep::tree(
            cell_name,
            (!repo_path.is_empty()).then(|| repo_path.to_owned()),
        ))
    } else if repo_path.is_empty() {
        Some(RepositoryPathLabelDep::cell(cell_name))
    } else {
        Some(RepositoryPathLabelDep::cell_path(
            cell_name,
            repo_path.to_owned(),
        ))
    }
}

fn repository_ctx_external_input_ready(path: &Path) -> bool {
    path.exists()
}

fn repository_ctx_execute_output_local(
    command: Command,
    timeout: i32,
    quiet: bool,
) -> Result<RepositoryCommandOutput, String> {
    if timeout <= 0 {
        return Err(format!("timeout must be positive, got {timeout}"));
    }

    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("could not create repository_ctx.execute runtime: {error}"))?
            .block_on(repository_ctx_execute_output_async(command, timeout, quiet))
    })
    .join()
    .map_err(|_| "repository_ctx.execute worker thread panicked".to_owned())?
}

async fn repository_ctx_execute_output_async(
    command: Command,
    timeout: i32,
    quiet: bool,
) -> Result<RepositoryCommandOutput, String> {
    let stream = spawn_command_and_stream_events(
        command,
        Some(Duration::from_secs(timeout as u64)),
        futures::future::pending::<bz_error::Result<GatherOutputStatus>>(),
        DefaultStatusDecoder,
        DefaultKillProcess::default(),
        None,
        false,
        None,
        futures::stream::pending::<ActionFreezeEvent>(),
    )
    .await
    .map_err(|error| error.to_string())?;

    futures::pin_mut!(stream);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    while let Some(event) = stream.try_next().await.map_err(|error| error.to_string())? {
        match event {
            CommandEvent::Stdout(bytes) => {
                if !quiet {
                    std::io::stderr()
                        .write_all(&bytes)
                        .map_err(|error| error.to_string())?;
                }
                stdout.extend_from_slice(&bytes);
            }
            CommandEvent::Stderr(bytes) => {
                if !quiet {
                    std::io::stderr()
                        .write_all(&bytes)
                        .map_err(|error| error.to_string())?;
                }
                stderr.extend_from_slice(&bytes);
            }
            CommandEvent::Exit(status, _orphan_processes) => {
                return Ok(match status {
                    GatherOutputStatus::Finished { exit_code, .. } => RepositoryCommandOutput {
                        stdout,
                        stderr,
                        return_code: exit_code,
                    },
                    GatherOutputStatus::TimedOut(_) => RepositoryCommandOutput {
                        stdout: Vec::new(),
                        stderr: format!("Command timed out after {timeout} seconds").into_bytes(),
                        return_code: 256,
                    },
                    GatherOutputStatus::Cancelled => RepositoryCommandOutput {
                        stdout: Vec::new(),
                        stderr: b"Command was cancelled".to_vec(),
                        return_code: 256,
                    },
                    GatherOutputStatus::SpawnFailed(reason) => RepositoryCommandOutput {
                        stdout: Vec::new(),
                        stderr: reason.into_bytes(),
                        return_code: 256,
                    },
                });
            }
        }
    }

    Err("command event stream ended without exit status".to_owned())
}

fn repository_ctx_latin1_output(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

fn repository_ctx_reject_nonblocking_download(
    block: bool,
    function: &'static str,
) -> starlark::Result<()> {
    if block {
        Ok(())
    } else {
        Err(bz_error::Error::from(
            BazelRepositoryError::RepositoryCtxNonblockingDownloadUnsupported { function },
        )
        .into())
    }
}

#[starlark_module]
fn repository_context_methods(builder: &mut MethodsBuilder) {
    fn file<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(default = "")] content: &str,
        #[starlark(default = true)] executable: bool,
        #[starlark(require = named, default = false)] _legacy_utf8: bool,
    ) -> starlark::Result<NoneType> {
        let path = repository_ctx_output_path_from_value(path, repository_ctx_working_dir(this))?;
        let full_path = Path::new(repository_ctx_working_dir(this))
            .join(&path)
            .to_string_lossy()
            .into_owned();
        repository_ctx_write_bytes(&full_path, content.as_bytes(), executable)?;
        repository_ctx_push_file(
            this,
            BazelRepositoryGeneratedFile {
                path,
                content: content.to_owned(),
                executable,
            },
        );
        Ok(NoneType)
    }

    fn template<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = pos)] template: Value<'v>,
        #[starlark(default = UnpackDictEntries::default())] substitutions: UnpackDictEntries<
            &'v str,
            &'v str,
        >,
        #[starlark(default = true)] executable: bool,
        #[starlark(require = named, default = "auto")] watch_template: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let working_dir = repository_ctx_working_dir(this);
        let path = repository_ctx_output_path_from_value(path, working_dir)?;
        let template_path = repository_ctx_path_from_value_relative_to(this, template, eval)?;
        if repository_should_record_watch(watch_template)? {
            record_repository_file_input(
                &repository_ctx_recorded_inputs(this),
                &template_path,
                working_dir,
            )?;
        }
        let read_path = repository_path_for_read(&template_path);
        let mut content = fs::read_to_string(&read_path).map_err(|e| {
            bz_error::Error::from(BazelRepositoryError::RepositoryCtxTemplateReadFile {
                path: template_path.clone(),
                error: e.to_string(),
            })
        })?;
        for (key, value) in substitutions.entries {
            content = content.replace(key, value);
        }
        let full_path = Path::new(working_dir)
            .join(&path)
            .to_string_lossy()
            .into_owned();
        repository_ctx_write_bytes(&full_path, content.as_bytes(), executable)?;
        repository_ctx_push_file(
            this,
            BazelRepositoryGeneratedFile {
                path,
                content,
                executable,
            },
        );
        Ok(NoneType)
    }

    fn path<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkRepositoryPath> {
        if let Some(path) = path.downcast_ref::<StarlarkRepositoryPath>()
            && path.remote_context.is_some()
        {
            return Ok(path.clone());
        }
        let (path, dep) = repository_path_and_dep_from_value_relative_to(
            path,
            eval,
            Some(repository_ctx_working_dir(this)),
        )?;
        if let Some(dep) = dep.clone() {
            repository_ctx_record_path_dep(this, dep);
            record_repository_file_input(
                &repository_ctx_recorded_inputs(this),
                &path,
                repository_ctx_working_dir(this),
            )?;
        }
        Ok(StarlarkRepositoryPath::new_with_dep(path, dep))
    }

    fn read<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = named, default = "auto")] watch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let path = repository_ctx_path_from_value_relative_to(this, path, eval)?;
        if repository_should_record_watch(watch)? {
            record_repository_file_input(
                &repository_ctx_recorded_inputs(this),
                &path,
                repository_ctx_working_dir(this),
            )?;
        }
        let read_path = repository_path_for_read(&path);
        let bytes = fs::read(&read_path).map_err(|e| {
            bz_error::Error::from(BazelRepositoryError::ModuleCtxReadFile {
                path: path.clone(),
                error: e.to_string(),
            })
        })?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn watch<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let path = repository_ctx_path_from_value_relative_to(this, path, eval)?;
        record_repository_file_input(
            &repository_ctx_recorded_inputs(this),
            &path,
            repository_ctx_working_dir(this),
        )?;
        Ok(NoneType)
    }

    fn watch_tree<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let path = repository_ctx_path_from_value_relative_to(this, path, eval)?;
        if let Some(dep) = repository_ctx_external_input_tree_dep(Path::new(&path)) {
            repository_ctx_record_path_dep(this, dep);
        }
        record_repository_dir_tree_input(
            &repository_ctx_recorded_inputs(this),
            &path,
            repository_ctx_working_dir(this),
        )?;
        Ok(NoneType)
    }

    fn repo_metadata<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = named, default = false)] reproducible: bool,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        attrs_for_reproducibility: UnpackDictEntries<Value<'v>, Value<'v>>,
    ) -> starlark::Result<StarlarkRepositoryMetadata> {
        let _unused = this;
        if reproducible && !attrs_for_reproducibility.entries.is_empty() {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "attrs_for_reproducibility can only be specified if reproducible is False"
            )
            .into());
        }
        Ok(StarlarkRepositoryMetadata { reproducible })
    }

    fn report_progress<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] message: &str,
    ) -> starlark::Result<NoneType> {
        let _unused = this;
        bz_events::dispatch::instant_event(bz_data::BzlmodProgress {
            progress: message.to_owned(),
        });
        Ok(NoneType)
    }

    fn delete<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let path = repository_ctx_path_from_value_relative_to(this, path, eval)?;
        let write_path = repository_path_for_write(&path)?;
        if !write_path.exists() {
            return Ok(false);
        }
        let result = if write_path.is_dir() {
            fs::remove_dir_all(&write_path)
        } else {
            fs::remove_file(&write_path)
        };
        result.map_err(|e| {
            bz_error::Error::from(BazelRepositoryError::RepositoryCtxDeletePath {
                path: write_path.to_string_lossy().into_owned(),
                error: e.to_string(),
            })
        })?;
        Ok(true)
    }

    fn patch<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] patch_file: Value<'v>,
        #[starlark(default = 0)] strip: i32,
        #[starlark(require = named, default = "auto")] watch_patch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let working_dir = repository_ctx_working_dir(this);
        let patch_path = repository_ctx_path_from_value_relative_to(this, patch_file, eval)?;
        if repository_should_record_watch(watch_patch)? {
            record_repository_file_input(
                &repository_ctx_recorded_inputs(this),
                &patch_path,
                working_dir,
            )?;
        }
        let patch_path_abs = repository_path_for_read_abs_relative_to(&patch_path, working_dir);
        if patch_path_abs.is_dir() {
            return Err(
                bz_error::Error::from(BazelRepositoryError::RepositoryCtxPatch {
                    patch: patch_path.clone(),
                    error: "attempting to use a directory as patch file".to_owned(),
                })
                .into(),
            );
        }
        let working_dir_abs = repository_path_for_write(working_dir)?;
        fs::create_dir_all(&working_dir_abs).map_err(|error| {
            bz_error::Error::from(BazelRepositoryError::RepositoryCtxPatch {
                patch: patch_path.clone(),
                error: error.to_string(),
            })
        })?;
        let strip = repository_ctx_patch_strip(strip, &patch_path)?;
        apply_unified_patch_file(&working_dir_abs, &patch_path_abs, strip).map_err(|error| {
            bz_error::Error::from(BazelRepositoryError::RepositoryCtxPatch {
                patch: patch_path,
                error: error.to_string(),
            })
        })?;
        Ok(NoneType)
    }

    fn symlink<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] target: Value<'v>,
        #[starlark(require = pos)] link_name: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let working_dir = repository_ctx_working_dir(this);
        let target_remote_context = target
            .downcast_ref::<StarlarkRepositoryPath>()
            .and_then(|path| path.remote_context.clone());
        let target = repository_ctx_path_from_value_relative_to(this, target, eval)?;
        let link = repository_ctx_output_path_from_value_relative_to(link_name, eval, working_dir)?;
        if let Some(remote_context) = target_remote_context {
            let target_arg = repository_path_for_read_abs_relative_to(&target, working_dir)
                .to_string_lossy()
                .into_owned();
            let output = repository_remote_shell(
                &remote_context,
                "mkdir -p \"$(dirname \"$2\")\" && rm -rf \"$2\" && cp -a \"$1\" \"$2\"",
                &[target_arg, link.clone()],
                60,
                true,
            )?;
            if output.return_code != 0 {
                return Err(
                    bz_error::Error::from(BazelRepositoryError::RepositoryCtxSymlink {
                        target,
                        link,
                        error: repository_ctx_latin1_output(&output.stderr),
                    })
                    .into(),
                );
            }
            return Ok(NoneType);
        }
        let target_path = repository_path_for_read_abs_relative_to(&target, working_dir);
        let link_abs = Path::new(working_dir)
            .join(&link)
            .to_string_lossy()
            .into_owned();
        let link_path = repository_path_for_write(&link_abs)?;
        if let Some(dep) = repository_ctx_external_input_dep(&target_path) {
            repository_ctx_record_path_dep(this, dep);
        }
        if repository_ctx_external_input_dep(&target_path).is_some()
            && !repository_ctx_external_input_ready(&target_path)
        {
            return Err(
                bz_error::Error::from(BazelRepositoryError::RepositoryCtxSymlink {
                    target,
                    link,
                    error: "external symlink target is not materialized".to_owned(),
                })
                .into(),
            );
        }
        if target_path.is_dir()
            && let Some(dep) = repository_ctx_external_input_tree_dep(&target_path)
        {
            repository_ctx_record_path_dep(this, dep);
        }
        if let Some(parent) = link_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                bz_error::Error::from(BazelRepositoryError::RepositoryCtxSymlink {
                    target: target.clone(),
                    link: link.clone(),
                    error: error.to_string(),
                })
            })?;
        }
        repository_ctx_create_symlink(&target_path, &link_path).map_err(|error| {
            bz_error::Error::from(BazelRepositoryError::RepositoryCtxSymlink {
                target,
                link,
                error: error.to_string(),
            })
        })?;
        Ok(NoneType)
    }

    fn which<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] program: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if program.is_empty() {
            return Err(bz_error::Error::from(
                BazelRepositoryError::RepositoryCtxWhichEmptyProgram,
            )
            .into());
        }
        if program.contains('/') || program.contains('\\') {
            return Err(bz_error::Error::from(
                BazelRepositoryError::RepositoryCtxWhichInvalidProgram(program.to_owned()),
            )
            .into());
        }
        let repo_env = repository_ctx_repo_env(this);
        let recorded_inputs = repository_ctx_recorded_inputs(this);
        let Some(path) = record_repository_env_var(&repo_env, &recorded_inputs, "PATH") else {
            return Ok(Value::new_none());
        };
        let path = repository_ctx_command_executor(this)
            .which(program, &path, &repo_env, repository_ctx_working_dir(this))
            .map_err(|error| {
                bz_error::Error::from(BazelRepositoryError::RepositoryCtxWhichFailed {
                    program: program.to_owned(),
                    error,
                })
            })?;
        match path {
            Some(path) => Ok(eval.heap().alloc(path).to_value()),
            None => Ok(Value::new_none()),
        }
    }

    fn execute<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        #[starlark(require = pos)] arguments: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        environment: UnpackDictEntries<&'v str, NoneOr<&'v str>>,
        #[starlark(require = named, default = 600)] timeout: i32,
        #[starlark(require = named, default = true)] quiet: bool,
        #[starlark(require = named)] working_directory: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let repository_working_dir = repository_ctx_working_dir(this).to_owned();
        let mut arguments = arguments
            .items
            .into_iter()
            .map(|arg| repository_ctx_command_arg(arg, &repository_working_dir, eval))
            .collect::<starlark::Result<Vec<_>>>()?;
        if arguments.is_empty() {
            return Err(bz_error::Error::from(
                BazelRepositoryError::RepositoryCtxExecuteEmptyArguments,
            )
            .into());
        }
        let program = arguments.remove(0);
        let repository_working_dir_abs = repository_path_for_write(&repository_working_dir)?;
        let environment = environment
            .entries
            .into_iter()
            .map(|(key, value)| (key, value.into_option()))
            .map(|(key, value)| {
                let value =
                    value.map(|value| repository_ctx_command_env(value, &repository_working_dir));
                (key, value)
            })
            .collect::<Vec<_>>();
        repository_ctx_validate_external_inputs_ready(
            std::iter::once(program.clone()).chain(arguments.iter().cloned()),
            &repository_working_dir_abs,
            &program,
            |dep| repository_ctx_record_path_dep(this, dep),
        )?;
        let repo_env = repository_ctx_repo_env(this);
        let mut command = Command::new(&program);
        command.env_clear();
        command.envs(
            repo_env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        let progress = repository_ctx_command_progress(&program, &arguments);
        command.args(arguments);
        for (key, value) in environment {
            match value {
                Some(value) => {
                    command.env(key, value);
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
        let working_directory = match working_directory {
            Some(working_directory) => repository_path_from_value_relative_to(
                working_directory,
                eval,
                Some(&repository_working_dir),
            )?,
            None => repository_working_dir.clone(),
        };
        let working_directory = if working_directory == repository_working_dir {
            repository_working_dir_abs
        } else {
            repository_path_for_write(&working_directory)?
        };
        fs::create_dir_all(&working_directory).map_err(|error| {
            bz_error::Error::from(BazelRepositoryError::RepositoryCtxExecuteFailed {
                program: program.clone(),
                error: error.to_string(),
            })
        })?;
        command.current_dir(working_directory);
        bz_events::dispatch::instant_event(bz_data::BzlmodProgress { progress });
        let output = repository_ctx_command_executor(this)
            .execute(command, &repository_working_dir, timeout, quiet)
            .map_err(|error| {
                bz_error::Error::from(BazelRepositoryError::RepositoryCtxExecuteFailed {
                    program: program.clone(),
                    error,
                })
            })?;
        Ok(eval.heap().alloc(AllocStruct([
            (
                "stdout",
                eval.heap()
                    .alloc(repository_ctx_latin1_output(&output.stdout)),
            ),
            (
                "stderr",
                eval.heap()
                    .alloc(repository_ctx_latin1_output(&output.stderr)),
            ),
            ("return_code", eval.heap().alloc(output.return_code)),
        ])))
    }

    fn download<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        url: Value<'v>,
        output: Value<'v>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = false)] executable: bool,
        #[starlark(default = false)] allow_fail: bool,
        #[starlark(default = "")] canonical_id: &str,
        #[starlark(default = UnpackDictEntries::default())] auth: UnpackDictEntries<
            Value<'v>,
            Value<'v>,
        >,
        #[starlark(default = UnpackDictEntries::default())] headers: UnpackDictEntries<
            Value<'v>,
            Value<'v>,
        >,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        repository_ctx_reject_nonblocking_download(block, "repository_ctx.download")?;
        let auth_headers = module_ctx_download_auth_headers_from_entries(&auth)?;
        let download_headers = module_ctx_download_headers_from_entries(&headers)?;

        let urls = module_ctx_urls_from_value(url, eval.heap())?;
        let remote_downloader = repository_ctx_remote_downloader(this);
        let output_path = repository_ctx_output_abs_path_from_value_relative_to(
            output,
            eval,
            repository_ctx_working_dir(this),
        )?;
        let (result, _) = repository_ctx_download_to_path(
            urls,
            output_path,
            sha256,
            executable,
            allow_fail,
            integrity,
            canonical_id,
            &download_headers,
            &auth_headers,
            remote_downloader.as_ref(),
        )?;
        Ok(module_ctx_pending_download(block, result, eval))
    }

    #[allow(non_snake_case)]
    fn download_and_extract<'v>(
        this: ValueTypedComplex<'v, StarlarkRepositoryContext<'v>>,
        url: Value<'v>,
        #[starlark(default = "")] output: Value<'v>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = "")] r#type: &str,
        #[starlark(default = "")] strip_prefix: &str,
        #[starlark(default = false)] allow_fail: bool,
        #[starlark(default = "")] canonical_id: &str,
        #[starlark(default = UnpackDictEntries::default())] auth: UnpackDictEntries<
            Value<'v>,
            Value<'v>,
        >,
        #[starlark(default = UnpackDictEntries::default())] headers: UnpackDictEntries<
            Value<'v>,
            Value<'v>,
        >,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        rename_files: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = "")] stripPrefix: &str,
        #[starlark(require = named, default = 0)] strip_components: i32,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        repository_ctx_reject_nonblocking_download(block, "repository_ctx.download_and_extract")?;
        let working_dir = repository_ctx_working_dir(this);
        let archive_name = if r#type.is_empty() {
            ".bz_download_and_extract.archive".to_owned()
        } else {
            format!(
                ".bz_download_and_extract.archive.{}",
                r#type.trim_start_matches('.')
            )
        };
        let archive_path = Path::new(working_dir).join(archive_name);
        let archive_path_string = archive_path.to_string_lossy().into_owned();
        let auth_headers = module_ctx_download_auth_headers_from_entries(&auth)?;
        let download_headers = module_ctx_download_headers_from_entries(&headers)?;
        let rename_files = repository_ctx_rename_files_from_entries(&rename_files)?;
        let urls = module_ctx_urls_from_value(url, eval.heap())?;
        let remote_downloader = repository_ctx_remote_downloader(this);
        let archive_url = urls
            .first()
            .cloned()
            .unwrap_or_else(|| archive_path_string.clone());
        let (result, success) = repository_ctx_download_to_path(
            urls,
            archive_path_string.clone(),
            sha256,
            false,
            allow_fail,
            integrity,
            canonical_id,
            &download_headers,
            &auth_headers,
            remote_downloader.as_ref(),
        )?;
        if !success {
            return Ok(module_ctx_pending_download(block, result, eval));
        }
        let output_path =
            repository_ctx_output_abs_path_from_value_relative_to(output, eval, working_dir)?;
        let output_path = repository_path_for_write(&output_path)?;
        let archive_path = repository_path_for_write(&archive_path_string)?;
        let strip_prefix =
            repository_ctx_renamed_strip_prefix("download_and_extract", strip_prefix, stripPrefix)?;
        let result = match repository_ctx_extract_archive(
            &archive_path,
            &output_path,
            r#type,
            &archive_url,
            strip_prefix,
            strip_components,
            &rename_files,
        ) {
            Ok(()) => result,
            Err(error) => repository_ctx_download_error_result(allow_fail, error)?,
        };
        Ok(module_ctx_pending_download(block, result, eval))
    }
}

fn module_ctx_working_dir<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
) -> &'v str {
    match this.unpack() {
        either::Either::Left(ctx) => &ctx.working_dir,
        either::Either::Right(ctx) => &ctx.working_dir,
    }
}

fn module_ctx_repo_env<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
) -> Arc<BTreeMap<String, String>> {
    match this.unpack() {
        either::Either::Left(ctx) => ctx.repo_env.clone(),
        either::Either::Right(ctx) => ctx.repo_env.clone(),
    }
}

fn module_ctx_recorded_inputs<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
) -> Arc<Mutex<Vec<BazelRepositoryRecordedInput>>> {
    match this.unpack() {
        either::Either::Left(ctx) => ctx.recorded_inputs.clone(),
        either::Either::Right(ctx) => ctx.recorded_inputs.clone(),
    }
}

fn module_ctx_command_executor<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
) -> BazelRepositoryCommandExecutor {
    match this.unpack() {
        either::Either::Left(ctx) => ctx.command_executor.clone(),
        either::Either::Right(ctx) => ctx.command_executor.clone(),
    }
}

fn module_ctx_remote_downloader<'v>(
    this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
) -> Option<BazelRepositoryRemoteDownloaderConfig> {
    match this.unpack() {
        either::Either::Left(ctx) => ctx.remote_downloader.clone(),
        either::Either::Right(ctx) => ctx.remote_downloader.clone(),
    }
}

#[starlark_module]
fn module_extension_context_methods(builder: &mut MethodsBuilder) {
    fn is_dev_dependency<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        tag: Value<'v>,
    ) -> starlark::Result<bool> {
        let _unused = this;
        bazel_module_tag_dev_dependency(tag)
    }

    fn tag_sort_key<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        tag: Value<'v>,
    ) -> starlark::Result<i32> {
        let _unused = this;
        bazel_module_tag_sort_key(tag)
    }

    fn getenv<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] name: &str,
        #[starlark(require = pos, default = NoneOr::None)] default: NoneOr<StringValue<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneOr<StringValue<'v>>> {
        let repo_env = module_ctx_repo_env(this);
        let recorded_inputs = module_ctx_recorded_inputs(this);
        match record_repository_env_var(&repo_env, &recorded_inputs, name) {
            Some(value) => Ok(NoneOr::Other(eval.heap().alloc_str(&value))),
            None => Ok(default),
        }
    }

    fn file<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(default = "")] content: &str,
        #[starlark(default = true)] executable: bool,
        #[starlark(require = named, default = false)] _legacy_utf8: bool,
    ) -> starlark::Result<NoneType> {
        let working_dir = module_ctx_working_dir(this);
        let path = repository_ctx_output_path_from_value(path, working_dir)?;
        let full_path = Path::new(working_dir)
            .join(&path)
            .to_string_lossy()
            .into_owned();
        repository_ctx_write_bytes(&full_path, content.as_bytes(), executable)?;
        Ok(NoneType)
    }

    fn path<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkRepositoryPath> {
        let (path, dep) = repository_path_and_dep_from_value_relative_to(
            path,
            eval,
            Some(module_ctx_working_dir(this)),
        )?;
        if let Some(dep) = dep.clone() {
            module_ctx_record_path_dep(this, dep);
            record_repository_file_input(
                &module_ctx_recorded_inputs(this),
                &path,
                module_ctx_working_dir(this),
            )?;
        }
        Ok(StarlarkRepositoryPath::new_with_dep(path, dep))
    }

    fn watch<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let path = module_ctx_path_from_value_relative_to(this, path, eval)?;
        record_repository_file_input(
            &module_ctx_recorded_inputs(this),
            &path,
            module_ctx_working_dir(this),
        )?;
        Ok(NoneType)
    }

    fn report_progress<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] message: &str,
    ) -> starlark::Result<NoneType> {
        let _unused = this;
        bz_events::dispatch::instant_event(bz_data::BzlmodProgress {
            progress: message.to_owned(),
        });
        Ok(NoneType)
    }

    fn execute<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] arguments: UnpackListOrTuple<Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        environment: UnpackDictEntries<&'v str, NoneOr<&'v str>>,
        #[starlark(require = named, default = 600)] timeout: i32,
        #[starlark(require = named, default = true)] quiet: bool,
        #[starlark(require = named)] working_directory: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let repository_working_dir = module_ctx_working_dir(this).to_owned();
        let mut arguments = arguments
            .items
            .into_iter()
            .map(|arg| repository_ctx_command_arg(arg, &repository_working_dir, eval))
            .collect::<starlark::Result<Vec<_>>>()?;
        if arguments.is_empty() {
            return Err(bz_error::Error::from(
                BazelRepositoryError::RepositoryCtxExecuteEmptyArguments,
            )
            .into());
        }
        let program = arguments.remove(0);
        let repository_working_dir_abs = repository_path_for_write(&repository_working_dir)?;
        let environment = environment
            .entries
            .into_iter()
            .map(|(key, value)| (key, value.into_option()))
            .map(|(key, value)| {
                let value =
                    value.map(|value| repository_ctx_command_env(value, &repository_working_dir));
                (key, value)
            })
            .collect::<Vec<_>>();
        repository_ctx_validate_external_inputs_ready(
            std::iter::once(program.clone()).chain(arguments.iter().cloned()),
            &repository_working_dir_abs,
            &program,
            |dep| module_ctx_record_path_dep(this, dep),
        )?;
        let repo_env = module_ctx_repo_env(this);
        let mut command = Command::new(&program);
        command.env_clear();
        command.envs(
            repo_env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        let progress = repository_ctx_command_progress(&program, &arguments);
        command.args(arguments);
        for (key, value) in environment {
            match value {
                Some(value) => {
                    command.env(key, value);
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
        let working_directory = match working_directory {
            Some(working_directory) => {
                module_ctx_path_from_value_relative_to(this, working_directory, eval)?
            }
            None => repository_working_dir.clone(),
        };
        let working_directory = if working_directory == repository_working_dir {
            repository_working_dir_abs
        } else {
            repository_path_for_write(&working_directory)?
        };
        fs::create_dir_all(&working_directory).map_err(|error| {
            bz_error::Error::from(BazelRepositoryError::RepositoryCtxExecuteFailed {
                program: program.clone(),
                error: error.to_string(),
            })
        })?;
        command.current_dir(working_directory);
        bz_events::dispatch::instant_event(bz_data::BzlmodProgress { progress });
        let output = module_ctx_command_executor(this)
            .execute(command, &repository_working_dir, timeout, quiet)
            .map_err(|error| {
                bz_error::Error::from(BazelRepositoryError::RepositoryCtxExecuteFailed {
                    program: program.clone(),
                    error,
                })
            })?;
        Ok(eval.heap().alloc(AllocStruct([
            (
                "stdout",
                eval.heap()
                    .alloc(repository_ctx_latin1_output(&output.stdout)),
            ),
            (
                "stderr",
                eval.heap()
                    .alloc(repository_ctx_latin1_output(&output.stderr)),
            ),
            ("return_code", eval.heap().alloc(output.return_code)),
        ])))
    }

    fn read<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = named, default = "auto")] watch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let path = module_ctx_path_from_value_relative_to(this, path, eval)?;
        if repository_should_record_watch(watch)? {
            record_repository_file_input(
                &module_ctx_recorded_inputs(this),
                &path,
                module_ctx_working_dir(this),
            )?;
        }
        let read_path = repository_path_for_read(&path);
        let bytes = fs::read(&read_path).map_err(|e| {
            bz_error::Error::from(BazelRepositoryError::ModuleCtxReadFile {
                path: path.clone(),
                error: e.to_string(),
            })
        })?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn download<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = named)] url: Value<'v>,
        #[starlark(require = named)] output: Option<Value<'v>>,
        #[starlark(require = named, default = "")] sha256: &str,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named, default = false)] allow_fail: bool,
        #[starlark(require = named, default = "")] canonical_id: &str,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        auth: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = UnpackDictEntries::default())]
        headers: UnpackDictEntries<Value<'v>, Value<'v>>,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        repository_ctx_reject_nonblocking_download(block, "module_ctx.download")?;
        let auth_headers = module_ctx_download_auth_headers_from_entries(&auth)?;
        let download_headers = module_ctx_download_headers_from_entries(&headers)?;

        let urls = module_ctx_urls_from_value(url, eval.heap())?;
        let output = output.unwrap_or_else(|| eval.heap().alloc(""));
        let output_path = repository_ctx_output_abs_path_from_value_relative_to(
            output,
            eval,
            module_ctx_working_dir(this),
        )?;
        let write_path = match repository_path_for_write(&output_path) {
            Ok(path) => path,
            Err(error) => {
                return module_ctx_download_error_with_block(block, allow_fail, error, eval);
            }
        };
        let expected_checksum = match module_ctx_expected_checksum(sha256, integrity) {
            Ok(expected_checksum) => expected_checksum,
            Err(error) => {
                return module_ctx_download_error_with_block(block, allow_fail, error, eval);
            }
        };

        let remote_downloader = module_ctx_remote_downloader(this);
        let (got_sha256, got_integrity) = match module_ctx_download_to_path_blocking(
            &urls,
            &write_path,
            expected_checksum.as_ref(),
            canonical_id,
            executable,
            &download_headers,
            &auth_headers,
            remote_downloader.as_ref(),
        ) {
            Ok(checksums) => checksums,
            Err(error) => {
                return module_ctx_download_error_with_block(block, allow_fail, error, eval);
            }
        };

        let result =
            ModuleCtxDownloadResult::new(true, got_sha256.as_deref(), Some(&got_integrity), None);
        Ok(module_ctx_pending_download(block, result, eval))
    }

    fn extension_metadata<'v>(
        this: ValueTypedComplex<'v, StarlarkModuleExtensionContext<'v>>,
        #[starlark(require = named, default = false)] reproducible: bool,
        #[starlark(require = named, default = NoneOr::None)] root_module_direct_deps: NoneOr<
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] root_module_direct_dev_deps: NoneOr<
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] facts: NoneOr<Value<'v>>,
    ) -> starlark::Result<StarlarkModuleExtensionMetadata> {
        let _unused = this;
        let _unused = root_module_direct_deps;
        let _unused = root_module_direct_dev_deps;
        let _unused = facts;
        Ok(StarlarkModuleExtensionMetadata { reproducible })
    }
}

#[starlark_module]
#[starlark_types(
    StarlarkRepositoryRule<'_> as RepositoryRule,
    StarlarkTagClass as TagClass,
    StarlarkModuleExtension<'_> as ModuleExtension
)]
pub(crate) fn register_bazel_repository_globals(builder: &mut GlobalsBuilder) {
    fn repository_rule<'v>(
        implementation: Option<StarlarkCallable<'v, (Value<'v>,), Value<'v>>>,
        #[starlark(require = named)] attrs: Option<
            UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        >,
        #[starlark(require = named, default = false)] local: bool,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        environ: UnpackListOrTuple<String>,
        #[starlark(require = named, default = false)] configure: bool,
        #[starlark(require = named, default = false)] remotable: bool,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkRepositoryRule<'v>> {
        let implementation = implementation.ok_or_else(|| {
            bz_error::Error::from(BazelRepositoryError::MissingRepositoryRuleImplementation)
        })?;
        Ok(StarlarkRepositoryRule::new(
            implementation,
            attrs,
            local,
            configure,
            remotable,
            environ,
            doc,
            eval,
        )?)
    }

    fn tag_class<'v>(
        #[starlark(require = named)] attrs: Option<
            UnpackDictEntries<&'v str, &'v StarlarkAttribute<'v>>,
        >,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
    ) -> starlark::Result<StarlarkTagClass> {
        Ok(StarlarkTagClass::new(attrs, doc)?)
    }

    fn module_extension<'v>(
        implementation: StarlarkCallable<'v, (Value<'v>,), Value<'v>>,
        #[starlark(require = named, default = SmallMap::new())] tag_classes: SmallMap<
            String,
            Value<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] doc: NoneOr<&str>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        environ: UnpackListOrTuple<String>,
        #[starlark(require = named, default = false)] os_dependent: bool,
        #[starlark(require = named, default = false)] arch_dependent: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkModuleExtension<'v>> {
        Ok(StarlarkModuleExtension::new(
            implementation,
            tag_classes,
            doc,
            environ,
            os_dependent,
            arch_dependent,
            eval,
        )?)
    }
}
