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
use std::collections::BTreeSet;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;

use base64::Engine;
use buck2_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use buck2_common::dice::data::HasIoProvider;
use buck2_common::file_ops::delegate::FileOpsDelegate;
use buck2_common::file_ops::dice::ReadFileProxy;
use buck2_common::file_ops::metadata::FileDigestConfig;
use buck2_common::file_ops::metadata::RawDirEntry;
use buck2_common::file_ops::metadata::RawPathMetadata;
use buck2_common::http::HasHttpClient;
use buck2_common::io::IoProvider;
use buck2_common::io::fs::FsIoProvider;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::external::BzlmodBazelFeaturesGlobalsSetup;
use buck2_core::cells::external::BzlmodBazelFeaturesVersionSetup;
use buck2_core::cells::external::BzlmodCellSetup;
use buck2_core::cells::external::BzlmodGeneratedCellGenerator;
use buck2_core::cells::external::BzlmodGeneratedCellSetup;
use buck2_core::cells::external::BzlmodGoDepsModuleSetup;
use buck2_core::cells::external::BzlmodGoDepsRepositoryConfigSetup;
use buck2_core::cells::external::BzlmodGoRegisterNogoSetup;
use buck2_core::cells::external::BzlmodHostPlatformSetup;
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
use buck2_execute::directory::INTERNER;
use buck2_execute::entry::build_entry_from_disk;
use buck2_execute::execute::blocking::HasBlockingExecutor;
use buck2_execute::execute::blocking::IoRequest;
use buck2_execute::materialize::http::Checksum;
use buck2_execute::materialize::http::http_download;
use buck2_execute::materialize::materializer::DeclareArtifactPayload;
use buck2_execute::materialize::materializer::HasMaterializer;
use buck2_execute::materialize::materializer::Materializer;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use cmp_any::PartialEqAny;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use pagable::Pagable;
use pagable::pagable_typetag;
use serde::Deserialize;

#[derive(buck2_error::Error, Debug)]
#[buck2(tag = Tier0)]
enum BzlmodError {
    #[error("Unsupported bzlmod archive type for `{0}`")]
    UnsupportedArchiveType(String),
    #[error("Error extracting bzlmod module, exit code: {exit_code:?}, stderr:\n{stderr}")]
    ExtractFailed {
        exit_code: ExitStatus,
        stderr: String,
    },
    #[error("Error applying bzlmod patch, exit code: {exit_code:?}, stderr:\n{stderr}")]
    PatchFailed {
        exit_code: ExitStatus,
        stderr: String,
    },
    #[error("Expected extracted bzlmod module directory at `{0}`")]
    MissingExtractedDirectory(String),
    #[error("Expected bzlmod materialization to create a directory")]
    NoDirectory,
    #[error("Invalid bzlmod integrity `{0}`")]
    InvalidIntegrity(String),
    #[error("Generated go_deps repo `{repo_name}` was not found in `{go_mod}`")]
    GoDepsRepoNotFound { repo_name: String, go_mod: String },
    #[error(
        "Error downloading Go module `{module}` with `go mod download`, exit code: {exit_code:?}, stdout:\n{stdout}\nstderr:\n{stderr}"
    )]
    GoModDownloadFailed {
        module: String,
        exit_code: ExitStatus,
        stdout: String,
        stderr: String,
    },
    #[error("`go mod download -json {module}` did not return a module directory")]
    GoModDownloadMissingDir { module: String },
    #[error("Invalid generated bzlmod repo path `{0}`")]
    InvalidGeneratedRepoPath(String),
    #[error("Could not find `{dict}` in bazel_features globals at `{path}`")]
    MissingBazelFeaturesGlobalsDict { path: String, dict: &'static str },
}

struct BzlmodExtractIoRequest {
    setup: BzlmodCellSetup,
    archive: ProjectRelativePathBuf,
    patch_files: Vec<ProjectRelativePathBuf>,
    temp: ProjectRelativePathBuf,
    dest: ProjectRelativePathBuf,
}

impl IoRequest for BzlmodExtractIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
        let archive = project_fs.resolve(&self.archive);
        let temp = project_fs.resolve(&self.temp);
        let dest = project_fs.resolve(&self.dest);

        fs_util::create_dir_all(temp.clone())?;
        fs_util::create_dir_all(dest.clone())?;

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

        copy_dir_contents(&source, &dest)?;

        for patch in &self.patch_files {
            apply_patch(project_fs, &dest, patch, self.setup.patch_strip)?;
        }

        Ok(())
    }
}

struct BzlmodGeneratedIoRequest {
    setup: BzlmodGeneratedCellSetup,
    dest: ProjectRelativePathBuf,
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
            BzlmodGeneratedCellGenerator::GoRegisterNogo(setup) => {
                write_go_register_nogo_repo(&dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::GoDepsModule(setup) => {
                write_go_deps_module_repo(project_fs, &self.dest, &dest, setup)?;
            }
            BzlmodGeneratedCellGenerator::GoDepsRepositoryConfig(setup) => {
                write_go_deps_repository_config_repo(&dest, setup)?;
            }
        }
        Ok(())
    }
}

fn write_go_register_nogo_repo(
    dest: &AbsNormPath,
    setup: &BzlmodGoRegisterNogoSetup,
) -> buck2_error::Result<()> {
    write_generated_module_file(dest, "io_bazel_rules_nogo")?;
    let build = format!(
        "package(default_visibility = [\"//visibility:public\"])\n\nalias(\n    name = \"nogo\",\n    actual = \"{}\",\n)\n\nexports_files([\"scope.bzl\"])\n",
        setup.nogo
    );
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    let scope = format!(
        "INCLUDES = {}\nEXCLUDES = {}\n",
        scope_list_repr(&setup.includes),
        scope_list_repr(&setup.excludes),
    );
    fs_util::write(dest.join(ForwardRelativePath::new("scope.bzl")?), scope)
        .categorize_internal()?;
    Ok(())
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
    let Some((external_cells_root, _)) = dest_rel.as_str().split_once("/bzlmod_generated/") else {
        return Err(BzlmodError::InvalidGeneratedRepoPath(dest_rel.to_string()).into());
    };
    let globals_path = ProjectRelativePathBuf::unchecked_new(format!(
        "{external_cells_root}/bzlmod/{}/private/globals.bzl",
        setup.parent_canonical_repo_name
    ));
    let globals_text = fs_util::read_to_string(project_fs.resolve(&globals_path))
        .categorize_internal()
        .with_buck_error_context(|| {
            format!("Error reading bazel_features globals `{}`", globals_path)
        })?;
    let globals = parse_bazel_features_string_dict(&globals_text, "GLOBALS", &globals_path)?;
    let legacy_globals =
        parse_bazel_features_string_dict(&globals_text, "LEGACY_GLOBALS", &globals_path)?;

    write_generated_module_file(dest, "bazel_features_globals")?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("BUILD.bazel")?),
        "load(\"@bazel_skylib//:bzl_library.bzl\", \"bzl_library\")\n\nexports_files([\"globals.bzl\"])\n\nbzl_library(\n    name = \"globals\",\n    srcs = [\"globals.bzl\"],\n    visibility = [\"//visibility:public\"],\n)\n",
    )
    .categorize_internal()?;

    let mut globals_bzl = String::from("globals = struct(\n");
    for (global, version) in globals {
        let value = if bazel_version_ge(&setup.bazel_version, &version) {
            global.as_str()
        } else {
            "None"
        };
        globals_bzl.push_str(&format!("    {global} = {value},\n"));
    }
    for (global, version) in legacy_globals {
        let value = if bazel_version_lt(&setup.bazel_version, &version) {
            format!("getattr(getattr(native, 'legacy_globals', None), {global:?}, {global})")
        } else {
            "None".to_owned()
        };
        globals_bzl.push_str(&format!("    {global} = {value},\n"));
    }
    globals_bzl.push_str(")\n");
    fs_util::write(
        dest.join(ForwardRelativePath::new("globals.bzl")?),
        globals_bzl,
    )
    .categorize_internal()?;
    Ok(())
}

fn parse_bazel_features_string_dict(
    text: &str,
    dict: &'static str,
    path: &ProjectRelativePath,
) -> buck2_error::Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    let mut in_dict = false;
    let start = format!("{dict} = {{");

    for line in text.lines() {
        let line = strip_starlark_line_comment(line).trim();
        if !in_dict {
            if line == start {
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
        let Some(value) = parse_simple_bzl_string(value) else {
            continue;
        };
        values.insert(key, value);
    }

    Err(BzlmodError::MissingBazelFeaturesGlobalsDict {
        path: path.to_string(),
        dict,
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

fn scope_list_repr(scopes: &[Arc<str>]) -> String {
    if scopes.iter().any(|scope| scope.as_ref() == "all") {
        return "[\"all\"]".to_owned();
    }
    let labels = scopes
        .iter()
        .map(|scope| {
            let scope = scope
                .strip_prefix("@@//")
                .map_or_else(|| scope.to_string(), |rest| format!("root//{rest}"));
            format!("Label({scope:?})")
        })
        .collect::<Vec<_>>();
    format!("[{}]", labels.join(", "))
}

#[derive(Debug, Clone)]
struct GoRequire {
    path: String,
    version: String,
}

#[derive(Deserialize)]
struct GoModDownload {
    #[serde(rename = "Dir")]
    dir: Option<String>,
    #[serde(rename = "Error")]
    error: Option<String>,
}

fn write_go_deps_module_repo(
    project_fs: &ProjectRoot,
    dest_rel: &ProjectRelativePath,
    dest: &AbsNormPath,
    setup: &BzlmodGoDepsModuleSetup,
) -> buck2_error::Result<()> {
    let Some((external_cells_root, _)) = dest_rel.as_str().split_once("/bzlmod_generated/") else {
        return Err(BzlmodError::InvalidGeneratedRepoPath(dest_rel.to_string()).into());
    };
    let parent_go_mod = ProjectRelativePathBuf::unchecked_new(format!(
        "{external_cells_root}/bzlmod/{}/{}",
        setup.parent_canonical_repo_name, setup.go_mod
    ));
    let go_mod_text = fs_util::read_to_string(project_fs.resolve(&parent_go_mod))
        .categorize_internal()
        .with_buck_error_context(|| format!("Error reading parent go.mod `{}`", parent_go_mod))?;
    let requires = parse_go_mod_requires(&go_mod_text);
    let Some(module) = requires
        .iter()
        .find(|require| go_module_repo_name(&require.path) == setup.repo_name.as_ref())
        .cloned()
    else {
        return Err(BzlmodError::GoDepsRepoNotFound {
            repo_name: setup.repo_name.to_string(),
            go_mod: setup.go_mod.to_string(),
        }
        .into());
    };

    let module_dir = go_mod_download(&module)?;
    copy_dir_contents(&module_dir, dest)?;
    write_generated_module_file(dest, &setup.repo_name)?;
    write_go_module_build_files(dest, &module.path, &requires)?;
    Ok(())
}

fn write_go_deps_repository_config_repo(
    dest: &AbsNormPath,
    setup: &BzlmodGoDepsRepositoryConfigSetup,
) -> buck2_error::Result<()> {
    write_generated_module_file(dest, "bazel_gazelle_go_repository_config")?;
    let go_env: serde_json::Value = serde_json::from_str(&setup.go_env_json)
        .buck_error_context("Invalid go_env_json for go_deps repository config")?;
    let config = serde_json::json!({
        "go_env": go_env,
        "dep_files": setup.deps_files.iter().map(|file| file.as_ref()).collect::<Vec<_>>(),
    });
    let config_json = serde_json::to_string_pretty(&config)
        .buck_error_context("Error serializing go_deps repository config")?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("config.json")?),
        config_json,
    )
    .categorize_internal()?;
    fs_util::write(
        dest.join(ForwardRelativePath::new("BUILD.bazel")?),
        "package(default_visibility = [\"//visibility:public\"])\n\nexports_files([\"config.json\"])\n",
    )
    .categorize_internal()?;
    Ok(())
}

fn write_generated_module_file(dest: &AbsNormPath, name: &str) -> buck2_error::Result<()> {
    fs_util::write(
        dest.join(ForwardRelativePath::new("MODULE.bazel")?),
        format!("module(name = {name:?})\n"),
    )
    .categorize_internal()?;
    Ok(())
}

fn go_mod_download(module: &GoRequire) -> buck2_error::Result<AbsNormPathBuf> {
    let module_arg = format!("{}@{}", module.path, module.version);
    let output = Command::new("go")
        .arg("mod")
        .arg("download")
        .arg("-json")
        .arg(&module_arg)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .buck_error_context("Could not run `go mod download` for go_deps generated repo")?;

    if !output.status.success() {
        return Err(BzlmodError::GoModDownloadFailed {
            module: module_arg,
            exit_code: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
        .into());
    }

    let download: GoModDownload = serde_json::from_slice(&output.stdout)
        .with_buck_error_context(|| format!("Invalid JSON from `go mod download {module_arg}`"))?;
    if let Some(error) = download.error {
        return Err(BzlmodError::GoModDownloadFailed {
            module: module_arg,
            exit_code: output.status,
            stdout: error,
            stderr: String::new(),
        }
        .into());
    }
    let Some(dir) = download.dir else {
        return Err(BzlmodError::GoModDownloadMissingDir { module: module_arg }.into());
    };
    Ok(AbsNormPath::new(&dir)?.to_owned())
}

fn parse_go_mod_requires(go_mod: &str) -> Vec<GoRequire> {
    let mut requires = Vec::new();
    let mut in_block = false;

    for line in go_mod.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if in_block {
            if line == ")" {
                in_block = false;
                continue;
            }
            push_go_require(line, &mut requires);
            continue;
        }
        if line == "require (" {
            in_block = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("require ") {
            push_go_require(rest, &mut requires);
        }
    }

    requires
}

fn push_go_require(line: &str, requires: &mut Vec<GoRequire>) {
    let mut parts = line.split_whitespace();
    let Some(path) = parts.next() else {
        return;
    };
    let Some(version) = parts.next() else {
        return;
    };
    requires.push(GoRequire {
        path: path.to_owned(),
        version: version.to_owned(),
    });
}

fn go_module_repo_name(module_path: &str) -> String {
    let mut parts = module_path.split('/');
    let mut repo_parts = Vec::new();
    if let Some(domain) = parts.next() {
        repo_parts.extend(domain.split('.').rev().map(sanitize_go_repo_part));
    }
    repo_parts.extend(parts.map(sanitize_go_repo_part));
    repo_parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn sanitize_go_repo_part(part: &str) -> String {
    let mut out = String::new();
    let mut previous_was_separator = false;
    for ch in part.chars() {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !previous_was_separator {
            out.push('_');
            previous_was_separator = true;
        }
    }
    out.trim_matches('_').to_owned()
}

fn write_go_module_build_files(
    repo_root: &AbsNormPath,
    module_path: &str,
    requires: &[GoRequire],
) -> buck2_error::Result<()> {
    let mut packages = Vec::new();
    collect_go_package_dirs(repo_root, "", &mut packages)?;
    let require_repos = requires
        .iter()
        .map(|require| (require.path.clone(), go_module_repo_name(&require.path)))
        .collect::<BTreeMap<_, _>>();

    for package in packages {
        write_go_package_build(repo_root, &package, module_path, &require_repos)?;
    }
    Ok(())
}

fn collect_go_package_dirs(
    dir: &AbsNormPath,
    relative: &str,
    packages: &mut Vec<String>,
) -> buck2_error::Result<()> {
    let mut has_go_src = false;
    let mut children = Vec::new();
    for entry in fs_util::read_dir(dir).categorize_internal()? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if name == "vendor" || name == "testdata" || name.starts_with('.') {
                continue;
            }
            children.push(name);
        } else if file_type.is_file() && is_go_library_source(&name) {
            has_go_src = true;
        }
    }

    if has_go_src {
        packages.push(relative.to_owned());
    }

    children.sort();
    for child in children {
        let child_relative = if relative.is_empty() {
            child.clone()
        } else {
            format!("{relative}/{child}")
        };
        collect_go_package_dirs(
            &dir.join(ForwardRelativePath::new(&child)?),
            &child_relative,
            packages,
        )?;
    }

    Ok(())
}

fn write_go_package_build(
    repo_root: &AbsNormPath,
    package: &str,
    module_path: &str,
    require_repos: &BTreeMap<String, String>,
) -> buck2_error::Result<()> {
    let package_dir = if package.is_empty() {
        repo_root.to_owned()
    } else {
        repo_root.join(ForwardRelativePath::new(package)?)
    };
    let mut srcs = Vec::new();
    let mut imports = BTreeSet::new();

    for entry in fs_util::read_dir(&package_dir).categorize_internal()? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file() && is_go_library_source(&name) {
            srcs.push(name.clone());
            let source = fs_util::read_to_string(entry.path()).categorize_internal()?;
            imports.extend(parse_go_imports(&source));
        }
    }

    srcs.sort();
    let importpath = if package.is_empty() {
        module_path.to_owned()
    } else {
        format!("{module_path}/{package}")
    };
    let deps = imports
        .into_iter()
        .filter_map(|import| go_import_dep_label(&import, &importpath, module_path, require_repos))
        .collect::<BTreeSet<_>>();

    let mut build = String::new();
    build.push_str("load(\"@io_bazel_rules_go//go:def.bzl\", \"go_library\")\n\n");
    build.push_str("go_library(\n");
    build.push_str("    name = \"go_default_library\",\n");
    build.push_str("    srcs = [\n");
    for src in srcs {
        build.push_str(&format!("        {src:?},\n"));
    }
    build.push_str("    ],\n");
    build.push_str(&format!("    importpath = {importpath:?},\n"));
    if !deps.is_empty() {
        build.push_str("    deps = [\n");
        for dep in deps {
            build.push_str(&format!("        {dep:?},\n"));
        }
        build.push_str("    ],\n");
    }
    build.push_str("    visibility = [\"//visibility:public\"],\n");
    build.push_str(")\n");

    fs_util::write(
        package_dir.join(ForwardRelativePath::new("BUILD.bazel")?),
        build,
    )
    .categorize_internal()?;
    Ok(())
}

fn is_go_library_source(name: &str) -> bool {
    name.ends_with(".go") && !name.ends_with("_test.go")
}

fn parse_go_imports(source: &str) -> BTreeSet<String> {
    let mut imports = BTreeSet::new();
    let mut in_block = false;

    for line in source.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if in_block {
            if line.starts_with(')') {
                in_block = false;
            } else if let Some(import) = go_import_string(line) {
                imports.insert(import);
            }
            continue;
        }
        if line == "import (" {
            in_block = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("import ") {
            if let Some(import) = go_import_string(rest.trim()) {
                imports.insert(import);
            }
        }
    }

    imports
}

fn go_import_string(line: &str) -> Option<String> {
    let start = line.find(['"', '`'])?;
    let quote = line[start..].chars().next()?;
    let rest = &line[start + quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_owned())
}

fn go_import_dep_label(
    import: &str,
    current_importpath: &str,
    module_path: &str,
    require_repos: &BTreeMap<String, String>,
) -> Option<String> {
    if import == current_importpath || !go_import_is_external(import) {
        return None;
    }
    if import == module_path || import.starts_with(&format!("{module_path}/")) {
        let package = import
            .strip_prefix(module_path)
            .unwrap_or("")
            .trim_matches('/');
        return Some(if package.is_empty() {
            "//:go_default_library".to_owned()
        } else {
            format!("//{package}:go_default_library")
        });
    }

    let (module, repo) = require_repos
        .iter()
        .filter(|(module, _)| {
            import == module.as_str() || import.starts_with(&format!("{}/", module))
        })
        .max_by_key(|(module, _)| module.len())?;
    let package = import.strip_prefix(module).unwrap_or("").trim_matches('/');
    Some(if package.is_empty() {
        format!("@{repo}//:go_default_library")
    } else {
        format!("@{repo}//{package}:go_default_library")
    })
}

fn go_import_is_external(import: &str) -> bool {
    import
        .split('/')
        .next()
        .is_some_and(|first| first.contains('.'))
}

fn extract_archive(
    setup: &BzlmodCellSetup,
    archive: &AbsNormPath,
    temp: &AbsNormPath,
) -> buck2_error::Result<()> {
    let archive_type = setup
        .archive_type
        .as_deref()
        .or_else(|| archive.as_path().extension().and_then(|ext| ext.to_str()))
        .unwrap_or("");

    let mut command = if archive_type == "zip" || setup.url.ends_with(".zip") {
        let mut command = Command::new("unzip");
        command
            .arg("-q")
            .arg(archive.as_path())
            .arg("-d")
            .arg(temp.as_path());
        command
    } else if matches!(
        archive_type,
        "tar" | "gz" | "tgz" | "tar.gz" | "tar.xz" | "tar.bz2"
    ) || setup.url.ends_with(".tar.gz")
        || setup.url.ends_with(".tgz")
        || setup.url.ends_with(".tar.xz")
        || setup.url.ends_with(".tar.bz2")
        || setup.url.ends_with(".tar")
    {
        let mut command = Command::new("tar");
        command
            .arg("-xf")
            .arg(archive.as_path())
            .arg("-C")
            .arg(temp.as_path());
        command
    } else {
        return Err(BzlmodError::UnsupportedArchiveType(setup.url.to_string()).into());
    };

    let output = command
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .buck_error_context("Could not run archive extractor for bzlmod external cell")?;

    if !output.status.success() {
        return Err(BzlmodError::ExtractFailed {
            exit_code: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
        .into());
    }

    Ok(())
}

fn apply_patch(
    project_fs: &ProjectRoot,
    dest: &AbsNormPath,
    patch: &ProjectRelativePath,
    patch_strip: u32,
) -> buck2_error::Result<()> {
    let patch = project_fs.resolve(patch);
    let output = Command::new("patch")
        .current_dir(dest.as_path())
        .arg(format!("-p{patch_strip}"))
        .arg("-i")
        .arg(patch.as_path())
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .buck_error_context("Could not run patch for bzlmod external cell")?;

    if !output.status.success() {
        return Err(BzlmodError::PatchFailed {
            exit_code: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
        .into());
    }

    Ok(())
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
            fs_util::copy(&from_path, &to_path).categorize_internal()?;
        } else if file_type.is_symlink() {
            let target = fs_util::read_link(&from_path).categorize_internal()?;
            fs_util::symlink(target, &to_path).categorize_internal()?;
        }
    }
    Ok(())
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

fn bzlmod_path(setup: &BzlmodCellSetup, suffix: &str) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/external_cells/bzlmod/{}/{}",
        setup.canonical_repo_name, suffix
    ))
}

async fn download_impl(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodCellSetup,
    dest: &ProjectRelativePath,
    materializer: &dyn Materializer,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    let io = ctx.get_blocking_executor();
    let archive = bzlmod_path(setup, "source.archive");
    let temp = bzlmod_path(setup, "extract-tmp");
    let patch_dir = bzlmod_path(setup, "patches");
    let patch_files: Vec<_> = setup
        .patches
        .iter()
        .enumerate()
        .map(|(idx, _)| patch_dir.join(ForwardRelativePath::new(&format!("{idx}.patch")).unwrap()))
        .collect();

    io.execute_io(
        Box::new(
            buck2_execute::execute::clean_output_paths::CleanOutputPaths {
                paths: vec![
                    dest.to_owned(),
                    archive.clone(),
                    temp.clone(),
                    patch_dir.clone(),
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
    let archive_checksum = Checksum::new(None, Some(&integrity_to_sha256_hex(&setup.integrity)?))?;
    http_download(
        &client,
        project_root,
        digest_config.dupe(),
        &archive,
        &setup.url,
        &archive_checksum,
        false,
    )
    .await?;

    for (patch, output) in setup.patches.iter().zip(&patch_files) {
        let checksum = Checksum::new(None, Some(&integrity_to_sha256_hex(&patch.integrity)?))?;
        http_download(
            &client,
            project_root,
            digest_config.dupe(),
            output,
            &patch.url,
            &checksum,
            false,
        )
        .await?;
    }

    io.execute_io(
        Box::new(BzlmodExtractIoRequest {
            setup: setup.dupe(),
            archive,
            patch_files,
            temp,
            dest: dest.to_owned(),
        }),
        cancellations,
    )
    .await?;

    declare_existing_directory(ctx, dest, materializer).await
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

async fn download_and_materialize(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodCellSetup,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    let materializer = ctx.per_transaction_data().get_materializer();

    if materializer.has_artifact_at(path.to_owned()).await? {
        return Ok(());
    }

    cancellations
        .critical_section(|| download_impl(ctx, setup, path, &*materializer, cancellations))
        .await
}

async fn materialize_generated(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    let materializer = ctx.per_transaction_data().get_materializer();

    if materializer.has_artifact_at(path.to_owned()).await? {
        return Ok(());
    }

    cancellations
        .critical_section(|| async move {
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
            declare_existing_directory(ctx, path, &*materializer).await
        })
        .await
}

#[derive(allocative::Allocative, Pagable)]
pub(crate) struct BzlmodFileOpsDelegate {
    buck_out_resolver: BuckOutPathResolver,
    cell: CellName,
    setup: BzlmodCellSetup,
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
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        let project_path = self.resolve(path);
        let mut entries = (&self.io as &dyn IoProvider)
            .read_dir(project_path)
            .await
            .with_buck_error_context(|| format!("Error listing dir `{path}`"))?;

        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(entries.into())
    }

    async fn read_path_metadata_if_exists(
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

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
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

#[pagable_typetag]
#[async_trait::async_trait]
impl FileOpsDelegate for BzlmodGeneratedFileOpsDelegate {
    async fn read_file_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<ReadFileProxy> {
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
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        let project_path = self.resolve(path);
        let mut entries = (&self.io as &dyn IoProvider)
            .read_dir(project_path)
            .await
            .with_buck_error_context(|| format!("Error listing dir `{path}`"))?;

        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(entries.into())
    }

    async fn read_path_metadata_if_exists(
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

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
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
            let ops = BzlmodFileOpsDelegate {
                buck_out_resolver: artifact_fs.buck_out_path_resolver().clone(),
                cell: self.0,
                setup: self.1.dupe(),
                io: FsIoProvider::new(
                    artifact_fs.fs().dupe(),
                    ctx.global_data().get_digest_config().cas_digest_config(),
                ),
            };
            download_and_materialize(ctx, &ops.get_base_path(), &self.1, cancellations).await?;
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
            cancellations: &CancellationContext,
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
            materialize_generated(ctx, &ops.get_base_path(), &self.1, cancellations).await?;
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
    Ok(ops.get_base_path())
}

pub(crate) async fn materialize_generated_all(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodGeneratedCellSetup,
) -> buck2_error::Result<ProjectRelativePathBuf> {
    let ops = get_generated_file_ops_delegate(ctx, cell, setup.dupe()).await?;
    Ok(ops.get_base_path())
}
