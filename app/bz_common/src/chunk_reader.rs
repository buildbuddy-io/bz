/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_core::bz_env;
use bz_error::BuckErrorContext;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;

/// A small utility to read AsyncRead in chunks before uploading them.
pub struct ChunkReader {
    chunk_size: u64,
}

impl ChunkReader {
    pub fn new() -> bz_error::Result<Self> {
        let chunk_size = bz_env!(
            "BUCK2_TEST_ARTIFACT_UPLOAD_CHUNK_BYTES",
            type=u64,
            applicability=testing,
        )?
        .unwrap_or(8 * 1024 * 1024);
        Ok(ChunkReader { chunk_size })
    }

    pub async fn read<R>(&self, reader: &mut R) -> Result<Vec<u8>, bz_error::Error>
    where
        R: AsyncRead + Unpin,
    {
        let mut buf = vec![];
        let mut reader = reader.take(self.chunk_size);
        let len = reader
            .read_to_end(&mut buf)
            .await
            .buck_error_context("Error reading chunk")?;
        buf.truncate(len);
        Ok(buf)
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }
}
