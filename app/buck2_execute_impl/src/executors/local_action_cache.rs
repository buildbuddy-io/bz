/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::ops::ControlFlow;
use std::sync::Arc;

use async_trait::async_trait;
use buck2_common::sqlite::sqlite_db::SqliteTable;
use buck2_common::sqlite::sqlite_db::SqliteTables;
use buck2_core::async_once_cell::AsyncOnceCell;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_error::BuckErrorContext;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::execute::action_digest::ActionDigest;
use buck2_execute::execute::blocking::BlockingExecutor;
use buck2_execute::execute::manager::CommandExecutionManager;
use buck2_execute::execute::prepared::PreparedCommand;
use buck2_execute::execute::prepared::PreparedCommandOptionalExecutor;
use buck2_execute::execute::prepared::UnpreparedCommand;
use buck2_execute::execute::request::CommandExecutionOutput;
use buck2_execute::execute::result::CommandExecutionResult;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::file_name::FileName;
use buck2_hash::BuckDashMap;
use buck2_hash::BuckIndexMap;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use parking_lot::Mutex;
use rusqlite::Connection;

const STATE_TABLE_NAME: &str = "local_action_cache_v2";
const ACTION_METADATA_TABLE_NAME: &str = "local_action_cache_v3";

pub struct LocalActionCache {
    state: LocalActionCacheState,
}

enum LocalActionCacheState {
    Disabled,
    Lazy {
        cache_dir: AbsNormPathBuf,
        io_executor: Arc<dyn BlockingExecutor>,
        cache: AsyncOnceCell<LoadedLocalActionCache>,
    },
    #[cfg(test)]
    Loaded(LoadedLocalActionCache),
}

struct LoadedLocalActionCache {
    entries: BuckDashMap<String, Arc<[u8]>>,
    action_metadata_entries: BuckDashMap<String, LocalActionCacheEntry>,
    connection: Arc<Mutex<Connection>>,
}

#[derive(Clone)]
pub struct LocalActionCacheEntry {
    pub action_fingerprint: Arc<[u8]>,
    pub outputs_fingerprint: Arc<[u8]>,
}

impl LocalActionCache {
    #[cfg(test)]
    pub(crate) fn testing_new_in_memory() -> buck2_error::Result<Self> {
        let connection = Arc::new(Mutex::new(Connection::open_in_memory()?));
        LocalActionCacheSqliteTable::new(connection.dupe()).create_table()?;
        Ok(Self {
            state: LocalActionCacheState::Loaded(LoadedLocalActionCache {
                entries: BuckDashMap::default(),
                action_metadata_entries: BuckDashMap::default(),
                connection,
            }),
        })
    }

    pub fn new(
        cache_dir: AbsNormPathBuf,
        io_executor: Arc<dyn BlockingExecutor>,
        enabled: bool,
    ) -> Self {
        if !enabled {
            return Self {
                state: LocalActionCacheState::Disabled,
            };
        }

        Self {
            state: LocalActionCacheState::Lazy {
                cache_dir,
                io_executor,
                cache: AsyncOnceCell::new(),
            },
        }
    }

    pub async fn load(&self) -> buck2_error::Result<()> {
        match &self.state {
            LocalActionCacheState::Disabled => Ok(()),
            #[cfg(test)]
            LocalActionCacheState::Loaded(_) => Ok(()),
            LocalActionCacheState::Lazy {
                cache_dir,
                io_executor,
                cache,
            } => {
                let cache_dir = cache_dir.clone();
                let io_executor = io_executor.dupe();
                cache
                    .get_or_try_init(async move {
                        tracing::info!("Loading local action cache...");
                        io_executor
                            .execute_io_inline(|| {
                                LoadedLocalActionCache::initialize_blocking(cache_dir)
                            })
                            .await
                    })
                    .await?;
                Ok(())
            }
        }
    }

    fn loaded(&self) -> Option<&LoadedLocalActionCache> {
        match &self.state {
            LocalActionCacheState::Disabled => None,
            LocalActionCacheState::Lazy { cache, .. } => cache.get(),
            #[cfg(test)]
            LocalActionCacheState::Loaded(cache) => Some(cache),
        }
    }

    pub fn get(&self, action_digest: &ActionDigest) -> Option<Arc<[u8]>> {
        self.loaded()?.get(action_digest)
    }

    pub fn get_action_metadata(&self, key: &str) -> Option<LocalActionCacheEntry> {
        self.loaded()?.get_action_metadata(key)
    }

    pub fn insert(
        &self,
        action_digest: &ActionDigest,
        outputs_fingerprint: Vec<u8>,
    ) -> buck2_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.insert(action_digest, outputs_fingerprint)
    }

    pub fn remove(&self, action_digest: &ActionDigest) -> buck2_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.remove(action_digest)
    }

    pub fn insert_action_metadata(
        &self,
        key: String,
        action_fingerprint: Vec<u8>,
        outputs_fingerprint: Vec<u8>,
    ) -> buck2_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.insert_action_metadata(key, action_fingerprint, outputs_fingerprint)
    }

    pub fn remove_action_metadata(&self, key: &str) -> buck2_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.remove_action_metadata(key)
    }
}

impl LoadedLocalActionCache {
    fn initialize_blocking(cache_dir: AbsNormPathBuf) -> buck2_error::Result<Self> {
        match Self::open_blocking(&cache_dir) {
            Ok(cache) => Ok(cache),
            Err(e) => {
                tracing::warn!(
                    "Discarding local action cache at `{}` after open/read failure: {}",
                    cache_dir,
                    e
                );
                if cache_dir.exists() {
                    fs_util::remove_dir_all(&cache_dir).categorize_internal()?;
                }
                Self::open_blocking(&cache_dir)
            }
        }
    }

    fn open_blocking(cache_dir: &AbsNormPathBuf) -> buck2_error::Result<Self> {
        fs_util::create_dir_all(cache_dir)?;
        let db_path = cache_dir.join(FileName::unchecked_new("db.sqlite"));
        let connection = SqliteTables::<LocalActionCacheSqliteTable>::create_connection(&db_path)?;
        let table = LocalActionCacheSqliteTable::new(connection.dupe());
        table.create_table()?;
        let entries = table.read_all_action_digest_entries()?;
        let action_metadata_entries = table.read_all_action_metadata_entries()?;
        Ok(Self {
            entries,
            action_metadata_entries,
            connection,
        })
    }

    fn get(&self, action_digest: &ActionDigest) -> Option<Arc<[u8]>> {
        self.entries
            .get(action_digest.to_string().as_str())
            .map(|entry| entry.dupe())
    }

    fn insert(
        &self,
        action_digest: &ActionDigest,
        outputs_fingerprint: Vec<u8>,
    ) -> buck2_error::Result<()> {
        let key = action_digest.to_string();
        self.entries
            .insert(key.clone(), Arc::from(outputs_fingerprint.as_slice()));
        LocalActionCacheSqliteTable::new(self.connection.dupe())
            .insert_or_replace(key, outputs_fingerprint)
    }

    fn get_action_metadata(&self, key: &str) -> Option<LocalActionCacheEntry> {
        self.action_metadata_entries
            .get(key)
            .map(|entry| entry.value().clone())
    }

    fn insert_action_metadata(
        &self,
        key: String,
        action_fingerprint: Vec<u8>,
        outputs_fingerprint: Vec<u8>,
    ) -> buck2_error::Result<()> {
        self.action_metadata_entries.insert(
            key.clone(),
            LocalActionCacheEntry {
                action_fingerprint: Arc::from(action_fingerprint.as_slice()),
                outputs_fingerprint: Arc::from(outputs_fingerprint.as_slice()),
            },
        );
        LocalActionCacheSqliteTable::new(self.connection.dupe()).insert_or_replace_action_metadata(
            key,
            action_fingerprint,
            outputs_fingerprint,
        )
    }

    fn remove(&self, action_digest: &ActionDigest) -> buck2_error::Result<()> {
        let key = action_digest.to_string();
        self.entries.remove(key.as_str());
        LocalActionCacheSqliteTable::new(self.connection.dupe()).delete(key)?;
        Ok(())
    }

    fn remove_action_metadata(&self, key: &str) -> buck2_error::Result<()> {
        self.action_metadata_entries.remove(key);
        LocalActionCacheSqliteTable::new(self.connection.dupe())
            .delete_action_metadata(key.to_owned())?;
        Ok(())
    }
}

struct LocalActionCacheSqliteTable {
    connection: Arc<Mutex<Connection>>,
}

impl SqliteTable for LocalActionCacheSqliteTable {
    fn create_table(&self) -> buck2_error::Result<()> {
        LocalActionCacheSqliteTable::create_table(self)
    }
}

impl LocalActionCacheSqliteTable {
    fn new(connection: Arc<Mutex<Connection>>) -> Self {
        Self { connection }
    }

    fn create_table(&self) -> buck2_error::Result<()> {
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {STATE_TABLE_NAME} (
                action_digest       TEXT PRIMARY KEY NOT NULL,
                outputs_fingerprint BLOB NOT NULL
            )",
        );
        self.connection
            .lock()
            .execute(&sql, [])
            .with_buck_error_context(|| format!("creating sqlite table {STATE_TABLE_NAME}"))?;
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {ACTION_METADATA_TABLE_NAME} (
                cache_key           TEXT PRIMARY KEY NOT NULL,
                action_fingerprint  BLOB NOT NULL,
                outputs_fingerprint BLOB NOT NULL
            )",
        );
        self.connection
            .lock()
            .execute(&sql, [])
            .with_buck_error_context(|| {
                format!("creating sqlite table {ACTION_METADATA_TABLE_NAME}")
            })?;
        Ok(())
    }

    fn read_all_action_digest_entries(
        &self,
    ) -> buck2_error::Result<BuckDashMap<String, Arc<[u8]>>> {
        let sql = format!("SELECT action_digest, outputs_fingerprint FROM {STATE_TABLE_NAME}");
        let entries = BuckDashMap::default();
        let connection = self.connection.lock();
        let mut stmt = connection.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (action_digest, outputs_fingerprint) = row?;
            entries.insert(action_digest, Arc::from(outputs_fingerprint.as_slice()));
        }
        Ok(entries)
    }

    fn read_all_action_metadata_entries(
        &self,
    ) -> buck2_error::Result<BuckDashMap<String, LocalActionCacheEntry>> {
        let sql = format!(
            "SELECT cache_key, action_fingerprint, outputs_fingerprint FROM {ACTION_METADATA_TABLE_NAME}"
        );
        let entries = BuckDashMap::default();
        let connection = self.connection.lock();
        let mut stmt = connection.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        for row in rows {
            let (key, action_fingerprint, outputs_fingerprint) = row?;
            entries.insert(
                key,
                LocalActionCacheEntry {
                    action_fingerprint: Arc::from(action_fingerprint.as_slice()),
                    outputs_fingerprint: Arc::from(outputs_fingerprint.as_slice()),
                },
            );
        }
        Ok(entries)
    }

    fn insert_or_replace(
        &self,
        action_digest: String,
        outputs_fingerprint: Vec<u8>,
    ) -> buck2_error::Result<()> {
        let sql = format!(
            "INSERT OR REPLACE INTO {STATE_TABLE_NAME} \
                (action_digest, outputs_fingerprint) VALUES (?1, ?2)"
        );
        self.connection
            .lock()
            .execute(&sql, rusqlite::params![action_digest, outputs_fingerprint])
            .with_buck_error_context(|| {
                format!("inserting into sqlite table {STATE_TABLE_NAME}")
            })?;
        Ok(())
    }

    fn insert_or_replace_action_metadata(
        &self,
        key: String,
        action_fingerprint: Vec<u8>,
        outputs_fingerprint: Vec<u8>,
    ) -> buck2_error::Result<()> {
        let sql = format!(
            "INSERT OR REPLACE INTO {ACTION_METADATA_TABLE_NAME} \
                (cache_key, action_fingerprint, outputs_fingerprint) VALUES (?1, ?2, ?3)"
        );
        self.connection
            .lock()
            .execute(
                &sql,
                rusqlite::params![key, action_fingerprint, outputs_fingerprint],
            )
            .with_buck_error_context(|| {
                format!("inserting into sqlite table {ACTION_METADATA_TABLE_NAME}")
            })?;
        Ok(())
    }

    fn delete(&self, action_digest: String) -> buck2_error::Result<()> {
        let sql = format!("DELETE FROM {STATE_TABLE_NAME} WHERE action_digest = ?1");
        self.connection
            .lock()
            .execute(&sql, rusqlite::params![action_digest])
            .with_buck_error_context(|| format!("deleting from sqlite table {STATE_TABLE_NAME}"))?;
        Ok(())
    }

    fn delete_action_metadata(&self, key: String) -> buck2_error::Result<()> {
        let sql = format!("DELETE FROM {ACTION_METADATA_TABLE_NAME} WHERE cache_key = ?1");
        self.connection
            .lock()
            .execute(&sql, rusqlite::params![key])
            .with_buck_error_context(|| {
                format!("deleting from sqlite table {ACTION_METADATA_TABLE_NAME}")
            })?;
        Ok(())
    }
}

pub struct ChainedCommandOptionalExecutor {
    pub first: Arc<dyn PreparedCommandOptionalExecutor>,
    pub second: Arc<dyn PreparedCommandOptionalExecutor>,
}

#[async_trait]
impl PreparedCommandOptionalExecutor for ChainedCommandOptionalExecutor {
    async fn maybe_execute(
        &self,
        command: &PreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        match self
            .first
            .maybe_execute(command, manager, cancellations)
            .await
        {
            ControlFlow::Break(result) => ControlFlow::Break(result),
            ControlFlow::Continue(manager) => {
                self.second
                    .maybe_execute(command, manager, cancellations)
                    .await
            }
        }
    }

    async fn maybe_execute_unprepared(
        &self,
        command: &UnpreparedCommand<'_, '_>,
        manager: CommandExecutionManager,
        cancellations: &CancellationContext,
    ) -> ControlFlow<CommandExecutionResult, CommandExecutionManager> {
        match self
            .first
            .maybe_execute_unprepared(command, manager, cancellations)
            .await
        {
            ControlFlow::Break(result) => ControlFlow::Break(result),
            ControlFlow::Continue(manager) => {
                self.second
                    .maybe_execute_unprepared(command, manager, cancellations)
                    .await
            }
        }
    }
}

pub(crate) fn local_action_cache_outputs_fingerprint(
    artifact_fs: &ArtifactFs,
    outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
) -> buck2_error::Result<Vec<u8>> {
    let mut fingerprint = Vec::new();
    for (output, value) in outputs {
        let content_hash = value.content_based_path_hash();
        let resolved_path = output
            .as_ref()
            .resolve(artifact_fs, Some(&content_hash))?
            .into_path();
        fingerprint.extend_from_slice(b"output\0");
        fingerprint.extend_from_slice(resolved_path.to_string().as_bytes());
        fingerprint.push(0);
        fingerprint.extend_from_slice(format!("{output:?}").as_bytes());
        fingerprint.push(0);
        fingerprint.extend_from_slice(value.action_cache_fingerprint().as_bytes());
        fingerprint.push(b'\n');
    }
    Ok(fingerprint)
}
