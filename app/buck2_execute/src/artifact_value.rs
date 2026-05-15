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
