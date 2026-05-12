/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Generate source file containing buck2/prelude tree with contents.

use std::collections::BTreeSet;
use std::io;
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
    let prelude_path = source_tree_path(repo_root, "prelude", "prelude.bzl")?;
    let bazel_tools_path =
        source_tree_path(repo_root, "bazel_tools", "tools/cpp/toolchain_utils.bzl")?;

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
                .strip_prefix(&format!("_main/{module}/"))
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
    for (module, sentinel) in [
        ("prelude", "prelude.bzl"),
        ("bazel_tools", "tools/cpp/toolchain_utils.bzl"),
    ] {
        let files = args.lines().skip(2).filter_map(|line| {
            let line = line.trim_matches('\'');
            let (runfile_path, contents_path) = line.split_once('=')?;
            let path = runfile_path.strip_prefix(&format!("{module}/"))?;
            Some((
                path.to_owned(),
                include_path_from_out_dir(out_dir, runfile_path),
                runfiles_root.join(contents_path),
            ))
        });

        write_include_module_from_collected(&mut include_file, module, sentinel, files)?;
    }

    Ok(())
}

fn include_path_from_out_dir(out_dir: &Path, runfile_path: &str) -> PathBuf {
    let cwd = std::env::current_dir().ok();
    let out_dir = cwd
        .as_deref()
        .and_then(|cwd| out_dir.strip_prefix(cwd).ok())
        .unwrap_or(out_dir);

    let mut path = PathBuf::new();
    for _ in out_dir.components() {
        path.push("..");
    }
    path.push(runfile_path);
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
