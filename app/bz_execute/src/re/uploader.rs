/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::borrow::Borrow;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use bz_common::cas_digest::TrackedCasDigest;
use bz_common::file_ops::metadata::FileDigest;
use bz_common::file_ops::metadata::FileDigestKind;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_core::bz_env;
use bz_core::execution_types::executor_config::RemoteExecutorUseCase;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_data::ReUploadMetrics;
use bz_directory::directory::directory::Directory;
use bz_directory::directory::directory_iterator::DirectoryIterator;
use bz_directory::directory::directory_iterator::DirectoryIteratorPathStack;
use bz_directory::directory::directory_ref::FingerprintedDirectoryRef;
use bz_directory::directory::entry::DirectoryEntry;
use bz_directory::directory::fingerprinted_directory::FingerprintedDirectory;
use bz_error::BuckErrorContext;
use bz_error::conversion::from_any_with_tag;
use bz_error::internal_error;
use bz_hash::StdBuckHashMap;
use bz_hash::StdBuckHashSet;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use dupe::Dupe;
use either::Either;
use futures::FutureExt;
use futures::TryStreamExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use futures::stream::FuturesUnordered;
use gazebo::prelude::*;
use once_cell::sync::Lazy;
use remote_execution::GetDigestsTtlResponse;
use remote_execution::InlinedBlobWithDigest;
use remote_execution::NamedDigest;
use remote_execution::TCode;
use remote_execution::TCodeReasonGroup;
use remote_execution::TDigest;
use remote_execution::UploadRequest;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;

use crate::digest::CasDigestFromReExt;
use crate::digest::CasDigestToReExt;
use crate::digest_config::DigestConfig;
use crate::directory::ActionDirectoryMember;
use crate::directory::ActionFingerprintedDirectoryRef;
use crate::directory::ActionImmutableDirectory;
use crate::directory::ReDirectorySerializer;
use crate::execute::blobs::ActionBlobs;
use crate::execute::request::CommandExecutionPaths;
use crate::materialize::materializer::ArtifactNotMaterializedReason;
use crate::materialize::materializer::LostRemoteCasArtifact;
use crate::materialize::materializer::LostRemoteCasArtifacts;
use crate::materialize::materializer::Materializer;
use crate::re::action_identity::ReActionIdentity;
use crate::re::client::RemoteExecutionClient;
use crate::re::error::re_error;
use crate::re::error::with_error_handler;
use crate::re::metadata::RemoteExecutionMetadataExt;

#[derive(Clone, Debug, Default)]
pub struct UploadStats {
    pub total: ReUploadMetrics,
    pub by_extension: StdBuckHashMap<String, ReUploadMetrics>,
}

pub struct Uploader {}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct UploadDigestKey {
    hash: String,
    size_in_bytes: i64,
}

impl UploadDigestKey {
    fn new(digest: &TDigest) -> Self {
        Self {
            hash: digest.hash.clone(),
            size_in_bytes: digest.size_in_bytes,
        }
    }
}

enum UploadClaimState {
    Own(oneshot::Sender<bz_error::Result<()>>),
    Wait(UploadWaiter),
    Uploaded,
}

type UploadWaiter = Shared<BoxFuture<'static, bz_error::Result<()>>>;

#[derive(Default)]
struct UploadDeduper {
    in_flight: StdBuckHashMap<UploadDigestKey, UploadWaiter>,
    uploaded: StdBuckHashMap<UploadDigestKey, DateTime<Utc>>,
}

fn upload_cancelled_error() -> bz_error::Error {
    re_error(
        "upload",
        "unknown",
        "remote upload was cancelled before completion".to_owned(),
        TCode::CANCELLED,
        TCodeReasonGroup::UNKNOWN,
    )
}

fn dupe_result(result: &bz_error::Result<()>) -> bz_error::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(e) => Err(e.dupe()),
    }
}

impl UploadDeduper {
    fn claim(&mut self, key: UploadDigestKey) -> UploadClaimState {
        if self.uploaded.contains_key(&key) {
            return UploadClaimState::Uploaded;
        }

        if let Some(waiter) = self.in_flight.get(&key) {
            return UploadClaimState::Wait(waiter.clone());
        }

        let (sender, receiver) = oneshot::channel();
        let waiter = async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(upload_cancelled_error()))
        }
        .boxed()
        .shared();
        self.in_flight.insert(key, waiter);
        UploadClaimState::Own(sender)
    }

    fn prune_uploaded(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::minutes(30);
        self.uploaded
            .retain(|_, uploaded_at| *uploaded_at >= cutoff);
    }
}

static UPLOAD_DEDUPER: Lazy<Mutex<UploadDeduper>> =
    Lazy::new(|| Mutex::new(UploadDeduper::default()));

static UPLOAD_SETUP_SEMAPHORE: Lazy<Semaphore> = Lazy::new(|| {
    Semaphore::new(std::thread::available_parallelism().map_or(1, |value| value.get()))
});

#[derive(Default)]
struct UploadClaim {
    owned: Vec<(UploadDigestKey, oneshot::Sender<bz_error::Result<()>>)>,
    waiters: Vec<UploadWaiter>,
}

impl UploadClaim {
    fn claim_uploads(
        upload_files: Vec<NamedDigest>,
        upload_blobs: Vec<InlinedBlobWithDigest>,
    ) -> (Vec<NamedDigest>, Vec<InlinedBlobWithDigest>, Self) {
        let now = Utc::now();
        let mut deduper = UPLOAD_DEDUPER.lock().unwrap();
        deduper.prune_uploaded(now);

        let mut claim = UploadClaim::default();
        let mut claimed_files = Vec::new();
        let mut claimed_blobs = Vec::new();

        for file in upload_files {
            let key = UploadDigestKey::new(&file.digest);
            match deduper.claim(key.clone()) {
                UploadClaimState::Own(sender) => {
                    claim.owned.push((key, sender));
                    claimed_files.push(file);
                }
                UploadClaimState::Wait(waiter) => claim.waiters.push(waiter),
                UploadClaimState::Uploaded => {}
            }
        }

        for blob in upload_blobs {
            let key = UploadDigestKey::new(&blob.digest);
            match deduper.claim(key.clone()) {
                UploadClaimState::Own(sender) => {
                    claim.owned.push((key, sender));
                    claimed_blobs.push(blob);
                }
                UploadClaimState::Wait(waiter) => claim.waiters.push(waiter),
                UploadClaimState::Uploaded => {}
            }
        }

        (claimed_files, claimed_blobs, claim)
    }

    async fn wait_for_other_uploads(&self) -> bz_error::Result<()> {
        for waiter in &self.waiters {
            waiter.clone().await?;
        }
        Ok(())
    }

    fn complete(&mut self, result: bz_error::Result<()>) {
        let now = Utc::now();
        let mut deduper = UPLOAD_DEDUPER.lock().unwrap();
        for (key, sender) in self.owned.drain(..) {
            deduper.in_flight.remove(&key);
            if result.is_ok() {
                deduper.uploaded.insert(key.clone(), now);
            }
            let _ = sender.send(dupe_result(&result));
        }
    }
}

impl Drop for UploadClaim {
    fn drop(&mut self) {
        if !self.owned.is_empty() {
            self.complete(Err(upload_cancelled_error()));
        }
    }
}

impl Uploader {
    async fn find_missing<'a>(
        client: &RemoteExecutionClient,
        input_dir: &'a ActionImmutableDirectory,
        blobs: &'a ActionBlobs,
        use_case: &RemoteExecutorUseCase,
        identity: Option<&ReActionIdentity<'_>>,
        digest_config: DigestConfig,
        deduplicate_get_digests_ttl_calls: bool,
        force_reupload: bool,
    ) -> bz_error::Result<(
        Vec<InlinedBlobWithDigest>,
        StdBuckHashSet<&'a TrackedCasDigest<FileDigestKind>>,
    )> {
        // See if anything needs uploading
        let mut input_digests = blobs.keys().collect::<StdBuckHashSet<_>>();

        if force_reupload {
            for entry in input_dir.unordered_walk().without_paths() {
                let digest = match entry {
                    DirectoryEntry::Dir(d) => d.as_fingerprinted_dyn().fingerprint(),
                    DirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => &f.digest,
                    DirectoryEntry::Leaf(..) => continue,
                };
                input_digests.insert(digest);
            }
            input_digests.insert(input_dir.fingerprint());

            let mut upload_blobs = Vec::new();
            let mut missing_digests = StdBuckHashSet::default();
            for digest in input_digests {
                match blobs.get(digest) {
                    Some(blob) => {
                        upload_blobs.push(InlinedBlobWithDigest {
                            blob: blob.clone().0,
                            digest: digest.to_re(),
                            ..Default::default()
                        });
                    }
                    None => {
                        missing_digests.insert(digest);
                    }
                }
            }
            return Ok((upload_blobs, missing_digests));
        }

        // RE mentions they usually take 5-10 minutes of leeway so we mirror this here.
        let now = Utc::now();
        let ttl_wanted = 1;
        let ttl_deadline = now + Duration::seconds(ttl_wanted);

        {
            // Collect the digests we need to upload
            for entry in input_dir.unordered_walk().without_paths() {
                let digest = match entry {
                    DirectoryEntry::Dir(d) => d.as_fingerprinted_dyn().fingerprint(),
                    DirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => &f.digest,
                    DirectoryEntry::Leaf(..) => continue,
                };

                if digest.expires()? <= ttl_deadline {
                    input_digests.insert(digest);
                }
            }

            let root_dir_digest = input_dir.fingerprint();
            if root_dir_digest.expires()? <= ttl_deadline {
                input_digests.insert(root_dir_digest);
            }
        };

        let mut upload_blobs = Vec::new();
        let mut missing_digests = StdBuckHashSet::default();
        add_injected_missing_digests(&input_digests, &mut missing_digests)?;

        let digests_and_ttls_iterator = if deduplicate_get_digests_ttl_calls {
            let (fut, reqs, new) = {
                static GET_DIGESTS_TTL_DEDUP: Lazy<Mutex<GetDigestsTtlDeduper>> =
                    Lazy::new(|| Mutex::new(GetDigestsTtlDeduper::default()));

                GetDigestsTtlDeduper::get_ttls(
                    &GET_DIGESTS_TTL_DEDUP,
                    client,
                    *use_case,
                    identity,
                    digest_config,
                    input_digests.iter().copied(),
                )
            };

            tracing::debug!(
                "Requested digests for {}: {:#?}: {} futures, {} newly dispatched digests",
                input_dir.fingerprint(),
                input_digests.len(),
                reqs,
                new
            );

            let input_digests_ttls = fut.await?;

            struct DigestsWithTtlIterator<I> {
                ttls: StdBuckHashMap<TrackedFileDigest, i64>,
                inner: I,
            }

            impl<'a, I> Iterator for DigestsWithTtlIterator<I>
            where
                I: Iterator<Item = &'a TrackedFileDigest>,
            {
                type Item = bz_error::Result<(&'a TrackedFileDigest, i64)>;

                fn next(&mut self) -> Option<bz_error::Result<(&'a TrackedFileDigest, i64)>> {
                    let digest = self.inner.next()?;
                    let digest_ttl = self
                        .ttls
                        .get(digest)
                        .ok_or_else(|| internal_error!("Did not get a TTL for digest: {}", digest));
                    Some(digest_ttl.map(|ttl| (digest, *ttl)))
                }
            }

            Either::Left(DigestsWithTtlIterator {
                ttls: input_digests_ttls,
                inner: input_digests.into_iter(),
            })
        } else {
            let client = client.clone();
            let metadata = use_case.metadata(identity);
            let digests = input_digests.iter().map(|d| d.to_re()).collect();
            let digests_ttl = client.get_digests_ttl(digests, metadata).await;

            let input_digests = input_digests.iter().copied().collect();

            Either::Right(process_get_digest_ttls_response(
                input_digests,
                digests_ttl?,
                digest_config,
            )?)
        };

        tracing::debug!("Got digests for {}", input_dir.fingerprint());

        // Now find the blobs that need to be uploaded
        for digest_with_ttl in digests_and_ttls_iterator {
            let (digest, digest_ttl) = digest_with_ttl?;

            if digest_ttl <= ttl_wanted {
                tracing::debug!(digest=%digest, ttl=digest_ttl, "Mark for upload");

                match blobs.get(digest) {
                    Some(blob) => {
                        upload_blobs.push(InlinedBlobWithDigest {
                            blob: blob.clone().0,
                            digest: digest.to_re(),
                            ..Default::default()
                        });
                    }
                    None => {
                        missing_digests.insert(digest);
                    }
                }
            } else {
                tracing::debug!(digest=%digest, ttl=digest_ttl, "Not uploading");
                let ttl = Duration::seconds(digest_ttl);
                digest.update_expires(now + ttl);
            }
        }

        Ok((upload_blobs, missing_digests))
    }

    pub async fn upload(
        fs: &ProjectRoot,
        client: &RemoteExecutionClient,
        materializer: &Arc<dyn Materializer>,
        dir_path: &ProjectRelativePath,
        input_dir: &ActionImmutableDirectory,
        input_paths: Option<&CommandExecutionPaths>,
        blobs: &ActionBlobs,
        use_case: RemoteExecutorUseCase,
        identity: Option<&ReActionIdentity<'_>>,
        digest_config: DigestConfig,
        deduplicate_get_digests_ttl_calls: bool,
        force_reupload: bool,
    ) -> bz_error::Result<UploadStats> {
        // Bazel limits remote action building/input upload setup to CPU count.
        // Keep this process-wide so multiple RE client instances cannot stampede
        // get_digests_ttl/upload calls on the same daemon.
        let _upload_setup = UPLOAD_SETUP_SEMAPHORE.acquire().await.map_err(|_| {
            bz_error::bz_error!(
                bz_error::ErrorTag::InternalError,
                "remote upload setup semaphore was closed"
            )
        })?;

        let (mut upload_blobs, mut missing_digests) = Self::find_missing(
            client,
            input_dir,
            blobs,
            &use_case,
            identity,
            digest_config,
            deduplicate_get_digests_ttl_calls,
            force_reupload,
        )
        .await?;

        if upload_blobs.is_empty() && missing_digests.is_empty() {
            return Ok(UploadStats::default());
        }

        // Find the file paths and directory blobs that need to be uploaded
        let mut upload_files = Vec::new();

        // Track what files should be materialized before we upload.
        let mut paths_to_materialize = Vec::new();

        if !missing_digests.is_empty() {
            let artifact_path_alias_upload_paths = input_paths.map(|input_paths| {
                let mut exact_paths = StdBuckHashMap::default();
                let mut directory_paths = Vec::new();
                for (path, source_path, is_dir) in input_paths.artifact_path_alias_upload_paths() {
                    if is_dir {
                        directory_paths.push((path, source_path));
                    } else {
                        exact_paths.insert(path.as_forward_relative_path(), source_path);
                    }
                }
                (exact_paths, directory_paths)
            });
            let external_symlink_upload_paths = input_paths.map(|input_paths| {
                let mut exact_paths = StdBuckHashMap::default();
                let mut directory_paths = Vec::new();
                for upload_path in input_paths.external_symlink_upload_paths() {
                    if upload_path.is_dir {
                        directory_paths.push((&upload_path.path, &upload_path.source_path));
                    } else {
                        exact_paths.insert(
                            upload_path.path.as_forward_relative_path(),
                            &upload_path.source_path,
                        );
                    }
                }
                (exact_paths, directory_paths)
            });
            let resolved_symlink_upload_paths = input_paths.map(|input_paths| {
                let mut exact_paths = StdBuckHashMap::default();
                let mut directory_paths = Vec::new();
                for upload_path in input_paths.resolved_symlink_upload_paths() {
                    if upload_path.is_dir {
                        directory_paths.push((&upload_path.path, &upload_path.source_path));
                    } else {
                        exact_paths.insert(
                            upload_path.path.as_forward_relative_path(),
                            &upload_path.source_path,
                        );
                    }
                }
                (exact_paths, directory_paths)
            });
            let mut upload_file_paths = Vec::new();
            let mut upload_file_digests = Vec::new();

            {
                let mut walk = input_dir.unordered_walk();
                while let Some((path, entry)) = walk.next() {
                    let digest = match entry {
                        DirectoryEntry::Dir(d) => d.as_fingerprinted_dyn().fingerprint(),
                        DirectoryEntry::Leaf(ActionDirectoryMember::File(f)) => &f.digest,
                        DirectoryEntry::Leaf(..) => continue,
                    };

                    if !missing_digests.remove(digest) {
                        continue;
                    }

                    match entry {
                        DirectoryEntry::Dir(d) => {
                            upload_blobs.push(directory_to_blob(d));
                        }
                        DirectoryEntry::Leaf(ActionDirectoryMember::File(..)) => {
                            let input_path = path.get();
                            let input_path = &*input_path;
                            if let Some(upload_file_path) = resolved_symlink_upload_paths
                                .as_ref()
                                .and_then(|(exact_paths, directory_paths)| {
                                    if let Some(source_path) = exact_paths.get(input_path) {
                                        return Some(source_path.to_buf());
                                    }
                                    for (path, source_path) in directory_paths {
                                        if let Some(suffix) = input_path
                                            .strip_prefix_opt(path.as_forward_relative_path())
                                        {
                                            return Some(source_path.join(suffix));
                                        }
                                    }
                                    None
                                })
                            {
                                upload_file_paths.push(upload_file_path);
                                upload_file_digests.push(digest.to_re());
                                continue;
                            }
                            if let Some(upload_file_path) = external_symlink_upload_paths
                                .as_ref()
                                .and_then(|(exact_paths, directory_paths)| {
                                    if let Some(source_path) = exact_paths.get(input_path) {
                                        return Some((*source_path).clone());
                                    }
                                    for (path, source_path) in directory_paths {
                                        if let Some(suffix) = input_path
                                            .strip_prefix_opt(path.as_forward_relative_path())
                                        {
                                            return Some(source_path.join(suffix.as_str()));
                                        }
                                    }
                                    None
                                })
                            {
                                upload_files.push(NamedDigest {
                                    name: upload_file_path.display().to_string(),
                                    digest: digest.to_re(),
                                    ..Default::default()
                                });
                                continue;
                            }
                            let upload_file_path = artifact_path_alias_upload_paths
                                .as_ref()
                                .and_then(|(exact_paths, directory_paths)| {
                                    if let Some(source_path) = exact_paths.get(input_path) {
                                        return Some(source_path.to_buf());
                                    }
                                    for (path, source_path) in directory_paths {
                                        if let Some(suffix) = input_path
                                            .strip_prefix_opt(path.as_forward_relative_path())
                                        {
                                            return Some(source_path.join(suffix));
                                        }
                                    }
                                    None
                                })
                                .unwrap_or_else(|| dir_path.join(input_path));
                            upload_file_paths.push(upload_file_path);
                            upload_file_digests.push(digest.to_re());
                        }
                        DirectoryEntry::Leaf(..) => unreachable!(), // TODO: Better representation of this.
                    };
                }
            }

            if missing_digests.remove(input_dir.fingerprint()) {
                upload_blobs.push(directory_to_blob(input_dir.as_fingerprinted_ref()));
            }

            assert!(
                missing_digests.is_empty(),
                "Expected a path to be found for every digest, traversal code is inconsistent. Left with {missing_digests:?}."
            );

            // Get the real path of the files we are going to upload.
            // This needs to be done because we could have A copied to B, and
            // we are asked to upload B. But since we defer local copies until
            // it's actually needed for a local run, B might not have been
            // copied yet (or ever), so we should upload A directly instead.
            let upload_file_paths = materializer
                .get_materialized_file_paths(upload_file_paths)
                .await?;

            for (name, digest) in upload_file_paths.into_iter().zip(upload_file_digests) {
                match name {
                    Ok(name) => {
                        upload_files.push(NamedDigest {
                            name: fs.resolve(&name).as_maybe_relativized_str()?.to_owned(),
                            digest,
                            ..Default::default()
                        });
                    }
                    Err(
                        ref err @ ArtifactNotMaterializedReason::RequiresCasDownload {
                            ref path,
                            ref entry,
                            ref info,
                        },
                    ) => {
                        if let DirectoryEntry::Leaf(ActionDirectoryMember::File(file)) =
                            entry.as_ref()
                        {
                            // NOTE: find_missing has negative caching, so when we query to know if an
                            // artifact was uploaded, if it was the result of an action we just ran, it
                            // won't be here. On the flip side, if a digest has been in the CAS for
                            // a very long time, it might have expired.
                            if file.digest.to_re() == digest {
                                if let Some(origin) = info.remote_origin() {
                                    let lost = LostRemoteCasArtifact {
                                        path: Arc::new(path.clone()),
                                        owner: None,
                                        missing_digests: Arc::from(vec![file.digest.dupe()]),
                                        producer_path_hint: None,
                                        origin,
                                    };
                                    return Err(bz_error::bz_error!(
                                        bz_error::ErrorTag::Input,
                                        "Remote-backed CAS artifact is missing while preparing remote execution inputs: {:#}",
                                        err,
                                    )
                                    .context(LostRemoteCasArtifacts::new(vec![lost])));
                                }

                                return Err(bz_error::bz_error!(
                                    bz_error::ErrorTag::Input,
                                    "Declared CAS artifact `{}` is missing while preparing remote execution inputs. Debug information: {:#}",
                                    file.digest,
                                    err,
                                ));
                            }
                        }

                        return Err(error_for_missing_file(&digest, err));
                    }
                    Err(ArtifactNotMaterializedReason::RequiresMaterialization { path }) => {
                        upload_files.push(NamedDigest {
                            name: fs.resolve(&path).as_maybe_relativized_str()?.to_owned(),
                            digest,
                            ..Default::default()
                        });
                        paths_to_materialize.push(path);
                    }
                    Err(
                        ref err @ ArtifactNotMaterializedReason::DeferredMaterializerCorruption {
                            ..
                        },
                    ) => {
                        return Err(error_for_missing_file(&digest, err));
                    }
                };
            }
        }

        if !paths_to_materialize.is_empty() {
            materializer
                .ensure_materialized(paths_to_materialize)
                .await
                .buck_error_context("Error materializing paths for upload")?;
        }

        let (upload_files, upload_blobs, mut upload_claim) =
            UploadClaim::claim_uploads(upload_files, upload_blobs);

        // Compute stats of digests we're about to upload so we can report them
        // to the span end event of this stage of execution.
        let stats = {
            let mut stats_by_extension = StdBuckHashMap::default();
            let mut named_digest_byte_count: u64 = 0;
            for nd in &upload_files {
                // Aggregate metrics by file extension.
                let byte_count: u64 = nd.digest.size_in_bytes.try_into().unwrap_or_default();
                let extension = extract_file_extension(&nd.name);
                let ext_stats: &mut ReUploadMetrics =
                    stats_by_extension.entry(extension).or_default();
                ext_stats.digests_uploaded += 1;
                ext_stats.bytes_uploaded += byte_count;
                named_digest_byte_count += byte_count;
            }
            let blob_byte_count: u64 = upload_blobs
                .iter()
                .map(|blob| {
                    let byte_count: u64 = blob.digest.size_in_bytes.try_into().unwrap_or_default();
                    byte_count
                })
                .sum();

            UploadStats {
                total: ReUploadMetrics {
                    digests_uploaded: (upload_files.len() + upload_blobs.len()) as u64,
                    bytes_uploaded: named_digest_byte_count + blob_byte_count,
                },
                by_extension: stats_by_extension,
            }
        };

        // Upload
        let upload_result = if !upload_files.is_empty() || !upload_blobs.is_empty() {
            with_error_handler(
                "upload",
                client.get_session_id(),
                client.get_raw_re_client()
                    .upload(
                        use_case.metadata(identity),
                        UploadRequest {
                            files_with_digest: Some(upload_files),
                            inlined_blobs_with_digest: Some(upload_blobs),
                            // all find missing checks are done previously
                            // and we can skip them and upload all digests
                            upload_only_missing: false,
                            ..Default::default()
                        },
                    )
                    .await,
            )
            .await
            .map(|_| ())
            .map_err(|e| {
                if e.tags().contains(&bz_error::ErrorTag::ReInvalidArgument) {
                    bz_error::bz_error!(
                        bz_error::ErrorTag::ReInvalidArgument,
                        "RE Upload failed. It looks like you might have modified files while the build \
                        was in progress. Retry your build to proceed. Debug information: {:#}",
                        e
                    )
                } else {
                    e
                }
            })
        } else {
            Ok(())
        };
        match upload_result {
            Ok(()) => upload_claim.complete(Ok(())),
            Err(e) => {
                upload_claim.complete(Err(e.dupe()));
                return Err(e);
            }
        }
        upload_claim.wait_for_other_uploads().await?;

        Ok(stats)
    }
}

fn directory_to_blob<'a, D>(d: D) -> InlinedBlobWithDigest
where
    D: ActionFingerprintedDirectoryRef<'a>,
{
    InlinedBlobWithDigest {
        digest: d.as_fingerprinted_dyn().fingerprint().to_re(),
        blob: ReDirectorySerializer::serialize_entries(d.entries()),
        ..Default::default()
    }
}

fn error_for_missing_file(
    digest: &TDigest,
    cause: &ArtifactNotMaterializedReason,
) -> bz_error::Error {
    bz_error::bz_error!(
        bz_error::ErrorTag::ReInvalidGetCasResponse,
        "Action execution requires artifact `{}` but the materializer did not return a matching \
        file for this path. This indicates inconsistent materializer metadata. \
        Debug information: {:#}",
        digest,
        cause,
    )
}

/// This is used for tests. We allow an environment variable to be set to report that some digests
/// are _always_ missing if they are required. This lets us test our upload paths more easily.
fn add_injected_missing_digests<'a>(
    input_digests: &StdBuckHashSet<&'a TrackedFileDigest>,
    missing_digests: &mut StdBuckHashSet<&'a TrackedFileDigest>,
) -> bz_error::Result<()> {
    fn convert_digests(val: &str) -> bz_error::Result<Vec<FileDigest>> {
        val.split(' ')
            .map(|digest| {
                let digest = TDigest::from_str(digest)
                    .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::InvalidDigest))
                    .with_buck_error_context(|| format!("Invalid digest: `{digest}`"))?;
                // This code does not run in a test but it is only used for testing.
                let digest = FileDigest::from_re(&digest, DigestConfig::testing_default())?;
                bz_error::Ok(digest)
            })
            .collect()
    }

    let ingested_digests = bz_env!(
        "BUCK2_TEST_INJECTED_MISSING_DIGESTS",
        type=Vec<FileDigest>,
        converter=convert_digests,
        applicability=testing
    )?;
    if let Some(digests) = ingested_digests {
        for d in digests {
            if let Some(i) = input_digests.get(d) {
                missing_digests.insert(i);
            }
        }
    }

    Ok(())
}

fn extract_file_extension(path: &str) -> String {
    let path = Path::new(path);
    match path.extension() {
        Some(ext) => ext.to_string_lossy().to_lowercase(),
        None => "<empty>".to_owned(),
    }
}

#[derive(
    allocative::Allocative,
    Copy,
    Clone,
    Debug,
    dupe::Dupe,
    PartialEq,
    Eq,
    Hash
)]
struct RequestId(u64);

/// Tracks digests that have in-flight calls to RE and dedupes them.
#[derive(Default)]
struct GetDigestsTtlDeduper<'s> {
    /// Used to allow `digests` to index into `queries`.
    next_request_id: u64,
    /// Maps a given digest to a request that will produce this digest (and
    /// possibly / likely others). The request is referenced as an ID that
    /// can be used to lookup in `queries`.
    digests: StdBuckHashMap<TrackedFileDigest, RequestId>,
    /// Maps a request to the actual future that will contain its results.
    queries: StdBuckHashMap<
        RequestId,
        Shared<BoxFuture<'s, bz_error::Result<StdBuckHashMap<TrackedFileDigest, i64>>>>,
    >,
}

impl<'s> GetDigestsTtlDeduper<'s> {
    /// Obtain a future that will return the TTLs for the digests that are
    /// queried (and possibly more TTLs).
    fn get_ttls<'a>(
        deduper: &'s Mutex<Self>,
        client: &'a RemoteExecutionClient,
        use_case: RemoteExecutorUseCase,
        identity: Option<&'a ReActionIdentity<'a>>,
        digest_config: DigestConfig,
        digests: impl IntoIterator<Item = &'a TrackedFileDigest>,
    ) -> (
        impl Future<Output = bz_error::Result<StdBuckHashMap<TrackedFileDigest, i64>>> + 's,
        usize,
        usize,
    ) {
        let mut guard = deduper.lock().expect("Poisoned lock");

        let mut reqs = StdBuckHashSet::default();

        let mut to_schedule = Vec::new();

        for digest in digests {
            if let Some(req_id) = guard.digests.get(digest) {
                reqs.insert(*req_id);
            } else {
                to_schedule.push(digest.dupe());
            }
        }

        let to_schedule_len = to_schedule.len();

        if !to_schedule.is_empty() {
            let request_id = RequestId(guard.next_request_id);
            guard.next_request_id += 1;

            reqs.insert(request_id);

            for digest in &to_schedule {
                guard.digests.insert(digest.dupe(), request_id);
            }

            guard.queries.insert(
                request_id,
                query_digest_ttls(
                    deduper,
                    request_id,
                    client,
                    use_case,
                    identity,
                    digest_config,
                    to_schedule,
                )
                .shared(),
            );
        }

        let reqs_len = reqs.len();

        let futs = reqs
            .into_iter()
            .map(|req| guard.queries.get(&req).unwrap().clone())
            .collect::<FuturesUnordered<_>>();

        let fut = async move {
            let results: Vec<_> = futs.try_collect().await?;
            Ok(results.into_iter().flatten().collect())
        };

        (fut, reqs_len, to_schedule_len)
    }
}

/// Call RE, get  the TTLs, then match them back to our inputs. Also deregister
/// this request once it finishes so we don't cache it forever.
fn query_digest_ttls<'s>(
    deduper: &'s Mutex<GetDigestsTtlDeduper>,
    request_id: RequestId,
    client: &RemoteExecutionClient,
    use_case: RemoteExecutorUseCase,
    identity: Option<&ReActionIdentity<'_>>,
    digest_config: DigestConfig,
    input_digests: Vec<TrackedFileDigest>,
) -> BoxFuture<'s, bz_error::Result<StdBuckHashMap<TrackedFileDigest, i64>>> {
    let client = client.dupe();
    let metadata = use_case.metadata(identity);
    let digests = input_digests.iter().map(|d| d.to_re()).collect();

    async move {
        let digests_ttl = client.get_digests_ttl(digests, metadata).await;

        {
            let mut guard = deduper.lock().expect("Poisoned lock");
            guard.queries.remove(&request_id);
            for digest in &input_digests {
                guard.digests.remove(digest);
            }
        }

        // It's possibly a bit of a shame that we deregister this response
        // before we process it here, but in practice we need to draw a line at
        // some point and no matter where we draw it, races where we "just miss"
        // the deduped request will exist, unless we have synchronization
        // between checking and setting digest TTLs, unless we cache digests
        // forever (which right now we don't do because we track TTLs on the
        // digest object itself and not all actions are guaranteed to hold the
        // same instance, but maybe that's something that should be revisited),
        // AND figure out invalidation (because the TTL will change when we
        // upload).
        process_get_digest_ttls_response(input_digests, digests_ttl?, digest_config)?.collect()
    }
    .boxed()
}

fn process_get_digest_ttls_response<T>(
    mut req: Vec<T>,
    res: GetDigestsTtlResponse,
    digest_config: DigestConfig,
) -> bz_error::Result<impl Iterator<Item = bz_error::Result<(T, i64)>>>
where
    T: Borrow<TrackedFileDigest> + Ord,
{
    let digest_ttls = res.digests_with_ttl;

    if req.len() != digest_ttls.len() {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::ReInvalidGetCasResponse,
            "Invalid response from get_digests_ttl: expected {}, got {} digests",
            req.len(),
            digest_ttls.len()
        ));
    }

    req.sort();

    let mut digest_ttls = digest_ttls.into_try_map(|d| {
        bz_error::Ok((
            FileDigest::from_re(&d.digest, digest_config).map_err(bz_error::Error::from)?,
            d.ttl,
        ))
    })?;
    digest_ttls.sort();

    Ok(req
        .into_iter()
        .zip(digest_ttls)
        .map(|(digest, (matching_digest, digest_ttl))| {
            if *digest.borrow().data() != matching_digest {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::ReInvalidGetCasResponse,
                    "Invalid response from get_digests_ttl"
                ));
            }

            Ok((digest, digest_ttl))
        }))
}
