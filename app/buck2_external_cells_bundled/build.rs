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
    let cargo_prelude_path = Path::new(&manifest_path)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("prelude");
    let cwd_prelude_path = std::env::current_dir()?.join("prelude");
    let prelude_path = if cwd_prelude_path.join("prelude.bzl").exists() {
        cwd_prelude_path
    } else {
        cargo_prelude_path
    };

    // Self-check.
    assert!(prelude_path.join("prelude.bzl").exists());

    println!("cargo:rerun-if-changed={}", prelude_path.display());

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
        write_include_file(&prelude_path, include_file)?;
    }

    Ok(())
}

fn as_unix_like(path: &Path) -> String {
    path.to_str().unwrap().replace('\\', "/")
}

fn write_include_file(prelude: &Path, mut include_file: impl io::Write) -> io::Result<()> {
    write_include_header(&mut include_file)?;

    let mut files = Vec::new();
    for res in walkdir::WalkDir::new(prelude) {
        let entry = res.map_err(|e| e.into_io_error().unwrap())?;
        if !entry.file_type().is_file() {
            continue;
        }

        files.push((
            as_unix_like(entry.path().strip_prefix(prelude).unwrap()),
            entry.path().to_owned(),
        ));
    }
    files.sort_by(|(a, _), (b, _)| a.cmp(b));

    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "prelude directory `{}` did not contain any files; current directory is `{}`",
                prelude.display(),
                std::env::current_dir()?.display()
            ),
        ));
    }

    for (path, contents_path) in files {
        write_include_entry(&mut include_file, &path, &contents_path, &contents_path)?;
    }

    write_include_footer(&mut include_file)
}

fn write_include_file_from_runfiles_manifest(
    manifest: &Path,
    mut include_file: impl io::Write,
) -> io::Result<()> {
    write_include_header(&mut include_file)?;

    let mut files = Vec::new();
    for line in std::fs::read_to_string(manifest)?.lines() {
        let Some((runfile_path, contents_path)) = line.split_once(' ') else {
            continue;
        };
        let Some(path) = runfile_path
            .strip_prefix("_main/prelude/")
            .or_else(|| runfile_path.strip_prefix("prelude/"))
        else {
            continue;
        };
        files.push((path.to_owned(), PathBuf::from(contents_path)));
    }
    files.sort_by(|(a, _), (b, _)| a.cmp(b));

    if !files.iter().any(|(path, _)| path == "prelude.bzl") {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "runfiles manifest `{}` did not contain the bundled prelude",
                manifest.display()
            ),
        ));
    }

    for (path, contents_path) in files {
        write_include_entry(&mut include_file, &path, &contents_path, &contents_path)?;
    }

    write_include_footer(&mut include_file)
}

fn write_include_file_from_cargo_manifest_args(
    manifest_args: &Path,
    runfiles_root: &Path,
    out_dir: &Path,
    mut include_file: impl io::Write,
) -> io::Result<()> {
    write_include_header(&mut include_file)?;

    let mut files = Vec::new();
    for line in std::fs::read_to_string(manifest_args)?.lines().skip(2) {
        let line = line.trim_matches('\'');
        let Some((runfile_path, contents_path)) = line.split_once('=') else {
            continue;
        };
        let Some(path) = runfile_path.strip_prefix("prelude/") else {
            continue;
        };
        files.push((
            path.to_owned(),
            include_path_from_out_dir(out_dir, runfile_path),
            runfiles_root.join(contents_path),
        ));
    }
    files.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));

    if !files.iter().any(|(path, _, _)| path == "prelude.bzl") {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "cargo manifest args `{}` did not contain the bundled prelude",
                manifest_args.display()
            ),
        ));
    }

    for (path, contents_path, metadata_path) in files {
        write_include_entry(&mut include_file, &path, &contents_path, &metadata_path)?;
    }

    write_include_footer(&mut include_file)
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
    #[allow(clippy::write_literal)]
    writeln!(include_file, "// {}generated by crate build.rs", "@")?;

    writeln!(
        include_file,
        "pub(crate) const DATA: &[crate::BundledFile] = &["
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

fn write_include_footer(mut include_file: impl io::Write) -> io::Result<()> {
    writeln!(include_file, "];")?;
    Ok(())
}
