use std::collections::BTreeSet;
use std::fmt;

use allocative::Allocative;
use bz_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use bz_build_api::interpreter::rule_defs::bazel::depset::bazel_depset_get_singleton;
use bz_build_api::interpreter::rule_defs::bazel::depset::bazel_depset_is_singleton;
use bz_build_api::interpreter::rule_defs::bazel::depset::bazel_depset_to_list;
use bz_build_api::interpreter::rule_defs::context::bazel_analysis_context_declare_file;
use bz_build_api::interpreter::rule_defs::context::bazel_ctx_is_exec_configuration;
use bz_build_api::interpreter::rule_defs::provider::builtin::default_info::BazelRunfiles;
use bz_build_api::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_with_generated_inits_empty_files_supplier;
use bz_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use bz_core::cells::external::bzlmod_cell_aliases_for_cell;
use bz_core::cells::external::bzlmod_cell_name;
use bz_core::deferred::base_deferred_key::BaseDeferredKey;
use bz_core::package::PackageLabel;
use bz_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use bz_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use bz_interpreter::types::target_label::StarlarkConfiguredTargetLabel;
use bz_interpreter::types::target_label::StarlarkTargetLabel;
use fancy_regex::Regex;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::none::NoneOr;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum BazelPythonError {
    #[error("Invalid py_internal regex `{pattern}`: {error}")]
    InvalidRegex { pattern: String, error: String },
    #[error("Error matching py_internal regex `{pattern}`: {error}")]
    RegexMatch { pattern: String, error: String },
    #[error("py_internal.get_label_repo_runfiles_path expected Label, got `{0}`")]
    ExpectedLabel(String),
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelPyInternal;

impl fmt::Display for BazelPyInternal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("py_internal")
    }
}

starlark::starlark_simple_value!(BazelPyInternal);

#[starlark_value(type = "py_internal")]
impl<'v> StarlarkValue<'v> for BazelPyInternal {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_py_internal_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "cc_helper".to_owned(),
            "get_current_os_name".to_owned(),
            "get_label_repo_runfiles_path".to_owned(),
            "get_legacy_external_runfiles".to_owned(),
            "get_singleton_depset".to_owned(),
            "is_bzlmod_enabled".to_owned(),
            "is_singleton_depset".to_owned(),
            "is_tool_configuration".to_owned(),
            "declare_constant_metadata_file".to_owned(),
            "create_repo_mapping_manifest".to_owned(),
            "make_runfiles_respect_legacy_external_runfiles".to_owned(),
            "merge_runfiles_with_generated_inits_empty_files_supplier".to_owned(),
            "regex_match".to_owned(),
            "runfiles_enabled".to_owned(),
            "stamp_binaries".to_owned(),
        ]
    }
}

fn bazel_repo_name_for_cell(cell: &str) -> String {
    if cell == "root"
        || bzlmod_canonical_repo_name_for_cell(cell).is_some_and(|repo| repo.is_empty())
    {
        return String::new();
    }
    bzlmod_canonical_repo_name_for_cell(cell).unwrap_or_else(|| cell.to_owned())
}

fn label_repo_runfiles_path(package: PackageLabel) -> String {
    let cell = package.cell_name();
    let package_path = package.cell_relative_path().as_str();
    if cell.as_str() == "root"
        || bzlmod_canonical_repo_name_for_cell(cell.as_str()).is_some_and(|repo| repo.is_empty())
    {
        return package_path.to_owned();
    }
    let repo = bazel_repo_name_for_cell(cell.as_str());
    if package_path.is_empty() {
        format!("../{repo}")
    } else {
        format!("../{repo}/{package_path}")
    }
}

fn label_value_repo_runfiles_path(label: Value<'_>) -> bz_error::Result<String> {
    if let Some(label) = StarlarkConfiguredProvidersLabel::from_value(label) {
        return Ok(label_repo_runfiles_path(label.label().target().pkg()));
    }
    if let Some(label) = StarlarkProvidersLabel::from_value(label) {
        return Ok(label_repo_runfiles_path(label.label().target().pkg()));
    }
    if let Some(label) = StarlarkConfiguredTargetLabel::from_value(label) {
        return Ok(label_repo_runfiles_path(label.label().pkg()));
    }
    if let Some(label) = StarlarkTargetLabel::from_value(label) {
        return Ok(label_repo_runfiles_path(label.label().pkg()));
    }
    Err(BazelPythonError::ExpectedLabel(label.to_string_for_type_error()).into())
}

fn repo_mapping_source_repo_name_for_cell(cell: &str) -> String {
    if cell == "root"
        || bzlmod_canonical_repo_name_for_cell(cell).is_some_and(|repo| repo.is_empty())
    {
        String::new()
    } else {
        bazel_repo_name_for_cell(cell)
    }
}

fn repo_mapping_target_repo_directory_for_cell(cell: &str) -> String {
    if cell == "root"
        || bzlmod_canonical_repo_name_for_cell(cell).is_some_and(|repo| repo.is_empty())
    {
        "_main".to_owned()
    } else {
        bazel_repo_name_for_cell(cell)
    }
}

fn repo_mapping_cell_from_owner(owner: &BaseDeferredKey) -> Option<String> {
    owner
        .configured_label()
        .map(|label| label.pkg().cell_name().as_str().to_owned())
}

fn repo_mapping_cell_from_bazel_path(path: &str) -> String {
    let Some(path) = path.strip_prefix("external/") else {
        return "root".to_owned();
    };
    let repo = path.split('/').next().unwrap_or(path);
    if repo.contains('+') {
        bzlmod_cell_name(repo)
    } else {
        repo.to_owned()
    }
}

fn repo_mapping_cell_from_artifact<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> bz_error::Result<Option<String>> {
    let Some(artifact) = ValueAsInputArtifactLike::unpack_value(value)? else {
        return Ok(None);
    };
    if let Some(owner) = artifact.0.owner()? {
        return Ok(repo_mapping_cell_from_owner(&owner));
    }
    let path = artifact.0.with_bazel_path(&|path| heap.alloc_str(path))?;
    Ok(Some(repo_mapping_cell_from_bazel_path(path.as_str())))
}

fn repo_mapping_ctx_cell<'v>(ctx: Value<'v>, heap: Heap<'v>) -> bz_error::Result<Option<String>> {
    let label = ctx.get_attr_error("label", heap)?;
    if label.is_none() {
        return Ok(None);
    }
    if let Some(label) = StarlarkConfiguredProvidersLabel::from_value(label) {
        return Ok(Some(
            label.label().target().pkg().cell_name().as_str().to_owned(),
        ));
    }
    if let Some(label) = StarlarkProvidersLabel::from_value(label) {
        return Ok(Some(
            label.label().target().pkg().cell_name().as_str().to_owned(),
        ));
    }
    if let Some(label) = StarlarkConfiguredTargetLabel::from_value(label) {
        return Ok(Some(label.label().pkg().cell_name().as_str().to_owned()));
    }
    if let Some(label) = StarlarkTargetLabel::from_value(label) {
        return Ok(Some(label.label().pkg().cell_name().as_str().to_owned()));
    }
    Err(BazelPythonError::ExpectedLabel(label.to_string_for_type_error()).into())
}

fn repo_mapping_symlink_path<'v>(heap: Heap<'v>, symlink: Value<'v>) -> starlark::Result<String> {
    symlink
        .get_attr_error("path", heap)?
        .unpack_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "runfiles symlink path should be a string"
            )
            .into()
        })
}

fn repo_mapping_symlink_target<'v>(
    heap: Heap<'v>,
    symlink: Value<'v>,
) -> starlark::Result<Value<'v>> {
    symlink.get_attr_error("target_file", heap)
}

fn repo_mapping_manifest_content<'v>(
    ctx: Value<'v>,
    runfiles: &BazelRunfiles<'v>,
    heap: Heap<'v>,
) -> starlark::Result<String> {
    let mut source_cells = BTreeSet::new();
    let mut target_repos = BTreeSet::new();

    if let Some(cell) = repo_mapping_ctx_cell(ctx, heap)? {
        source_cells.insert(cell);
    }

    for file in bazel_depset_to_list(runfiles.files_value())? {
        if let Some(cell) = repo_mapping_cell_from_artifact(file, heap)? {
            target_repos.insert(repo_mapping_source_repo_name_for_cell(&cell));
            source_cells.insert(cell);
        }
    }

    let symlinks = bazel_depset_to_list(runfiles.symlinks_value())?;
    if !symlinks.is_empty() {
        target_repos.insert(String::new());
    }
    for symlink in symlinks {
        let target = repo_mapping_symlink_target(heap, symlink)?;
        if let Some(cell) = repo_mapping_cell_from_artifact(target, heap)? {
            target_repos.insert(repo_mapping_source_repo_name_for_cell(&cell));
            source_cells.insert(cell);
        }
    }

    for symlink in bazel_depset_to_list(runfiles.root_symlinks_value())? {
        if let Some(first_segment) = repo_mapping_symlink_path(heap, symlink)?.split('/').next()
            && !first_segment.is_empty()
        {
            target_repos.insert(first_segment.to_owned());
        }
        let target = repo_mapping_symlink_target(heap, symlink)?;
        if let Some(cell) = repo_mapping_cell_from_artifact(target, heap)? {
            target_repos.insert(repo_mapping_source_repo_name_for_cell(&cell));
            source_cells.insert(cell);
        }
    }

    let mut lines = BTreeSet::new();
    for source_cell in source_cells {
        let source = repo_mapping_source_repo_name_for_cell(&source_cell);
        let mut aliases = bzlmod_cell_aliases_for_cell(&source_cell);
        aliases.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (apparent_name, target_cell) in aliases {
            if apparent_name.is_empty() {
                continue;
            }
            let target_repo = repo_mapping_source_repo_name_for_cell(&target_cell);
            if !target_repos.contains(&target_repo) {
                continue;
            }
            let target = repo_mapping_target_repo_directory_for_cell(&target_cell);
            lines.insert(format!("{source},{apparent_name},{target}\n"));
        }
    }

    Ok(lines.into_iter().collect())
}

fn ctx_is_tool_configuration<'v>(ctx: Value<'v>, heap: Heap<'v>) -> starlark::Result<bool> {
    if let Some(is_exec) = bazel_ctx_is_exec_configuration(ctx, heap)? {
        return Ok(is_exec);
    }
    let label = ctx.get_attr_error("label", heap)?;
    if label.is_none() {
        return Ok(false);
    }
    let label = StarlarkConfiguredProvidersLabel::from_value(label).ok_or_else(|| {
        bz_error::Error::from(BazelPythonError::ExpectedLabel(
            label.to_string_for_type_error(),
        ))
    })?;
    Ok(label.label().target().cfg().is_marked_as_exec_platform())
}

#[starlark_module]
fn bazel_py_internal_methods(builder: &mut MethodsBuilder) {
    /// Adapter for Bazel's internal `cc_helper`, accessed by rules_python as
    /// `getattr(py_internal, "cc_helper", None)`.
    #[starlark(attribute)]
    fn cc_helper<'v>(this: &BazelPyInternal, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(BazelCcHelper))
    }

    fn is_singleton_depset<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        value: Value<'v>,
    ) -> starlark::Result<bool> {
        bazel_depset_is_singleton(value)
    }

    fn get_singleton_depset<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        value: Value<'v>,
    ) -> starlark::Result<NoneOr<Value<'v>>> {
        Ok(match bazel_depset_get_singleton(value)? {
            Some(value) => NoneOr::Other(value),
            None => NoneOr::None,
        })
    }

    fn regex_match<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        subject: &str,
        pattern: &str,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let normalized_pattern = pattern
            .strip_prefix("(?U)")
            .or_else(|| pattern.strip_prefix("(?u)"))
            .unwrap_or(pattern);
        let anchored = format!("^(?:{normalized_pattern})$");
        let regex = Regex::new(&anchored).map_err(|error| {
            bz_error::Error::from(BazelPythonError::InvalidRegex {
                pattern: pattern.to_owned(),
                error: error.to_string(),
            })
        })?;
        regex
            .is_match(subject)
            .map_err(|error| BazelPythonError::RegexMatch {
                pattern: pattern.to_owned(),
                error: error.to_string(),
            })
            .map_err(|error| bz_error::Error::from(error).into())
    }

    fn get_current_os_name(
        #[starlark(this)] _this: &BazelPyInternal,
    ) -> starlark::Result<&'static str> {
        Ok(match std::env::consts::OS {
            "macos" => "osx",
            "freebsd" => "freebsd",
            "openbsd" => "openbsd",
            "linux" => "linux",
            "windows" => "windows",
            _ => "unknown",
        })
    }

    fn get_label_repo_runfiles_path<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        label: Value<'v>,
    ) -> starlark::Result<String> {
        label_value_repo_runfiles_path(label).map_err(Into::into)
    }

    fn is_tool_configuration<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        ctx: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<bool> {
        ctx_is_tool_configuration(ctx, heap)
    }

    fn merge_runfiles_with_generated_inits_empty_files_supplier<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] runfiles: &BazelRunfiles<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<BazelRunfiles<'v>> {
        let _ = ctx;
        bazel_runfiles_with_generated_inits_empty_files_supplier(heap, runfiles)
    }

    fn make_runfiles_respect_legacy_external_runfiles<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(require = pos)] _ctx: Value<'v>,
        #[starlark(require = pos)] runfiles: &BazelRunfiles<'v>,
    ) -> starlark::Result<BazelRunfiles<'v>> {
        Ok(runfiles.clone())
    }

    fn declare_constant_metadata_file<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] root: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let _ = root;
        bazel_analysis_context_declare_file(ctx, name, heap)
    }

    fn create_repo_mapping_manifest<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] runfiles: &BazelRunfiles<'v>,
        #[starlark(require = named)] output: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let heap = eval.heap();
        let actions = ctx.get_attr_error("actions", heap)?;
        let write = actions.get_attr_error("write", heap)?;
        let content = heap
            .alloc_str(&repo_mapping_manifest_content(ctx, runfiles, heap)?)
            .to_value();
        eval.eval_function(write, &[output, content], &[])?;
        Ok(NoneType)
    }

    fn get_legacy_external_runfiles<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn is_bzlmod_enabled<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn runfiles_enabled<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(!cfg!(windows))
    }

    fn stamp_binaries<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(false)
    }
}

/// Returns whether a file name denotes a shared library, mirroring Bazel's
/// `cc_helper.is_valid_shared_library_artifact`: a direct shared-library
/// extension, or a versioned shared object such as `libfoo.so.1.2`.
fn is_valid_shared_library_basename(name: &str) -> bool {
    const SHARED_LIBRARY_EXTENSIONS: &[&str] = &[".so", ".dll", ".dylib", ".wasm", ".pyd"];
    if SHARED_LIBRARY_EXTENSIONS
        .iter()
        .any(|ext| name.ends_with(ext))
    {
        return true;
    }
    if let Some(pos) = name.find(".so.") {
        let version = &name[pos + ".so.".len()..];
        return !version.is_empty()
            && version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    }
    false
}

/// Bazel-internal `cc_helper` adapter exposed to rules_python via
/// `py_internal.cc_helper`. Only the subset of helpers used by the Python rules
/// is implemented.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCcHelper;

impl fmt::Display for BazelCcHelper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cc_helper")
    }
}

starlark::starlark_simple_value!(BazelCcHelper);

#[starlark_value(type = "cc_helper")]
impl<'v> StarlarkValue<'v> for BazelCcHelper {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cc_helper_methods)
    }
}

#[starlark_module]
fn bazel_cc_helper_methods(builder: &mut MethodsBuilder) {
    /// Returns whether the given artifact is a (possibly versioned) shared library.
    fn is_valid_shared_library_artifact<'v>(
        #[starlark(this)] _this: &BazelCcHelper,
        #[starlark(require = pos)] artifact: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<bool> {
        let Some(artifact) = ValueAsInputArtifactLike::unpack_value(artifact)? else {
            return Ok(false);
        };
        let basename = artifact
            .0
            .with_filename(&|filename| heap.alloc_str(filename.as_str()))?;
        Ok(is_valid_shared_library_basename(basename.as_str()))
    }
}

pub(crate) fn register_bazel_python_globals(builder: &mut GlobalsBuilder) {
    builder.set("py_internal", BazelPyInternal);
}
