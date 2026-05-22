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
use std::io;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use buck2_error::BuckErrorContext;
use buck2_error::buck2_error;
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use tar::Archive;
use xz2::read::XzDecoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    Tar,
    TarGz,
    Gz,
    TarXz,
    Xz,
    TarBz2,
    Bz2,
    TarZst,
    Zst,
}

pub fn archive_kind_from_type_or_url(archive_type: Option<&str>, url: &str) -> Option<ArchiveKind> {
    if let Some(archive_type) = archive_type
        .map(|archive_type| archive_type.trim_start_matches('.').to_ascii_lowercase())
        .filter(|archive_type| !archive_type.is_empty())
    {
        return archive_kind_from_extension(&archive_type);
    }

    let url = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    for extension in [
        "tar.gz", "tgz", "tar.xz", "txz", "tar.bz2", "tbz", "tar.zst", "tzst", "zip", "jar", "war",
        "aar", "nupkg", "whl", "tar", "gz", "xz", "bz2", "zst",
    ] {
        if url.ends_with(&format!(".{extension}")) {
            return archive_kind_from_extension(extension);
        }
    }
    None
}

fn archive_kind_from_extension(extension: &str) -> Option<ArchiveKind> {
    match extension {
        "zip" | "jar" | "war" | "aar" | "nupkg" | "whl" => Some(ArchiveKind::Zip),
        "tar" => Some(ArchiveKind::Tar),
        "tar.gz" | "tgz" => Some(ArchiveKind::TarGz),
        "gz" => Some(ArchiveKind::Gz),
        "tar.xz" | "txz" => Some(ArchiveKind::TarXz),
        "xz" => Some(ArchiveKind::Xz),
        "tar.bz2" | "tbz" => Some(ArchiveKind::TarBz2),
        "bz2" => Some(ArchiveKind::Bz2),
        "tar.zst" | "tzst" => Some(ArchiveKind::TarZst),
        "zst" => Some(ArchiveKind::Zst),
        _ => None,
    }
}

pub fn extract_archive(
    archive: &Path,
    output: &Path,
    kind: ArchiveKind,
    strip_prefix: &str,
    strip_components: u32,
    rename_files: &[(String, String)],
) -> buck2_error::Result<()> {
    if strip_components > 0 && !strip_prefix.is_empty() {
        return Err(buck2_error!(
            buck2_error::ErrorTag::Input,
            "Only one of strip_prefix or strip_components can be set"
        ));
    }
    fs::create_dir_all(output)
        .with_buck_error_context(|| format!("Error creating `{}`", output.display()))?;
    match kind {
        ArchiveKind::Zip => extract_zip_archive(
            archive,
            output,
            strip_prefix,
            strip_components,
            rename_files,
        ),
        ArchiveKind::Tar => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            extract_tar_archive(reader, output, strip_prefix, strip_components, rename_files)
        }
        ArchiveKind::TarGz => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            extract_tar_archive(
                GzDecoder::new(reader),
                output,
                strip_prefix,
                strip_components,
                rename_files,
            )
        }
        ArchiveKind::Gz => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            extract_compressed_file(GzDecoder::new(reader), archive, output, "gz", rename_files)
        }
        ArchiveKind::TarXz => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            extract_tar_archive(
                XzDecoder::new(reader),
                output,
                strip_prefix,
                strip_components,
                rename_files,
            )
        }
        ArchiveKind::Xz => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            extract_compressed_file(XzDecoder::new(reader), archive, output, "xz", rename_files)
        }
        ArchiveKind::TarBz2 => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            extract_tar_archive(
                BzDecoder::new(reader),
                output,
                strip_prefix,
                strip_components,
                rename_files,
            )
        }
        ArchiveKind::Bz2 => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            extract_compressed_file(BzDecoder::new(reader), archive, output, "bz2", rename_files)
        }
        ArchiveKind::TarZst => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            let reader = zstd::stream::read::Decoder::new(reader)
                .buck_error_context("Error initializing zstd archive decoder")?;
            extract_tar_archive(reader, output, strip_prefix, strip_components, rename_files)
        }
        ArchiveKind::Zst => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            let reader = zstd::stream::read::Decoder::new(reader)
                .buck_error_context("Error initializing zstd archive decoder")?;
            extract_compressed_file(reader, archive, output, "zst", rename_files)
        }
    }
}

fn extract_compressed_file<R: Read>(
    mut reader: R,
    archive: &Path,
    output: &Path,
    extension: &str,
    rename_files: &[(String, String)],
) -> buck2_error::Result<()> {
    let entry_name = compressed_file_entry_name(archive, extension)?;
    let entry_name = renamed_archive_entry_name(&entry_name, rename_files);
    let components = safe_archive_components(&entry_name)?;
    if components.is_empty() {
        return Ok(());
    }
    let destination = output.join(components.into_iter().collect::<PathBuf>());
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_buck_error_context(|| format!("Error creating `{}`", parent.display()))?;
    }
    let mut file = fs::File::create(&destination)
        .with_buck_error_context(|| format!("Error creating `{}`", destination.display()))?;
    io::copy(&mut reader, &mut file)
        .with_buck_error_context(|| format!("Error writing `{}`", destination.display()))?;
    Ok(())
}

fn compressed_file_entry_name(archive: &Path, extension: &str) -> buck2_error::Result<String> {
    let file_name = archive
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .ok_or_else(|| {
            buck2_error!(
                buck2_error::ErrorTag::Input,
                "Compressed archive path `{}` has no UTF-8 file name",
                archive.display()
            )
        })?;
    Ok(file_name
        .strip_suffix(&format!(".{extension}"))
        .unwrap_or(file_name)
        .to_owned())
}

fn extract_zip_archive(
    archive: &Path,
    output: &Path,
    strip_prefix: &str,
    strip_components: u32,
    rename_files: &[(String, String)],
) -> buck2_error::Result<()> {
    let reader = fs::File::open(archive)
        .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
    let mut zip = zip::ZipArchive::new(reader).map_err(|error| {
        buck2_error!(
            buck2_error::ErrorTag::Input,
            "Error reading zip archive `{}`: {}",
            archive.display(),
            error
        )
    })?;
    let mut found_prefix = strip_prefix.is_empty();
    let mut available_prefixes = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| {
            buck2_error!(
                buck2_error::ErrorTag::Input,
                "Error reading zip entry {} from `{}`: {}",
                index,
                archive.display(),
                error
            )
        })?;
        let entry_name = renamed_archive_entry_name(entry.name(), rename_files);
        let Some(relative_path) = prepare_archive_entry_path(
            &entry_name,
            strip_prefix,
            strip_components,
            &mut found_prefix,
            &mut available_prefixes,
        )?
        else {
            continue;
        };
        let destination = output.join(relative_path);
        if entry.is_dir() {
            fs::create_dir_all(&destination).with_buck_error_context(|| {
                format!("Error creating `{}`", destination.display())
            })?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_buck_error_context(|| format!("Error creating `{}`", parent.display()))?;
        }
        if is_zip_symlink(entry.unix_mode()) {
            let mut target = String::new();
            entry
                .read_to_string(&mut target)
                .with_buck_error_context(|| {
                    format!("Error reading symlink target from `{}`", entry.name())
                })?;
            create_symlink(Path::new(&target), &destination)?;
            continue;
        }
        let mut file = fs::File::create(&destination)
            .with_buck_error_context(|| format!("Error creating `{}`", destination.display()))?;
        io::copy(&mut entry, &mut file)
            .with_buck_error_context(|| format!("Error writing `{}`", destination.display()))?;
        if let Some(mode) = entry.unix_mode() {
            set_extracted_file_mode(&destination, mode)?;
        }
    }
    ensure_strip_prefix_found(strip_prefix, found_prefix, available_prefixes)
}

fn extract_tar_archive<R: Read>(
    reader: R,
    output: &Path,
    strip_prefix: &str,
    strip_components: u32,
    rename_files: &[(String, String)],
) -> buck2_error::Result<()> {
    let mut archive = Archive::new(reader);
    archive.set_preserve_permissions(true);
    let mut found_prefix = strip_prefix.is_empty();
    let mut available_prefixes = Vec::new();
    for entry in archive
        .entries()
        .buck_error_context("Error reading tar archive")?
    {
        let mut entry = entry.buck_error_context("Error reading tar archive entry")?;
        let path = entry
            .path()
            .buck_error_context("Error reading tar archive entry path")?;
        let entry_name = renamed_archive_entry_name(&path.to_string_lossy(), rename_files);
        let Some(relative_path) = prepare_archive_entry_path(
            &entry_name,
            strip_prefix,
            strip_components,
            &mut found_prefix,
            &mut available_prefixes,
        )?
        else {
            continue;
        };
        let destination = output.join(relative_path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_buck_error_context(|| format!("Error creating `{}`", parent.display()))?;
        }
        entry
            .unpack(&destination)
            .with_buck_error_context(|| format!("Error extracting `{}`", destination.display()))?;
    }
    ensure_strip_prefix_found(strip_prefix, found_prefix, available_prefixes)
}

fn renamed_archive_entry_name(entry_name: &str, rename_files: &[(String, String)]) -> String {
    rename_files
        .iter()
        .find_map(|(from, to)| (from == entry_name).then(|| to.clone()))
        .unwrap_or_else(|| entry_name.to_owned())
}

fn prepare_archive_entry_path(
    entry_name: &str,
    strip_prefix: &str,
    strip_components: u32,
    found_prefix: &mut bool,
    available_prefixes: &mut Vec<String>,
) -> buck2_error::Result<Option<PathBuf>> {
    let Some(entry_name) = strip_archive_prefix(entry_name, strip_prefix, found_prefix) else {
        if !strip_prefix.is_empty()
            && let Some(prefix) = first_archive_path_component(entry_name)
            && !available_prefixes.contains(&prefix)
        {
            available_prefixes.push(prefix);
        }
        return Ok(None);
    };
    let components = safe_archive_components(entry_name)?;
    let components = components
        .into_iter()
        .skip(strip_components as usize)
        .collect::<Vec<_>>();
    if components.is_empty() {
        return Ok(None);
    }
    Ok(Some(components.into_iter().collect()))
}

fn strip_archive_prefix<'a>(
    entry_name: &'a str,
    strip_prefix: &str,
    found_prefix: &mut bool,
) -> Option<&'a str> {
    let strip_prefix = strip_prefix.trim_matches('/');
    if strip_prefix.is_empty() {
        return Some(entry_name);
    }
    if entry_name == strip_prefix {
        *found_prefix = true;
        return Some("");
    }
    let prefix_with_slash = format!("{strip_prefix}/");
    entry_name.strip_prefix(&prefix_with_slash).inspect(|_| {
        *found_prefix = true;
    })
}

fn safe_archive_components(path: &str) -> buck2_error::Result<Vec<PathBuf>> {
    let mut components = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(component) => components.push(PathBuf::from(component)),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Archive entry `{}` escapes the extraction directory",
                    path
                ));
            }
        }
    }
    Ok(components)
}

fn first_archive_path_component(path: &str) -> Option<String> {
    safe_archive_components(path)
        .ok()
        .and_then(|components| components.into_iter().next())
        .map(|component| component.to_string_lossy().into_owned())
}

fn ensure_strip_prefix_found(
    strip_prefix: &str,
    found_prefix: bool,
    mut available_prefixes: Vec<String>,
) -> buck2_error::Result<()> {
    if strip_prefix.is_empty() || found_prefix {
        return Ok(());
    }
    available_prefixes.sort();
    Err(buck2_error!(
        buck2_error::ErrorTag::Input,
        "Prefix `{}` was given, but not found in the archive. Available prefixes: {}",
        strip_prefix,
        available_prefixes.join(", ")
    ))
}

fn is_zip_symlink(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & 0o170000 == 0o120000)
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path) -> buck2_error::Result<()> {
    std::os::unix::fs::symlink(target, destination).with_buck_error_context(|| {
        format!(
            "Error creating symlink `{}` -> `{}`",
            destination.display(),
            target.display()
        )
    })
}

#[cfg(not(unix))]
fn create_symlink(target: &Path, destination: &Path) -> buck2_error::Result<()> {
    fs::write(destination, target.to_string_lossy().as_bytes()).with_buck_error_context(|| {
        format!(
            "Error writing symlink placeholder `{}` -> `{}`",
            destination.display(),
            target.display()
        )
    })
}

#[cfg(unix)]
fn set_extracted_file_mode(path: &Path, mode: u32) -> buck2_error::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
        .with_buck_error_context(|| format!("Error setting permissions on `{}`", path.display()))
}

#[cfg(not(unix))]
fn set_extracted_file_mode(_path: &Path, _mode: u32) -> buck2_error::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::Builder;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::ArchiveKind;
    use super::archive_kind_from_type_or_url;
    use super::extract_archive;

    #[test]
    fn test_archive_kind_from_url_matches_bazel_suffixes() {
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.whl"),
            Some(ArchiveKind::Zip)
        );
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.tar.gz"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.gz"),
            Some(ArchiveKind::Gz)
        );
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.xz"),
            Some(ArchiveKind::Xz)
        );
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.bz2"),
            Some(ArchiveKind::Bz2)
        );
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.zst"),
            Some(ArchiveKind::Zst)
        );
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.zip?download=1"),
            Some(ArchiveKind::Zip)
        );
        assert_eq!(
            archive_kind_from_type_or_url(Some("tar.bz2"), "https://example.com/a"),
            Some(ArchiveKind::TarBz2)
        );
    }

    #[test]
    fn test_extract_zip_strips_prefix_and_renames() -> buck2_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.zip");
        let file = fs::File::create(&archive).unwrap();
        let mut zip = ZipWriter::new(file);
        zip.start_file("pkg/old.txt", SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, b"content").unwrap();
        zip.finish().unwrap();

        let output = dir.path().join("out");
        extract_archive(
            &archive,
            &output,
            ArchiveKind::Zip,
            "pkg",
            0,
            &[("pkg/old.txt".to_owned(), "pkg/new.txt".to_owned())],
        )?;

        assert_eq!(
            fs::read_to_string(output.join("new.txt")).unwrap(),
            "content"
        );
        Ok(())
    }

    #[test]
    fn test_extract_tar_gz_strips_components() -> buck2_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.tar.gz");
        let file = fs::File::create(&archive).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut tar = Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(7);
        header.set_cksum();
        tar.append_data(&mut header, "pkg/file.txt", "content".as_bytes())
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let output = dir.path().join("out");
        extract_archive(&archive, &output, ArchiveKind::TarGz, "", 1, &[])?;

        assert_eq!(
            fs::read_to_string(output.join("file.txt")).unwrap(),
            "content"
        );
        Ok(())
    }

    #[test]
    fn test_extract_gz_writes_single_file() -> buck2_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.txt.gz");
        let file = fs::File::create(&archive).unwrap();
        let mut encoder = GzEncoder::new(file, Compression::default());
        std::io::Write::write_all(&mut encoder, b"content").unwrap();
        encoder.finish().unwrap();

        let output = dir.path().join("out");
        extract_archive(
            &archive,
            &output,
            ArchiveKind::Gz,
            "ignored",
            1,
            &[("source.txt".to_owned(), "renamed.txt".to_owned())],
        )?;

        assert_eq!(
            fs::read_to_string(output.join("renamed.txt")).unwrap(),
            "content"
        );
        Ok(())
    }
}
