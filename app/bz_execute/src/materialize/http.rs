/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use allocative::Allocative;
use async_compression::tokio::bufread::GzipDecoder;
use bz_common::cas_digest::CasDigestConfig;
use bz_common::cas_digest::DigestAlgorithmFamily;
use bz_common::cas_digest::SHA1_SIZE;
use bz_common::cas_digest::SHA256_SIZE;
use bz_common::file_ops::metadata::FileDigest;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_core::cells::external::BAZEL_REPOSITORY_ACCEPT_ENCODING;
use bz_core::cells::external::BAZEL_REPOSITORY_ACCEPT_ENCODING_HEADER;
use bz_core::cells::external::BAZEL_REPOSITORY_USER_AGENT_HEADER;
use bz_core::cells::external::bazel_repository_user_agent;
use bz_core::fs::project::ProjectRoot;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use bz_error::BuckErrorContext;
use bz_fs::error::IoResultExt;
use bz_fs::fs_util;
use bz_http::HttpClient;
use bz_http::ResponseFinalUri;
use bz_http::retries::HttpError;
use bz_http::retries::HttpErrorForRetry;
use bz_http::retries::IntoBuck2Error;
use bz_http::retries::http_retry;
use bytes::Bytes;
use digest::DynDigest;
use dupe::Dupe;
use futures::TryStreamExt;
use futures::stream::Stream;
use hyper::Response;
use pagable::Pagable;
use sha1::Digest;
use sha1::Sha1;
use sha2::Sha256;
use sha2::Sha384;
use sha2::Sha512;
use smallvec::SmallVec;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio_util::io::StreamReader;

use crate::digest_config::DigestConfig;

const HTTP_DOWNLOAD_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Dupe, Allocative, Pagable)]
pub enum Checksum {
    None,
    Sha1(Arc<str>),
    Sha256(Arc<str>),
    Sha384(Arc<str>),
    Sha512(Arc<str>),
    Both { sha1: Arc<str>, sha256: Arc<str> },
}

#[derive(bz_error::Error, Debug)]
#[buck2(tag = Input)]
enum DownloadFileError {
    #[error("Must pass in at least one checksum (e.g. `sha1 = ...`)")]
    MissingChecksum,
    #[error("Invalid digest for `{digest_type}` argument, expected length of {expected_len} but got {}, digest `{digest}`", digest.len())]
    InvalidDigestLength {
        digest: String,
        expected_len: usize,
        digest_type: &'static str,
    },
    #[error(
        "Invalid digest for `{digest_type}` argument, expected 0-9 a-z hex characters, but got `{bad_char}`, digest `{digest}`"
    )]
    InvalidDigestCharacter {
        digest: String,
        bad_char: char,
        digest_type: &'static str,
    },
}

impl Checksum {
    pub fn none() -> Self {
        Self::None
    }

    fn is_hex_digit(x: char) -> bool {
        let x = x.to_ascii_lowercase();
        x.is_ascii_digit() || ('a'..='f').contains(&x)
    }

    fn validate_digest(
        digest: &str,
        digest_len: usize,
        digest_type: &'static str,
    ) -> bz_error::Result<Arc<str>> {
        let expected_len = digest_len * 2;
        if digest.len() != expected_len {
            return Err(DownloadFileError::InvalidDigestLength {
                digest: digest.to_owned(),
                expected_len,
                digest_type,
            }
            .into());
        }
        if let Some(bad_char) = digest.chars().find(|x| !Self::is_hex_digit(*x)) {
            return Err(DownloadFileError::InvalidDigestCharacter {
                digest: digest.to_owned(),
                bad_char,
                digest_type,
            }
            .into());
        }
        Ok(Arc::from(digest.to_ascii_lowercase()))
    }

    pub fn new(sha1: Option<&str>, sha256: Option<&str>) -> bz_error::Result<Self> {
        fn validate_digest(
            digest: Option<&str>,
            digest_len: usize,
            digest_type: &'static str,
        ) -> bz_error::Result<Option<Arc<str>>> {
            match digest {
                None => Ok(None),
                Some(digest) => {
                    Checksum::validate_digest(digest, digest_len, digest_type).map(Some)
                }
            }
        }

        match (
            validate_digest(sha1, SHA1_SIZE, "sha1")?,
            validate_digest(sha256, SHA256_SIZE, "sha256")?,
        ) {
            (Some(sha1), None) => Ok(Checksum::Sha1(sha1)),
            (None, Some(sha256)) => Ok(Checksum::Sha256(sha256)),
            (Some(sha1), Some(sha256)) => Ok(Checksum::Both { sha1, sha256 }),
            (None, None) => Err(DownloadFileError::MissingChecksum.into()),
        }
    }

    pub fn new_sha384(sha384: &str) -> bz_error::Result<Self> {
        Ok(Self::Sha384(Self::validate_digest(sha384, 48, "sha384")?))
    }

    pub fn new_sha512(sha512: &str) -> bz_error::Result<Self> {
        Ok(Self::Sha512(Self::validate_digest(sha512, 64, "sha512")?))
    }

    pub fn sha1(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Sha1(sha1) => Some(sha1),
            Self::Sha256(..) => None,
            Self::Sha384(..) => None,
            Self::Sha512(..) => None,
            Self::Both { sha1, .. } => Some(sha1),
        }
    }

    pub fn sha256(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Sha1(..) => None,
            Self::Sha256(sha256) => Some(sha256),
            Self::Sha384(..) => None,
            Self::Sha512(..) => None,
            Self::Both { sha256, .. } => Some(sha256),
        }
    }

    pub fn sha384(&self) -> Option<&str> {
        match self {
            Self::Sha384(sha384) => Some(sha384),
            Self::None
            | Self::Sha1(..)
            | Self::Sha256(..)
            | Self::Sha512(..)
            | Self::Both { .. } => None,
        }
    }

    pub fn sha512(&self) -> Option<&str> {
        match self {
            Self::Sha512(sha512) => Some(sha512),
            Self::None
            | Self::Sha1(..)
            | Self::Sha256(..)
            | Self::Sha384(..)
            | Self::Both { .. } => None,
        }
    }
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Http)]
enum HttpHeadError {
    #[error("Error performing http_head request")]
    Client(#[source] HttpError),
}

impl From<HttpError> for HttpHeadError {
    fn from(e: HttpError) -> Self {
        Self::Client(e)
    }
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Http)]
enum HttpDownloadError {
    #[error("Error performing http_download request")]
    Client(#[source] HttpError),

    #[error(
        "Invalid {digest_kind} digest. Expected {expected}, got {obtained}. URL: {url}. {debug}"
    )]
    #[buck2(input)]
    InvalidChecksum {
        digest_kind: &'static str,
        expected: String,
        obtained: String,
        url: String,
        debug: MaybeResponseDebugInfo,
    },

    #[error(
        "Received invalid {kind} digest from {url}; perhaps this is not allowed on vpnless?. Expected {want}, got {got}. {debug}"
    )]
    MaybeNotAllowedOnVpnless {
        kind: &'static str,
        want: String,
        got: String,
        url: String,
        debug: MaybeResponseDebugInfo,
    },

    #[error(transparent)]
    IoError(bz_error::Error),
}

impl HttpDownloadError {
    fn into_final(mut self) -> Self {
        match &mut self {
            Self::Client(..) | Self::IoError(..) => {}
            Self::InvalidChecksum { debug, .. } | Self::MaybeNotAllowedOnVpnless { debug, .. } => {
                debug.is_final = true;
            }
        }

        self
    }
}

impl From<HttpError> for HttpDownloadError {
    fn from(e: HttpError) -> Self {
        Self::Client(e)
    }
}

impl HttpErrorForRetry for HttpHeadError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Client(e) => e.is_retryable(),
        }
    }
}

impl HttpErrorForRetry for HttpDownloadError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Client(e) => e.is_retryable(),
            Self::InvalidChecksum { .. } => {
                // Normally, invalid checksums don't make sense to retry, but the HTTP servers we
                // talk to internally tend to happily return 200s and give you the error in the
                // message body... so it's a good idea to retry those.
                cfg!(fbcode_build)
            }
            Self::IoError(..) | Self::MaybeNotAllowedOnVpnless { .. } => false,
        }
    }
}

impl IntoBuck2Error for HttpHeadError {
    fn into_bz_error(self) -> bz_error::Error {
        bz_error::Error::from(self)
    }
}

impl IntoBuck2Error for HttpDownloadError {
    fn into_bz_error(self) -> bz_error::Error {
        bz_error::Error::from(self)
    }
}

pub async fn http_head(client: &HttpClient, url: &str) -> bz_error::Result<Response<()>> {
    let response = http_retry(
        || async {
            client
                .head(url)
                .await
                .map_err(|e| HttpHeadError::Client(HttpError::Client(e)))
        },
        vec![2, 4, 8].into_iter().map(Duration::from_secs).collect(),
    )
    .await?;
    Ok(response)
}

pub async fn http_download(
    client: &HttpClient,
    fs: &ProjectRoot,
    digest_config: DigestConfig,
    path: &ProjectRelativePath,
    url: &str,
    checksum: &Checksum,
    executable: bool,
) -> bz_error::Result<TrackedFileDigest> {
    http_download_with_headers(
        client,
        fs,
        digest_config,
        path,
        url,
        checksum,
        executable,
        &[],
    )
    .await
}

pub fn bazel_repository_download_headers(
    headers: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case(BAZEL_REPOSITORY_ACCEPT_ENCODING_HEADER)
            && !name.eq_ignore_ascii_case(BAZEL_REPOSITORY_USER_AGENT_HEADER)
        {
            result.push((name, value));
        }
    }
    result.push((
        BAZEL_REPOSITORY_ACCEPT_ENCODING_HEADER.to_owned(),
        BAZEL_REPOSITORY_ACCEPT_ENCODING.to_owned(),
    ));
    result.push((
        BAZEL_REPOSITORY_USER_AGENT_HEADER.to_owned(),
        bazel_repository_user_agent(),
    ));
    result
}

pub async fn http_download_with_headers(
    client: &HttpClient,
    fs: &ProjectRoot,
    digest_config: DigestConfig,
    path: &ProjectRelativePath,
    url: &str,
    checksum: &Checksum,
    executable: bool,
    headers: &[(String, String)],
) -> bz_error::Result<TrackedFileDigest> {
    let abs_path = fs.resolve(path);
    if let Some(dir) = abs_path.parent() {
        fs_util::create_dir_all(dir)?;
    }

    Ok(http_retry(
        || async {
            let request_headers = bazel_repository_request_headers_for_url(url, headers);
            let response = client
                .get_with_headers(url, request_headers.into_iter())
                .await
                .map_err(|e| HttpDownloadError::Client(HttpError::Client(e)))?;

            let (head, stream) = response.into_parts();
            let file = fs_util::create_file(&abs_path)
                .categorize_internal()
                .map_err(HttpDownloadError::IoError)?;
            let buf_writer = std::io::BufWriter::new(file);

            let digest = copy_and_hash(
                url,
                Some(head),
                &abs_path,
                stream,
                buf_writer,
                digest_config.cas_digest_config(),
                checksum,
                client.supports_vpnless(),
            )
            .await?;

            if executable {
                fs.set_executable(path)
                    .map_err(HttpDownloadError::IoError)?;
            }

            Result::<_, HttpDownloadError>::Ok(TrackedFileDigest::new(
                digest,
                digest_config.cas_digest_config(),
            ))
        },
        vec![2, 4, 8].into_iter().map(Duration::from_secs).collect(),
    )
    .await
    .map_err(|e| e.into_final())?)
}

fn bazel_repository_request_headers_for_url<'a>(
    url: &str,
    headers: &'a [(String, String)],
) -> Vec<(&'a str, &'a str)> {
    let compressed_url = url_has_bazel_compressed_extension(url);
    headers
        .iter()
        .filter(|(name, _)| {
            !compressed_url || !name.eq_ignore_ascii_case(BAZEL_REPOSITORY_ACCEPT_ENCODING_HEADER)
        })
        .map(|(name, value)| (&**name, &**value))
        .collect()
}

/// Copy a stream into a writer while producing its digest and checksumming it.
async fn copy_and_hash(
    url: &str,
    head: Option<http::response::Parts>,
    abs_path: &(impl std::fmt::Display + ?Sized),
    stream: impl Stream<Item = Result<Bytes, hyper::Error>> + Unpin,
    writer: impl Write,
    digest_config: CasDigestConfig,
    checksum: &Checksum,
    is_vpnless: bool,
) -> Result<FileDigest, HttpDownloadError> {
    let gunzip = response_should_gunzip(url, head.as_ref());
    let reader = StreamReader::new(stream.map_err(std::io::Error::other));
    if gunzip {
        copy_and_hash_from_reader(
            url,
            head,
            abs_path,
            GzipDecoder::new(reader),
            writer,
            digest_config,
            checksum,
            is_vpnless,
        )
        .await
    } else {
        copy_and_hash_from_reader(
            url,
            head,
            abs_path,
            reader,
            writer,
            digest_config,
            checksum,
            is_vpnless,
        )
        .await
    }
}

async fn copy_and_hash_from_reader(
    url: &str,
    head: Option<http::response::Parts>,
    abs_path: &(impl std::fmt::Display + ?Sized),
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl Write,
    digest_config: CasDigestConfig,
    checksum: &Checksum,
    is_vpnless: bool,
) -> Result<FileDigest, HttpDownloadError> {
    let mut digester = FileDigest::digester(digest_config);

    // For each checksum entry we have, we're going to add a validator. We might have to create
    // a new hasher, or reuse the `FileDigest::digester` if it matches.

    enum Validator {
        PrimaryDigest,
        ExtraDigest(Box<dyn DynDigest + Send>),
    }

    let mut validators = SmallVec::<[_; 2]>::new();

    if let Some(sha1) = checksum.sha1() {
        let validator = if digester.algorithm() == DigestAlgorithmFamily::Sha1 {
            Validator::PrimaryDigest
        } else {
            Validator::ExtraDigest(Box::new(Sha1::new()) as _)
        };

        validators.push((validator, sha1, "sha1"));
    }

    if let Some(sha256) = checksum.sha256() {
        let validator = if digester.algorithm() == DigestAlgorithmFamily::Sha256 {
            Validator::PrimaryDigest
        } else {
            Validator::ExtraDigest(Box::new(Sha256::new()) as _)
        };

        validators.push((validator, sha256, "sha256"));
    }

    if let Some(sha384) = checksum.sha384() {
        validators.push((
            Validator::ExtraDigest(Box::new(Sha384::new()) as _),
            sha384,
            "sha384",
        ));
    }

    if let Some(sha512) = checksum.sha512() {
        validators.push((
            Validator::ExtraDigest(Box::new(Sha512::new()) as _),
            sha512,
            "sha512",
        ));
    }

    let mut buff = DebugBuffer::new(512);
    let mut read_buffer = vec![0u8; 64 * 1024];

    loop {
        let bytes_read = tokio::time::timeout(
            HTTP_DOWNLOAD_READ_IDLE_TIMEOUT,
            reader.read(&mut read_buffer),
        )
        .await
        .map_err(|_| {
            HttpDownloadError::Client(HttpError::Client(bz_http::HttpError::Timeout {
                uri: url.to_owned(),
                duration: HTTP_DOWNLOAD_READ_IDLE_TIMEOUT.as_secs(),
            }))
        })?
        .with_buck_error_context(|| format!("read({url})"))
        .map_err(HttpDownloadError::IoError)?;
        if bytes_read == 0 {
            break;
        }
        let chunk = &read_buffer[..bytes_read];

        buff.peek(chunk);

        writer
            .write_all(chunk)
            .with_buck_error_context(|| format!("write({abs_path})"))
            .map_err(HttpDownloadError::IoError)?;

        digester.update(chunk);
        for (validator, _expected, _kind) in validators.iter_mut() {
            if let Validator::ExtraDigest(hasher) = validator {
                hasher.update(chunk);
            }
        }
    }
    writer
        .flush()
        .with_buck_error_context(|| format!("flush({abs_path})"))
        .map_err(HttpDownloadError::IoError)?;

    let digest = digester.finalize();

    // Validate
    for (validator, expected, kind) in validators {
        let obtained = match validator {
            Validator::PrimaryDigest => digest.raw_digest().to_string(),
            Validator::ExtraDigest(hasher) => hex::encode(hasher.finalize()),
        };

        if expected != obtained {
            let debug = MaybeResponseDebugInfo {
                bytes_seen: buff.bytes_seen,
                buff: buff.to_utf8().map(ToOwned::to_owned),
                head,
                is_final: false,
            };

            if is_vpnless {
                return Err(HttpDownloadError::MaybeNotAllowedOnVpnless {
                    kind,
                    want: expected.to_owned(),
                    got: obtained,
                    url: url.to_owned(),
                    debug,
                });
            }
            return Err(HttpDownloadError::InvalidChecksum {
                digest_kind: kind,
                expected: expected.to_owned(),
                obtained,
                url: url.to_owned(),
                debug,
            });
        }
    }

    Ok(digest)
}

fn response_should_gunzip(url: &str, head: Option<&http::response::Parts>) -> bool {
    let Some(head) = head else {
        return false;
    };
    let Some(content_encoding) = head.headers.get(http::header::CONTENT_ENCODING) else {
        return false;
    };
    let Ok(content_encoding) = content_encoding.to_str() else {
        return false;
    };
    if !content_encoding
        .split(',')
        .map(str::trim)
        .any(|encoding| matches!(encoding, "gzip" | "x-gzip"))
    {
        return false;
    }
    if url_has_gzipped_extension(url) {
        return false;
    }
    if let Some(final_uri) = head.extensions.get::<ResponseFinalUri>()
        && url_has_gzipped_extension(final_uri.as_str())
    {
        return false;
    }
    true
}

fn url_has_bazel_compressed_extension(url: &str) -> bool {
    matches!(
        url_extension(url),
        Some("bz2" | "gz" | "jar" | "tgz" | "war" | "xz" | "zip")
    )
}

fn url_has_gzipped_extension(url: &str) -> bool {
    matches!(url_extension(url), Some("gz" | "tgz"))
}

fn url_extension(url: &str) -> Option<&str> {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    let path = path.split_once('#').map_or(path, |(path, _)| path);
    let path = path.rsplit_once('/').map_or(path, |(_, basename)| basename);
    path.rsplit_once('.').map(|(_, extension)| extension)
}

struct DebugBuffer {
    bytes_seen: u64,
    max_size: usize,
    buff: Vec<u8>,
}

impl DebugBuffer {
    fn new(max_size: usize) -> Self {
        Self {
            bytes_seen: 0,
            max_size,
            buff: Vec::new(),
        }
    }

    fn peek(&mut self, chunk: &[u8]) {
        // unwrap safety: we can't possibly have a chunk whose length can't be represented with 64
        // bits.
        self.bytes_seen += u64::try_from(chunk.len()).unwrap();

        if self.buff.len() < self.max_size {
            let want_bytes = std::cmp::min(chunk.len(), self.max_size - self.buff.len());
            self.buff.extend(&chunk[..want_bytes]);
        }
    }

    fn to_utf8(&self) -> Option<&str> {
        match std::str::from_utf8(&self.buff) {
            Ok(utf8) => Some(utf8),
            Err(e) => {
                let valid_up_to = e.valid_up_to();

                // If at least 50% of the buffer is valid UTF8, let's show it.
                if valid_up_to >= (self.max_size / 2) {
                    std::str::from_utf8(&self.buff[..valid_up_to]).ok()
                } else {
                    None
                }
            }
        }
    }
}

/// A little helper to avoid showing the debug data in http_retry because that's reall verbose.
#[derive(Debug)]
struct MaybeResponseDebugInfo {
    bytes_seen: u64,
    buff: Option<String>,
    head: Option<http::response::Parts>,
    is_final: bool,
}

impl fmt::Display for MaybeResponseDebugInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Received {} bytes.", self.bytes_seen)?;

        if !self.is_final {
            return Ok(());
        }

        if let Some(head) = &self.head {
            let interesting_headers = ["error-mid", "x-fb-debug"]
                .into_iter()
                .filter_map(|header| {
                    head.headers
                        .get(header)
                        .and_then(|h| h.to_str().ok())
                        .map(|header_value| (header, header_value))
                })
                .collect::<Vec<_>>();

            write!(f, "\n\nRelevant debug headers:\n")?;
            if interesting_headers.is_empty() {
                write!(f, "<none>")?;
            } else {
                for (header, header_value) in interesting_headers {
                    writeln!(f, "{header}: {header_value}")?;
                }
            }
        }

        match &self.buff {
            Some(text) => {
                write!(f, "\n\nResponse started with:\n\n{text}")?;
            }
            None => {
                write!(f, "Response is not UTF-8")?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use bz_common::cas_digest::testing;
    use futures::stream;

    use super::*;

    async fn do_test(
        digest_config: CasDigestConfig,
        checksum: &Checksum,
    ) -> Result<(FileDigest, Vec<u8>), HttpDownloadError> {
        let mut out = Vec::new();

        let digest = copy_and_hash(
            "test",
            None,
            "test",
            stream::iter(vec![Ok(Bytes::from("foo")), Ok(Bytes::from("bar"))]),
            &mut out,
            digest_config,
            checksum,
            false,
        )
        .await?;

        Ok((digest, out))
    }

    #[tokio::test]
    async fn test_copy_and_hash_ok() -> bz_error::Result<()> {
        let (digest, bytes) = do_test(
            testing::blake3(),
            &Checksum::Both {
                sha1: Arc::from("8843d7f92416211de9ebb963ff4ce28125932878"),
                sha256: Arc::from(
                    "c3ab8ff13720e8ad9047dd39466b3c8974e592c2fa383d4a3960714caef0c4f2",
                ),
            },
        )
        .await?;

        assert_eq!(
            digest.to_string(),
            "aa51dcd43d5c6c5203ee16906fd6b35db298b9b2e1de3fce81811d4806b76b7d:6"
        );

        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "foobar");

        do_test(
            testing::blake3(),
            &Checksum::new_sha384(
                "3c9c30d9f665e74d515c842960d4a451c83a0125fd3de7392d7b37231af10c72ea58aedfcdf89a5765bf902af93ecf06",
            )?,
        )
        .await?;

        do_test(
            testing::blake3(),
            &Checksum::new_sha512(
                "0a50261ebd1a390fed2bf326f2673c145582a6342d523204973d0219337f81616a8069b012587cf5635f6925f1b56c360230c19b273500ee013e030601bf2425",
            )?,
        )
        .await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_copy_and_hash_invalid_primary_hash() -> bz_error::Result<()> {
        assert_matches!(
            do_test(testing::sha1(), &Checksum::Sha1(Arc::from("oops"))).await,
            Err(HttpDownloadError::InvalidChecksum { .. })
        );

        assert_matches!(
            do_test(testing::sha256(), &Checksum::Sha256(Arc::from("oops"))).await,
            Err(HttpDownloadError::InvalidChecksum { .. })
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_copy_and_hash_invalid_secondary_hash() -> bz_error::Result<()> {
        assert_matches!(
            do_test(testing::blake3(), &Checksum::Sha1(Arc::from("oops"))).await,
            Err(HttpDownloadError::InvalidChecksum { .. })
        );

        assert_matches!(
            do_test(testing::blake3(), &Checksum::Sha256(Arc::from("oops"))).await,
            Err(HttpDownloadError::InvalidChecksum { .. })
        );

        assert_matches!(
            do_test(testing::blake3(), &Checksum::Sha384(Arc::from("oops"))).await,
            Err(HttpDownloadError::InvalidChecksum { .. })
        );

        assert_matches!(
            do_test(testing::blake3(), &Checksum::Sha512(Arc::from("oops"))).await,
            Err(HttpDownloadError::InvalidChecksum { .. })
        );

        Ok(())
    }

    #[test]
    fn test_debug_buffer() {
        let mut buff = DebugBuffer::new(10);
        buff.peek(b"foo");
        assert_eq!(buff.to_utf8(), Some("foo"));

        let mut buff = DebugBuffer::new(2);
        buff.peek(b"foo");
        assert_eq!(buff.to_utf8(), Some("fo"));

        let mut buff = DebugBuffer::new(4);
        buff.peek(b"foo");
        buff.peek(b"foo");
        assert_eq!(buff.to_utf8(), Some("foof"));

        // 75% fine.
        let mut buff = DebugBuffer::new(4);
        buff.peek(b"foo");
        buff.peek(&[0xff]);
        assert_eq!(buff.to_utf8(), Some("foo"));

        // 50% fine.
        let mut buff = DebugBuffer::new(4);
        buff.peek(b"fo");
        buff.peek(&[0xff]);
        assert_eq!(buff.to_utf8(), Some("fo"));

        // 25% fine.
        let mut buff = DebugBuffer::new(4);
        buff.peek(b"f");
        buff.peek(&[0xff]);
        assert_eq!(buff.to_utf8(), None);

        // 0% fine.
        let mut buff = DebugBuffer::new(4);
        buff.peek(&[0xff]);
        assert_eq!(buff.to_utf8(), None);
    }
}
