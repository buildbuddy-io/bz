/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;

use bz_common::sqlite::sqlite_db::SqliteDb;
use bz_common::sqlite::sqlite_db::SqliteIdentity;
use bz_common::sqlite::sqlite_db::SqliteTable;
use bz_common::sqlite::sqlite_db::SqliteTables;
use bz_core::execution_types::executor_config::RemoteExecutorUseCase;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_execute::digest_config::DigestConfig;
use bz_execute::execute::blocking::BlockingExecutor;
use bz_fs::paths::abs_norm_path::AbsNormPath;
use bz_fs::paths::abs_norm_path::AbsNormPathBuf;
use bz_hash::StdBuckHashMap;
use chrono::DateTime;
use chrono::Utc;
use dupe::Dupe;

use crate::materializers::deferred::artifact_tree::ArtifactMetadata;
use crate::sqlite::tables::materializer_state_table::MaterializerStateSqliteTable;

/// Hand-maintained schema version for the materializer state sqlite db.
/// PLEASE bump this version if you are making a breaking change to the
/// materializer state sqlite db schema! If you forget to bump this version,
/// then you can fix forward by bumping the `buck2.sqlite_materializer_state_version`
/// buckconfig in the project root's .buckconfig.
pub const MATERIALIZER_DB_SCHEMA_VERSION: u64 = 10;

#[derive(Debug)]
pub struct MaterializerStateEntry {
    pub path: ProjectRelativePathBuf,
    pub metadata: ArtifactMetadata,
    pub last_access_time: DateTime<Utc>,
}

pub type MaterializerState = Vec<MaterializerStateEntry>;

#[derive(Debug, PartialEq)]
pub struct MaterializerDeclaredCasStateEntry {
    pub path: ProjectRelativePathBuf,
    pub metadata: ArtifactMetadata,
    pub re_use_case: RemoteExecutorUseCase,
}

pub type MaterializerDeclaredCasState = Vec<MaterializerDeclaredCasStateEntry>;

pub struct MaterializerPersistedState {
    pub materialized: MaterializerState,
    pub declared_cas: MaterializerDeclaredCasState,
}

/// Concrete implementation of SqliteTable for MaterializerStateSqliteTable
impl SqliteTable for MaterializerStateSqliteTable {
    fn create_table(&self) -> bz_error::Result<()> {
        MaterializerStateSqliteTable::create_table(self)
    }
}

/// DB that opens the sqlite connection to the materializer state db on disk and
/// holds all the sqlite tables we need for storing/querying materializer state
pub struct MaterializerStateSqliteDb {
    tables: SqliteTables<MaterializerStateSqliteTable>,
    /// A unique ID identifying this particular instance of the database. This will reset when we
    /// recreate it.
    identity: SqliteIdentity,
}

pub struct MaterializerStateSqliteDbDeferredLoad {
    db: MaterializerStateSqliteDb,
    load_error: Option<bz_error::Error>,
    digest_config: DigestConfig,
}

impl SqliteDb for MaterializerStateSqliteDb {
    type StateType = MaterializerState;
    type TableType = MaterializerStateSqliteTable;

    fn new(tables: SqliteTables<Self::TableType>) -> bz_error::Result<Self> {
        let identity = tables.get_identity()?;
        Ok(Self { tables, identity })
    }

    fn open_tables(path: &AbsNormPath) -> bz_error::Result<SqliteTables<Self::TableType>> {
        let connection = SqliteTables::<Self::TableType>::create_connection(path)?;
        let materializer_state_table = MaterializerStateSqliteTable::new(connection.dupe());
        Ok(SqliteTables::new(materializer_state_table, connection))
    }

    fn identity(&self) -> &SqliteIdentity {
        &self.identity
    }
}

impl MaterializerStateSqliteDb {
    /// Given path to the sqlite DB, opens the materializer state DB or creates a new one if the
    /// existing DB is missing or version-incompatible.
    ///
    /// Startup intentionally does not hydrate persisted materialized rows. This matches Bazel's
    /// shape more closely: cached action validation should be driven by action/cache metadata and
    /// per-output checks, not by eagerly loading the full materializer tree into memory.
    ///
    /// Lazy CAS declarations are still restored because they are metadata-only: they let cached
    /// remote outputs remain available as downloadable digests without pretending they exist on
    /// disk.
    pub async fn initialize(
        materializer_state_dir: AbsNormPathBuf,
        versions: StdBuckHashMap<String, String>,
        current_instance_metadata: StdBuckHashMap<String, String>,
        // Using `BlockingExecutor` out of convenience. This function should be called during startup
        // when there's not a lot of I/O so it shouldn't matter.
        io_executor: Arc<dyn BlockingExecutor>,
        digest_config: DigestConfig,
        reject_identity: Option<&SqliteIdentity>,
    ) -> bz_error::Result<(Self, bz_error::Result<MaterializerPersistedState>)> {
        io_executor
            .execute_io_inline(|| {
                Self::defer_load(
                    materializer_state_dir,
                    versions,
                    current_instance_metadata,
                    digest_config,
                    reject_identity,
                )
                .and_then(MaterializerStateSqliteDbDeferredLoad::load)
            })
            .await
    }

    pub fn defer_load(
        materializer_state_dir: AbsNormPathBuf,
        versions: StdBuckHashMap<String, String>,
        current_instance_metadata: StdBuckHashMap<String, String>,
        digest_config: DigestConfig,
        reject_identity: Option<&SqliteIdentity>,
    ) -> bz_error::Result<MaterializerStateSqliteDbDeferredLoad> {
        let reject_identity = reject_identity.cloned();

        match Self::get_sqlite_db(
            &materializer_state_dir,
            &versions,
            current_instance_metadata.clone(),
            reject_identity.as_ref(),
        ) {
            Ok(db) => Ok(MaterializerStateSqliteDbDeferredLoad {
                db,
                load_error: None,
                digest_config,
            }),
            Err(e) => {
                let db = Self::create_sqlite_db(
                    materializer_state_dir.clone(),
                    versions.clone(),
                    current_instance_metadata.clone(),
                )?;
                Ok(MaterializerStateSqliteDbDeferredLoad {
                    db,
                    load_error: Some(e),
                    digest_config,
                })
            }
        }
    }

    /// Internal implementation that handles digest config
    fn initialize_materializer_sqlite_db(
        materializer_state_dir: AbsNormPathBuf,
        versions: StdBuckHashMap<String, String>,
        current_instance_metadata: StdBuckHashMap<String, String>,
        digest_config: DigestConfig,
        reject_identity: Option<&SqliteIdentity>,
    ) -> bz_error::Result<(Self, bz_error::Result<MaterializerPersistedState>)> {
        let reject_identity = reject_identity.cloned();

        match Self::get_sqlite_db(
            &materializer_state_dir,
            &versions,
            current_instance_metadata.clone(),
            reject_identity.as_ref(),
        ) {
            Ok(db) => {
                let materialized = db
                    .tables
                    .domain_table
                    .read_materializer_state(digest_config);
                let declared_cas = db
                    .tables
                    .domain_table
                    .read_declared_cas_state(digest_config);
                match (materialized, declared_cas) {
                    (Ok(materialized), Ok(declared_cas)) => Ok((
                        db,
                        Ok(MaterializerPersistedState {
                            materialized,
                            declared_cas,
                        }),
                    )),
                    (Err(e), _) | (_, Err(e)) => {
                        let state = Self::create_sqlite_db(
                            materializer_state_dir,
                            versions,
                            current_instance_metadata,
                        )?;
                        Ok((state, Err(e)))
                    }
                }
            }
            Err(e) => {
                let state = Self::create_sqlite_db(
                    materializer_state_dir,
                    versions,
                    current_instance_metadata,
                )?;
                Ok((state, Err(e)))
            }
        }
    }

    fn empty_persisted_state() -> MaterializerPersistedState {
        MaterializerPersistedState {
            materialized: Vec::new(),
            declared_cas: Vec::new(),
        }
    }

    fn load_bazel_aligned_state(
        &self,
        digest_config: DigestConfig,
    ) -> bz_error::Result<MaterializerPersistedState> {
        Ok(MaterializerPersistedState {
            materialized: Vec::new(),
            declared_cas: self
                .tables
                .domain_table
                .read_declared_cas_state(digest_config)?,
        })
    }

    pub(crate) fn materializer_state_table(&mut self) -> &MaterializerStateSqliteTable {
        &self.tables.domain_table
    }
}

impl MaterializerStateSqliteDbDeferredLoad {
    pub fn identity(&self) -> &SqliteIdentity {
        self.db.identity()
    }

    pub fn load(
        self,
    ) -> bz_error::Result<(
        MaterializerStateSqliteDb,
        bz_error::Result<MaterializerPersistedState>,
    )> {
        let Self {
            db,
            load_error,
            digest_config,
        } = self;

        if load_error.is_some() {
            return Ok((db, Ok(MaterializerStateSqliteDb::empty_persisted_state())));
        }

        let state = db.load_bazel_aligned_state(digest_config);
        Ok((db, state))
    }
}

#[allow(unused)] // Used by test modules
pub(crate) fn testing_materializer_state_sqlite_db(
    fs: &ProjectRoot,
    versions: StdBuckHashMap<String, String>,
    metadata: StdBuckHashMap<String, String>,
    reject_identity: Option<&SqliteIdentity>,
) -> bz_error::Result<(
    MaterializerStateSqliteDb,
    bz_error::Result<MaterializerPersistedState>,
)> {
    MaterializerStateSqliteDb::initialize_materializer_sqlite_db(
        fs.resolve(ProjectRelativePath::unchecked_new(
            "buck-out/v2/cache/materializer_state",
        )),
        versions,
        metadata,
        DigestConfig::testing_default(),
        reject_identity,
    )
}

#[cfg(test)]
mod tests {

    use assert_matches::assert_matches;
    use bz_common::cas_digest::TrackedCasDigest;
    use bz_common::file_ops::metadata::FileMetadata;
    use bz_common::file_ops::metadata::Symlink;
    use bz_common::file_ops::metadata::TrackedFileDigest;
    use bz_core::fs::project::ProjectRootTemp;
    use bz_directory::directory::builder::DirectoryBuilder;
    use bz_directory::directory::dashmap_directory_interner::DashMapDirectoryInterner;
    use bz_directory::directory::entry::DirectoryEntry;
    use bz_events::daemon_id::DaemonId;
    use bz_execute::directory::ActionDirectoryMember;
    use bz_execute::directory::new_symlink;
    use bz_fs::paths::forward_rel_path::ForwardRelativePath;
    use chrono::TimeZone;
    use itertools::Itertools;
    use parking_lot::Mutex;
    use rusqlite::Connection;

    use super::*;

    fn artifact_metadata_eq(a: &ArtifactMetadata, b: &ArtifactMetadata) -> bool {
        match (a, b) {
            (DirectoryEntry::Dir(d1), DirectoryEntry::Dir(d2)) => {
                d1.fingerprint() == d2.fingerprint()
            }
            (DirectoryEntry::Leaf(l1), DirectoryEntry::Leaf(l2)) => l1 == l2,
            _ => false,
        }
    }

    fn now_seconds() -> DateTime<Utc> {
        Utc.timestamp_opt(Utc::now().timestamp(), 0)
            .single()
            .unwrap()
    }

    #[test]
    fn test_materializer_state_sqlite_table() {
        let digest_config = DigestConfig::testing_default();

        let fs = ProjectRootTemp::new().unwrap();
        let connection = Connection::open(
            fs.path()
                .resolve(ProjectRelativePath::unchecked_new("test.db")),
        )
        .unwrap();
        let table = MaterializerStateSqliteTable::new(Arc::new(Mutex::new(connection)));

        table.create_table().unwrap();

        let shared_directory = {
            let mut builder = DirectoryBuilder::empty();
            // Create the following directory:
            // ├── foo - file
            // ├── bar - empty directory
            // └── baz
            //     └── qux -> ../foo
            {
                let digest =
                    TrackedCasDigest::from_content(b"hello", digest_config.cas_digest_config());
                let metadata = FileMetadata {
                    digest,
                    is_executable: false,
                };
                let file = DirectoryEntry::Leaf(ActionDirectoryMember::File(metadata));
                builder
                    .insert(ForwardRelativePath::unchecked_new("foo"), file)
                    .unwrap();
            }
            {
                let empty_directory = DirectoryEntry::Dir(DirectoryBuilder::empty());
                builder
                    .insert(ForwardRelativePath::unchecked_new("bar"), empty_directory)
                    .unwrap();
            }
            {
                let mut directory_builder = DirectoryBuilder::empty();
                let symlink =
                    ActionDirectoryMember::Symlink(Arc::new(Symlink::new("../foo".into())));
                directory_builder
                    .insert(
                        ForwardRelativePath::unchecked_new("qux"),
                        DirectoryEntry::Leaf(symlink),
                    )
                    .unwrap();
                let directory = DirectoryEntry::Dir(directory_builder);
                builder
                    .insert(ForwardRelativePath::unchecked_new("baz"), directory)
                    .unwrap();
            }
            let interner = DashMapDirectoryInterner::new();
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&interner)
        };
        let file = ActionDirectoryMember::File(FileMetadata {
            digest: TrackedFileDigest::from_content(b"file", digest_config.cas_digest_config()),
            is_executable: false,
        });
        let symlink = new_symlink("foo/bar").unwrap();
        let external_symlink = new_symlink(if cfg!(windows) {
            // Not sure if we actually support any external symlink on windows, but better
            // to just check anyways.
            "C:\\external\\symlink\\to\\artifact"
        } else {
            "/mnt/gvfs/third-party2/zstd/28def025ee38919d509596da7d09e7a5262cbf32/1.4.x/platform010/64091f4/share"
        }).unwrap();
        // We use `new_symlink` as a helper but it can technically create both Symlink and ExternalSymlink.
        // Verify that we have proper symlink and external symlink.
        assert_matches!(symlink, ActionDirectoryMember::Symlink(..));
        assert_matches!(external_symlink, ActionDirectoryMember::ExternalSymlink(..));

        let artifacts = vec![
            MaterializerStateEntry {
                path: ProjectRelativePath::unchecked_new("a").to_owned(),
                metadata: DirectoryEntry::Dir(shared_directory),
                last_access_time: now_seconds(),
            },
            MaterializerStateEntry {
                path: ProjectRelativePath::unchecked_new("b/c").to_owned(),
                metadata: DirectoryEntry::Leaf(file),
                last_access_time: now_seconds(),
            },
            MaterializerStateEntry {
                path: ProjectRelativePath::unchecked_new("d").to_owned(),
                metadata: DirectoryEntry::Leaf(symlink),
                last_access_time: now_seconds(),
            },
            MaterializerStateEntry {
                path: ProjectRelativePath::unchecked_new("e").to_owned(),
                metadata: DirectoryEntry::Leaf(external_symlink),
                last_access_time: now_seconds(),
            },
        ];
        let mut artifacts: StdBuckHashMap<_, _> =
            artifacts.into_iter().map(|x| (x.path.clone(), x)).collect();

        for (path, entry) in artifacts.iter() {
            table
                .insert(path, &entry.metadata, entry.last_access_time)
                .unwrap();
        }

        let check_materializer_state_expected = |state: &MaterializerState,
                                                 artifacts: &StdBuckHashMap<
            ProjectRelativePathBuf,
            MaterializerStateEntry,
        >| {
            let expected_values = artifacts.values().sorted_by_key(|x| x.path.as_str());
            let result_values = state.iter().sorted_by_key(|x| x.path.as_str());
            assert!(expected_values.eq(result_values));
        };

        let state = table.read_materializer_state(digest_config).unwrap();
        check_materializer_state_expected(&state, &artifacts);

        let paths_to_remove = vec![
            ProjectRelativePath::unchecked_new("d").to_owned(),
            ProjectRelativePath::unchecked_new("doesnt/exist").to_owned(),
        ];
        let rows_deleted = table.delete(paths_to_remove.clone()).unwrap();
        assert_eq!(rows_deleted, 1);

        for path in paths_to_remove.iter() {
            artifacts.remove(path);
        }
        let state = table.read_materializer_state(digest_config).unwrap();

        check_materializer_state_expected(&state, &artifacts);
    }

    impl PartialEq for MaterializerStateEntry {
        fn eq(&self, other: &Self) -> bool {
            self.path == other.path
                && self.last_access_time == other.last_access_time
                && artifact_metadata_eq(&self.metadata, &other.metadata)
        }
    }

    #[test]
    fn test_initialize_sqlite_db() -> bz_error::Result<()> {
        fn testing_metadatas() -> Vec<StdBuckHashMap<String, String>> {
            let metadata = bz_events::metadata::collect(&DaemonId::new());
            let mut metadatas = vec![metadata; 5];
            for (i, metadata) in metadatas.iter_mut().enumerate() {
                metadata.insert("version".to_owned(), i.to_string());
            }
            metadatas
        }

        fn assert_metadata_matches(
            mut have: StdBuckHashMap<String, String>,
            want: &StdBuckHashMap<String, String>,
        ) {
            // Remove the key we inject (and check it's there).
            have.remove("timestamp_on_initialization").unwrap();
            assert_eq!(have, *want);
        }

        let digest_config = DigestConfig::testing_default();

        let fs = ProjectRootTemp::new()?;

        let path = ProjectRelativePath::unchecked_new("foo").to_owned();
        let artifact_metadata = DirectoryEntry::Leaf(ActionDirectoryMember::File(FileMetadata {
            digest: TrackedFileDigest::from_content(b"file", digest_config.cas_digest_config()),
            is_executable: false,
        }));
        let timestamp = now_seconds();
        let metadatas = testing_metadatas();

        let v0 = StdBuckHashMap::from([("version".to_owned(), "0".to_owned())]);
        let v1 = StdBuckHashMap::from([("version".to_owned(), "1".to_owned())]);

        {
            let (mut db, loaded_state) = testing_materializer_state_sqlite_db(
                fs.path(),
                v0.clone(),
                metadatas[0].clone(),
                None,
            )
            .unwrap();
            assert!(loaded_state.is_err());
            if let Err(e) = loaded_state {
                assert!(e.category_key().ends_with("PathDoesNotExist"));
            }
            assert_metadata_matches(db.tables.created_by_table.read_all()?, &metadatas[0]);

            db.materializer_state_table()
                .insert(&path, &artifact_metadata, timestamp)
                .unwrap();
        }

        {
            let (db, loaded_state) =
                testing_materializer_state_sqlite_db(fs.path(), v0, metadatas[1].clone(), None)
                    .unwrap();
            assert_matches!(
                loaded_state,
                Ok(v) => {
                    assert_eq!(v.materialized, vec![MaterializerStateEntry {path: path.clone(), metadata: artifact_metadata.clone(), last_access_time: timestamp}]);
                    assert!(v.declared_cas.is_empty());
                }
            );
            assert_metadata_matches(db.tables.created_by_table.read_all()?, &metadatas[0]);
        }

        {
            let (mut db, loaded_state) = testing_materializer_state_sqlite_db(
                fs.path(),
                v1.clone(),
                metadatas[2].clone(),
                None,
            )
            .unwrap();
            assert!(loaded_state.is_err());
            if let Err(e) = loaded_state {
                assert!(e.category_key().ends_with("VersionMismatch"));
            }
            assert_metadata_matches(db.tables.created_by_table.read_all()?, &metadatas[2]);

            db.materializer_state_table()
                .insert(&path, &artifact_metadata, timestamp)
                .unwrap();
        }

        let identity = {
            let (db, loaded_state) = testing_materializer_state_sqlite_db(
                fs.path(),
                v1.clone(),
                metadatas[3].clone(),
                None,
            )
            .unwrap();
            assert_matches!(
                loaded_state,
                Ok(v) => {
                    assert_eq!(v.materialized, vec![MaterializerStateEntry { path, metadata: artifact_metadata, last_access_time: timestamp }]);
                    assert!(v.declared_cas.is_empty());
                }
            );
            assert_metadata_matches(db.tables.created_by_table.read_all()?, &metadatas[2]);

            db.identity().clone()
        };

        {
            let (db, loaded_state) = testing_materializer_state_sqlite_db(
                fs.path(),
                v1,
                metadatas[4].clone(),
                Some(&identity),
            )
            .unwrap();
            assert!(loaded_state.is_err());
            if let Err(e) = loaded_state {
                assert!(e.category_key().ends_with("RejectedIdentity"));
            }
            assert_metadata_matches(db.tables.created_by_table.read_all()?, &metadatas[4]);
        }

        Ok(())
    }
}
