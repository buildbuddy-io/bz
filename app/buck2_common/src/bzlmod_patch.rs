/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

use buck2_error::BuckErrorContext;
use buck2_error::buck2_error;
use buck2_error::conversion::from_any_with_tag;

#[derive(Debug)]
struct NativePatchFile {
    old_path: Option<String>,
    new_path: Option<String>,
    hunks: Vec<NativePatchHunk>,
    file_mode: Option<u32>,
    renames: bool,
}

#[derive(Debug)]
struct NativePatchHunk {
    old_start: usize,
    lines: Vec<NativePatchLine>,
}

#[derive(Debug)]
enum NativePatchLine {
    Context(String),
    Add(String),
    Remove(String),
}

const NATIVE_BZLMOD_PATCH_FUZZ: usize = 2;

pub fn apply_unified_patch_file(
    directory: &Path,
    patch_file: &Path,
    patch_strip: u32,
) -> buck2_error::Result<()> {
    let patch = fs::read_to_string(patch_file)
        .with_buck_error_context(|| format!("Error reading `{}`", patch_file.display()))?;
    apply_unified_patch(directory, &patch, patch_strip)
}

pub fn apply_unified_patch(
    directory: &Path,
    patch: &str,
    patch_strip: u32,
) -> buck2_error::Result<()> {
    let file_patches = parse_native_bzlmod_patch(patch)?;
    if file_patches.is_empty() && !patch.trim().is_empty() {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "bzlmod patch did not contain a unified diff hunk"
        ));
    }
    for file_patch in file_patches {
        apply_native_bzlmod_file_patch(directory, file_patch, patch_strip)?;
    }
    Ok(())
}

fn parse_native_bzlmod_patch(patch: &str) -> buck2_error::Result<Vec<NativePatchFile>> {
    let lines = patch.lines().collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut current: Option<NativePatchFile> = None;
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        if let Some(rest) = line.strip_prefix("diff --git ") {
            finish_native_patch_file(&mut files, current.take())?;
            let mut paths = rest.split_whitespace();
            current = Some(NativePatchFile {
                old_path: paths.next().map(str::to_owned),
                new_path: paths.next().map(str::to_owned),
                hunks: Vec::new(),
                file_mode: None,
                renames: false,
            });
            index += 1;
            continue;
        }

        if let Some(rest) = line
            .strip_prefix("new mode ")
            .or_else(|| line.strip_prefix("new file mode "))
        {
            let file = current.get_or_insert_with(empty_native_patch_file);
            file.file_mode = parse_native_bzlmod_file_mode(rest.trim());
            index += 1;
            continue;
        }

        if let Some(old_path) = line.strip_prefix("rename from ") {
            let file = current.get_or_insert_with(empty_native_patch_file);
            if file.old_path.is_none() {
                file.old_path = Some(old_path.trim().to_owned());
            }
            file.renames = true;
            index += 1;
            continue;
        }

        if let Some(new_path) = line.strip_prefix("rename to ") {
            let file = current.get_or_insert_with(empty_native_patch_file);
            if file.new_path.is_none() {
                file.new_path = Some(new_path.trim().to_owned());
            }
            file.renames = true;
            index += 1;
            continue;
        }

        if let Some(old_path) = line.strip_prefix("--- ") {
            if current.as_ref().is_some_and(|file| !file.hunks.is_empty()) {
                finish_native_patch_file(&mut files, current.take())?;
            }
            let file = current.get_or_insert_with(empty_native_patch_file);
            file.old_path = Some(patch_header_path(old_path).to_owned());
            index += 1;
            if let Some(new_path) = lines.get(index).and_then(|line| line.strip_prefix("+++ ")) {
                file.new_path = Some(patch_header_path(new_path).to_owned());
                index += 1;
            }
            continue;
        }

        if let Some(new_path) = line.strip_prefix("+++ ") {
            let file = current.get_or_insert_with(empty_native_patch_file);
            file.new_path = Some(patch_header_path(new_path).to_owned());
            index += 1;
            continue;
        }

        if line.starts_with("@@ ") {
            let (hunk, next_index) = parse_native_bzlmod_hunk(&lines, index)?;
            current
                .get_or_insert_with(empty_native_patch_file)
                .hunks
                .push(hunk);
            index = next_index;
            continue;
        }

        index += 1;
    }

    finish_native_patch_file(&mut files, current)?;
    Ok(files)
}

fn finish_native_patch_file(
    files: &mut Vec<NativePatchFile>,
    file: Option<NativePatchFile>,
) -> buck2_error::Result<()> {
    if let Some(file) = file {
        if file.hunks.is_empty() && file.file_mode.is_none() && file.old_path == file.new_path {
            return Ok(());
        }
        if file.hunks.is_empty() && file.file_mode.is_none() && !file.renames {
            return Ok(());
        }
        files.push(file);
    }
    Ok(())
}

fn empty_native_patch_file() -> NativePatchFile {
    NativePatchFile {
        old_path: None,
        new_path: None,
        hunks: Vec::new(),
        file_mode: None,
        renames: false,
    }
}

fn parse_native_bzlmod_file_mode(mode: &str) -> Option<u32> {
    let mode = mode.split_whitespace().next()?;
    u32::from_str_radix(mode, 8).ok()
}

fn patch_header_path(path: &str) -> &str {
    path.split('\t')
        .next()
        .unwrap_or(path)
        .split_whitespace()
        .next()
        .unwrap_or(path)
        .trim_matches('"')
}

fn parse_native_bzlmod_hunk(
    lines: &[&str],
    start: usize,
) -> buck2_error::Result<(NativePatchHunk, usize)> {
    let (old_start, old_len, new_len) = parse_native_bzlmod_hunk_header(lines[start])?;
    let mut hunk = NativePatchHunk {
        old_start,
        lines: Vec::new(),
    };
    let mut old_count = 0usize;
    let mut new_count = 0usize;
    let mut index = start + 1;

    while index < lines.len() && (old_count < old_len || new_count < new_len) {
        let line = lines[index];
        if line.starts_with("\\ ") {
            index += 1;
            continue;
        }
        let (kind, text) = line.split_at(std::cmp::min(1, line.len()));
        match kind {
            "+" => {
                hunk.lines.push(NativePatchLine::Add(text.to_owned()));
                new_count += 1;
            }
            "-" => {
                hunk.lines.push(NativePatchLine::Remove(text.to_owned()));
                old_count += 1;
            }
            " " => {
                hunk.lines.push(NativePatchLine::Context(text.to_owned()));
                old_count += 1;
                new_count += 1;
            }
            "" => {
                hunk.lines.push(NativePatchLine::Context(String::new()));
                old_count += 1;
                new_count += 1;
            }
            _ => {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Invalid bzlmod patch hunk line `{}`",
                    line
                ));
            }
        }
        index += 1;
    }

    if old_count != old_len || new_count != new_len {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "Invalid bzlmod patch hunk: expected -{}, +{} lines, saw -{}, +{}",
            old_len,
            new_len,
            old_count,
            new_count
        ));
    }

    Ok((hunk, index))
}

fn parse_native_bzlmod_hunk_header(header: &str) -> buck2_error::Result<(usize, usize, usize)> {
    let header = header
        .strip_prefix("@@ ")
        .and_then(|header| header.split_once(" @@").map(|(header, _)| header))
        .ok_or_else(|| {
            buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid bzlmod patch hunk header `{}`",
                header
            )
        })?;
    let mut fields = header.split_whitespace();
    let (old_start, old_len) = parse_native_bzlmod_hunk_range(fields.next(), '-')?;
    let (_new_start, new_len) = parse_native_bzlmod_hunk_range(fields.next(), '+')?;
    Ok((old_start, old_len, new_len))
}

fn parse_native_bzlmod_hunk_range(
    range: Option<&str>,
    prefix: char,
) -> buck2_error::Result<(usize, usize)> {
    let range = range
        .and_then(|range| range.strip_prefix(prefix))
        .ok_or_else(|| {
            buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid bzlmod patch hunk range"
            )
        })?;
    let (start, len) = match range.split_once(',') {
        Some((start, len)) => (start, len.parse::<usize>()?),
        None => (range, 1),
    };
    Ok((start.parse::<usize>()?, len))
}

fn apply_native_bzlmod_file_patch(
    directory: &Path,
    file_patch: NativePatchFile,
    patch_strip: u32,
) -> buck2_error::Result<()> {
    let old_target_path = native_bzlmod_patch_path(&file_patch.old_path, patch_strip);
    let new_target_path = native_bzlmod_patch_path(&file_patch.new_path, patch_strip);
    let target_path = new_target_path
        .as_ref()
        .or(old_target_path.as_ref())
        .ok_or_else(|| {
            buck2_error!(
                buck2_error::ErrorTag::Input,
                "bzlmod patch file did not contain a usable target path"
            )
        })?;
    let old_target = old_target_path
        .as_deref()
        .map(|path| safe_join_native_bzlmod_patch_path(directory, path))
        .transpose()?;
    let new_target = new_target_path
        .as_deref()
        .map(|path| safe_join_native_bzlmod_patch_path(directory, path))
        .transpose()?;
    let target = new_target.as_ref().or(old_target.as_ref()).ok_or_else(|| {
        buck2_error!(
            buck2_error::ErrorTag::Input,
            "bzlmod patch file did not contain a usable target path"
        )
    })?;
    let input_target = old_target
        .as_ref()
        .filter(|path| path.exists())
        .or_else(|| new_target.as_ref().filter(|path| path.exists()));
    let old_text = match input_target {
        Some(path) => fs::read_to_string(path).map_err(|error| {
            from_any_with_tag(error, buck2_error::ErrorTag::Input).context(format!(
                "Error reading `{}` before applying bzlmod patch",
                path.display()
            ))
        })?,
        None => String::new(),
    };
    let old_lines = split_native_patch_lines(&old_text);
    let new_lines = apply_native_bzlmod_hunks(target_path, &old_lines, &file_patch.hunks)?;

    let deletes_file = file_patch.new_path.as_deref() == Some("/dev/null") && new_lines.is_empty();
    if let Some(old_target) = &old_target {
        if deletes_file
            || new_target
                .as_ref()
                .is_some_and(|new_target| new_target != old_target)
        {
            match fs::remove_file(old_target) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(
                        from_any_with_tag(error, buck2_error::ErrorTag::Input).context(format!(
                            "Error deleting `{}` after applying bzlmod patch",
                            old_target.display()
                        )),
                    );
                }
            }
        }
    }

    if deletes_file {
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_buck_error_context(|| format!("Error creating `{}`", parent.display()))?;
    }
    fs::write(target, join_native_patch_lines(&new_lines)).with_buck_error_context(|| {
        format!(
            "Error writing `{}` after applying bzlmod patch",
            target.display()
        )
    })?;
    if let Some(mode) = file_patch.file_mode {
        set_native_bzlmod_file_mode(target, mode)?;
    }
    Ok(())
}

fn native_bzlmod_patch_path(path: &Option<String>, patch_strip: u32) -> Option<String> {
    let path = path.as_deref()?;
    if path == "/dev/null" {
        return None;
    }
    patch_path_after_strip(path, patch_strip)
}

pub fn patch_path_after_strip(path: &str, patch_strip: u32) -> Option<String> {
    let path = path.split_whitespace().next()?.trim_matches('"');
    if path == "/dev/null" {
        return None;
    }
    let stripped = path
        .split('/')
        .skip(patch_strip as usize)
        .collect::<Vec<_>>()
        .join("/");
    (!stripped.is_empty()).then_some(stripped)
}

fn safe_join_native_bzlmod_patch_path(
    directory: &Path,
    relative: &str,
) -> buck2_error::Result<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "Invalid bzlmod patch path `{}`",
            relative
        ));
    }
    Ok(directory.join(path))
}

fn split_native_patch_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .map(str::to_owned)
        .collect()
}

fn join_native_patch_lines(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn apply_native_bzlmod_hunks(
    target_path: &str,
    old_lines: &[String],
    hunks: &[NativePatchHunk],
) -> buck2_error::Result<Vec<String>> {
    let mut result = old_lines.to_owned();
    let mut sorted_hunks = hunks.iter().collect::<Vec<_>>();
    sorted_hunks.sort_by_key(|hunk| hunk.old_start);

    for hunk in sorted_hunks.into_iter().rev() {
        let (position, fuzz) = native_bzlmod_patch_hunk_position(&result, hunk)
            .ok_or_else(|| native_bzlmod_patch_mismatch(target_path, hunk.old_start))?;
        apply_native_bzlmod_hunk_at(target_path, &mut result, hunk, position, fuzz)?;
    }
    Ok(result)
}

fn native_bzlmod_patch_hunk_position(
    lines: &[String],
    hunk: &NativePatchHunk,
) -> Option<(usize, usize)> {
    let source_lines = native_bzlmod_patch_source_lines(hunk);
    let target_lines = native_bzlmod_patch_target_lines(hunk);
    let default_position = hunk.old_start.saturating_sub(1);

    for fuzz in 0..=NATIVE_BZLMOD_PATCH_FUZZ {
        let max_position = native_bzlmod_patch_max_position(
            lines.len(),
            source_lines.len(),
            target_lines.len(),
            fuzz,
        )?;
        if default_position <= max_position
            && native_bzlmod_patch_source_matches(lines, &source_lines, default_position, fuzz)
        {
            return Some((default_position, fuzz));
        }

        if default_position > max_position {
            for position in (0..=max_position).rev() {
                if native_bzlmod_patch_source_matches(lines, &source_lines, position, fuzz) {
                    return Some((position, fuzz));
                }
            }
            continue;
        }

        let max_delta = default_position.max(max_position - default_position);
        for delta in 1..=max_delta {
            if let Some(position) = default_position.checked_sub(delta) {
                if native_bzlmod_patch_source_matches(lines, &source_lines, position, fuzz) {
                    return Some((position, fuzz));
                }
            }
            if let Some(position) = default_position.checked_add(delta) {
                if position <= max_position
                    && native_bzlmod_patch_source_matches(lines, &source_lines, position, fuzz)
                {
                    return Some((position, fuzz));
                }
            }
        }
    }
    None
}

fn native_bzlmod_patch_max_position(
    line_count: usize,
    source_line_count: usize,
    target_line_count: usize,
    fuzz: usize,
) -> Option<usize> {
    let source_end = source_line_count.checked_sub(fuzz)?;
    let target_end = target_line_count.checked_sub(fuzz)?;
    if fuzz > source_end || fuzz > target_end {
        return None;
    }
    line_count.checked_sub(source_end)
}

fn native_bzlmod_patch_source_matches(
    lines: &[String],
    source_lines: &[String],
    position: usize,
    fuzz: usize,
) -> bool {
    let Some(source_end) = source_lines.len().checked_sub(fuzz) else {
        return false;
    };
    if fuzz > source_end || position + source_end > lines.len() {
        return false;
    }

    (fuzz..source_end).all(|index| {
        lines
            .get(position + index)
            .is_some_and(|line| line == &source_lines[index])
    })
}

fn apply_native_bzlmod_hunk_at(
    target_path: &str,
    lines: &mut Vec<String>,
    hunk: &NativePatchHunk,
    position: usize,
    fuzz: usize,
) -> buck2_error::Result<()> {
    let source_lines = native_bzlmod_patch_source_lines(hunk);
    let target_lines = native_bzlmod_patch_target_lines(hunk);
    let source_end = source_lines
        .len()
        .checked_sub(fuzz)
        .ok_or_else(|| native_bzlmod_patch_mismatch(target_path, hunk.old_start))?;
    let target_end = target_lines
        .len()
        .checked_sub(fuzz)
        .ok_or_else(|| native_bzlmod_patch_mismatch(target_path, hunk.old_start))?;
    let replace_start = position + fuzz;
    let replace_end = position + source_end;

    lines.splice(
        replace_start..replace_end,
        target_lines[fuzz..target_end].iter().cloned(),
    );
    Ok(())
}

fn native_bzlmod_patch_source_lines(hunk: &NativePatchHunk) -> Vec<String> {
    hunk.lines
        .iter()
        .filter_map(|line| match line {
            NativePatchLine::Context(line) | NativePatchLine::Remove(line) => Some(line.clone()),
            NativePatchLine::Add(_) => None,
        })
        .collect()
}

fn native_bzlmod_patch_target_lines(hunk: &NativePatchHunk) -> Vec<String> {
    hunk.lines
        .iter()
        .filter_map(|line| match line {
            NativePatchLine::Context(line) | NativePatchLine::Add(line) => Some(line.clone()),
            NativePatchLine::Remove(_) => None,
        })
        .collect()
}

fn native_bzlmod_patch_mismatch(target_path: &str, line: usize) -> buck2_error::Error {
    buck2_error!(
        buck2_error::ErrorTag::Input,
        "bzlmod patch does not apply cleanly to `{}` near line {}",
        target_path,
        line
    )
}

#[cfg(unix)]
fn set_native_bzlmod_file_mode(path: &Path, mode: u32) -> buck2_error::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode)).with_buck_error_context(|| {
        format!(
            "Error setting permissions on `{}` after applying bzlmod patch",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn set_native_bzlmod_file_mode(_path: &Path, _mode: u32) -> buck2_error::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use indoc::indoc;

    use super::apply_unified_patch;

    #[test]
    fn test_native_bzlmod_patch_updates_file() -> buck2_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            "module(name = \"demo\")\nbazel_dep(name = \"old\", version = \"1.0\")\n",
        )
        .unwrap();

        let patch = indoc!(
            r#"
            diff --git a/MODULE.bazel b/MODULE.bazel
            --- a/MODULE.bazel
            +++ b/MODULE.bazel
            @@ -1,2 +1,2 @@
             module(name = "demo")
            -bazel_dep(name = "old", version = "1.0")
            +bazel_dep(name = "new", version = "2.0")
            "#
        );
        apply_unified_patch(dir.path(), patch, 1)?;

        assert_eq!(
            fs::read_to_string(dir.path().join("MODULE.bazel")).unwrap(),
            "module(name = \"demo\")\nbazel_dep(name = \"new\", version = \"2.0\")\n",
        );
        Ok(())
    }

    #[test]
    fn test_native_bzlmod_patch_applies_hunk_with_line_offset() -> buck2_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            "# inserted upstream\nmodule(name = \"demo\")\nbazel_dep(name = \"old\", version = \"1.0\")\n",
        )
        .unwrap();

        let patch = indoc!(
            r#"
            diff --git a/MODULE.bazel b/MODULE.bazel
            --- a/MODULE.bazel
            +++ b/MODULE.bazel
            @@ -1,2 +1,2 @@
             module(name = "demo")
            -bazel_dep(name = "old", version = "1.0")
            +bazel_dep(name = "new", version = "2.0")
            "#
        );
        apply_unified_patch(dir.path(), patch, 1)?;

        assert_eq!(
            fs::read_to_string(dir.path().join("MODULE.bazel")).unwrap(),
            "# inserted upstream\nmodule(name = \"demo\")\nbazel_dep(name = \"new\", version = \"2.0\")\n",
        );
        Ok(())
    }

    #[test]
    fn test_native_bzlmod_patch_applies_hunk_with_context_fuzz() -> buck2_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("foo.cc"),
            "#include <stdio.h>\n\nvoid main(){\n  printf(\"Hello foo\");\n}\n",
        )
        .unwrap();

        let patch = concat!(
            "diff --git a/foo.cc b/foo.cc\n",
            "--- a/foo.cc\n",
            "+++ b/foo.cc\n",
            "@@ -2,4 +2,5 @@\n",
            " \n",
            " void main(){\n",
            "   printf(\"Hello foo\");\n",
            "+  printf(\"Hello from patch\");\n",
            " WRONG CONTEXT LINE\n",
        );
        apply_unified_patch(dir.path(), patch, 1)?;

        assert_eq!(
            fs::read_to_string(dir.path().join("foo.cc")).unwrap(),
            "#include <stdio.h>\n\nvoid main(){\n  printf(\"Hello foo\");\n  printf(\"Hello from patch\");\n}\n",
        );
        Ok(())
    }

    #[test]
    fn test_native_bzlmod_patch_rejects_parent_directory_escape() {
        let dir = tempfile::tempdir().unwrap();
        let patch = indoc!(
            r#"
            --- ../MODULE.bazel
            +++ ../MODULE.bazel
            @@ -0,0 +1 @@
            +module(name = "bad")
            "#
        );

        assert!(apply_unified_patch(dir.path(), patch, 0).is_err());
    }
}
