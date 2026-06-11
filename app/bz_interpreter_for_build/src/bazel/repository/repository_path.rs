use super::*;

#[derive(Clone, Debug)]
pub(super) struct BazelRepositoryPathRemoteContext {
    pub(super) working_dir: String,
    pub(super) command_executor: BazelRepositoryCommandExecutor,
}

#[derive(Clone, Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
pub(crate) struct StarlarkRepositoryPath {
    pub(super) path: String,
    #[trace(unsafe_ignore)]
    pub(super) dep: Option<RepositoryPathLabelDep>,
    #[trace(unsafe_ignore)]
    #[allocative(skip)]
    pub(super) remote_context: Option<BazelRepositoryPathRemoteContext>,
}

starlark_simple_value!(StarlarkRepositoryPath);

impl Freeze for StarlarkRepositoryPath {
    type Frozen = StarlarkRepositoryPath;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(self)
    }
}

impl StarlarkRepositoryPath {
    pub(super) fn new(path: String) -> Self {
        Self {
            path,
            dep: None,
            remote_context: None,
        }
    }

    pub(super) fn new_with_dep(path: String, dep: Option<RepositoryPathLabelDep>) -> Self {
        Self {
            path,
            dep,
            remote_context: None,
        }
    }

    pub(super) fn new_with_remote(
        path: String,
        remote_context: Option<BazelRepositoryPathRemoteContext>,
        dep: Option<RepositoryPathLabelDep>,
    ) -> Self {
        Self {
            path,
            dep,
            remote_context,
        }
    }
}

impl Display for StarlarkRepositoryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.remote_context.is_some() {
            return self.path.fmt(f);
        }
        repository_path_for_read_abs(&self.path)
            .to_string_lossy()
            .fmt(f)
    }
}

#[starlark_value(type = "repository_path")]
impl<'v> StarlarkValue<'v> for StarlarkRepositoryPath {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(repository_path_methods)
    }
}

#[starlark_module]
fn repository_path_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn basename(this: &StarlarkRepositoryPath) -> starlark::Result<String> {
        Ok(Path::new(&this.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default())
    }

    #[starlark(attribute)]
    fn dirname(this: &StarlarkRepositoryPath) -> starlark::Result<StarlarkRepositoryPath> {
        let path = Path::new(&this.path)
            .parent()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let remote_context = this.remote_context.clone();
        let dep = if remote_context.is_some() {
            None
        } else {
            repository_ctx_external_input_tree_dep(Path::new(&path))
        };
        Ok(StarlarkRepositoryPath::new_with_remote(
            path,
            remote_context,
            dep,
        ))
    }

    fn get_child<'v>(
        this: &StarlarkRepositoryPath,
        args: &Arguments<'v, '_>,
        heap: Heap<'v>,
    ) -> starlark::Result<StarlarkRepositoryPath> {
        args.no_named_args()?;
        let mut children = Vec::new();
        for child in args.positions(heap)? {
            let Some(child) = child.unpack_str() else {
                return Err(bz_error::Error::from(
                    BazelRepositoryError::RepositoryPathGetChildNonString(
                        child.get_type().to_owned(),
                    ),
                )
                .into());
            };
            children.push(child);
        }
        Ok(StarlarkRepositoryPath::new_with_remote(
            repository_path_get_child(&this.path, children),
            this.remote_context.clone(),
            None,
        ))
    }

    #[starlark(attribute)]
    fn exists(this: &StarlarkRepositoryPath) -> starlark::Result<bool> {
        if let Some(remote_context) = &this.remote_context {
            return repository_remote_path_exists(remote_context, &this.path);
        }
        Ok(Path::new(&repository_path_for_read(&this.path)).exists())
    }

    #[starlark(attribute)]
    fn is_dir(this: &StarlarkRepositoryPath) -> starlark::Result<bool> {
        if let Some(remote_context) = &this.remote_context {
            return repository_remote_path_is_dir(remote_context, &this.path);
        }
        Ok(Path::new(&repository_path_for_read(&this.path)).is_dir())
    }

    #[starlark(attribute)]
    fn realpath(this: &StarlarkRepositoryPath) -> starlark::Result<StarlarkRepositoryPath> {
        if let Some(remote_context) = &this.remote_context {
            let path = repository_remote_path_realpath(remote_context, &this.path)?;
            return Ok(StarlarkRepositoryPath::new_with_remote(
                path,
                this.remote_context.clone(),
                None,
            ));
        }
        let read_path = repository_path_for_read(&this.path);
        let path = fs::canonicalize(&read_path).map_err(|error| {
            bz_error::Error::from(BazelRepositoryError::RepositoryPathRealpath {
                path: this.path.clone(),
                error: error.to_string(),
            })
        })?;
        Ok(StarlarkRepositoryPath::new_with_remote(
            path.to_string_lossy().into_owned(),
            this.remote_context.clone(),
            None,
        ))
    }

    fn readdir(
        this: &StarlarkRepositoryPath,
        #[starlark(require = named, default = "auto")] watch: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<Vec<StarlarkRepositoryPath>> {
        if repository_should_record_watch(watch)?
            && let Ok(build_context) = BuildContext::from_context(eval)
            && let Some(repository_context) = &build_context.bazel_repository_context
            && this.remote_context.is_none()
        {
            record_repository_dirents_input(
                &repository_context.recorded_inputs,
                &this.path,
                &repository_context.working_dir,
            )?;
        }
        if let Some(remote_context) = &this.remote_context {
            let mut paths = repository_remote_path_readdir(remote_context, &this.path)?
                .into_iter()
                .map(|path| {
                    StarlarkRepositoryPath::new_with_remote(path, this.remote_context.clone(), None)
                })
                .collect::<Vec<_>>();
            paths.sort_by(|left, right| left.path.cmp(&right.path));
            return Ok(paths);
        }
        let read_path = repository_path_for_read(&this.path);
        let entries = fs::read_dir(&read_path).map_err(|error| {
            bz_error::Error::from(BazelRepositoryError::RepositoryPathReaddir {
                path: this.path.clone(),
                error: error.to_string(),
            })
        })?;
        let mut paths = entries
            .map(|entry| {
                let entry = entry.map_err(|error| {
                    bz_error::Error::from(BazelRepositoryError::RepositoryPathReaddir {
                        path: this.path.clone(),
                        error: error.to_string(),
                    })
                })?;
                let path = Path::new(&this.path).join(entry.file_name());
                Ok(StarlarkRepositoryPath::new_with_remote(
                    path.to_string_lossy().into_owned(),
                    this.remote_context.clone(),
                    None,
                ))
            })
            .collect::<starlark::Result<Vec<_>>>()?;
        paths.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(paths)
    }
}

pub(super) fn repository_path_from_value_relative_to(
    value: Value<'_>,
    eval: &Evaluator<'_, '_, '_>,
    relative_root: Option<&str>,
) -> starlark::Result<String> {
    Ok(repository_path_and_dep_from_value_relative_to(value, eval, relative_root)?.0)
}

pub(super) fn repository_path_and_dep_from_value_relative_to(
    value: Value<'_>,
    eval: &Evaluator<'_, '_, '_>,
    relative_root: Option<&str>,
) -> starlark::Result<(String, Option<RepositoryPathLabelDep>)> {
    if let Some(path) = value.downcast_ref::<StarlarkRepositoryPath>() {
        return Ok((path.path.clone(), path.dep.clone()));
    }
    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
        let target = label.label().target();
        let cell_path = target
            .pkg()
            .to_cell_path()
            .join_normalized(target.name().as_str())?;
        let project_path = BuildContext::from_context(eval)?
            .cell_resolver()
            .resolve_path(cell_path.as_ref())?;
        return Ok((
            project_path.as_str().to_owned(),
            Some(RepositoryPathLabelDep::cell_path(
                cell_path.cell().as_str().to_owned(),
                cell_path.path().as_str().to_owned(),
            )),
        ));
    }
    if let Some(path) = value.unpack_str() {
        let path = if let Some(relative_root) = relative_root
            && !Path::new(path).is_absolute()
            && !path.starts_with("buck-out/")
        {
            repository_join_normalized(relative_root, path)
        } else {
            path.to_owned()
        };
        let dep = repository_ctx_external_input_dep(Path::new(&path));
        return Ok((path, dep));
    }
    Err(
        bz_error::Error::from(BazelRepositoryError::ModuleCtxPathUnsupportedValue(
            value.get_type().to_owned(),
        ))
        .into(),
    )
}

pub(super) fn repository_remote_shell(
    remote_context: &BazelRepositoryPathRemoteContext,
    script: &str,
    args: &[String],
    timeout: i32,
    quiet: bool,
) -> starlark::Result<RepositoryCommandOutput> {
    repository_remote_shell_in_context(remote_context, script, args, timeout, quiet)
}

fn repository_remote_shell_in_context(
    remote_context: &BazelRepositoryPathRemoteContext,
    script: &str,
    args: &[String],
    timeout: i32,
    quiet: bool,
) -> starlark::Result<RepositoryCommandOutput> {
    let BazelRepositoryCommandExecutor::Remote(command_executor) =
        remote_context.command_executor.clone()
    else {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "remote repository path operation requires a remote repository executor"
        )
        .into());
    };
    let working_dir_abs =
        repository_path_for_write(&remote_context.working_dir).map_err(starlark::Error::from)?;
    let mut command = Command::new("/bin/sh");
    command.env_clear();
    command.env("PATH", "/usr/bin:/bin:/usr/local/bin");
    command.args(["-c", script, "buck2-remote-repository-path"]);
    command.args(args);
    command.current_dir(working_dir_abs);
    command_executor
        .execute(command, &remote_context.working_dir, timeout, quiet)
        .map_err(|error| {
            bz_error::Error::from(BazelRepositoryError::RepositoryCtxExecuteFailed {
                program: "/bin/sh".to_owned(),
                error,
            })
            .into()
        })
}

fn repository_remote_path_status(
    remote_context: &BazelRepositoryPathRemoteContext,
    path: &str,
    script: &str,
) -> starlark::Result<bool> {
    let output = repository_remote_shell(remote_context, script, &[path.to_owned()], 60, true)?;
    if output.return_code != 0 {
        return Ok(false);
    }
    Ok(repository_ctx_latin1_output(&output.stdout).trim() == "true")
}

fn repository_remote_path_exists(
    remote_context: &BazelRepositoryPathRemoteContext,
    path: &str,
) -> starlark::Result<bool> {
    repository_remote_path_status(
        remote_context,
        path,
        r#"if [ -e "$1" ] || [ -L "$1" ]; then printf 'true\n'; else printf 'false\n'; fi"#,
    )
}

fn repository_remote_path_is_dir(
    remote_context: &BazelRepositoryPathRemoteContext,
    path: &str,
) -> starlark::Result<bool> {
    repository_remote_path_status(
        remote_context,
        path,
        r#"if [ -d "$1" ]; then printf 'true\n'; else printf 'false\n'; fi"#,
    )
}

fn repository_remote_path_realpath(
    remote_context: &BazelRepositoryPathRemoteContext,
    path: &str,
) -> starlark::Result<String> {
    let output = repository_remote_shell(
        remote_context,
        r#"resolved=$(readlink -f "$1" 2>/dev/null || realpath "$1" 2>/dev/null) || exit $?
printf '%s\n' "$resolved"
"#,
        &[path.to_owned()],
        60,
        true,
    )?;
    if output.return_code != 0 {
        return Err(
            bz_error::Error::from(BazelRepositoryError::RepositoryPathRealpath {
                path: path.to_owned(),
                error: repository_ctx_latin1_output(&output.stderr),
            })
            .into(),
        );
    }
    Ok(repository_ctx_latin1_output(&output.stdout)
        .trim_end_matches('\n')
        .to_owned())
}

fn repository_remote_path_readdir(
    remote_context: &BazelRepositoryPathRemoteContext,
    path: &str,
) -> starlark::Result<Vec<String>> {
    let output = repository_remote_shell(
        remote_context,
        r#"if [ ! -d "$1" ]; then
  printf 'not_directory\n'
  exit 0
fi
printf 'ok\n'
for entry in "$1"/* "$1"/.[!.]* "$1"/..?*; do
  if [ -e "$entry" ] || [ -L "$entry" ]; then
    printf '%s\n' "$entry"
  fi
done
"#,
        &[path.to_owned()],
        60,
        true,
    )?;
    if output.return_code != 0 {
        return Err(
            bz_error::Error::from(BazelRepositoryError::RepositoryPathReaddir {
                path: path.to_owned(),
                error: repository_ctx_latin1_output(&output.stderr),
            })
            .into(),
        );
    }
    let stdout = repository_ctx_latin1_output(&output.stdout);
    let mut lines = stdout.lines();
    match lines.next() {
        Some("ok") => Ok(lines.map(ToOwned::to_owned).collect()),
        Some("not_directory") => Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "can't readdir(), not a directory: {}",
            path,
        )
        .into()),
        Some(status) => Err(
            bz_error::Error::from(BazelRepositoryError::RepositoryPathReaddir {
                path: path.to_owned(),
                error: format!("remote readdir returned malformed status `{status}`"),
            })
            .into(),
        ),
        None => Err(
            bz_error::Error::from(BazelRepositoryError::RepositoryPathReaddir {
                path: path.to_owned(),
                error: "remote readdir returned no status".to_owned(),
            })
            .into(),
        ),
    }
}

pub(super) fn repository_join_normalized(root: &str, path: &str) -> String {
    let mut joined = PathBuf::from(root);
    for component in Path::new(path).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                joined.pop();
            }
            std::path::Component::Normal(part) => joined.push(part),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                joined.push(component.as_os_str());
            }
        }
    }
    joined.to_string_lossy().into_owned()
}

fn repository_path_get_child<'a>(
    path: &str,
    children: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut path = path.to_owned();
    for child in children {
        path = repository_join_normalized(&path, child);
    }
    path
}

pub(super) fn repository_path_for_read(path: &str) -> String {
    if Path::new(path).exists() {
        return path.to_owned();
    }

    let Some(suffix) = path.strip_prefix("buck-out/v2/external_cells/") else {
        return repository_project_relative_path_for_read(path).unwrap_or_else(|| path.to_owned());
    };

    for root in repository_read_roots() {
        let candidate = root.join(path);
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }

        if let Some(candidate) = repository_path_for_extracted_external_cell(&root, suffix) {
            return candidate;
        }

        let Ok(entries) = fs::read_dir(root.join("buck-out")) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join("external_cells").join(suffix);
            if candidate.exists() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }

    path.to_owned()
}

pub(super) fn repository_path_for_read_abs(path: &str) -> PathBuf {
    let path = repository_path_for_read(path);
    let path_buf = PathBuf::from(&path);
    if path_buf.is_absolute() {
        return path_buf;
    }
    repository_path_for_write(&path).unwrap_or(path_buf)
}

pub(super) fn repository_path_for_read_abs_relative_to(path: &str, working_dir: &str) -> PathBuf {
    if let Some(suffix) = repository_external_cell_suffix(path)
        && let Some(candidate) =
            repository_external_cell_existing_path_relative_to(suffix, working_dir)
    {
        return candidate;
    }
    repository_path_for_read_abs(path)
}

pub(super) fn repository_external_cell_suffix(path: &str) -> Option<&str> {
    let buck_out_relative = path
        .strip_prefix("buck-out/")
        .or_else(|| path.split_once("/buck-out/").map(|(_, suffix)| suffix))?;
    let (_, suffix) = buck_out_relative.split_once("/external_cells/")?;
    (!suffix.is_empty()).then_some(suffix)
}

pub(super) fn repository_external_cell_path_relative_to(
    suffix: &str,
    working_dir: &str,
) -> Option<PathBuf> {
    let candidate = if let Some((buck_out_root, _)) = working_dir.split_once("/external_cells/") {
        format!("{buck_out_root}/external_cells/{suffix}")
    } else if let Some((workspace_root, _)) = working_dir.split_once("/buck-out/v2/") {
        if workspace_root.is_empty() {
            format!("buck-out/v2/external_cells/{suffix}")
        } else {
            format!("{workspace_root}/buck-out/v2/external_cells/{suffix}")
        }
    } else {
        return None;
    };
    Some(repository_path_for_write(&candidate).unwrap_or_else(|_| PathBuf::from(candidate)))
}

pub(super) fn repository_external_cell_existing_path_relative_to(
    suffix: &str,
    working_dir: &str,
) -> Option<PathBuf> {
    let candidate = repository_external_cell_path_relative_to(suffix, working_dir)?;
    if candidate.exists() {
        return Some(candidate);
    }
    let candidate = repository_external_cell_repository_ctx_path_relative_to(suffix, working_dir)?;
    candidate.exists().then_some(candidate)
}

fn repository_external_cell_repository_ctx_path_relative_to(
    suffix: &str,
    working_dir: &str,
) -> Option<PathBuf> {
    let generated_suffix = suffix.strip_prefix("bzlmod_generated/")?;
    let (repo_name, repo_path) = generated_suffix
        .split_once('/')
        .unwrap_or((generated_suffix, ""));
    if repo_name.ends_with(".repository_ctx") {
        return None;
    }
    let source_suffix = if repo_path.is_empty() {
        format!("bzlmod_generated/{repo_name}.repository_ctx")
    } else {
        format!("bzlmod_generated/{repo_name}.repository_ctx/{repo_path}")
    };
    repository_external_cell_path_relative_to(&source_suffix, working_dir)
}

fn repository_project_relative_path_for_read(path: &str) -> Option<String> {
    for root in repository_read_roots() {
        let candidate = root.join(path);
        if candidate.exists() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn repository_path_for_extracted_external_cell(root: &Path, suffix: &str) -> Option<String> {
    let mut parts = suffix.splitn(3, '/');
    let cell_kind = parts.next()?;
    let cell_name = parts.next()?;
    let cell_path = parts.next()?;
    let candidate = root
        .join("buck-out/v2/external_cells")
        .join(cell_kind)
        .join(cell_name)
        .join("extract-tmp")
        .join(cell_path);
    if candidate.exists() {
        Some(candidate.to_string_lossy().into_owned())
    } else {
        None
    }
}

fn repository_read_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(pwd) = env::var_os("PWD") {
        push_repository_read_roots(&mut roots, PathBuf::from(pwd));
    }
    if let Ok(cwd) = env::current_dir() {
        push_repository_read_roots(&mut roots, cwd);
    }
    roots
}

fn push_repository_read_roots(roots: &mut Vec<PathBuf>, path: PathBuf) {
    for ancestor in path.ancestors() {
        if ancestor.join(".buckconfig").exists()
            || ancestor.join("MODULE.bazel").exists()
            || ancestor.join("WORKSPACE.bazel").exists()
            || ancestor.join("WORKSPACE").exists()
        {
            push_unique_repository_read_root(roots, ancestor.to_owned());
        }
    }
    push_unique_repository_read_root(roots, path);
}

fn push_unique_repository_read_root(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !roots.iter().any(|existing| existing == &root) {
        roots.push(root);
    }
}

pub(super) fn repository_path_for_write(path: &str) -> bz_error::Result<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    let root = match repository_read_roots().into_iter().next() {
        Some(root) => root,
        None => env::current_dir().map_err(|e| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "could not resolve repository write root: {}",
                e
            )
        })?,
    };
    Ok(root.join(path))
}
