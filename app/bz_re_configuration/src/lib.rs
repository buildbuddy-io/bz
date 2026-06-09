/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![feature(error_generic_member_access)]

use std::str::FromStr;

use allocative::Allocative;
use bz_common::init::BUILDBUDDY_API_KEY_HEADER;
use bz_common::init::RemoteExecutionStartupConfig;
use bz_common::legacy_configs::configs::LegacyBuckConfig;
use bz_common::legacy_configs::key::BuckconfigKeyRef;
use bz_core::rollout_percentage::RolloutPercentage;

static BUCK2_RE_CLIENT_CFG_SECTION: &str = "buck2_re_client";

fn available_parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, |value| value.get())
}

/// We put functions here that both things need to implement for code that isn't gated behind a
/// fbcode_build or not(fbcode_build)
pub trait RemoteExecutionStaticMetadataImpl: Sized {
    fn from_legacy_config(legacy_config: &LegacyBuckConfig) -> bz_error::Result<Self>;
    fn apply_remote_execution_startup_config(
        &mut self,
        config: &RemoteExecutionStartupConfig,
    ) -> bz_error::Result<()>;
    fn remote_action_building_semaphore_size(&self) -> usize;
    fn exec_semaphore_size(&self) -> usize;
}

#[derive(Clone, Debug)]
struct ParsedBazelRemoteEndpoint {
    address: String,
    tls: Option<bool>,
}

struct ResolvedBazelRemoteExecutionStartupConfig {
    cas_address: Option<Option<String>>,
    action_cache_address: Option<Option<String>>,
    engine_address: Option<Option<String>>,
    tls: Option<bool>,
}

fn parse_bazel_remote_endpoint(
    value: &str,
) -> bz_error::Result<Option<ParsedBazelRemoteEndpoint>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    let (address, tls) = if let Some(rest) = value.strip_prefix("grpc://") {
        (rest, Some(false))
    } else if let Some(rest) = value.strip_prefix("grpcs://") {
        (rest, Some(true))
    } else if let Some(rest) = value.strip_prefix("http://") {
        (rest, Some(false))
    } else if let Some(rest) = value.strip_prefix("https://") {
        (rest, Some(true))
    } else if value.starts_with("unix:") {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "bz does not currently support Bazel remote endpoint `{}`: unix socket endpoints are unsupported",
            value
        ));
    } else {
        // Bazel defaults scheme-less remote endpoints to grpcs.
        (value, Some(true))
    };

    if address.is_empty() {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "Invalid empty Bazel remote endpoint `{}`",
            value
        ));
    }

    Ok(Some(ParsedBazelRemoteEndpoint {
        address: address.to_owned(),
        tls,
    }))
}

fn resolve_bazel_remote_execution_startup_config(
    config: &RemoteExecutionStartupConfig,
    cache_address_configured: bool,
) -> bz_error::Result<ResolvedBazelRemoteExecutionStartupConfig> {
    let remote_executor = config
        .remote_executor
        .as_deref()
        .map(parse_bazel_remote_endpoint)
        .transpose()?
        .flatten();
    let remote_cache = config
        .remote_cache
        .as_deref()
        .map(parse_bazel_remote_endpoint)
        .transpose()?
        .flatten();

    let mut tls = None;
    for endpoint in [&remote_cache, &remote_executor].into_iter().flatten() {
        if let Some(endpoint_tls) = endpoint.tls {
            match tls {
                Some(existing) if existing != endpoint_tls => {
                    return Err(bz_error::bz_error!(
                        bz_error::ErrorTag::Input,
                        "bz remote execution currently requires --remote_cache and --remote_executor to use the same TLS mode"
                    ));
                }
                Some(_) => {}
                None => tls = Some(endpoint_tls),
            }
        }
    }

    let engine_address = if let Some(remote_executor) = &remote_executor {
        Some(Some(remote_executor.address.clone()))
    } else if config.remote_executor.is_some() {
        Some(None)
    } else {
        None
    };

    let (cas_address, action_cache_address) = match &remote_cache {
        Some(remote_cache) => (
            Some(Some(remote_cache.address.clone())),
            Some(Some(remote_cache.address.clone())),
        ),
        None if config.remote_cache.is_some() => {
            if let Some(remote_executor) = &remote_executor {
                (
                    Some(Some(remote_executor.address.clone())),
                    Some(Some(remote_executor.address.clone())),
                )
            } else {
                (Some(None), Some(None))
            }
        }
        None => {
            if let Some(remote_executor) = &remote_executor {
                if cache_address_configured {
                    (None, None)
                } else {
                    (
                        Some(Some(remote_executor.address.clone())),
                        Some(Some(remote_executor.address.clone())),
                    )
                }
            } else {
                (None, None)
            }
        }
    };

    Ok(ResolvedBazelRemoteExecutionStartupConfig {
        cas_address,
        action_cache_address,
        engine_address,
        tls,
    })
}

#[derive(Clone, Debug, Allocative)]
pub enum CASdAddress {
    Tcp(u16),
    Uds(String),
}

impl FromStr for CASdAddress {
    type Err = bz_error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(path) = s.strip_prefix("unix://") {
            Ok(CASdAddress::Uds(path.to_owned()))
        } else {
            Ok(CASdAddress::Tcp(s.parse()?))
        }
    }
}

#[derive(Clone, Debug, Allocative)]
pub enum CASdMode {
    LocalWithSync,
    LocalWithoutSync,
    Remote,
    RemoteToDest,
}

impl FromStr for CASdMode {
    type Err = bz_error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local_with_sync" => Ok(CASdMode::LocalWithSync),
            "local_without_sync" => Ok(CASdMode::LocalWithoutSync),
            "remote" => Ok(CASdMode::Remote),
            "remote_to_dest" => Ok(CASdMode::RemoteToDest),
            _ => Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "Invalid CASd mode: {}",
                s
            )),
        }
    }
}

#[derive(Clone, Debug, Allocative)]
pub enum CopyPolicy {
    Copy,
    Reflink,
    Hybrid,
}

impl FromStr for CopyPolicy {
    type Err = bz_error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hybrid" => Ok(CopyPolicy::Hybrid),
            "reflink" => Ok(CopyPolicy::Reflink),
            _ => Ok(CopyPolicy::Copy),
        }
    }
}

#[allow(unused)]
mod fbcode {
    use bz_common::legacy_configs::key::BuckconfigKeyRef;

    use super::*;

    /// Metadata that doesn't change between executions
    #[derive(Clone, Debug, Default, Allocative)]
    pub struct RemoteExecutionStaticMetadata {
        // gRPC settings
        pub cas_address: Option<String>,
        pub cas_connection_count: i32,
        pub shared_casd_cache_path: Option<String>,
        pub legacy_shared_casd_mode: Option<String>,
        pub shared_casd_mode_small_files: Option<CASdMode>,
        pub shared_casd_mode_large_files: Option<CASdMode>,
        pub shared_casd_cache_sync_wal_files_count: Option<u8>,
        pub shared_casd_cache_sync_wal_file_max_size: Option<u64>,
        pub shared_casd_cache_sync_max_batch_size: Option<u32>,
        pub shared_casd_cache_sync_max_delay_ms: Option<u32>,
        pub shared_casd_copy_policy: Option<CopyPolicy>,
        pub shared_casd_address: Option<CASdAddress>,
        pub shared_casd_use_tls: Option<bool>,
        pub cas_client_label: Option<String>,
        pub action_cache_address: Option<String>,
        pub action_cache_connection_count: i32,
        pub engine_address: Option<String>,
        pub engine_connection_count: i32,
        // End gRPC settings
        pub verbose_logging: bool,

        pub use_manifold_rich_client: bool,
        pub use_zippy_rich_client: bool,
        pub use_p2p: bool,

        pub cas_thread_count: i32,
        pub cas_thread_count_ratio: f32,

        pub rich_client_channels_per_blob: Option<i32>,
        pub rich_client_attempt_timeout_ms: Option<i32>,
        pub rich_client_retries_count: Option<i32>,
        pub force_enable_deduplicate_find_missing: Option<bool>,

        pub features_config_path: Option<String>,
        pub client_config_path: Option<String>,

        // curl reactor
        pub curl_reactor_max_number_of_retries: Option<i32>,
        pub curl_reactor_connection_timeout_ms: Option<i32>,
        pub curl_reactor_request_timeout_ms: Option<i32>,

        // ttl management
        pub minimal_blob_ttl_seconds: Option<i64>,
        // When less than (X*100)% of TTL remains, refresh data in the store
        pub remaining_ttl_fraction_refresh_threshold: Option<f32>,
        // Adds a randomness to when refresh the TTL
        pub remaining_ttl_random_extra_threshold: Option<f32>,

        pub disable_fallocate: bool,
        pub respect_file_symlinks: bool,

        // Thrift settings
        pub execution_concurrency_limit: i32,
        pub engine_tier: Option<String>,
        pub engine_host: Option<String>,
        pub engine_port: Option<i32>,
        // End Thrift settings
        /// When set to True, allows for cancellation of RE downloads when futures are dropped
        pub enable_download_cancellation: bool,
    }

    impl RemoteExecutionStaticMetadataImpl for RemoteExecutionStaticMetadata {
        fn from_legacy_config(legacy_config: &LegacyBuckConfig) -> bz_error::Result<Self> {
            Ok(Self {
                cas_address: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_address",
                })?,
                cas_connection_count: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "cas_connection_count",
                    })?
                    .unwrap_or(16),
                shared_casd_cache_path: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_shared_cache",
                })?,
                legacy_shared_casd_mode: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_shared_cache_mode",
                })?,
                shared_casd_mode_small_files: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_shared_cache_mode_small_files_v2",
                })?,
                shared_casd_mode_large_files: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_shared_cache_mode_large_files_v2",
                })?,
                shared_casd_cache_sync_wal_files_count: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_shared_cache_sync_wal_files_count_v2",
                })?,
                shared_casd_cache_sync_wal_file_max_size: legacy_config.parse(
                    BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "cas_shared_cache_sync_wal_file_max_size_v2",
                    },
                )?,
                shared_casd_cache_sync_max_batch_size: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_shared_cache_sync_max_batch_size_v2",
                })?,
                shared_casd_cache_sync_max_delay_ms: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_shared_cache_sync_max_delay_ms_v2",
                })?,
                shared_casd_copy_policy: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_shared_cache_copy_policy_v2",
                })?,
                shared_casd_address: {
                    let port_result = legacy_config.parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "cas_shared_cache_port",
                    });
                    match port_result {
                        Ok(Some(port)) => Some(port),
                        _ => legacy_config.parse(BuckconfigKeyRef {
                            section: BUCK2_RE_CLIENT_CFG_SECTION,
                            property: "cas_shared_cache_address_v2",
                        })?,
                    }
                },
                shared_casd_use_tls: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_shared_cache_tls",
                })?,
                cas_client_label: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_client_label_v2",
                })?,
                action_cache_address: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "action_cache_address",
                })?,
                action_cache_connection_count: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "action_cache_connection_count",
                    })?
                    .unwrap_or(4),
                engine_address: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "engine_address",
                })?,
                engine_connection_count: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "engine_connection_count",
                    })?
                    .unwrap_or(4),
                verbose_logging: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "verbose_logging",
                    })?
                    .unwrap_or(false),
                cas_thread_count: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "cas_thread_count",
                    })?
                    .unwrap_or(4),
                cas_thread_count_ratio: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "cas_thread_count_ratio",
                    })?
                    .unwrap_or(0.0),
                use_manifold_rich_client: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "use_manifold_rich_client_new",
                    })?
                    .unwrap_or(true),
                use_zippy_rich_client: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "use_zippy_rich_client",
                    })?
                    .unwrap_or(false),
                use_p2p: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "use_p2p",
                    })?
                    .unwrap_or(false),
                rich_client_channels_per_blob: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "rich_client_channels_per_blob",
                })?,
                rich_client_attempt_timeout_ms: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "rich_client_attempt_timeout_ms",
                })?,
                rich_client_retries_count: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "rich_client_retries_count",
                })?,
                force_enable_deduplicate_find_missing: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "force_enable_deduplicate_find_missing",
                })?,
                features_config_path: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "features_config_path",
                })?,
                client_config_path: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "client_config_path",
                })?,
                curl_reactor_max_number_of_retries: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "curl_reactor_max_number_of_retries",
                })?,
                curl_reactor_connection_timeout_ms: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "curl_reactor_connection_timeout_ms",
                })?,
                curl_reactor_request_timeout_ms: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "curl_reactor_request_timeout_ms",
                })?,
                minimal_blob_ttl_seconds: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "minimal_blob_ttl_seconds",
                })?,
                disable_fallocate: legacy_config
                    .parse::<RolloutPercentage>(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "disable_fallocate",
                    })?
                    .unwrap_or(RolloutPercentage::never())
                    .roll(),
                remaining_ttl_fraction_refresh_threshold: legacy_config.parse(
                    BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "remaining_ttl_fraction_refresh_threshold",
                    },
                )?,
                remaining_ttl_random_extra_threshold: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "remaining_ttl_random_extra_threshold",
                })?,
                respect_file_symlinks: legacy_config
                    .parse::<RolloutPercentage>(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "respect_file_symlinks",
                    })?
                    .unwrap_or(RolloutPercentage::never())
                    .roll(),
                execution_concurrency_limit: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "execution_concurrency_limit",
                    })?
                    .unwrap_or(4000),
                engine_tier: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "engine_tier",
                })?,
                engine_host: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "engine_host",
                })?,
                engine_port: legacy_config.parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "engine_port",
                })?,
                enable_download_cancellation: legacy_config
                    .parse(BuckconfigKeyRef {
                        section: BUCK2_RE_CLIENT_CFG_SECTION,
                        property: "enable_download_cancellation",
                    })?
                    .unwrap_or(false),
            })
        }

        fn apply_remote_execution_startup_config(
            &mut self,
            config: &RemoteExecutionStartupConfig,
        ) -> bz_error::Result<()> {
            let resolved = resolve_bazel_remote_execution_startup_config(
                config,
                self.cas_address.is_some() || self.action_cache_address.is_some(),
            )?;

            if let Some(engine_address) = resolved.engine_address {
                self.engine_address = engine_address;
            }
            if let Some(cas_address) = resolved.cas_address {
                self.cas_address = cas_address;
            }
            if let Some(action_cache_address) = resolved.action_cache_address {
                self.action_cache_address = action_cache_address;
            }

            Ok(())
        }

        fn remote_action_building_semaphore_size(&self) -> usize {
            available_parallelism()
        }

        fn exec_semaphore_size(&self) -> usize {
            self.execution_concurrency_limit as usize
        }
    }
}

#[allow(unused)]
mod not_fbcode {
    use super::*;

    /// Metadata that doesn't change between executions
    #[derive(Clone, Debug, Default, Allocative)]
    pub struct RemoteExecutionStaticMetadata(pub Buck2OssReConfiguration);

    impl RemoteExecutionStaticMetadataImpl for RemoteExecutionStaticMetadata {
        fn from_legacy_config(legacy_config: &LegacyBuckConfig) -> bz_error::Result<Self> {
            Ok(Self(Buck2OssReConfiguration::from_legacy_config(
                legacy_config,
            )?))
        }

        fn apply_remote_execution_startup_config(
            &mut self,
            config: &RemoteExecutionStartupConfig,
        ) -> bz_error::Result<()> {
            self.0.apply_remote_execution_startup_config(config)
        }

        fn remote_action_building_semaphore_size(&self) -> usize {
            available_parallelism()
        }

        fn exec_semaphore_size(&self) -> usize {
            self.0.execution_concurrency_limit.unwrap_or(400)
        }
    }
}

/// A configuration used only in our OSS builds. We still compile this always, which lets us
/// gate less code behind fbcode_build.
#[derive(Clone, Debug, Default, Allocative)]
pub struct Buck2OssReConfiguration {
    /// Address for RBE Content Addresable Storage service (including bytestream uploads service).
    pub cas_address: Option<String>,
    /// Address for RBE Engine service (including capabilities service).
    pub engine_address: Option<String>,
    /// Address for RBE Action Cache service.
    pub action_cache_address: Option<String>,
    /// Whether to use TLS to interact with remote execution.
    pub tls: bool,
    /// Path to a CA certificates bundle. This must be PEM-encoded. If none is set, a default
    /// bundle will be used.
    ///
    /// This can contain environment variables using shell interpolation syntax (i.e. $VAR). They
    /// will be substituted before using the value.
    pub tls_ca_certs: Option<String>,
    /// Path to a client certificate (and intermediate chain), as well as its associated private
    /// key. This must be PEM-encoded.
    ///
    /// This can contain environment variables using shell interpolation syntax (i.e. $VAR). They
    /// will be substituted before using the value.
    pub tls_client_cert: Option<String>,
    /// HTTP headers to inject in all requests to RE. This is a comma-separated list of `Header:
    /// Value` pairs. Minimal validation of those headers is done here.
    ///
    /// This can contain environment variables using shell interpolation syntax (i.e. $VAR). They
    /// will be substituted before using the value.
    pub http_headers: Vec<HttpHeader>,
    /// Whether to query capabilities from the RBE backend.
    pub capabilities: Option<bool>,
    /// The instance name to use in requests.
    pub instance_name: Option<String>,
    /// Use the Meta version of the request metadata
    pub use_fbcode_metadata: bool,
    /// The max size for a GRPC message to be decoded.
    pub max_decoding_message_size: Option<usize>,
    /// The max cumulative blob size for `Read` and `BatchReadBlobs` methods.
    pub max_total_batch_size: Option<usize>,
    /// Maximum number of concurrent upload requests for each action.
    pub max_concurrent_uploads_per_action: Option<usize>,
    /// Maximum number of concurrent remote cache/executor connections.
    pub remote_max_connections: Option<usize>,
    /// Maximum number of concurrent requests per remote gRPC connection.
    pub remote_max_concurrency_per_connection: Option<usize>,
    /// Maximum amount of time to wait for a remote execution/cache gRPC call.
    pub remote_timeout_secs: Option<u64>,
    /// Time that digests are assumed to live in CAS after being touched.
    pub cas_ttl_secs: Option<i64>,
    /// Interval in seconds for HTTP/2 ping frames to detect stale connections.
    pub grpc_keepalive_time_secs: Option<u64>,
    /// Timeout in seconds for receiving HTTP/2 ping acknowledgement.
    pub grpc_keepalive_timeout_secs: Option<u64>,
    /// Whether to send HTTP/2 pings when connection is idle.
    pub grpc_keepalive_while_idle: Option<bool>,
    /// Maximum number of concurrent execution requests.
    pub execution_concurrency_limit: Option<usize>,
}

#[derive(Clone, Debug, Default, Allocative)]
pub struct HttpHeader {
    pub key: String,
    pub value: String,
}

impl FromStr for HttpHeader {
    type Err = bz_error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut iter = s.splitn(2, ':');
        match (iter.next(), iter.next()) {
            (Some(key), Some(value)) => Ok(Self {
                key: key.trim().to_owned(),
                value: value.trim().to_owned(),
            }),
            _ => Err(bz_error::bz_error!(
                bz_error::ErrorTag::Input,
                "Invalid header (expect name and value separated by `:`): `{}`",
                s
            )),
        }
    }
}

fn apply_buildbuddy_api_key_header(headers: &mut Vec<HttpHeader>, api_key: &str) {
    headers.retain(|header| !header.key.eq_ignore_ascii_case(BUILDBUDDY_API_KEY_HEADER));
    if !api_key.trim().is_empty() {
        headers.push(HttpHeader {
            key: BUILDBUDDY_API_KEY_HEADER.to_owned(),
            value: api_key.to_owned(),
        });
    }
}

impl Buck2OssReConfiguration {
    pub fn apply_remote_execution_startup_config(
        &mut self,
        config: &RemoteExecutionStartupConfig,
    ) -> bz_error::Result<()> {
        let resolved = resolve_bazel_remote_execution_startup_config(
            config,
            self.cas_address.is_some() || self.action_cache_address.is_some(),
        )?;

        if let Some(engine_address) = resolved.engine_address {
            self.engine_address = engine_address;
        }
        if let Some(cas_address) = resolved.cas_address {
            self.cas_address = cas_address;
        }
        if let Some(action_cache_address) = resolved.action_cache_address {
            self.action_cache_address = action_cache_address;
        }

        if let Some(tls) = resolved.tls {
            self.tls = tls;
        }
        if let Some(api_key) = &config.buildbuddy_api_key {
            apply_buildbuddy_api_key_header(&mut self.http_headers, api_key);
        }
        if config.remote_max_connections.is_some() {
            self.remote_max_connections = config.remote_max_connections;
        }
        if config.remote_max_concurrency_per_connection.is_some() {
            self.remote_max_concurrency_per_connection =
                config.remote_max_concurrency_per_connection;
        }
        if config.remote_timeout_secs.is_some() {
            self.remote_timeout_secs = config.remote_timeout_secs;
        }

        Ok(())
    }

    pub fn from_legacy_config(legacy_config: &LegacyBuckConfig) -> bz_error::Result<Self> {
        // this is used for all three services by default, if given; if one of
        // them has an explicit address given as well though, use that instead
        let default_address: Option<String> = legacy_config.parse(BuckconfigKeyRef {
            section: BUCK2_RE_CLIENT_CFG_SECTION,
            property: "address",
        })?;

        Ok(Self {
            cas_address: legacy_config
                .parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "cas_address",
                })?
                .or(default_address.clone()),
            engine_address: legacy_config
                .parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "engine_address",
                })?
                .or(default_address.clone()),
            action_cache_address: legacy_config
                .parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "action_cache_address",
                })?
                .or(default_address),
            tls: legacy_config
                .parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "tls",
                })?
                .unwrap_or(true),
            tls_ca_certs: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "tls_ca_certs",
            })?,
            tls_client_cert: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "tls_client_cert",
            })?,
            http_headers: legacy_config
                .parse_list(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "http_headers",
                })?
                .unwrap_or_default(), // Empty list is as good None.
            capabilities: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "capabilities",
            })?,
            instance_name: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "instance_name",
            })?,
            use_fbcode_metadata: legacy_config
                .parse(BuckconfigKeyRef {
                    section: BUCK2_RE_CLIENT_CFG_SECTION,
                    property: "use_fbcode_metadata",
                })?
                .unwrap_or(false),
            max_decoding_message_size: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "max_decoding_message_size",
            })?,
            max_total_batch_size: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "max_total_batch_size",
            })?,
            max_concurrent_uploads_per_action: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "max_concurrent_uploads_per_action",
            })?,
            remote_max_connections: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "remote_max_connections",
            })?,
            remote_max_concurrency_per_connection: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "remote_max_concurrency_per_connection",
            })?,
            remote_timeout_secs: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "remote_timeout_secs",
            })?,
            cas_ttl_secs: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "cas_ttl_secs",
            })?,
            grpc_keepalive_time_secs: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "grpc_keepalive_time_secs",
            })?,
            grpc_keepalive_timeout_secs: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "grpc_keepalive_timeout_secs",
            })?,
            grpc_keepalive_while_idle: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "grpc_keepalive_while_idle",
            })?,
            execution_concurrency_limit: legacy_config.parse(BuckconfigKeyRef {
                section: BUCK2_RE_CLIENT_CFG_SECTION,
                property: "execution_concurrency_limit",
            })?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_executor_defaults_cache_when_cache_is_unset() -> bz_error::Result<()> {
        let mut config = Buck2OssReConfiguration::default();
        config.apply_remote_execution_startup_config(&RemoteExecutionStartupConfig {
            remote_executor: Some("grpc://executor.example.com".to_owned()),
            ..Default::default()
        })?;

        assert_eq!(
            config.engine_address.as_deref(),
            Some("executor.example.com")
        );
        assert_eq!(config.cas_address.as_deref(), Some("executor.example.com"));
        assert_eq!(
            config.action_cache_address.as_deref(),
            Some("executor.example.com")
        );
        assert!(!config.tls);

        Ok(())
    }

    #[test]
    fn remote_cache_overrides_executor_cache_fallback() -> bz_error::Result<()> {
        let mut config = Buck2OssReConfiguration::default();
        config.apply_remote_execution_startup_config(&RemoteExecutionStartupConfig {
            remote_cache: Some("cache.example.com".to_owned()),
            remote_executor: Some("executor.example.com".to_owned()),
            ..Default::default()
        })?;

        assert_eq!(
            config.engine_address.as_deref(),
            Some("executor.example.com")
        );
        assert_eq!(config.cas_address.as_deref(), Some("cache.example.com"));
        assert_eq!(
            config.action_cache_address.as_deref(),
            Some("cache.example.com")
        );
        assert!(config.tls);

        Ok(())
    }

    #[test]
    fn empty_remote_cache_disables_cache_without_executor() -> bz_error::Result<()> {
        let mut config = Buck2OssReConfiguration {
            cas_address: Some("configured-cache.example.com".to_owned()),
            action_cache_address: Some("configured-cache.example.com".to_owned()),
            ..Default::default()
        };
        config.apply_remote_execution_startup_config(&RemoteExecutionStartupConfig {
            remote_cache: Some(String::new()),
            ..Default::default()
        })?;

        assert_eq!(config.cas_address, None);
        assert_eq!(config.action_cache_address, None);

        Ok(())
    }

    #[test]
    fn api_key_sets_buildbuddy_header() -> bz_error::Result<()> {
        let mut config = Buck2OssReConfiguration::default();
        config.apply_remote_execution_startup_config(&RemoteExecutionStartupConfig {
            buildbuddy_api_key: Some("secret".to_owned()),
            ..Default::default()
        })?;

        assert_eq!(config.http_headers.len(), 1);
        assert_eq!(config.http_headers[0].key, BUILDBUDDY_API_KEY_HEADER);
        assert_eq!(config.http_headers[0].value, "secret");

        Ok(())
    }

    #[test]
    fn api_key_replaces_existing_buildbuddy_header() -> bz_error::Result<()> {
        let mut config = Buck2OssReConfiguration {
            http_headers: vec![
                HttpHeader {
                    key: "X-BuildBuddy-Api-Key".to_owned(),
                    value: "old".to_owned(),
                },
                HttpHeader {
                    key: "x-other-header".to_owned(),
                    value: "keep".to_owned(),
                },
            ],
            ..Default::default()
        };
        config.apply_remote_execution_startup_config(&RemoteExecutionStartupConfig {
            buildbuddy_api_key: Some("new".to_owned()),
            ..Default::default()
        })?;

        assert_eq!(config.http_headers.len(), 2);
        assert!(
            config
                .http_headers
                .iter()
                .any(|header| { header.key == "x-other-header" && header.value == "keep" })
        );
        assert!(
            config
                .http_headers
                .iter()
                .any(|header| { header.key == BUILDBUDDY_API_KEY_HEADER && header.value == "new" })
        );
        assert!(
            !config
                .http_headers
                .iter()
                .any(|header| header.value == "old")
        );

        Ok(())
    }

    #[test]
    fn empty_api_key_clears_existing_buildbuddy_header() -> bz_error::Result<()> {
        let mut config = Buck2OssReConfiguration {
            http_headers: vec![HttpHeader {
                key: BUILDBUDDY_API_KEY_HEADER.to_owned(),
                value: "old".to_owned(),
            }],
            ..Default::default()
        };
        config.apply_remote_execution_startup_config(&RemoteExecutionStartupConfig {
            buildbuddy_api_key: Some(String::new()),
            ..Default::default()
        })?;

        assert!(config.http_headers.is_empty());

        Ok(())
    }

    #[test]
    fn remote_connection_limits_are_startup_overrides() -> bz_error::Result<()> {
        let mut config = Buck2OssReConfiguration::default();
        config.apply_remote_execution_startup_config(&RemoteExecutionStartupConfig {
            remote_max_connections: Some(12),
            remote_max_concurrency_per_connection: Some(34),
            remote_timeout_secs: Some(56),
            ..Default::default()
        })?;

        assert_eq!(config.remote_max_connections, Some(12));
        assert_eq!(config.remote_max_concurrency_per_connection, Some(34));
        assert_eq!(config.remote_timeout_secs, Some(56));

        Ok(())
    }

    #[cfg(not(fbcode_build))]
    #[test]
    fn oss_remote_action_building_semaphore_matches_available_parallelism() {
        let metadata =
            not_fbcode::RemoteExecutionStaticMetadata(Buck2OssReConfiguration::default());

        assert_eq!(
            metadata.remote_action_building_semaphore_size(),
            available_parallelism()
        );
    }
}

#[cfg(fbcode_build)]
pub use fbcode::RemoteExecutionStaticMetadata;
#[cfg(not(fbcode_build))]
pub use not_fbcode::RemoteExecutionStaticMetadata;
