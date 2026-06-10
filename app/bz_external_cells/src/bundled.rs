/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::env;
use std::fs;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use bz_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use bz_common::cas_digest::CasDigestConfig;
use bz_common::dice::data::HasIoProvider;
use bz_common::file_ops::delegate::FileOpsDelegate;
use bz_common::file_ops::dice::ReadFileProxy;
use bz_common::file_ops::metadata::FileMetadata;
use bz_common::file_ops::metadata::FileType;
use bz_common::file_ops::metadata::RawDirEntry;
use bz_common::file_ops::metadata::RawPathMetadata;
use bz_common::file_ops::metadata::RawPathMetadataForNoWatchFs;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_common::io::IoProvider;
use bz_common::io::NoWatchFsMetadataCache;
use bz_common::io::fs::is_executable;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::name::CellName;
use bz_core::cells::paths::CellRelativePath;
use bz_core::cells::paths::CellRelativePathBuf;
use bz_core::directory_digest::DirectoryDigest;
use bz_core::fs::buck_out_path::BuckOutPathResolver;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_directory::directory::builder::DirectoryBuilder;
use bz_directory::directory::directory::Directory;
use bz_directory::directory::directory_hasher::DirectoryDigester;
use bz_directory::directory::directory_iterator::DirectoryIterator;
use bz_directory::directory::directory_ref::DirectoryRef;
use bz_directory::directory::directory_ref::FingerprintedDirectoryRef;
use bz_directory::directory::entry::DirectoryEntry;
use bz_directory::directory::find::DirectoryFindError;
use bz_directory::directory::find::find;
use bz_directory::directory::immutable_directory::ImmutableDirectory;
use bz_error::BuckErrorContext;
use bz_error::bz_error;
use bz_error::conversion::from_any_with_tag;
use bz_error::internal_error;
use bz_execute::digest_config::DigestConfig;
use bz_execute::digest_config::HasDigestConfig;
use bz_execute::materialize::materializer::HasMaterializer;
use bz_execute::materialize::materializer::WriteRequest;
use bz_external_cells_bundled::BundledCell;
use bz_external_cells_bundled::BundledFile;
use bz_external_cells_bundled::get_bundled_data;
use bz_fs::error::IoResultExt;
use bz_fs::fs_util;
use bz_fs::paths::abs_path::AbsPathBuf;
use bz_fs::paths::file_name::FileName;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_util::strong_hasher::Blake3StrongHasher;
use cmp_any::PartialEqAny;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use pagable::Pagable;
use pagable::PagablePanic;
use pagable::pagable_typetag;

fn load_nano_prelude() -> bz_error::Result<BundledCell> {
    let path = env::var("NANO_PRELUDE")
        .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Input))
        .buck_error_context(
            "NANO_PRELUDE env var must be set to the location of nano prelude\n\
        Consider `export NANO_PRELUDE=$HOME/fbsource/fbcode/buck2/tests/e2e_util/nano_prelude`",
        )?;
    if path.is_empty() {
        return Err(bz_error!(
            bz_error::ErrorTag::Input,
            "NANO_PRELUDE env var must not be empty"
        ));
    }
    let path = AbsPathBuf::new(Path::new(&path))
        .buck_error_context("NANO_PRELUDE env var must point to absolute path")?;

    let mut files = Vec::new();
    let mut dir_stack = Vec::new();
    dir_stack.push((path, ForwardRelativePathBuf::empty()));
    while let Some((dir, rel_path)) = dir_stack.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let entry_path = AbsPathBuf::new(entry.path())?;
            let entry_rel_path = rel_path.join(FileName::new(
                entry
                    .file_name()
                    .to_str()
                    .ok_or_else(|| internal_error!("not UTF-8 string"))?,
            )?);
            match FileType::from(entry.file_type()?) {
                FileType::Directory => dir_stack.push((entry_path, entry_rel_path)),
                FileType::File => {
                    let contents = fs_util::read(&entry_path).categorize_internal()?;
                    files.push(BundledFile {
                        path: entry_rel_path.as_str().to_owned().leak(),
                        contents: contents.leak(),
                        is_executable: is_executable(&entry.metadata()?),
                    });
                }
                FileType::Symlink | FileType::Unknown => {
                    // We don't have these in nano-prelude.
                }
            }
        }
    }

    Ok(BundledCell {
        name: "nano_prelude",
        files: files.leak(),
        is_testing: true,
    })
}

fn nano_prelude() -> bz_error::Result<BundledCell> {
    static NANO_PRELUDE: OnceLock<BundledCell> = OnceLock::new();
    Ok(*NANO_PRELUDE
        .get_or_try_init(|| load_nano_prelude().buck_error_context("loading nano_prelude"))?)
}

pub(crate) fn find_bundled_data(cell_name: CellName) -> bz_error::Result<BundledCell> {
    #[derive(bz_error::Error, Debug)]
    #[error("No bundled cell named `{0}`, options are `{}`", _1.join(", "))]
    #[buck2(tag = Input)]
    struct CellNotBundled(String, Vec<&'static str>);

    let cell_name = cell_name.as_str();

    if cell_name == "nano_prelude" {
        return nano_prelude();
    }

    get_bundled_data()
        .iter()
        .find(|data| data.name == cell_name)
        .copied()
        .ok_or_else(|| {
            CellNotBundled(
                cell_name.to_owned(),
                get_bundled_data()
                    .iter()
                    .filter(|data| !data.is_testing)
                    .map(|data| data.name)
                    .collect(),
            )
            .into()
        })
}

#[derive(Clone, PartialEq, Eq, Debug, allocative::Allocative)]
struct ContentsAndMetadata {
    contents: &'static [u8],
    is_executable: bool,
}

impl ContentsAndMetadata {
    fn metadata(&self, source_digest_config: CasDigestConfig) -> FileMetadata {
        FileMetadata {
            digest: TrackedFileDigest::from_content(self.contents, source_digest_config),
            is_executable: self.is_executable,
        }
    }
}

impl Hash for ContentsAndMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.contents.len().hash(state);
        self.is_executable.hash(state);
    }
}

/// We don't actually need the directory digest, but unfortunately the directory tooling kind of
/// requires us to have one.
#[derive(
    allocative::Allocative,
    derive_more::Display,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Copy,
    Clone
)]
struct BundledDirectoryDigest(#[allocative(skip)] blake3::Hash);

impl Dupe for BundledDirectoryDigest {
    fn dupe(&self) -> Self {
        *self
    }
}

impl DirectoryDigest for BundledDirectoryDigest {}

struct BundledDirectoryDigester;

impl DirectoryDigester<ContentsAndMetadata, BundledDirectoryDigest> for BundledDirectoryDigester {
    fn hash_entries<'a, D, I>(&self, entries: I) -> BundledDirectoryDigest
    where
        I: IntoIterator<Item = (&'a FileName, DirectoryEntry<D, &'a ContentsAndMetadata>)>,
        D: FingerprintedDirectoryRef<
                'a,
                Leaf = ContentsAndMetadata,
                DirectoryDigest = BundledDirectoryDigest,
            > + 'a,
        Self: Sized,
    {
        let mut hasher = Blake3StrongHasher::default();
        for (name, entry) in entries {
            name.hash(&mut hasher);
            match entry {
                DirectoryEntry::Dir(dir) => {
                    dir.as_fingerprinted_dyn().fingerprint().hash(&mut hasher);
                }
                DirectoryEntry::Leaf(leaf) => {
                    leaf.hash(&mut hasher);
                }
            }
        }
        BundledDirectoryDigest(hasher.finalize())
    }

    fn leaf_size(&self, leaf: &ContentsAndMetadata) -> u64 {
        leaf.contents.len() as u64
    }
}

#[derive(allocative::Allocative, PagablePanic)]
pub(crate) struct BundledFileOpsDelegate {
    cell: CellName,
    buck_out_resolver: BuckOutPathResolver,
    source_digest_config: CasDigestConfig,
    dir: ImmutableDirectory<ContentsAndMetadata, BundledDirectoryDigest>,
}

#[derive(bz_error::Error, Debug)]
#[buck2(tag = Environment)]
enum BundledPathSearchError {
    #[error("Expected a directory at `{0}` but found a file")]
    ExpectedDirectory(String),
    #[error("Path not found: `{0}`")]
    MissingFile(CellRelativePathBuf),
    #[error("Expected file at `{0}` but found a directory")]
    ExpectedFile(CellRelativePathBuf),
}

impl BundledFileOpsDelegate {
    fn resolve(&self, path: &CellRelativePath) -> ProjectRelativePathBuf {
        self.buck_out_resolver
            .resolve_external_cell_source(path, ExternalCellOrigin::Bundled(self.cell))
    }

    fn get_entry_at_path_if_exists(
        &self,
        path: &CellRelativePath,
    ) -> bz_error::Result<
        Option<
            DirectoryEntry<
                impl DirectoryRef<
                    '_,
                    Leaf = ContentsAndMetadata,
                    DirectoryDigest = BundledDirectoryDigest,
                > + use<'_>,
                &ContentsAndMetadata,
            >,
        >,
    > {
        match find(self.dir.as_ref(), path.iter()) {
            Ok(entry) => Ok(entry),
            Err(DirectoryFindError::CannotTraverseLeaf { path }) => {
                Err(BundledPathSearchError::ExpectedDirectory(path.to_string()).into())
            }
        }
    }

    fn get_entry_at_path(
        &self,
        path: &CellRelativePath,
    ) -> bz_error::Result<
        DirectoryEntry<
            impl DirectoryRef<'_, Leaf = ContentsAndMetadata, DirectoryDigest = BundledDirectoryDigest>
            + use<'_>,
            &ContentsAndMetadata,
        >,
    > {
        self.get_entry_at_path_if_exists(path)?
            .ok_or_else(|| BundledPathSearchError::MissingFile(path.to_owned()).into())
    }

    /// Return the list of file outputs, sorted.
    async fn read_dir(&self, path: &CellRelativePath) -> bz_error::Result<Arc<[RawDirEntry]>> {
        let dir = match self.get_entry_at_path(path)? {
            DirectoryEntry::Dir(dir) => dir,
            DirectoryEntry::Leaf(_) => {
                return Err(BundledPathSearchError::ExpectedDirectory(path.to_string()).into());
            }
        };

        let entries = dir
            .entries()
            .map(|(name, entry)| RawDirEntry {
                file_name: name.to_owned().into_inner(),
                file_type: match entry {
                    DirectoryEntry::Leaf(_) => FileType::File,
                    DirectoryEntry::Dir(_) => FileType::Directory,
                },
            })
            .collect();

        Ok(entries)
    }

    fn get_file_at_path_if_exists(
        &self,
        path: &CellRelativePath,
    ) -> bz_error::Result<Option<&ContentsAndMetadata>> {
        match self.get_entry_at_path_if_exists(path)? {
            Some(DirectoryEntry::Leaf(leaf)) => Ok(Some(leaf)),
            Some(DirectoryEntry::Dir(_)) => {
                Err(BundledPathSearchError::ExpectedFile(path.to_owned()).into())
            }
            None => Ok(None),
        }
    }

    fn read_file_if_exists(
        &self,
        path: &CellRelativePath,
    ) -> bz_error::Result<Option<&'static str>> {
        Ok(self
            .get_file_at_path_if_exists(path)?
            .map(|leaf| str::from_utf8(leaf.contents))
            .transpose()?)
    }

    fn read_path_metadata_if_exists(
        &self,
        path: &CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadata>> {
        match self.get_entry_at_path_if_exists(path)? {
            Some(DirectoryEntry::Leaf(leaf)) => Ok(Some(RawPathMetadata::File(
                leaf.metadata(self.source_digest_config),
            ))),
            Some(DirectoryEntry::Dir(_)) => Ok(Some(RawPathMetadata::Directory)),
            None => Ok(None),
        }
    }

    async fn declare_file_source_artifact_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &CellRelativePath,
    ) -> bz_error::Result<()> {
        let Some(leaf) = self.get_file_at_path_if_exists(path)? else {
            return Ok(());
        };

        let project_path = self.resolve(path);
        // Bundled contents are fixed for a given binary, but the declare runs
        // again on every daemon start, and `declare_write` unconditionally
        // unlinks and rewrites the destination. Reading every loaded bundled
        // file back to disk dominated cold daemon startup, so skip the write
        // when the previously materialized copy already matches.
        let abs_path = ctx
            .global_data()
            .get_io_provider()
            .project_root()
            .resolve(&project_path);
        if bundled_file_already_materialized(abs_path.as_path(), leaf.contents, leaf.is_executable)
        {
            return Ok(());
        }
        let contents = leaf.contents;
        let is_executable = leaf.is_executable;
        let materializer = ctx.per_transaction_data().get_materializer();
        materializer
            .declare_write(Box::new(move || {
                Ok(vec![WriteRequest {
                    path: project_path,
                    content: contents.to_vec(),
                    is_executable,
                    configuration_path: None,
                }])
            }))
            .await
            .map(|_| ())
    }
}

fn bundled_file_already_materialized(abs_path: &Path, contents: &[u8], executable: bool) -> bool {
    let Ok(metadata) = fs::symlink_metadata(abs_path) else {
        return false;
    };
    if !metadata.is_file()
        || metadata.len() != contents.len() as u64
        || is_executable(&metadata) != executable
    {
        return false;
    }
    fs::read(abs_path).is_ok_and(|existing| existing.as_slice() == contents)
}

#[pagable_typetag]
#[async_trait::async_trait]
impl FileOpsDelegate for BundledFileOpsDelegate {
    async fn read_file_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<ReadFileProxy> {
        let res = self.read_file_if_exists(path)?;
        Ok(ReadFileProxy::new_with_captures(res, |res| async move {
            Ok(res.map(|s| s.to_owned()))
        }))
    }

    /// Return the list of file outputs, sorted.
    async fn read_dir(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir(path).await
    }

    async fn read_dir_for_no_watchfs_without_dice(
        &self,
        _io_provider: Arc<dyn IoProvider>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Arc<[RawDirEntry]>> {
        self.read_dir(path).await
    }

    async fn read_path_metadata_if_exists(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadata>> {
        let metadata = self.read_path_metadata_if_exists(path)?;
        if matches!(metadata, Some(RawPathMetadata::File(_))) {
            self.declare_file_source_artifact_if_exists(ctx, path)
                .await?;
        }
        Ok(metadata)
    }

    async fn read_path_metadata_if_exists_for_no_watchfs(
        &self,
        ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> bz_error::Result<Option<RawPathMetadata>> {
        let metadata = self.read_path_metadata_if_exists(path)?;
        if matches!(metadata, Some(RawPathMetadata::File(_))) {
            self.declare_file_source_artifact_if_exists(ctx, path)
                .await?;
        }
        Ok(metadata)
    }

    async fn read_path_metadata_for_no_watchfs_if_exists_without_dice(
        &self,
        _io_provider: Arc<dyn IoProvider>,
        path: &'async_trait CellRelativePath,
        _cache: Option<Arc<NoWatchFsMetadataCache>>,
    ) -> bz_error::Result<Option<RawPathMetadataForNoWatchFs>> {
        Ok(self
            .read_path_metadata_if_exists(path)?
            .map(RawPathMetadataForNoWatchFs::from))
    }

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
    }
}

fn get_file_ops_delegate_impl(
    data: BundledCell,
    digest_config: DigestConfig,
    cell: CellName,
    buck_out_resolver: BuckOutPathResolver,
) -> bz_error::Result<BundledFileOpsDelegate> {
    let mut builder: DirectoryBuilder<ContentsAndMetadata, BundledDirectoryDigest> =
        DirectoryBuilder::empty();
    let source_digest_config = digest_config.cas_digest_config().source_files_config();
    for file in data.files {
        let path = ForwardRelativePath::new(file.path)
            .internal_error("non-forward relative bundled path")?;

        builder
            .insert(
                path,
                DirectoryEntry::Leaf(ContentsAndMetadata {
                    contents: file.contents,
                    is_executable: file.is_executable,
                }),
            )
            .internal_error("conflicting bundled source paths")?;
    }
    let builder = builder.fingerprint(&BundledDirectoryDigester);
    Ok(BundledFileOpsDelegate {
        cell,
        buck_out_resolver,
        source_digest_config,
        dir: builder,
    })
}

async fn declare_all_source_artifacts(
    ctx: &mut DiceComputations<'_>,
    cell_name: CellName,
    ops: &BundledFileOpsDelegate,
) -> bz_error::Result<()> {
    let mut requests = Vec::new();
    let artifact_fs = ctx.get_artifact_fs().await?;
    let buck_out_resolver = artifact_fs.buck_out_path_resolver();

    for (path, entry) in ops.dir.unordered_walk_leaves().with_paths() {
        let path = buck_out_resolver.resolve_external_cell_source(
            CellRelativePath::new(path.as_ref()),
            ExternalCellOrigin::Bundled(cell_name),
        );
        requests.push(WriteRequest {
            path,
            content: entry.contents.to_vec(),
            is_executable: entry.is_executable,
            configuration_path: None,
        });
    }

    let materializer = ctx.per_transaction_data().get_materializer();
    materializer
        .declare_write(Box::new(move || Ok(requests)))
        .await
        .map(|_| ())
}

pub(crate) async fn get_file_ops_delegate(
    ctx: &mut DiceComputations<'_>,
    cell_name: CellName,
) -> bz_error::Result<Arc<BundledFileOpsDelegate>> {
    #[derive(
        dupe::Dupe,
        Clone,
        Copy,
        Debug,
        derive_more::Display,
        PartialEq,
        Eq,
        Hash,
        allocative::Allocative,
        Pagable
    )]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct BundledFileOpsDelegateKey(CellName);

    #[async_trait::async_trait]
    impl Key for BundledFileOpsDelegateKey {
        type Value = bz_error::Result<Arc<BundledFileOpsDelegate>>;

        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            _cancellations: &CancellationContext,
        ) -> Self::Value {
            let data = find_bundled_data(self.0)?;
            let artifact_fs = ctx.get_artifact_fs().await?;
            let ops = get_file_ops_delegate_impl(
                data,
                ctx.global_data().get_digest_config(),
                self.0,
                artifact_fs.buck_out_path_resolver().clone(),
            )?;
            Ok(Arc::new(ops))
        }

        fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
            // No need for non-trivial equality, because this has no deps and is never recomputed
            false
        }

        fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
            OkPagableValueSerialize::<Self::Value>::new()
        }
    }

    ctx.compute(&BundledFileOpsDelegateKey(cell_name)).await?
}

pub(crate) async fn materialize_all(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
) -> bz_error::Result<ProjectRelativePathBuf> {
    let artifact_fs = ctx.get_artifact_fs().await?;
    let buck_out_resolver = artifact_fs.buck_out_path_resolver();

    let ops = get_file_ops_delegate(ctx, cell).await?;
    declare_all_source_artifacts(ctx, cell, &ops).await?;
    let materializer = ctx.per_transaction_data().get_materializer();
    let mut paths = Vec::new();
    for (path, _entry) in ops.dir.unordered_walk_leaves().with_paths() {
        let path = buck_out_resolver.resolve_external_cell_source(
            CellRelativePath::new(path.as_ref()),
            ExternalCellOrigin::Bundled(cell),
        );
        paths.push(path);
    }

    materializer.ensure_materialized(paths).await?;
    Ok(buck_out_resolver.resolve_external_cell_source(
        CellRelativePath::unchecked_new(""),
        ExternalCellOrigin::Bundled(cell),
    ))
}

#[cfg(test)]
mod tests {
    use std::assert_matches::assert_matches;

    use super::*;

    fn testing_ops() -> BundledFileOpsDelegate {
        let cell = CellName::testing_new("test_bundled_cell");
        let data = find_bundled_data(cell).unwrap();
        get_file_ops_delegate_impl(
            data,
            DigestConfig::testing_default(),
            cell,
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2".to_owned(),
            )),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_smoke_read() {
        let ops = testing_ops();
        let content = ops
            .read_file_if_exists(CellRelativePath::unchecked_new("dir/src.txt"))
            .unwrap()
            .unwrap();
        let content = if cfg!(windows) {
            // Git may check out files on Windows with \r\n as line separator.
            // We could configure git, but it's more reliable to handle it in the test.
            content.replace("\r\n", "\n")
        } else {
            content.to_owned()
        };
        assert_eq!(content, "foobar\n");
        assert!(
            ops.read_file_if_exists(CellRelativePath::unchecked_new("dir/does_not_exist.txt"))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_executable_bit() {
        let ops = testing_ops();
        assert_matches!(
            ops.read_path_metadata_if_exists(CellRelativePath::unchecked_new("dir/src.txt"))
                .unwrap()
                .unwrap(),
            RawPathMetadata::File(FileMetadata {
                digest: _,
                is_executable: false,
            }),
        );
        assert_matches!(
            ops.read_path_metadata_if_exists(CellRelativePath::unchecked_new("dir/src2.txt"))
                .unwrap()
                .unwrap(),
            RawPathMetadata::File(FileMetadata {
                digest: _,
                is_executable: true,
            }),
        );
    }

    #[tokio::test]
    async fn test_dir_listing() {
        let ops = testing_ops();

        let root = CellRelativePath::unchecked_new("");
        let root_metadata = ops.read_path_metadata_if_exists(root).unwrap().unwrap();
        assert_matches!(root_metadata, RawPathMetadata::Directory);
        let root_entries = ops.read_dir(root).await.unwrap();
        assert!(root_entries.is_sorted());
        assert_eq!(
            &*root_entries,
            &[
                RawDirEntry {
                    file_name: ".buckconfig".into(),
                    file_type: FileType::File
                },
                RawDirEntry {
                    file_name: "BUCK_TREE".into(),
                    file_type: FileType::File
                },
                RawDirEntry {
                    file_name: "dir".into(),
                    file_type: FileType::Directory
                },
            ],
        );

        let dir = CellRelativePath::unchecked_new("dir");
        let dir_metadata = ops.read_path_metadata_if_exists(dir).unwrap().unwrap();
        assert_matches!(dir_metadata, RawPathMetadata::Directory);
        let dir_entries = ops.read_dir(dir).await.unwrap();
        assert!(dir_entries.is_sorted());
        assert_eq!(dir_entries.len(), 5);
    }

    #[test]
    fn test_load_all_bundled_cells() {
        for c in get_bundled_data() {
            let cell = CellName::testing_new(c.name);
            get_file_ops_delegate_impl(
                *c,
                DigestConfig::testing_default(),
                cell,
                BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new(
                    "buck-out/v2".to_owned(),
                )),
            )
            .unwrap();
        }
    }
}
