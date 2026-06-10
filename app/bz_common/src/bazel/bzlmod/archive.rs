use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hash;
use std::hash::Hasher;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::str;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use bz_error::BuckErrorContext;
use bz_error::bz_error;
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use object::read::archive::ArchiveFile;
use tar::Archive;
use xz2::read::XzDecoder;

const ARCHIVE_BUFFER_SIZE: usize = 32 * 1024;

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
    Ar,
    SevenZ,
    TarBr,
    Br,
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
        "tar.gz", "tgz", "tar.xz", "txz", "tar.bz2", "tbz", "tar.zst", "tzst", "tar.br", "zip",
        "jar", "war", "aar", "nupkg", "whl", "tar", "gz", "xz", "bz2", "zst", "ar", "deb", "7z",
        "br",
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
        "ar" | "deb" => Some(ArchiveKind::Ar),
        "7z" => Some(ArchiveKind::SevenZ),
        "tar.br" => Some(ArchiveKind::TarBr),
        "br" => Some(ArchiveKind::Br),
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
) -> bz_error::Result<()> {
    if strip_components > 0 && !strip_prefix.is_empty() {
        return Err(bz_error!(
            bz_error::ErrorTag::Input,
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
        ArchiveKind::Ar => extract_ar_archive(archive, output, rename_files),
        ArchiveKind::SevenZ => extract_sevenz_archive(
            archive,
            output,
            strip_prefix,
            strip_components,
            rename_files,
        ),
        ArchiveKind::TarBr => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            let reader = brotli::Decompressor::new(reader, ARCHIVE_BUFFER_SIZE);
            extract_tar_archive(reader, output, strip_prefix, strip_components, rename_files)
        }
        ArchiveKind::Br => {
            let reader = fs::File::open(archive)
                .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
            let reader = brotli::Decompressor::new(reader, ARCHIVE_BUFFER_SIZE);
            extract_compressed_file(reader, archive, output, "br", rename_files)
        }
    }
}

fn extract_ar_archive(
    archive: &Path,
    output: &Path,
    rename_files: &[(String, String)],
) -> bz_error::Result<()> {
    let data = fs::read(archive)
        .with_buck_error_context(|| format!("Error reading `{}`", archive.display()))?;
    let ar = ArchiveFile::parse(data.as_slice()).map_err(|error| {
        bz_error!(
            bz_error::ErrorTag::Input,
            "Error reading ar archive `{}`: {}",
            archive.display(),
            error
        )
    })?;
    for member in ar.members() {
        let member = member.map_err(|error| {
            bz_error!(
                bz_error::ErrorTag::Input,
                "Error reading ar archive member from `{}`: {}",
                archive.display(),
                error
            )
        })?;
        let entry_name = str::from_utf8(member.name()).map_err(|error| {
            bz_error!(
                bz_error::ErrorTag::Input,
                "Ar archive `{}` has a non-UTF-8 member name: {}",
                archive.display(),
                error
            )
        })?;
        let entry_name = renamed_archive_entry_name(entry_name, rename_files);
        let components = safe_archive_components(&entry_name)?;
        if components.is_empty() {
            continue;
        }
        let destination = output.join(components.into_iter().collect::<PathBuf>());
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_buck_error_context(|| format!("Error creating `{}`", parent.display()))?;
        }
        fs::write(
            &destination,
            member.data(data.as_slice()).map_err(|error| {
                bz_error!(
                    bz_error::ErrorTag::Input,
                    "Error reading ar archive member `{}` from `{}`: {}",
                    entry_name,
                    archive.display(),
                    error
                )
            })?,
        )
        .with_buck_error_context(|| format!("Error writing `{}`", destination.display()))?;
        set_extracted_file_mode(
            &destination,
            (member.mode().unwrap_or(0o644) | 0o400) as u32,
        )?;
        if let Some(date) = member.date() {
            let time = filetime::FileTime::from_unix_time(date as i64, 0);
            filetime::set_file_mtime(&destination, time).with_buck_error_context(|| {
                format!(
                    "Error setting modification time on `{}`",
                    destination.display()
                )
            })?;
        }
    }
    Ok(())
}

fn extract_sevenz_archive(
    archive: &Path,
    output: &Path,
    strip_prefix: &str,
    strip_components: u32,
    rename_files: &[(String, String)],
) -> bz_error::Result<()> {
    let reader = fs::File::open(archive)
        .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
    let mut found_prefix = strip_prefix.is_empty();
    let mut available_prefixes = Vec::new();
    let mut extraction_error: Option<bz_error::Error> = None;
    let result = sevenz_rust::decompress_with_extract_fn(reader, output, |entry, reader, _dest| {
        if extraction_error.is_some() {
            return Err(sevenz_rust::Error::other(
                "previous 7z extraction error".to_owned(),
            ));
        }
        let entry_name = renamed_archive_entry_name(entry.name(), rename_files);
        let relative_path = match prepare_archive_entry_path(
            &entry_name,
            strip_prefix,
            strip_components,
            &mut found_prefix,
            &mut available_prefixes,
        ) {
            Ok(Some(relative_path)) => relative_path,
            Ok(None) => return Ok(true),
            Err(error) => {
                extraction_error = Some(error);
                return Err(sevenz_rust::Error::other(
                    "invalid 7z archive entry path".to_owned(),
                ));
            }
        };
        let destination = output.join(&relative_path);
        if entry.is_directory() {
            if let Err(error) = fs::create_dir_all(&destination) {
                extraction_error = Some(bz_error!(
                    bz_error::ErrorTag::IoSystem,
                    "Error creating `{}`: {}",
                    destination.display(),
                    error
                ));
                return Err(sevenz_rust::Error::other(
                    "error creating 7z output directory".to_owned(),
                ));
            }
            return Ok(true);
        }
        if let Some(parent) = destination.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            extraction_error = Some(bz_error!(
                bz_error::ErrorTag::IoSystem,
                "Error creating `{}`: {}",
                parent.display(),
                error
            ));
            return Err(sevenz_rust::Error::other(
                "error creating 7z output parent directory".to_owned(),
            ));
        }
        let mut file = match fs::File::create(&destination) {
            Ok(file) => file,
            Err(error) => {
                extraction_error = Some(bz_error!(
                    bz_error::ErrorTag::IoSystem,
                    "Error creating `{}`: {}",
                    destination.display(),
                    error
                ));
                return Err(sevenz_rust::Error::other(
                    "error creating 7z output file".to_owned(),
                ));
            }
        };
        if let Err(error) = io::copy(reader, &mut file) {
            extraction_error = Some(bz_error!(
                bz_error::ErrorTag::IoSystem,
                "Error writing `{}`: {}",
                destination.display(),
                error
            ));
            return Err(sevenz_rust::Error::other(
                "error writing 7z output file".to_owned(),
            ));
        }
        Ok(true)
    });
    match result {
        Ok(()) => ensure_strip_prefix_found(strip_prefix, found_prefix, available_prefixes),
        Err(error) => {
            if let Some(error) = extraction_error {
                return Err(error);
            }
            Err(bz_error!(
                bz_error::ErrorTag::Input,
                "Error reading 7z archive `{}`: {}",
                archive.display(),
                error
            ))
        }
    }
}

fn extract_compressed_file<R: Read>(
    mut reader: R,
    archive: &Path,
    output: &Path,
    extension: &str,
    rename_files: &[(String, String)],
) -> bz_error::Result<()> {
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

fn compressed_file_entry_name(archive: &Path, extension: &str) -> bz_error::Result<String> {
    let file_name = archive
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .ok_or_else(|| {
            bz_error!(
                bz_error::ErrorTag::Input,
                "Compressed archive path `{}` has no UTF-8 file name",
                archive.display()
            )
        })?;
    Ok(file_name
        .strip_suffix(&format!(".{extension}"))
        .unwrap_or(file_name)
        .to_owned())
}

/// Archive entries larger than this are written inline on the decode thread to
/// bound the memory held by queued writes.
const ARCHIVE_INLINE_WRITE_THRESHOLD: u64 = 8 * 1024 * 1024;
/// Total queued file writes across all writer threads.
const ARCHIVE_WRITE_QUEUE_CAPACITY: usize = 64;
const ARCHIVE_MAX_WRITER_THREADS: usize = 8;

#[derive(Debug)]
enum PendingArchiveFilePerms {
    /// Raw tar header mode; applied unmasked like `tar::Entry::unpack` does
    /// with `preserve_permissions` enabled.
    Tar(Option<u32>),
    /// Zip unix mode; applied via `set_extracted_file_mode`.
    Zip(Option<u32>),
}

#[derive(Debug)]
struct PendingArchiveFileWrite {
    destination: PathBuf,
    bytes: Vec<u8>,
    perms: PendingArchiveFilePerms,
    /// Tar header mtime; `None` skips setting times (zip entries).
    mtime: Option<u64>,
}

/// Routes buffered file writes to a pool of writer threads. The decode thread
/// creates parent directories before dispatching a write, so writers never
/// create directories. Writes to the same destination are routed to the same
/// thread so duplicate archive entries keep their last-entry-wins order.
struct ArchiveFileWriteQueue<'env> {
    senders: Vec<mpsc::SyncSender<PendingArchiveFileWrite>>,
    write_failed: &'env AtomicBool,
}

impl ArchiveFileWriteQueue<'_> {
    fn write_failed(&self) -> bool {
        self.write_failed.load(Ordering::Relaxed)
    }

    fn send(&self, write: PendingArchiveFileWrite) {
        let mut hasher = DefaultHasher::new();
        write.destination.hash(&mut hasher);
        let index = (hasher.finish() % self.senders.len() as u64) as usize;
        // Writers drain their queue even after a write failure, so a send only
        // fails if a writer thread panicked; that panic is propagated when the
        // writer scope joins.
        let _ = self.senders[index].send(write);
    }
}

/// Runs `decode` with a pool of writer threads consuming
/// `PendingArchiveFileWrite`s. The first writer error is propagated and takes
/// precedence over a decode error, since the decode loop stops early once
/// `write_failed` is observed.
fn with_archive_file_writers<T>(
    decode: impl FnOnce(&ArchiveFileWriteQueue) -> bz_error::Result<T>,
) -> bz_error::Result<T> {
    let writer_count = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(ARCHIVE_MAX_WRITER_THREADS);
    let queue_capacity = ARCHIVE_WRITE_QUEUE_CAPACITY.div_ceil(writer_count);
    let write_failed = AtomicBool::new(false);
    let first_write_error: Mutex<Option<bz_error::Error>> = Mutex::new(None);
    let result = std::thread::scope(|scope| {
        let mut senders = Vec::with_capacity(writer_count);
        for _ in 0..writer_count {
            let (sender, receiver) = mpsc::sync_channel(queue_capacity);
            senders.push(sender);
            let write_failed = &write_failed;
            let first_write_error = &first_write_error;
            scope.spawn(move || {
                while let Ok(write) = receiver.recv() {
                    // Keep draining after a failure so the decode thread never
                    // blocks on a full queue.
                    if write_failed.load(Ordering::Relaxed) {
                        continue;
                    }
                    if let Err(error) = write_pending_archive_file(&write) {
                        write_failed.store(true, Ordering::Relaxed);
                        first_write_error
                            .lock()
                            .expect("archive write error mutex poisoned")
                            .get_or_insert(error);
                    }
                }
            });
        }
        let queue = ArchiveFileWriteQueue {
            senders,
            write_failed: &write_failed,
        };
        let result = decode(&queue);
        drop(queue);
        result
    });
    if let Some(error) = first_write_error
        .into_inner()
        .expect("archive write error mutex poisoned")
    {
        return Err(error);
    }
    result
}

fn write_pending_archive_file(write: &PendingArchiveFileWrite) -> bz_error::Result<()> {
    let mut file = match write.perms {
        // Mirror `tar::Entry::unpack`: never write through an existing file or
        // symlink, replace it instead.
        PendingArchiveFilePerms::Tar(_) => create_replacing_extracted_file(&write.destination),
        PendingArchiveFilePerms::Zip(_) => fs::File::create(&write.destination),
    }
    .with_buck_error_context(|| format!("Error creating `{}`", write.destination.display()))?;
    file.write_all(&write.bytes)
        .with_buck_error_context(|| format!("Error writing `{}`", write.destination.display()))?;
    if let Some(mtime) = write.mtime {
        // Mirror `tar::Entry::unpack`: avoid emitting 0-mtime files.
        let mtime = if mtime == 0 { 1 } else { mtime };
        let time = filetime::FileTime::from_unix_time(mtime as i64, 0);
        filetime::set_file_handle_times(&file, Some(time), Some(time)).with_buck_error_context(
            || {
                format!(
                    "Error setting modification time on `{}`",
                    write.destination.display()
                )
            },
        )?;
    }
    match write.perms {
        PendingArchiveFilePerms::Tar(Some(mode)) => {
            set_extracted_tar_file_mode(&file, &write.destination, mode)?;
        }
        PendingArchiveFilePerms::Zip(Some(mode)) => {
            set_extracted_file_mode(&write.destination, mode)?;
        }
        PendingArchiveFilePerms::Tar(None) | PendingArchiveFilePerms::Zip(None) => {}
    }
    Ok(())
}

fn create_replacing_extracted_file(destination: &Path) -> io::Result<fs::File> {
    fn create_new(destination: &Path) -> io::Result<fs::File> {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
    }
    match create_new(destination) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match fs::remove_file(destination) {
                Ok(()) => create_new(destination),
                Err(error) if error.kind() == io::ErrorKind::NotFound => create_new(destination),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn set_extracted_tar_file_mode(
    file: &fs::File,
    destination: &Path,
    mode: u32,
) -> bz_error::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(mode))
        .with_buck_error_context(|| {
            format!("Error setting permissions on `{}`", destination.display())
        })
}

#[cfg(not(unix))]
fn set_extracted_tar_file_mode(
    _file: &fs::File,
    _destination: &Path,
    _mode: u32,
) -> bz_error::Result<()> {
    Ok(())
}

fn extract_zip_archive(
    archive: &Path,
    output: &Path,
    strip_prefix: &str,
    strip_components: u32,
    rename_files: &[(String, String)],
) -> bz_error::Result<()> {
    let reader = fs::File::open(archive)
        .with_buck_error_context(|| format!("Error opening `{}`", archive.display()))?;
    let mut zip = zip::ZipArchive::new(reader).map_err(|error| {
        bz_error!(
            bz_error::ErrorTag::Input,
            "Error reading zip archive `{}`: {}",
            archive.display(),
            error
        )
    })?;
    let mut found_prefix = strip_prefix.is_empty();
    let mut available_prefixes = Vec::new();
    let mut pending_links = Vec::new();
    with_archive_file_writers(|queue| {
        for index in 0..zip.len() {
            if queue.write_failed() {
                break;
            }
            let mut entry = zip.by_index(index).map_err(|error| {
                bz_error!(
                    bz_error::ErrorTag::Input,
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
            let destination = output.join(&relative_path);
            if entry.is_dir() {
                fs::create_dir_all(&destination).with_buck_error_context(|| {
                    format!("Error creating `{}`", destination.display())
                })?;
                continue;
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).with_buck_error_context(|| {
                    format!("Error creating `{}`", parent.display())
                })?;
            }
            if is_zip_symlink(entry.unix_mode()) {
                let mut target = String::new();
                entry
                    .read_to_string(&mut target)
                    .with_buck_error_context(|| {
                        format!("Error reading symlink target from `{}`", entry.name())
                    })?;
                let target = prepare_archive_symlink_target(
                    &relative_path,
                    Path::new(&target),
                    strip_prefix,
                )?;
                pending_links.push(PendingArchiveLink {
                    kind: PendingArchiveLinkKind::Symlink,
                    destination,
                    target,
                });
                continue;
            }
            if entry.size() <= ARCHIVE_INLINE_WRITE_THRESHOLD {
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut bytes).with_buck_error_context(|| {
                    format!("Error extracting `{}`", destination.display())
                })?;
                queue.send(PendingArchiveFileWrite {
                    destination,
                    bytes,
                    perms: PendingArchiveFilePerms::Zip(entry.unix_mode()),
                    mtime: None,
                });
                continue;
            }
            let mut file = fs::File::create(&destination).with_buck_error_context(|| {
                format!("Error creating `{}`", destination.display())
            })?;
            io::copy(&mut entry, &mut file)
                .with_buck_error_context(|| format!("Error writing `{}`", destination.display()))?;
            if let Some(mode) = entry.unix_mode() {
                set_extracted_file_mode(&destination, mode)?;
            }
        }
        Ok(())
    })?;
    ensure_strip_prefix_found(strip_prefix, found_prefix, available_prefixes)?;
    create_pending_archive_links(&pending_links)
}

fn extract_tar_archive<R: Read>(
    reader: R,
    output: &Path,
    strip_prefix: &str,
    strip_components: u32,
    rename_files: &[(String, String)],
) -> bz_error::Result<()> {
    let mut archive = Archive::new(reader);
    archive.set_preserve_permissions(true);
    let mut found_prefix = strip_prefix.is_empty();
    let mut available_prefixes = Vec::new();
    let mut pending_links = Vec::new();
    with_archive_file_writers(|queue| {
        for entry in archive
            .entries()
            .buck_error_context("Error reading tar archive")?
        {
            if queue.write_failed() {
                break;
            }
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
            let destination = output.join(&relative_path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).with_buck_error_context(|| {
                    format!("Error creating `{}`", parent.display())
                })?;
            }
            let entry_type = entry.header().entry_type();
            if entry_type.is_symlink() || entry_type.is_hard_link() {
                let target = entry
                    .link_name()
                    .with_buck_error_context(|| {
                        format!("Error reading link target from `{path:?}`")
                    })?
                    .ok_or_else(|| {
                        bz_error!(
                            bz_error::ErrorTag::Input,
                            "Archive link entry `{}` has no target",
                            entry_name
                        )
                    })?;
                if target.as_os_str().is_empty() {
                    return Err(bz_error!(
                        bz_error::ErrorTag::Input,
                        "Archive link entry `{}` has an empty target",
                        entry_name
                    ));
                }
                let (kind, target) = if entry_type.is_symlink() {
                    (
                        PendingArchiveLinkKind::Symlink,
                        prepare_archive_symlink_target(&relative_path, &target, strip_prefix)?,
                    )
                } else {
                    let target = prepare_archive_hardlink_target(
                        &target,
                        strip_prefix,
                        strip_components,
                        rename_files,
                    )?;
                    (PendingArchiveLinkKind::Hardlink, output.join(target))
                };
                pending_links.push(PendingArchiveLink {
                    kind,
                    destination,
                    target,
                });
                continue;
            }
            // Plain regular files are buffered and written on the writer pool.
            // Everything else (directories, sparse files, fifos, old-style
            // trailing-slash directories, oversized files) keeps the
            // sequential `unpack` path.
            if entry_type.is_file()
                && entry.size() <= ARCHIVE_INLINE_WRITE_THRESHOLD
                && !entry.path_bytes().ends_with(b"/")
            {
                let mode = entry.header().mode().ok();
                let mtime = entry.header().mtime().ok();
                let size = entry.size();
                let mut bytes = Vec::with_capacity(size as usize);
                entry.read_to_end(&mut bytes).with_buck_error_context(|| {
                    format!("Error extracting `{}`", destination.display())
                })?;
                if bytes.len() as u64 != size {
                    return Err(bz_error!(
                        bz_error::ErrorTag::Input,
                        "Archive entry `{}` is truncated",
                        entry_name
                    ));
                }
                queue.send(PendingArchiveFileWrite {
                    destination,
                    bytes,
                    perms: PendingArchiveFilePerms::Tar(mode),
                    mtime,
                });
                continue;
            }
            entry.unpack(&destination).with_buck_error_context(|| {
                format!("Error extracting `{}`", destination.display())
            })?;
        }
        Ok(())
    })?;
    ensure_strip_prefix_found(strip_prefix, found_prefix, available_prefixes)?;
    create_pending_archive_links(&pending_links)
}

#[derive(Debug)]
enum PendingArchiveLinkKind {
    Symlink,
    Hardlink,
}

#[derive(Debug)]
struct PendingArchiveLink {
    kind: PendingArchiveLinkKind,
    destination: PathBuf,
    target: PathBuf,
}

fn create_pending_archive_links(links: &[PendingArchiveLink]) -> bz_error::Result<()> {
    for link in links {
        if let Some(parent) = link.destination.parent() {
            fs::create_dir_all(parent)
                .with_buck_error_context(|| format!("Error creating `{}`", parent.display()))?;
        }
        match link.kind {
            PendingArchiveLinkKind::Symlink => {
                remove_existing_archive_link_destination(&link.destination)?;
                create_symlink(&link.target, &link.destination)?;
            }
            PendingArchiveLinkKind::Hardlink => {
                if link.destination == link.target {
                    continue;
                }
                remove_existing_archive_link_destination(&link.destination)?;
                create_hard_link(&link.target, &link.destination)?;
            }
        }
    }
    Ok(())
}

fn remove_existing_archive_link_destination(destination: &Path) -> bz_error::Result<()> {
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_buck_error_context(|| {
                format!(
                    "Error checking existing archive link destination `{}`",
                    destination.display()
                )
            });
        }
    };

    if metadata.is_dir() {
        fs::remove_dir(destination).with_buck_error_context(|| {
            format!(
                "Error removing existing archive link directory `{}`",
                destination.display()
            )
        })
    } else {
        fs::remove_file(destination).with_buck_error_context(|| {
            format!(
                "Error removing existing archive link file `{}`",
                destination.display()
            )
        })
    }
}

fn prepare_archive_symlink_target(
    destination_relative_path: &Path,
    target: &Path,
    strip_prefix: &str,
) -> bz_error::Result<PathBuf> {
    if target.as_os_str().is_empty() {
        return Err(invalid_archive_link_target(
            destination_relative_path,
            target,
        ));
    }

    if let Some(stripped_target) = strip_archive_link_prefix(target, strip_prefix) {
        let stripped_target = normalize_archive_relative_path(&stripped_target)?;
        let destination_parent = destination_relative_path.parent().unwrap_or(Path::new(""));
        let target = relative_archive_path(destination_parent, &stripped_target);
        validate_archive_symlink_target(destination_relative_path, &target)?;
        return Ok(target);
    }

    validate_archive_symlink_target(destination_relative_path, target)?;
    Ok(target.to_owned())
}

fn prepare_archive_hardlink_target(
    target: &Path,
    strip_prefix: &str,
    strip_components: u32,
    rename_files: &[(String, String)],
) -> bz_error::Result<PathBuf> {
    let target_name = renamed_archive_entry_name(&target.to_string_lossy(), rename_files);
    let mut found_prefix = strip_prefix.is_empty();
    let mut available_prefixes = Vec::new();
    prepare_archive_entry_path(
        &target_name,
        strip_prefix,
        strip_components,
        &mut found_prefix,
        &mut available_prefixes,
    )?
    .ok_or_else(|| {
        bz_error!(
            bz_error::ErrorTag::Input,
            "Archive hardlink target `{}` is outside the stripped extraction root",
            target.display()
        )
    })
}

fn strip_archive_link_prefix(target: &Path, strip_prefix: &str) -> Option<PathBuf> {
    let strip_prefix = strip_prefix.trim_matches('/');
    if strip_prefix.is_empty() {
        return None;
    }
    target
        .strip_prefix(Path::new(strip_prefix))
        .ok()
        .map(Path::to_owned)
}

fn validate_archive_symlink_target(
    destination_relative_path: &Path,
    target: &Path,
) -> bz_error::Result<()> {
    let destination_parent = destination_relative_path.parent().unwrap_or(Path::new(""));
    let mut normalized = normalize_archive_relative_path(destination_parent)?;
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid_archive_link_target(
                        destination_relative_path,
                        target,
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid_archive_link_target(
                    destination_relative_path,
                    target,
                ));
            }
        }
    }
    Ok(())
}

fn normalize_archive_relative_path(path: &Path) -> bz_error::Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(bz_error!(
                        bz_error::ErrorTag::Input,
                        "Archive path `{}` escapes the extraction directory",
                        path.display()
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(bz_error!(
                    bz_error::ErrorTag::Input,
                    "Archive path `{}` escapes the extraction directory",
                    path.display()
                ));
            }
        }
    }
    Ok(normalized)
}

fn relative_archive_path(from_dir: &Path, to: &Path) -> PathBuf {
    let from_components = archive_normal_components(from_dir);
    let to_components = archive_normal_components(to);
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();

    let mut result = PathBuf::new();
    for _ in common..from_components.len() {
        result.push("..");
    }
    for component in &to_components[common..] {
        result.push(component);
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

fn archive_normal_components(path: &Path) -> Vec<PathBuf> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(component) => Some(PathBuf::from(component)),
            Component::CurDir => None,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect()
}

fn invalid_archive_link_target(destination: &Path, target: &Path) -> bz_error::Error {
    bz_error!(
        bz_error::ErrorTag::Input,
        "Archive link `{}` -> `{}` escapes the extraction directory",
        destination.display(),
        target.display()
    )
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
) -> bz_error::Result<Option<PathBuf>> {
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

fn safe_archive_components(path: &str) -> bz_error::Result<Vec<PathBuf>> {
    let mut components = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(component) => components.push(PathBuf::from(component)),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(bz_error!(
                    bz_error::ErrorTag::Input,
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
) -> bz_error::Result<()> {
    if strip_prefix.is_empty() || found_prefix {
        return Ok(());
    }
    available_prefixes.sort();
    Err(bz_error!(
        bz_error::ErrorTag::Input,
        "Prefix `{}` was given, but not found in the archive. Available prefixes: {}",
        strip_prefix,
        available_prefixes.join(", ")
    ))
}

fn is_zip_symlink(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & 0o170000 == 0o120000)
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path) -> bz_error::Result<()> {
    std::os::unix::fs::symlink(target, destination).with_buck_error_context(|| {
        format!(
            "Error creating symlink `{}` -> `{}`",
            destination.display(),
            target.display()
        )
    })
}

#[cfg(not(unix))]
fn create_symlink(target: &Path, destination: &Path) -> bz_error::Result<()> {
    fs::write(destination, target.to_string_lossy().as_bytes()).with_buck_error_context(|| {
        format!(
            "Error writing symlink placeholder `{}` -> `{}`",
            destination.display(),
            target.display()
        )
    })
}

fn create_hard_link(target: &Path, destination: &Path) -> bz_error::Result<()> {
    fs::hard_link(target, destination).with_buck_error_context(|| {
        format!(
            "Error creating hardlink `{}` -> `{}`",
            destination.display(),
            target.display()
        )
    })
}

#[cfg(unix)]
fn set_extracted_file_mode(path: &Path, mode: u32) -> bz_error::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
        .with_buck_error_context(|| format!("Error setting permissions on `{}`", path.display()))
}

#[cfg(not(unix))]
fn set_extracted_file_mode(_path: &Path, _mode: u32) -> bz_error::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use brotli::CompressorWriter;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::Builder;
    use tar::EntryType;
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
            archive_kind_from_type_or_url(None, "https://example.com/a.tar.br"),
            Some(ArchiveKind::TarBr)
        );
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.br"),
            Some(ArchiveKind::Br)
        );
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.deb"),
            Some(ArchiveKind::Ar)
        );
        assert_eq!(
            archive_kind_from_type_or_url(None, "https://example.com/a.7z"),
            Some(ArchiveKind::SevenZ)
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
    fn test_extract_zip_strips_prefix_and_renames() -> bz_error::Result<()> {
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
    fn test_extract_tar_gz_strips_components() -> bz_error::Result<()> {
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
    fn test_extract_tar_br_strips_components() -> bz_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.tar.br");
        let file = fs::File::create(&archive).unwrap();
        let encoder = CompressorWriter::new(file, 4096, 5, 22);
        let mut tar = Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(7);
        header.set_cksum();
        tar.append_data(&mut header, "pkg/file.txt", "content".as_bytes())
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.into_inner();

        let output = dir.path().join("out");
        extract_archive(&archive, &output, ArchiveKind::TarBr, "", 1, &[])?;

        assert_eq!(
            fs::read_to_string(output.join("file.txt")).unwrap(),
            "content"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_zip_deprefixes_symlink_target() -> bz_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.zip");
        let file = fs::File::create(&archive).unwrap();
        let mut zip = ZipWriter::new(file);
        zip.start_file("pkg/file.txt", SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, b"content").unwrap();
        zip.start_file(
            "pkg/dir/link",
            SimpleFileOptions::default().unix_permissions(0o120777),
        )
        .unwrap();
        std::io::Write::write_all(&mut zip, b"pkg/file.txt").unwrap();
        zip.finish().unwrap();

        let output = dir.path().join("out");
        extract_archive(&archive, &output, ArchiveKind::Zip, "pkg", 0, &[])?;

        assert_eq!(
            fs::read_link(output.join("dir/link")).unwrap(),
            PathBuf::from("../file.txt")
        );
        assert_eq!(
            fs::read_to_string(output.join("dir/link")).unwrap(),
            "content"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_tar_replaces_existing_symlink() -> bz_error::Result<()> {
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
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        tar.append_link(&mut header, "pkg/dir/link.txt", "pkg/file.txt")
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let output = dir.path().join("out");
        extract_archive(&archive, &output, ArchiveKind::TarGz, "pkg", 0, &[])?;
        extract_archive(&archive, &output, ArchiveKind::TarGz, "pkg", 0, &[])?;

        assert_eq!(
            fs::read_link(output.join("dir/link.txt")).unwrap(),
            PathBuf::from("../file.txt")
        );
        assert_eq!(
            fs::read_to_string(output.join("dir/link.txt")).unwrap(),
            "content"
        );
        Ok(())
    }

    #[test]
    fn test_extract_gz_writes_single_file() -> bz_error::Result<()> {
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

    #[test]
    fn test_extract_sevenz_strips_components() -> bz_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        fs::create_dir_all(input.join("pkg")).unwrap();
        fs::write(input.join("pkg/file.txt"), "content").unwrap();
        let archive = dir.path().join("source.7z");
        sevenz_rust::compress_to_path(&input, &archive).unwrap();

        let output = dir.path().join("out");
        extract_archive(&archive, &output, ArchiveKind::SevenZ, "", 1, &[])?;

        assert_eq!(
            fs::read_to_string(output.join("file.txt")).unwrap(),
            "content"
        );
        Ok(())
    }

    #[test]
    fn test_extract_br_writes_single_file() -> bz_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.txt.br");
        let file = fs::File::create(&archive).unwrap();
        let mut encoder = CompressorWriter::new(file, 4096, 5, 22);
        std::io::Write::write_all(&mut encoder, b"content").unwrap();
        encoder.into_inner();

        let output = dir.path().join("out");
        extract_archive(
            &archive,
            &output,
            ArchiveKind::Br,
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

    #[test]
    fn test_extract_ar_writes_members_and_renames() -> bz_error::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.deb");
        let mut ar = b"!<arch>\n".to_vec();
        write_ar_entry(&mut ar, "old.txt", b"content", 0o100600, 42);
        fs::write(&archive, ar).unwrap();

        let output = dir.path().join("out");
        extract_archive(
            &archive,
            &output,
            ArchiveKind::Ar,
            "ignored",
            1,
            &[("old.txt".to_owned(), "renamed.txt".to_owned())],
        )?;

        assert_eq!(
            fs::read_to_string(output.join("renamed.txt")).unwrap(),
            "content"
        );
        Ok(())
    }

    fn write_ar_entry(archive: &mut Vec<u8>, name: &str, content: &[u8], mode: u32, modified: u64) {
        let name = format!("{name}/");
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8o}{:<10}`\n",
            name,
            modified,
            0,
            0,
            mode,
            content.len()
        );
        assert_eq!(header.len(), 60);
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(content);
        if content.len() % 2 != 0 {
            archive.push(b'\n');
        }
    }

    #[test]
    fn test_extract_zip_rejects_escaping_symlink_target() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.zip");
        let file = fs::File::create(&archive).unwrap();
        let mut zip = ZipWriter::new(file);
        zip.start_file(
            "pkg/dir/link",
            SimpleFileOptions::default().unix_permissions(0o120777),
        )
        .unwrap();
        std::io::Write::write_all(&mut zip, b"../../outside").unwrap();
        zip.finish().unwrap();

        let output = dir.path().join("out");
        let error = extract_archive(&archive, &output, ArchiveKind::Zip, "pkg", 0, &[])
            .expect_err("escaping symlink target should be rejected");
        assert!(format!("{error:#}").contains("escapes the extraction directory"));
    }

    #[test]
    fn test_extract_tar_deprefixes_hardlink_target() -> bz_error::Result<()> {
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
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Link);
        header.set_size(0);
        tar.append_link(&mut header, "pkg/link.txt", "pkg/file.txt")
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let output = dir.path().join("out");
        extract_archive(&archive, &output, ArchiveKind::TarGz, "pkg", 0, &[])?;

        assert_eq!(
            fs::read_to_string(output.join("link.txt")).unwrap(),
            "content"
        );
        Ok(())
    }

    #[test]
    fn test_extract_tar_replaces_existing_hardlink() -> bz_error::Result<()> {
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
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Link);
        header.set_size(0);
        tar.append_link(&mut header, "pkg/link.txt", "pkg/file.txt")
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let output = dir.path().join("out");
        extract_archive(&archive, &output, ArchiveKind::TarGz, "pkg", 0, &[])?;
        extract_archive(&archive, &output, ArchiveKind::TarGz, "pkg", 0, &[])?;

        assert_eq!(
            fs::read_to_string(output.join("link.txt")).unwrap(),
            "content"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_tar_parallel_writes_preserve_contents_modes_and_links() -> bz_error::Result<()>
    {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.tar.gz");
        let file = fs::File::create(&archive).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut tar = Builder::new(encoder);
        for index in 0..32 {
            let contents = format!("contents-{index}");
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(1_234_567);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("pkg/nested/dir{}/file{index}.txt", index % 4),
                contents.as_bytes(),
            )
            .unwrap();
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o755);
        header.set_mtime(1_234_567);
        header.set_cksum();
        tar.append_data(&mut header, "pkg/bin/tool", "run\n".as_bytes())
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        tar.append_link(&mut header, "pkg/bin/link", "pkg/nested/dir0/file0.txt")
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let output = dir.path().join("out");
        extract_archive(&archive, &output, ArchiveKind::TarGz, "pkg", 0, &[])?;

        for index in 0..32 {
            let path = output.join(format!("nested/dir{}/file{index}.txt", index % 4));
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                format!("contents-{index}")
            );
            let metadata = fs::metadata(&path).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
            assert_eq!(
                metadata.modified().unwrap(),
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_234_567)
            );
        }
        let tool = output.join("bin/tool");
        assert_eq!(fs::read_to_string(&tool).unwrap(), "run\n");
        assert_eq!(
            fs::metadata(&tool).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(output.join("bin/link")).unwrap(),
            PathBuf::from("../nested/dir0/file0.txt")
        );
        assert_eq!(
            fs::read_to_string(output.join("bin/link")).unwrap(),
            "contents-0"
        );
        Ok(())
    }

    #[test]
    fn test_extract_tar_surfaces_parallel_write_error() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.tar.gz");
        let file = fs::File::create(&archive).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut tar = Builder::new(encoder);
        for index in 0..16 {
            let contents = format!("contents-{index}");
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("pkg/file{index}.txt"),
                contents.as_bytes(),
            )
            .unwrap();
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(7);
        header.set_cksum();
        tar.append_data(&mut header, "pkg/blocked", "content".as_bytes())
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        // `blocked` already exists as a non-empty directory, so the writer
        // thread fails to replace it with a file.
        let output = dir.path().join("out");
        fs::create_dir_all(output.join("blocked/keep")).unwrap();

        let error = extract_archive(&archive, &output, ArchiveKind::TarGz, "pkg", 0, &[])
            .expect_err("write error should surface");
        assert!(format!("{error:#}").contains("blocked"));
    }

    #[test]
    fn test_extract_tar_rejects_escaping_hardlink_target() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("source.tar.gz");
        let file = fs::File::create(&archive).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut tar = Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Link);
        header.set_size(0);
        tar.append_link(&mut header, "pkg/link.txt", "../../outside")
            .unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let output = dir.path().join("out");
        let error = extract_archive(&archive, &output, ArchiveKind::TarGz, "pkg", 0, &[])
            .expect_err("escaping hardlink target should be rejected");
        assert!(format!("{error:#}").contains("escapes the extraction directory"));
    }
}
