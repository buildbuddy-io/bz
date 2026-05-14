/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt::Write;
use std::ops::ControlFlow;
use std::sync::Arc;

use async_trait::async_trait;
use buck2_common::sqlite::sqlite_db::SqliteTable;
use buck2_common::sqlite::sqlite_db::SqliteTables;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_error::BuckErrorContext;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::ActionSharedDirectory;
use buck2_execute::execute::action_digest::ActionDigest;
use buck2_execute::execute::blocking::BlockingExecutor;
use buck2_execute::execute::manager::CommandExecutionManager;
use buck2_execute::execute::prepared::PreparedCommand;
use buck2_execute::execute::prepared::PreparedCommandOptionalExecutor;
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

pub struct LocalActionCache {
    entries: BuckDashMap<String, Arc<[u8]>>,
    connection: Option<Arc<Mutex<Connection>>>,
}

impl LocalActionCache {
    #[cfg(test)]
    pub(crate) fn testing_new_in_memory() -> buck2_error::Result<Self> {
        let connection = Arc::new(Mutex::new(Connection::open_in_memory()?));
        LocalActionCacheSqliteTable::new(connection.dupe()).create_table()?;
        Ok(Self {
            entries: BuckDashMap::default(),
            connection: Some(connection),
        })
    }

    pub async fn initialize(
        cache_dir: AbsNormPathBuf,
        io_executor: Arc<dyn BlockingExecutor>,
        enabled: bool,
    ) -> buck2_error::Result<Self> {
        if !enabled {
            return Ok(Self {
                entries: BuckDashMap::default(),
                connection: None,
            });
        }

        io_executor
            .execute_io_inline(|| Self::initialize_blocking(cache_dir))
            .await
    }

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
        let entries = table.read_all()?;
        Ok(Self {
            entries,
            connection: Some(connection),
        })
    }

    pub fn get(&self, action_digest: &ActionDigest) -> Option<Arc<[u8]>> {
        self.entries
            .get(action_digest.to_string().as_str())
            .map(|entry| entry.dupe())
    }

    pub fn insert(
        &self,
        action_digest: &ActionDigest,
        outputs_fingerprint: Vec<u8>,
    ) -> buck2_error::Result<()> {
        let Some(connection) = &self.connection else {
            return Ok(());
        };

        let key = action_digest.to_string();
        self.entries
            .insert(key.clone(), Arc::from(outputs_fingerprint.as_slice()));
        LocalActionCacheSqliteTable::new(connection.dupe())
            .insert_or_replace(key, outputs_fingerprint)
    }

    pub fn remove(&self, action_digest: &ActionDigest) -> buck2_error::Result<()> {
        let Some(connection) = &self.connection else {
            return Ok(());
        };

        let key = action_digest.to_string();
        self.entries.remove(key.as_str());
        LocalActionCacheSqliteTable::new(connection.dupe()).delete(key)?;
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
        Ok(())
    }

    fn read_all(&self) -> buck2_error::Result<BuckDashMap<String, Arc<[u8]>>> {
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

    fn delete(&self, action_digest: String) -> buck2_error::Result<()> {
        let sql = format!("DELETE FROM {STATE_TABLE_NAME} WHERE action_digest = ?1");
        self.connection
            .lock()
            .execute(&sql, rusqlite::params![action_digest])
            .with_buck_error_context(|| format!("deleting from sqlite table {STATE_TABLE_NAME}"))?;
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
        fingerprint.extend_from_slice(artifact_value_fingerprint(value)?.as_bytes());
        fingerprint.push(b'\n');
    }
    Ok(fingerprint)
}

fn artifact_value_fingerprint(value: &ArtifactValue) -> buck2_error::Result<String> {
    let mut fingerprint = String::new();
    write!(
        &mut fingerprint,
        "entry:{}\0content_hash:{}",
        entry_fingerprint(value.entry())?,
        value.content_based_path_hash().as_str()
    )
    .expect("writing to a string cannot fail");
    if let Some(deps) = value.deps() {
        write!(
            &mut fingerprint,
            "\0deps:{}:{}",
            deps.fingerprint(),
            deps.size()
        )
        .expect("writing to a string cannot fail");
    }
    Ok(fingerprint)
}

fn entry_fingerprint(
    entry: &DirectoryEntry<ActionSharedDirectory, ActionDirectoryMember>,
) -> buck2_error::Result<String> {
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
    Ok(fingerprint)
}
