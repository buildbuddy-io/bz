/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt::Write;
use std::hash::Hash;
use std::sync::Arc;

use allocative::Allocative;
use bz_common::cas_digest::DataDigester;
use bz_common::cas_digest::DigestAlgorithmFamily;
use bz_common::external_symlink::ExternalSymlink;
use bz_common::file_ops::metadata::FileDigest;
use bz_common::file_ops::metadata::FileDigestConfig;
use bz_common::file_ops::metadata::FileMetadata;
use bz_common::file_ops::metadata::SourceFileMetadata;
use bz_common::file_ops::metadata::Symlink;
use bz_core::content_hash::ContentBasedPathHash;
use bz_directory::directory::entry::DirectoryEntry;
use bz_fs::paths::RelativePathBuf;
use bz_fs::paths::abs_path::AbsPath;
use bz_fs::paths::file_name::FileNameBuf;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_util::strong_hasher::Blake3StrongHasher;
use dupe::Dupe;
use pagable::Pagable;

use crate::digest_config::DigestConfig;
use crate::directory::ActionDirectoryBuilder;
use crate::directory::ActionDirectoryEntry;
use crate::directory::ActionDirectoryMember;
use crate::directory::ActionSharedDirectory;
use crate::directory::INTERNER;

const LOCAL_ACTION_CACHE_ARTIFACT_VALUE_VERSION: u8 = 1;
const LOCAL_ACTION_CACHE_ENTRY_FILE: u8 = 0;
const LOCAL_ACTION_CACHE_ENTRY_SYMLINK: u8 = 1;
const LOCAL_ACTION_CACHE_ENTRY_EXTERNAL_SYMLINK: u8 = 2;
const LOCAL_ACTION_CACHE_ENTRY_DIRECTORY: u8 = 3;
const LOCAL_ACTION_CACHE_DIGEST_SHA1: u8 = 0;
const LOCAL_ACTION_CACHE_DIGEST_SHA256: u8 = 1;
const LOCAL_ACTION_CACHE_DIGEST_BLAKE3: u8 = 2;
const LOCAL_ACTION_CACHE_DIGEST_BLAKE3_KEYED: u8 = 3;

#[derive(Clone, Dupe, Debug, PartialEq, Eq, Allocative, Pagable)]
pub enum UnderlyingContentBasedPathHash {
    Inferred,
    Explicit(Arc<ContentBasedPathHash>),
}

/// `ArtifactValue` stores enough information about an artifact such that, if
/// it's in the CAS, we don't have to read anything from disk. In summary:
/// - for files, that's the digest and whether it's executable;
/// - for symlinks, that's its target (which we'd read with `fs::read_link`);
/// - for directories, that's the whole file tree.
///
/// However, when we have symlinks, we also must make the artifacts they point
/// to available. Therefore, when this represents a symlink, or a directory
/// with symlinks pointing outside such directory, we must also store the value
/// of the artifacts pointed to by those symlinks. That's the `deps` attribute.
#[derive(Clone, Debug, Dupe, PartialEq, Eq, Allocative, Pagable)]
pub struct ArtifactValue {
    /// The information about the artifact i.e. digest + is_executable if this
    /// is a file, the file tree if this is a directory, and so on.
    entry: ActionDirectoryEntry<ActionSharedDirectory>,
    /// A tree with all other artifacts which this value depends on. Unlike
    /// `entry` above, which is rooted at this artifact's path, `deps` is
    /// always rooted at the project root.
    deps: Option<ActionSharedDirectory>,
    /// The content-based path hash of the artifact. This is usually inferred,
    /// but in some cases (e.g. projected artifacts) it is explicitly provided.
    content_based_path_hash: UnderlyingContentBasedPathHash,
}

impl ArtifactValue {
    pub fn new(
        entry: ActionDirectoryEntry<ActionSharedDirectory>,
        deps: Option<ActionSharedDirectory>,
    ) -> Self {
        Self {
            entry,
            deps,
            content_based_path_hash: UnderlyingContentBasedPathHash::Inferred,
        }
    }

    pub fn file(meta: FileMetadata) -> Self {
        Self {
            entry: ActionDirectoryEntry::Leaf(ActionDirectoryMember::File(meta)),
            deps: None,
            content_based_path_hash: UnderlyingContentBasedPathHash::Inferred,
        }
    }

    pub fn source_file(meta: SourceFileMetadata) -> Self {
        Self {
            entry: ActionDirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(meta)),
            deps: None,
            content_based_path_hash: UnderlyingContentBasedPathHash::Inferred,
        }
    }

    pub fn dir(dir: ActionSharedDirectory) -> Self {
        Self {
            entry: ActionDirectoryEntry::Dir(dir),
            deps: None,
            content_based_path_hash: UnderlyingContentBasedPathHash::Inferred,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.entry, ActionDirectoryEntry::Dir(_))
    }

    pub fn is_symlink(&self) -> bool {
        matches!(
            self.entry,
            ActionDirectoryEntry::Leaf(
                ActionDirectoryMember::Symlink(_) | ActionDirectoryMember::ExternalSymlink(_)
            )
        )
    }

    pub fn has_source_file_proxy(&self) -> bool {
        matches!(
            self.entry,
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(_))
        )
    }

    pub fn resolve_source_file_proxy(
        &self,
        path: &AbsPath,
        digest_config: DigestConfig,
    ) -> bz_error::Result<Self> {
        let ActionDirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(source)) = &self.entry
        else {
            return Ok(self.dupe());
        };

        let file_digest_config = FileDigestConfig::source(digest_config.cas_digest_config());
        let digest = FileDigest::from_file(path, file_digest_config)?;
        let digest = bz_common::file_ops::metadata::TrackedFileDigest::new(
            digest,
            file_digest_config.as_cas_digest_config(),
        );
        Ok(Self::file(FileMetadata {
            digest,
            is_executable: source.contents_proxy.is_executable,
        }))
    }

    pub fn external_symlink(symlink: Arc<ExternalSymlink>) -> Self {
        Self {
            entry: ActionDirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(symlink)),
            deps: None,
            content_based_path_hash: UnderlyingContentBasedPathHash::Inferred,
        }
    }

    pub fn entry(&self) -> &ActionDirectoryEntry<ActionSharedDirectory> {
        &self.entry
    }

    pub fn with_executable_bit(&self, executable: bool) -> Self {
        let entry = match &self.entry {
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::File(file)) => {
                ActionDirectoryEntry::Leaf(ActionDirectoryMember::File(
                    file.dupe().with_executable(executable),
                ))
            }
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(file)) => {
                let mut contents_proxy = (*file.contents_proxy).clone();
                contents_proxy.is_executable = executable;
                ActionDirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(
                    SourceFileMetadata::new(contents_proxy),
                ))
            }
            _ => self.entry.dupe(),
        };

        Self {
            entry,
            deps: self.deps.dupe(),
            content_based_path_hash: self.content_based_path_hash.dupe(),
        }
    }

    pub fn deps(&self) -> Option<&ActionSharedDirectory> {
        self.deps.as_ref()
    }

    pub fn digest(&self) -> Option<&FileDigest> {
        match &self.entry {
            ActionDirectoryEntry::Dir(d) => Some(d.fingerprint().data()),
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => Some(f.digest.data()),
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(..)) => None,
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::Symlink(..)) => None,
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(..)) => None,
        }
    }

    /// Size of this artifact (and its dependencies) in bytes.
    pub fn size(&self) -> u64 {
        (match &self.entry {
            DirectoryEntry::Dir(d) => d.size(),
            DirectoryEntry::Leaf(m) => m.size(),
        } + self.deps.as_ref().map_or(0, |d| d.size()))
    }

    pub fn with_content_based_path_hash(
        self,
        content_based_path_hash: ContentBasedPathHash,
    ) -> Self {
        Self {
            content_based_path_hash: UnderlyingContentBasedPathHash::Explicit(Arc::new(
                content_based_path_hash,
            )),
            ..self
        }
    }

    pub fn content_based_path_hash(&self) -> ContentBasedPathHash {
        if let UnderlyingContentBasedPathHash::Explicit(hash) = &self.content_based_path_hash {
            return (**hash).clone();
        }

        match &self.entry {
            ActionDirectoryEntry::Dir(d) => {
                ContentBasedPathHash::new(d.fingerprint().data().raw_digest().as_bytes())
            }
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => {
                ContentBasedPathHash::new(f.digest.data().raw_digest().as_bytes())
            }
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(f)) => {
                let mut hasher = Blake3StrongHasher::new();
                f.hash(&mut hasher);
                ContentBasedPathHash::new(hasher.finalize().as_bytes())
            }
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::Symlink(s)) => {
                let mut hasher = Blake3StrongHasher::new();
                s.target().hash(&mut hasher);
                ContentBasedPathHash::new(hasher.finalize().as_bytes())
            }
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(s)) => {
                let mut hasher = Blake3StrongHasher::new();
                s.hash(&mut hasher);
                ContentBasedPathHash::new(hasher.finalize().as_bytes())
            }
        }
        .expect("Constructed valid content-based path hash")
    }

    pub fn action_cache_fingerprint(&self) -> String {
        let mut fingerprint = String::new();
        write!(
            &mut fingerprint,
            "entry:{}\0content_hash:{}",
            entry_action_cache_fingerprint(self.entry()),
            self.content_based_path_hash().as_str()
        )
        .expect("writing to a string cannot fail");
        if let Some(deps) = self.deps() {
            write!(
                &mut fingerprint,
                "\0deps:{}:{}",
                deps.fingerprint(),
                deps.size()
            )
            .expect("writing to a string cannot fail");
        }
        fingerprint
    }

    pub fn hash_action_cache_fingerprint(&self, fingerprint: &mut DataDigester) {
        action_cache_hash_entry(fingerprint, self.entry());
        match &self.content_based_path_hash {
            UnderlyingContentBasedPathHash::Inferred => {
                action_cache_add_str(fingerprint, "content_hash_inferred");
            }
            UnderlyingContentBasedPathHash::Explicit(hash) => {
                action_cache_add_str(fingerprint, "content_hash_explicit");
                action_cache_add_str(fingerprint, hash.as_str());
            }
        }
        if let Some(deps) = self.deps() {
            action_cache_add_str(fingerprint, "deps");
            action_cache_add_tracked_file_digest(fingerprint, deps.fingerprint());
            action_cache_add_u64(fingerprint, deps.size());
        }
    }

    pub fn write_action_cache_fingerprint_bytes(&self, bytes: &mut Vec<u8>) {
        action_cache_write_entry(bytes, self.entry());
        match &self.content_based_path_hash {
            UnderlyingContentBasedPathHash::Inferred => {
                action_cache_write_str(bytes, "content_hash_inferred");
            }
            UnderlyingContentBasedPathHash::Explicit(hash) => {
                action_cache_write_str(bytes, "content_hash_explicit");
                action_cache_write_str(bytes, hash.as_str());
            }
        }
        if let Some(deps) = self.deps() {
            action_cache_write_str(bytes, "deps");
            action_cache_write_tracked_file_digest(bytes, deps.fingerprint());
            action_cache_write_u64(bytes, deps.size());
        }
    }

    pub fn write_local_action_cache_bytes(&self, bytes: &mut Vec<u8>) -> bz_error::Result<()> {
        bytes.push(LOCAL_ACTION_CACHE_ARTIFACT_VALUE_VERSION);
        write_action_cache_directory_entry(bytes, &self.entry)?;
        write_action_cache_option_directory(bytes, self.deps.as_ref())?;
        match &self.content_based_path_hash {
            UnderlyingContentBasedPathHash::Inferred => write_action_cache_bool(bytes, false),
            UnderlyingContentBasedPathHash::Explicit(hash) => {
                write_action_cache_bool(bytes, true);
                write_action_cache_str(bytes, hash.as_str())?;
            }
        }
        Ok(())
    }

    pub fn read_local_action_cache_bytes(
        bytes: &[u8],
        digest_config: DigestConfig,
    ) -> bz_error::Result<Self> {
        let mut reader = ActionCacheBytesReader::new(bytes);
        let version = reader.read_u8()?;
        if version != LOCAL_ACTION_CACHE_ARTIFACT_VALUE_VERSION {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "unsupported local action cache artifact value version `{}`",
                version
            ));
        }

        let entry = reader.read_action_cache_directory_entry(digest_config)?;
        let deps = reader.read_action_cache_option_directory(digest_config)?;
        let content_based_path_hash = if reader.read_bool()? {
            UnderlyingContentBasedPathHash::Explicit(Arc::new(ContentBasedPathHash::Specified(
                reader.read_str()?.to_owned(),
            )))
        } else {
            UnderlyingContentBasedPathHash::Inferred
        };
        reader.expect_eof()?;

        Ok(Self {
            entry,
            deps,
            content_based_path_hash,
        })
    }
}

fn write_action_cache_bool(bytes: &mut Vec<u8>, value: bool) {
    bytes.push(value as u8);
}

fn write_action_cache_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend(value.to_le_bytes());
}

fn write_action_cache_bytes(bytes: &mut Vec<u8>, value: &[u8]) -> bz_error::Result<()> {
    write_action_cache_u64(bytes, value.len().try_into()?);
    bytes.extend(value);
    Ok(())
}

fn write_action_cache_str(bytes: &mut Vec<u8>, value: &str) -> bz_error::Result<()> {
    write_action_cache_bytes(bytes, value.as_bytes())
}

fn write_action_cache_digest(
    bytes: &mut Vec<u8>,
    digest: &bz_common::file_ops::metadata::TrackedFileDigest,
) -> bz_error::Result<()> {
    let raw_digest = digest.raw_digest();
    bytes.push(match raw_digest.algorithm() {
        DigestAlgorithmFamily::Sha1 => LOCAL_ACTION_CACHE_DIGEST_SHA1,
        DigestAlgorithmFamily::Sha256 => LOCAL_ACTION_CACHE_DIGEST_SHA256,
        DigestAlgorithmFamily::Blake3 => LOCAL_ACTION_CACHE_DIGEST_BLAKE3,
        DigestAlgorithmFamily::Blake3Keyed => LOCAL_ACTION_CACHE_DIGEST_BLAKE3_KEYED,
    });
    write_action_cache_u64(bytes, digest.size());
    write_action_cache_bytes(bytes, raw_digest.as_bytes())
}

fn write_action_cache_directory_member(
    bytes: &mut Vec<u8>,
    member: &ActionDirectoryMember,
) -> bz_error::Result<()> {
    match member {
        ActionDirectoryMember::File(file) => {
            bytes.push(LOCAL_ACTION_CACHE_ENTRY_FILE);
            write_action_cache_digest(bytes, &file.digest)?;
            write_action_cache_bool(bytes, file.is_executable);
        }
        ActionDirectoryMember::SourceFile(_) => {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "source file proxy cannot be stored as a local action cache output value"
            ));
        }
        ActionDirectoryMember::Symlink(symlink) => {
            bytes.push(LOCAL_ACTION_CACHE_ENTRY_SYMLINK);
            write_action_cache_str(bytes, symlink.target().as_str())?;
        }
        ActionDirectoryMember::ExternalSymlink(symlink) => {
            bytes.push(LOCAL_ACTION_CACHE_ENTRY_EXTERNAL_SYMLINK);
            write_action_cache_str(bytes, symlink.target_str())?;
        }
    }
    Ok(())
}

fn write_action_cache_directory_entry(
    bytes: &mut Vec<u8>,
    entry: &ActionDirectoryEntry<ActionSharedDirectory>,
) -> bz_error::Result<()> {
    match entry {
        DirectoryEntry::Leaf(member) => write_action_cache_directory_member(bytes, member),
        DirectoryEntry::Dir(directory) => {
            bytes.push(LOCAL_ACTION_CACHE_ENTRY_DIRECTORY);
            write_action_cache_directory(bytes, directory)
        }
    }
}

fn write_action_cache_directory(
    bytes: &mut Vec<u8>,
    directory: &ActionSharedDirectory,
) -> bz_error::Result<()> {
    let entries = directory.entries();
    write_action_cache_u64(bytes, entries.into_iter().count().try_into()?);
    for (name, entry) in directory.entries() {
        write_action_cache_str(bytes, name.as_str())?;
        write_action_cache_directory_entry(bytes, entry)?;
    }
    Ok(())
}

fn write_action_cache_option_directory(
    bytes: &mut Vec<u8>,
    directory: Option<&ActionSharedDirectory>,
) -> bz_error::Result<()> {
    match directory {
        Some(directory) => {
            write_action_cache_bool(bytes, true);
            write_action_cache_directory(bytes, directory)?;
        }
        None => write_action_cache_bool(bytes, false),
    }
    Ok(())
}

struct ActionCacheBytesReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ActionCacheBytesReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read_exact(&mut self, len: usize) -> bz_error::Result<&'a [u8]> {
        let end = self.position.checked_add(len).ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "local action cache artifact value length overflow"
            )
        })?;
        if end > self.bytes.len() {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "truncated local action cache artifact value"
            ));
        }
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn read_u8(&mut self) -> bz_error::Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_bool(&mut self) -> bz_error::Result<bool> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "invalid bool `{}` in local action cache artifact value",
                value
            )),
        }
    }

    fn read_u64(&mut self) -> bz_error::Result<u64> {
        Ok(u64::from_le_bytes(self.read_exact(8)?.try_into()?))
    }

    fn read_bytes(&mut self) -> bz_error::Result<&'a [u8]> {
        let len: usize = self.read_u64()?.try_into()?;
        self.read_exact(len)
    }

    fn read_str(&mut self) -> bz_error::Result<&'a str> {
        Ok(std::str::from_utf8(self.read_bytes()?)?)
    }

    fn read_digest_algorithm(&mut self) -> bz_error::Result<DigestAlgorithmFamily> {
        match self.read_u8()? {
            LOCAL_ACTION_CACHE_DIGEST_SHA1 => Ok(DigestAlgorithmFamily::Sha1),
            LOCAL_ACTION_CACHE_DIGEST_SHA256 => Ok(DigestAlgorithmFamily::Sha256),
            LOCAL_ACTION_CACHE_DIGEST_BLAKE3 => Ok(DigestAlgorithmFamily::Blake3),
            LOCAL_ACTION_CACHE_DIGEST_BLAKE3_KEYED => Ok(DigestAlgorithmFamily::Blake3Keyed),
            value => Err(bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "invalid digest algorithm `{}` in local action cache artifact value",
                value
            )),
        }
    }

    fn read_digest(
        &mut self,
        digest_config: DigestConfig,
    ) -> bz_error::Result<bz_common::file_ops::metadata::TrackedFileDigest> {
        let algorithm = self.read_digest_algorithm()?;
        let size = self.read_u64()?;
        let digest = FileDigest::from_digest_bytes(algorithm, self.read_bytes()?, size)?;
        Ok(bz_common::file_ops::metadata::TrackedFileDigest::new(
            digest,
            digest_config.cas_digest_config(),
        ))
    }

    fn read_directory_member(
        &mut self,
        tag: u8,
        digest_config: DigestConfig,
    ) -> bz_error::Result<ActionDirectoryMember> {
        Ok(match tag {
            LOCAL_ACTION_CACHE_ENTRY_FILE => ActionDirectoryMember::File(FileMetadata {
                digest: self.read_digest(digest_config)?,
                is_executable: self.read_bool()?,
            }),
            LOCAL_ACTION_CACHE_ENTRY_SYMLINK => ActionDirectoryMember::Symlink(Arc::new(
                Symlink::new(RelativePathBuf::from(self.read_str()?.to_owned())),
            )),
            LOCAL_ACTION_CACHE_ENTRY_EXTERNAL_SYMLINK => {
                ActionDirectoryMember::ExternalSymlink(Arc::new(ExternalSymlink::new(
                    self.read_str()?.to_owned().into(),
                    ForwardRelativePathBuf::default(),
                )?))
            }
            value => {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Tier0,
                    "invalid directory member tag `{}` in local action cache artifact value",
                    value
                ));
            }
        })
    }

    fn read_action_cache_directory_entry(
        &mut self,
        digest_config: DigestConfig,
    ) -> bz_error::Result<ActionDirectoryEntry<ActionSharedDirectory>> {
        let tag = self.read_u8()?;
        if tag == LOCAL_ACTION_CACHE_ENTRY_DIRECTORY {
            return Ok(DirectoryEntry::Dir(
                self.read_action_cache_directory(digest_config)?,
            ));
        }
        Ok(DirectoryEntry::Leaf(
            self.read_directory_member(tag, digest_config)?,
        ))
    }

    fn read_action_cache_builder_entry(
        &mut self,
        digest_config: DigestConfig,
    ) -> bz_error::Result<DirectoryEntry<ActionDirectoryBuilder, ActionDirectoryMember>> {
        let tag = self.read_u8()?;
        if tag == LOCAL_ACTION_CACHE_ENTRY_DIRECTORY {
            return Ok(DirectoryEntry::Dir(
                self.read_action_cache_directory(digest_config)?
                    .into_builder(),
            ));
        }
        Ok(DirectoryEntry::Leaf(
            self.read_directory_member(tag, digest_config)?,
        ))
    }

    fn read_action_cache_directory(
        &mut self,
        digest_config: DigestConfig,
    ) -> bz_error::Result<ActionSharedDirectory> {
        let len = self.read_u64()?;
        let mut builder = ActionDirectoryBuilder::empty();
        for _ in 0..len {
            let name = FileNameBuf::try_from(self.read_str()?.to_owned())?;
            let entry = self.read_action_cache_builder_entry(digest_config)?;
            builder.insert(name, entry)?;
        }
        Ok(builder
            .fingerprint(digest_config.as_directory_serializer())
            .shared(&*INTERNER))
    }

    fn read_action_cache_option_directory(
        &mut self,
        digest_config: DigestConfig,
    ) -> bz_error::Result<Option<ActionSharedDirectory>> {
        if self.read_bool()? {
            Ok(Some(self.read_action_cache_directory(digest_config)?))
        } else {
            Ok(None)
        }
    }

    fn expect_eof(&self) -> bz_error::Result<()> {
        if self.position != self.bytes.len() {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "trailing data in local action cache artifact value"
            ));
        }
        Ok(())
    }
}

fn entry_action_cache_fingerprint(
    entry: &DirectoryEntry<ActionSharedDirectory, ActionDirectoryMember>,
) -> String {
    let mut fingerprint = String::new();
    match entry {
        DirectoryEntry::Dir(dir) => {
            write!(&mut fingerprint, "dir:{}:{}", dir.fingerprint(), dir.size())
                .expect("writing to a string cannot fail");
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::File(file)) => {
            write!(
                &mut fingerprint,
                "file:{}:{}:{}",
                file.digest,
                file.digest.size(),
                file.is_executable
            )
            .expect("writing to a string cannot fail");
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(file)) => {
            let proxy = &file.contents_proxy;
            write!(
                &mut fingerprint,
                "source_file:{}:{}:{}:{}:{}:{}:{}",
                proxy.size,
                proxy.modified_secs,
                proxy.modified_nanos,
                proxy.changed_secs,
                proxy.changed_nanos,
                proxy.node_id,
                proxy.is_executable
            )
            .expect("writing to a string cannot fail");
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(symlink)) => {
            write!(&mut fingerprint, "symlink:{}", symlink.target())
                .expect("writing to a string cannot fail");
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(symlink)) => {
            write!(
                &mut fingerprint,
                "external_symlink:{}",
                symlink.target_str()
            )
            .expect("writing to a string cannot fail");
        }
    }
    fingerprint
}

fn action_cache_add_bytes(fingerprint: &mut DataDigester, bytes: &[u8]) {
    fingerprint.update(&(bytes.len() as u64).to_le_bytes());
    fingerprint.update(bytes);
}

fn action_cache_add_str(fingerprint: &mut DataDigester, value: &str) {
    action_cache_add_bytes(fingerprint, value.as_bytes());
}

fn action_cache_add_u64(fingerprint: &mut DataDigester, value: u64) {
    fingerprint.update(&value.to_le_bytes());
}

fn action_cache_add_bool(fingerprint: &mut DataDigester, value: bool) {
    fingerprint.update(&[value as u8]);
}

fn action_cache_add_file_digest(fingerprint: &mut DataDigester, digest: &FileDigest) {
    let raw_digest = digest.raw_digest();
    fingerprint.update(&[raw_digest.algorithm() as u8]);
    action_cache_add_bytes(fingerprint, raw_digest.as_bytes());
    action_cache_add_u64(fingerprint, digest.size());
}

fn action_cache_add_tracked_file_digest(
    fingerprint: &mut DataDigester,
    digest: &bz_common::file_ops::metadata::TrackedFileDigest,
) {
    let raw_digest = digest.raw_digest();
    fingerprint.update(&[raw_digest.algorithm() as u8]);
    action_cache_add_bytes(fingerprint, raw_digest.as_bytes());
    action_cache_add_u64(fingerprint, digest.size());
}

fn action_cache_hash_entry(
    fingerprint: &mut DataDigester,
    entry: &DirectoryEntry<ActionSharedDirectory, ActionDirectoryMember>,
) {
    match entry {
        DirectoryEntry::Dir(dir) => {
            action_cache_add_str(fingerprint, "dir");
            action_cache_add_tracked_file_digest(fingerprint, dir.fingerprint());
            action_cache_add_u64(fingerprint, dir.size());
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::File(file)) => {
            action_cache_add_str(fingerprint, "file");
            action_cache_add_file_digest(fingerprint, file.digest.data());
            action_cache_add_bool(fingerprint, file.is_executable);
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(file)) => {
            let proxy = &file.contents_proxy;
            action_cache_add_str(fingerprint, "source_file");
            action_cache_add_u64(fingerprint, proxy.size);
            fingerprint.update(&proxy.modified_secs.to_le_bytes());
            fingerprint.update(&proxy.modified_nanos.to_le_bytes());
            fingerprint.update(&proxy.changed_secs.to_le_bytes());
            fingerprint.update(&proxy.changed_nanos.to_le_bytes());
            action_cache_add_u64(fingerprint, proxy.node_id);
            action_cache_add_bool(fingerprint, proxy.is_executable);
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(symlink)) => {
            action_cache_add_str(fingerprint, "symlink");
            action_cache_add_str(fingerprint, symlink.target().as_str());
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(symlink)) => {
            action_cache_add_str(fingerprint, "external_symlink");
            action_cache_add_str(fingerprint, symlink.target_str());
        }
    }
}

fn action_cache_write_bytes(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend((value.len() as u64).to_le_bytes());
    bytes.extend(value);
}

fn action_cache_write_str(bytes: &mut Vec<u8>, value: &str) {
    action_cache_write_bytes(bytes, value.as_bytes());
}

fn action_cache_write_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend(value.to_le_bytes());
}

fn action_cache_write_bool(bytes: &mut Vec<u8>, value: bool) {
    bytes.push(value as u8);
}

fn action_cache_write_file_digest(bytes: &mut Vec<u8>, digest: &FileDigest) {
    let raw_digest = digest.raw_digest();
    bytes.push(raw_digest.algorithm() as u8);
    action_cache_write_bytes(bytes, raw_digest.as_bytes());
    action_cache_write_u64(bytes, digest.size());
}

fn action_cache_write_tracked_file_digest(
    bytes: &mut Vec<u8>,
    digest: &bz_common::file_ops::metadata::TrackedFileDigest,
) {
    let raw_digest = digest.raw_digest();
    bytes.push(raw_digest.algorithm() as u8);
    action_cache_write_bytes(bytes, raw_digest.as_bytes());
    action_cache_write_u64(bytes, digest.size());
}

fn action_cache_write_entry(
    bytes: &mut Vec<u8>,
    entry: &DirectoryEntry<ActionSharedDirectory, ActionDirectoryMember>,
) {
    match entry {
        DirectoryEntry::Dir(dir) => {
            action_cache_write_str(bytes, "dir");
            action_cache_write_tracked_file_digest(bytes, dir.fingerprint());
            action_cache_write_u64(bytes, dir.size());
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::File(file)) => {
            action_cache_write_str(bytes, "file");
            action_cache_write_file_digest(bytes, file.digest.data());
            action_cache_write_bool(bytes, file.is_executable);
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::SourceFile(file)) => {
            let proxy = &file.contents_proxy;
            action_cache_write_str(bytes, "source_file");
            action_cache_write_u64(bytes, proxy.size);
            bytes.extend(proxy.modified_secs.to_le_bytes());
            bytes.extend(proxy.modified_nanos.to_le_bytes());
            bytes.extend(proxy.changed_secs.to_le_bytes());
            bytes.extend(proxy.changed_nanos.to_le_bytes());
            action_cache_write_u64(bytes, proxy.node_id);
            action_cache_write_bool(bytes, proxy.is_executable);
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(symlink)) => {
            action_cache_write_str(bytes, "symlink");
            action_cache_write_str(bytes, symlink.target().as_str());
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(symlink)) => {
            action_cache_write_str(bytes, "external_symlink");
            action_cache_write_str(bytes, symlink.target_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bz_common::file_ops::metadata::TrackedFileDigest;

    fn file_value(content: &[u8], is_executable: bool) -> ArtifactValue {
        ArtifactValue::file(FileMetadata {
            digest: TrackedFileDigest::from_content(
                content,
                DigestConfig::testing_default().cas_digest_config(),
            ),
            is_executable,
        })
    }

    fn assert_local_action_cache_roundtrip(value: ArtifactValue) -> bz_error::Result<()> {
        let digest_config = DigestConfig::testing_default();
        let mut bytes = Vec::new();
        value.write_local_action_cache_bytes(&mut bytes)?;
        let decoded = ArtifactValue::read_local_action_cache_bytes(&bytes, digest_config)?;
        assert_eq!(value, decoded);
        Ok(())
    }

    #[test]
    fn local_action_cache_roundtrips_file() -> bz_error::Result<()> {
        assert_local_action_cache_roundtrip(
            file_value(b"hello", true).with_content_based_path_hash(
                ContentBasedPathHash::Specified("0123456789abcdef".to_owned()),
            ),
        )
    }

    #[test]
    fn local_action_cache_roundtrips_symlink() -> bz_error::Result<()> {
        assert_local_action_cache_roundtrip(ArtifactValue::new(
            DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(Arc::new(Symlink::new(
                RelativePathBuf::from("target"),
            )))),
            None,
        ))
    }

    #[test]
    fn local_action_cache_roundtrips_directory() -> bz_error::Result<()> {
        let digest_config = DigestConfig::testing_default();
        let mut builder = ActionDirectoryBuilder::empty();
        builder.insert(
            FileNameBuf::try_from("file".to_owned())?,
            DirectoryEntry::Leaf(ActionDirectoryMember::File(FileMetadata {
                digest: TrackedFileDigest::from_content(b"file", digest_config.cas_digest_config()),
                is_executable: false,
            })),
        )?;
        let directory = builder
            .fingerprint(digest_config.as_directory_serializer())
            .shared(&*INTERNER);
        assert_local_action_cache_roundtrip(ArtifactValue::dir(directory))
    }
}
