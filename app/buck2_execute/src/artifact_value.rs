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
use buck2_common::cas_digest::DataDigester;
use buck2_common::external_symlink::ExternalSymlink;
use buck2_common::file_ops::metadata::FileDigest;
use buck2_common::file_ops::metadata::FileMetadata;
use buck2_core::content_hash::ContentBasedPathHash;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_util::strong_hasher::Blake3StrongHasher;
use dupe::Dupe;
use pagable::Pagable;

use crate::directory::ActionDirectoryEntry;
use crate::directory::ActionDirectoryMember;
use crate::directory::ActionSharedDirectory;

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

    pub fn deps(&self) -> Option<&ActionSharedDirectory> {
        self.deps.as_ref()
    }

    pub fn digest(&self) -> Option<&FileDigest> {
        match &self.entry {
            ActionDirectoryEntry::Dir(d) => Some(d.fingerprint().data()),
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => Some(f.digest.data()),
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
    digest: &buck2_common::file_ops::metadata::TrackedFileDigest,
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
