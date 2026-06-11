/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_error::bz_error;
use http::HeaderMap;
use http::HeaderValue;
use http::Uri;
use hyper_http_proxy::Proxy;

pub fn find_proxy() -> bz_error::Result<Option<Proxy>> {
    Err(bz_error!(
        bz_error::ErrorTag::Input,
        "VPNless development not supported for non-internal workspace builds"
    ))
}

/// Collection of different kinds of errors we can see from x2pagent. Typically
/// denotes a URL is not authorized for vpnless access and/or using the wrong,
/// non-vpnless url.
#[derive(Debug, bz_error::Error)]
#[buck2(tag = Environment)]
pub enum X2PAgentError {
    #[error("Host `{host}` is not authorized for vpnless access: {message}")]
    ForbiddenHost { host: String, message: String },
    #[error("Failed to connect to `{host}`: {message}")]
    Connection { host: String, message: String },
    #[error("Host `{host}` and path `{path}` is not authorized on vpnless")]
    AccessDenied { host: String, path: String },
    #[error(transparent)]
    Error(bz_error::Error),
}

impl From<bz_error::Error> for X2PAgentError {
    fn from(e: bz_error::Error) -> Self {
        Self::Error(e)
    }
}

impl X2PAgentError {
    pub fn from_headers(uri: &Uri, headers: &HeaderMap) -> Option<Self> {
        fn to_str(h: &HeaderValue) -> String {
            String::from_utf8_lossy(h.as_bytes()).into_owned()
        }

        let auth_decision = headers.get("x-fb-validated-x2pauth-decision");
        let error_type = headers.get("x-x2pagentd-error-type");
        let error_msg = headers.get("x-x2pagentd-error-msg");

        let host = uri.host().unwrap_or("<no host>").to_owned();
        match (auth_decision, error_type, error_msg) {
            (Some(decision), _, _) if decision == "deny" => Some(Self::AccessDenied {
                host,
                path: uri.path().to_owned(),
            }),
            (_, Some(typ), Some(msg)) if typ == "FORBIDDEN_HOST" => Some(Self::ForbiddenHost {
                host,
                message: to_str(msg),
            }),
            (_, Some(typ), Some(msg)) if typ == "CONNECTION" => Some(Self::Connection {
                host,
                message: to_str(msg),
            }),
            (_, _, Some(message)) => Some(Self::Error(bz_error!(
                bz_error::ErrorTag::Environment,
                "{}",
                to_str(message)
            ))),
            _ => None,
        }
    }
}
