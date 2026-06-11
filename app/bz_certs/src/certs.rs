/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::ffi::OsString;
use std::path::Path;

use bz_error::BuckErrorContext;
use bz_error::bz_error;
use bz_error::conversion::from_any_with_tag;
use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;

pub fn maybe_setup_cryptography() {
    setup_cryptography().ok();
}

pub fn setup_cryptography_or_fail() {
    setup_cryptography().unwrap();
}

fn setup_cryptography() -> std::result::Result<(), std::sync::Arc<rustls::crypto::CryptoProvider>> {
    // Note that all but the first call will fail, so we callers should only use
    // this function as early as possible in their lifetime
    // Note that the use of 'ring' here is arbitrary and should not be
    // taken as an intentional choice of cryptographic provider
    rustls::crypto::ring::default_provider().install_default()
}

/// Load system root certs, trying a few different methods to get a valid root
/// certificate store.
async fn load_system_root_certs() -> bz_error::Result<RootCertStore> {
    let root_certs = if let Some(path) = find_root_ca_certs() {
        load_certs(&path).await.with_buck_error_context(|| {
            format!("Loading root certs from: {}", path.to_string_lossy())
        })
    } else {
        let mut native_certs_results = rustls_native_certs::load_native_certs();

        if !native_certs_results.certs.is_empty() {
            Ok(native_certs_results.certs)
        } else {
            // Consider the last error to be indicative of the overall problem
            let native_certs_error = native_certs_results
                .errors
                .pop()
                .map(bz_error::Error::from)
                .unwrap_or(bz_error!(
                    bz_error::ErrorTag::NoValidCerts,
                    "No certs or cert errors"
                ));

            Err(native_certs_error
                .context("Error loading system root certificates native frameworks."))
        }
    }?;

    // According to [`rustls` documentation](https://docs.rs/rustls/latest/rustls/struct.RootCertStore.html#method.add_parsable_certificates),
    // it's better to only add parseable certs when loading system certs because
    // there are typically many system certs and not all of them can be valid. This
    // is pertinent for e.g. macOS which may have a lot of old certificates that may
    // not parse correctly.
    let mut roots = RootCertStore::empty();
    let (valid, invalid) = roots.add_parsable_certificates(root_certs);

    // But make sure we get at least _one_ valid cert, otherwise we legitimately won't be
    // able to make any connections via https.
    if valid == 0 {
        return Err(bz_error!(
            bz_error::ErrorTag::Environment,
            "Error loading system certs: unable to find any valid system certs"
        ));
    }
    tracing::debug!("Loaded {} valid system root certs", valid);
    tracing::debug!("Loaded {} invalid system root certs", invalid);
    Ok(roots)
}

// Load private key from the given path
async fn load_key<P: AsRef<Path>>(key: P) -> bz_error::Result<PrivateKeyDer<'static>> {
    let key = key.as_ref();

    let private_key = PrivateKeyDer::from_pem_file(key)
        .with_buck_error_context(|| format!("Error opening key file `{}`", key.display()))?;

    Ok(private_key)
}

/// Deserialize certificate pair at `cert` and `key` into structures that can
/// be inserted into rustls CertStore.
async fn load_cert_pair<P: AsRef<Path>>(
    cert: P,
    key: P,
) -> bz_error::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs = load_certs(cert).await?;
    let key = load_key(key).await?;

    Ok((certs, key))
}

pub async fn tls_config_with_system_roots() -> bz_error::Result<ClientConfig> {
    let system_roots = load_system_root_certs().await?;
    Ok(ClientConfig::builder()
        .with_root_certificates(system_roots)
        .with_no_client_auth())
}

pub async fn tls_config_with_single_cert<P: AsRef<Path>>(
    cert_path: P,
    key_path: P,
) -> bz_error::Result<ClientConfig> {
    let system_roots = load_system_root_certs().await?;
    let (cert, key) = load_cert_pair(cert_path, key_path)
        .await
        .buck_error_context("Error loading certificate pair")?;
    ClientConfig::builder()
        .with_root_certificates(system_roots)
        .with_client_auth_cert(cert, key)
        .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Certs))
        .buck_error_context("Error creating TLS config with cert and key path")
}

// Load certs from the given path
pub(crate) async fn load_certs<P: AsRef<Path>>(
    cert_path: P,
) -> bz_error::Result<Vec<CertificateDer<'static>>> {
    let cert_path = cert_path.as_ref();

    let cert_data = tokio::fs::read(cert_path)
        .await
        .with_buck_error_context(|| {
            format!("Error reading certificate file `{}`", cert_path.display())
        })?;

    let cert_results: Vec<Result<CertificateDer, rustls_pki_types::pem::Error>> =
        CertificateDer::pem_reader_iter(&mut cert_data.as_slice()).collect();

    let certs: Result<Vec<CertificateDer<'static>>, rustls_pki_types::pem::Error> =
        cert_results.into_iter().collect();

    certs.with_buck_error_context(|| {
        format!("Error reading certificate file `{}`", cert_path.display())
    })
}

/// Find root CA certs.
///
/// In OSS or non-workspace builds, returns None; we do not support hardcoded root
/// certificates in non-workspace builds and rely solely on rustls-native-certs.
pub(crate) fn find_root_ca_certs() -> Option<OsString> {
    match std::env::var_os("ROOT_CA_CERT_PATH") {
        Some(path) if Path::new(&path).exists() => Some(path),
        _ => None,
    }
}

/// Find TLS certs.
///
/// Return `None` in Cargo or open source builds; we do not support internal certs
/// in these builds.
pub fn find_internal_cert() -> Option<OsString> {
    None
}

/// Whether the machine buck is running on supports vpnless operation.
pub fn supports_vpnless() -> bool {
    false
}
