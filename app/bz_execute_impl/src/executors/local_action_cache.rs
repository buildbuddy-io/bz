use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use bz_common::sqlite::sqlite_db::SqliteTable;
use bz_common::sqlite::sqlite_db::SqliteTables;
use bz_core::async_once_cell::AsyncOnceCell;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest_config::DigestConfig;
use bz_execute::execute::action_digest::ActionDigest;
use bz_execute::execute::blocking::BlockingExecutor;
use bz_execute::execute::manager::CommandExecutionManager;
use bz_execute::execute::prepared::PreparedCommand;
use bz_execute::execute::prepared::PreparedCommandOptionalExecutor;
use bz_execute::execute::prepared::UnpreparedCommand;
use bz_execute::execute::request::CommandExecutionOutput;
use bz_execute::execute::result::CommandExecutionResult;
use bz_execute::materialize::materializer::RemoteActionCacheOrigin;
use bz_fs::error::IoResultExt;
use bz_fs::fs_util;
use bz_fs::paths::abs_norm_path::AbsNormPathBuf;
use bz_fs::paths::file_name::FileName;
use bz_hash::BuckDashMap;
use bz_hash::BuckIndexMap;
use chrono::Duration as ChronoDuration;
use chrono::TimeZone;
use chrono::Utc;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::OptionalExtension;

const STATE_TABLE_NAME: &str = "local_action_cache_outputs_v7";
const ACTION_METADATA_TABLE_NAME: &str = "local_action_cache_action_metadata_v7";
const REMOTE_ACTION_CACHE_TABLE_NAME: &str = "local_action_cache_remote_outputs_v7";
const OUTPUT_VALUES_VERSION: u8 = 1;

pub struct LocalActionCache {
    state: LocalActionCacheState,
}

enum LocalActionCacheState {
    Disabled,
    Lazy {
        cache_dir: AbsNormPathBuf,
        io_executor: Arc<dyn BlockingExecutor>,
        digest_config: DigestConfig,
        cache: AsyncOnceCell<LoadedLocalActionCache>,
    },
    #[cfg(test)]
    Loaded(LoadedLocalActionCache),
}

struct LoadedLocalActionCache {
    entries: BuckDashMap<String, LocalActionCacheStoredOutputEntry>,
    action_metadata_entries: BuckDashMap<String, LocalActionCacheStoredEntry>,
    connection: Arc<Mutex<Connection>>,
    digest_config: DigestConfig,
}

#[derive(Clone)]
pub struct LocalActionCacheOutputEntry {
    pub outputs_fingerprint: Arc<[u8]>,
    pub output_values: Arc<[ArtifactValue]>,
    pub remote_cache_entry: bool,
    pub remote_cache_origin: Option<RemoteActionCacheOrigin>,
}

#[derive(Clone)]
pub struct LocalActionCacheEntry {
    pub action_key_digest: Arc<[u8]>,
    pub input_metadata_digest: Arc<[u8]>,
    pub action_fingerprint: Arc<[u8]>,
    pub outputs_fingerprint: Arc<[u8]>,
    pub output_values: Arc<[ArtifactValue]>,
    pub remote_cache_entry: bool,
    pub remote_cache_origin: Option<RemoteActionCacheOrigin>,
}

#[derive(Clone)]
struct LocalActionCacheStoredOutputEntry {
    outputs_fingerprint: Arc<[u8]>,
    output_values: Arc<LocalActionCacheStoredOutputValues>,
    remote_cache_origin: Option<RemoteActionCacheOrigin>,
}

#[derive(Clone)]
struct LocalActionCacheStoredEntry {
    action_key_digest: Arc<[u8]>,
    input_metadata_digest: Arc<[u8]>,
    action_fingerprint: Arc<[u8]>,
    outputs_fingerprint: Arc<[u8]>,
    output_values: Arc<LocalActionCacheStoredOutputValues>,
    remote_cache_origin: Option<RemoteActionCacheOrigin>,
}

struct LocalActionCacheStoredOutputValues {
    serialized: Arc<[u8]>,
    decoded: OnceLock<Arc<[ArtifactValue]>>,
}

impl LocalActionCacheStoredOutputValues {
    fn serialized(serialized: Vec<u8>) -> Self {
        Self {
            serialized: Arc::from(serialized.as_slice()),
            decoded: OnceLock::new(),
        }
    }

    fn decoded(serialized: Vec<u8>, decoded: Arc<[ArtifactValue]>) -> Self {
        let cell = OnceLock::new();
        let _ignored = cell.set(decoded);
        Self {
            serialized: Arc::from(serialized.as_slice()),
            decoded: cell,
        }
    }

    fn get(&self, digest_config: DigestConfig) -> bz_error::Result<Arc<[ArtifactValue]>> {
        if let Some(decoded) = self.decoded.get() {
            return Ok(decoded.clone());
        }

        let decoded: Arc<[ArtifactValue]> =
            deserialize_output_values(&self.serialized, digest_config)?.into();
        let _ignored = self.decoded.set(decoded.clone());
        Ok(self.decoded.get().cloned().unwrap_or(decoded))
    }
}

impl LocalActionCacheStoredOutputEntry {
    fn get(&self, digest_config: DigestConfig) -> bz_error::Result<LocalActionCacheOutputEntry> {
        Ok(LocalActionCacheOutputEntry {
            outputs_fingerprint: self.outputs_fingerprint.clone(),
            output_values: self.output_values.get(digest_config)?,
            remote_cache_entry: self.remote_cache_origin.is_some(),
            remote_cache_origin: self.remote_cache_origin.clone(),
        })
    }
}

impl LocalActionCacheStoredEntry {
    fn get(&self, digest_config: DigestConfig) -> bz_error::Result<LocalActionCacheEntry> {
        Ok(LocalActionCacheEntry {
            action_key_digest: self.action_key_digest.clone(),
            input_metadata_digest: self.input_metadata_digest.clone(),
            action_fingerprint: self.action_fingerprint.clone(),
            outputs_fingerprint: self.outputs_fingerprint.clone(),
            output_values: self.output_values.get(digest_config)?,
            remote_cache_entry: self.remote_cache_origin.is_some(),
            remote_cache_origin: self.remote_cache_origin.clone(),
        })
    }
}

fn serialize_output_values(output_values: &[ArtifactValue]) -> bz_error::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.push(OUTPUT_VALUES_VERSION);
    write_u64(&mut bytes, output_values.len().try_into()?);
    for value in output_values {
        let mut value_bytes = Vec::new();
        value.write_local_action_cache_bytes(&mut value_bytes)?;
        write_bytes(&mut bytes, &value_bytes)?;
    }
    Ok(bytes)
}

pub fn deserialize_output_values(
    bytes: &[u8],
    digest_config: DigestConfig,
) -> bz_error::Result<Vec<ArtifactValue>> {
    let mut reader = OutputValuesReader::new(bytes);
    let version = reader.read_u8()?;
    if version != OUTPUT_VALUES_VERSION {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Tier0,
            "unsupported local action cache output values version `{}`",
            version
        ));
    }

    let len = reader.read_u64()?;
    let mut values = Vec::with_capacity(len.try_into()?);
    for _ in 0..len {
        values.push(ArtifactValue::read_local_action_cache_bytes(
            reader.read_bytes()?,
            digest_config,
        )?);
    }
    reader.expect_eof()?;
    Ok(values)
}

fn remote_origin_from_sqlite(
    action_digest: Option<String>,
    action_instant: Option<i64>,
    ttl_seconds: Option<i64>,
    digest_config: DigestConfig,
) -> bz_error::Result<Option<RemoteActionCacheOrigin>> {
    let (action_digest, action_instant, ttl_seconds) =
        match (action_digest, action_instant, ttl_seconds) {
            (Some(action_digest), Some(action_instant), Some(ttl_seconds)) => {
                (action_digest, action_instant, ttl_seconds)
            }
            (None, None, None) => return Ok(None),
            _ => {
                return Err(internal_error!(
                    "incomplete remote local action cache origin metadata"
                ));
            }
        };

    let (action_digest, _algorithm) =
        ActionDigest::parse_digest(&action_digest, digest_config.cas_digest_config())
            .with_buck_error_context(|| {
                format!("parsing remote local action cache origin digest `{action_digest}`")
            })?;
    let action_instant = Utc
        .timestamp_opt(action_instant, 0)
        .single()
        .ok_or_else(|| {
            internal_error!(
                "invalid remote local action cache origin timestamp `{}`",
                action_instant
            )
        })?;

    Ok(Some(RemoteActionCacheOrigin::new(
        action_digest,
        action_instant,
        ChronoDuration::seconds(ttl_seconds),
    )))
}

fn remote_origin_to_sqlite(
    remote_cache_origin: Option<&RemoteActionCacheOrigin>,
) -> (Option<String>, Option<i64>, Option<i64>) {
    match remote_cache_origin {
        Some(origin) => (
            Some(origin.action_digest().to_string()),
            Some(origin.action_instant().timestamp()),
            Some(origin.ttl().num_seconds()),
        ),
        None => (None, None, None),
    }
}

fn write_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend(value.to_le_bytes());
}

fn write_bytes(bytes: &mut Vec<u8>, value: &[u8]) -> bz_error::Result<()> {
    write_u64(bytes, value.len().try_into()?);
    bytes.extend(value);
    Ok(())
}

struct OutputValuesReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> OutputValuesReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read_exact(&mut self, len: usize) -> bz_error::Result<&'a [u8]> {
        let end = self.position.checked_add(len).ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "local action cache output values length overflow"
            )
        })?;
        if end > self.bytes.len() {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "truncated local action cache output values"
            ));
        }
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn read_u8(&mut self) -> bz_error::Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u64(&mut self) -> bz_error::Result<u64> {
        Ok(u64::from_le_bytes(self.read_exact(8)?.try_into()?))
    }

    fn read_bytes(&mut self) -> bz_error::Result<&'a [u8]> {
        let len: usize = self.read_u64()?.try_into()?;
        self.read_exact(len)
    }

    fn expect_eof(&self) -> bz_error::Result<()> {
        if self.position != self.bytes.len() {
            return Err(bz_error::bz_error!(
                bz_error::ErrorTag::Tier0,
                "trailing data in local action cache output values"
            ));
        }
        Ok(())
    }
}

impl LocalActionCache {
    #[cfg(test)]
    pub(crate) fn testing_new_in_memory() -> bz_error::Result<Self> {
        let connection = Arc::new(Mutex::new(Connection::open_in_memory()?));
        LocalActionCacheSqliteTable::new(connection.dupe()).create_table()?;
        Ok(Self {
            state: LocalActionCacheState::Loaded(LoadedLocalActionCache {
                entries: BuckDashMap::default(),
                action_metadata_entries: BuckDashMap::default(),
                connection,
                digest_config: DigestConfig::testing_default(),
            }),
        })
    }

    pub fn new(
        cache_dir: AbsNormPathBuf,
        io_executor: Arc<dyn BlockingExecutor>,
        digest_config: DigestConfig,
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
                digest_config,
                cache: AsyncOnceCell::new(),
            },
        }
    }

    pub async fn load(&self) -> bz_error::Result<()> {
        match &self.state {
            LocalActionCacheState::Disabled => Ok(()),
            #[cfg(test)]
            LocalActionCacheState::Loaded(_) => Ok(()),
            LocalActionCacheState::Lazy {
                cache_dir,
                io_executor,
                digest_config,
                cache,
            } => {
                let cache_dir = cache_dir.clone();
                let io_executor = io_executor.dupe();
                let digest_config = *digest_config;
                cache
                    .get_or_try_init(async move {
                        tracing::info!("Loading local action cache...");
                        io_executor
                            .execute_io_inline(|| {
                                LoadedLocalActionCache::initialize_blocking(
                                    cache_dir,
                                    digest_config,
                                )
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

    pub fn get(&self, action_digest: &ActionDigest) -> Option<LocalActionCacheOutputEntry> {
        self.loaded()?.get(action_digest)
    }

    pub fn get_action_metadata(&self, key: &str) -> Option<LocalActionCacheEntry> {
        self.loaded()?.get_action_metadata(key)
    }

    pub fn insert(
        &self,
        action_digest: &ActionDigest,
        outputs_fingerprint: Vec<u8>,
        output_values: Arc<[ArtifactValue]>,
    ) -> bz_error::Result<()> {
        self.insert_impl(action_digest, outputs_fingerprint, output_values, None)
    }

    pub fn insert_remote(
        &self,
        action_digest: &ActionDigest,
        outputs_fingerprint: Vec<u8>,
        output_values: Arc<[ArtifactValue]>,
        remote_cache_origin: RemoteActionCacheOrigin,
    ) -> bz_error::Result<()> {
        self.insert_impl(
            action_digest,
            outputs_fingerprint,
            output_values,
            Some(remote_cache_origin),
        )
    }

    fn insert_impl(
        &self,
        action_digest: &ActionDigest,
        outputs_fingerprint: Vec<u8>,
        output_values: Arc<[ArtifactValue]>,
        remote_cache_origin: Option<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.insert(
            action_digest,
            outputs_fingerprint,
            output_values,
            remote_cache_origin,
        )
    }

    pub fn remove(&self, action_digest: &ActionDigest) -> bz_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.remove(action_digest)
    }

    pub fn insert_action_metadata(
        &self,
        key: String,
        action_key_digest: Vec<u8>,
        input_metadata_digest: Vec<u8>,
        action_fingerprint: Vec<u8>,
        outputs_fingerprint: Vec<u8>,
        output_values: Arc<[ArtifactValue]>,
        remote_cache_origin: Option<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.insert_action_metadata(
            key,
            action_key_digest,
            input_metadata_digest,
            action_fingerprint,
            outputs_fingerprint,
            output_values,
            remote_cache_origin,
        )
    }

    pub fn remove_action_metadata(&self, key: &str) -> bz_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.remove_action_metadata(key)
    }

    pub fn remove_remote_entries(&self) -> bz_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.remove_remote_entries()
    }

    pub fn remove_remote_entries_for_origin_action_digests(
        &self,
        origin_action_digests: &[ActionDigest],
    ) -> bz_error::Result<()> {
        let Some(cache) = self.loaded() else {
            return Ok(());
        };
        cache.remove_remote_entries_for_origin_action_digests(origin_action_digests)
    }

    pub async fn clear(&self) -> bz_error::Result<()> {
        match &self.state {
            LocalActionCacheState::Disabled => Ok(()),
            #[cfg(test)]
            LocalActionCacheState::Loaded(cache) => cache.clear(),
            LocalActionCacheState::Lazy {
                cache_dir,
                io_executor,
                cache,
                ..
            } => {
                if let Some(cache) = cache.get() {
                    return cache.clear();
                }

                let cache_dir = cache_dir.clone();
                (io_executor.dupe() as Arc<dyn BlockingExecutor>)
                    .execute_io_inline(move || {
                        if cache_dir.exists() {
                            fs_util::remove_dir_all(&cache_dir).categorize_internal()?;
                        }
                        Ok(())
                    })
                    .await
            }
        }
    }
}

impl LoadedLocalActionCache {
    fn initialize_blocking(
        cache_dir: AbsNormPathBuf,
        digest_config: DigestConfig,
    ) -> bz_error::Result<Self> {
        match Self::open_blocking(&cache_dir, digest_config) {
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
                Self::open_blocking(&cache_dir, digest_config)
            }
        }
    }

    fn open_blocking(
        cache_dir: &AbsNormPathBuf,
        digest_config: DigestConfig,
    ) -> bz_error::Result<Self> {
        fs_util::create_dir_all(cache_dir)?;
        let db_path = cache_dir.join(FileName::unchecked_new("db.sqlite"));
        let connection = SqliteTables::<LocalActionCacheSqliteTable>::create_connection(&db_path)?;
        let table = LocalActionCacheSqliteTable::new(connection.dupe());
        table.create_table()?;
        Ok(Self {
            entries: BuckDashMap::default(),
            action_metadata_entries: BuckDashMap::default(),
            connection,
            digest_config,
        })
    }

    fn get(&self, action_digest: &ActionDigest) -> Option<LocalActionCacheOutputEntry> {
        let key = action_digest.to_string();
        if let Some(entry) = self.entries.get(key.as_str()) {
            return match entry.value().get(self.digest_config) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    tracing::warn!(
                        "Ignoring corrupted local action cache entry `{}`: {}",
                        key,
                        e
                    );
                    None
                }
            };
        }

        let stored = match LocalActionCacheSqliteTable::new(self.connection.dupe())
            .read_action_digest_entry(&key, self.digest_config)
        {
            Ok(Some(entry)) => entry,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!("Error reading local action cache entry `{}`: {}", key, e);
                return None;
            }
        };
        self.entries.insert(key.clone(), stored.clone());
        match stored.get(self.digest_config) {
            Ok(entry) => Some(entry),
            Err(e) => {
                tracing::warn!(
                    "Ignoring corrupted local action cache entry `{}`: {}",
                    key,
                    e
                );
                None
            }
        }
    }

    fn insert(
        &self,
        action_digest: &ActionDigest,
        outputs_fingerprint: Vec<u8>,
        output_values: Arc<[ArtifactValue]>,
        remote_cache_origin: Option<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        let serialized_output_values = serialize_output_values(output_values.as_ref())?;
        let key = action_digest.to_string();
        self.entries.insert(
            key.clone(),
            LocalActionCacheStoredOutputEntry {
                outputs_fingerprint: Arc::from(outputs_fingerprint.as_slice()),
                output_values: Arc::new(LocalActionCacheStoredOutputValues::decoded(
                    serialized_output_values.clone(),
                    output_values,
                )),
                remote_cache_origin: remote_cache_origin.clone(),
            },
        );
        LocalActionCacheSqliteTable::new(self.connection.dupe()).insert_or_replace(
            key,
            outputs_fingerprint,
            serialized_output_values,
            remote_cache_origin,
        )
    }

    fn get_action_metadata(&self, key: &str) -> Option<LocalActionCacheEntry> {
        if let Some(entry) = self.action_metadata_entries.get(key) {
            return match entry.value().get(self.digest_config) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    tracing::warn!(
                        "Ignoring corrupted local action cache metadata entry `{}`: {}",
                        key,
                        e
                    );
                    None
                }
            };
        }

        let stored = match LocalActionCacheSqliteTable::new(self.connection.dupe())
            .read_action_metadata_entry(key, self.digest_config)
        {
            Ok(Some(entry)) => entry,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(
                    "Error reading local action cache metadata entry `{}`: {}",
                    key,
                    e
                );
                return None;
            }
        };
        self.action_metadata_entries
            .insert(key.to_owned(), stored.clone());
        match stored.get(self.digest_config) {
            Ok(entry) => Some(entry),
            Err(e) => {
                tracing::warn!(
                    "Ignoring corrupted local action cache metadata entry `{}`: {}",
                    key,
                    e
                );
                None
            }
        }
    }

    fn insert_action_metadata(
        &self,
        key: String,
        action_key_digest: Vec<u8>,
        input_metadata_digest: Vec<u8>,
        action_fingerprint: Vec<u8>,
        outputs_fingerprint: Vec<u8>,
        output_values: Arc<[ArtifactValue]>,
        remote_cache_origin: Option<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        let serialized_output_values = serialize_output_values(output_values.as_ref())?;
        self.action_metadata_entries.insert(
            key.clone(),
            LocalActionCacheStoredEntry {
                action_key_digest: Arc::from(action_key_digest.as_slice()),
                input_metadata_digest: Arc::from(input_metadata_digest.as_slice()),
                action_fingerprint: Arc::from(action_fingerprint.as_slice()),
                outputs_fingerprint: Arc::from(outputs_fingerprint.as_slice()),
                output_values: Arc::new(LocalActionCacheStoredOutputValues::decoded(
                    serialized_output_values.clone(),
                    output_values,
                )),
                remote_cache_origin: remote_cache_origin.clone(),
            },
        );
        LocalActionCacheSqliteTable::new(self.connection.dupe()).insert_or_replace_action_metadata(
            key,
            action_key_digest,
            input_metadata_digest,
            action_fingerprint,
            outputs_fingerprint,
            serialized_output_values,
            remote_cache_origin,
        )
    }

    fn remove(&self, action_digest: &ActionDigest) -> bz_error::Result<()> {
        let key = action_digest.to_string();
        self.entries.remove(key.as_str());
        LocalActionCacheSqliteTable::new(self.connection.dupe()).delete(key)?;
        Ok(())
    }

    fn remove_action_metadata(&self, key: &str) -> bz_error::Result<()> {
        self.action_metadata_entries.remove(key);
        LocalActionCacheSqliteTable::new(self.connection.dupe())
            .delete_action_metadata(key.to_owned())?;
        Ok(())
    }

    fn clear(&self) -> bz_error::Result<()> {
        self.entries.clear();
        self.action_metadata_entries.clear();
        LocalActionCacheSqliteTable::new(self.connection.dupe()).clear()
    }

    fn remove_remote_entries(&self) -> bz_error::Result<()> {
        let remote_action_digests = self
            .entries
            .iter()
            .filter_map(|entry| {
                entry
                    .value()
                    .remote_cache_origin
                    .is_some()
                    .then(|| entry.key().to_owned())
            })
            .collect::<Vec<_>>();
        for key in remote_action_digests {
            self.entries.remove(&key);
        }

        let remote_metadata_keys = self
            .action_metadata_entries
            .iter()
            .filter_map(|entry| {
                entry
                    .value()
                    .remote_cache_origin
                    .is_some()
                    .then(|| entry.key().to_owned())
            })
            .collect::<Vec<_>>();
        for key in remote_metadata_keys {
            self.action_metadata_entries.remove(&key);
        }

        LocalActionCacheSqliteTable::new(self.connection.dupe()).delete_remote_entries()
    }

    fn remove_remote_entries_for_origin_action_digests(
        &self,
        origin_action_digests: &[ActionDigest],
    ) -> bz_error::Result<()> {
        let remote_action_digests = self
            .entries
            .iter()
            .filter_map(|entry| {
                entry
                    .value()
                    .remote_cache_origin
                    .as_ref()
                    .is_some_and(|origin| origin_action_digests.contains(origin.action_digest()))
                    .then(|| entry.key().to_owned())
            })
            .collect::<Vec<_>>();
        for key in remote_action_digests {
            self.entries.remove(&key);
        }

        let remote_metadata_keys = self
            .action_metadata_entries
            .iter()
            .filter_map(|entry| {
                entry
                    .value()
                    .remote_cache_origin
                    .as_ref()
                    .is_some_and(|origin| origin_action_digests.contains(origin.action_digest()))
                    .then(|| entry.key().to_owned())
            })
            .collect::<Vec<_>>();
        for key in remote_metadata_keys {
            self.action_metadata_entries.remove(&key);
        }

        LocalActionCacheSqliteTable::new(self.connection.dupe())
            .delete_remote_entries_for_origin_action_digests(origin_action_digests)
    }
}

struct LocalActionCacheSqliteTable {
    connection: Arc<Mutex<Connection>>,
}

impl SqliteTable for LocalActionCacheSqliteTable {
    fn create_table(&self) -> bz_error::Result<()> {
        LocalActionCacheSqliteTable::create_table(self)
    }
}

impl LocalActionCacheSqliteTable {
    fn new(connection: Arc<Mutex<Connection>>) -> Self {
        Self { connection }
    }

    fn create_table(&self) -> bz_error::Result<()> {
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {STATE_TABLE_NAME} (
                action_digest       TEXT PRIMARY KEY NOT NULL,
                outputs_fingerprint BLOB NOT NULL,
                output_values       BLOB NOT NULL
            )",
        );
        self.connection
            .lock()
            .execute(&sql, [])
            .with_buck_error_context(|| format!("creating sqlite table {STATE_TABLE_NAME}"))?;
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {ACTION_METADATA_TABLE_NAME} (
                cache_key             TEXT PRIMARY KEY NOT NULL,
                action_key_digest     BLOB NOT NULL,
                input_metadata_digest BLOB NOT NULL,
                action_fingerprint    BLOB NOT NULL,
                outputs_fingerprint   BLOB NOT NULL,
                output_values         BLOB NOT NULL,
                remote_cache_entry    INTEGER NOT NULL,
                origin_action_digest  TEXT NULL,
                origin_action_instant INTEGER NULL,
                origin_ttl_seconds    INTEGER NULL
            )",
        );
        self.connection
            .lock()
            .execute(&sql, [])
            .with_buck_error_context(|| {
                format!("creating sqlite table {ACTION_METADATA_TABLE_NAME}")
            })?;
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {REMOTE_ACTION_CACHE_TABLE_NAME} (
                action_digest          TEXT PRIMARY KEY NOT NULL,
                origin_action_digest   TEXT NOT NULL,
                origin_action_instant  INTEGER NOT NULL,
                origin_ttl_seconds     INTEGER NOT NULL
            )",
        );
        self.connection
            .lock()
            .execute(&sql, [])
            .with_buck_error_context(|| {
                format!("creating sqlite table {REMOTE_ACTION_CACHE_TABLE_NAME}")
            })?;
        Ok(())
    }

    fn read_action_digest_entry(
        &self,
        action_digest: &str,
        digest_config: DigestConfig,
    ) -> bz_error::Result<Option<LocalActionCacheStoredOutputEntry>> {
        let sql = format!(
            "SELECT outputs_fingerprint, output_values FROM {STATE_TABLE_NAME} WHERE action_digest = ?1"
        );
        let connection = self.connection.lock();
        let mut stmt = connection.prepare(&sql)?;
        let stored = stmt
            .query_row([action_digest], |row| {
                let outputs_fingerprint = row.get::<_, Vec<u8>>(0)?;
                let output_values = row.get::<_, Vec<u8>>(1)?;
                Ok((outputs_fingerprint, output_values))
            })
            .optional()?;
        let Some((outputs_fingerprint, output_values)) = stored else {
            return Ok(None);
        };
        drop(stmt);

        let sql = format!(
            "SELECT origin_action_digest, origin_action_instant, origin_ttl_seconds FROM {REMOTE_ACTION_CACHE_TABLE_NAME} WHERE action_digest = ?1"
        );
        let remote_cache_origin = connection
            .query_row(&sql, [action_digest], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .optional()?
            .map(|(action_digest, action_instant, ttl_seconds)| {
                remote_origin_from_sqlite(
                    Some(action_digest),
                    Some(action_instant),
                    Some(ttl_seconds),
                    digest_config,
                )
            })
            .transpose()?
            .flatten();

        Ok(Some(LocalActionCacheStoredOutputEntry {
            outputs_fingerprint: Arc::from(outputs_fingerprint.as_slice()),
            output_values: Arc::new(LocalActionCacheStoredOutputValues::serialized(
                output_values,
            )),
            remote_cache_origin,
        }))
    }

    fn read_action_metadata_entry(
        &self,
        key: &str,
        digest_config: DigestConfig,
    ) -> bz_error::Result<Option<LocalActionCacheStoredEntry>> {
        let sql = format!(
            "SELECT action_key_digest, input_metadata_digest, action_fingerprint, outputs_fingerprint, output_values, remote_cache_entry, origin_action_digest, origin_action_instant, origin_ttl_seconds FROM {ACTION_METADATA_TABLE_NAME} WHERE cache_key = ?1"
        );
        let connection = self.connection.lock();
        let mut stmt = connection.prepare(&sql)?;
        let mut rows = stmt.query_map([key], |row| {
            let action_key_digest = row.get::<_, Vec<u8>>(0)?;
            let input_metadata_digest = row.get::<_, Vec<u8>>(1)?;
            let action_fingerprint = row.get::<_, Vec<u8>>(2)?;
            let outputs_fingerprint = row.get::<_, Vec<u8>>(3)?;
            let output_values = row.get::<_, Vec<u8>>(4)?;
            let remote_cache_entry = row.get::<_, bool>(5)?;
            let origin_action_digest = row.get::<_, Option<String>>(6)?;
            let origin_action_instant = row.get::<_, Option<i64>>(7)?;
            let origin_ttl_seconds = row.get::<_, Option<i64>>(8)?;
            Ok((
                action_key_digest,
                input_metadata_digest,
                action_fingerprint,
                outputs_fingerprint,
                output_values,
                remote_cache_entry,
                origin_action_digest,
                origin_action_instant,
                origin_ttl_seconds,
            ))
        })?;
        match rows.next() {
            Some(row) => {
                let (
                    action_key_digest,
                    input_metadata_digest,
                    action_fingerprint,
                    outputs_fingerprint,
                    output_values,
                    remote_cache_entry,
                    origin_action_digest,
                    origin_action_instant,
                    origin_ttl_seconds,
                ) = row?;
                let remote_cache_origin = remote_origin_from_sqlite(
                    origin_action_digest,
                    origin_action_instant,
                    origin_ttl_seconds,
                    digest_config,
                )?;
                if remote_cache_entry != remote_cache_origin.is_some() {
                    return Err(internal_error!(
                        "inconsistent remote local action cache metadata for `{}`",
                        key
                    ));
                }
                Ok(Some(LocalActionCacheStoredEntry {
                    action_key_digest: Arc::from(action_key_digest.as_slice()),
                    input_metadata_digest: Arc::from(input_metadata_digest.as_slice()),
                    action_fingerprint: Arc::from(action_fingerprint.as_slice()),
                    outputs_fingerprint: Arc::from(outputs_fingerprint.as_slice()),
                    output_values: Arc::new(LocalActionCacheStoredOutputValues::serialized(
                        output_values,
                    )),
                    remote_cache_origin,
                }))
            }
            None => Ok(None),
        }
    }

    fn insert_or_replace(
        &self,
        action_digest: String,
        outputs_fingerprint: Vec<u8>,
        output_values: Vec<u8>,
        remote_cache_origin: Option<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        let sql = format!(
            "INSERT OR REPLACE INTO {STATE_TABLE_NAME} \
                (action_digest, outputs_fingerprint, output_values) VALUES (?1, ?2, ?3)"
        );
        let mut connection = self.connection.lock();
        let tx = connection.transaction()?;
        tx.execute(
            &sql,
            rusqlite::params![&action_digest, outputs_fingerprint, output_values],
        )
        .with_buck_error_context(|| format!("inserting into sqlite table {STATE_TABLE_NAME}"))?;
        if let Some(remote_cache_origin) = remote_cache_origin {
            let (origin_action_digest, origin_action_instant, origin_ttl_seconds) =
                remote_origin_to_sqlite(Some(&remote_cache_origin));
            let sql = format!(
                "INSERT OR REPLACE INTO {REMOTE_ACTION_CACHE_TABLE_NAME} \
                    (action_digest, origin_action_digest, origin_action_instant, origin_ttl_seconds) VALUES (?1, ?2, ?3, ?4)"
            );
            tx.execute(
                &sql,
                rusqlite::params![
                    &action_digest,
                    origin_action_digest,
                    origin_action_instant,
                    origin_ttl_seconds,
                ],
            )
            .with_buck_error_context(|| {
                format!("inserting into sqlite table {REMOTE_ACTION_CACHE_TABLE_NAME}")
            })?;
        } else {
            let sql =
                format!("DELETE FROM {REMOTE_ACTION_CACHE_TABLE_NAME} WHERE action_digest = ?1");
            tx.execute(&sql, rusqlite::params![&action_digest])
                .with_buck_error_context(|| {
                    format!("deleting from sqlite table {REMOTE_ACTION_CACHE_TABLE_NAME}")
                })?;
        }
        tx.commit()?;
        Ok(())
    }

    fn insert_or_replace_action_metadata(
        &self,
        key: String,
        action_key_digest: Vec<u8>,
        input_metadata_digest: Vec<u8>,
        action_fingerprint: Vec<u8>,
        outputs_fingerprint: Vec<u8>,
        output_values: Vec<u8>,
        remote_cache_origin: Option<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        let remote_cache_entry = remote_cache_origin.is_some();
        let (origin_action_digest, origin_action_instant, origin_ttl_seconds) =
            remote_origin_to_sqlite(remote_cache_origin.as_ref());
        let sql = format!(
            "INSERT OR REPLACE INTO {ACTION_METADATA_TABLE_NAME} \
                (cache_key, action_key_digest, input_metadata_digest, action_fingerprint, outputs_fingerprint, output_values, remote_cache_entry, origin_action_digest, origin_action_instant, origin_ttl_seconds) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        );
        self.connection
            .lock()
            .execute(
                &sql,
                rusqlite::params![
                    key,
                    action_key_digest,
                    input_metadata_digest,
                    action_fingerprint,
                    outputs_fingerprint,
                    output_values,
                    remote_cache_entry,
                    origin_action_digest,
                    origin_action_instant,
                    origin_ttl_seconds,
                ],
            )
            .with_buck_error_context(|| {
                format!("inserting into sqlite table {ACTION_METADATA_TABLE_NAME}")
            })?;
        Ok(())
    }

    fn delete(&self, action_digest: String) -> bz_error::Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction()?;
        let sql = format!("DELETE FROM {STATE_TABLE_NAME} WHERE action_digest = ?1");
        tx.execute(&sql, rusqlite::params![&action_digest])
            .with_buck_error_context(|| format!("deleting from sqlite table {STATE_TABLE_NAME}"))?;
        let sql = format!("DELETE FROM {REMOTE_ACTION_CACHE_TABLE_NAME} WHERE action_digest = ?1");
        tx.execute(&sql, rusqlite::params![&action_digest])
            .with_buck_error_context(|| {
                format!("deleting from sqlite table {REMOTE_ACTION_CACHE_TABLE_NAME}")
            })?;
        tx.commit()?;
        Ok(())
    }

    fn delete_action_metadata(&self, key: String) -> bz_error::Result<()> {
        let sql = format!("DELETE FROM {ACTION_METADATA_TABLE_NAME} WHERE cache_key = ?1");
        self.connection
            .lock()
            .execute(&sql, rusqlite::params![key])
            .with_buck_error_context(|| {
                format!("deleting from sqlite table {ACTION_METADATA_TABLE_NAME}")
            })?;
        Ok(())
    }

    fn clear(&self) -> bz_error::Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction()?;
        tx.execute(&format!("DELETE FROM {STATE_TABLE_NAME}"), [])
            .with_buck_error_context(|| format!("clearing sqlite table {STATE_TABLE_NAME}"))?;
        tx.execute(&format!("DELETE FROM {ACTION_METADATA_TABLE_NAME}"), [])
            .with_buck_error_context(|| {
                format!("clearing sqlite table {ACTION_METADATA_TABLE_NAME}")
            })?;
        tx.execute(&format!("DELETE FROM {REMOTE_ACTION_CACHE_TABLE_NAME}"), [])
            .with_buck_error_context(|| {
                format!("clearing sqlite table {REMOTE_ACTION_CACHE_TABLE_NAME}")
            })?;
        tx.commit()?;
        Ok(())
    }

    fn delete_remote_entries(&self) -> bz_error::Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction()?;
        tx.execute(
            &format!(
                "DELETE FROM {STATE_TABLE_NAME} WHERE action_digest IN \
                (SELECT action_digest FROM {REMOTE_ACTION_CACHE_TABLE_NAME})"
            ),
            [],
        )
        .with_buck_error_context(|| {
            format!("deleting remote-backed rows from sqlite table {STATE_TABLE_NAME}")
        })?;
        tx.execute(&format!("DELETE FROM {REMOTE_ACTION_CACHE_TABLE_NAME}"), [])
            .with_buck_error_context(|| {
                format!("clearing sqlite table {REMOTE_ACTION_CACHE_TABLE_NAME}")
            })?;
        tx.execute(
            &format!("DELETE FROM {ACTION_METADATA_TABLE_NAME} WHERE remote_cache_entry = 1"),
            [],
        )
        .with_buck_error_context(|| {
            format!("deleting remote-backed rows from sqlite table {ACTION_METADATA_TABLE_NAME}")
        })?;
        tx.commit()?;
        Ok(())
    }

    fn delete_remote_entries_for_origin_action_digests(
        &self,
        origin_action_digests: &[ActionDigest],
    ) -> bz_error::Result<()> {
        let mut connection = self.connection.lock();
        let tx = connection.transaction()?;
        for origin_action_digest in origin_action_digests {
            let origin_action_digest = origin_action_digest.to_string();
            tx.execute(
                &format!(
                    "DELETE FROM {STATE_TABLE_NAME} WHERE action_digest IN \
                    (SELECT action_digest FROM {REMOTE_ACTION_CACHE_TABLE_NAME} \
                    WHERE origin_action_digest = ?1)"
                ),
                rusqlite::params![&origin_action_digest],
            )
            .with_buck_error_context(|| {
                format!(
                    "deleting remote-backed rows from sqlite table {STATE_TABLE_NAME} for origin action digest"
                )
            })?;
            tx.execute(
                &format!(
                    "DELETE FROM {REMOTE_ACTION_CACHE_TABLE_NAME} WHERE origin_action_digest = ?1"
                ),
                rusqlite::params![&origin_action_digest],
            )
            .with_buck_error_context(|| {
                format!(
                    "deleting rows from sqlite table {REMOTE_ACTION_CACHE_TABLE_NAME} for origin action digest"
                )
            })?;
            tx.execute(
                &format!(
                    "DELETE FROM {ACTION_METADATA_TABLE_NAME} \
                    WHERE remote_cache_entry = 1 AND origin_action_digest = ?1"
                ),
                rusqlite::params![&origin_action_digest],
            )
            .with_buck_error_context(|| {
                format!(
                    "deleting remote-backed rows from sqlite table {ACTION_METADATA_TABLE_NAME} for origin action digest"
                )
            })?;
        }
        tx.commit()?;
        Ok(())
    }
}

pub struct ChainedCommandOptionalExecutor {
    pub first: Arc<dyn PreparedCommandOptionalExecutor>,
    pub second: Arc<dyn PreparedCommandOptionalExecutor>,
}

#[async_trait]
impl PreparedCommandOptionalExecutor for ChainedCommandOptionalExecutor {
    fn insert_unprepared_action_cache_metadata(
        &self,
        local_action_cache_key: &bz_execute::execute::request::LocalActionCacheKey,
        outputs: &BuckIndexMap<CommandExecutionOutput, ArtifactValue>,
        remote_cache_origin: Option<RemoteActionCacheOrigin>,
    ) -> bz_error::Result<()> {
        self.first.insert_unprepared_action_cache_metadata(
            local_action_cache_key,
            outputs,
            remote_cache_origin.clone(),
        )?;
        self.second.insert_unprepared_action_cache_metadata(
            local_action_cache_key,
            outputs,
            remote_cache_origin,
        )
    }

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
) -> bz_error::Result<Vec<u8>> {
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
