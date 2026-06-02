use super::*;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Freeze, Allocative)]
pub struct RepositoryPathLabelDep {
    pub(super) cell_name: String,
    pub(super) path: Option<String>,
    pub(super) recursive: bool,
}

impl RepositoryPathLabelDep {
    pub(super) fn cell(cell_name: String) -> Self {
        Self {
            cell_name,
            path: None,
            recursive: false,
        }
    }

    pub(super) fn cell_path(cell_name: String, path: String) -> Self {
        Self {
            cell_name,
            path: Some(path),
            recursive: false,
        }
    }

    pub(super) fn tree(cell_name: String, path: Option<String>) -> Self {
        Self {
            cell_name,
            path,
            recursive: true,
        }
    }
}

fn record_repository_input(
    recorded_inputs: &Mutex<Vec<BazelRepositoryRecordedInput>>,
    input: BazelRepositoryRecordedInput,
) {
    let mut recorded_inputs = recorded_inputs
        .lock()
        .expect("repository recorded inputs poisoned");
    if !recorded_inputs.iter().any(|existing| existing == &input) {
        recorded_inputs.push(input);
    }
}

pub(super) fn record_repository_env_var(
    repo_env: &BTreeMap<String, String>,
    recorded_inputs: &Mutex<Vec<BazelRepositoryRecordedInput>>,
    name: &str,
) -> Option<String> {
    let value = repo_env.get(name).cloned();
    record_repository_input(
        recorded_inputs,
        BazelRepositoryRecordedInput::EnvVar {
            name: name.to_owned(),
            value: value.clone(),
        },
    );
    value
}

pub(super) fn repository_recorded_file_value(path: &Path) -> io::Result<String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok("ENOENT".to_owned()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        return Ok(format!("SYMLINK:{}", target.to_string_lossy()));
    }
    if metadata.is_dir() {
        return Ok("DIR".to_owned());
    }
    if metadata.is_file() {
        let mut file = fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; 8192];
        loop {
            let len = file.read(&mut buf)?;
            if len == 0 {
                break;
            }
            hasher.update(&buf[..len]);
        }
        return Ok(format!(
            "FILE:{}",
            blake3::Hasher::finalize(&hasher).to_hex()
        ));
    }
    Ok("OTHER".to_owned())
}

pub(super) fn repository_recorded_dirents_value(path: &Path) -> io::Result<String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok("ENOENT".to_owned()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return repository_recorded_file_value(path);
    }
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    let mut hasher = blake3::Hasher::new();
    for entry in entries {
        hasher.update(entry.as_bytes());
        hasher.update(&[0]);
    }
    Ok(format!(
        "DIRENTS:{}",
        blake3::Hasher::finalize(&hasher).to_hex()
    ))
}

pub(super) fn repository_recorded_dir_tree_value(path: &Path) -> io::Result<String> {
    fn visit(base: &Path, path: &Path, hasher: &mut blake3::Hasher) -> io::Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                hasher.update(b"ENOENT");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let relative = path.strip_prefix(base).unwrap_or(path);
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        if metadata.file_type().is_symlink() {
            hasher.update(repository_recorded_file_value(path)?.as_bytes());
        } else if metadata.is_dir() {
            hasher.update(b"DIR");
            let mut entries = fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()?;
            entries.sort();
            for entry in entries {
                visit(base, &entry, hasher)?;
            }
        } else if metadata.is_file() {
            hasher.update(repository_recorded_file_value(path)?.as_bytes());
        } else {
            hasher.update(b"OTHER");
        }
        hasher.update(&[0]);
        Ok(())
    }

    let mut hasher = blake3::Hasher::new();
    visit(path, path, &mut hasher)?;
    Ok(format!(
        "DIRTREE:{}",
        blake3::Hasher::finalize(&hasher).to_hex()
    ))
}

fn repository_recorded_input_path(path: &str, working_dir: &str) -> PathBuf {
    repository_path_for_read_abs_relative_to(path, working_dir)
}

fn repository_path_is_under_working_dir(path: &Path, working_dir: &str) -> bool {
    let Ok(working_dir) = repository_path_for_write(working_dir) else {
        return false;
    };
    path == working_dir || path.starts_with(working_dir)
}

pub(super) fn record_repository_file_input(
    recorded_inputs: &Mutex<Vec<BazelRepositoryRecordedInput>>,
    path: &str,
    working_dir: &str,
) -> starlark::Result<()> {
    let resolved = repository_recorded_input_path(path, working_dir);
    if repository_path_is_under_working_dir(&resolved, working_dir) {
        return Ok(());
    }
    let value = repository_recorded_file_value(&resolved).map_err(|error| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Tier0,
            "failed to record repository file input `{}`: {}",
            resolved.to_string_lossy(),
            error
        )
    })?;
    record_repository_input(
        recorded_inputs,
        BazelRepositoryRecordedInput::File {
            path: resolved.to_string_lossy().into_owned(),
            value,
        },
    );
    Ok(())
}

pub(super) fn record_repository_dirents_input(
    recorded_inputs: &Mutex<Vec<BazelRepositoryRecordedInput>>,
    path: &str,
    working_dir: &str,
) -> starlark::Result<()> {
    let resolved = repository_recorded_input_path(path, working_dir);
    if repository_path_is_under_working_dir(&resolved, working_dir) {
        return Ok(());
    }
    let value = repository_recorded_dirents_value(&resolved).map_err(|error| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Tier0,
            "failed to record repository directory entries input `{}`: {}",
            resolved.to_string_lossy(),
            error
        )
    })?;
    record_repository_input(
        recorded_inputs,
        BazelRepositoryRecordedInput::Dirents {
            path: resolved.to_string_lossy().into_owned(),
            value,
        },
    );
    Ok(())
}

pub(super) fn record_repository_dir_tree_input(
    recorded_inputs: &Mutex<Vec<BazelRepositoryRecordedInput>>,
    path: &str,
    working_dir: &str,
) -> starlark::Result<()> {
    let resolved = repository_recorded_input_path(path, working_dir);
    if repository_path_is_under_working_dir(&resolved, working_dir) {
        return Ok(());
    }
    let value = repository_recorded_dir_tree_value(&resolved).map_err(|error| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Tier0,
            "failed to record repository directory tree input `{}`: {}",
            resolved.to_string_lossy(),
            error
        )
    })?;
    record_repository_input(
        recorded_inputs,
        BazelRepositoryRecordedInput::DirTree {
            path: resolved.to_string_lossy().into_owned(),
            value,
        },
    );
    Ok(())
}

pub(super) fn repository_should_record_watch(watch: &str) -> starlark::Result<bool> {
    match watch {
        "auto" | "yes" => Ok(true),
        "no" => Ok(false),
        other => Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "repository watch mode must be `auto`, `yes`, or `no`, got `{}`",
            other
        )
        .into()),
    }
}
