use super::*;

pub(super) fn repository_ctx_download_error_result(
    allow_fail: bool,
    error: buck2_error::Error,
) -> starlark::Result<ModuleCtxDownloadResult> {
    if allow_fail {
        Ok(ModuleCtxDownloadResult::new(
            false,
            None,
            None,
            Some(&error.to_string()),
        ))
    } else {
        Err(error.into())
    }
}

#[derive(Debug, Clone)]
pub(super) struct ModuleCtxDownloadAuthHeader {
    url: String,
    name: String,
    value: String,
}

fn module_ctx_download_auth_string_field(
    auth: &DictRef<'_>,
    url: &str,
    field: &'static str,
) -> starlark::Result<Option<String>> {
    let Some(value) = auth.get_str(field) else {
        return Ok(None);
    };
    let Some(value) = value.unpack_str() else {
        return Err(buck2_error::Error::from(
            BazelRepositoryError::ModuleCtxDownloadAuthFieldUnsupportedValue {
                url: url.to_owned(),
                field,
                got: value.get_type().to_owned(),
            },
        )
        .into());
    };
    Ok(Some(value.to_owned()))
}

pub(super) fn module_ctx_download_auth_headers_from_entries(
    entries: &UnpackDictEntries<Value<'_>, Value<'_>>,
) -> starlark::Result<Vec<ModuleCtxDownloadAuthHeader>> {
    let mut headers = Vec::new();
    for (url, auth) in entries.entries.iter() {
        let Some(url) = url.unpack_str() else {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadAuthKeyUnsupportedValue(
                    url.get_type().to_owned(),
                ),
            )
            .into());
        };
        let Some(auth) = DictRef::from_value(*auth) else {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadAuthValueUnsupportedValue {
                    url: url.to_owned(),
                    got: auth.get_type().to_owned(),
                },
            )
            .into());
        };
        let Some(auth_type) = auth.get_str("type").and_then(|value| value.unpack_str()) else {
            continue;
        };
        match auth_type {
            "basic" => {
                let Some(login) = module_ctx_download_auth_string_field(&auth, url, "login")?
                else {
                    return Err(buck2_error::Error::from(
                        BazelRepositoryError::ModuleCtxDownloadAuthBasicMissingCredentials {
                            url: url.to_owned(),
                        },
                    )
                    .into());
                };
                let Some(password) = module_ctx_download_auth_string_field(&auth, url, "password")?
                else {
                    return Err(buck2_error::Error::from(
                        BazelRepositoryError::ModuleCtxDownloadAuthBasicMissingCredentials {
                            url: url.to_owned(),
                        },
                    )
                    .into());
                };
                let credentials = format!("{login}:{password}");
                headers.push(ModuleCtxDownloadAuthHeader {
                    url: url.to_owned(),
                    name: "Authorization".to_owned(),
                    value: format!("Basic {}", BASE64_STANDARD.encode(credentials)),
                });
            }
            "pattern" => {
                let Some(mut authorization) =
                    module_ctx_download_auth_string_field(&auth, url, "pattern")?
                else {
                    return Err(buck2_error::Error::from(
                        BazelRepositoryError::ModuleCtxDownloadAuthPatternMissingPattern {
                            url: url.to_owned(),
                        },
                    )
                    .into());
                };
                for component in ["password", "login"] {
                    let marker = format!("<{component}>");
                    if authorization.contains(&marker) {
                        let Some(value) =
                            module_ctx_download_auth_string_field(&auth, url, component)?
                        else {
                            return Err(buck2_error::Error::from(
                                BazelRepositoryError::ModuleCtxDownloadAuthPatternMissingComponent {
                                    component: marker,
                                },
                            )
                            .into());
                        };
                        authorization = authorization.replace(&marker, &value);
                    }
                }
                headers.push(ModuleCtxDownloadAuthHeader {
                    url: url.to_owned(),
                    name: "Authorization".to_owned(),
                    value: authorization,
                });
            }
            _ => {}
        }
    }
    Ok(headers)
}

fn module_ctx_download_header_value_to_strings(
    header: &str,
    value: Value<'_>,
) -> starlark::Result<Vec<String>> {
    if let Some(value) = value.unpack_str() {
        return Ok(vec![value.to_owned()]);
    }

    let values = if let Some(list) = ListRef::from_value(value) {
        list.iter().collect::<Vec<_>>()
    } else if let Some(tuple) = TupleRef::from_value(value) {
        tuple.iter().collect::<Vec<_>>()
    } else {
        return Err(buck2_error::Error::from(
            BazelRepositoryError::ModuleCtxDownloadHeaderValueUnsupportedValue {
                header: header.to_owned(),
                got: value.get_type().to_owned(),
            },
        )
        .into());
    };

    let mut strings = Vec::with_capacity(values.len());
    for value in values {
        let Some(value) = value.unpack_str() else {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadHeaderValueUnsupportedValue {
                    header: header.to_owned(),
                    got: value.get_type().to_owned(),
                },
            )
            .into());
        };
        strings.push(value.to_owned());
    }
    Ok(strings)
}

pub(super) fn module_ctx_download_headers_from_entries(
    entries: &UnpackDictEntries<Value<'_>, Value<'_>>,
) -> starlark::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for (name, value) in entries.entries.iter() {
        let Some(name) = name.unpack_str() else {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadHeaderKeyUnsupportedValue(
                    name.get_type().to_owned(),
                ),
            )
            .into());
        };
        if name.eq_ignore_ascii_case(BAZEL_REPOSITORY_ACCEPT_ENCODING_HEADER)
            || name.eq_ignore_ascii_case(BAZEL_REPOSITORY_USER_AGENT_HEADER)
        {
            continue;
        }
        for value in module_ctx_download_header_value_to_strings(name, *value)? {
            headers.push((name.to_owned(), value));
        }
    }

    // Bazel appends these after user-provided headers, so Starlark rules cannot
    // override the repository downloader identity or content-encoding behavior.
    headers.push((
        BAZEL_REPOSITORY_ACCEPT_ENCODING_HEADER.to_owned(),
        BAZEL_REPOSITORY_ACCEPT_ENCODING.to_owned(),
    ));
    headers.push((
        BAZEL_REPOSITORY_USER_AGENT_HEADER.to_owned(),
        bazel_repository_user_agent(),
    ));
    Ok(headers)
}

pub(super) fn repository_ctx_download_to_path(
    urls: Vec<String>,
    output_path: String,
    sha256: &str,
    executable: bool,
    allow_fail: bool,
    integrity: &str,
    canonical_id: &str,
    headers: &[(String, String)],
    auth_headers: &[ModuleCtxDownloadAuthHeader],
    remote_downloader: Option<&BazelRepositoryRemoteDownloaderConfig>,
) -> starlark::Result<(ModuleCtxDownloadResult, bool)> {
    let expected_checksum = match module_ctx_expected_checksum(sha256, integrity) {
        Ok(expected_checksum) => expected_checksum,
        Err(error) => {
            return Ok((
                repository_ctx_download_error_result(allow_fail, error)?,
                false,
            ));
        }
    };
    let write_path = match repository_path_for_write(&output_path) {
        Ok(path) => path,
        Err(error) => {
            return Ok((
                repository_ctx_download_error_result(allow_fail, error)?,
                false,
            ));
        }
    };
    let (got_sha256, got_integrity) = match module_ctx_download_to_path_blocking(
        &urls,
        &write_path,
        expected_checksum.as_ref(),
        canonical_id,
        executable,
        headers,
        auth_headers,
        remote_downloader,
    ) {
        Ok(checksums) => checksums,
        Err(error) => {
            return Ok((
                repository_ctx_download_error_result(allow_fail, error)?,
                false,
            ));
        }
    };
    Ok((
        ModuleCtxDownloadResult::new(true, got_sha256.as_deref(), Some(&got_integrity), None),
        true,
    ))
}

pub(super) fn repository_ctx_extract_archive(
    archive: &Path,
    output: &Path,
    archive_type: &str,
    archive_url: &str,
    strip_prefix: &str,
    strip_components: i32,
    rename_files: &[(String, String)],
) -> buck2_error::Result<()> {
    if strip_components < 0 {
        return Err(BazelRepositoryError::RepositoryCtxExtractArchive {
            archive: archive.to_string_lossy().into_owned(),
            error: format!("strip_components must be non-negative, got {strip_components}"),
        }
        .into());
    }
    let kind = archive_kind_from_type_or_url(
        (!archive_type.is_empty()).then_some(archive_type),
        archive_url,
    )
    .or_else(|| archive_kind_from_type_or_url(None, &archive.to_string_lossy()))
    .ok_or_else(|| BazelRepositoryError::RepositoryCtxExtractArchive {
        archive: archive.to_string_lossy().into_owned(),
        error: "unsupported archive type".to_owned(),
    })?;
    extract_archive(
        archive,
        output,
        kind,
        strip_prefix,
        strip_components as u32,
        rename_files,
    )
    .map_err(|e| BazelRepositoryError::RepositoryCtxExtractArchive {
        archive: archive.to_string_lossy().into_owned(),
        error: e.to_string(),
    })
    .map_err(Into::into)
}

pub(super) fn repository_ctx_renamed_strip_prefix<'a>(
    method: &str,
    strip_prefix: &'a str,
    strip_prefix_legacy: &'a str,
) -> buck2_error::Result<&'a str> {
    if strip_prefix_legacy.is_empty() {
        return Ok(strip_prefix);
    }
    if strip_prefix.is_empty() {
        return Ok(strip_prefix_legacy);
    }
    Err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::Input,
        "{}() got multiple values for parameter 'strip_prefix' (via compatibility alias 'stripPrefix')",
        method
    ))
}

pub(super) fn repository_ctx_rename_files_from_entries(
    entries: &UnpackDictEntries<Value<'_>, Value<'_>>,
) -> starlark::Result<Vec<(String, String)>> {
    let mut rename_files = Vec::new();
    for (from, to) in entries.entries.iter() {
        let Some(from) = from.unpack_str() else {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::RepositoryCtxRenameFilesKeyUnsupportedValue(
                    from.get_type().to_owned(),
                ),
            )
            .into());
        };
        let Some(to) = to.unpack_str() else {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::RepositoryCtxRenameFilesValueUnsupportedValue {
                    path: from.to_owned(),
                    got: to.get_type().to_owned(),
                },
            )
            .into());
        };
        rename_files.push((from.to_owned(), to.to_owned()));
    }
    Ok(rename_files)
}

#[allow(dead_code)]
pub(super) fn module_ctx_urls_from_value<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<String>> {
    if let Some(url) = value.unpack_str() {
        return Ok(vec![url.to_owned()]);
    }

    let mut urls = Vec::new();
    for value in value.iterate(heap).map_err(|_| {
        buck2_error::Error::from(BazelRepositoryError::ModuleCtxDownloadUrlUnsupportedValue(
            value.get_type().to_owned(),
        ))
    })? {
        let Some(url) = value.unpack_str() else {
            return Err(buck2_error::Error::from(
                BazelRepositoryError::ModuleCtxDownloadUrlUnsupportedValue(
                    value.get_type().to_owned(),
                ),
            )
            .into());
        };
        urls.push(url.to_owned());
    }
    if urls.is_empty() {
        return Err(buck2_error::Error::from(BazelRepositoryError::ModuleCtxDownloadNoUrls).into());
    }
    Ok(urls)
}

fn module_ctx_download_error_is_retryable(error: &buck2_http::HttpError) -> bool {
    match error {
        buck2_http::HttpError::Status { status, .. } => {
            let status = status.as_u16();
            matches!(status, 403 | 408 | 429)
                || (500..600).contains(&status) && status != 501 && status != 505
        }
        buck2_http::HttpError::SendRequest { .. } | buck2_http::HttpError::Timeout { .. } => true,
        _ => false,
    }
}

fn module_ctx_download_retry_delay(attempt: usize) -> Duration {
    const MIN_RETRY_DELAY_MS: u64 = 100;
    let shift = attempt.min(6) as u32;
    Duration::from_millis(MIN_RETRY_DELAY_MS.saturating_mul(1u64 << shift))
}

const MODULE_CTX_HTTP_MAX_PARALLEL_DOWNLOADS: usize = 8;
const MODULE_CTX_HTTP_MAX_REDIRECTS: usize = 40;
const MODULE_CTX_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MODULE_CTX_HTTP_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const MODULE_CTX_HTTP_READ_TIMEOUT: Duration = Duration::from_secs(20);
const MODULE_CTX_HTTP_WRITE_TIMEOUT: Duration = Duration::from_secs(20);

static MODULE_CTX_HTTP_DOWNLOAD_SEMAPHORE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
static MODULE_CTX_DOWNLOAD_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn module_ctx_http_download_semaphore() -> &'static tokio::sync::Semaphore {
    MODULE_CTX_HTTP_DOWNLOAD_SEMAPHORE
        .get_or_init(|| {
            Arc::new(tokio::sync::Semaphore::new(
                MODULE_CTX_HTTP_MAX_PARALLEL_DOWNLOADS,
            ))
        })
        .as_ref()
}

#[derive(Clone)]
struct RepositoryRemoteAssetEndpoint {
    uri: String,
    tls: bool,
}

impl RepositoryRemoteAssetEndpoint {
    fn parse(endpoint: &str) -> buck2_error::Result<Self> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "invalid remote downloader endpoint: empty endpoint"
            ));
        }
        if let Some(rest) = endpoint.strip_prefix("grpc://") {
            return Ok(Self {
                uri: format!("http://{rest}"),
                tls: false,
            });
        }
        if let Some(rest) = endpoint.strip_prefix("grpcs://") {
            return Ok(Self {
                uri: format!("https://{rest}"),
                tls: true,
            });
        }
        if endpoint.starts_with("http://") {
            return Ok(Self {
                uri: endpoint.to_owned(),
                tls: false,
            });
        }
        if endpoint.starts_with("https://") {
            return Ok(Self {
                uri: endpoint.to_owned(),
                tls: true,
            });
        }
        Ok(Self {
            uri: format!("https://{endpoint}"),
            tls: true,
        })
    }
}

#[derive(Clone, PartialEq, Message)]
struct RepositoryRemoteAssetQualifier {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, Message)]
struct RepositoryFetchBlobRequest {
    #[prost(string, tag = "1")]
    instance_name: String,
    #[prost(message, optional, tag = "2")]
    timeout: Option<prost_types::Duration>,
    #[prost(message, optional, tag = "3")]
    oldest_content_accepted: Option<prost_types::Timestamp>,
    #[prost(string, repeated, tag = "4")]
    uris: Vec<String>,
    #[prost(message, repeated, tag = "5")]
    qualifiers: Vec<RepositoryRemoteAssetQualifier>,
    #[prost(enumeration = "RepositoryRemoteExecutionDigestFunction", tag = "6")]
    digest_function: i32,
}

#[derive(Clone, PartialEq, Message)]
struct RepositoryFetchBlobResponse {
    #[prost(message, optional, tag = "1")]
    status: Option<RemoteAssetStatus>,
    #[prost(string, tag = "2")]
    uri: String,
    #[prost(message, repeated, tag = "3")]
    qualifiers: Vec<RepositoryRemoteAssetQualifier>,
    #[prost(message, optional, tag = "4")]
    expires_at: Option<prost_types::Timestamp>,
    #[prost(message, optional, tag = "5")]
    blob_digest: Option<RemoteExecutionDigest>,
    #[prost(enumeration = "RepositoryRemoteExecutionDigestFunction", tag = "6")]
    digest_function: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
enum RepositoryRemoteExecutionDigestFunction {
    Unknown = 0,
    Sha256 = 1,
}

#[derive(Debug)]
enum ModuleCtxDownloadAttemptError {
    Retryable(String),
    NonRetryable(String),
    Fatal(buck2_error::Error),
}

enum ModuleCtxChecksumHasher {
    Sha1(Sha1),
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
}

impl ModuleCtxChecksumHasher {
    fn new(kind: ModuleCtxChecksumKind) -> Self {
        match kind {
            ModuleCtxChecksumKind::Sha1 => Self::Sha1(Sha1::new()),
            ModuleCtxChecksumKind::Sha256 => Self::Sha256(Sha256::new()),
            ModuleCtxChecksumKind::Sha384 => Self::Sha384(Sha384::new()),
            ModuleCtxChecksumKind::Sha512 => Self::Sha512(Sha512::new()),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha1(hasher) => hasher.update(bytes),
            Self::Sha256(hasher) => hasher.update(bytes),
            Self::Sha384(hasher) => hasher.update(bytes),
            Self::Sha512(hasher) => hasher.update(bytes),
        }
    }

    fn finalize_hex(self) -> String {
        match self {
            Self::Sha1(hasher) => hex::encode(hasher.finalize()),
            Self::Sha256(hasher) => hex::encode(hasher.finalize()),
            Self::Sha384(hasher) => hex::encode(hasher.finalize()),
            Self::Sha512(hasher) => hex::encode(hasher.finalize()),
        }
    }
}

fn module_ctx_remove_partial_download(path: &Path) {
    let _unused = fs::remove_file(path);
}

fn module_ctx_download_tmp_path(destination: &Path) -> PathBuf {
    let counter =
        MODULE_CTX_DOWNLOAD_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut file_name = std::ffi::OsString::from(".");
    file_name.push(
        destination
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("download")),
    );
    file_name.push(format!(".tmp.{}.{}", std::process::id(), counter));
    destination.with_file_name(file_name)
}

fn module_ctx_prepare_download_tmp(destination: &Path) -> buck2_error::Result<PathBuf> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| module_ctx_download_write_error(parent, error))?;
    }
    let tmp = module_ctx_download_tmp_path(destination);
    module_ctx_remove_partial_download(&tmp);
    Ok(tmp)
}

fn module_ctx_publish_download_tmp(
    tmp: &Path,
    destination: &Path,
    executable: bool,
) -> buck2_error::Result<()> {
    module_ctx_set_executable(tmp, executable)?;
    if let Err(error) = fs::rename(tmp, destination) {
        module_ctx_remove_partial_download(tmp);
        return Err(module_ctx_download_write_error(destination, error));
    }
    Ok(())
}

async fn module_ctx_download_url_to_path(
    client: &buck2_http::HttpClient,
    url: &str,
    destination: &Path,
    expected_checksum: Option<&ModuleCtxChecksum>,
    executable: bool,
    headers: &[(String, String)],
    auth_headers: &[ModuleCtxDownloadAuthHeader],
) -> Result<(Option<String>, String), ModuleCtxDownloadAttemptError> {
    let request_headers = module_ctx_download_request_headers_for_url(url, headers, auth_headers);
    let response = client
        .get_with_headers(url, request_headers.into_iter())
        .await
        .map_err(|error| {
            let retryable = module_ctx_download_error_is_retryable(&error);
            let error = error.to_string();
            if retryable {
                ModuleCtxDownloadAttemptError::Retryable(error)
            } else {
                ModuleCtxDownloadAttemptError::NonRetryable(error)
            }
        })?;

    let tmp_destination = module_ctx_prepare_download_tmp(destination)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)?;
    let mut file = fs::File::create(&tmp_destination).map_err(|error| {
        ModuleCtxDownloadAttemptError::Fatal(module_ctx_download_write_error(
            &tmp_destination,
            error,
        ))
    })?;
    let checksum_kind = expected_checksum
        .map(|checksum| checksum.kind)
        .unwrap_or(ModuleCtxChecksumKind::Sha256);
    let mut hasher = ModuleCtxChecksumHasher::new(checksum_kind);
    let (head, body) = response.into_parts();
    let gunzip = module_ctx_download_response_should_gunzip(url, &head);
    let reader = StreamReader::new(body.map_err(std::io::Error::other));
    if gunzip {
        module_ctx_download_copy_response(
            url,
            &tmp_destination,
            GzipDecoder::new(reader),
            &mut file,
            &mut hasher,
        )
        .await?;
    } else {
        module_ctx_download_copy_response(url, &tmp_destination, reader, &mut file, &mut hasher)
            .await?;
    }

    file.flush().map_err(|error| {
        module_ctx_remove_partial_download(&tmp_destination);
        ModuleCtxDownloadAttemptError::Fatal(module_ctx_download_write_error(
            &tmp_destination,
            error,
        ))
    })?;
    drop(file);

    let got = hasher.finalize_hex();
    let checksum = if let Some(expected_checksum) = expected_checksum {
        if expected_checksum.hex != got {
            module_ctx_remove_partial_download(&tmp_destination);
            return Err(ModuleCtxDownloadAttemptError::NonRetryable(
                BazelRepositoryError::ModuleCtxDownloadChecksumMismatch {
                    path: destination.to_string_lossy().into_owned(),
                    expected: expected_checksum.hex.clone(),
                    got,
                }
                .to_string(),
            ));
        }
        expected_checksum.clone()
    } else {
        ModuleCtxChecksum {
            kind: ModuleCtxChecksumKind::Sha256,
            hex: got,
        }
    };

    module_ctx_publish_download_tmp(&tmp_destination, destination, executable)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)?;
    module_ctx_download_result_checksums_verified(&checksum)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)
}

async fn module_ctx_remote_asset_download_url_to_path(
    config: &BazelRepositoryRemoteDownloaderConfig,
    url: &str,
    destination: &Path,
    expected_checksum: Option<&ModuleCtxChecksum>,
    executable: bool,
    headers: &[(String, String)],
    auth_headers: &[ModuleCtxDownloadAuthHeader],
) -> Result<(Option<String>, String), ModuleCtxDownloadAttemptError> {
    let request_headers = module_ctx_download_request_headers_for_url(url, headers, auth_headers);
    let digest = module_ctx_remote_asset_fetch_blob(
        config,
        &[url.to_owned()],
        expected_checksum,
        &request_headers,
    )
    .await?;
    module_ctx_remote_asset_download_blob(
        config,
        url,
        destination,
        expected_checksum,
        executable,
        &digest,
    )
    .await
}

async fn module_ctx_remote_asset_fetch_blob(
    config: &BazelRepositoryRemoteDownloaderConfig,
    urls: &[String],
    expected_checksum: Option<&ModuleCtxChecksum>,
    headers: &[(&str, &str)],
) -> Result<RemoteExecutionDigest, ModuleCtxDownloadAttemptError> {
    let endpoint = RepositoryRemoteAssetEndpoint::parse(&config.endpoint)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)?;
    let mut endpoint_builder = Endpoint::from_shared(endpoint.uri.clone()).map_err(|error| {
        ModuleCtxDownloadAttemptError::Fatal(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "invalid remote downloader endpoint `{}`: {}",
            config.endpoint,
            error
        ))
    })?;
    if endpoint.tls {
        endpoint_builder = endpoint_builder
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|error| {
                ModuleCtxDownloadAttemptError::Fatal(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "invalid remote downloader endpoint `{}`: {}",
                    config.endpoint,
                    error
                ))
            })?;
    }
    let channel = endpoint_builder
        .connect()
        .await
        .map_err(|error| ModuleCtxDownloadAttemptError::Retryable(error.to_string()))?;
    let mut client = tonic::client::Grpc::new(channel);

    let mut qualifiers = Vec::new();
    if let Some(checksum) = expected_checksum {
        qualifiers.push(RepositoryRemoteAssetQualifier {
            name: "checksum.sri".to_owned(),
            value: module_ctx_checksum_to_subresource_integrity(checksum)
                .map_err(ModuleCtxDownloadAttemptError::Fatal)?,
        });
    }
    for (name, value) in headers {
        qualifiers.push(RepositoryRemoteAssetQualifier {
            name: format!("http_header:{name}"),
            value: (*value).to_owned(),
        });
    }

    let oldest_content_accepted =
        module_ctx_remote_asset_oldest_content_accepted(expected_checksum)
            .map_err(ModuleCtxDownloadAttemptError::Fatal)?;

    let request = RepositoryFetchBlobRequest {
        instance_name: String::new(),
        timeout: Some(prost_types::Duration {
            seconds: 10 * 60,
            nanos: 0,
        }),
        oldest_content_accepted,
        uris: urls.to_owned(),
        qualifiers,
        digest_function: RepositoryRemoteExecutionDigestFunction::Sha256 as i32,
    };
    let mut request = tonic::Request::new(request);
    module_ctx_add_remote_asset_metadata(request.metadata_mut(), config)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)?;

    let path = tonic::codegen::http::uri::PathAndQuery::from_static(
        "/build.bazel.remote.asset.v1.Fetch/FetchBlob",
    );
    let codec =
        tonic_prost::ProstCodec::<RepositoryFetchBlobRequest, RepositoryFetchBlobResponse>::default(
        );
    client
        .ready()
        .await
        .map_err(|error| ModuleCtxDownloadAttemptError::Retryable(error.to_string()))?;
    let response = client
        .unary(request, path, codec)
        .await
        .map_err(|error| ModuleCtxDownloadAttemptError::Retryable(error.to_string()))?
        .into_inner();

    if let Some(status) = &response.status
        && status.code != 0
    {
        return Err(ModuleCtxDownloadAttemptError::NonRetryable(format!(
            "remote downloader returned non-OK status for URLs {:?}: code {}: {}",
            urls, status.code, status.message
        )));
    }
    response.blob_digest.ok_or_else(|| {
        ModuleCtxDownloadAttemptError::Retryable(format!(
            "remote downloader did not return a CAS blob digest for URLs {:?}",
            urls
        ))
    })
}

fn module_ctx_remote_asset_oldest_content_accepted(
    expected_checksum: Option<&ModuleCtxChecksum>,
) -> buck2_error::Result<Option<prost_types::Timestamp>> {
    if expected_checksum.is_some() {
        return Ok(None);
    }

    // Match Bazel's GrpcRemoteDownloader: checksumless downloads are mutable, so never accept
    // cached Remote Asset content. The hour offset allows for clock skew.
    let timestamp = SystemTime::now()
        .checked_add(Duration::from_secs(60 * 60))
        .ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "system time overflow computing remote downloader cache cutoff"
            )
        })?
        .duration_since(UNIX_EPOCH)
        .buck_error_context(
            "system time is before Unix epoch computing remote downloader cache cutoff",
        )?;
    Ok(Some(prost_types::Timestamp {
        seconds: timestamp.as_secs() as i64,
        nanos: timestamp.subsec_nanos() as i32,
    }))
}

async fn module_ctx_remote_asset_download_blob(
    config: &BazelRepositoryRemoteDownloaderConfig,
    url: &str,
    destination: &Path,
    expected_checksum: Option<&ModuleCtxChecksum>,
    executable: bool,
    digest: &RemoteExecutionDigest,
) -> Result<(Option<String>, String), ModuleCtxDownloadAttemptError> {
    let endpoint = RepositoryRemoteAssetEndpoint::parse(&config.endpoint)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)?;
    let mut endpoint_builder = Endpoint::from_shared(endpoint.uri.clone()).map_err(|error| {
        ModuleCtxDownloadAttemptError::Fatal(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "invalid remote downloader endpoint `{}`: {}",
            config.endpoint,
            error
        ))
    })?;
    if endpoint.tls {
        endpoint_builder = endpoint_builder
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|error| {
                ModuleCtxDownloadAttemptError::Fatal(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "invalid remote downloader endpoint `{}`: {}",
                    config.endpoint,
                    error
                ))
            })?;
    }
    let channel = endpoint_builder
        .connect()
        .await
        .map_err(|error| ModuleCtxDownloadAttemptError::Retryable(error.to_string()))?;
    let mut client = ByteStreamClient::new(channel);
    let mut request = tonic::Request::new(ReadRequest {
        resource_name: module_ctx_bytestream_download_resource_name("", digest),
        read_offset: 0,
        read_limit: 0,
    });
    module_ctx_add_remote_asset_metadata(request.metadata_mut(), config)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)?;
    let mut stream = client
        .read(request)
        .await
        .map_err(|error| ModuleCtxDownloadAttemptError::Retryable(error.to_string()))?
        .into_inner();

    let tmp_destination = module_ctx_prepare_download_tmp(destination)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)?;
    let mut file = tokio::fs::File::create(&tmp_destination)
        .await
        .map_err(|error| {
            ModuleCtxDownloadAttemptError::Fatal(module_ctx_download_write_error(
                &tmp_destination,
                error,
            ))
        })?;
    let checksum_kind = expected_checksum
        .map(|checksum| checksum.kind)
        .unwrap_or(ModuleCtxChecksumKind::Sha256);
    let mut hasher = ModuleCtxChecksumHasher::new(checksum_kind);

    while let Some(response) = stream
        .message()
        .await
        .map_err(|error| ModuleCtxDownloadAttemptError::Retryable(error.to_string()))?
    {
        hasher.update(&response.data);
        file.write_all(&response.data).await.map_err(|error| {
            module_ctx_remove_partial_download(&tmp_destination);
            ModuleCtxDownloadAttemptError::Fatal(module_ctx_download_write_error(
                &tmp_destination,
                error,
            ))
        })?;
    }
    file.flush().await.map_err(|error| {
        module_ctx_remove_partial_download(&tmp_destination);
        ModuleCtxDownloadAttemptError::Fatal(module_ctx_download_write_error(
            &tmp_destination,
            error,
        ))
    })?;
    drop(file);

    if module_ctx_remote_asset_blob_should_gunzip(url, &tmp_destination)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)?
    {
        let decoded_tmp_destination = module_ctx_prepare_download_tmp(destination)
            .map_err(ModuleCtxDownloadAttemptError::Fatal)?;
        let source = tokio::fs::File::open(&tmp_destination)
            .await
            .map_err(|error| {
                ModuleCtxDownloadAttemptError::Fatal(module_ctx_download_write_error(
                    &tmp_destination,
                    error,
                ))
            })?;
        let reader = GzipDecoder::new(tokio::io::BufReader::new(source));
        let mut decoded_file = fs::File::create(&decoded_tmp_destination).map_err(|error| {
            ModuleCtxDownloadAttemptError::Fatal(module_ctx_download_write_error(
                &decoded_tmp_destination,
                error,
            ))
        })?;
        let mut decoded_hasher = ModuleCtxChecksumHasher::new(checksum_kind);
        module_ctx_download_copy_response(
            url,
            &decoded_tmp_destination,
            reader,
            &mut decoded_file,
            &mut decoded_hasher,
        )
        .await?;
        decoded_file.flush().map_err(|error| {
            module_ctx_remove_partial_download(&decoded_tmp_destination);
            ModuleCtxDownloadAttemptError::Fatal(module_ctx_download_write_error(
                &decoded_tmp_destination,
                error,
            ))
        })?;
        drop(decoded_file);
        module_ctx_remove_partial_download(&tmp_destination);
        return module_ctx_remote_asset_finish_download_tmp(
            &decoded_tmp_destination,
            destination,
            executable,
            expected_checksum,
            decoded_hasher.finalize_hex(),
        );
    }

    let got = hasher.finalize_hex();
    module_ctx_remote_asset_finish_download_tmp(
        &tmp_destination,
        destination,
        executable,
        expected_checksum,
        got,
    )
}

fn module_ctx_remote_asset_finish_download_tmp(
    tmp_destination: &Path,
    destination: &Path,
    executable: bool,
    expected_checksum: Option<&ModuleCtxChecksum>,
    got: String,
) -> Result<(Option<String>, String), ModuleCtxDownloadAttemptError> {
    let checksum = if let Some(expected_checksum) = expected_checksum {
        if expected_checksum.hex != got {
            module_ctx_remove_partial_download(&tmp_destination);
            return Err(ModuleCtxDownloadAttemptError::NonRetryable(
                BazelRepositoryError::ModuleCtxDownloadChecksumMismatch {
                    path: destination.to_string_lossy().into_owned(),
                    expected: expected_checksum.hex.clone(),
                    got,
                }
                .to_string(),
            ));
        }
        expected_checksum.clone()
    } else {
        ModuleCtxChecksum {
            kind: ModuleCtxChecksumKind::Sha256,
            hex: got,
        }
    };

    module_ctx_publish_download_tmp(&tmp_destination, destination, executable)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)?;
    module_ctx_download_result_checksums_verified(&checksum)
        .map_err(ModuleCtxDownloadAttemptError::Fatal)
}

fn module_ctx_remote_asset_blob_should_gunzip(url: &str, path: &Path) -> buck2_error::Result<bool> {
    if module_ctx_download_url_has_gzipped_extension(url) {
        return Ok(false);
    }

    let mut file = fs::File::open(path).with_buck_error_context(|| {
        format!("error opening remote-downloaded blob `{}`", path.display())
    })?;
    let mut magic = [0u8; 2];
    let bytes_read = file.read(&mut magic).with_buck_error_context(|| {
        format!("error reading remote-downloaded blob `{}`", path.display())
    })?;
    Ok(bytes_read == magic.len() && magic == [0x1f, 0x8b])
}

pub(super) fn module_ctx_download_request_headers_for_url<'a>(
    url: &str,
    headers: &'a [(String, String)],
    auth_headers: &'a [ModuleCtxDownloadAuthHeader],
) -> Vec<(&'a str, &'a str)> {
    let compressed_url = module_ctx_download_url_has_bazel_compressed_extension(url);
    let mut request_headers = headers
        .iter()
        .filter(|(name, _)| {
            !compressed_url || !name.eq_ignore_ascii_case(BAZEL_REPOSITORY_ACCEPT_ENCODING_HEADER)
        })
        .map(|(name, value)| (&**name, &**value))
        .collect::<Vec<_>>();
    request_headers.extend(
        auth_headers
            .iter()
            .filter(|auth| auth.url == url)
            .map(|auth| (&*auth.name, &*auth.value)),
    );
    request_headers
}

async fn module_ctx_download_copy_response(
    url: &str,
    destination: &Path,
    mut reader: impl AsyncRead + Unpin,
    file: &mut fs::File,
    hasher: &mut ModuleCtxChecksumHasher,
) -> Result<(), ModuleCtxDownloadAttemptError> {
    let mut read_buffer = vec![0u8; 64 * 1024];
    loop {
        let bytes_read =
            tokio::time::timeout(MODULE_CTX_HTTP_READ_TIMEOUT, reader.read(&mut read_buffer))
                .await
                .map_err(|_| {
                    module_ctx_remove_partial_download(destination);
                    ModuleCtxDownloadAttemptError::Retryable(format!(
                        "timed out reading {url} after {} seconds",
                        MODULE_CTX_HTTP_READ_TIMEOUT.as_secs()
                    ))
                })?
                .map_err(|error| {
                    module_ctx_remove_partial_download(destination);
                    ModuleCtxDownloadAttemptError::Retryable(error.to_string())
                })?;
        if bytes_read == 0 {
            break;
        }

        let chunk = &read_buffer[..bytes_read];
        hasher.update(chunk);
        file.write_all(chunk).map_err(|error| {
            module_ctx_remove_partial_download(destination);
            ModuleCtxDownloadAttemptError::Fatal(module_ctx_download_write_error(
                destination,
                error,
            ))
        })?;
    }
    Ok(())
}

fn module_ctx_download_response_should_gunzip(url: &str, head: &http::response::Parts) -> bool {
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
    if module_ctx_download_url_has_gzipped_extension(url) {
        return false;
    }
    if let Some(final_uri) = head.extensions.get::<buck2_http::ResponseFinalUri>()
        && module_ctx_download_url_has_gzipped_extension(final_uri.as_str())
    {
        return false;
    }
    true
}

fn module_ctx_download_url_has_bazel_compressed_extension(url: &str) -> bool {
    matches!(
        module_ctx_download_url_extension(url),
        Some("bz2" | "gz" | "jar" | "tgz" | "war" | "xz" | "zip")
    )
}

fn module_ctx_download_url_has_gzipped_extension(url: &str) -> bool {
    matches!(module_ctx_download_url_extension(url), Some("gz" | "tgz"))
}

fn module_ctx_download_url_extension(url: &str) -> Option<&str> {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    let path = path.split_once('#').map_or(path, |(path, _)| path);
    let path = path.rsplit_once('/').map_or(path, |(_, basename)| basename);
    path.rsplit_once('.').map(|(_, extension)| extension)
}

async fn module_ctx_download_to_path_uncached(
    urls: &[String],
    destination: &Path,
    expected_checksum: Option<&ModuleCtxChecksum>,
    executable: bool,
    headers: &[(String, String)],
    auth_headers: &[ModuleCtxDownloadAuthHeader],
    remote_downloader: Option<&BazelRepositoryRemoteDownloaderConfig>,
) -> buck2_error::Result<(Option<String>, String)> {
    const MAX_ATTEMPTS: usize = 8;

    let client = buck2_http::HttpClientBuilder::oss()
        .await?
        .with_max_redirects(MODULE_CTX_HTTP_MAX_REDIRECTS)
        .with_http2(false)
        .with_connect_timeout(Some(MODULE_CTX_HTTP_CONNECT_TIMEOUT))
        .with_response_header_timeout(Some(MODULE_CTX_HTTP_RESPONSE_HEADER_TIMEOUT))
        .with_read_timeout(Some(MODULE_CTX_HTTP_READ_TIMEOUT))
        .with_write_timeout(Some(MODULE_CTX_HTTP_WRITE_TIMEOUT))
        .build();
    let mut last_error = None;
    for url in urls {
        for attempt in 0..MAX_ATTEMPTS {
            let _permit = module_ctx_http_download_semaphore()
                .acquire()
                .await
                .map_err(|error| {
                    buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "could not acquire module_ctx.download semaphore: {}",
                        error
                    )
                })?;
            let result = if let Some(remote_downloader) = remote_downloader {
                module_ctx_remote_asset_download_url_to_path(
                    remote_downloader,
                    url,
                    destination,
                    expected_checksum,
                    executable,
                    headers,
                    auth_headers,
                )
                .await
            } else {
                module_ctx_download_url_to_path(
                    &client,
                    url,
                    destination,
                    expected_checksum,
                    executable,
                    headers,
                    auth_headers,
                )
                .await
            };
            drop(_permit);

            match result {
                Ok(checksums) => return Ok(checksums),
                Err(ModuleCtxDownloadAttemptError::Fatal(error)) => return Err(error),
                Err(ModuleCtxDownloadAttemptError::NonRetryable(error)) => {
                    last_error = Some(error);
                    break;
                }
                Err(ModuleCtxDownloadAttemptError::Retryable(error)) => {
                    last_error = Some(error);
                    if attempt + 1 == MAX_ATTEMPTS {
                        break;
                    }
                    tokio::time::sleep(module_ctx_download_retry_delay(attempt)).await;
                }
            }
        }
    }

    Err(BazelRepositoryError::ModuleCtxDownloadFailed {
        urls: urls.to_owned(),
        error: last_error.unwrap_or_else(|| "no URL attempted".to_owned()),
    }
    .into())
}

fn module_ctx_download_to_path_uncached_blocking(
    urls: &[String],
    destination: &Path,
    expected_checksum: Option<&ModuleCtxChecksum>,
    executable: bool,
    headers: &[(String, String)],
    auth_headers: &[ModuleCtxDownloadAuthHeader],
    remote_downloader: Option<&BazelRepositoryRemoteDownloaderConfig>,
) -> buck2_error::Result<(Option<String>, String)> {
    let urls = urls.to_owned();
    let destination = destination.to_owned();
    let expected_checksum = expected_checksum.cloned();
    let headers = headers.to_owned();
    let auth_headers = auth_headers.to_owned();
    let remote_downloader = remote_downloader.cloned();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "could not create module_ctx.download runtime: {}",
                    e
                )
            })?
            .block_on(async move {
                module_ctx_download_to_path_uncached(
                    &urls,
                    &destination,
                    expected_checksum.as_ref(),
                    executable,
                    &headers,
                    &auth_headers,
                    remote_downloader.as_ref(),
                )
                .await
            })
    })
    .join()
    .map_err(|_| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "module_ctx.download worker thread panicked"
        )
    })?
}

pub(super) static MODULE_CTX_DOWNLOAD_CACHE_LOCKS: OnceLock<
    Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModuleCtxVerifiedDownloadCacheMetadata {
    len: u64,
    modified: Option<SystemTime>,
}

static MODULE_CTX_VERIFIED_DOWNLOAD_CACHE: OnceLock<
    Mutex<BTreeMap<String, ModuleCtxVerifiedDownloadCacheMetadata>>,
> = OnceLock::new();

pub(super) fn module_ctx_download_cache_lock(key: &str) -> Arc<Mutex<()>> {
    let locks = MODULE_CTX_DOWNLOAD_CACHE_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks
        .lock()
        .expect("module ctx download cache lock map is poisoned");
    locks
        .entry(key.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub(super) fn module_ctx_download_cache_release_lock(key: &str, lock: &Arc<Mutex<()>>) {
    let Some(locks) = MODULE_CTX_DOWNLOAD_CACHE_LOCKS.get() else {
        return;
    };
    let mut locks = locks
        .lock()
        .expect("module ctx download cache lock map is poisoned");
    if matches!(locks.get(key), Some(stored) if Arc::ptr_eq(stored, lock))
        && Arc::strong_count(lock) == 2
    {
        locks.remove(key);
    }
}

fn module_ctx_download_cache_verification_key(
    file: &Path,
    checksum: &ModuleCtxChecksum,
    canonical_id: &str,
) -> String {
    format!(
        "{}:{}:{}:{}",
        file.to_string_lossy(),
        checksum.kind.repository_cache_dir_name(),
        checksum.hex,
        canonical_id
    )
}

fn module_ctx_download_cache_file_metadata(
    file: &Path,
) -> buck2_error::Result<ModuleCtxVerifiedDownloadCacheMetadata> {
    let metadata = fs::metadata(file)
        .map_err(|error| module_ctx_download_cache_io_error("stat", file, error))?;
    Ok(ModuleCtxVerifiedDownloadCacheMetadata {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn module_ctx_download_cache_is_verified(
    key: &str,
    metadata: ModuleCtxVerifiedDownloadCacheMetadata,
) -> bool {
    let verified = MODULE_CTX_VERIFIED_DOWNLOAD_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    verified
        .lock()
        .expect("module ctx verified download cache is poisoned")
        .get(key)
        .copied()
        == Some(metadata)
}

fn module_ctx_download_cache_record_verified(
    key: String,
    metadata: ModuleCtxVerifiedDownloadCacheMetadata,
) {
    let verified = MODULE_CTX_VERIFIED_DOWNLOAD_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    verified
        .lock()
        .expect("module ctx verified download cache is poisoned")
        .insert(key, metadata);
}

fn module_ctx_repository_cache_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("BUCK2_BAZEL_REPOSITORY_CACHE") {
        if path.is_empty() {
            return None;
        }
        return Some(PathBuf::from(path));
    }
    Some(
        PathBuf::from(env::var_os("HOME")?)
            .join(".cache")
            .join("buck2")
            .join("cache")
            .join("repos")
            .join("v1"),
    )
}

fn module_ctx_repository_cache_entry_dir(checksum: &ModuleCtxChecksum) -> Option<PathBuf> {
    Some(
        module_ctx_repository_cache_path()?
            .join("content_addressable")
            .join(checksum.kind.repository_cache_dir_name())
            .join(&checksum.hex),
    )
}

fn module_ctx_repository_cache_id_path(
    entry: &Path,
    checksum: &ModuleCtxChecksum,
    canonical_id: &str,
) -> Option<PathBuf> {
    if canonical_id.is_empty() {
        return None;
    }
    Some(entry.join(format!(
        "id-{}",
        module_ctx_checksum_hex(checksum.kind, canonical_id.as_bytes())
    )))
}

fn module_ctx_download_cache_io_error(
    action: &str,
    path: &Path,
    error: std::io::Error,
) -> buck2_error::Error {
    buck2_error::buck2_error!(
        buck2_error::ErrorTag::Input,
        "failed to {} Bazel repository cache path `{}`: {}",
        action,
        path.display(),
        error
    )
}

fn module_ctx_download_write_error(path: &Path, error: std::io::Error) -> buck2_error::Error {
    BazelRepositoryError::ModuleCtxDownloadWriteFile {
        path: path.to_string_lossy().into_owned(),
        error: error.to_string(),
    }
    .into()
}

fn module_ctx_set_executable(path: &Path, executable: bool) -> buck2_error::Result<()> {
    if executable {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                .map_err(|error| module_ctx_download_write_error(path, error))?;
        }
    }
    Ok(())
}

pub(super) fn module_ctx_copy_download_file(
    source: &Path,
    destination: &Path,
    executable: bool,
) -> buck2_error::Result<()> {
    let tmp = module_ctx_prepare_download_tmp(destination)?;
    match fs::copy(source, &tmp) {
        Ok(_) => module_ctx_publish_download_tmp(&tmp, destination, executable),
        Err(error) => {
            module_ctx_remove_partial_download(&tmp);
            Err(module_ctx_download_write_error(destination, error))
        }
    }
}

fn module_ctx_download_cache_get_to_path(
    checksum: &ModuleCtxChecksum,
    canonical_id: &str,
    destination: &Path,
    executable: bool,
) -> buck2_error::Result<bool> {
    let Some(entry) = module_ctx_repository_cache_entry_dir(checksum) else {
        return Ok(false);
    };
    let file = entry.join("file");
    if !file.is_file() {
        return Ok(false);
    }
    if let Some(id_path) = module_ctx_repository_cache_id_path(&entry, checksum, canonical_id)
        && !id_path.exists()
    {
        return Ok(false);
    }
    let verification_key =
        module_ctx_download_cache_verification_key(&file, checksum, canonical_id);
    let metadata = module_ctx_download_cache_file_metadata(&file)?;
    if !module_ctx_download_cache_is_verified(&verification_key, metadata) {
        module_ctx_validate_download_file_checksum(&file, checksum)?;
        module_ctx_download_cache_record_verified(verification_key, metadata);
    }
    module_ctx_copy_download_file(&file, destination, executable)?;
    Ok(true)
}

fn module_ctx_download_cache_put_verified(
    checksum: &ModuleCtxChecksum,
    canonical_id: &str,
    source: &Path,
) -> buck2_error::Result<()> {
    let Some(entry) = module_ctx_repository_cache_entry_dir(checksum) else {
        return Ok(());
    };
    fs::create_dir_all(&entry)
        .map_err(|error| module_ctx_download_cache_io_error("create", &entry, error))?;
    let file = entry.join("file");
    if !file.is_file() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let tmp = entry.join(format!("tmp-{}-{}", std::process::id(), nanos));
        fs::copy(source, &tmp)
            .map_err(|error| module_ctx_download_cache_io_error("write", &tmp, error))?;
        if let Err(error) = fs::rename(&tmp, &file) {
            let _unused = fs::remove_file(&tmp);
            if !file.is_file() {
                return Err(module_ctx_download_cache_io_error("rename", &file, error));
            }
        }
    }
    if let Some(id_path) = module_ctx_repository_cache_id_path(&entry, checksum, canonical_id) {
        fs::write(&id_path, b"")
            .map_err(|error| module_ctx_download_cache_io_error("write", &id_path, error))?;
    }
    let verification_key =
        module_ctx_download_cache_verification_key(&file, checksum, canonical_id);
    let metadata = module_ctx_download_cache_file_metadata(&file)?;
    module_ctx_download_cache_record_verified(verification_key, metadata);
    Ok(())
}

pub(super) fn module_ctx_download_to_path_blocking(
    urls: &[String],
    destination: &Path,
    expected_checksum: Option<&ModuleCtxChecksum>,
    canonical_id: &str,
    executable: bool,
    headers: &[(String, String)],
    auth_headers: &[ModuleCtxDownloadAuthHeader],
    remote_downloader: Option<&BazelRepositoryRemoteDownloaderConfig>,
) -> buck2_error::Result<(Option<String>, String)> {
    if let Some(expected_checksum) = expected_checksum {
        if destination.is_file()
            && module_ctx_validate_download_file_checksum(destination, expected_checksum).is_ok()
        {
            module_ctx_set_executable(destination, executable)?;
            return module_ctx_download_result_checksums_verified(expected_checksum);
        }

        let lock_key = format!(
            "{}:{}:{}",
            expected_checksum.kind.repository_cache_dir_name(),
            expected_checksum.hex,
            canonical_id
        );
        let lock = module_ctx_download_cache_lock(&lock_key);
        let result: buck2_error::Result<(Option<String>, String)> = {
            let _guard = lock
                .lock()
                .expect("module ctx download cache entry lock is poisoned");
            (|| {
                if destination.is_file()
                    && module_ctx_validate_download_file_checksum(destination, expected_checksum)
                        .is_ok()
                {
                    module_ctx_set_executable(destination, executable)?;
                    return module_ctx_download_result_checksums_verified(expected_checksum);
                }
                if module_ctx_download_cache_get_to_path(
                    expected_checksum,
                    canonical_id,
                    destination,
                    executable,
                )
                .unwrap_or(false)
                {
                    return module_ctx_download_result_checksums_verified(expected_checksum);
                }

                buck2_events::dispatch::span(
                    buck2_data::DiceStateUpdateStageStart {
                        stage: module_ctx_download_stage_label(urls, destination),
                    },
                    || {
                        (
                            module_ctx_download_to_path_uncached_blocking(
                                urls,
                                destination,
                                Some(expected_checksum),
                                executable,
                                headers,
                                auth_headers,
                                remote_downloader,
                            ),
                            buck2_data::DiceStateUpdateStageEnd {},
                        )
                    },
                )?;
                module_ctx_download_cache_put_verified(
                    expected_checksum,
                    canonical_id,
                    destination,
                )?;
                module_ctx_download_result_checksums_verified(expected_checksum)
            })()
        };
        module_ctx_download_cache_release_lock(&lock_key, &lock);
        return result;
    }

    let checksums = buck2_events::dispatch::span(
        buck2_data::DiceStateUpdateStageStart {
            stage: module_ctx_download_stage_label(urls, destination),
        },
        || {
            (
                module_ctx_download_to_path_uncached_blocking(
                    urls,
                    destination,
                    None,
                    executable,
                    headers,
                    auth_headers,
                    remote_downloader,
                ),
                buck2_data::DiceStateUpdateStageEnd {},
            )
        },
    )?;
    if let Some(sha256) = &checksums.0 {
        module_ctx_download_cache_put_verified(
            &ModuleCtxChecksum {
                kind: ModuleCtxChecksumKind::Sha256,
                hex: sha256.clone(),
            },
            canonical_id,
            destination,
        )?;
    }
    Ok(checksums)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ModuleCtxChecksumKind {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl ModuleCtxChecksumKind {
    fn integrity_prefix(&self) -> &'static str {
        match self {
            Self::Sha1 => "sha1-",
            Self::Sha256 => "sha256-",
            Self::Sha384 => "sha384-",
            Self::Sha512 => "sha512-",
        }
    }

    fn byte_len(&self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
        }
    }

    fn repository_cache_dir_name(&self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ModuleCtxChecksum {
    pub(super) kind: ModuleCtxChecksumKind,
    pub(super) hex: String,
}

pub(super) fn module_ctx_expected_checksum(
    sha256: &str,
    integrity: &str,
) -> buck2_error::Result<Option<ModuleCtxChecksum>> {
    if !sha256.is_empty() && !integrity.is_empty() {
        return Err(BazelRepositoryError::ModuleCtxDownloadConflictingChecksums.into());
    }
    if !sha256.is_empty() {
        return Ok(Some(ModuleCtxChecksum {
            kind: ModuleCtxChecksumKind::Sha256,
            hex: sha256.to_ascii_lowercase(),
        }));
    }
    module_ctx_checksum_from_integrity(integrity)
}

pub(super) fn module_ctx_checksum_from_integrity(
    integrity: &str,
) -> buck2_error::Result<Option<ModuleCtxChecksum>> {
    if integrity.is_empty() {
        return Ok(None);
    }
    for kind in [
        ModuleCtxChecksumKind::Sha1,
        ModuleCtxChecksumKind::Sha256,
        ModuleCtxChecksumKind::Sha384,
        ModuleCtxChecksumKind::Sha512,
    ] {
        if let Some(encoded) = integrity.strip_prefix(kind.integrity_prefix()) {
            let bytes = BASE64_STANDARD.decode(encoded).map_err(|_| {
                BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(integrity.to_owned())
            })?;
            if bytes.len() != kind.byte_len() {
                return Err(BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(
                    integrity.to_owned(),
                )
                .into());
            }
            return Ok(Some(ModuleCtxChecksum {
                kind,
                hex: hex::encode(bytes),
            }));
        }
    }
    Err(BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(integrity.to_owned()).into())
}

fn module_ctx_checksum_to_subresource_integrity(
    checksum: &ModuleCtxChecksum,
) -> buck2_error::Result<String> {
    let bytes = hex::decode(&checksum.hex)?;
    Ok(format!(
        "{}{}",
        checksum.kind.integrity_prefix(),
        BASE64_STANDARD.encode(bytes)
    ))
}

fn module_ctx_add_remote_asset_metadata(
    metadata: &mut tonic::metadata::MetadataMap,
    config: &BazelRepositoryRemoteDownloaderConfig,
) -> buck2_error::Result<()> {
    if let Some(api_key) = config
        .api_key
        .as_ref()
        .filter(|api_key| !api_key.trim().is_empty())
    {
        metadata.insert(
            BUILDBUDDY_API_KEY_HEADER,
            MetadataValue::try_from(api_key.as_str()).map_err(|error| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "invalid `{BUILDBUDDY_API_KEY_HEADER}` metadata value: {error}"
                )
            })?,
        );
    }
    Ok(())
}

fn module_ctx_bytestream_download_resource_name(
    instance_name: &str,
    digest: &RemoteExecutionDigest,
) -> String {
    let blob = format!("blobs/{}/{}", digest.hash, digest.size_bytes);
    if instance_name.is_empty() {
        blob
    } else {
        format!("{instance_name}/{blob}")
    }
}

fn module_ctx_checksum_hex(kind: ModuleCtxChecksumKind, bytes: &[u8]) -> String {
    match kind {
        ModuleCtxChecksumKind::Sha1 => hex::encode(Sha1::digest(bytes)),
        ModuleCtxChecksumKind::Sha256 => hex::encode(Sha256::digest(bytes)),
        ModuleCtxChecksumKind::Sha384 => hex::encode(Sha384::digest(bytes)),
        ModuleCtxChecksumKind::Sha512 => hex::encode(Sha512::digest(bytes)),
    }
}

fn module_ctx_checksum_hex_file(
    kind: ModuleCtxChecksumKind,
    path: &Path,
) -> buck2_error::Result<String> {
    fn read_chunks(path: &Path, mut update: impl FnMut(&[u8])) -> buck2_error::Result<()> {
        let mut file = fs::File::open(path).map_err(|error| {
            buck2_error::Error::from(BazelRepositoryError::ModuleCtxReadFile {
                path: path.to_string_lossy().into_owned(),
                error: error.to_string(),
            })
        })?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let bytes_read = file.read(&mut buffer).map_err(|error| {
                buck2_error::Error::from(BazelRepositoryError::ModuleCtxReadFile {
                    path: path.to_string_lossy().into_owned(),
                    error: error.to_string(),
                })
            })?;
            if bytes_read == 0 {
                return Ok(());
            }
            update(&buffer[..bytes_read]);
        }
    }

    match kind {
        ModuleCtxChecksumKind::Sha1 => {
            let mut hasher = Sha1::new();
            read_chunks(path, |bytes| hasher.update(bytes))?;
            Ok(hex::encode(hasher.finalize()))
        }
        ModuleCtxChecksumKind::Sha256 => {
            let mut hasher = Sha256::new();
            read_chunks(path, |bytes| hasher.update(bytes))?;
            Ok(hex::encode(hasher.finalize()))
        }
        ModuleCtxChecksumKind::Sha384 => {
            let mut hasher = Sha384::new();
            read_chunks(path, |bytes| hasher.update(bytes))?;
            Ok(hex::encode(hasher.finalize()))
        }
        ModuleCtxChecksumKind::Sha512 => {
            let mut hasher = Sha512::new();
            read_chunks(path, |bytes| hasher.update(bytes))?;
            Ok(hex::encode(hasher.finalize()))
        }
    }
}

pub(super) fn module_ctx_integrity_from_checksum(
    checksum: &ModuleCtxChecksum,
) -> buck2_error::Result<String> {
    let bytes = hex::decode(&checksum.hex).map_err(|_| {
        BazelRepositoryError::ModuleCtxDownloadUnsupportedIntegrity(checksum.hex.clone())
    })?;
    Ok(format!(
        "{}{}",
        checksum.kind.integrity_prefix(),
        BASE64_STANDARD.encode(bytes)
    ))
}

fn module_ctx_validate_download_file_checksum(
    path: &Path,
    expected_checksum: &ModuleCtxChecksum,
) -> buck2_error::Result<()> {
    let got = module_ctx_checksum_hex_file(expected_checksum.kind, path)?;
    if expected_checksum.hex == got {
        return Ok(());
    }
    Err(BazelRepositoryError::ModuleCtxDownloadChecksumMismatch {
        path: path.to_string_lossy().into_owned(),
        expected: expected_checksum.hex.clone(),
        got,
    }
    .into())
}

fn module_ctx_download_result_checksums_verified(
    expected_checksum: &ModuleCtxChecksum,
) -> buck2_error::Result<(Option<String>, String)> {
    let sha256 = (expected_checksum.kind == ModuleCtxChecksumKind::Sha256)
        .then(|| expected_checksum.hex.clone());
    let integrity = module_ctx_integrity_from_checksum(expected_checksum)?;
    Ok((sha256, integrity))
}

fn module_ctx_download_display_url(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let Some((authority, path)) = rest.split_once('/') else {
        let authority = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        return format!("{scheme}://{authority}");
    };
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{authority}/{path}")
}

fn module_ctx_download_stage_label(urls: &[String], destination: &Path) -> String {
    let display_url = urls
        .first()
        .map(|url| module_ctx_download_display_url(url))
        .unwrap_or_else(|| destination.to_string_lossy().into_owned());
    let mirrors = urls.len().saturating_sub(1);
    if mirrors == 0 {
        format!("downloading {display_url}")
    } else {
        format!("downloading {display_url} (+{mirrors} mirrors)")
    }
}

#[derive(Debug, Clone, Allocative)]
pub(super) struct ModuleCtxDownloadResult {
    success: bool,
    sha256: Option<String>,
    integrity: Option<String>,
    error: Option<String>,
}

impl ModuleCtxDownloadResult {
    pub(super) fn new(
        success: bool,
        sha256: Option<&str>,
        integrity: Option<&str>,
        error: Option<&str>,
    ) -> Self {
        Self {
            success,
            sha256: sha256.map(str::to_owned),
            integrity: integrity.map(str::to_owned),
            error: error.map(str::to_owned),
        }
    }

    fn alloc<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        let success = heap.alloc(self.success);
        let mut fields = Vec::new();
        fields.push(("success", success));
        if let Some(sha256) = &self.sha256 {
            fields.push(("sha256", heap.alloc_str(sha256).to_value()));
        }
        if let Some(integrity) = &self.integrity {
            fields.push(("integrity", heap.alloc_str(integrity).to_value()));
        }
        if let Some(error) = &self.error {
            fields.push(("error", heap.alloc_str(error).to_value()));
        }
        heap.alloc(AllocStruct(fields))
    }
}

#[derive(Debug, ProvidesStaticType, Trace, NoSerialize, Allocative)]
struct StarlarkPendingDownload<'v> {
    #[trace(unsafe_ignore)]
    result: ModuleCtxDownloadResult,
    _marker: std::marker::PhantomData<&'v ()>,
}

impl<'v> AllocValue<'v> for StarlarkPendingDownload<'v> {
    fn alloc_value(self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

impl<'v> Freeze for StarlarkPendingDownload<'v> {
    type Frozen = FrozenStarlarkPendingDownload;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(FrozenStarlarkPendingDownload {
            result: self.result,
        })
    }
}

impl<'v> Display for StarlarkPendingDownload<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<pending download>")
    }
}

#[starlark_value(type = "pending_download")]
impl<'v> StarlarkValue<'v> for StarlarkPendingDownload<'v> {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(pending_download_methods)
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct FrozenStarlarkPendingDownload {
    result: ModuleCtxDownloadResult,
}

impl Display for FrozenStarlarkPendingDownload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<pending download>")
    }
}

starlark_simple_value!(FrozenStarlarkPendingDownload);

#[starlark_value(type = "pending_download")]
impl<'v> StarlarkValue<'v> for FrozenStarlarkPendingDownload {
    type Canonical = StarlarkPendingDownload<'v>;

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(pending_download_methods)
    }
}

#[starlark_module]
fn pending_download_methods(builder: &mut MethodsBuilder) {
    fn wait<'v>(
        this: ValueTypedComplex<'v, StarlarkPendingDownload<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(match this.unpack() {
            either::Either::Left(download) => download.result.alloc(eval.heap()),
            either::Either::Right(download) => download.result.alloc(eval.heap()),
        })
    }
}

pub(super) fn module_ctx_pending_download<'v>(
    block: bool,
    result: ModuleCtxDownloadResult,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    if block {
        result.alloc(eval.heap())
    } else {
        eval.heap().alloc(StarlarkPendingDownload {
            result,
            _marker: std::marker::PhantomData,
        })
    }
}

pub(super) fn module_ctx_download_error_with_block<'v>(
    block: bool,
    allow_fail: bool,
    error: buck2_error::Error,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let result = repository_ctx_download_error_result(allow_fail, error)?;
    Ok(module_ctx_pending_download(block, result, eval))
}
