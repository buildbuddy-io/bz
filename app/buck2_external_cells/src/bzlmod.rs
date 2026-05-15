/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use base64::Engine;
use buck2_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use buck2_common::bzlmod_archive::archive_kind_from_type_or_url;
use buck2_common::bzlmod_archive::extract_archive as extract_bazel_archive;
use buck2_common::bzlmod_patch::apply_unified_patch_file;
use buck2_common::dice::data::HasIoProvider;
use buck2_common::file_ops::delegate::FileOpsDelegate;
use buck2_common::file_ops::dice::ReadFileProxy;
use buck2_common::file_ops::metadata::FileDigestConfig;
use buck2_common::file_ops::metadata::FileType;
use buck2_common::file_ops::metadata::RawDirEntry;
use buck2_common::file_ops::metadata::RawPathMetadata;
use buck2_common::file_ops::metadata::RawPathMetadataForNoWatchFs;
use buck2_common::file_ops::metadata::RawSymlink;
use buck2_common::http::HasHttpClient;
use buck2_common::io::IoProvider;
use buck2_common::io::NoWatchFsMetadataCache;
use buck2_common::io::fs::FsIoProvider;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::external::BZLMOD_EXTERNAL_CELL_KIND;
use buck2_core::cells::external::BZLMOD_GENERATED_EXTERNAL_CELL_PATH_MARKER;
use buck2_core::cells::external::BzlmodBazelFeaturesGlobalsSetup;
use buck2_core::cells::external::BzlmodBazelFeaturesVersionSetup;
use buck2_core::cells::external::BzlmodCcAutoconfSetup;
use buck2_core::cells::external::BzlmodCcAutoconfToolchainsSetup;
use buck2_core::cells::external::BzlmodCellSetup;
use buck2_core::cells::external::BzlmodGeneratedCellGenerator;
use buck2_core::cells::external::BzlmodGeneratedCellSetup;
use buck2_core::cells::external::BzlmodHostPlatformSetup;
use buck2_core::cells::external::BzlmodHttpArchiveSetup;
use buck2_core::cells::external::BzlmodModuleExtensionRepoSetup;
use buck2_core::cells::external::BzlmodPythonHubSetup;
use buck2_core::cells::external::BzlmodRepositoryRuleFile;
use buck2_core::cells::external::BzlmodRepositoryRuleInvocationSetup;
use buck2_core::cells::external::BzlmodRepositoryRuleSetup;
use buck2_core::cells::external::BzlmodShellConfigSetup;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePath;
use buck2_core::fs::buck_out_path::BuckOutPathResolver;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_directory::directory::directory::Directory;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_execute::directory::ActionDirectoryEntry;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::INTERNER;
use buck2_execute::entry::build_entry_from_disk;
use buck2_execute::execute::blocking::HasBlockingExecutor;
use buck2_execute::execute::blocking::IoRequest;
use buck2_execute::materialize::http::Checksum;
use buck2_execute::materialize::http::bazel_repository_download_headers;
use buck2_execute::materialize::http::http_download_with_headers;
use buck2_execute::materialize::materializer::DeclareArtifactPayload;
use buck2_execute::materialize::materializer::HasMaterializer;
use buck2_execute::materialize::materializer::Materializer;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_interpreter_for_build::bazel_repository::bzlmod_repository_rule_invocation_from_setup;
use buck2_interpreter_for_build::bazel_repository::evaluate_bzlmod_module_extension_repo;
use buck2_interpreter_for_build::bazel_repository::evaluate_bzlmod_repository_rule;
use buck2_interpreter_for_build::interpreter::build_context::BazelModuleExtensionEvaluationResult;
use buck2_interpreter_for_build::interpreter::build_context::BazelRepositoryRuleInvocation;
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

static BZLMOD_MATERIALIZATION_LOCKS: OnceLock<
    Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
> = OnceLock::new();

#[derive(buck2_error::Error, Debug)]
#[buck2(tag = Tier0)]
enum BzlmodError {
    #[error("Unsupported bzlmod archive type for `{0}`")]
    UnsupportedArchiveType(String),
    #[error("Expected extracted bzlmod module directory at `{0}`")]
    MissingExtractedDirectory(String),
    #[error("Expected bzlmod materialization to create a directory")]
    NoDirectory,
    #[error("Invalid bzlmod integrity `{0}`")]
    InvalidIntegrity(String),
    #[error("Invalid generated bzlmod repo path `{0}`")]
    InvalidGeneratedRepoPath(String),
    #[error("Could not find `{dict}` in bazel_features globals at `{path}`")]
    MissingBazelFeaturesGlobalsDict { path: String, dict: &'static str },
    #[error("Could not download bzlmod archive from any URL {urls:?}: {error}")]
    DownloadFailed { urls: Vec<String>, error: String },
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

fn bzlmod_materialization_lock(path: &ProjectRelativePath) -> Arc<tokio::sync::Mutex<()>> {
    let locks = BZLMOD_MATERIALIZATION_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    locks
        .lock()
        .expect("bzlmod materialization locks poisoned")
        .entry(path.as_str().to_owned())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .dupe()
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
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
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

        let source = match self.setup.strip_prefix.as_ref() {
            Some(strip_prefix) if !strip_prefix.is_empty() => {
                temp.join(ForwardRelativePath::new(&**strip_prefix)?)
            }
            _ => temp.clone(),
        };
        if !source.exists() {
            return Err(BzlmodError::MissingExtractedDirectory(source.to_string()).into());
        }

        copy_dir_contents(&source, &cache_tmp)?;

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
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
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

struct BzlmodGeneratedHttpArchiveIoRequest {
    setup: BzlmodHttpArchiveSetup,
    archive: ProjectRelativePathBuf,
    temp: ProjectRelativePathBuf,
    dest: ProjectRelativePathBuf,
}

impl IoRequest for BzlmodGeneratedHttpArchiveIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
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

        let source = match self.setup.strip_prefix.as_ref() {
            Some(strip_prefix) if !strip_prefix.is_empty() => {
                temp.join(ForwardRelativePath::new(&**strip_prefix)?)
            }
            _ => temp.clone(),
        };
        if !source.exists() {
            return Err(BzlmodError::MissingExtractedDirectory(source.to_string()).into());
        }
        copy_dir_contents(&source, &dest)?;
        write_generated_module_file(&dest, &self.setup.repo_name)?;
        Ok(())
    }
}

impl IoRequest for BzlmodGeneratedIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
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
) -> buck2_error::Error {
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
) -> buck2_error::Error {
    BzlmodError::RepositoryRuleInvocationNotMaterialized {
        repo_name: setup.repo_name.to_string(),
    }
    .into()
}

fn write_repository_rule_repo(
    project_fs: &ProjectRoot,
    dest: &AbsNormPath,
    canonical_repo_name: &str,
    setup: &BzlmodRepositoryRuleSetup,
) -> buck2_error::Result<()> {
    if let Some(source_dir) = &setup.source_dir {
        let source_dir = ProjectRelativePath::new(source_dir.as_ref())?;
        let source = project_fs.resolve(source_dir);
        copy_dir_contents(&source, dest)?;
    }
    write_generated_module_file(dest, canonical_repo_name)?;
    for file in setup.files.iter() {
        let rel_path = ForwardRelativePath::new(file.path.as_ref())?;
        let path = dest.join(rel_path);
        if let Some(parent) = path.parent() {
            fs_util::create_dir_all(parent)?;
        }
        fs_util::remove_all(&path).categorize_internal()?;
        fs_util::write(&path, file.content.as_bytes()).categorize_internal()?;
        if file.executable {
            fs_util::set_executable(&path, true).categorize_internal()?;
        }
    }
    Ok(())
}

fn write_cc_autoconf_toolchains_repo(
    project_fs: &ProjectRoot,
    dest_rel: &ProjectRelativePath,
    dest: &AbsNormPath,
    setup: &BzlmodCcAutoconfToolchainsSetup,
) -> buck2_error::Result<()> {
    write_generated_module_file(dest, "local_config_cc_toolchains")?;
    let template =
        cc_toolchains_build_template(project_fs, dest_rel, &setup.parent_canonical_repo_name)?;
    let build = template.replace("%{name}", host_cc_cpu_value());
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    Ok(())
}

fn write_cc_autoconf_repo(
    dest: &AbsNormPath,
    _setup: &BzlmodCcAutoconfSetup,
) -> buck2_error::Result<()> {
    write_generated_module_file(dest, "local_config_cc")?;
    write_cc_autoconf_support_files(dest)?;
    let build = local_config_cc_build_file();
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    Ok(())
}

fn write_cc_autoconf_support_files(dest: &AbsNormPath) -> buck2_error::Result<()> {
    let cc = host_tool_path("CC", "cc");
    let cxx = host_tool_path("CXX", "c++");
    let ar = if std::env::consts::OS == "macos" {
        host_tool_path("LIBTOOL", "libtool")
    } else {
        host_tool_path("AR", "ar")
    };

    fs_util::write(
        dest.join(ForwardRelativePath::new("cc_toolchain_config.bzl")?),
        LOCAL_CONFIG_CC_TOOLCHAIN_CONFIG,
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("armeabi_cc_toolchain_config.bzl")?),
        LOCAL_CONFIG_CC_ARMEABI_TOOLCHAIN_CONFIG,
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("cc_wrapper.sh")?),
        format!(
            r#"#!/usr/bin/env bash
has_c_source=0
has_cxx_source=0
previous_arg=
for arg in "$@"; do
  if [[ "$previous_arg" == "-x" ]]; then
    case "$arg" in
      c|objective-c)
        has_c_source=1
        ;;
      c++|objective-c++|c++-*|objective-c++-*)
        has_cxx_source=1
        ;;
    esac
  fi
  case "$arg" in
    -xc|-xobjective-c)
      has_c_source=1
      ;;
    -xc++|-xobjective-c++|-xc++-*|-xobjective-c++-*)
      has_cxx_source=1
      ;;
    *.c|*.m)
      has_c_source=1
      ;;
    *.cc|*.cpp|*.cxx|*.c++|*.C|*.mm)
      has_cxx_source=1
      ;;
  esac
  previous_arg="$arg"
done

if [[ "$has_c_source" == "1" && "$has_cxx_source" == "0" ]]; then
  exec {} "$@"
fi

exec {} "$@"
"#,
            shell_quote(&cc),
            shell_quote(&cxx),
        ),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("ar_wrapper.sh")?),
        format!("#!/usr/bin/env bash\nexec {} \"$@\"\n", shell_quote(&ar)),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("dwp_wrapper.sh")?),
        host_tool_wrapper("DWP", "dwp"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("gcov_wrapper.sh")?),
        host_tool_wrapper("GCOV", "gcov"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("llvm_profdata_wrapper.sh")?),
        host_tool_wrapper("LLVM_PROFDATA", "llvm-profdata"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("nm_wrapper.sh")?),
        host_tool_wrapper("NM", "nm"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("objcopy_wrapper.sh")?),
        host_tool_wrapper("OBJCOPY", "objcopy"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("objdump_wrapper.sh")?),
        host_tool_wrapper("OBJDUMP", "objdump"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("strip_wrapper.sh")?),
        host_tool_wrapper("STRIP", "strip"),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("deps_scanner_wrapper.sh")?),
        format!("#!/usr/bin/env bash\nexec {} \"$@\"\n", shell_quote(&cc)),
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("validate_static_library.sh")?),
        "#!/usr/bin/env bash\nexit 0\n",
    )
    .categorize_internal()?;
    for wrapper in [
        "cc_wrapper.sh",
        "ar_wrapper.sh",
        "dwp_wrapper.sh",
        "gcov_wrapper.sh",
        "llvm_profdata_wrapper.sh",
        "nm_wrapper.sh",
        "objcopy_wrapper.sh",
        "objdump_wrapper.sh",
        "strip_wrapper.sh",
        "deps_scanner_wrapper.sh",
        "validate_static_library.sh",
    ] {
        fs_util::set_executable(&dest.join(ForwardRelativePath::new(wrapper)?), true)
            .categorize_internal()?;
    }
    fs_util::create_dir_all(dest.join(ForwardRelativePath::new("tools/cpp")?))?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("tools/cpp/empty.cc")?),
        "int main() { return 0; }\n",
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("builtin_include_directory_paths")?),
        "",
    )
    .categorize_internal()?;
    Ok(())
}

fn host_tool_wrapper(env_var: &str, fallback: &str) -> String {
    match find_host_tool_path(env_var, fallback) {
        Some(path) => format!("#!/usr/bin/env bash\nexec {} \"$@\"\n", shell_quote(&path)),
        None => missing_tool_wrapper_content(fallback),
    }
}

fn missing_tool_wrapper_content(description: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
echo \"Buck2 generated local_config_cc cannot execute {description}.\" >&2\n\
exit 1\n",
    )
}

fn host_tool_path(env_var: &str, fallback: &str) -> String {
    find_host_tool_path(env_var, fallback).unwrap_or_else(|| fallback.to_owned())
}

fn find_host_tool_path(env_var: &str, fallback: &str) -> Option<String> {
    if let Ok(value) = std::env::var(env_var) {
        if !value.trim().is_empty() {
            return Some(value);
        }
    }
    find_executable_on_path(fallback)
}

fn find_executable_on_path(name: &str) -> Option<String> {
    if name.contains('/') || name.contains('\\') {
        let path = std::path::Path::new(name);
        return path.exists().then(|| path.to_string_lossy().into_owned());
    }

    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn local_config_cc_build_file() -> String {
    let host_toolchain_identifier = host_cc_toolchain_identifier();
    LOCAL_CONFIG_CC_BUILD
        .replace(
            "%HOST_CPU_LITERAL%",
            &starlark_string_literal(host_cc_cpu_value()),
        )
        .replace("%HOST_CPU%", host_cc_cpu_value())
        .replace(
            "%HOST_TOOLCHAIN_IDENTIFIER_LITERAL%",
            &starlark_string_literal(&host_toolchain_identifier),
        )
        .replace("%HOST_TOOLCHAIN_IDENTIFIER%", &host_toolchain_identifier)
        .replace(
            "%HOST_TARGET_LIBC%",
            &starlark_string_literal(host_cc_target_libc()),
        )
}

fn starlark_string_literal(value: &str) -> String {
    format!("{value:?}")
}

fn host_cc_toolchain_identifier() -> String {
    std::env::var("CC_TOOLCHAIN_NAME").unwrap_or_else(|_| "local".to_owned())
}

fn host_cc_target_libc() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macosx",
        "android" => "android",
        _ => "local",
    }
}

const LOCAL_CONFIG_CC_BUILD: &str = r#"load(":cc_toolchain_config.bzl", "cc_toolchain_config")
load(":armeabi_cc_toolchain_config.bzl", "armeabi_cc_toolchain_config")
load("@rules_cc//cc/toolchains:cc_toolchain.bzl", "cc_toolchain")
load("@rules_cc//cc/toolchains:cc_toolchain_suite.bzl", "cc_toolchain_suite")

package(default_visibility = ["//visibility:public"])

licenses(["notice"])

cc_library(name = "empty_lib")

label_flag(
    name = "link_extra_libs",
    build_setting_default = ":empty_lib",
)

cc_library(
    name = "link_extra_lib",
    deps = [
        ":link_extra_libs",
    ],
)

cc_library(name = "malloc")

filegroup(
    name = "empty",
    srcs = [],
)

filegroup(
    name = "builtin_include_directory_paths",
    srcs = ["builtin_include_directory_paths"],
)

filegroup(
    name = "cc_wrapper",
    srcs = ["cc_wrapper.sh"],
)

filegroup(
    name = "ar_wrapper",
    srcs = ["ar_wrapper.sh"],
)

filegroup(
    name = "dwp_wrapper",
    srcs = ["dwp_wrapper.sh"],
)

filegroup(
    name = "gcov_wrapper",
    srcs = ["gcov_wrapper.sh"],
)

filegroup(
    name = "llvm_profdata_wrapper",
    srcs = ["llvm_profdata_wrapper.sh"],
)

filegroup(
    name = "nm_wrapper",
    srcs = ["nm_wrapper.sh"],
)

filegroup(
    name = "objcopy_wrapper",
    srcs = ["objcopy_wrapper.sh"],
)

filegroup(
    name = "objdump_wrapper",
    srcs = ["objdump_wrapper.sh"],
)

filegroup(
    name = "strip_wrapper",
    srcs = ["strip_wrapper.sh"],
)

filegroup(
    name = "deps_scanner_wrapper",
    srcs = ["deps_scanner_wrapper.sh"],
)

filegroup(
    name = "validate_static_library",
    srcs = ["validate_static_library.sh"],
)

filegroup(
    name = "compiler_deps",
    srcs = [
        "builtin_include_directory_paths",
        "cc_wrapper.sh",
        "deps_scanner_wrapper.sh",
    ],
)

filegroup(
    name = "ar_files",
    srcs = [
        "ar_wrapper.sh",
        "builtin_include_directory_paths",
        "cc_wrapper.sh",
        "deps_scanner_wrapper.sh",
        "validate_static_library.sh",
    ],
)

filegroup(
    name = "dwp_files",
    srcs = ["dwp_wrapper.sh"],
)

filegroup(
    name = "objcopy_files",
    srcs = ["objcopy_wrapper.sh"],
)

filegroup(
    name = "strip_files",
    srcs = ["strip_wrapper.sh"],
)

cc_toolchain_suite(
    name = "toolchain",
    toolchains = {
        "%HOST_CPU%|compiler": ":cc-compiler-%HOST_CPU%",
        "%HOST_CPU%": ":cc-compiler-%HOST_CPU%",
        "armeabi-v7a|compiler": ":cc-compiler-armeabi-v7a",
        "armeabi-v7a": ":cc-compiler-armeabi-v7a",
    },
)

cc_toolchain(
    name = "cc-compiler-%HOST_CPU%",
    toolchain_identifier = %HOST_TOOLCHAIN_IDENTIFIER_LITERAL%,
    toolchain_config = ":%HOST_TOOLCHAIN_IDENTIFIER%",
    all_files = ":compiler_deps",
    ar_files = ":ar_files",
    as_files = ":compiler_deps",
    compiler_files = ":compiler_deps",
    dwp_files = ":dwp_files",
    linker_files = ":compiler_deps",
    objcopy_files = ":objcopy_files",
    strip_files = ":strip_files",
    supports_header_parsing = True,
    supports_param_files = True,
)

cc_toolchain_config(
    name = %HOST_TOOLCHAIN_IDENTIFIER_LITERAL%,
    cpu = %HOST_CPU_LITERAL%,
    compiler = "compiler",
    toolchain_identifier = %HOST_TOOLCHAIN_IDENTIFIER_LITERAL%,
    host_system_name = "local",
    target_system_name = "local",
    target_libc = %HOST_TARGET_LIBC%,
    abi_version = "local",
    abi_libc_version = "local",
    tool_paths = {
        "ar": "ar_wrapper.sh",
        "cpp": "cc_wrapper.sh",
        "cpp-module-deps-scanner": "deps_scanner_wrapper.sh",
        "dwp": "dwp_wrapper.sh",
        "gcc": "cc_wrapper.sh",
        "gcov": "gcov_wrapper.sh",
        "ld": "cc_wrapper.sh",
        "llvm-profdata": "llvm_profdata_wrapper.sh",
        "nm": "nm_wrapper.sh",
        "objcopy": "objcopy_wrapper.sh",
        "objdump": "objdump_wrapper.sh",
        "parse_headers": "cc_wrapper.sh",
        "strip": "strip_wrapper.sh",
        "validate_static_library": "validate_static_library.sh",
    },
)

cc_toolchain(
    name = "cc-compiler-armeabi-v7a",
    toolchain_identifier = "stub_armeabi-v7a",
    toolchain_config = ":stub_armeabi-v7a",
    all_files = ":empty",
    ar_files = ":empty",
    as_files = ":empty",
    compiler_files = ":empty",
    dwp_files = ":empty",
    linker_files = ":empty",
    objcopy_files = ":empty",
    strip_files = ":empty",
    supports_param_files = 1,
)

armeabi_cc_toolchain_config(name = "stub_armeabi-v7a")
"#;

const LOCAL_CONFIG_CC_TOOLCHAIN_CONFIG: &str = r#"load("@rules_cc//cc/toolchains:cc_toolchain_config_info.bzl", "CcToolchainConfigInfo")

def _tool_path(name, path):
    return struct(name = name, path = path)

def _impl(ctx):
    tool_paths = [
        _tool_path(name, path)
        for name, path in ctx.attr.tool_paths.items()
    ]
    return [cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        features = [],
        action_configs = [],
        artifact_name_patterns = [],
        cxx_builtin_include_directories = ctx.attr.cxx_builtin_include_directories,
        toolchain_identifier = ctx.attr.toolchain_identifier,
        host_system_name = ctx.attr.host_system_name,
        target_system_name = ctx.attr.target_system_name,
        target_cpu = ctx.attr.cpu,
        target_libc = ctx.attr.target_libc,
        compiler = ctx.attr.compiler,
        abi_version = ctx.attr.abi_version,
        abi_libc_version = ctx.attr.abi_libc_version,
        tool_paths = tool_paths,
        builtin_sysroot = ctx.attr.builtin_sysroot,
        cc_target_os = None,
    )]

cc_toolchain_config = rule(
    implementation = _impl,
    attrs = {
        "abi_libc_version": attr.string(mandatory = True),
        "abi_version": attr.string(mandatory = True),
        "builtin_sysroot": attr.string(),
        "compiler": attr.string(mandatory = True),
        "cpu": attr.string(mandatory = True),
        "cxx_builtin_include_directories": attr.string_list(),
        "host_system_name": attr.string(mandatory = True),
        "target_libc": attr.string(mandatory = True),
        "target_system_name": attr.string(mandatory = True),
        "tool_paths": attr.string_dict(),
        "toolchain_identifier": attr.string(mandatory = True),
    },
    provides = [CcToolchainConfigInfo],
)
"#;

const LOCAL_CONFIG_CC_ARMEABI_TOOLCHAIN_CONFIG: &str = r#"load(
    "@rules_cc//cc:cc_toolchain_config_lib.bzl",
    "feature",
    "tool_path",
)
load("@rules_cc//cc/common:cc_common.bzl", "cc_common")
load("@rules_cc//cc/toolchains:cc_toolchain_config_info.bzl", "CcToolchainConfigInfo")

def _impl(ctx):
    toolchain_identifier = "stub_armeabi-v7a"
    host_system_name = "armeabi-v7a"
    target_system_name = "armeabi-v7a"
    target_cpu = "armeabi-v7a"
    target_libc = "armeabi-v7a"
    compiler = "compiler"
    abi_version = "armeabi-v7a"
    abi_libc_version = "armeabi-v7a"
    cc_target_os = None
    builtin_sysroot = None
    action_configs = []

    supports_pic_feature = feature(name = "supports_pic", enabled = True)
    supports_dynamic_linker_feature = feature(name = "supports_dynamic_linker", enabled = True)
    features = [supports_dynamic_linker_feature, supports_pic_feature]

    cxx_builtin_include_directories = []
    artifact_name_patterns = []
    make_variables = []

    tool_paths = [
        tool_path(name = "ar", path = "/bin/false"),
        tool_path(name = "compat-ld", path = "/bin/false"),
        tool_path(name = "cpp", path = "/bin/false"),
        tool_path(name = "dwp", path = "/bin/false"),
        tool_path(name = "gcc", path = "/bin/false"),
        tool_path(name = "gcov", path = "/bin/false"),
        tool_path(name = "ld", path = "/bin/false"),
        tool_path(name = "llvm-profdata", path = "/bin/false"),
        tool_path(name = "nm", path = "/bin/false"),
        tool_path(name = "objcopy", path = "/bin/false"),
        tool_path(name = "objdump", path = "/bin/false"),
        tool_path(name = "strip", path = "/bin/false"),
    ]
    return cc_common.create_cc_toolchain_config_info(
        ctx = ctx,
        features = features,
        action_configs = action_configs,
        artifact_name_patterns = artifact_name_patterns,
        cxx_builtin_include_directories = cxx_builtin_include_directories,
        toolchain_identifier = toolchain_identifier,
        host_system_name = host_system_name,
        target_system_name = target_system_name,
        target_cpu = target_cpu,
        target_libc = target_libc,
        compiler = compiler,
        abi_version = abi_version,
        abi_libc_version = abi_libc_version,
        tool_paths = tool_paths,
        make_variables = make_variables,
        builtin_sysroot = builtin_sysroot,
        cc_target_os = cc_target_os,
    )

armeabi_cc_toolchain_config = rule(
    implementation = _impl,
    attrs = {},
    provides = [CcToolchainConfigInfo],
)
"#;

fn write_shell_config_repo(
    dest: &AbsNormPath,
    _setup: &BzlmodShellConfigSetup,
) -> buck2_error::Result<()> {
    write_generated_module_file(dest, "local_config_shell")?;
    let mut toolchains = Vec::new();
    for (os, default_shell_path) in [
        ("windows", "c:/msys64/usr/bin/bash.exe"),
        ("linux", "/bin/bash"),
        ("osx", "/bin/bash"),
        ("freebsd", "/usr/local/bin/bash"),
        ("openbsd", "/usr/local/bin/bash"),
    ] {
        let is_host = host_platform_os_constraint() == Some(os);
        let sh_path = if is_host {
            std::env::var("BAZEL_SH")
                .ok()
                .filter(|path| !path.trim().is_empty())
                .or_else(|| find_executable_on_path("bash"))
                .unwrap_or_else(|| default_shell_path.to_owned())
        } else {
            default_shell_path.to_owned()
        };
        if os == "windows" {
            toolchains.push(format!(
                r#"sh_toolchain(
    name = "{os}_sh",
    path = {sh_path:?},
    launcher = "@bazel_tools//tools/launcher",
    launcher_maker = "@bazel_tools//tools/launcher:launcher_maker",
)"#
            ));
        } else {
            toolchains.push(format!(
                r#"sh_toolchain(
    name = "{os}_sh",
    path = {sh_path:?},
)"#
            ));
        }
        toolchains.push(format!(
            r#"toolchain(
    name = "{os}_sh_toolchain",
    toolchain = ":{os}_sh",
    toolchain_type = "@rules_shell//shell:toolchain_type",
    target_compatible_with = [
        "@platforms//os:{os}",
    ],
)"#
        ));
    }
    let build = format!(
        "load(\"@rules_shell//shell/toolchains:sh_toolchain.bzl\", \"sh_toolchain\")\n\n{}\n",
        toolchains.join("\n\n")
    );
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    Ok(())
}

fn write_python_hub_repo(
    dest: &AbsNormPath,
    _setup: &BzlmodPythonHubSetup,
) -> buck2_error::Result<()> {
    write_generated_module_file(dest, "pythons_hub")?;
    let build = r#"package(default_visibility = ["//visibility:public"])

exports_files([
    "interpreters.bzl",
    "versions.bzl",
])
"#;
    let interpreters = r#"# Generated by Buck2 for an unpopulated rules_python hub.

INTERPRETER_LABELS = {}
"#;
    let versions = r#"# Generated by Buck2 for an unpopulated rules_python hub.

DEFAULT_PYTHON_VERSION = ""
MINOR_MAPPING = {}
PYTHON_VERSIONS = []
"#;
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("interpreters.bzl")?),
        interpreters,
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("versions.bzl")?),
        versions,
    )
    .categorize_internal()?;
    Ok(())
}

fn cc_toolchains_build_template(
    project_fs: &ProjectRoot,
    dest_rel: &ProjectRelativePath,
    parent_canonical_repo_name: &str,
) -> buck2_error::Result<String> {
    let Some((external_cells_root, _)) = dest_rel
        .as_str()
        .split_once(BZLMOD_GENERATED_EXTERNAL_CELL_PATH_MARKER)
    else {
        return Err(BzlmodError::InvalidGeneratedRepoPath(dest_rel.to_string()).into());
    };
    read_bzlmod_module_file_text(
        project_fs,
        external_cells_root,
        parent_canonical_repo_name,
        "cc/private/toolchain/BUILD.toolchains.tpl",
    )
    .with_buck_error_context(|| {
        format!("Error reading rules_cc toolchains template from `{parent_canonical_repo_name}`")
    })
}

fn read_bzlmod_module_file_text(
    project_fs: &ProjectRoot,
    external_cells_root: &str,
    canonical_repo_name: &str,
    path: &str,
) -> buck2_error::Result<String> {
    let materialized_path = ProjectRelativePathBuf::unchecked_new(format!(
        "{external_cells_root}/{BZLMOD_EXTERNAL_CELL_KIND}/{canonical_repo_name}/{path}",
    ));
    match fs_util::read_to_string(project_fs.resolve(&materialized_path)) {
        Ok(contents) => return Ok(contents),
        Err(error)
            if matches!(
                error.io_error_kind(),
                Some(ErrorKind::NotFound | ErrorKind::NotADirectory)
            ) => {}
        Err(error) => return Err(error.categorize_internal()),
    }

    let alias_path = bzlmod_repo_contents_cache_alias_path(canonical_repo_name);
    let cache_repo = fs_util::read_to_string(project_fs.resolve(&alias_path))
        .categorize_internal()
        .with_buck_error_context(|| {
            format!("Error reading bzlmod repo contents cache alias `{alias_path}`")
        })?;
    let cache_path = ProjectRelativePathBuf::unchecked_new(format!("{cache_repo}/{path}"));
    fs_util::read_to_string(project_fs.resolve(&cache_path))
        .categorize_internal()
        .with_buck_error_context(|| {
            format!("Error reading bzlmod cached module file `{cache_path}`")
        })
}

fn host_cc_cpu_value() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin_arm64",
        ("macos", _) => "darwin_x86_64",
        ("freebsd", _) => "freebsd",
        ("openbsd", _) => "openbsd",
        ("windows", "aarch64") => "arm64_windows",
        ("windows", _) => "x64_windows",
        (_, "power" | "powerpc" | "powerpc64" | "powerpc64le") => "ppc",
        (_, "s390x") => "s390x",
        (_, "mips64") => "mips64",
        (_, "riscv64") => "riscv64",
        (_, "arm" | "armv7" | "armv7l") => "arm",
        (_, "aarch64") => "aarch64",
        (_, "x86_64") => "k8",
        (_, "x86" | "i386" | "i486" | "i586" | "i686" | "i786") => "piii",
        _ => "k8",
    }
}

fn write_bazel_features_version_repo(
    dest: &AbsNormPath,
    setup: &BzlmodBazelFeaturesVersionSetup,
) -> buck2_error::Result<()> {
    write_generated_module_file(dest, "bazel_features_version")?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("BUILD.bazel")?),
        "load(\"@bazel_skylib//:bzl_library.bzl\", \"bzl_library\")\n\nexports_files([\"version.bzl\"])\n\nbzl_library(\n    name = \"version\",\n    srcs = [\"version.bzl\"],\n    visibility = [\"//visibility:public\"],\n)\n",
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("version.bzl")?),
        format!("version = {:?}\n", setup.bazel_version.as_ref()),
    )
    .categorize_internal()?;
    Ok(())
}

fn write_host_platform_repo(
    dest: &AbsNormPath,
    _setup: &BzlmodHostPlatformSetup,
) -> buck2_error::Result<()> {
    write_generated_module_file(dest, "host_platform")?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("BUILD.bazel")?),
        "# DO NOT EDIT: automatically generated BUILD file\nexports_files([\"constraints.bzl\"])\n",
    )
    .categorize_internal()?;

    let mut constraints = Vec::new();
    if let Some(cpu) = host_platform_cpu_constraint() {
        constraints.push(format!("    '@platforms//cpu:{cpu}',"));
    }
    if let Some(os) = host_platform_os_constraint() {
        constraints.push(format!("    '@platforms//os:{os}',"));
    }
    let constraints = if constraints.is_empty() {
        String::new()
    } else {
        format!("\n{}\n", constraints.join("\n"))
    };
    fs_util::write(
        dest.join(ForwardRelativePath::new("constraints.bzl")?),
        format!(
            "# DO NOT EDIT: automatically generated constraints list\nHOST_CONSTRAINTS = [{}]\n",
            constraints
        ),
    )
    .categorize_internal()?;
    Ok(())
}

fn host_platform_cpu_constraint() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86" | "i386" | "i486" | "i586" | "i686" | "i786" => Some("x86_32"),
        "x86_64" => Some("x86_64"),
        "powerpc" | "powerpc64" => Some("ppc"),
        "powerpc64le" => Some("ppc64le"),
        "arm" | "armv7" => Some("arm"),
        "aarch64" => Some("aarch64"),
        "s390x" => Some("s390x"),
        "mips64" => Some("mips64"),
        "riscv64" => Some("riscv64"),
        _ => None,
    }
}

fn host_platform_os_constraint() -> Option<&'static str> {
    match std::env::consts::OS {
        "macos" => Some("osx"),
        "freebsd" => Some("freebsd"),
        "openbsd" => Some("openbsd"),
        "linux" => Some("linux"),
        "windows" => Some("windows"),
        _ => None,
    }
}

fn write_bazel_features_globals_repo(
    project_fs: &ProjectRoot,
    dest_rel: &ProjectRelativePath,
    dest: &AbsNormPath,
    setup: &BzlmodBazelFeaturesGlobalsSetup,
) -> buck2_error::Result<()> {
    let Some((external_cells_root, _)) = dest_rel
        .as_str()
        .split_once(BZLMOD_GENERATED_EXTERNAL_CELL_PATH_MARKER)
    else {
        return Err(BzlmodError::InvalidGeneratedRepoPath(dest_rel.to_string()).into());
    };
    let globals_path = ProjectRelativePathBuf::unchecked_new(format!(
        "{external_cells_root}/{BZLMOD_EXTERNAL_CELL_KIND}/{}/private/globals.bzl",
        setup.parent_canonical_repo_name
    ));
    let globals_text = read_bzlmod_module_file_text(
        project_fs,
        external_cells_root,
        &setup.parent_canonical_repo_name,
        "private/globals.bzl",
    )
    .with_buck_error_context(|| {
        format!("Error reading bazel_features globals `{}`", globals_path)
    })?;
    let globals = parse_bazel_features_globals_dict(&globals_text, &globals_path)?;

    write_generated_module_file(dest, "bazel_features_globals")?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("BUILD.bazel")?),
        "load(\"@bazel_skylib//:bzl_library.bzl\", \"bzl_library\")\n\nexports_files([\"globals.bzl\"])\n\nbzl_library(\n    name = \"globals\",\n    srcs = [\"globals.bzl\"],\n    visibility = [\"//visibility:public\"],\n)\n",
    )
    .categorize_internal()?;

    let mut globals_bzl = String::from("globals = struct(\n");
    for (global, versions) in globals {
        let value = if bazel_feature_global_is_available(
            &setup.bazel_version,
            &versions.min_version,
            &versions.max_version,
        ) {
            global.as_str()
        } else {
            "None"
        };
        globals_bzl.push_str(&format!(
            "    {global} = getattr(getattr(native, 'legacy_globals', None), {global:?}, {value}),\n"
        ));
    }
    globals_bzl.push_str(")\n");
    fs_util::write(
        dest.join(ForwardRelativePath::new("globals.bzl")?),
        globals_bzl,
    )
    .categorize_internal()?;
    Ok(())
}

struct BazelFeatureGlobalVersions {
    min_version: String,
    max_version: String,
}

fn parse_bazel_features_globals_dict(
    text: &str,
    path: &ProjectRelativePath,
) -> buck2_error::Result<BTreeMap<String, BazelFeatureGlobalVersions>> {
    let mut values = BTreeMap::new();
    let mut in_dict = false;

    for line in text.lines() {
        let line = strip_starlark_line_comment(line).trim();
        if !in_dict {
            if line == "GLOBALS = {" {
                in_dict = true;
            }
            continue;
        }
        if line.starts_with('}') {
            return Ok(values);
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(key) = parse_simple_bzl_string(key.trim()) else {
            continue;
        };
        let value = value.trim().trim_end_matches(',');
        let Some((min_version, max_version)) = parse_bazel_features_version_pair(value) else {
            continue;
        };
        values.insert(
            key,
            BazelFeatureGlobalVersions {
                min_version,
                max_version,
            },
        );
    }

    Err(BzlmodError::MissingBazelFeaturesGlobalsDict {
        path: path.to_string(),
        dict: "GLOBALS",
    }
    .into())
}

fn strip_starlark_line_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;

    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            in_string = true;
            quote = ch;
            continue;
        }
        if ch == '#' {
            return &line[..idx];
        }
    }

    line
}

fn parse_simple_bzl_string(value: &str) -> Option<String> {
    let quote = value.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = value.strip_prefix(quote)?.strip_suffix(quote)?;
    Some(value.to_owned())
}

fn parse_bazel_features_version_pair(value: &str) -> Option<(String, String)> {
    if let Some(min_version) = parse_simple_bzl_string(value) {
        return Some((min_version, String::new()));
    }

    let value = value
        .strip_prefix('(')
        .and_then(|value| value.strip_suffix(')'))
        .or_else(|| {
            value
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
        })?;
    let mut parts = value.split(',');
    let min_version = parse_simple_bzl_string(parts.next()?.trim())?;
    let max_version = parse_simple_bzl_string(parts.next()?.trim())?;
    if parts.next().is_some() {
        return None;
    }
    Some((min_version, max_version))
}

#[cfg(test)]
mod tests {
    use buck2_core::cells::external::BzlmodPatch;

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
    fn checksum_from_integrity_allows_empty_integrity() {
        assert!(matches!(
            checksum_from_integrity("").unwrap(),
            Checksum::None
        ));
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

fn write_generated_module_file(dest: &AbsNormPath, name: &str) -> buck2_error::Result<()> {
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
) -> buck2_error::Result<()> {
    let primary_url = bzlmod_cell_setup_primary_url(setup);
    let archive_type = setup.archive_type.as_deref();
    let kind = archive_kind_from_type_or_url(archive_type, primary_url)
        .ok_or_else(|| BzlmodError::UnsupportedArchiveType(primary_url.to_owned()))?;
    extract_bazel_archive(archive.as_path(), temp.as_path(), kind, "", 0, &[])
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
) -> buck2_error::Result<()> {
    let patch = project_fs.resolve(patch);
    apply_unified_patch_file(dest.as_path(), patch.as_path(), patch_strip)
        .buck_error_context("Could not apply patch for bzlmod external cell")
}

fn copy_dir_contents(from: &AbsNormPath, to: &AbsNormPath) -> buck2_error::Result<()> {
    for entry in fs_util::read_dir(from).categorize_internal()? {
        let entry = entry?;
        let from_path = entry.path();
        let to_path = to.join(ForwardRelativePath::new(&entry.file_name())?);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs_util::create_dir_all(&to_path)?;
            copy_dir_contents(&from_path, &to_path)?;
        } else if file_type.is_file() {
            link_or_copy_file(&from_path, &to_path)?;
        } else if file_type.is_symlink() {
            let target = fs_util::read_link(&from_path).categorize_internal()?;
            fs_util::symlink(target, &to_path).categorize_internal()?;
        }
    }
    Ok(())
}

fn link_or_copy_file(from: &AbsNormPath, to: &AbsNormPath) -> buck2_error::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs_util::copy(from, to).categorize_internal()?;
            Ok(())
        }
    }
}

fn integrity_to_sha256_hex(integrity: &str) -> buck2_error::Result<String> {
    let Some(encoded) = integrity.strip_prefix("sha256-") else {
        return Err(BzlmodError::InvalidIntegrity(integrity.to_owned()).into());
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| BzlmodError::InvalidIntegrity(integrity.to_owned()))?;
    if bytes.len() != 32 {
        return Err(BzlmodError::InvalidIntegrity(integrity.to_owned()).into());
    }
    Ok(hex::encode(bytes))
}

fn checksum_from_integrity(integrity: &str) -> buck2_error::Result<Checksum> {
    if integrity.is_empty() {
        Ok(Checksum::none())
    } else {
        Checksum::new(None, Some(&integrity_to_sha256_hex(integrity)?))
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

fn bzlmod_repo_contents_cache_alias_path(canonical_repo_name: &str) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/cache/bzlmod_repo_contents/by_canonical/{canonical_repo_name}",
    ))
}

fn bzlmod_repo_contents_cache_exists(
    project_fs: &ProjectRoot,
    cache_repo: &ProjectRelativePath,
) -> buck2_error::Result<bool> {
    let cache_repo = project_fs.resolve(cache_repo);
    Ok(matches!(
        fs_util::symlink_metadata_if_exists(&cache_repo)?,
        Some(metadata) if metadata.is_dir()
    ))
}

fn record_bzlmod_repo_contents_cache_alias(
    project_fs: &ProjectRoot,
    cache_alias: &ProjectRelativePath,
    cache_repo: &ProjectRelativePath,
) -> buck2_error::Result<()> {
    let cache_alias = project_fs.resolve(cache_alias);
    if let Some(parent) = cache_alias.parent() {
        fs_util::create_dir_all(parent)?;
    }
    fs_util::write(cache_alias, cache_repo.as_str()).categorize_internal()
}

fn prepare_bzlmod_external_cell_root(
    project_fs: &ProjectRoot,
    cache_repo: &ProjectRelativePath,
    dest: &ProjectRelativePath,
) -> buck2_error::Result<()> {
    if bzlmod_external_cell_root_is_current(project_fs, cache_repo, dest)? {
        return Ok(());
    }

    let stamp_path = project_fs.resolve(&bzlmod_external_cell_root_stamp_path(dest));
    let stamp_content = bzlmod_external_cell_root_stamp_content(cache_repo);
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

fn bzlmod_external_cell_root_stamp_path(dest: &ProjectRelativePath) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!("{}.materialization_stamp", dest.as_str()))
}

fn bzlmod_external_cell_root_stamp_content(cache_repo: &ProjectRelativePath) -> String {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(&mut hasher, "buck2-bzlmod-external-cell-root-v3");
    update_bzlmod_repo_contents_cache_key(&mut hasher, cache_repo.as_str());
    format!("{}\n", hasher.finalize().to_hex())
}

fn bzlmod_external_cell_root_is_current(
    project_fs: &ProjectRoot,
    cache_repo: &ProjectRelativePath,
    dest: &ProjectRelativePath,
) -> buck2_error::Result<bool> {
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

    let stamp_path = project_fs.resolve(&bzlmod_external_cell_root_stamp_path(dest));
    let stamp_content = bzlmod_external_cell_root_stamp_content(cache_repo);
    Ok(matches!(
        fs_util::read_to_string_if_exists(&stamp_path)?,
        Some(content) if content == stamp_content
    ))
}

fn bzlmod_generated_repo_symlink_targets_exist(path: &AbsNormPath) -> buck2_error::Result<bool> {
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
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
                {
                    return Ok(false);
                }
                Err(error) => {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Tier0,
                        "Error checking generated bzlmod symlink target `{}`: {}",
                        target_path.display(),
                        error
                    ));
                }
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

fn bzlmod_generated_materialization_stamp_content(setup: &BzlmodGeneratedCellSetup) -> String {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(&mut hasher, "buck2-bzlmod-generated-materialization-v2");
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
        BzlmodGeneratedCellGenerator::HostPlatform(_) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "host_platform");
        }
        BzlmodGeneratedCellGenerator::CcAutoconfToolchains(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "cc_autoconf_toolchains");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_canonical_repo_name);
        }
        BzlmodGeneratedCellGenerator::CcAutoconf(_) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "cc_autoconf");
        }
        BzlmodGeneratedCellGenerator::ShellConfig(_) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "shell_config");
        }
        BzlmodGeneratedCellGenerator::HttpArchive(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "http_archive");
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
            let files_json = serde_json::to_string(&setup.files)
                .expect("serializing repository_rule file manifest cannot fail");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &files_json);
            update_bzlmod_repo_contents_cache_key_opt(&mut hasher, setup.source_dir.as_deref());
        }
        BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "repository_rule_invocation");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.repo_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.rule_bzl_cell);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.rule_bzl_path);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.rule_bzl_build_file_cell);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.rule_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.attrs.len().to_string());
            for (key, value) in setup.attrs.iter() {
                update_bzlmod_repo_contents_cache_key(&mut hasher, key);
                update_bzlmod_repo_contents_cache_key(&mut hasher, value);
            }
        }
        BzlmodGeneratedCellGenerator::ModuleExtensionRepo(setup) => {
            update_bzlmod_repo_contents_cache_key(&mut hasher, "module_extension_repo");
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_canonical_repo_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_is_root.to_string());
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_file);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_cell);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_path);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.repo_name);
            update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_usages_key);
        }
    }
    format!("{}\n", hasher.finalize().to_hex())
}

async fn bzlmod_generated_materialization_is_current(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
) -> buck2_error::Result<bool> {
    let io = ctx.global_data().get_io_provider().dupe();
    let repo_path = path.to_owned();
    let stamp_path = bzlmod_generated_materialization_stamp_path(setup, path);
    let stamp_content = bzlmod_generated_materialization_stamp_content(setup);
    if !matches!(
        (&*io).read_path_metadata_if_exists(repo_path).await?,
        Some(RawPathMetadata::Directory)
    ) {
        return Ok(false);
    }
    let stamp_matches = matches!(
        (&*io).read_file_if_exists(stamp_path).await?,
        Some(content) if content == stamp_content
    );
    if !stamp_matches {
        return Ok(false);
    }
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let path = project_root.resolve(path);
    ctx.get_blocking_executor()
        .execute_io_inline(move || bzlmod_generated_repo_symlink_targets_exist(&path))
        .await
}

async fn write_bzlmod_generated_materialization_stamp(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
) -> buck2_error::Result<()> {
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let stamp_path =
        project_root.resolve(&bzlmod_generated_materialization_stamp_path(setup, path));
    let stamp_content = bzlmod_generated_materialization_stamp_content(setup);
    ctx.get_blocking_executor()
        .execute_io_inline(move || {
            if let Some(parent) = stamp_path.parent() {
                fs_util::create_dir_all(parent)?;
            }
            fs_util::write(stamp_path, stamp_content).categorize_internal()
        })
        .await
}

fn bzlmod_module_extension_evaluation_cache_key(setup: &BzlmodModuleExtensionRepoSetup) -> String {
    let mut hasher = blake3::Hasher::new();
    update_bzlmod_repo_contents_cache_key(&mut hasher, "buck2-bzlmod-module-extension-v1");
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_canonical_repo_name);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.parent_is_root.to_string());
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_file);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_cell);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_bzl_path);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_name);
    update_bzlmod_repo_contents_cache_key(&mut hasher, &setup.extension_usages_key);
    hasher.finalize().to_hex().to_string()
}

fn bzlmod_module_extension_evaluation_setup(
    setup: &BzlmodModuleExtensionRepoSetup,
) -> BzlmodModuleExtensionRepoSetup {
    BzlmodModuleExtensionRepoSetup {
        parent_canonical_repo_name: setup.parent_canonical_repo_name.dupe(),
        parent_is_root: setup.parent_is_root,
        extension_bzl_file: setup.extension_bzl_file.dupe(),
        extension_bzl_cell: setup.extension_bzl_cell.dupe(),
        extension_bzl_path: setup.extension_bzl_path.dupe(),
        extension_name: setup.extension_name.dupe(),
        repo_name: Arc::from(""),
        extension_usages_key: setup.extension_usages_key.dupe(),
        extension_usages_json: setup.extension_usages_json.dupe(),
    }
}

fn bzlmod_module_extension_evaluation_working_dir(
    generated_repo_path: &ProjectRelativePath,
    setup: &BzlmodModuleExtensionRepoSetup,
) -> ProjectRelativePathBuf {
    let external_cells_root = generated_repo_path
        .as_str()
        .rsplit_once(BZLMOD_GENERATED_EXTERNAL_CELL_PATH_MARKER)
        .map(|(external_cells_root, _)| external_cells_root)
        .unwrap_or_else(|| {
            generated_repo_path
                .as_str()
                .rsplit_once('/')
                .map(|(parent, _)| parent)
                .unwrap_or("")
        });
    ProjectRelativePathBuf::unchecked_new(format!(
        "{}/bzlmod_module_extensions/{}",
        external_cells_root,
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
#[display("{setup:?}")]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodModuleExtensionEvaluationKey {
    setup: BzlmodModuleExtensionRepoSetup,
    working_dir: ProjectRelativePathBuf,
}

#[async_trait::async_trait]
impl Key for BzlmodModuleExtensionEvaluationKey {
    type Value = buck2_error::Result<Arc<BazelModuleExtensionEvaluationResult>>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        cancellations: &CancellationContext,
    ) -> Self::Value {
        ctx.get_blocking_executor()
            .execute_io(
                Box::new(
                    buck2_execute::execute::clean_output_paths::CleanOutputPaths {
                        paths: vec![self.working_dir.clone()],
                    },
                ),
                cancellations,
            )
            .await?;
        Ok(Arc::new(
            evaluate_bzlmod_module_extension_repo(
                ctx,
                &self.setup,
                self.working_dir.as_str(),
                None,
                cancellations,
            )
            .await?,
        ))
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

async fn evaluate_cached_bzlmod_module_extension(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
    generated_repo_path: &ProjectRelativePath,
) -> buck2_error::Result<Arc<BazelModuleExtensionEvaluationResult>> {
    let setup = bzlmod_module_extension_evaluation_setup(setup);
    let working_dir = bzlmod_module_extension_evaluation_working_dir(generated_repo_path, &setup);
    ctx.compute(&BzlmodModuleExtensionEvaluationKey { setup, working_dir })
        .await?
}

async fn evaluate_and_materialize_bzlmod_repository_rule(
    ctx: &mut DiceComputations<'_>,
    canonical_repo_name: &str,
    path: &ProjectRelativePath,
    invocation: &BazelRepositoryRuleInvocation,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    ctx.get_blocking_executor()
        .execute_io(
            Box::new(
                buck2_execute::execute::clean_output_paths::CleanOutputPaths {
                    paths: vec![path.to_owned()],
                },
            ),
            cancellations,
        )
        .await?;
    let files = evaluate_bzlmod_repository_rule(ctx, invocation, path.as_str(), cancellations)
        .await?
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
                            source_dir: None,
                        },
                    ),
                },
                dest: path.to_owned(),
            }),
            cancellations,
        )
        .await
}

async fn download_impl(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodCellSetup,
    dest: &ProjectRelativePath,
    cache_repo: &ProjectRelativePath,
    cache_tmp: &ProjectRelativePath,
    cache_alias: &ProjectRelativePath,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    if prepare_bzlmod_external_cell_root_if_cache_exists(
        ctx,
        cache_repo,
        cache_alias,
        dest,
        cancellations,
    )
    .await?
    {
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

    io.execute_io(
        Box::new(
            buck2_execute::execute::clean_output_paths::CleanOutputPaths {
                paths: vec![
                    stamp,
                    dest.to_owned(),
                    archive.clone(),
                    temp.clone(),
                    patch_dir.clone(),
                    overlay_dir.clone(),
                    cache_tmp.to_owned(),
                ],
            },
        ),
        cancellations,
    )
    .await?;

    let io_provider = ctx.global_data().get_io_provider();
    let project_root = io_provider.project_root();
    let digest_config = ctx.global_data().get_digest_config();
    let client = ctx.per_transaction_data().get_http_client();
    let bazel_download_headers = bazel_repository_download_headers(std::iter::empty());
    let archive_checksum = checksum_from_integrity(&setup.integrity)?;
    let archive_urls = bzlmod_cell_setup_urls(setup);
    http_download_any_with_headers(
        &client,
        project_root,
        digest_config.dupe(),
        &archive,
        &archive_urls,
        &archive_checksum,
        false,
        &bazel_download_headers,
    )
    .await?;

    for (patch, output) in setup.patches.iter().zip(&patch_files) {
        if patch.path.is_some() {
            continue;
        }
        let checksum = checksum_from_integrity(&patch.integrity)?;
        http_download_with_headers(
            &client,
            project_root,
            digest_config.dupe(),
            &output.path,
            &patch.url,
            &checksum,
            false,
            &bazel_download_headers,
        )
        .await?;
    }

    for (overlay, output) in setup.overlays.iter().zip(&overlay_files) {
        let checksum = checksum_from_integrity(&overlay.integrity)?;
        http_download_with_headers(
            &client,
            project_root,
            digest_config.dupe(),
            &output.file,
            &overlay.url,
            &checksum,
            false,
            &bazel_download_headers,
        )
        .await?;
    }

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
    )
    .await?;

    Ok(())
}

async fn prepare_bzlmod_external_cell_root_if_cache_exists(
    ctx: &mut DiceComputations<'_>,
    cache_repo: &ProjectRelativePath,
    cache_alias: &ProjectRelativePath,
    dest: &ProjectRelativePath,
    cancellations: &CancellationContext,
) -> buck2_error::Result<bool> {
    let io = ctx.get_blocking_executor();
    let project_root = ctx.global_data().get_io_provider().project_root().dupe();
    let cache_repo_abs = project_root.resolve(cache_repo);
    let hit = io
        .execute_io_inline(move || {
            Ok(matches!(
                fs_util::symlink_metadata_if_exists(&cache_repo_abs)?,
                Some(metadata) if metadata.is_dir()
            ))
        })
        .await?;
    if !hit {
        return Ok(false);
    }

    io.execute_io(
        Box::new(BzlmodPrepareExternalCellRootIoRequest {
            cache_repo: cache_repo.to_owned(),
            cache_alias: Some(cache_alias.to_owned()),
            dest: dest.to_owned(),
        }),
        cancellations,
    )
    .await?;
    Ok(true)
}

async fn prepare_bzlmod_external_cell_root_from_source(
    ctx: &mut DiceComputations<'_>,
    source: &ProjectRelativePath,
    dest: &ProjectRelativePath,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
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

async fn http_download_any_with_headers(
    client: &buck2_http::HttpClient,
    fs: &ProjectRoot,
    digest_config: buck2_execute::digest_config::DigestConfig,
    path: &ProjectRelativePath,
    urls: &[String],
    checksum: &Checksum,
    executable: bool,
    headers: &[(String, String)],
) -> buck2_error::Result<()> {
    let mut last_error = None;
    for url in urls {
        match http_download_with_headers(
            client,
            fs,
            digest_config.dupe(),
            path,
            url,
            checksum,
            executable,
            headers,
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

async fn declare_existing_directory(
    ctx: &mut DiceComputations<'_>,
    dest: &ProjectRelativePath,
    materializer: &dyn Materializer,
) -> buck2_error::Result<()> {
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
) -> buck2_error::Result<()> {
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
) -> buck2_error::Result<()> {
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
) -> buck2_error::Result<()> {
    let lock = bzlmod_materialization_lock(path);
    let _guard = lock.lock().await;
    let cache_key = bzlmod_repo_contents_cache_key(setup);
    let cache_repo = bzlmod_repo_contents_cache_path(&cache_key, "repo");
    let cache_tmp =
        bzlmod_repo_contents_cache_path(&cache_key, &format!("repo.tmp.{}", std::process::id()));
    let cache_alias = bzlmod_repo_contents_cache_alias_path(&setup.canonical_repo_name);
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
            )
        })
        .await
}

async fn materialize_generated(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    let lock = bzlmod_materialization_lock(path);
    let _guard = lock.lock().await;
    if bzlmod_generated_materialization_is_current(ctx, path, setup).await? {
        return Ok(());
    }

    cancellations
        .critical_section(|| async move {
            let stamp_path = bzlmod_generated_materialization_stamp_path(setup, path);
            ctx.get_blocking_executor()
                .execute_io(
                    Box::new(
                        buck2_execute::execute::clean_output_paths::CleanOutputPaths {
                            paths: vec![stamp_path],
                        },
                    ),
                    cancellations,
                )
                .await?;
            match &setup.generator {
                BzlmodGeneratedCellGenerator::ModuleExtensionRepo(module_extension) => {
                    let evaluation =
                        evaluate_cached_bzlmod_module_extension(ctx, module_extension, path)
                            .await?;
                    if let Some(invocation) = evaluation
                        .repository_rule_invocations
                        .iter()
                        .find(|invocation| invocation.name == module_extension.repo_name.as_ref())
                    {
                        let mut invocation = invocation.clone();
                        invocation.name = setup.canonical_repo_name.to_string();
                        evaluate_and_materialize_bzlmod_repository_rule(
                            ctx,
                            &setup.canonical_repo_name,
                            path,
                            &invocation,
                            cancellations,
                        )
                        .await?;
                    } else {
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
                    }
                }
                BzlmodGeneratedCellGenerator::RepositoryRuleInvocation(invocation_setup) => {
                    let invocation = bzlmod_repository_rule_invocation_from_setup(
                        invocation_setup,
                        &setup.canonical_repo_name,
                    )?;
                    evaluate_and_materialize_bzlmod_repository_rule(
                        ctx,
                        &setup.canonical_repo_name,
                        path,
                        &invocation,
                        cancellations,
                    )
                    .await?;
                }
                BzlmodGeneratedCellGenerator::HttpArchive(http_archive) => {
                    let archive = bzlmod_generated_sibling_path(setup, path, "source.archive");
                    let temp = bzlmod_generated_sibling_path(setup, path, "extract-tmp");
                    ctx.get_blocking_executor()
                        .execute_io(
                            Box::new(
                                buck2_execute::execute::clean_output_paths::CleanOutputPaths {
                                    paths: vec![path.to_owned(), archive.clone(), temp.clone()],
                                },
                            ),
                            cancellations,
                        )
                        .await?;
                    let io_provider = ctx.global_data().get_io_provider();
                    let project_root = io_provider.project_root();
                    let digest_config = ctx.global_data().get_digest_config();
                    let client = ctx.per_transaction_data().get_http_client();
                    let bazel_download_headers =
                        bazel_repository_download_headers(std::iter::empty());
                    let archive_checksum = Checksum::new(None, Some(&*http_archive.sha256))?;
                    http_download_with_headers(
                        &client,
                        project_root,
                        digest_config.dupe(),
                        &archive,
                        &http_archive.url,
                        &archive_checksum,
                        false,
                        &bazel_download_headers,
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
                }
                _ => {
                    ctx.get_blocking_executor()
                        .execute_io(
                            Box::new(
                                buck2_execute::execute::clean_output_paths::CleanOutputPaths {
                                    paths: vec![path.to_owned()],
                                },
                            ),
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
                }
            }
            write_bzlmod_generated_materialization_stamp(ctx, path, setup).await?;
            Ok(())
        })
        .await
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
#[display("({}, {})", path, setup)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct BzlmodGeneratedCellMaterializationKey {
    path: ProjectRelativePathBuf,
    setup: BzlmodGeneratedCellSetup,
}

#[async_trait::async_trait]
impl Key for BzlmodGeneratedCellMaterializationKey {
    type Value = buck2_error::Result<()>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        cancellations: &CancellationContext,
    ) -> Self::Value {
        materialize_generated(ctx, &self.path, &self.setup, cancellations).await
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
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

async fn ensure_generated_materialized(
    ctx: &mut DiceComputations<'_>,
    path: ProjectRelativePathBuf,
    setup: BzlmodGeneratedCellSetup,
) -> buck2_error::Result<()> {
    ctx.compute(&BzlmodGeneratedCellMaterializationKey { path, setup })
        .await?
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
    ) -> buck2_error::Result<ReadFileProxy> {
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
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
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

    async fn read_path_metadata_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
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
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
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
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(None, path)
            .await
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_with_cache(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(cache, path)
            .await
    }

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
    }
}

impl BzlmodFileOpsDelegate {
    async fn read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(
        &self,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
        path: &CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
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
}

fn follow_bzlmod_symlinked_directory_entries(
    project_root: &ProjectRoot,
    project_path: &ProjectRelativePath,
    entries: &mut [RawDirEntry],
) -> buck2_error::Result<()> {
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

#[pagable_typetag]
#[async_trait::async_trait]
impl FileOpsDelegate for BzlmodGeneratedFileOpsDelegate {
    async fn read_file_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<ReadFileProxy> {
        ensure_generated_materialized(ctx, self.get_base_path(), self.setup.dupe()).await?;
        Ok(ReadFileProxy::new_with_captures(
            (self.resolve(path), self.io.dupe()),
            |(project_path, io)| async move {
                (&io as &dyn IoProvider)
                    .read_file_if_exists(project_path)
                    .await
            },
        ))
    }

    async fn read_dir(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
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
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
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

    async fn read_path_metadata_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
        ensure_generated_materialized(ctx, self.get_base_path(), self.setup.dupe()).await?;
        let project_path = self.resolve(path);
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists(project_path.clone())
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        declare_observed_source_artifact(ctx, project_path, &metadata).await?;
        Ok(Some(metadata.try_map(
            |path| match path.strip_prefix_opt(self.get_base_path()) {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    path,
                    self.cell
                )),
            },
        )?))
    }

    async fn read_path_metadata_if_exists_for_no_watchfs(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
        let project_path = self.resolve(path);
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists(project_path)
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        Ok(Some(metadata.try_map(
            |path| match path.strip_prefix_opt(self.get_base_path()) {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    path,
                    self.cell
                )),
            },
        )?))
    }

    async fn read_path_metadata_for_no_watchfs_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(None, path)
            .await
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_with_cache(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        self.read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(cache, path)
            .await
    }

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
    }
}

impl BzlmodGeneratedFileOpsDelegate {
    async fn read_path_metadata_for_no_watchfs_if_exists_with_cache_impl(
        &self,
        cache: Option<Arc<NoWatchFsMetadataCache>>,
        path: &CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        let project_path = self.resolve(path);
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists_for_no_watchfs_with_cache(project_path, cache)
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        Ok(Some(metadata.try_map(
            |path| match path.strip_prefix_opt(self.get_base_path()) {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    path,
                    self.cell
                )),
            },
        )?))
    }
}

pub(crate) async fn get_file_ops_delegate(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodCellSetup,
) -> buck2_error::Result<Arc<BzlmodFileOpsDelegate>> {
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
    #[display("({}, {})", _0, _1)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct BzlmodFileOpsDelegateKey(CellName, BzlmodCellSetup);

    #[async_trait::async_trait]
    impl Key for BzlmodFileOpsDelegateKey {
        type Value = buck2_error::Result<Arc<BzlmodFileOpsDelegate>>;

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

pub(crate) async fn get_generated_file_ops_delegate(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodGeneratedCellSetup,
) -> buck2_error::Result<Arc<BzlmodGeneratedFileOpsDelegate>> {
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
    #[display("({}, {})", _0, _1)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct BzlmodGeneratedFileOpsDelegateKey(CellName, BzlmodGeneratedCellSetup);

    #[async_trait::async_trait]
    impl Key for BzlmodGeneratedFileOpsDelegateKey {
        type Value = buck2_error::Result<Arc<BzlmodGeneratedFileOpsDelegate>>;

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

pub(crate) async fn materialize_all(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodCellSetup,
) -> buck2_error::Result<ProjectRelativePathBuf> {
    let ops = get_file_ops_delegate(ctx, cell, setup.dupe()).await?;
    let materializer = ctx.per_transaction_data().get_materializer();
    declare_existing_directory(ctx, &ops.backing_base_path, &*materializer).await?;
    Ok(ops.backing_base_path.clone())
}

pub(crate) async fn materialize_generated_all(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodGeneratedCellSetup,
) -> buck2_error::Result<ProjectRelativePathBuf> {
    let ops = get_generated_file_ops_delegate(ctx, cell, setup.dupe()).await?;
    ensure_generated_materialized(ctx, ops.get_base_path(), setup).await?;
    let materializer = ctx.per_transaction_data().get_materializer();
    declare_existing_directory(ctx, &ops.get_base_path(), &*materializer).await?;
    Ok(ops.get_base_path())
}
