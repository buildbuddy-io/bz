use std::sync::Mutex;

use bz_common::file_ops::metadata::FileDigest;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_directory::directory::directory::Directory;
use bz_directory::directory::directory_iterator::DirectoryIterator;
use bz_directory::directory::entry::DirectoryEntry;
use bz_directory::directory::walk::unordered_entry_walk;
use bz_hash::StdBuckHashSet;
use dupe::Dupe;

use crate::artifact_value::ArtifactValue;
use crate::directory::ActionDirectoryMember;
use crate::materialize::materializer::LostRemoteCasArtifacts;

#[derive(Default)]
pub struct KnownMissingRemoteCasTracker {
    file_digests: Mutex<StdBuckHashSet<FileDigest>>,
}

impl KnownMissingRemoteCasTracker {
    pub fn record_lost_remote_cas_artifacts(&self, lost: &LostRemoteCasArtifacts) {
        self.record_file_digests(
            lost.iter()
                .flat_map(|artifact| artifact.missing_digests.iter()),
        );
    }

    pub fn record_file_digests<'a>(
        &self,
        digests: impl IntoIterator<Item = &'a TrackedFileDigest>,
    ) {
        let mut file_digests = self
            .file_digests
            .lock()
            .expect("known missing remote CAS tracker lock poisoned");
        file_digests.extend(digests.into_iter().map(|digest| digest.data().dupe()));
    }

    pub fn contains_file_digest(&self, digest: &FileDigest) -> bool {
        self.file_digests
            .lock()
            .expect("known missing remote CAS tracker lock poisoned")
            .contains(digest)
    }

    pub fn contains_tracked_file_digest(&self, digest: &TrackedFileDigest) -> bool {
        self.contains_file_digest(digest.data())
    }

    pub fn contains_artifact_value(&self, value: &ArtifactValue) -> bool {
        let file_digests = self
            .file_digests
            .lock()
            .expect("known missing remote CAS tracker lock poisoned");
        artifact_value_file_digests(value).any(|digest| file_digests.contains(digest.data()))
    }

    pub fn contains_artifact_values<'a>(
        &self,
        values: impl IntoIterator<Item = &'a ArtifactValue>,
    ) -> bool {
        let file_digests = self
            .file_digests
            .lock()
            .expect("known missing remote CAS tracker lock poisoned");
        values.into_iter().any(|value| {
            artifact_value_file_digests(value).any(|digest| file_digests.contains(digest.data()))
        })
    }

    /// Forgets all recorded digests. Called after a successful build: missing
    /// digests recorded by previous builds may have been re-uploaded since, and
    /// keeping them would needlessly reject action cache hits for them.
    pub fn clear(&self) {
        self.file_digests
            .lock()
            .expect("known missing remote CAS tracker lock poisoned")
            .clear();
    }

    pub fn remove_artifact_values<'a>(
        &self,
        values: impl IntoIterator<Item = &'a ArtifactValue>,
    ) -> bool {
        let mut file_digests = self
            .file_digests
            .lock()
            .expect("known missing remote CAS tracker lock poisoned");
        let mut removed = false;
        for value in values {
            for digest in artifact_value_file_digests(value) {
                removed |= file_digests.remove(digest.data());
            }
        }
        removed
    }
}

fn artifact_value_file_digests(
    value: &ArtifactValue,
) -> impl Iterator<Item = &TrackedFileDigest> + '_ {
    unordered_entry_walk(value.entry().as_ref().map_dir(Directory::as_ref))
        .without_paths()
        .filter_map(|entry| match entry {
            DirectoryEntry::Leaf(ActionDirectoryMember::File(file)) => Some(&file.digest),
            _ => None,
        })
}
