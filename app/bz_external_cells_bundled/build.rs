/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Generate source files containing bundled prelude and bazel_tools trees.

use std::collections::BTreeSet;
use std::io;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

fn main() {
    imp().unwrap();
}

fn imp() -> io::Result<()> {
    let out_path = std::env::var_os("OUT_DIR").unwrap();
    let include_file = Path::new(&out_path).join("include.rs");
    let manifest_path = std::env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let repo_root = Path::new(&manifest_path)
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let prelude_path = source_tree_path(repo_root, "cells/prelude", "prelude.bzl")?;
    let bazel_tools_path = source_tree_path(
        repo_root,
        "cells/bazel_tools",
        "tools/cpp/toolchain_utils.bzl",
    )?;

    // Self-check.
    assert!(prelude_path.join("prelude.bzl").exists());
    assert!(
        bazel_tools_path
            .join("tools/cpp/toolchain_utils.bzl")
            .exists()
    );

    println!("cargo:rerun-if-changed={}", prelude_path.display());
    println!("cargo:rerun-if-changed={}", bazel_tools_path.display());

    let include_file = std::fs::File::create(&include_file)?;
    let cargo_manifest_args = Path::new(&out_path).with_file_name("build_script.out_dir-0.params");
    let runfiles_root = Path::new(&manifest_path).ancestors().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "build_script.cargo_runfiles")
    });
    let bazel_runfiles_manifest = std::env::var_os("RUNFILES_MANIFEST_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            Path::new(&out_path)
                .parent()
                .map(|parent| parent.join("build_script-.runfiles_manifest"))
                .filter(|path| path.exists())
        })
        .or_else(|| {
            let exe = std::env::current_exe().ok()?;
            let manifest =
                exe.with_file_name(format!("{}.runfiles_manifest", exe.file_name()?.to_str()?));
            manifest.exists().then_some(manifest)
        })
        .or_else(|| {
            let cwd = std::env::current_dir().ok()?;
            for ancestor in cwd.ancestors() {
                let manifest = ancestor.join("build_script-.runfiles_manifest");
                if manifest.exists() {
                    return Some(manifest);
                }
            }
            None
        });
    if let Some(runfiles_root) = runfiles_root.filter(|_| cargo_manifest_args.exists()) {
        write_include_file_from_cargo_manifest_args(
            &cargo_manifest_args,
            runfiles_root,
            Path::new(&out_path),
            include_file,
        )?;
    } else if let Some(runfiles_manifest) = bazel_runfiles_manifest {
        write_include_file_from_runfiles_manifest(&runfiles_manifest, include_file)?;
    } else {
        write_include_file(
            &[
                SourceTree {
                    module: "prelude",
                    path: &prelude_path,
                    sentinel: "prelude.bzl",
                },
                SourceTree {
                    module: "bazel_tools",
                    path: &bazel_tools_path,
                    sentinel: "tools/cpp/toolchain_utils.bzl",
                },
            ],
            include_file,
        )?;
    }

    Ok(())
}

fn source_tree_path(repo_root: &Path, name: &str, sentinel: &str) -> io::Result<PathBuf> {
    let cwd_path = std::env::current_dir()?.join(name);
    if cwd_path.join(sentinel).exists() {
        return Ok(cwd_path);
    }
    Ok(repo_root.join(name))
}

fn as_unix_like(path: &Path) -> String {
    path.to_str().unwrap().replace('\\', "/")
}

struct SourceTree<'a> {
    module: &'static str,
    path: &'a Path,
    sentinel: &'static str,
}

fn write_include_file(
    source_trees: &[SourceTree<'_>],
    mut include_file: impl io::Write,
) -> io::Result<()> {
    write_include_header(&mut include_file)?;

    for source_tree in source_trees {
        write_include_module_header(&mut include_file, source_tree.module)?;

        let mut files = Vec::new();
        for res in walkdir::WalkDir::new(source_tree.path) {
            let entry = res.map_err(|e| e.into_io_error().unwrap())?;
            if !entry.file_type().is_file() {
                continue;
            }

            files.push((
                as_unix_like(entry.path().strip_prefix(source_tree.path).unwrap()),
                entry.path().to_owned(),
            ));
        }
        files.sort_by(|(a, _), (b, _)| a.cmp(b));

        if !files.iter().any(|(path, _)| path == source_tree.sentinel) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} directory `{}` did not contain `{}`; current directory is `{}`",
                    source_tree.module,
                    source_tree.path.display(),
                    source_tree.sentinel,
                    std::env::current_dir()?.display()
                ),
            ));
        }

        let files = files
            .into_iter()
            .map(|(path, contents_path)| (path, contents_path.clone(), contents_path));
        let files = normalize_bazel_tools_embedded_files(source_tree.module, files);
        for (path, contents_path, metadata_path) in files {
            write_include_entry(&mut include_file, &path, &contents_path, &metadata_path)?;
        }

        write_include_module_footer(&mut include_file)?;
    }

    Ok(())
}

fn write_include_module_from_collected<I>(
    mut include_file: impl io::Write,
    module: &str,
    sentinel: &str,
    files: I,
) -> io::Result<()>
where
    I: IntoIterator<Item = (String, PathBuf, PathBuf)>,
{
    let mut files: Vec<_> = files.into_iter().collect();
    files = normalize_bazel_tools_embedded_files(module, files);
    files.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));

    if !files.iter().any(|(path, _, _)| path == sentinel) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "input data for bundled `{}` did not contain `{}`",
                module, sentinel
            ),
        ));
    }

    write_include_module_header(&mut include_file, module)?;
    for (path, contents_path, metadata_path) in files {
        write_include_entry(&mut include_file, &path, &contents_path, &metadata_path)?;
    }
    write_include_module_footer(&mut include_file)
}

fn normalize_bazel_tools_embedded_files<I>(
    module: &str,
    files: I,
) -> Vec<(String, PathBuf, PathBuf)>
where
    I: IntoIterator<Item = (String, PathBuf, PathBuf)>,
{
    let files = files.into_iter().collect::<Vec<_>>();
    if module != "bazel_tools" {
        return files;
    }

    let build_tools_dirs = files
        .iter()
        .filter_map(|(path, _, _)| path.strip_suffix("BUILD.tools"))
        .map(|prefix| prefix.to_owned())
        .collect::<BTreeSet<_>>();

    files
        .into_iter()
        .filter_map(|(path, contents_path, metadata_path)| {
            if let Some(prefix) = path.strip_suffix("BUILD.tools") {
                return Some((format!("{prefix}BUILD"), contents_path, metadata_path));
            }
            if let Some(prefix) = path.strip_suffix("MODULE.tools") {
                return Some((
                    format!("{prefix}MODULE.bazel"),
                    contents_path,
                    metadata_path,
                ));
            }
            if let Some(prefix) = path.strip_suffix(".bzl.tools") {
                return Some((format!("{prefix}.bzl"), contents_path, metadata_path));
            }
            for build_tools_dir in &build_tools_dirs {
                if path == format!("{build_tools_dir}BUILD")
                    || path == format!("{build_tools_dir}BUILD.bazel")
                {
                    return None;
                }
            }
            Some((path, contents_path, metadata_path))
        })
        .collect()
}

fn write_include_file_from_runfiles_manifest(
    manifest: &Path,
    mut include_file: impl io::Write,
) -> io::Result<()> {
    write_include_header(&mut include_file)?;

    let manifest = std::fs::read_to_string(manifest)?;
    for (module, sentinel) in [
        ("prelude", "prelude.bzl"),
        ("bazel_tools", "tools/cpp/toolchain_utils.bzl"),
    ] {
        let files = manifest.lines().filter_map(|line| {
            let (runfile_path, contents_path) = line.split_once(' ')?;
            let path = runfile_path
                .strip_prefix(&format!("_main/cells/{module}/"))
                .or_else(|| runfile_path.strip_prefix(&format!("cells/{module}/")))
                .or_else(|| runfile_path.strip_prefix(&format!("_main/{module}/")))
                .or_else(|| runfile_path.strip_prefix(&format!("{module}/")))?;
            Some((
                path.to_owned(),
                PathBuf::from(contents_path),
                PathBuf::from(contents_path),
            ))
        });

        write_include_module_from_collected(&mut include_file, module, sentinel, files)?;
    }

    Ok(())
}

fn write_include_file_from_cargo_manifest_args(
    manifest_args: &Path,
    runfiles_root: &Path,
    out_dir: &Path,
    mut include_file: impl io::Write,
) -> io::Result<()> {
    write_include_header(&mut include_file)?;

    let args = std::fs::read_to_string(manifest_args)?;
    let logical_out_dir = logical_out_dir_from_cargo_manifest_args(&args);
    let include_out_dir = logical_out_dir.as_deref().unwrap_or(out_dir);
    for (module, sentinel) in [
        ("prelude", "prelude.bzl"),
        ("bazel_tools", "tools/cpp/toolchain_utils.bzl"),
    ] {
        let files = args.lines().skip(2).filter_map(|line| {
            let line = line.trim_matches('\'');
            let (runfile_path, contents_path) = line.split_once('=')?;
            let path = runfile_path
                .strip_prefix(&format!("cells/{module}/"))
                .or_else(|| runfile_path.strip_prefix(&format!("{module}/")))?;
            let contents_path = runfiles_root.join(contents_path);
            let contents_include_path = if use_buck_generated_out_dir_include_paths(out_dir) {
                syntactic_include_path_from_out_dir(include_out_dir, runfile_path)
            } else {
                include_path_from_out_dir_to_existing_path(include_out_dir, &contents_path)
                    .unwrap_or_else(|| include_path_from_out_dir(include_out_dir, runfile_path))
            };
            Some((path.to_owned(), contents_include_path, contents_path))
        });

        write_include_module_from_collected(&mut include_file, module, sentinel, files)?;
    }

    Ok(())
}

fn logical_out_dir_from_cargo_manifest_args(args: &str) -> Option<PathBuf> {
    let runfiles_dir = Path::new(args.lines().next()?.trim_matches('\''));
    Some(runfiles_dir.parent()?.join("build_script.out_dir"))
}

fn include_path_from_out_dir(out_dir: &Path, runfile_path: &str) -> PathBuf {
    if let Some(contents_path) = find_existing_runfile_path(runfile_path)
        && let Some(path) = include_path_from_out_dir_to_existing_path(out_dir, &contents_path)
    {
        return path;
    }

    syntactic_include_path_from_out_dir(out_dir, runfile_path)
}

fn syntactic_include_path_from_out_dir(out_dir: &Path, runfile_path: &str) -> PathBuf {
    let cwd = std::env::current_dir().ok();
    syntactic_include_path_from_out_dir_for_cwd(cwd.as_deref(), out_dir, runfile_path)
}

fn syntactic_include_path_from_out_dir_for_cwd(
    cwd: Option<&Path>,
    out_dir: &Path,
    runfile_path: &str,
) -> PathBuf {
    let execroot_to_workspace_parent_count =
        cwd.and_then(parent_count_from_buck_execroot_to_workspace);
    let out_dir = cwd
        .and_then(|cwd| out_dir.strip_prefix(cwd).ok())
        .unwrap_or(out_dir);

    let mut path = PathBuf::new();
    let mut parent_count = out_dir.components().count();
    if let Some(execroot_to_workspace_parent_count) = execroot_to_workspace_parent_count {
        parent_count += execroot_to_workspace_parent_count;
    } else if out_dir_str_contains(out_dir, "__bazel_execroot") {
        // Buck exposes OUT_DIR through an execroot symlink. Filesystem resolution applies `..`
        // after following that symlink, so the syntactic fallback needs fewer parents than the
        // execroot spelling itself.
        parent_count = parent_count.saturating_sub(2);
    } else if path_contains_buck_out_bin(out_dir) {
        // `buck-out/bin/<config>` is a symlink to `buck-out/v2/art/<cell>/<config>`, which is
        // two components deeper when `include_bytes!` resolves `..` through the symlink.
        parent_count += 2;
    }
    for _ in 0..parent_count {
        path.push("..");
    }
    path.push(runfile_path);
    path
}

fn parent_count_from_buck_execroot_to_workspace(path: &Path) -> Option<usize> {
    let components = normal_components(path);
    let execroot_index = components
        .iter()
        .position(|component| *component == "__bazel_execroot")?;
    let buck_out_index = components[..execroot_index]
        .iter()
        .rposition(|component| *component == "buck-out")?;

    Some(components.len() - buck_out_index)
}

fn out_dir_str_contains(out_dir: &Path, needle: &str) -> bool {
    out_dir.to_str().is_some_and(|path| path.contains(needle))
}

fn normal_components(path: &Path) -> Vec<&str> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(component) => component.to_str(),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>()
}

fn path_contains_buck_out_bin(path: &Path) -> bool {
    let components = normal_components(path);
    components
        .windows(2)
        .any(|window| window == ["buck-out", "bin"])
}

fn use_buck_generated_out_dir_include_paths(out_dir: &Path) -> bool {
    out_dir.to_str().is_some_and(|path| {
        path.starts_with("buck-out/")
            || path.contains("/buck-out/")
            || path.contains("__bazel_execroot")
            || path.contains("__build_script__")
    })
}

fn find_existing_runfile_path(runfile_path: &str) -> Option<PathBuf> {
    for base in [
        std::env::current_dir().ok(),
        std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from),
        std::env::current_exe().ok(),
    ]
    .into_iter()
    .flatten()
    {
        for ancestor in base.ancestors() {
            let path = ancestor.join(runfile_path);
            if path.exists() {
                return Some(path);
            }
        }
    }

    None
}

fn include_path_from_out_dir_to_existing_path(
    out_dir: &Path,
    contents_path: &Path,
) -> Option<PathBuf> {
    // `OUT_DIR` may be a symlink planted in an execroot. Paths inside `include_bytes!` are
    // resolved by the filesystem through that symlink, so compute from canonical paths.
    let out_dir = out_dir.canonicalize().ok()?;
    let contents_path = contents_path.canonicalize().ok()?;
    Some(relative_path(&out_dir, &contents_path))
}

fn relative_path(from_dir: &Path, to: &Path) -> PathBuf {
    let from_components = from_dir.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();

    let mut common = 0;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }

    if common == 0 {
        return to.to_owned();
    }

    let mut path = PathBuf::new();
    for component in &from_components[common..] {
        match component {
            Component::Normal(_) => path.push(".."),
            Component::CurDir => {}
            Component::ParentDir => path.push(".."),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    for component in &to_components[common..] {
        path.push(component.as_os_str());
    }
    path
}

fn write_include_header(mut include_file: impl io::Write) -> io::Result<()> {
    writeln!(include_file, "// {}generated by crate build.rs", "@")
}

fn write_include_module_header(mut include_file: impl io::Write, module: &str) -> io::Result<()> {
    writeln!(
        include_file,
        "pub(crate) mod {module} {{\n  pub(crate) const DATA: &[crate::BundledFile] = &["
    )
}

fn write_include_entry(
    mut include_file: impl io::Write,
    path: &str,
    contents_path: &Path,
    metadata_path: &Path,
) -> io::Result<()> {
    writeln!(include_file, "crate::BundledFile {{")?;
    writeln!(include_file, "  path: r\"{}\",", path)?;
    writeln!(
        include_file,
        "  contents: include_bytes!(r\"{}\"),",
        contents_path.display()
    )?;

    let exec_bit;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        exec_bit = metadata_path.metadata()?.mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    {
        exec_bit = false;
    }

    writeln!(include_file, "  is_executable: {exec_bit},")?;
    writeln!(include_file, "}},")
}

fn write_include_module_footer(mut include_file: impl io::Write) -> io::Result<()> {
    writeln!(include_file, "  ];\n}}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use crate::parent_count_from_buck_execroot_to_workspace;
    use crate::syntactic_include_path_from_out_dir_for_cwd;

    fn parents(count: usize, suffix: &str) -> PathBuf {
        let mut path = PathBuf::new();
        for _ in 0..count {
            path.push("..");
        }
        path.push(suffix);
        path
    }

    #[test]
    fn buck_execroot_current_dir_counts_parents_back_to_workspace() {
        let cwd = Path::new("/repo/buck-out/v2/__bazel_execroot/09afd8b101deb8aa");
        assert_eq!(parent_count_from_buck_execroot_to_workspace(cwd), Some(4));
    }

    #[test]
    fn include_path_from_buck_bazel_execroot_points_to_workspace_source() {
        let cwd = Path::new("/repo/buck-out/v2/__bazel_execroot/09afd8b101deb8aa");
        let out_dir = cwd.join(
            "buck-out/bin/ecbd565059a02aa3/app/bz_external_cells_bundled/build_script.out_dir",
        );

        assert_eq!(
            syntactic_include_path_from_out_dir_for_cwd(
                Some(cwd),
                &out_dir,
                "cells/bazel_tools/tools/test/generate-xml.sh",
            ),
            parents(10, "cells/bazel_tools/tools/test/generate-xml.sh"),
        );
    }
}
