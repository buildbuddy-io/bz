/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::io;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use bz_fs::paths::abs_path::AbsPath;
use bz_http::HttpClient;
use bz_http::HttpClientBuilder;
use bz_http::retries::HttpError;
use bz_http::retries::HttpErrorForRetry;
use bz_http::retries::IntoBuck2Error;
use bz_http::retries::http_retry;
use dupe::Dupe;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use hyper::Response;
use tokio::fs::File;
use tokio::io::AsyncRead;

use crate::chunk_reader::ChunkReader;

#[derive(Copy, Clone, Dupe)]
pub struct Ttl {
    duration: Duration,
}

impl Ttl {
    pub fn from_secs(ttl: u64) -> Self {
        Self {
            duration: Duration::from_secs(ttl),
        }
    }

    pub fn from_days(days: u64) -> Self {
        let secs = days * 24 * 60 * 60;
        Self {
            duration: Duration::from_secs(secs),
        }
    }

    pub fn as_secs(&self) -> u64 {
        self.duration.as_secs()
    }
}

impl Default for Ttl {
    fn default() -> Self {
        Self::from_secs(164 * 86_400)
    }
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Http)]
enum HttpWriteError {
    #[error(transparent)]
    Client(HttpError),
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Http)]
enum HttpAppendError {
    #[error(transparent)]
    Client(HttpError),
}

impl HttpErrorForRetry for HttpWriteError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Client(e) => e.is_retryable(),
        }
    }
}

impl HttpErrorForRetry for HttpAppendError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Client(e) => e.is_retryable(),
        }
    }
}

impl IntoBuck2Error for HttpWriteError {
    fn into_bz_error(self) -> bz_error::Error {
        bz_error::Error::from(self)
    }
}

impl IntoBuck2Error for HttpAppendError {
    fn into_bz_error(self) -> bz_error::Error {
        bz_error::Error::from(self)
    }
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Environment)]
pub enum UploadError {
    #[error(
        "No result code from uploading path `{0}` to the artifact store, probably due to signal interrupt"
    )]
    NoResultCodeError(String),
    #[error("Failed to find suitable artifact upload command")]
    CommandNotFound,
    #[error(
        "Failed to upload path `{path}` to the artifact store with exit code `{code}`, stderr: `{stderr}`"
    )]
    FileUploadExitCode {
        path: String,
        code: i32,
        stderr: String,
    },
    #[error(
        "Failed to upload stream to the artifact store with exit code `{code}`, stderr: `{stderr}`"
    )]
    StreamUploadExitCode { code: i32, stderr: String },
    #[error("No artifact upload endpoint is configured in this build")]
    Unavailable,
    #[error("File not found")]
    FileNotFound,
    #[error(transparent)]
    Other(bz_error::Error),
}

impl From<io::Error> for UploadError {
    fn from(err: io::Error) -> Self {
        UploadError::Other(err.into())
    }
}

#[derive(Clone, Copy)]
pub struct Bucket {
    pub name: &'static str,
}

impl Bucket {
    pub const EVENT_LOGS: Bucket = Bucket { name: "bz_logs" };

    pub const RAGE_DUMPS: Bucket = Bucket {
        name: "bz_rage_dumps",
    };

    pub const RE_LOGS: Bucket = Bucket { name: "bz_re_logs" };

    pub const INSTALLER_LOGS: Bucket = Bucket {
        name: "bz_installer_logs",
    };

    pub fn path(&self, filename: &str) -> String {
        format!("{}/{}", self.name, filename)
    }

    pub fn artifact_url(&self, filename: &str) -> String {
        format!("artifact://{}", self.path(filename))
    }
}

fn artifact_url(bucket: &Bucket, filename: String) -> String {
    let full_path = format!("{}/{}", bucket.name, filename);
    format!("artifact://{full_path}")
}

/// Return the scheme+host artifact upload endpoint, or None to disable uploads.
fn upload_endpoint_url(use_vpnless: bool) -> Option<&'static str> {
    let _unused = use_vpnless;
    None
}

pub struct ArtifactUploadClient {
    client: HttpClient,
    upload_url: Option<String>,
}

impl ArtifactUploadClient {
    pub async fn new() -> bz_error::Result<Self> {
        let client = HttpClientBuilder::internal().await?.build();
        let upload_url = upload_endpoint_url(client.supports_vpnless()).map(|s| s.to_owned());

        Ok(Self { client, upload_url })
    }

    pub fn is_available(&self) -> bool {
        self.upload_url.is_some()
    }

    pub async fn write(
        &self,
        bucket: Bucket,
        artifact_bucket_path: &str,
        buf: bytes::Bytes,
        ttl: Ttl,
    ) -> bz_error::Result<()> {
        let upload_url = match &self.upload_url {
            None => return Err(UploadError::Unavailable.into()),
            Some(x) => x,
        };
        let url = format!(
            "{}/v0/write/{}?bucketName={}&timeoutMsec=20000",
            upload_url, artifact_bucket_path, bucket.name
        );

        let mut headers = vec![(
            "X-Artifact-Obj-Predicate".to_owned(),
            "NoPredicate".to_owned(),
        )];

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let expiration = now.as_secs() + ttl.duration.as_secs();
        headers.push((
            "X-Artifact-Obj-ExpiresAt".to_owned(),
            expiration.to_string(),
        ));

        let res = http_retry(
            || async {
                self.client
                    .put(&url, buf.clone(), headers.clone())
                    .await
                    .map_err(|e| HttpWriteError::Client(HttpError::Client(e)))
            },
            vec![Duration::from_secs(1), Duration::from_secs(2)],
        )
        .await?;

        consume_response(res).await;

        Ok(())
    }

    pub async fn append(
        &self,
        bucket: Bucket,
        artifact_bucket_path: &str,
        buf: bytes::Bytes,
        offset: u64,
    ) -> bz_error::Result<()> {
        let upload_url = match &self.upload_url {
            None => return Err(UploadError::Unavailable.into()),
            Some(x) => x,
        };
        let url = format!(
            "{}/v0/append/{}?bucketName={}&timeoutMsec=20000&writeOffset={}",
            upload_url, artifact_bucket_path, bucket.name, offset
        );

        let res = http_retry(
            || async {
                self.client
                    .post(&url, buf.clone(), vec![])
                    .await
                    .map_err(|e| HttpAppendError::Client(HttpError::Client(e)))
            },
            vec![Duration::from_secs(1), Duration::from_secs(2)],
        )
        .await?;

        consume_response(res).await;

        Ok(())
    }

    pub async fn read_and_upload<R>(
        &self,
        bucket: Bucket,
        path: &str,
        ttl: Ttl,
        read: &mut R,
    ) -> bz_error::Result<()>
    where
        R: AsyncRead + Unpin,
    {
        let reader = ChunkReader::new()?;
        let mut upload = self.start_chunked_upload(bucket, path, ttl);
        let mut first = true;
        loop {
            let chunk = reader.read(read).await?;
            if !first && chunk.is_empty() {
                break;
            }
            first = false;
            upload.write(chunk.into()).await?;
        }
        bz_error::Ok(())
    }

    pub fn start_chunked_upload<'a>(
        &'a self,
        bucket: Bucket,
        path: &'a str,
        ttl: Ttl,
    ) -> ArtifactChunkedUploader<'a> {
        ArtifactChunkedUploader {
            client: self,
            position: 0,
            bucket,
            path,
            ttl,
        }
    }

    pub async fn upload_file(
        &self,
        local_path: &AbsPath,
        filename: String,
        bucket: Bucket,
        ttl: Ttl,
    ) -> bz_error::Result<String> {
        let mut file = File::open(&local_path).await?;
        self.read_and_upload(bucket, &filename, ttl, &mut file)
            .await?;

        Ok(artifact_url(&bucket, filename))
    }
}

async fn consume_response<'a>(mut res: Response<BoxStream<'a, hyper::Result<Bytes>>>) {
    // HTTP/1: Allow reusing the connection by consuming entire response
    while let Some(_chunk) = res.body_mut().next().await {}
}

/// Keep track of a chunk upload to a given artifact key.
pub struct ArtifactChunkedUploader<'a> {
    client: &'a ArtifactUploadClient,
    position: u64,
    bucket: Bucket,
    path: &'a str,
    ttl: Ttl,
}

impl ArtifactChunkedUploader<'_> {
    pub async fn write(&mut self, chunk: Bytes) -> bz_error::Result<()> {
        let len = u64::try_from(chunk.len())?;

        if self.position == 0 {
            // First chunk
            self.client
                .write(self.bucket, self.path, chunk, self.ttl)
                .await?
        } else {
            self.client
                .append(self.bucket, self.path, chunk, self.position)
                .await?
        }

        self.position += len;

        Ok(())
    }

    pub fn position(&self) -> u64 {
        self.position
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_days_to_secs() {
        assert_eq!(Ttl::from_days(1).duration.as_secs(), 86400);
        assert_eq!(Ttl::from_days(3).duration.as_secs(), 86400 * 3);
    }
}
