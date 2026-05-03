/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;

use base64::Engine;
use buck2_build_api::actions::artifact::get_artifact_fs::GetArtifactFs;
use buck2_common::dice::data::HasIoProvider;
use buck2_common::file_ops::delegate::FileOpsDelegate;
use buck2_common::file_ops::dice::ReadFileProxy;
use buck2_common::file_ops::metadata::FileDigestConfig;
use buck2_common::file_ops::metadata::RawDirEntry;
use buck2_common::file_ops::metadata::RawPathMetadata;
use buck2_common::http::HasHttpClient;
use buck2_common::io::IoProvider;
use buck2_common::io::fs::FsIoProvider;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::external::BzlmodCellSetup;
use buck2_core::cells::external::BzlmodGeneratedCellGenerator;
use buck2_core::cells::external::BzlmodGeneratedCellSetup;
use buck2_core::cells::external::BzlmodGoRegisterNogoSetup;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePath;
use buck2_core::fs::buck_out_path::BuckOutPathResolver;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_directory::directory::directory::Directory;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_execute::directory::INTERNER;
use buck2_execute::entry::build_entry_from_disk;
use buck2_execute::execute::blocking::HasBlockingExecutor;
use buck2_execute::execute::blocking::IoRequest;
use buck2_execute::materialize::http::Checksum;
use buck2_execute::materialize::http::http_download;
use buck2_execute::materialize::materializer::DeclareArtifactPayload;
use buck2_execute::materialize::materializer::HasMaterializer;
use buck2_execute::materialize::materializer::Materializer;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use cmp_any::PartialEqAny;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use pagable::Pagable;
use pagable::pagable_typetag;

#[derive(buck2_error::Error, Debug)]
#[buck2(tag = Tier0)]
enum BzlmodError {
    #[error("Unsupported bzlmod archive type for `{0}`")]
    UnsupportedArchiveType(String),
    #[error("Error extracting bzlmod module, exit code: {exit_code:?}, stderr:\n{stderr}")]
    ExtractFailed {
        exit_code: ExitStatus,
        stderr: String,
    },
    #[error("Error applying bzlmod patch, exit code: {exit_code:?}, stderr:\n{stderr}")]
    PatchFailed {
        exit_code: ExitStatus,
        stderr: String,
    },
    #[error("Expected extracted bzlmod module directory at `{0}`")]
    MissingExtractedDirectory(String),
    #[error("Expected bzlmod materialization to create a directory")]
    NoDirectory,
    #[error("Invalid bzlmod integrity `{0}`")]
    InvalidIntegrity(String),
}

struct BzlmodExtractIoRequest {
    setup: BzlmodCellSetup,
    archive: ProjectRelativePathBuf,
    patch_files: Vec<ProjectRelativePathBuf>,
    temp: ProjectRelativePathBuf,
    dest: ProjectRelativePathBuf,
}

impl IoRequest for BzlmodExtractIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
        let archive = project_fs.resolve(&self.archive);
        let temp = project_fs.resolve(&self.temp);
        let dest = project_fs.resolve(&self.dest);

        fs_util::create_dir_all(temp.clone())?;
        fs_util::create_dir_all(dest.clone())?;

        extract_archive(&self.setup, &archive, &temp)?;

        let source = match self.setup.strip_prefix.as_ref() {
            Some(strip_prefix) if !strip_prefix.is_empty() => {
                temp.join(ForwardRelativePath::new(&**strip_prefix)?)
            }
            _ => temp.clone(),
        };
        if !source.exists() {
            return Err(BzlmodError::MissingExtractedDirectory(source.to_string()).into());
        }

        copy_dir_contents(&source, &dest)?;

        for patch in &self.patch_files {
            apply_patch(project_fs, &dest, patch, self.setup.patch_strip)?;
        }

        Ok(())
    }
}

struct BzlmodGeneratedIoRequest {
    setup: BzlmodGeneratedCellSetup,
    dest: ProjectRelativePathBuf,
}

impl IoRequest for BzlmodGeneratedIoRequest {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
        let dest = project_fs.resolve(&self.dest);
        fs_util::create_dir_all(dest.clone())?;
        match &self.setup.generator {
            BzlmodGeneratedCellGenerator::GoRegisterNogo(setup) => {
                write_go_register_nogo_repo(&dest, setup)?;
            }
        }
        Ok(())
    }
}

fn write_go_register_nogo_repo(
    dest: &AbsNormPath,
    setup: &BzlmodGoRegisterNogoSetup,
) -> buck2_error::Result<()> {
    let build = format!(
        "package(default_visibility = [\"//visibility:public\"])\n\nalias(\n    name = \"nogo\",\n    actual = \"{}\",\n)\n\nexports_files([\"scope.bzl\"])\n",
        setup.nogo
    );
    fs_util::write(dest.join(ForwardRelativePath::new("BUILD.bazel")?), build)
        .categorize_internal()?;
    let scope = format!(
        "INCLUDES = {}\nEXCLUDES = {}\n",
        scope_list_repr(&setup.includes),
        scope_list_repr(&setup.excludes),
    );
    fs_util::write(dest.join(ForwardRelativePath::new("scope.bzl")?), scope)
        .categorize_internal()?;
    Ok(())
}

fn scope_list_repr(scopes: &[Arc<str>]) -> String {
    if scopes.iter().any(|scope| scope.as_ref() == "all") {
        return "[\"all\"]".to_owned();
    }
    let labels = scopes
        .iter()
        .map(|scope| {
            let scope = scope
                .strip_prefix("@@//")
                .map_or_else(|| scope.to_string(), |rest| format!("root//{rest}"));
            format!("Label({scope:?})")
        })
        .collect::<Vec<_>>();
    format!("[{}]", labels.join(", "))
}

fn extract_archive(
    setup: &BzlmodCellSetup,
    archive: &AbsNormPath,
    temp: &AbsNormPath,
) -> buck2_error::Result<()> {
    let archive_type = setup
        .archive_type
        .as_deref()
        .or_else(|| archive.as_path().extension().and_then(|ext| ext.to_str()))
        .unwrap_or("");

    let mut command = if archive_type == "zip" || setup.url.ends_with(".zip") {
        let mut command = Command::new("unzip");
        command
            .arg("-q")
            .arg(archive.as_path())
            .arg("-d")
            .arg(temp.as_path());
        command
    } else if matches!(
        archive_type,
        "tar" | "gz" | "tgz" | "tar.gz" | "tar.xz" | "tar.bz2"
    ) || setup.url.ends_with(".tar.gz")
        || setup.url.ends_with(".tgz")
        || setup.url.ends_with(".tar.xz")
        || setup.url.ends_with(".tar.bz2")
        || setup.url.ends_with(".tar")
    {
        let mut command = Command::new("tar");
        command
            .arg("-xf")
            .arg(archive.as_path())
            .arg("-C")
            .arg(temp.as_path());
        command
    } else {
        return Err(BzlmodError::UnsupportedArchiveType(setup.url.to_string()).into());
    };

    let output = command
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .buck_error_context("Could not run archive extractor for bzlmod external cell")?;

    if !output.status.success() {
        return Err(BzlmodError::ExtractFailed {
            exit_code: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
        .into());
    }

    Ok(())
}

fn apply_patch(
    project_fs: &ProjectRoot,
    dest: &AbsNormPath,
    patch: &ProjectRelativePath,
    patch_strip: u32,
) -> buck2_error::Result<()> {
    let patch = project_fs.resolve(patch);
    let output = Command::new("patch")
        .current_dir(dest.as_path())
        .arg(format!("-p{patch_strip}"))
        .arg("-i")
        .arg(patch.as_path())
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .buck_error_context("Could not run patch for bzlmod external cell")?;

    if !output.status.success() {
        return Err(BzlmodError::PatchFailed {
            exit_code: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
        .into());
    }

    Ok(())
}

fn copy_dir_contents(from: &AbsNormPath, to: &AbsNormPath) -> buck2_error::Result<()> {
    for entry in fs_util::read_dir(from).categorize_internal()? {
        let entry = entry?;
        let from_path = entry.path();
        let to_path = to.join(ForwardRelativePath::new(&entry.file_name())?);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs_util::create_dir_all(&to_path)?;
            copy_dir_contents(&from_path, &to_path)?;
        } else if file_type.is_file() {
            fs_util::copy(&from_path, &to_path).categorize_internal()?;
        } else if file_type.is_symlink() {
            let target = fs_util::read_link(&from_path).categorize_internal()?;
            fs_util::symlink(target, &to_path).categorize_internal()?;
        }
    }
    Ok(())
}

fn integrity_to_sha256_hex(integrity: &str) -> buck2_error::Result<String> {
    let Some(encoded) = integrity.strip_prefix("sha256-") else {
        return Err(BzlmodError::InvalidIntegrity(integrity.to_owned()).into());
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| BzlmodError::InvalidIntegrity(integrity.to_owned()))?;
    if bytes.len() != 32 {
        return Err(BzlmodError::InvalidIntegrity(integrity.to_owned()).into());
    }
    Ok(hex::encode(bytes))
}

fn bzlmod_path(setup: &BzlmodCellSetup, suffix: &str) -> ProjectRelativePathBuf {
    ProjectRelativePathBuf::unchecked_new(format!(
        "buck-out/v2/external_cells/bzlmod/{}/{}",
        setup.canonical_repo_name, suffix
    ))
}

async fn download_impl(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodCellSetup,
    dest: &ProjectRelativePath,
    materializer: &dyn Materializer,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    let io = ctx.get_blocking_executor();
    let archive = bzlmod_path(setup, "source.archive");
    let temp = bzlmod_path(setup, "extract-tmp");
    let patch_dir = bzlmod_path(setup, "patches");
    let patch_files: Vec<_> = setup
        .patches
        .iter()
        .enumerate()
        .map(|(idx, _)| patch_dir.join(ForwardRelativePath::new(&format!("{idx}.patch")).unwrap()))
        .collect();

    io.execute_io(
        Box::new(
            buck2_execute::execute::clean_output_paths::CleanOutputPaths {
                paths: vec![
                    dest.to_owned(),
                    archive.clone(),
                    temp.clone(),
                    patch_dir.clone(),
                ],
            },
        ),
        cancellations,
    )
    .await?;

    let io_provider = ctx.global_data().get_io_provider();
    let project_root = io_provider.project_root();
    let digest_config = ctx.global_data().get_digest_config();
    let client = ctx.per_transaction_data().get_http_client();
    let archive_checksum = Checksum::new(None, Some(&integrity_to_sha256_hex(&setup.integrity)?))?;
    http_download(
        &client,
        project_root,
        digest_config.dupe(),
        &archive,
        &setup.url,
        &archive_checksum,
        false,
    )
    .await?;

    for (patch, output) in setup.patches.iter().zip(&patch_files) {
        let checksum = Checksum::new(None, Some(&integrity_to_sha256_hex(&patch.integrity)?))?;
        http_download(
            &client,
            project_root,
            digest_config.dupe(),
            output,
            &patch.url,
            &checksum,
            false,
        )
        .await?;
    }

    io.execute_io(
        Box::new(BzlmodExtractIoRequest {
            setup: setup.dupe(),
            archive,
            patch_files,
            temp,
            dest: dest.to_owned(),
        }),
        cancellations,
    )
    .await?;

    declare_existing_directory(ctx, dest, materializer).await
}

async fn declare_existing_directory(
    ctx: &mut DiceComputations<'_>,
    dest: &ProjectRelativePath,
    materializer: &dyn Materializer,
) -> buck2_error::Result<()> {
    let io = ctx.get_blocking_executor();
    let io_provider = ctx.global_data().get_io_provider();
    let project_root = io_provider.project_root();
    let digest_config = ctx.global_data().get_digest_config();
    let proj_root = project_root.root();
    let abs_path = proj_root.join(dest);
    let file_digest_config = FileDigestConfig::build(digest_config.cas_digest_config());
    let entry = build_entry_from_disk(abs_path, file_digest_config, &*io, proj_root)
        .await?
        .0
        .ok_or(BzlmodError::NoDirectory)?;
    let entry = entry.map_dir(|d| {
        d.to_builder()
            .fingerprint(digest_config.as_directory_serializer())
            .shared(&*INTERNER)
    });

    materializer
        .declare_existing(vec![DeclareArtifactPayload {
            path: dest.to_owned(),
            artifact: ArtifactValue::new(entry, None),
            configuration_path: None,
        }])
        .await?;

    Ok(())
}

async fn download_and_materialize(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodCellSetup,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    let materializer = ctx.per_transaction_data().get_materializer();

    if materializer.has_artifact_at(path.to_owned()).await? {
        return Ok(());
    }

    cancellations
        .critical_section(|| download_impl(ctx, setup, path, &*materializer, cancellations))
        .await
}

async fn materialize_generated(
    ctx: &mut DiceComputations<'_>,
    path: &ProjectRelativePath,
    setup: &BzlmodGeneratedCellSetup,
    cancellations: &CancellationContext,
) -> buck2_error::Result<()> {
    let materializer = ctx.per_transaction_data().get_materializer();

    if materializer.has_artifact_at(path.to_owned()).await? {
        return Ok(());
    }

    cancellations
        .critical_section(|| async move {
            ctx.get_blocking_executor()
                .execute_io(
                    Box::new(
                        buck2_execute::execute::clean_output_paths::CleanOutputPaths {
                            paths: vec![path.to_owned()],
                        },
                    ),
                    cancellations,
                )
                .await?;
            ctx.get_blocking_executor()
                .execute_io(
                    Box::new(BzlmodGeneratedIoRequest {
                        setup: setup.dupe(),
                        dest: path.to_owned(),
                    }),
                    cancellations,
                )
                .await?;
            declare_existing_directory(ctx, path, &*materializer).await
        })
        .await
}

#[derive(allocative::Allocative, Pagable)]
pub(crate) struct BzlmodFileOpsDelegate {
    buck_out_resolver: BuckOutPathResolver,
    cell: CellName,
    setup: BzlmodCellSetup,
    io: FsIoProvider,
}

impl BzlmodFileOpsDelegate {
    fn resolve(&self, path: &CellRelativePath) -> ProjectRelativePathBuf {
        self.buck_out_resolver
            .resolve_external_cell_source(path, ExternalCellOrigin::Bzlmod(self.setup.dupe()))
    }

    fn get_base_path(&self) -> ProjectRelativePathBuf {
        self.resolve(CellRelativePath::empty())
    }
}

#[pagable_typetag]
#[async_trait::async_trait]
impl FileOpsDelegate for BzlmodFileOpsDelegate {
    async fn read_file_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<ReadFileProxy> {
        Ok(ReadFileProxy::new_with_captures(
            (self.resolve(path), self.io.dupe()),
            |(project_path, io)| async move {
                (&io as &dyn IoProvider)
                    .read_file_if_exists(project_path)
                    .await
            },
        ))
    }

    async fn read_dir(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        let project_path = self.resolve(path);
        let mut entries = (&self.io as &dyn IoProvider)
            .read_dir(project_path)
            .await
            .with_buck_error_context(|| format!("Error listing dir `{path}`"))?;

        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(entries.into())
    }

    async fn read_path_metadata_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
        let project_path = self.resolve(path);
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists(project_path)
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        Ok(Some(metadata.try_map(
            |path| match path.strip_prefix_opt(self.get_base_path()) {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    path,
                    self.cell
                )),
            },
        )?))
    }

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
    }
}

#[derive(allocative::Allocative, Pagable)]
pub(crate) struct BzlmodGeneratedFileOpsDelegate {
    buck_out_resolver: BuckOutPathResolver,
    cell: CellName,
    setup: BzlmodGeneratedCellSetup,
    io: FsIoProvider,
}

impl BzlmodGeneratedFileOpsDelegate {
    fn resolve(&self, path: &CellRelativePath) -> ProjectRelativePathBuf {
        self.buck_out_resolver.resolve_external_cell_source(
            path,
            ExternalCellOrigin::BzlmodGenerated(self.setup.dupe()),
        )
    }

    fn get_base_path(&self) -> ProjectRelativePathBuf {
        self.resolve(CellRelativePath::empty())
    }
}

#[pagable_typetag]
#[async_trait::async_trait]
impl FileOpsDelegate for BzlmodGeneratedFileOpsDelegate {
    async fn read_file_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<ReadFileProxy> {
        Ok(ReadFileProxy::new_with_captures(
            (self.resolve(path), self.io.dupe()),
            |(project_path, io)| async move {
                (&io as &dyn IoProvider)
                    .read_file_if_exists(project_path)
                    .await
            },
        ))
    }

    async fn read_dir(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Arc<[RawDirEntry]>> {
        let project_path = self.resolve(path);
        let mut entries = (&self.io as &dyn IoProvider)
            .read_dir(project_path)
            .await
            .with_buck_error_context(|| format!("Error listing dir `{path}`"))?;

        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(entries.into())
    }

    async fn read_path_metadata_if_exists(
        &self,
        _ctx: &mut DiceComputations<'_>,
        path: &'async_trait CellRelativePath,
    ) -> buck2_error::Result<Option<RawPathMetadata>> {
        let project_path = self.resolve(path);
        let Some(metadata) = (&self.io as &dyn IoProvider)
            .read_path_metadata_if_exists(project_path)
            .await
            .with_buck_error_context(|| format!("Error accessing metadata for path `{path}`"))?
        else {
            return Ok(None);
        };
        Ok(Some(metadata.try_map(
            |path| match path.strip_prefix_opt(self.get_base_path()) {
                Some(path) => Ok(Arc::new(CellPath::new(self.cell, path.to_owned().into()))),
                None => Err(internal_error!(
                    "Non-cell internal symlink at `{}` in cell `{}`",
                    path,
                    self.cell
                )),
            },
        )?))
    }

    fn eq_token(&self) -> PartialEqAny<'_> {
        PartialEqAny::always_false()
    }
}

pub(crate) async fn get_file_ops_delegate(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodCellSetup,
) -> buck2_error::Result<Arc<BzlmodFileOpsDelegate>> {
    #[derive(
        dupe::Dupe,
        Clone,
        Debug,
        derive_more::Display,
        PartialEq,
        Eq,
        Hash,
        allocative::Allocative,
        Pagable
    )]
    #[display("({}, {})", _0, _1)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct BzlmodFileOpsDelegateKey(CellName, BzlmodCellSetup);

    #[async_trait::async_trait]
    impl Key for BzlmodFileOpsDelegateKey {
        type Value = buck2_error::Result<Arc<BzlmodFileOpsDelegate>>;

        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            cancellations: &CancellationContext,
        ) -> Self::Value {
            let artifact_fs = ctx.get_artifact_fs().await?;
            let ops = BzlmodFileOpsDelegate {
                buck_out_resolver: artifact_fs.buck_out_path_resolver().clone(),
                cell: self.0,
                setup: self.1.dupe(),
                io: FsIoProvider::new(
                    artifact_fs.fs().dupe(),
                    ctx.global_data().get_digest_config().cas_digest_config(),
                ),
            };
            download_and_materialize(ctx, &ops.get_base_path(), &self.1, cancellations).await?;
            Ok(Arc::new(ops))
        }

        fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
            false
        }

        fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
            OkPagableValueSerialize::<Self::Value>::new()
        }
    }

    ctx.compute(&BzlmodFileOpsDelegateKey(cell, setup)).await?
}

pub(crate) async fn get_generated_file_ops_delegate(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodGeneratedCellSetup,
) -> buck2_error::Result<Arc<BzlmodGeneratedFileOpsDelegate>> {
    #[derive(
        dupe::Dupe,
        Clone,
        Debug,
        derive_more::Display,
        PartialEq,
        Eq,
        Hash,
        allocative::Allocative,
        Pagable
    )]
    #[display("({}, {})", _0, _1)]
    #[pagable_typetag(dice::DiceKeyDyn)]
    struct BzlmodGeneratedFileOpsDelegateKey(CellName, BzlmodGeneratedCellSetup);

    #[async_trait::async_trait]
    impl Key for BzlmodGeneratedFileOpsDelegateKey {
        type Value = buck2_error::Result<Arc<BzlmodGeneratedFileOpsDelegate>>;

        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            cancellations: &CancellationContext,
        ) -> Self::Value {
            let artifact_fs = ctx.get_artifact_fs().await?;
            let ops = BzlmodGeneratedFileOpsDelegate {
                buck_out_resolver: artifact_fs.buck_out_path_resolver().clone(),
                cell: self.0,
                setup: self.1.dupe(),
                io: FsIoProvider::new(
                    artifact_fs.fs().dupe(),
                    ctx.global_data().get_digest_config().cas_digest_config(),
                ),
            };
            materialize_generated(ctx, &ops.get_base_path(), &self.1, cancellations).await?;
            Ok(Arc::new(ops))
        }

        fn equality(_x: &Self::Value, _y: &Self::Value) -> bool {
            false
        }

        fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
            OkPagableValueSerialize::<Self::Value>::new()
        }
    }

    ctx.compute(&BzlmodGeneratedFileOpsDelegateKey(cell, setup))
        .await?
}

pub(crate) async fn materialize_all(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodCellSetup,
) -> buck2_error::Result<ProjectRelativePathBuf> {
    let ops = get_file_ops_delegate(ctx, cell, setup.dupe()).await?;
    Ok(ops.get_base_path())
}

pub(crate) async fn materialize_generated_all(
    ctx: &mut DiceComputations<'_>,
    cell: CellName,
    setup: BzlmodGeneratedCellSetup,
) -> buck2_error::Result<ProjectRelativePathBuf> {
    let ops = get_generated_file_ops_delegate(ctx, cell, setup.dupe()).await?;
    Ok(ops.get_base_path())
}
