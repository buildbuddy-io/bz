use super::*;

pub(super) struct RepositoryCommandOutput {
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) return_code: i32,
}

static REPOSITORY_CTX_REMOTE_EXECUTE_COUNTER: AtomicU64 = AtomicU64::new(0);
static REPOSITORY_CTX_REMOTE_EXECUTE_RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> =
    OnceLock::new();
const REPOSITORY_CTX_REMOTE_WHICH_TIMEOUT: u64 = 60;
const REPOSITORY_CTX_REMOTE_WHICH_SCRIPT: &str = r#"
set -f
program="$1"
case "$program" in
  ""|*/*|*\\*)
    printf '2\n'
    exit 0
    ;;
esac
old_ifs="$IFS"
IFS=:
for dir in $PATH; do
  IFS="$old_ifs"
  if [ -n "$dir" ]; then
    case "$dir" in
      /*)
        candidate="$dir/$program"
        if [ -f "$candidate" ] && [ -x "$candidate" ]; then
          printf '0\n%s\n' "$candidate"
          exit 0
        fi
        ;;
    esac
  fi
  IFS=:
done
printf '1\n'
exit 0
"#;

fn parse_repository_ctx_remote_which_output(stdout: &[u8]) -> Result<Option<String>, String> {
    let stdout = repository_ctx_latin1_output(stdout);
    let mut lines = stdout.lines();
    match lines.next() {
        Some("0") => {
            let Some(path) = lines.next() else {
                return Err("remote which reported success without a path".to_owned());
            };
            if path.is_empty() {
                return Err("remote which reported an empty path".to_owned());
            }
            Ok(Some(path.to_owned()))
        }
        Some("1") => Ok(None),
        Some("2") => Err("remote which rejected the program name".to_owned()),
        Some(status) => Err(format!("remote which returned malformed status `{status}`")),
        None => Err("remote which returned no status".to_owned()),
    }
}

fn repository_ctx_remote_execute_runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    REPOSITORY_CTX_REMOTE_EXECUTE_RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_name("buck2-remote-repository-ctx")
                .build()
                .map_err(|error| {
                    format!("could not create remote repository_ctx.execute runtime: {error}")
                })
        })
        .as_ref()
        .map_err(|error| error.clone())
}

fn repository_ctx_remote_execute_block_in_place<T>(
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

fn repository_ctx_remote_execute_on_thread<T>(
    timeout: Duration,
    timeout_result: impl FnOnce() -> Result<T, String>,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    repository_ctx_remote_execute_block_in_place(|| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let handle = std::thread::Builder::new()
            .name("buck2-remote-repository-ctx-call".to_owned())
            .spawn(move || {
                let result = f();
                let _ = sender.send(result);
            })
            .map_err(|error| {
                format!("remote repository_ctx.execute failed to spawn worker thread: {error}")
            })?;
        match receiver.recv_timeout(timeout) {
            Ok(result) => {
                handle.join().map_err(|_| {
                    "remote repository_ctx.execute worker thread panicked".to_owned()
                })?;
                result
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                drop(handle);
                timeout_result()
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                handle.join().map_err(|_| {
                    "remote repository_ctx.execute worker thread panicked".to_owned()
                })?;
                Err(
                    "remote repository_ctx.execute worker thread exited without a result"
                        .to_owned(),
                )
            }
        }
    })
}

#[derive(Clone)]
pub(crate) enum BazelRepositoryCommandExecutor {
    Local,
    Remote(Arc<BazelRemoteRepositoryCommandExecutor>),
}

#[derive(Clone, Debug)]
pub(crate) struct BazelRepositoryRemoteDownloaderConfig {
    pub(super) endpoint: String,
    pub(super) api_key: Option<String>,
}

pub(crate) fn bazel_repository_remote_downloader_config(
    ctx: &DiceComputations<'_>,
) -> Option<BazelRepositoryRemoteDownloaderConfig> {
    let startup_config = ctx
        .per_transaction_data()
        .data
        .get::<RemoteExecutionStartupConfig>()
        .ok()?;
    let endpoint = startup_config
        .remote_downloader
        .as_ref()
        .filter(|endpoint| !endpoint.trim().is_empty())?;
    Some(BazelRepositoryRemoteDownloaderConfig {
        endpoint: endpoint.clone(),
        api_key: startup_config.buildbuddy_api_key.clone(),
    })
}

impl fmt::Debug for BazelRepositoryCommandExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("Local"),
            Self::Remote(_) => f.write_str("Remote"),
        }
    }
}

impl BazelRepositoryCommandExecutor {
    pub(super) fn which(
        &self,
        program: &str,
        path: &str,
        repo_env: &BTreeMap<String, String>,
        repository_working_dir: &str,
    ) -> Result<Option<StarlarkRepositoryPath>, String> {
        match self {
            Self::Local => {
                if !repository_ctx_should_search_local_path(repo_env) {
                    return Ok(None);
                }
                Ok(repository_ctx_which_local_path(path, program).map(StarlarkRepositoryPath::new))
            }
            Self::Remote(remote) => remote.which(program, path).map(|path| {
                path.map(|path| {
                    StarlarkRepositoryPath::new_with_remote(
                        path,
                        Some(BazelRepositoryPathRemoteContext {
                            working_dir: repository_working_dir.to_owned(),
                            command_executor: self.clone(),
                        }),
                        None,
                    )
                })
            }),
        }
    }

    pub(super) fn execute(
        &self,
        command: Command,
        repository_working_dir: &str,
        timeout: i32,
        quiet: bool,
    ) -> Result<RepositoryCommandOutput, String> {
        match self {
            Self::Local => repository_ctx_execute_output_local(command, timeout, quiet),
            Self::Remote(remote) => remote.execute(command, repository_working_dir, timeout, quiet),
        }
    }
}

pub(crate) struct BazelRemoteRepositoryCommandExecutor {
    command_executor: CommandExecutor,
    artifact_fs: ArtifactFs,
    project_root: ProjectRoot,
    blocking_executor: Arc<dyn BlockingExecutor>,
    materializer: Arc<dyn Materializer>,
    digest_config: DigestConfig,
}

impl BazelRemoteRepositoryCommandExecutor {
    pub(crate) fn new(
        command_executor: CommandExecutor,
        artifact_fs: ArtifactFs,
        project_root: ProjectRoot,
        blocking_executor: Arc<dyn BlockingExecutor>,
        materializer: Arc<dyn Materializer>,
        digest_config: DigestConfig,
    ) -> Self {
        Self {
            command_executor,
            artifact_fs,
            project_root,
            blocking_executor,
            materializer,
            digest_config,
        }
    }

    fn which(self: &Arc<Self>, program: &str, path: &str) -> Result<Option<String>, String> {
        let executor = self.dupe();
        let program = program.to_owned();
        let path = path.to_owned();
        repository_ctx_remote_execute_on_thread(
            Duration::from_secs(REPOSITORY_CTX_REMOTE_WHICH_TIMEOUT),
            || {
                Err(format!(
                    "remote repository_ctx.which timed out after {REPOSITORY_CTX_REMOTE_WHICH_TIMEOUT} seconds"
                ))
            },
            move || {
                repository_ctx_remote_execute_runtime()?
                    .block_on(executor.which_async(&program, &path))
            },
        )
    }

    async fn which_async(&self, program: &str, path: &str) -> Result<Option<String>, String> {
        let paths = CommandExecutionPaths::new(
            Vec::new(),
            BuckIndexSet::new(),
            &self.artifact_fs,
            self.digest_config,
            None,
        )
        .map_err(|error| error.to_string())?;

        let env =
            sorted_vector_map::SortedVectorMap::from_iter([("PATH".to_owned(), path.to_owned())]);
        let request = bz_execute::execute::request::CommandExecutionRequest::new(
            vec!["/bin/sh".to_owned()],
            vec![
                "-c".to_owned(),
                REPOSITORY_CTX_REMOTE_WHICH_SCRIPT.to_owned(),
                "buck2-remote-repository-ctx-which".to_owned(),
                program.to_owned(),
            ],
            paths,
            env,
        )
        .with_timeout(Duration::from_secs(REPOSITORY_CTX_REMOTE_WHICH_TIMEOUT))
        .with_executor_preference(ExecutorPreference::RemoteRequired)
        .with_prefetch_lossy_stderr(true);

        let prepared_action = self
            .command_executor
            .prepare_action(&request, self.digest_config, true)
            .map_err(|error| error.to_string())?;
        let target = RepositoryCommandExecutionTarget {
            repository: "<which>".to_owned(),
            program: program.to_owned(),
        };
        let prepared_command = PreparedCommand {
            request: &request,
            target: &target,
            prepared_action: &prepared_action,
            digest_config: self.digest_config,
        };
        let manager = CommandExecutionManager::new(
            Box::new(MutexClaimManager::new()),
            bz_events::dispatch::get_dispatcher(),
            NoopLivelinessObserver::create(),
            Default::default(),
        );
        let result = self
            .command_executor
            .exec_cmd(
                manager,
                &prepared_command,
                dice_futures::cancellation::CancellationContext::never_cancelled(),
            )
            .await;

        let status_string = result.report.status.to_string();
        let streams = result
            .report
            .std_streams
            .into_bytes()
            .await
            .map_err(|error| error.to_string())?;
        match result.report.status {
            CommandExecutionStatus::Success { .. } => {
                parse_repository_ctx_remote_which_output(&streams.stdout)
            }
            _ => Err(format!(
                "{status_string}\nstdout:\n{}\nstderr:\n{}",
                repository_ctx_latin1_output(&streams.stdout),
                repository_ctx_latin1_output(&streams.stderr),
            )),
        }
    }

    pub(super) fn execute(
        self: &Arc<Self>,
        command: Command,
        repository_working_dir: &str,
        timeout: i32,
        quiet: bool,
    ) -> Result<RepositoryCommandOutput, String> {
        if timeout <= 0 {
            return Err(format!("timeout must be positive, got {timeout}"));
        }
        let executor = self.dupe();
        let repository_working_dir = repository_working_dir.to_owned();
        repository_ctx_remote_execute_on_thread(
            Duration::from_secs(timeout as u64),
            || {
                Ok(RepositoryCommandOutput {
                    stdout: Vec::new(),
                    stderr: format!("Command timed out after {timeout} seconds").into_bytes(),
                    return_code: 256,
                })
            },
            move || {
                repository_ctx_remote_execute_runtime()?.block_on(executor.execute_async(
                    command,
                    &repository_working_dir,
                    timeout,
                    quiet,
                ))
            },
        )
    }

    async fn execute_async(
        &self,
        command: Command,
        repository_working_dir: &str,
        timeout: i32,
        quiet: bool,
    ) -> Result<RepositoryCommandOutput, String> {
        let (repository_working_dir_rel, repository_working_dir_abs) =
            self.repository_working_dir_paths(repository_working_dir)?;
        let repository_working_dir_abs = repository_working_dir_abs.as_path();

        let program = command.get_program().to_string_lossy().into_owned();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let current_dir = command
            .get_current_dir()
            .ok_or_else(|| {
                "remote repository_ctx.execute command has no working directory".to_owned()
            })?
            .to_path_buf();
        let mut inputs = Vec::new();
        self.add_disk_input(&mut inputs, repository_working_dir_abs)
            .await?;
        self.add_disk_symlink_target_inputs(&mut inputs, repository_working_dir_abs)
            .await?;
        for value in std::iter::once(program.as_str()).chain(args.iter().map(String::as_str)) {
            self.add_disk_inputs_for_command_value(&mut inputs, value, repository_working_dir_abs)
                .await?;
        }
        for (_key, value) in command.get_envs() {
            if let Some(value) = value {
                self.add_disk_inputs_for_command_value(
                    &mut inputs,
                    &value.to_string_lossy(),
                    repository_working_dir_abs,
                )
                .await?;
            }
        }

        let counter = REPOSITORY_CTX_REMOTE_EXECUTE_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let output_base = ForwardRelativePathBuf::unchecked_new(
            "buck-out/v2/tmp/repository_ctx_execute".to_owned(),
        );
        let output_file = ForwardRelativePathBuf::unchecked_new(format!("{counter}/repo.tar"));
        let exit_code_file = ForwardRelativePathBuf::unchecked_new(format!("{counter}/exit_code"));
        let output_project_path =
            ProjectRelativePathBuf::from(output_base.clone().join(&output_file));
        let exit_code_project_path =
            ProjectRelativePathBuf::from(output_base.clone().join(&exit_code_file));
        let output_tar_spec = CommandExecutionOutput::TestPath {
            path: BuckOutTestPath::new(output_base.clone(), output_file),
            create: OutputCreationBehavior::Parent,
        };
        let exit_code_spec = CommandExecutionOutput::TestPath {
            path: BuckOutTestPath::new(output_base, exit_code_file),
            create: OutputCreationBehavior::Parent,
        };
        let outputs: BuckIndexSet<_> = [output_tar_spec.clone(), exit_code_spec.clone()]
            .into_iter()
            .collect();

        let paths = CommandExecutionPaths::new(
            inputs,
            outputs,
            &self.artifact_fs,
            self.digest_config,
            None,
        )
        .map_err(|error| error.to_string())?;

        let remote_repo_root = "__bz_repository_ctx_work";
        let remote_input_root = repository_working_dir_rel.as_str().to_owned();
        let remote_working_dir =
            self.remote_path_for_local_path(&current_dir, repository_working_dir_abs)?;
        let env =
            self.remote_environment(&command, repository_working_dir_abs, &remote_working_dir)?;
        let remote_program =
            self.remote_arg(&program, repository_working_dir_abs, &remote_working_dir);
        let remote_args = args
            .iter()
            .map(|arg| self.remote_arg(arg, repository_working_dir_abs, &remote_working_dir))
            .collect::<Vec<_>>();
        let local_project_root = self
            .project_root
            .root()
            .as_path()
            .to_string_lossy()
            .into_owned();
        let script = r#"
set +e
input_root="$1"
	work_root="$2"
	remote_working_dir="$3"
	output_tar="$4"
	exit_code_file="$5"
	local_project_root="$6"
	shift 6
	exec_root="$PWD"
	rewrite_project_symlinks() {
	  root="$1"
	  from="$2"
	  to="$3"
	  if [ ! -d "$root" ]; then
	    return 0
	  fi
	  while IFS= read -r -d '' link; do
	    target="$(readlink "$link")" || continue
	    case "$target" in
	      "$from")
	        rm "$link" && ln -s "$to" "$link"
	        ;;
	      "$from"/*)
	        suffix="${target#"$from"/}"
	        rm "$link" && ln -s "$to/$suffix" "$link"
	        ;;
	    esac
	  done < <(find "$root" -type l -print0)
	}
	rm -rf "$work_root"
	mkdir -p "$work_root"
	if [ -d "$input_root" ]; then
	  cp -a "$input_root"/. "$work_root"/
	fi
	rewrite_project_symlinks "$work_root" "$local_project_root" "$exec_root"
	mkdir -p "$remote_working_dir"
	cd "$remote_working_dir"
	rewritten_args=()
	for arg in "$@"; do
	  rewritten_args+=("${arg//__BUCK2_REMOTE_EXEC_ROOT__/$exec_root}")
	done
	"${rewritten_args[@]}"
	rc=$?
	cd "$exec_root"
	mkdir -p "$(dirname "$output_tar")"
	mkdir -p "$(dirname "$exit_code_file")"
	printf '%s\n' "$rc" > "$exit_code_file"
	exit_code_rc=$?
	if [ "$exit_code_rc" -ne 0 ]; then
	  exit "$exit_code_rc"
	fi
	rewrite_project_symlinks "$work_root" "$exec_root" "$local_project_root"
	tar -cf "$output_tar" -C "$work_root" .
	tar_rc=$?
	if [ "$tar_rc" -ne 0 ]; then
	  exit "$tar_rc"
	fi
	exit 0
	"#;

        let mut request_args = vec![
            "-c".to_owned(),
            script.to_owned(),
            "buck2-remote-repository-ctx".to_owned(),
            remote_input_root,
            remote_repo_root.to_owned(),
            remote_working_dir,
            output_project_path.as_str().to_owned(),
            exit_code_project_path.as_str().to_owned(),
            local_project_root,
            remote_program,
        ];
        request_args.extend(remote_args);

        let request = bz_execute::execute::request::CommandExecutionRequest::new(
            vec!["/bin/bash".to_owned()],
            request_args,
            paths,
            env,
        )
        .with_timeout(Duration::from_secs(timeout as u64))
        .with_executor_preference(ExecutorPreference::RemoteRequired)
        .with_prefetch_lossy_stderr(true);

        let prepared_action = self
            .command_executor
            .prepare_action(&request, self.digest_config, true)
            .map_err(|error| error.to_string())?;
        let target = RepositoryCommandExecutionTarget {
            repository: repository_working_dir_rel.as_str().to_owned(),
            program: program.clone(),
        };
        let prepared_command = PreparedCommand {
            request: &request,
            target: &target,
            prepared_action: &prepared_action,
            digest_config: self.digest_config,
        };
        let manager = CommandExecutionManager::new(
            Box::new(MutexClaimManager::new()),
            bz_events::dispatch::get_dispatcher(),
            NoopLivelinessObserver::create(),
            Default::default(),
        );
        let result = self
            .command_executor
            .exec_cmd(
                manager,
                &prepared_command,
                dice_futures::cancellation::CancellationContext::never_cancelled(),
            )
            .await;

        let status_string = result.report.status.to_string();
        let streams = result
            .report
            .std_streams
            .into_bytes()
            .await
            .map_err(|error| error.to_string())?;
        if !quiet {
            std::io::stderr()
                .write_all(&streams.stdout)
                .map_err(|error| error.to_string())?;
            std::io::stderr()
                .write_all(&streams.stderr)
                .map_err(|error| error.to_string())?;
        }

        match result.report.status {
            CommandExecutionStatus::Success { .. } => {
                let missing_outputs = [
                    (&output_project_path, &output_tar_spec),
                    (&exit_code_project_path, &exit_code_spec),
                ]
                .into_iter()
                .filter_map(|(path, output)| {
                    (!result.outputs.contains_key(output)).then_some(path.as_str())
                })
                .join(", ");
                if !missing_outputs.is_empty() {
                    let produced_outputs = result
                        .outputs
                        .keys()
                        .map(|output| format!("{output:?}"))
                        .join(", ");
                    return Err(format!(
                        "{status_string} did not produce expected remote repository outputs: {missing_outputs}\nproduced outputs: {produced_outputs}\nstdout:\n{}\nstderr:\n{}",
                        repository_ctx_latin1_output(&streams.stdout),
                        repository_ctx_latin1_output(&streams.stderr),
                    ));
                }
                self.materializer
                    .ensure_materialized(vec![
                        output_project_path.clone(),
                        exit_code_project_path.clone(),
                    ])
                    .await
                    .map_err(|error| {
                        format!(
                            "materializing remote repository outputs `{}` and `{}`: {error:#}",
                            output_project_path, exit_code_project_path
                        )
                    })?;
                let return_code = self.read_remote_repository_exit_code(&exit_code_project_path)?;
                self.unpack_remote_repository_tree(
                    &output_project_path,
                    repository_working_dir_abs,
                )
                .map_err(|error| error.to_string())?;
                Ok(RepositoryCommandOutput {
                    stdout: streams.stdout,
                    stderr: streams.stderr,
                    return_code,
                })
            }
            _ => Err(format!(
                "{status_string}\nstdout:\n{}\nstderr:\n{}",
                repository_ctx_latin1_output(&streams.stdout),
                repository_ctx_latin1_output(&streams.stderr),
            )),
        }
    }

    fn remote_environment(
        &self,
        command: &Command,
        repository_working_dir: &Path,
        remote_working_dir: &str,
    ) -> Result<sorted_vector_map::SortedVectorMap<String, String>, String> {
        let mut env = sorted_vector_map::SortedVectorMap::new();
        for (key, value) in command.get_envs() {
            let key = key
                .to_str()
                .ok_or_else(|| "repository_ctx.execute environment key is not UTF-8".to_owned())?
                .to_owned();
            if let Some(value) = value {
                let value = value.to_string_lossy();
                if repository_ctx_remote_temp_env_key(&key)
                    && Path::new(value.as_ref()).is_absolute()
                    && !Path::new(value.as_ref()).starts_with(self.project_root.root().as_path())
                {
                    env.insert(key, "/tmp".to_owned());
                    continue;
                }
                env.insert(
                    key,
                    self.remote_arg(&value, repository_working_dir, remote_working_dir),
                );
            }
        }
        Ok(env)
    }

    async fn add_disk_input(
        &self,
        inputs: &mut Vec<CommandExecutionInput>,
        path: &Path,
    ) -> Result<(), String> {
        let Some(mut project_relative_path) = self.project_relative_from_abs_path(path) else {
            return Ok(());
        };
        if project_relative_path.as_str().is_empty() {
            return Ok(());
        }
        if let Some(external_repo_root) =
            repository_ctx_external_repo_root_project_path(&project_relative_path)
        {
            project_relative_path = external_repo_root;
        }
        if inputs.iter().any(|input| match input {
            CommandExecutionInput::IncrementalRemoteOutput(existing, _) => {
                existing == &project_relative_path
            }
            _ => false,
        }) {
            return Ok(());
        }
        let abs_path = self.project_root.resolve(&project_relative_path);
        let entry_abs_path = repository_ctx_remote_input_entry_path(abs_path)?;
        let (entry, _hashing) = build_entry_from_disk(
            entry_abs_path,
            FileDigestConfig::build(self.digest_config.cas_digest_config()),
            self.blocking_executor.as_ref(),
            self.project_root.root(),
        )
        .await
        .map_err(|error| error.to_string())?;
        let entry = match entry {
            Some(entry) => self.share_entry(entry),
            None => return Ok(()),
        };
        inputs.push(CommandExecutionInput::IncrementalRemoteOutput(
            project_relative_path,
            entry,
        ));
        Ok(())
    }

    fn share_entry(
        &self,
        entry: ActionDirectoryEntry<ActionDirectoryBuilder>,
    ) -> ActionDirectoryEntry<ActionSharedDirectory> {
        entry.map_dir(|dir| {
            dir.fingerprint(self.digest_config.as_directory_serializer())
                .shared(&*INTERNER)
        })
    }

    async fn add_disk_inputs_for_command_value(
        &self,
        inputs: &mut Vec<CommandExecutionInput>,
        value: &str,
        repository_working_dir: &Path,
    ) -> Result<(), String> {
        for path in self.project_paths_from_command_value(value) {
            if !path.starts_with(repository_working_dir) {
                self.add_disk_input(inputs, &path).await?;
            }
        }
        Ok(())
    }

    async fn add_disk_symlink_target_inputs(
        &self,
        inputs: &mut Vec<CommandExecutionInput>,
        repository_working_dir: &Path,
    ) -> Result<(), String> {
        let mut dirs = vec![repository_working_dir.to_path_buf()];
        let mut seen_dirs = BTreeSet::new();
        let mut seen_targets = BTreeSet::new();

        while let Some(dir) = dirs.pop() {
            if !seen_dirs.insert(dir.clone()) {
                continue;
            }
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "reading remote repository input directory `{}`: {}",
                        dir.display(),
                        error
                    ));
                }
            };
            for entry in entries {
                let entry = entry.map_err(|error| error.to_string())?;
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path).map_err(|error| {
                    format!(
                        "reading remote repository input metadata `{}`: {}",
                        path.display(),
                        error
                    )
                })?;
                if metadata.file_type().is_symlink() {
                    let target = fs::read_link(&path).map_err(|error| {
                        format!(
                            "reading remote repository input symlink `{}`: {}",
                            path.display(),
                            error
                        )
                    })?;
                    let target = if target.is_absolute() {
                        target
                    } else {
                        path.parent().unwrap_or(Path::new("")).join(target)
                    };
                    if target.starts_with(self.project_root.root().as_path())
                        && !target.starts_with(repository_working_dir)
                        && seen_targets.insert(target.clone())
                    {
                        self.add_disk_input(inputs, &target).await?;
                    }
                } else if metadata.is_dir() {
                    dirs.push(path);
                }
            }
        }

        Ok(())
    }

    fn project_paths_from_command_value(&self, value: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(path) = self.project_path_from_plain_command_value(value) {
            paths.push(path);
        }
        if let Some((key, value)) = value.split_once('=')
            && !key.contains('/')
            && !key.contains('\\')
            && let Some(path) = self.project_path_from_plain_command_value(value)
        {
            paths.push(path);
        }
        paths.extend(repository_ctx_embedded_project_paths(
            value,
            self.project_root.root().as_path(),
        ));
        paths
    }

    fn project_path_from_plain_command_value(&self, value: &str) -> Option<PathBuf> {
        let path = Path::new(value);
        if path.is_absolute() {
            if path.starts_with(self.project_root.root().as_path()) {
                return Some(path.to_path_buf());
            }
            return None;
        }
        if value.starts_with("buck-out/") {
            return Some(self.project_root.root().as_path().join(path));
        }
        None
    }

    fn remote_path_for_local_path(
        &self,
        path: &Path,
        repository_working_dir: &Path,
    ) -> Result<String, String> {
        if path.starts_with(repository_working_dir) {
            let suffix = path
                .strip_prefix(repository_working_dir)
                .map_err(|error| error.to_string())?;
            return Ok(remote_repo_path(suffix));
        }
        self.project_relative_from_abs_path(path)
            .map(|path| path.as_str().to_owned())
            .ok_or_else(|| {
                format!(
                    "remote repository_ctx.execute cannot use working directory outside project root: {}",
                    path.display()
                )
            })
    }

    fn remote_arg(
        &self,
        value: &str,
        repository_working_dir: &Path,
        _remote_working_dir: &str,
    ) -> String {
        if let Some((key, env_value)) = value.split_once('=')
            && !key.contains('/')
            && !key.contains('\\')
        {
            if repository_ctx_remote_temp_env_key(key)
                && Path::new(env_value).is_absolute()
                && !Path::new(env_value).starts_with(self.project_root.root().as_path())
            {
                return format!("{key}=/tmp");
            }
            if let Some(remote_path) = self.remote_command_path(env_value, repository_working_dir) {
                return format!("{key}={remote_path}");
            }
        }

        self.remote_command_path(value, repository_working_dir)
            .or_else(|| {
                repository_ctx_rewrite_embedded_project_paths(
                    value,
                    self.project_root.root().as_path(),
                    repository_working_dir,
                )
            })
            .unwrap_or_else(|| value.to_owned())
    }

    fn remote_command_path(&self, value: &str, repository_working_dir: &Path) -> Option<String> {
        let path = Path::new(value);
        if path.is_absolute() {
            if path.starts_with(repository_working_dir)
                && let Ok(suffix) = path.strip_prefix(repository_working_dir)
            {
                return Some(remote_path_in_exec_root(&remote_repo_path(suffix)));
            }
            if let Some(project_relative) = self.project_relative_from_abs_path(path) {
                return Some(remote_path_in_exec_root(project_relative.as_str()));
            }
        }
        if value.starts_with("buck-out/") {
            return Some(remote_path_in_exec_root(value));
        }
        None
    }

    fn project_relative_from_abs_path(&self, path: &Path) -> Option<ProjectRelativePathBuf> {
        let suffix = path.strip_prefix(self.project_root.root().as_path()).ok()?;
        let suffix = suffix.to_string_lossy();
        Some(ProjectRelativePathBuf::unchecked_new(suffix.into_owned()))
    }

    fn repository_working_dir_paths(
        &self,
        repository_working_dir: &str,
    ) -> Result<(ProjectRelativePathBuf, PathBuf), String> {
        let path = Path::new(repository_working_dir);
        if path.is_absolute() {
            let rel = self.project_relative_from_abs_path(path).ok_or_else(|| {
                format!(
                    "remote repository_ctx.execute requires repository working dir `{}` to be under project root `{}`",
                    path.display(),
                    self.project_root.root()
                )
            })?;
            return Ok((rel, path.to_path_buf()));
        }

        let rel = ProjectRelativePath::new(repository_working_dir)
            .map_err(|error| {
                format!("invalid repository working dir `{repository_working_dir}`: {error}")
            })?
            .to_buf();
        let abs = self.project_root.resolve(&rel).as_path().to_path_buf();
        Ok((rel, abs))
    }

    fn unpack_remote_repository_tree(
        &self,
        output_project_path: &ProjectRelativePath,
        repository_working_dir: &Path,
    ) -> bz_error::Result<()> {
        let tar_path = self.project_root.resolve(output_project_path);
        fs::remove_dir_all(repository_working_dir).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })?;
        fs::create_dir_all(repository_working_dir)?;
        let file = File::open(&tar_path)
            .with_buck_error_context(|| format!("opening remote repository result `{tar_path}`"))?;
        let mut archive = Archive::new(file);
        archive
            .unpack(repository_working_dir)
            .with_buck_error_context(|| {
                format!(
                    "extracting remote repository result `{}` into `{}`",
                    tar_path,
                    repository_working_dir.display()
                )
            })?;
        Ok(())
    }

    fn read_remote_repository_exit_code(
        &self,
        exit_code_project_path: &ProjectRelativePath,
    ) -> Result<i32, String> {
        let exit_code_path = self.project_root.resolve(exit_code_project_path);
        let exit_code = fs::read_to_string(&exit_code_path).map_err(|error| {
            format!("reading remote repository exit code `{exit_code_path}`: {error}")
        })?;
        exit_code.trim().parse::<i32>().map_err(|error| {
            format!("parsing remote repository exit code `{exit_code_path}`: {error}")
        })
    }
}

fn remote_repo_path(suffix: &Path) -> String {
    if suffix.as_os_str().is_empty() {
        "__bz_repository_ctx_work".to_owned()
    } else {
        format!("__bz_repository_ctx_work/{}", suffix.to_string_lossy())
    }
}

const REMOTE_EXEC_ROOT_MARKER: &str = "__BUCK2_REMOTE_EXEC_ROOT__";

fn remote_path_in_exec_root(path: &str) -> String {
    if path.is_empty() {
        REMOTE_EXEC_ROOT_MARKER.to_owned()
    } else {
        format!("{REMOTE_EXEC_ROOT_MARKER}/{path}")
    }
}

pub(super) fn repository_ctx_embedded_project_paths(
    value: &str,
    project_root: &Path,
) -> Vec<PathBuf> {
    let project_root = project_root.to_string_lossy();
    if project_root.is_empty() {
        return Vec::new();
    }
    value
        .match_indices(project_root.as_ref())
        .map(|(start, _)| {
            let end = repository_ctx_embedded_path_end(value, start);
            PathBuf::from(&value[start..end])
        })
        .collect()
}

pub(super) fn repository_ctx_rewrite_embedded_project_paths(
    value: &str,
    project_root: &Path,
    repository_working_dir: &Path,
) -> Option<String> {
    let mut rewritten = value.to_owned();
    let repository_working_dir = repository_working_dir.to_string_lossy();
    if !repository_working_dir.is_empty() && rewritten.contains(repository_working_dir.as_ref()) {
        rewritten = rewritten.replace(
            repository_working_dir.as_ref(),
            &remote_path_in_exec_root(&remote_repo_path(Path::new(""))),
        );
    }
    let project_root = project_root.to_string_lossy();
    if !project_root.is_empty() && rewritten.contains(project_root.as_ref()) {
        rewritten = rewritten.replace(project_root.as_ref(), REMOTE_EXEC_ROOT_MARKER);
    }
    (rewritten != value).then_some(rewritten)
}

fn repository_ctx_embedded_path_end(value: &str, start: usize) -> usize {
    value[start..]
        .char_indices()
        .find_map(|(offset, ch)| {
            (offset != 0 && repository_ctx_embedded_path_delimiter(ch)).then_some(start + offset)
        })
        .unwrap_or(value.len())
}

fn repository_ctx_embedded_path_delimiter(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\'' | '`' | '<' | '>' | '|' | '&' | ';' | '(' | ')' | '[' | ']' | '{' | '}'
        )
}

fn repository_ctx_remote_temp_env_key(key: &str) -> bool {
    matches!(key, "TMPDIR" | "TMP" | "TEMP")
}

fn repository_ctx_external_repo_root_project_path(
    path: &ProjectRelativePath,
) -> Option<ProjectRelativePathBuf> {
    let mut parts = path.as_str().split('/');
    if parts.next()? != "buck-out" || parts.next()? != "v2" || parts.next()? != "external_cells" {
        return None;
    }
    let repo_kind = parts.next()?;
    if repo_kind != "bzlmod" && repo_kind != "bzlmod_generated" {
        return None;
    }
    let repo_name = parts.next()?;
    Some(ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/external_cells/{repo_kind}/{repo_name}"
    )))
}

fn repository_ctx_remote_input_entry_path(path: AbsNormPathBuf) -> Result<AbsNormPathBuf, String> {
    let metadata = match fs::symlink_metadata(path.as_path()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(path);
    }
    let target_metadata = fs::metadata(path.as_path()).map_err(|error| error.to_string())?;
    if !target_metadata.is_dir() {
        return Ok(path);
    }
    let canonical = fs::canonicalize(path.as_path()).map_err(|error| error.to_string())?;
    AbsNormPathBuf::try_from(canonical).map_err(|error| error.to_string())
}

#[cfg(test)]
pub(super) fn remote_path_relative_to_working_dir(
    remote_working_dir: &str,
    target: &str,
) -> String {
    let from = remote_path_components(remote_working_dir);
    let to = remote_path_components(target);
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(from, to)| from == to)
        .count();

    let mut relative = Vec::new();
    relative.extend(std::iter::repeat_n("..", from.len() - common));
    relative.extend(to[common..].iter().copied());

    if relative.is_empty() {
        ".".to_owned()
    } else {
        relative.join("/")
    }
}

#[cfg(test)]
fn remote_path_components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect()
}

#[derive(Debug)]
struct RepositoryCommandExecutionTarget {
    repository: String,
    program: String,
}

impl CommandExecutionTarget for RepositoryCommandExecutionTarget {
    fn re_action_key(&self) -> String {
        format!(
            "repository_ctx_execute {} {}",
            self.repository, self.program
        )
    }

    fn re_affinity_key(&self) -> String {
        self.repository.clone()
    }

    fn as_proto_action_key(&self) -> bz_data::ActionKey {
        bz_data::ActionKey {
            id: self.re_action_key().into_bytes(),
            owner: None,
            key: self.re_action_key(),
        }
    }

    fn as_proto_action_name(&self) -> bz_data::ActionName {
        bz_data::ActionName {
            category: "BazelRepositoryExecute".to_owned(),
            identifier: self.program.clone(),
        }
    }
}
