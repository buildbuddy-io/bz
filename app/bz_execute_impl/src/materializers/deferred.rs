/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

pub mod clean_stale;
mod data_tree;
mod eager_materialization;
mod extension;
mod io_handler;
mod materialize_stack;
mod subscriptions;

pub(crate) mod artifact_tree;
mod command_processor;
pub(crate) mod file_tree;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use allocative::Allocative;
use artifact_tree::ArtifactMaterializationMethod;
use artifact_tree::ArtifactMaterializationStage;
use artifact_tree::Processing;
use artifact_tree::ProcessingFuture;
use async_trait::async_trait;
use bz_common::file_ops::metadata::FileMetadata;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_common::init::RemoteDownloadOutputsMode;
use bz_common::liveliness_observer::LivelinessGuard;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_directory::directory::directory::Directory;
use bz_directory::directory::directory_iterator::DirectoryIterator;
use bz_directory::directory::directory_iterator::DirectoryIteratorPathStack;
use bz_directory::directory::entry::DirectoryEntry;
use bz_directory::directory::walk::unordered_entry_walk;
use bz_error::BuckErrorContext;
use bz_events::dispatch::EventDispatcher;
use bz_events::dispatch::current_span;
use bz_events::dispatch::get_dispatcher;
use bz_events::dispatch::get_dispatcher_opt;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest_config::DigestConfig;
use bz_execute::directory::ActionDirectoryEntry;
use bz_execute::directory::ActionDirectoryMember;
use bz_execute::directory::ActionSharedDirectory;
use bz_execute::execute::blocking::BlockingExecutor;
use bz_execute::materialize::materializer::ArtifactNotMaterializedReason;
use bz_execute::materialize::materializer::CasDownloadInfo;
use bz_execute::materialize::materializer::CasNotFoundError;
use bz_execute::materialize::materializer::CopiedArtifact;
use bz_execute::materialize::materializer::DeclareArtifactPayload;
use bz_execute::materialize::materializer::DeclareMatchOutcome;
use bz_execute::materialize::materializer::DeferredMaterializerExtensions;
use bz_execute::materialize::materializer::EagerMaterializationGuard;
use bz_execute::materialize::materializer::HttpDownloadInfo;
use bz_execute::materialize::materializer::MaterializationError;
use bz_execute::materialize::materializer::Materializer;
use bz_execute::materialize::materializer::WriteRequest;
use bz_execute::re::manager::ReConnectionManager;
use bz_hash::BuckDashMap;
use bz_hash::StdBuckHashSet;
use bz_http::HttpClient;
use bz_util::threads::thread_spawn;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use derivative::Derivative;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use futures::stream::BoxStream;
use parking_lot::RwLock;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::materializers::deferred::artifact_tree::ArtifactTree;
use crate::materializers::deferred::artifact_tree::Version;
use crate::materializers::deferred::clean_stale::CleanStaleConfig;
use crate::materializers::deferred::command_processor::DeferredMaterializerCommandProcessor;
use crate::materializers::deferred::command_processor::LowPriorityMaterializerCommand;
use crate::materializers::deferred::command_processor::MaterializerCommand;
use crate::materializers::deferred::eager_materialization::EagerPathLeases;
use crate::materializers::deferred::file_tree::FileTree;
use crate::materializers::deferred::io_handler::DefaultIoHandler;
use crate::materializers::deferred::io_handler::IoHandler;
use crate::sqlite::materializer_db::MaterializerStateSqliteDbDeferredLoad;

/// Materializer implementation that defers materialization of declared
/// artifacts until they are needed (i.e. `ensure_materialized` is called).
///
/// # Important
///
/// This materializer defers both CAS fetches and local copies. Therefore, one
/// needs to be careful when choosing to call `ensure_materialized`.
/// Between `declare` and `ensure` calls, the local files could have changed.
///
/// This limits us to only "safely" using the materializer within the
/// computation of a build rule, and only to materialize inputs or outputs of
/// the rule, not random artifacts/paths. That's because:
/// - file changes before/after a build are handled by DICE, which invalidates
///   the outputs that depend on it. The materializer ends up having the wrong
///   information about these outputs. But because it's only used within the
///   build rules, the affected rule is recomputed and therefore has its
///   artifacts re-declared. So when `ensure` is called the materializer has
///   up-to-date information about the artifacts.
/// - file changes during a build are not properly supported by Buck and
///   treated as undefined behaviour, so there's no need to worry about them.
#[derive(Allocative)]
pub struct DeferredMaterializerAccessor<T: IoHandler + 'static> {
    /// Sender to emit commands to the command loop. See `MaterializerCommand`.
    #[allocative(skip)]
    command_sender: Arc<MaterializerSender<T>>,
    /// Handle of the command loop thread. Aborted on Drop.
    /// This thread serves as a queue for declare/ensure requests, making
    /// sure only one executes at a time and in the order they came in.
    /// TODO(rafaelc): aim to replace it with a simple mutex.
    #[allocative(skip)]
    #[cfg_attr(not(test), expect(dead_code))]
    command_thread: Option<std::thread::JoinHandle<()>>,
    remote_download_outputs: RemoteDownloadOutputsMode,
    defer_write_actions: bool,
    eager_materialization_enabled: bool,

    io: Arc<T>,

    /// Tracked for logging purposes.
    materializer_state_entries_from_sqlite: Arc<AtomicU64>,

    stats: Arc<DeferredMaterializerStats>,
}

pub type DeferredMaterializer = DeferredMaterializerAccessor<DefaultIoHandler>;

impl<T: IoHandler> Drop for DeferredMaterializerAccessor<T> {
    fn drop(&mut self) {
        // We don't try to stop the underlying thread, since in practice when we drop the
        // DeferredMaterializer we are about to just terminate the process.
    }
}

/// Statistics we collect while operating the Deferred Materializer.
#[derive(Allocative, Default)]
pub struct DeferredMaterializerStats {
    declares: AtomicU64,
    declares_reused: AtomicU64,
}

pub struct DeferredMaterializerConfigs {
    pub remote_download_outputs: RemoteDownloadOutputsMode,
    pub defer_write_actions: bool,
    pub ttl_refresh: TtlRefreshConfiguration,
    pub update_access_times: AccessTimesUpdates,
    pub verbose_materializer_log: bool,
    pub clean_stale_config: Option<CleanStaleConfig>,
    pub disable_eager_write_dispatch: bool,
    pub eager_materialization_enabled: bool,
}

pub struct TtlRefreshConfiguration {
    pub frequency: std::time::Duration,
    pub min_ttl: Duration,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Dupe, PartialEq)]
pub enum AccessTimesUpdates {
    /// Flushes when the buffer is full and periodically
    Full,
    ///Flushes only when buffer is full
    Partial,
    /// Does not flush at all
    Disabled,
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
pub enum AccessTimesUpdatesError {
    #[error(
        "Invalid value for buckconfig `[buck2] update_access_times`. Got `{0}`. Expected one of `full`, `partial`  or `disabled`."
    )]
    InvalidValueForConfig(String),
}

impl AccessTimesUpdates {
    pub fn try_new_from_config_value(config_value: Option<&str>) -> bz_error::Result<Self> {
        match config_value {
            None | Some("") | Some("full") => Ok(AccessTimesUpdates::Full),
            Some("partial") => Ok(AccessTimesUpdates::Partial),
            Some("disabled") => Ok(AccessTimesUpdates::Disabled),
            Some(v) => Err(AccessTimesUpdatesError::InvalidValueForConfig(v.to_owned()).into()),
        }
    }
}

#[derive(Copy, Dupe, Clone)]
struct MaterializerCounters {
    sent: &'static AtomicUsize,
    received: &'static AtomicUsize,
}

impl MaterializerCounters {
    /// New counters. Note that this leaks the underlying data. See comments on MaterializerSender.
    fn leak_new() -> Self {
        Self {
            sent: Box::leak(Box::new(AtomicUsize::new(0))),
            received: Box::leak(Box::new(AtomicUsize::new(0))),
        }
    }

    fn ack_received(&self) {
        self.received.fetch_add(1, Ordering::Relaxed);
    }

    fn queue_size(&self) -> usize {
        self.sent
            .load(Ordering::Relaxed)
            .saturating_sub(self.received.load(Ordering::Relaxed))
    }
}

pub struct MaterializerSender<T: 'static> {
    /// High priority commands are processed in order.
    high_priority: mpsc::UnboundedSender<MaterializerCommand<T>>,
    /// Low priority commands are processed in order relative to each other, but high priority
    /// commands can be reordered ahead of them.
    low_priority: mpsc::UnboundedSender<LowPriorityMaterializerCommand>,
    counters: MaterializerCounters,
    /// Liveliness guard held while clean stale executes, dropped to interrupt clean.
    clean_guard: RwLock<Option<LivelinessGuard>>,
}

impl<T> MaterializerSender<T> {
    #[allow(clippy::result_large_err)]
    fn send(
        &self,
        command: MaterializerCommand<T>,
    ) -> Result<(), mpsc::error::SendError<MaterializerCommand<T>>> {
        {
            let read = self.clean_guard.read();
            if read.is_some() {
                drop(read);
                *self.clean_guard.write() = None;
            }
        }
        let res = self.high_priority.send(command);
        self.counters.sent.fetch_add(1, Ordering::Relaxed);
        res
    }

    fn send_low_priority(
        &self,
        command: LowPriorityMaterializerCommand,
    ) -> Result<(), mpsc::error::SendError<LowPriorityMaterializerCommand>> {
        let res = self.low_priority.send(command);
        self.counters.sent.fetch_add(1, Ordering::Relaxed);
        res
    }
}

struct MaterializerReceiver<T: 'static> {
    high_priority: mpsc::UnboundedReceiver<MaterializerCommand<T>>,
    low_priority: mpsc::UnboundedReceiver<LowPriorityMaterializerCommand>,
    counters: MaterializerCounters,
}

struct TtlRefreshHistoryEntry {
    at: DateTime<Utc>,
    outcome: Option<bz_error::Result<()>>,
}

// NOTE: This doesn't derive `Error` and that's on purpose.  We don't want to make it easy (or
// possible, in fact) to add  `context` to this SharedProcessingError and lose the variant.
#[derive(Debug, Clone, Dupe)]
pub enum SharedMaterializingError {
    Error(bz_error::Error),
    NotFound(CasNotFoundError),
}

#[derive(bz_error::Error, Debug)]
#[buck2(tag = Tier0)]
pub enum MaterializeEntryError {
    #[error(transparent)]
    Error(bz_error::Error),

    /// The artifact wasn't found. This typically means it expired in the CAS.
    #[error(transparent)]
    NotFound(CasNotFoundError),
}

impl From<bz_error::Error> for MaterializeEntryError {
    fn from(e: bz_error::Error) -> MaterializeEntryError {
        Self::Error(e)
    }
}

impl From<MaterializeEntryError> for SharedMaterializingError {
    fn from(e: MaterializeEntryError) -> SharedMaterializingError {
        match e {
            MaterializeEntryError::Error(e) => Self::Error(e),
            MaterializeEntryError::NotFound(e) => Self::NotFound(e),
        }
    }
}

#[async_trait]
impl<T: IoHandler + Allocative> Materializer for DeferredMaterializerAccessor<T> {
    fn name(&self) -> &str {
        "deferred"
    }

    async fn declare_existing(
        &self,
        artifacts: Vec<DeclareArtifactPayload>,
    ) -> bz_error::Result<()> {
        let cmd = MaterializerCommand::DeclareExisting(
            artifacts,
            current_span(),
            get_dispatcher_opt().map(|d| d.trace_id().dupe()),
        );
        self.command_sender.send(cmd)?;
        Ok(())
    }

    async fn declare_copy_impl(
        &self,
        path: ProjectRelativePathBuf,
        value: ArtifactValue,
        srcs: Vec<CopiedArtifact>,
        configuration_path: Option<ProjectRelativePathBuf>,
    ) -> bz_error::Result<()> {
        // TODO(rafaelc): get rid of this tree; it'd save a lot of memory.
        let mut srcs_tree = FileTree::new();
        for copied_artifact in srcs.iter() {
            let dest = copied_artifact.dest.strip_prefix(&path)?;

            {
                let mut walk = unordered_entry_walk(
                    copied_artifact
                        .dest_entry
                        .as_ref()
                        .map_dir(Directory::as_ref),
                );
                while let Some((path, entry)) = walk.next() {
                    if let DirectoryEntry::Leaf(ActionDirectoryMember::File(..)) = entry {
                        let path = path.get();
                        let dest_iter = dest.iter().chain(path.iter()).map(|f| f.to_owned());
                        let src = copied_artifact.src.join(&path);
                        srcs_tree.insert(dest_iter, src);
                    }
                }
            }
        }
        let cmd = MaterializerCommand::Declare(
            DeclareArtifactPayload {
                path,
                artifact: value,
                configuration_path,
            },
            Box::new(ArtifactMaterializationMethod::LocalCopy(srcs_tree, srcs)),
            get_dispatcher(),
            current_span(),
        );
        self.command_sender.send(cmd)?;
        Ok(())
    }

    async fn declare_cas_many_impl<'a, 'b>(
        &self,
        info: Arc<CasDownloadInfo>,
        artifacts: Vec<DeclareArtifactPayload>,
    ) -> bz_error::Result<()> {
        let materialize_paths = self
            .remote_download_outputs
            .materializes_remote_outputs_eagerly()
            .then(|| artifacts.iter().map(|a| a.path.clone()).collect::<Vec<_>>());

        for a in artifacts {
            let cmd = MaterializerCommand::Declare(
                a,
                Box::new(ArtifactMaterializationMethod::CasDownload { info: info.dupe() }),
                get_dispatcher(),
                current_span(),
            );
            self.command_sender.send(cmd)?;
        }
        if let Some(paths) = materialize_paths {
            self.ensure_materialized(paths).await?;
        }
        Ok(())
    }

    async fn declare_http(
        &self,
        path: ProjectRelativePathBuf,
        info: HttpDownloadInfo,
        configuration_path: Option<ProjectRelativePathBuf>,
    ) -> bz_error::Result<()> {
        let materialize_path = self
            .remote_download_outputs
            .materializes_remote_outputs_eagerly()
            .then(|| path.clone());
        let cmd = MaterializerCommand::Declare(
            DeclareArtifactPayload {
                path,
                artifact: ArtifactValue::file(info.metadata.dupe()),
                configuration_path,
            },
            Box::new(ArtifactMaterializationMethod::HttpDownload { info }),
            get_dispatcher(),
            current_span(),
        );
        self.command_sender.send(cmd)?;
        if let Some(path) = materialize_path {
            self.ensure_materialized(vec![path]).await?;
        }

        Ok(())
    }

    async fn declare_write<'a>(
        &self,
        generate: Box<dyn FnOnce() -> bz_error::Result<Vec<WriteRequest>> + Send + 'a>,
    ) -> bz_error::Result<Vec<ArtifactValue>> {
        if !self.defer_write_actions {
            return self.io.immediate_write(generate).await;
        }

        let contents = generate()?;

        let mut paths = Vec::with_capacity(contents.len());
        let mut configuration_paths = Vec::with_capacity(contents.len());
        let mut values = Vec::with_capacity(contents.len());
        let mut methods = Vec::with_capacity(contents.len());

        for WriteRequest {
            path,
            content,
            is_executable,
            configuration_path,
        } in contents
        {
            let digest = TrackedFileDigest::from_content(
                &content,
                self.io.digest_config().cas_digest_config(),
            );

            let meta = FileMetadata {
                digest,
                is_executable,
            };

            // NOTE: The zstd crate doesn't release extra capacity of its encoding buffer so it's
            // important to do so here (or the compressed Vec is the same capacity as the input!).
            let compressed_data = zstd::bulk::compress(&content, 0)
                .with_buck_error_context(|| format!("Error compressing {} bytes", content.len()))?
                .into_boxed_slice();

            paths.push(path);
            configuration_paths.push(configuration_path);
            values.push(ArtifactValue::file(meta));
            methods.push(ArtifactMaterializationMethod::Write(Arc::new(WriteFile {
                compressed_data,
                decompressed_size: content.len(),
                is_executable,
            })));
        }

        for ((path, cfg_path), (value, method)) in std::iter::zip(
            std::iter::zip(paths, configuration_paths),
            std::iter::zip(values.iter(), methods),
        ) {
            self.command_sender.send(MaterializerCommand::Declare(
                DeclareArtifactPayload {
                    path,
                    artifact: value.dupe(),
                    configuration_path: cfg_path,
                },
                Box::new(method),
                get_dispatcher(),
                current_span(),
            ))?;
        }

        Ok(values)
    }

    async fn declare_match(
        &self,
        artifacts: Vec<(ProjectRelativePathBuf, ArtifactValue)>,
    ) -> bz_error::Result<DeclareMatchOutcome> {
        let (sender, recv) = oneshot::channel();

        self.command_sender
            .send(MaterializerCommand::MatchArtifacts(artifacts, sender))?;

        let is_match = recv
            .await
            .buck_error_context("Recv'ing match future from command thread.")?;

        Ok(is_match.into())
    }

    async fn get_declared_artifact_values(
        &self,
        paths: Vec<ProjectRelativePathBuf>,
    ) -> bz_error::Result<Vec<Option<ArtifactValue>>> {
        let (sender, recv) = oneshot::channel();
        self.command_sender
            .send(MaterializerCommand::GetDeclaredArtifactValues(
                paths, sender,
            ))?;
        Ok(recv.await?)
    }

    async fn get_declared_artifact_values_and_match(
        &self,
        paths: Vec<ProjectRelativePathBuf>,
    ) -> bz_error::Result<(Vec<Option<ArtifactValue>>, DeclareMatchOutcome)> {
        let (sender, recv) = oneshot::channel();
        self.command_sender
            .send(MaterializerCommand::GetDeclaredArtifactValuesAndMatch(
                paths, sender,
            ))?;
        let (values, is_match) = recv.await?;
        Ok((values, is_match.into()))
    }

    async fn has_artifact_at(&self, path: ProjectRelativePathBuf) -> bz_error::Result<bool> {
        let (sender, recv) = oneshot::channel();

        self.command_sender
            .send(MaterializerCommand::HasArtifact(path, sender))?;

        let has_artifact = recv
            .await
            .buck_error_context("Receiving \"has artifact\" future from command thread.")?;

        Ok(has_artifact)
    }

    async fn invalidate_many(&self, paths: Vec<ProjectRelativePathBuf>) -> bz_error::Result<()> {
        let (sender, recv) = oneshot::channel();

        self.command_sender
            .send(MaterializerCommand::InvalidateFilePaths(
                paths,
                sender,
                get_dispatcher(),
                current_span(),
            ))?;

        // Wait on future to finish before invalidation can continue.
        let invalidate_fut = recv.await?;
        invalidate_fut.await
    }

    async fn materialize_many(
        &self,
        artifact_paths: Vec<ProjectRelativePathBuf>,
    ) -> bz_error::Result<BoxStream<'static, Result<(), MaterializationError>>> {
        // TODO: display [materializing] in superconsole
        let (sender, recv) = oneshot::channel();
        self.command_sender
            .send(MaterializerCommand::Ensure(
                artifact_paths,
                get_dispatcher(),
                current_span(),
                sender,
            ))
            .buck_error_context("Sending Ensure() command.")?;
        let materialization_fut = recv
            .await
            .buck_error_context("Receiving materialization future from command thread.")?;
        Ok(materialization_fut)
    }

    async fn try_materialize_final_artifact(
        &self,
        artifact_path: ProjectRelativePathBuf,
    ) -> bz_error::Result<bool> {
        if self.remote_download_outputs.materializes_final_artifacts() {
            self.ensure_materialized(vec![artifact_path]).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn get_materialized_file_paths(
        &self,
        paths: Vec<ProjectRelativePathBuf>,
    ) -> bz_error::Result<Vec<Result<ProjectRelativePathBuf, ArtifactNotMaterializedReason>>>
    {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let (sender, recv) = oneshot::channel();
        self.command_sender
            .send(MaterializerCommand::GetMaterializedFilePaths(paths, sender))?;
        Ok(recv.await?)
    }

    fn as_deferred_materializer_extension(&self) -> Option<&dyn DeferredMaterializerExtensions> {
        Some(self as _)
    }

    fn log_materializer_state(&self, events: &EventDispatcher) {
        events.instant_event(bz_data::MaterializerStateInfo {
            num_entries_from_sqlite: self
                .materializer_state_entries_from_sqlite
                .load(Ordering::Relaxed),
        })
    }

    fn add_snapshot_stats(&self, snapshot: &mut bz_data::Snapshot) {
        snapshot.deferred_materializer_declares = self.stats.declares.load(Ordering::Relaxed);
        snapshot.deferred_materializer_declares_reused =
            self.stats.declares_reused.load(Ordering::Relaxed);
        snapshot.deferred_materializer_queue_size = self.command_sender.counters.queue_size() as _;
    }

    async fn get_artifact_entries_for_materialized_paths(
        &self,
        paths: Vec<ProjectRelativePathBuf>,
        fetch_root_artifact_entries_for_subpaths: bool,
    ) -> bz_error::Result<
        Vec<
            Option<(
                ProjectRelativePathBuf,
                ActionDirectoryEntry<ActionSharedDirectory>,
            )>,
        >,
    > {
        let (sender, recv) = oneshot::channel();

        self.command_sender.send(
            MaterializerCommand::GetArtifactEntriesForMaterializedPaths {
                paths,
                fetch_root_artifact_entries_for_subpaths,
                sender,
            },
        )?;

        let result = recv.await.buck_error_context(
            "Receiving \"artifact entries for materialized paths\" future from command thread.",
        )?;

        Ok(result)
    }

    fn is_eager_materialization_enabled(&self) -> bool {
        self.eager_materialization_enabled
    }

    async fn register_eager_paths(
        &self,
        paths: Vec<ProjectRelativePathBuf>,
        event_dispatcher: EventDispatcher,
    ) -> bz_error::Result<Box<dyn EagerMaterializationGuard>> {
        let (sender, receiver) = oneshot::channel();
        self.command_sender
            .send(MaterializerCommand::RegisterEagerPaths(
                paths,
                event_dispatcher,
                sender,
            ))?;
        let leases = receiver
            .await
            .buck_error_context("No response from materializer")?;
        Ok(Box::new(EagerPathLeases(leases)))
    }
}

impl DeferredMaterializerAccessor<DefaultIoHandler> {
    /// Spawns two threads (`materialization_loop` and `command_loop`).
    /// Creates and returns a new `DeferredMaterializer` that aborts those
    /// threads when dropped.
    pub fn new(
        fs: ProjectRoot,
        digest_config: DigestConfig,
        buck_out_path: ProjectRelativePathBuf,
        re_client_manager: Arc<ReConnectionManager>,
        io_executor: Arc<dyn BlockingExecutor>,
        configs: DeferredMaterializerConfigs,
        sqlite_db: Option<MaterializerStateSqliteDbDeferredLoad>,
        http_client: HttpClient,
        daemon_dispatcher: EventDispatcher,
    ) -> bz_error::Result<Self> {
        let (high_priority_sender, high_priority_receiver) = mpsc::unbounded_channel();
        let (low_priority_sender, low_priority_receiver) = mpsc::unbounded_channel();

        let counters = MaterializerCounters::leak_new();

        let command_sender = Arc::new(MaterializerSender {
            high_priority: high_priority_sender,
            low_priority: low_priority_sender,
            counters,
            clean_guard: RwLock::new(None),
        });

        let command_receiver = MaterializerReceiver {
            high_priority: high_priority_receiver,
            low_priority: low_priority_receiver,
            counters,
        };

        let stats = Arc::new(DeferredMaterializerStats::default());

        let materializer_state_entries_from_sqlite = Arc::new(AtomicU64::new(0));
        let access_times_buffer =
            (!matches!(configs.update_access_times, AccessTimesUpdates::Disabled))
                .then(StdBuckHashSet::new);

        let declared_artifact_values = Arc::new(BuckDashMap::default());

        let io = Arc::new(DefaultIoHandler::new(
            fs,
            digest_config,
            buck_out_path,
            re_client_manager,
            io_executor,
            http_client,
        ));

        let command_processor = {
            let command_sender = command_sender.dupe();
            let declared_artifact_values = declared_artifact_values.dupe();
            let rt = Handle::current();
            let stats = stats.dupe();
            let io = io.dupe();
            let materializer_state_entries_from_sqlite =
                materializer_state_entries_from_sqlite.dupe();
            move |cancellations| -> bz_error::Result<_> {
                let (sqlite_db, sqlite_state, declared_cas_state) = match sqlite_db {
                    Some(sqlite_db) => {
                        let (sqlite_db, persisted_state) = sqlite_db.load()?;
                        let persisted_state = persisted_state.ok();
                        let (sqlite_state, declared_cas_state) = persisted_state
                            .map(|state| (Some(state.materialized), Some(state.declared_cas)))
                            .unwrap_or((None, None));

                        let num_entries_from_sqlite = sqlite_state.as_ref().map_or(0, |s| s.len())
                            + declared_cas_state.as_ref().map_or(0, |s| s.len());
                        materializer_state_entries_from_sqlite
                            .store(num_entries_from_sqlite as u64, Ordering::Relaxed);

                        if let Some(sqlite_state) = &sqlite_state {
                            for entry in sqlite_state {
                                declared_artifact_values.insert(
                                    entry.path.clone(),
                                    ArtifactValue::new(entry.metadata.dupe(), None),
                                );
                            }
                        }
                        if let Some(declared_cas_state) = &declared_cas_state {
                            for entry in declared_cas_state {
                                declared_artifact_values.insert(
                                    entry.path.clone(),
                                    ArtifactValue::new(entry.metadata.dupe(), None),
                                );
                            }
                        }

                        (Some(sqlite_db), sqlite_state, declared_cas_state)
                    }
                    None => (None, None, None),
                };

                let tree = ArtifactTree::initialize(sqlite_state, declared_cas_state);

                Ok(DeferredMaterializerCommandProcessor::new(
                    io,
                    sqlite_db,
                    rt,
                    configs.defer_write_actions,
                    command_sender,
                    tree,
                    declared_artifact_values,
                    cancellations,
                    stats,
                    access_times_buffer,
                    configs.verbose_materializer_log,
                    daemon_dispatcher,
                    configs.disable_eager_write_dispatch,
                ))
            }
        };

        let command_thread = thread_spawn("buck2-dm", {
            move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let cancellations = CancellationContext::never_cancelled();

                let command_processor = match command_processor(cancellations) {
                    Ok(command_processor) => command_processor,
                    Err(e) => {
                        tracing::error!("Error initializing deferred materializer: {e:#}");
                        return;
                    }
                };

                rt.block_on(command_processor.run(
                    command_receiver,
                    configs.ttl_refresh,
                    configs.update_access_times,
                    configs.clean_stale_config,
                ));
            }
        })
        .buck_error_context("Cannot start materializer thread")?;

        Ok(Self {
            command_thread: Some(command_thread),
            command_sender,
            remote_download_outputs: configs.remote_download_outputs,
            defer_write_actions: configs.defer_write_actions,
            eager_materialization_enabled: configs.eager_materialization_enabled,
            io,
            materializer_state_entries_from_sqlite,
            stats,
        })
    }
}

/// Wait on all futures in `futs` to finish. Return Error for first future that failed
/// in the Vec.
async fn join_all_existing_futs(
    existing_futs: Vec<(ProjectRelativePathBuf, ProcessingFuture)>,
) -> bz_error::Result<()> {
    // We can await inside a loop here because all ProcessingFuture's are spawned.
    for (path, fut) in existing_futs.into_iter() {
        match fut {
            ProcessingFuture::Materializing(f) => {
                // We don't care about errors from previous materializations.
                // We are trying to delete anything that has been materialized,
                // so these errors can be ignored.
                f.await.ok();
            }
            ProcessingFuture::Cleaning(f) => {
                f.await.with_buck_error_context(|| {
                    format!(
                        "Error waiting for a previous future to finish cleaning output path {path}"
                    )
                })?;
            }
        };
    }

    Ok(())
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct WriteFile {
    #[derivative(Debug = "ignore")]
    compressed_data: Box<[u8]>,
    decompressed_size: usize,
    is_executable: bool,
}
