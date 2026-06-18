use super::*;

#[derive(Clone, Debug, Deserialize)]
struct BzlmodModuleLockfile {
    #[serde(default, rename = "registryFileHashes")]
    registry_file_hashes: BTreeMap<String, Option<String>>,
    #[serde(default, rename = "selectedYankedVersions")]
    selected_yanked_versions: BTreeMap<String, String>,
    #[serde(default, rename = "moduleExtensions")]
    module_extensions: BTreeMap<String, BTreeMap<String, BzlmodModuleLockfileExtension>>,
    #[serde(default)]
    facts: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct BzlmodModuleLockfileExtension {
    #[serde(default, rename = "generatedRepoSpecs")]
    generated_repo_specs: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Allocative, Pagable)]
pub(super) struct BzlmodModuleLockfileData {
    pub(super) registry_file_hashes: BTreeMap<String, Option<String>>,
    pub(super) selected_yanked_versions: BTreeMap<(String, String), String>,
    pub(super) extension_generated_repos: BTreeMap<String, BTreeSet<String>>,
    pub(super) extension_facts: BTreeSet<String>,
}

const BZLMOD_HIDDEN_LOCKFILE_SCHEMA_FIELD: &str = "buck2HiddenLockfileSchemaVersion";
const BZLMOD_HIDDEN_LOCKFILE_SCHEMA_VERSION: u64 = 3;

/// Bazel records registry files that 404'd with this marker value
/// (GsonTypeAdapterUtil.OptionalChecksumTypeAdapter.NOT_FOUND_MARKER).
const BZLMOD_LOCKFILE_NOT_FOUND_MARKER: &str = "not found";

/// The lockfile version current Bazel writes (BazelLockFileValue.LOCK_FILE_VERSION).
const BZLMOD_LOCKFILE_VERSION: u64 = 26;

pub(super) fn empty_bzlmod_lockfile_data() -> BzlmodModuleLockfileData {
    BzlmodModuleLockfileData {
        registry_file_hashes: BTreeMap::new(),
        selected_yanked_versions: BTreeMap::new(),
        extension_generated_repos: BTreeMap::new(),
        extension_facts: BTreeSet::new(),
    }
}

pub(super) async fn bzlmod_lockfile_data(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> bz_error::Result<BzlmodModuleLockfileData> {
    let lockfile_path = ConfigPath::Project(
        cell_path
            .as_project_relative_path()
            .join(ForwardRelativePath::new("MODULE.bazel.lock")?),
    );
    let Some(lines) = file_ops.read_file_lines_if_exists(&lockfile_path).await? else {
        return Ok(empty_bzlmod_lockfile_data());
    };
    bzlmod_lockfile_data_from_str(&lines.join("\n"))
}

pub(super) async fn bzlmod_vendor_file_data(
    cell_path: &CellRootPath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> bz_error::Result<BzlmodVendorFileValue> {
    let vendor_file_path = ConfigPath::Project(
        cell_path
            .as_project_relative_path()
            .join(ForwardRelativePath::new("VENDOR.bazel")?),
    );
    let Some(lines) = file_ops
        .read_file_lines_if_exists(&vendor_file_path)
        .await?
    else {
        return Ok(BzlmodVendorFileValue {
            ignored_repos: Vec::new(),
            pinned_repos: Vec::new(),
        });
    };
    Ok(BzlmodVendorFileValue {
        ignored_repos: bzlmod_vendor_repos_from_calls(&lines, "ignore("),
        pinned_repos: bzlmod_vendor_repos_from_calls(&lines, "pin("),
    })
}

fn bzlmod_vendor_repos_from_calls(lines: &[String], function: &str) -> Vec<String> {
    let mut repos = vendor_bzl_calls(lines, function)
        .into_iter()
        .flat_map(|call| {
            vendor_bzl_call_args(&call)
                .into_iter()
                .filter_map(|arg| bzlmod_string_literal_prefix(arg.trim()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    repos.sort_unstable();
    repos.dedup();
    repos
}

fn vendor_bzl_calls(lines: &[String], function: &str) -> Vec<String> {
    let mut calls = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;

    for line in lines {
        if current.is_empty() {
            let rest = line.trim_start();
            if !rest.starts_with(function) {
                continue;
            };
            let line = vendor_strip_bzl_comment(line);
            let rest = line.trim_start();
            depth = vendor_paren_delta(rest);
            current.push_str(rest);
        } else {
            let line = vendor_strip_bzl_comment(line);
            current.push('\n');
            current.push_str(&line);
            depth += vendor_paren_delta(&line);
        }

        if depth <= 0 {
            calls.push(std::mem::take(&mut current));
            depth = 0;
        }
    }

    calls
}

fn vendor_bzl_call_args(call: &str) -> Vec<String> {
    let Some((_, args)) = call.split_once('(') else {
        return Vec::new();
    };
    let args = args.trim();
    let args = args.strip_suffix(')').unwrap_or(args);
    vendor_bzl_split_top_level(args, ',')
}

fn vendor_bzl_split_top_level(s: &str, delimiter: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut quote = '\0';
    let mut escaped = false;
    let mut depth = 0i32;

    for (idx, ch) in s.char_indices() {
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
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ if ch == delimiter && depth == 0 => {
                parts.push(s[start..idx].to_owned());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    parts.push(s[start..].to_owned());
    parts
}

fn vendor_strip_bzl_comment(line: &str) -> String {
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
            return line[..idx].to_owned();
        }
    }

    line.to_owned()
}

fn vendor_paren_delta(s: &str) -> i32 {
    s.chars()
        .map(|ch| match ch {
            '(' => 1,
            ')' => -1,
            _ => 0,
        })
        .sum()
}

pub(super) fn bzlmod_lockfile_data_from_str(
    contents: &str,
) -> bz_error::Result<BzlmodModuleLockfileData> {
    let lockfile: BzlmodModuleLockfile = serde_json::from_str(contents)
        .buck_error_context("Error parsing MODULE.bazel.lock for bzlmod generated repositories")?;
    let mut repos_by_extension = BTreeMap::new();
    for (extension_key, evaluations) in lockfile.module_extensions {
        for evaluation in evaluations.into_values() {
            repos_by_extension
                .entry(extension_key.clone())
                .or_insert_with(BTreeSet::new)
                .extend(evaluation.generated_repo_specs.into_keys());
        }
    }
    Ok(BzlmodModuleLockfileData {
        registry_file_hashes: lockfile
            .registry_file_hashes
            .into_iter()
            .map(|(url, hash)| {
                // Bazel records files that 404'd with a "not found" marker
                // (GsonTypeAdapterUtil.OptionalChecksumTypeAdapter), not null.
                let hash = hash.filter(|hash| hash != BZLMOD_LOCKFILE_NOT_FOUND_MARKER);
                (url, hash)
            })
            .collect(),
        selected_yanked_versions: lockfile
            .selected_yanked_versions
            .into_iter()
            .filter_map(|(key, info)| {
                let (name, version) = key.rsplit_once('@')?;
                Some(((name.to_owned(), version.to_owned()), info))
            })
            .collect(),
        extension_generated_repos: repos_by_extension,
        extension_facts: lockfile.facts.into_keys().collect(),
    })
}

/// Mirror of Bazel's BazelLockFileModule: after module resolution, write the
/// registry file hashes and selected yanked versions back to MODULE.bazel.lock
/// so subsequent resolutions (by bz or by Bazel) can avoid registry I/O.
/// Fields bz does not produce (module extension results written by Bazel) are
/// preserved verbatim, existing entries are kept rather than garbage-collected,
/// and the file is only rewritten when its contents change.
pub(super) fn bzlmod_write_back_lockfile(
    project_fs: &ProjectRoot,
    registry_file_hashes: &BTreeMap<String, Option<String>>,
    selected_yanked_versions: &BTreeMap<(String, String), String>,
) -> bz_error::Result<()> {
    let path = project_fs.resolve(&ProjectRelativePathBuf::unchecked_new(
        "MODULE.bazel.lock".to_owned(),
    ));
    let existing_text = match fs::read_to_string(path.as_path()) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_buck_error_context(|| format!("Error reading `{}`", path.display()));
        }
    };
    // Fields bz does not produce are carried through as raw JSON text so
    // their formatting and key order (gson writes fields in declaration
    // order, not sorted) survive byte for byte. A malformed lockfile is
    // ignored and rewritten, like Bazel does.
    let mut existing: BTreeMap<String, Box<serde_json::value::RawValue>> = existing_text
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();

    let mut hashes: BTreeMap<String, String> = existing
        .remove("registryFileHashes")
        .and_then(|value| serde_json::from_str(value.get()).ok())
        .unwrap_or_default();
    for (url, hash) in registry_file_hashes {
        hashes.insert(
            url.clone(),
            hash.clone()
                .unwrap_or_else(|| BZLMOD_LOCKFILE_NOT_FOUND_MARKER.to_owned()),
        );
    }

    let mut yanked: BTreeMap<String, String> = existing
        .remove("selectedYankedVersions")
        .and_then(|value| serde_json::from_str(value.get()).ok())
        .unwrap_or_default();
    for ((name, version), info) in selected_yanked_versions {
        yanked.insert(format!("{name}@{version}"), info.clone());
    }

    let version = existing
        .remove("lockFileVersion")
        .and_then(|value| value.get().trim().parse::<u64>().ok())
        .unwrap_or(BZLMOD_LOCKFILE_VERSION);

    // Top-level fields in the order Bazel writes them (BazelLockFileValue
    // field declaration order), so alternating bz and Bazel runs don't churn
    // the checked-in file.
    let mut fields: Vec<(String, String)> = vec![
        ("lockFileVersion".to_owned(), version.to_string()),
        (
            "registryFileHashes".to_owned(),
            bzlmod_lockfile_string_map_json(&hashes)?,
        ),
        (
            "selectedYankedVersions".to_owned(),
            bzlmod_lockfile_string_map_json(&yanked)?,
        ),
    ];
    for known in ["moduleExtensions", "facts"] {
        // Bazel always writes these fields, even when empty.
        let value = existing
            .remove(known)
            .map(|value| value.get().to_owned())
            .unwrap_or_else(|| "{}".to_owned());
        fields.push((known.to_owned(), value));
    }
    fields.extend(
        existing
            .into_iter()
            .map(|(key, value)| (key, value.get().to_owned())),
    );

    let new_text = bzlmod_lockfile_json(&fields);
    if existing_text.as_deref() == Some(new_text.as_str()) {
        return Ok(());
    }

    let temp = path
        .as_path()
        .with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&temp, &new_text).with_buck_error_context(|| {
        format!("Error writing temporary lockfile `{}`", temp.display())
    })?;
    fs::rename(&temp, path.as_path())
        .with_buck_error_context(|| format!("Error committing lockfile `{}`", path.display()))?;
    Ok(())
}

/// Render a sorted string map with gson-compatible pretty-printing at the
/// lockfile's top-level nesting depth.
fn bzlmod_lockfile_string_map_json(map: &BTreeMap<String, String>) -> bz_error::Result<String> {
    let rendered =
        serde_json::to_string_pretty(map).buck_error_context("Error encoding lockfile contents")?;
    Ok(rendered.replace('\n', "\n  "))
}

/// Assemble the lockfile from pre-rendered top-level fields, preserving the
/// given field order. Values must already be indented for top-level nesting.
fn bzlmod_lockfile_json(fields: &[(String, String)]) -> String {
    let mut out = String::from("{");
    let mut first = true;
    for (key, value) in fields {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str("\n  ");
        out.push_str(&serde_json::Value::String(key.clone()).to_string());
        out.push_str(": ");
        out.push_str(value);
    }
    out.push_str("\n}\n");
    out
}

pub(super) fn bzlmod_hidden_lockfile_schema_matches(contents: &str) -> bool {
    let Ok(lockfile) = serde_json::from_str::<serde_json::Value>(contents) else {
        return false;
    };
    lockfile
        .get(BZLMOD_HIDDEN_LOCKFILE_SCHEMA_FIELD)
        .and_then(|value| value.as_u64())
        == Some(BZLMOD_HIDDEN_LOCKFILE_SCHEMA_VERSION)
}

pub(super) fn bzlmod_lockfile_extension_key(
    extension_id: &BzlmodExtensionId,
    canonical_repo_names_by_cell: &BTreeMap<String, String>,
) -> bz_error::Result<String> {
    let canonical_repo_name = if extension_id.bzl_cell_name == "root" {
        ""
    } else {
        canonical_repo_names_by_cell
            .get(&extension_id.bzl_cell_name)
            .ok_or_else(|| {
                bz_error!(
                    bz_error::ErrorTag::Input,
                    "bzlmod module extension `{}//{}%{}` resolves to unknown cell `{}`",
                    extension_id.bzl_cell_name,
                    extension_id.bzl_path,
                    extension_id.extension_name,
                    extension_id.bzl_cell_name
                )
            })?
            .as_str()
    };
    if canonical_repo_name.is_empty() {
        return Ok(format!(
            "//{}%{}",
            bzlmod_bzl_path_to_label_path(&extension_id.bzl_path),
            extension_id.extension_name
        ));
    }
    Ok(format!(
        "@@{}//{}%{}",
        canonical_repo_name,
        bzlmod_bzl_path_to_label_path(&extension_id.bzl_path),
        extension_id.extension_name
    ))
}

fn bzlmod_bzl_path_to_label_path(path: &str) -> String {
    if let Some((package, target)) = path.rsplit_once('/') {
        format!("{package}:{target}")
    } else {
        format!(":{path}")
    }
}
