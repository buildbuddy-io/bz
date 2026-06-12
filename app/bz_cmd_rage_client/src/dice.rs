/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::path::Path;

use bz_cli_proto::UnstableDiceDumpRequest;
use bz_cli_proto::unstable_dice_dump_request::DiceDumpFormat;
use bz_client_ctx::daemon::client::BuckdClientConnector;
use bz_client_ctx::daemon::client::connect::BootstrapBuckdClient;
use bz_client_ctx::events_ctx::EventsCtx;
use bz_common::artifact_upload::Bucket;
use bz_common::artifact_upload::ArtifactUploadClient;
use bz_error::BuckErrorContext;
use bz_fs::error::IoResultExt;
use bz_fs::fs_util;
use bz_fs::paths::abs_norm_path::AbsNormPathBuf;
use bz_fs::paths::abs_path::AbsPathBuf;
use bz_util::process::async_background_command;

use crate::artifact_upload::artifact_upload_leads;

pub async fn upload_dice_dump(
    buckd: BootstrapBuckdClient,
    buck_out_dice: AbsNormPathBuf,
    artifact_client: &ArtifactUploadClient,
    artifact_id: &String,
) -> bz_error::Result<String> {
    let buckd = buckd.to_connector();
    let mut events_ctx = EventsCtx::new(None, Default::default());
    let artifact_bucket = Bucket::RAGE_DUMPS;
    let artifact_filename = format!("flat/{artifact_id}_dice-dump.tar");
    let this_dump_folder_name = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    DiceDump::new(buck_out_dice, &this_dump_folder_name)
        .upload(
            buckd,
            &mut events_ctx,
            artifact_client,
            artifact_bucket,
            &artifact_filename,
        )
        .await?;

    Ok(artifact_upload_leads(&artifact_bucket, artifact_filename))
}

struct DiceDump {
    buck_out_dice: AbsNormPathBuf,
    dump_folder: AbsPathBuf,
}

impl DiceDump {
    fn new(buck_out_dice: AbsNormPathBuf, dump_folder_name: &str) -> Self {
        let dump_folder = buck_out_dice.as_abs_path().join(dump_folder_name);

        Self {
            buck_out_dice,
            dump_folder,
        }
    }

    async fn upload(
        &self,
        mut buckd: BuckdClientConnector,
        events_ctx: &mut EventsCtx,
        artifact_client: &ArtifactUploadClient,
        artifact_bucket: Bucket,
        artifact_filename: &str,
    ) -> bz_error::Result<()> {
        fs_util::create_dir_all(&self.buck_out_dice).with_buck_error_context(|| {
            format!(
                "Failed to create directory `{}`, no DICE dump will be created",
                self.buck_out_dice.display()
            )
        })?;

        buckd
            .with_flushing()
            .unstable_dice_dump(
                UnstableDiceDumpRequest {
                    destination_path: self.dump_folder.to_str().unwrap().to_owned(),
                    format: DiceDumpFormat::Tsv.into(),
                },
                events_ctx,
            )
            .await
            .with_buck_error_context(|| {
                format!(
                    "DICE dump at `{}` failed to complete",
                    self.dump_folder.display()
                )
            })?;

        // create DICE dump name using the old command being rage on and the trace id of this rage command.
        upload_to_artifact_store(
            &self.dump_folder,
            artifact_client,
            artifact_bucket,
            artifact_filename,
        )
        .await
        .with_buck_error_context(|| "Failed during artifact upload!")?;

        Ok(())
    }
}

async fn upload_to_artifact_store(
    dump_folder: &Path,
    artifact_client: &ArtifactUploadClient,
    artifact_bucket: Bucket,
    artifact_filename: &str,
) -> bz_error::Result<()> {
    if !cfg!(target_os = "windows") {
        let tar = async_background_command("tar")
            .arg("-c")
            .arg(dump_folder)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        artifact_client
            .read_and_upload(
                artifact_bucket,
                artifact_filename,
                Default::default(),
                &mut tar.stdout.unwrap(),
            )
            .await?;
    }
    Ok(())
}

impl Drop for DiceDump {
    fn drop(&mut self) {
        if let Err(e) = fs_util::remove_all(&self.dump_folder)
            .categorize_internal()
            .with_buck_error_context(|| {
                format!(
                    "Failed to remove bz DICE dump folder at `{}`. Please remove this manually as it could be quite large.",
                    self.dump_folder.display()
                )
            })
        {
            tracing::warn!("{:#}", e);
        };
    }
}
