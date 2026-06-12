/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::io::Cursor;

use bz_common::artifact_upload::Bucket;
use bz_common::artifact_upload::ArtifactUploadClient;
use bz_fs::async_fs_util;
use bz_fs::error::IoResultExt;
use bz_fs::paths::abs_path::AbsPath;

pub(crate) fn artifact_upload_leads(bucket: &Bucket, filename: String) -> String {
    bucket.artifact_url(filename.as_str())
}

pub(crate) async fn file_to_artifact_store(
    artifact_client: &ArtifactUploadClient,
    path: &AbsPath,
    filename: String,
) -> bz_error::Result<String> {
    let bucket = Bucket::RAGE_DUMPS;
    // can't use async_fs_util
    // the trait to convert from tokio::fs::File is not implemented for Stdio
    let mut file = async_fs_util::open(&path).await.categorize_internal()?;

    artifact_client
        .read_and_upload(bucket, &filename, Default::default(), &mut file)
        .await?;

    Ok(artifact_upload_leads(&bucket, filename))
}

pub(crate) async fn buf_to_artifact_store(
    artifact_client: &ArtifactUploadClient,
    buf: &[u8],
    filename: String,
) -> bz_error::Result<String> {
    let bucket = Bucket::RAGE_DUMPS;
    let mut cursor = &mut Cursor::new(buf);

    artifact_client
        .read_and_upload(bucket, &filename, Default::default(), &mut cursor)
        .await?;

    Ok(artifact_upload_leads(&bucket, filename))
}
