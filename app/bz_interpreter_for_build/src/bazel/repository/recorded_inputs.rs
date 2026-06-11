use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

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
    recorded_inputs: &Mutex<BazelRepositoryRecordedInputSet>,
    input: BazelRepositoryRecordedInput,
) {
    recorded_inputs
        .lock()
        .expect("repository recorded inputs poisoned")
        .insert(input);
}

pub(super) fn record_repository_env_var(
    repo_env: &BTreeMap<String, String>,
    recorded_inputs: &Mutex<BazelRepositoryRecordedInputSet>,
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

pub fn repository_recorded_file_value(path: &Path) -> io::Result<String> {
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

pub fn repository_recorded_dirents_value(path: &Path) -> io::Result<String> {
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

/// One pending `blake3::Hasher::update` of the DIRTREE byte stream: either
/// literal bytes, or the `repository_recorded_file_value` of a regular file
/// whose contents are hashed in parallel before the stream is replayed.
enum DirTreeHashUpdate {
    Bytes(Vec<u8>),
    FileValue(PathBuf),
}

pub fn repository_recorded_dir_tree_value(path: &Path) -> io::Result<String> {
    // DIRTREE values are persisted as recorded inputs across daemons and
    // re-verified elsewhere (including by bz_external_cells), so the hashed
    // byte stream must stay byte-identical to the original sequential
    // implementation: the walk records every hasher update in visit order,
    // only regular-file content hashing happens in parallel, and the updates
    // are then replayed sequentially in the recorded order.
    fn visit(base: &Path, path: &Path, updates: &mut Vec<DirTreeHashUpdate>) -> io::Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                updates.push(DirTreeHashUpdate::Bytes(b"ENOENT".to_vec()));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let relative = path.strip_prefix(base).unwrap_or(path);
        let mut header = relative.to_string_lossy().into_owned().into_bytes();
        header.push(0);
        updates.push(DirTreeHashUpdate::Bytes(header));
        if metadata.file_type().is_symlink() {
            updates.push(DirTreeHashUpdate::Bytes(
                repository_recorded_file_value(path)?.into_bytes(),
            ));
        } else if metadata.is_dir() {
            updates.push(DirTreeHashUpdate::Bytes(b"DIR".to_vec()));
            let mut entries = fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()?;
            entries.sort();
            for entry in entries {
                visit(base, &entry, updates)?;
            }
        } else if metadata.is_file() {
            updates.push(DirTreeHashUpdate::FileValue(path.to_owned()));
        } else {
            updates.push(DirTreeHashUpdate::Bytes(b"OTHER".to_vec()));
        }
        updates.push(DirTreeHashUpdate::Bytes(vec![0]));
        Ok(())
    }

    let mut updates = Vec::new();
    visit(path, path, &mut updates)?;

    let files = updates
        .iter()
        .filter_map(|update| match update {
            DirTreeHashUpdate::FileValue(path) => Some(path.as_path()),
            DirTreeHashUpdate::Bytes(_) => None,
        })
        .collect::<Vec<_>>();
    let mut file_values = repository_recorded_file_values_in_parallel(&files).into_iter();

    let mut hasher = blake3::Hasher::new();
    for update in &updates {
        match update {
            DirTreeHashUpdate::Bytes(bytes) => {
                hasher.update(bytes);
            }
            DirTreeHashUpdate::FileValue(_) => {
                let value = file_values
                    .next()
                    .expect("one recorded file value per file update")?;
                hasher.update(value.as_bytes());
            }
        }
    }
    Ok(format!(
        "DIRTREE:{}",
        blake3::Hasher::finalize(&hasher).to_hex()
    ))
}

/// Computes `repository_recorded_file_value` for each path on a bounded pool
/// of threads, returning results in input order.
fn repository_recorded_file_values_in_parallel(files: &[&Path]) -> Vec<io::Result<String>> {
    if files.len() <= 1 {
        return files
            .iter()
            .map(|path| repository_recorded_file_value(path))
            .collect();
    }
    let worker_count = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(files.len());
    let next = AtomicUsize::new(0);
    let results = files
        .iter()
        .map(|_| Mutex::new(None))
        .collect::<Vec<Mutex<Option<io::Result<String>>>>>();
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(index) else {
                        break;
                    };
                    *results[index]
                        .lock()
                        .expect("repository recorded file hash result poisoned") =
                        Some(repository_recorded_file_value(path));
                }
            });
        }
    });
    results
        .into_iter()
        .map(|result| {
            result
                .into_inner()
                .expect("repository recorded file hash result poisoned")
                .expect("every file index is claimed by a worker")
        })
        .collect()
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
    recorded_inputs: &Mutex<BazelRepositoryRecordedInputSet>,
    path: &str,
    working_dir: &str,
) -> starlark::Result<()> {
    let resolved = repository_recorded_input_path(path, working_dir);
    if repository_path_is_under_working_dir(&resolved, working_dir) {
        return Ok(());
    }
    let value = repository_recorded_file_value(&resolved).map_err(|error| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Tier0,
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
    recorded_inputs: &Mutex<BazelRepositoryRecordedInputSet>,
    path: &str,
    working_dir: &str,
) -> starlark::Result<()> {
    let resolved = repository_recorded_input_path(path, working_dir);
    if repository_path_is_under_working_dir(&resolved, working_dir) {
        return Ok(());
    }
    let value = repository_recorded_dirents_value(&resolved).map_err(|error| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Tier0,
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
    recorded_inputs: &Mutex<BazelRepositoryRecordedInputSet>,
    path: &str,
    working_dir: &str,
) -> starlark::Result<()> {
    let resolved = repository_recorded_input_path(path, working_dir);
    if repository_path_is_under_working_dir(&resolved, working_dir) {
        return Ok(());
    }
    let value = repository_recorded_dir_tree_value(&resolved).map_err(|error| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Tier0,
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
        other => Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "repository watch mode must be `auto`, `yes`, or `no`, got `{}`",
            other
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copy of the original, fully sequential implementation of
    /// `repository_recorded_dir_tree_value`. DIRTREE values are persisted as
    /// recorded inputs across the fleet, so the parallel implementation must
    /// stay byte-identical to this reference forever.
    fn sequential_reference_dir_tree_value(path: &Path) -> io::Result<String> {
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

    fn recorded_dir_tree_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "buck2-recorded-dir-tree-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn test_repository_recorded_dir_tree_value_matches_sequential_reference() {
        use std::os::unix::fs::PermissionsExt;

        let dir = recorded_dir_tree_test_dir("fixture");
        let tree = dir.join("tree");
        fs::create_dir_all(tree.join("a/b/c")).unwrap();
        fs::create_dir_all(tree.join("empty")).unwrap();
        fs::write(tree.join("top.txt"), "top").unwrap();
        fs::write(tree.join("a/empty-file"), "").unwrap();
        for index in 0..24 {
            fs::write(
                tree.join("a/b").join(format!("file{index}.txt")),
                format!("contents-{index}"),
            )
            .unwrap();
        }
        fs::write(tree.join("a/b/c/deep.txt"), "deep").unwrap();
        let exec = tree.join("a/run.sh");
        fs::write(&exec, "#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&exec).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&exec, perms).unwrap();
        std::os::unix::fs::symlink("../top.txt", tree.join("a/link")).unwrap();
        std::os::unix::fs::symlink("missing-target", tree.join("a/b/dangling")).unwrap();

        let parallel = repository_recorded_dir_tree_value(&tree).unwrap();
        assert_eq!(
            parallel,
            sequential_reference_dir_tree_value(&tree).unwrap()
        );

        fs::write(tree.join("top.txt"), "changed").unwrap();
        let parallel_after = repository_recorded_dir_tree_value(&tree).unwrap();
        assert_ne!(parallel_after, parallel);
        assert_eq!(
            parallel_after,
            sequential_reference_dir_tree_value(&tree).unwrap()
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_repository_recorded_dir_tree_value_missing_root_matches_sequential_reference() {
        let dir = recorded_dir_tree_test_dir("missing");
        let missing = dir.join("missing");

        assert_eq!(
            repository_recorded_dir_tree_value(&missing).unwrap(),
            sequential_reference_dir_tree_value(&missing).unwrap()
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}
