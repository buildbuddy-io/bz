/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_common::file_ops::metadata::FileMetadata;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_directory::directory::directory::Directory;
use bz_directory::directory::directory_iterator::DirectoryIterator;
use bz_directory::directory::directory_iterator::DirectoryIteratorPathStack;
use bz_directory::directory::entry::DirectoryEntry;
use bz_directory::directory::walk::unordered_entry_walk;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest::CasDigestToReExt;
use bz_execute::digest_config::DigestConfig;
use bz_execute::directory::ActionDirectoryMember;
use bz_execute::execute::blocking::BlockingExecutor;
use bz_execute::execute::clean_output_paths::CleanOutputPaths;
use bz_execute::execute::clean_output_paths::cleanup_path;
use bz_execute::materialize::materializer::CasDownloadInfo;
use bz_execute::materialize::materializer::WriteRequest;
use bz_execute::materialize::utils::dynamic_priority_handle::DynamicPriorityHandle;
use bz_execute::materialize::utils::priority_semaphore::Priority;
use bz_execute::re::manager::ReConnectionManager;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use gazebo::prelude::*;
use remote_execution::NamedDigest;
use remote_execution::NamedDigestWithPermissions;

use crate::materializers::io::MaterializeTreeStructure;

pub async fn write_to_disk<'a>(
    fs: &ProjectRoot,
    io_executor: &dyn BlockingExecutor,
    digest_config: DigestConfig,
    generate: Box<dyn FnOnce() -> bz_error::Result<Vec<WriteRequest>> + Send + 'a>,
) -> bz_error::Result<Vec<ArtifactValue>> {
    io_executor
        .execute_io_inline({
            move || {
                let requests = generate()?;
                let mut values = Vec::with_capacity(requests.len());

                for WriteRequest {
                    path,
                    content,
                    is_executable,
                    configuration_path: _,
                } in requests
                {
                    let digest = TrackedFileDigest::from_content(
                        &content,
                        digest_config.cas_digest_config(),
                    );
                    cleanup_path(fs, &path)?;
                    fs.write_file(&path, &content, is_executable)?;

                    values.push(ArtifactValue::file(FileMetadata {
                        digest,
                        is_executable,
                    }));
                }

                Ok(values)
            }
        })
        .await
}

pub async fn cas_download(
    fs: &ProjectRoot,
    io: &dyn BlockingExecutor,
    re: &ReConnectionManager,
    info: &CasDownloadInfo,
    artifacts: Vec<(ProjectRelativePathBuf, ArtifactValue)>,
    cancellations: &CancellationContext,
) -> bz_error::Result<()> {
    io.execute_io(
        Box::new(CleanOutputPaths {
            paths: artifacts.map(|(p, _)| p.to_owned()),
        }),
        cancellations,
    )
    .await?;

    for (path, value) in artifacts.iter() {
        io.execute_io(
            Box::new(MaterializeTreeStructure {
                path: path.to_owned(),
                entry: value.entry().dupe(),
            }),
            cancellations,
        )
        .await?;
    }

    let mut files = Vec::new();
    for (path, value) in artifacts.iter() {
        let mut walk = unordered_entry_walk(value.entry().as_ref().map_dir(Directory::as_ref));
        while let Some((entry_path, entry)) = walk.next() {
            if let DirectoryEntry::Leaf(ActionDirectoryMember::File(m)) = entry {
                files.push(NamedDigestWithPermissions {
                    named_digest: NamedDigest {
                        digest: m.digest.to_re(),
                        name: fs
                            .resolve(path.join(entry_path.get()))
                            .as_maybe_relativized_str()?
                            .to_owned(),
                        ..Default::default()
                    },
                    is_executable: m.is_executable,
                    ..Default::default()
                });
            }
        }
    }

    let re_conn = re.get_re_connection();
    let re_client = re_conn.get_client().with_use_case(info.re_use_case);
    cancellations
        .critical_section(|| {
            re_client.materialize_files(files, DynamicPriorityHandle::new(Priority::High))
        })
        .await?;
    Ok(())
}
