/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Client side of the remote repo contents cache protocol (Bazel 9 compatible).
//!
//! This mirrors Bazel's `RemoteRepoContentsCacheImpl`
//! (`src/main/java/com/google/devtools/build/lib/remote/RemoteRepoContentsCacheImpl.java`):
//! the contents of an external repository are cached as an ordinary REAPI
//! ActionCache entry for a *synthetic* action that is never executed.
//!
//! The protocol:
//!
//! * A constant synthetic [`RE::Command`]: a unique but nonsensical command
//!   (its single argument is a constant GUID) that is valid on all platforms
//!   and should pass any checks an RE backend may apply to commands. It
//!   declares two outputs: the `.recorded_inputs` marker file and the
//!   `repo_contents` output directory. The command is constant, so its
//!   serialized bytes (and per digest function, its digest) are constant.
//! * An empty input root `Directory` (the default instance).
//! * A per-repo [`RE::Action`]: `command_digest` + `input_root_digest` +
//!   `salt`, where the salt is the byte representation of the repo's
//!   predeclared input hash. Like Bazel, we embed the hash into the salt
//!   simply because that results in a constant `Command` message.
//! * The cache entry is an `ActionResult` with exactly one output file
//!   (path `.recorded_inputs`, containing the recorded-inputs JSON) and one
//!   output directory (path `repo_contents`, whose `tree_digest` points at an
//!   [`RE::Tree`] of the repo contents in CAS).
//!
//! Phase 1 (unlike Bazel) does not implement the DAG of intermediate AC
//! entries for repos with dynamically recorded inputs: there is exactly one
//! entry per predeclared input hash, and the full recorded-inputs JSON is
//! stored as the `.recorded_inputs` output file. The caller validates the
//! recorded inputs locally after download and falls back to a normal fetch if
//! they are out of date.
//!
//! bz and Bazel construct predeclared input hashes differently, so entries
//! written by one client are never looked up by the other (and the GUIDs
//! differ); only the protocol shape is shared so that any REAPI cache that
//! serves Bazel 9 (e.g. BuildBuddy) serves these entries too.

use std::sync::Arc;
use std::sync::LazyLock;

use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_directory::directory::fingerprinted_directory::FingerprintedDirectory;
use chrono::Utc;
use dupe::Dupe;
use remote_execution as RE;
use remote_execution::DigestWithStatus;
use remote_execution::TActionResult2;
use remote_execution::TCode;
use remote_execution::TDigest;
use remote_execution::TDirectory2;
use remote_execution::TExecutedActionMetadata;
use remote_execution::TFile;
use remote_execution::TStatus;

use crate::digest::CasDigestToReExt;
use crate::digest_config::DigestConfig;
use crate::directory::ActionDirectoryBuilder;
use crate::directory::ActionImmutableDirectory;
use crate::directory::directory_to_re_tree;
use crate::directory::re_tree_to_directory;
use crate::execute::action_digest::ActionDigest;
use crate::execute::blobs::ActionBlobs;
use crate::execute::request::ActionMetadataBlobData;
use crate::materialize::materializer::Materializer;
use crate::re::action_identity::ReActionIdentity;
use crate::re::client::ActionCacheWriteType;
use crate::re::error::RemoteExecutionError;
use crate::re::manager::ManagedRemoteExecutionClient;

/// The constant GUID used as the single argument of the synthetic command.
///
/// This is bz's own namespace: it intentionally differs from Bazel's GUID
/// (`f4a165a9-5557-45a7-bf25-230b6d42393a`) so that bz and Bazel entries can
/// never be confused for one another even on a shared cache.
pub const REPO_CONTENTS_CACHE_GUID: &str = "89c727ce-ee85-48ae-8217-0f83e8535700";

/// Output file path holding the recorded-inputs JSON (Bazel: marker file).
pub const REPO_CONTENTS_CACHE_MARKER_FILE_PATH: &str = ".recorded_inputs";

/// Output directory path holding the repo contents tree.
pub const REPO_CONTENTS_CACHE_REPO_DIRECTORY_PATH: &str = "repo_contents";

/// The constant synthetic command, serialized once. Its digest is constant
/// per digest function and is computed from these bytes on each use.
static COMMAND_BYTES: LazyLock<Vec<u8>> =
    LazyLock::new(|| ActionMetadataBlobData::from_message(&synthetic_command()).0);

/// Builds the constant synthetic REAPI `Command`, mirroring Bazel's `COMMAND`
/// constant in `RemoteRepoContentsCacheImpl`: both the v2.1 `output_paths`
/// and the deprecated `output_files`/`output_directories` fields are
/// populated, and the platform is the default (empty) instance.
fn synthetic_command() -> RE::Command {
    #[allow(deprecated)]
    let mut command = RE::Command {
        arguments: vec![REPO_CONTENTS_CACHE_GUID.to_owned()],
        output_files: vec![REPO_CONTENTS_CACHE_MARKER_FILE_PATH.to_owned()],
        output_directories: vec![REPO_CONTENTS_CACHE_REPO_DIRECTORY_PATH.to_owned()],
        platform: Some(RE::Platform::default()),
        ..Default::default()
    };
    // The workspace RE types predate REAPI v2.1 and do not have `output_paths`
    // (see `OutputPathsBehavior::OutputPaths` in `execute/command_executor.rs`).
    // This protocol is only used with OSS REAPI backends.
    {
        command.output_paths = vec![
            REPO_CONTENTS_CACHE_MARKER_FILE_PATH.to_owned(),
            REPO_CONTENTS_CACHE_REPO_DIRECTORY_PATH.to_owned(),
        ];
    }
    command
}

/// The synthetic command bytes plus their digest for the given digest config.
fn command_blob(digest_config: DigestConfig) -> (TrackedFileDigest, ActionMetadataBlobData) {
    let bytes = COMMAND_BYTES.clone();
    let digest = TrackedFileDigest::from_content(&bytes, digest_config.cas_digest_config());
    (digest, ActionMetadataBlobData(bytes))
}

/// Builds the per-repo synthetic `Action`: constant command digest, empty
/// input root, and the predeclared input hash embedded as the salt (the bytes
/// of the hash string, exactly like Bazel's
/// `setSalt(ByteString.copyFrom(StringUnsafe.getByteArray(inputHash)))`).
fn build_action(predeclared_input_hash: &str, digest_config: DigestConfig) -> RE::Action {
    let (command_digest, _) = command_blob(digest_config);
    // The empty input root `Directory` (default instance) serializes to zero
    // bytes, so its digest is the empty digest.
    let input_root_digest = TrackedFileDigest::empty(digest_config.cas_digest_config());
    RE::Action {
        command_digest: Some(command_digest.to_grpc()),
        input_root_digest: Some(input_root_digest.to_grpc()),
        salt: predeclared_input_hash.as_bytes().to_vec(),
        platform: Some(RE::Platform::default()),
        ..Default::default()
    }
}

/// The serialized synthetic `Action` for the given predeclared input hash,
/// plus the resulting `ActionDigest` (the AC key).
fn action_blob(
    predeclared_input_hash: &str,
    digest_config: DigestConfig,
) -> (ActionDigest, TrackedFileDigest, ActionMetadataBlobData) {
    let blob =
        ActionMetadataBlobData::from_message(&build_action(predeclared_input_hash, digest_config));
    let digest = TrackedFileDigest::from_content(&blob.0, digest_config.cas_digest_config());
    let action_digest: ActionDigest = digest.data().dupe().coerce();
    (action_digest, digest, blob)
}

/// Computes the ActionCache key for the repo contents cache entry of the
/// given predeclared input hash.
pub fn repo_contents_cache_action_digest(
    predeclared_input_hash: &str,
    digest_config: DigestConfig,
) -> ActionDigest {
    action_blob(predeclared_input_hash, digest_config).0
}

/// A remote repo contents cache hit.
pub struct RepoContentsCacheHit {
    /// The raw bytes of the `.recorded_inputs` output file (the
    /// recorded-inputs JSON written next to the local cache entry). The
    /// caller must validate these against the current environment before
    /// using the hit.
    pub recorded_inputs_json: Vec<u8>,
    /// The `repo_contents` output directory of the cache entry.
    ///
    /// `repo_contents.tree_digest` points at an [`RE::Tree`] blob in CAS:
    /// download and convert it with
    /// [`ManagedRemoteExecutionClient::repo_contents_cache_download_tree`],
    /// then materialize by declaring the directory to the materializer.
    pub repo_contents: TDirectory2,
    /// The TTL reported by RE for the action result, if any.
    pub ttl: i64,
}

impl RepoContentsCacheHit {
    /// Convenience accessor for the digest of the `RE::Tree` blob describing
    /// the repo contents.
    pub fn tree_digest(&self) -> &TDigest {
        &self.repo_contents.tree_digest
    }
}

fn is_re_not_found(error: &bz_error::Error) -> bool {
    error
        .find_typed_context::<RemoteExecutionError>()
        .is_some_and(|error| error.code == TCode::NOT_FOUND)
}

impl ManagedRemoteExecutionClient {
    /// Looks up the remote repo contents cache entry for the given
    /// predeclared input hash.
    ///
    /// Returns `Ok(None)` on a cache miss (`NOT_FOUND`, including the marker
    /// file blob having been evicted from CAS) or when the entry does not
    /// have the expected shape; transient errors are retried by the
    /// underlying calls like every other AC/CAS operation.
    ///
    /// Note: REAPI servers may inline the marker file contents in the
    /// `ActionResult`, but the client's action result representation
    /// (`TFile`) only carries a digest, so the marker file is always read
    /// back from CAS. It is a small blob served via the inlined-download
    /// path.
    pub async fn repo_contents_cache_lookup(
        &self,
        predeclared_input_hash: &str,
        identity: Option<&ReActionIdentity<'_>>,
        digest_config: DigestConfig,
    ) -> bz_error::Result<Option<RepoContentsCacheHit>> {
        let action_digest =
            repo_contents_cache_action_digest(predeclared_input_hash, digest_config);

        // `action_cache` already maps NOT_FOUND (and other lookup errors) to
        // `None` and retries transient errors.
        let response = match self
            .action_cache(action_digest, &RE::Platform::default(), identity)
            .await?
        {
            Some(response) => response,
            None => return Ok(None),
        };

        let result = &response.action_result;
        if result.exit_code != 0
            || result.output_files.len() != 1
            || result.output_directories.len() != 1
            || !result.output_symlinks.is_empty()
        {
            tracing::warn!(
                predeclared_input_hash,
                "Unexpected action result shape for remotely cached repo contents, \
                 treating as a miss"
            );
            return Ok(None);
        }

        let marker_file = &result.output_files[0];
        let repo_contents = &result.output_directories[0];
        if marker_file.name != REPO_CONTENTS_CACHE_MARKER_FILE_PATH
            || repo_contents.path != REPO_CONTENTS_CACHE_REPO_DIRECTORY_PATH
        {
            tracing::warn!(
                predeclared_input_hash,
                marker_path = %marker_file.name,
                repo_path = %repo_contents.path,
                "Unexpected output paths in remotely cached repo contents entry, \
                 treating as a miss"
            );
            return Ok(None);
        }

        let recorded_inputs_json = match self.download_blob(&marker_file.digest.digest).await {
            Ok(bytes) => bytes,
            Err(error) if is_re_not_found(&error) => {
                // The AC entry outlived its marker file blob; treat the entry
                // as evicted.
                tracing::warn!(
                    predeclared_input_hash,
                    "Recorded-inputs blob of remotely cached repo contents entry is \
                     missing in CAS, treating as a miss"
                );
                return Ok(None);
            }
            Err(error) => return Err(error),
        };

        Ok(Some(RepoContentsCacheHit {
            recorded_inputs_json,
            repo_contents: repo_contents.clone(),
            ttl: response.ttl,
        }))
    }

    /// Downloads the [`RE::Tree`] describing the repo contents of a cache hit
    /// and converts it into a fingerprinted directory builder of the repo
    /// contents (the children of the `repo_contents` output directory).
    ///
    /// This only fetches the tree *metadata*; the file blobs themselves are
    /// fetched when the resulting directory is declared to the materializer
    /// and materialized.
    pub async fn repo_contents_cache_download_tree(
        &self,
        hit: &RepoContentsCacheHit,
        identity: Option<&ReActionIdentity<'_>>,
        digest_config: DigestConfig,
    ) -> bz_error::Result<ActionDirectoryBuilder> {
        let expires = Utc::now() + chrono::Duration::seconds(hit.ttl.max(0));
        let tree = self
            .download_typed_blobs::<RE::Tree>(identity, vec![hit.tree_digest().clone()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                bz_error::internal_error!(
                    "No tree returned for remotely cached repo contents digest `{}`",
                    hit.tree_digest()
                )
            })?;
        re_tree_to_directory(&tree, &expires, digest_config, /* fingerprint */ true)
    }

    /// Uploads a repo contents cache entry for the given predeclared input
    /// hash.
    ///
    /// `repo_tree` is the fingerprinted directory of the local repo contents
    /// (the promoted cache entry directory), and `repo_contents_path` is its
    /// project-relative location on disk, from which file blobs are read.
    ///
    /// This:
    /// 1. uploads the synthetic `Command`/`Action` blobs (the REAPI spec
    ///    requires them in CAS before an action result referencing them is
    ///    written) and the recorded-inputs JSON blob,
    /// 2. uploads the repo contents tree (the `RE::Tree` message, the
    ///    directory blobs and any missing file blobs) through the regular
    ///    uploader, reusing its FindMissing/TTL dedup machinery, and
    /// 3. writes the `ActionResult` (one output file, one output directory)
    ///    via `UpdateActionResult` ("latest wins").
    ///
    /// Errors are returned to the caller, which is expected to treat uploads
    /// as best-effort.
    pub async fn repo_contents_cache_upload(
        &self,
        predeclared_input_hash: &str,
        recorded_inputs_json: &[u8],
        repo_tree: &ActionImmutableDirectory,
        fs: &ProjectRoot,
        materializer: &Arc<dyn Materializer>,
        repo_contents_path: &ProjectRelativePath,
        identity: Option<&ReActionIdentity<'_>>,
        digest_config: DigestConfig,
    ) -> bz_error::Result<()> {
        let (command_digest, command_blob) = command_blob(digest_config);
        let (action_digest, action_blob_digest, action_blob) =
            action_blob(predeclared_input_hash, digest_config);

        let recorded_inputs_digest = TrackedFileDigest::from_content(
            recorded_inputs_json,
            digest_config.cas_digest_config(),
        );

        // Metadata blobs referenced by the action / action result. Like the
        // action cache uploader (`bz_execute_impl/src/executors/caching.rs`),
        // these are small and uploaded inline with server-side
        // missing-blob dedup. This also covers the empty input root
        // `Directory`, whose blob `ActionBlobs::new` always includes.
        let mut meta_blobs = ActionBlobs::new(digest_config);
        meta_blobs.add_blob(command_digest, command_blob);
        meta_blobs.add_blob(action_blob_digest, action_blob);
        meta_blobs.add_blob(
            recorded_inputs_digest.dupe(),
            ActionMetadataBlobData(recorded_inputs_json.to_vec()),
        );
        self.upload_files_and_directories(
            Vec::new(),
            Vec::new(),
            meta_blobs.to_inlined_blobs(),
            /* force_reupload */ false,
        )
        .await?;

        // The repo contents tree: the `RE::Tree` message travels as a blob
        // alongside the directory/file uploads, exactly like output trees in
        // the action cache uploader.
        let tree = directory_to_re_tree(repo_tree);
        let mut tree_blobs = ActionBlobs::new(digest_config);
        let tree_digest = tree_blobs.add_protobuf_message(&tree, digest_config);

        self.upload(
            fs,
            materializer,
            &tree_blobs,
            repo_contents_path,
            repo_tree,
            /* input_paths */ None,
            identity,
            digest_config,
            /* deduplicate_get_digests_ttl_calls */ true,
            /* force_reupload */ false,
        )
        .await?;

        let result = TActionResult2 {
            output_files: vec![TFile {
                digest: DigestWithStatus {
                    digest: recorded_inputs_digest.to_re(),
                    status: TStatus {
                        code: TCode::OK,
                        message: String::new(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                name: REPO_CONTENTS_CACHE_MARKER_FILE_PATH.to_owned(),
                executable: false,
                ..Default::default()
            }],
            output_directories: vec![TDirectory2 {
                path: REPO_CONTENTS_CACHE_REPO_DIRECTORY_PATH.to_owned(),
                tree_digest: tree_digest.to_re(),
                root_directory_digest: repo_tree.fingerprint().to_re(),
                ..Default::default()
            }],
            exit_code: 0,
            execution_metadata: TExecutedActionMetadata {
                execution_attempts: 1,
                ..Default::default()
            },
            ..Default::default()
        };

        self.write_action_result(
            action_digest,
            result,
            identity,
            &RE::Platform::default(),
            ActionCacheWriteType::RepoContentsCache,
        )
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    #[test]
    fn test_command_is_constant_and_well_formed() {
        let command = RE::Command::decode(COMMAND_BYTES.as_slice()).unwrap();
        assert_eq!(command.arguments, vec![REPO_CONTENTS_CACHE_GUID.to_owned()]);
        #[allow(deprecated)]
        {
            assert_eq!(
                command.output_files,
                vec![REPO_CONTENTS_CACHE_MARKER_FILE_PATH.to_owned()]
            );
            assert_eq!(
                command.output_directories,
                vec![REPO_CONTENTS_CACHE_REPO_DIRECTORY_PATH.to_owned()]
            );
        }
        assert_eq!(
            command.output_paths,
            vec![
                REPO_CONTENTS_CACHE_MARKER_FILE_PATH.to_owned(),
                REPO_CONTENTS_CACHE_REPO_DIRECTORY_PATH.to_owned(),
            ]
        );
        assert_eq!(command.platform, Some(RE::Platform::default()));
        // Serialized exactly once.
        assert_eq!(
            &*COMMAND_BYTES,
            &ActionMetadataBlobData::from_message(&synthetic_command()).0
        );
    }

    #[test]
    fn test_action_embeds_hash_as_salt() {
        let digest_config = DigestConfig::testing_default();
        let action = build_action("d4c0ffee", digest_config);
        assert_eq!(action.salt, b"d4c0ffee".to_vec());
        assert_eq!(action.platform, Some(RE::Platform::default()));
        // The command digest is independent of the repo hash.
        assert_eq!(
            action.command_digest,
            build_action("other", digest_config).command_digest
        );
        // The input root is the empty directory.
        assert_eq!(
            action.input_root_digest,
            Some(TrackedFileDigest::empty(digest_config.cas_digest_config()).to_grpc())
        );
    }

    #[test]
    fn test_action_digest_is_deterministic_and_hash_dependent() {
        let digest_config = DigestConfig::testing_default();
        let a = repo_contents_cache_action_digest("hash-1", digest_config);
        let b = repo_contents_cache_action_digest("hash-1", digest_config);
        let c = repo_contents_cache_action_digest("hash-2", digest_config);
        assert_eq!(a.to_string(), b.to_string());
        assert_ne!(a.to_string(), c.to_string());
    }
}
