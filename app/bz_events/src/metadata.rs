/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Metadata collection, for telemetry purposes.
use std::env;
use std::sync::OnceLock;

use bz_core::ci::ci_identifiers;
use bz_hash::StdBuckHashMap;
use bz_wrapper_common::BUCK2_WRAPPER_ENV_VAR;

use crate::daemon_id::DaemonId;

/// Collects metadata from the current binary and environment and writes it as map, suitable for telemetry purposes.
pub fn collect(daemon: &DaemonId) -> StdBuckHashMap<String, String> {
    fn add_env_var(map: &mut StdBuckHashMap<String, String>, key: &'static str, var: &'static str) {
        if let Ok(data) = env::var(var) {
            map.insert(key.to_owned(), data);
        }
    }

    let mut map = StdBuckHashMap::default();

    let info = system_info();
    if let Some(hostname) = info.hostname {
        map.insert("hostname".to_owned(), hostname);
    }
    if let Some(username) = info.username {
        map.insert("username".to_owned(), username);
    }
    if let Some(system_fingerprint) = info.system_fingerprint {
        map.insert("system_fingerprint".to_owned(), system_fingerprint);
    }
    map.insert("arch".to_owned(), info.arch);

    if let Some(rev) = bz_build_info::revision() {
        map.insert("bz_revision".to_owned(), rev.to_owned());
    }

    if let Some(time) = bz_build_info::time_iso8601() {
        map.insert("bz_build_time".to_owned(), time.to_owned());
    }

    if let Some(ts) = bz_build_info::release_timestamp() {
        map.insert("bz_release_timestamp".to_owned(), ts.to_owned());
    }

    if is_proc_translated::is_proc_translated() {
        map.insert("rosetta".to_owned(), "1".to_owned());
    }

    if let Some(ces_id) = ces_id() {
        map.insert("ces_id".to_owned(), ces_id);
    }

    if let Some(devx_session_id) = devx_session_id() {
        map.insert("devx_session_id".to_owned(), devx_session_id);
    }

    // Global trace ID
    map.insert("daemon_uuid".to_owned(), daemon.to_string());

    map.insert("os".to_owned(), info.os);
    if let Some(version) = info.os_version {
        map.insert("os_version".to_owned(), version);
    }

    if let Some(environment) = environment() {
        map.insert("environment".to_owned(), environment);
    }

    add_env_var(&mut map, "launched_via_wrapper", BUCK2_WRAPPER_ENV_VAR);

    if let Ok(ci_identifiers) = ci_identifiers() {
        for (ci_name, ci_value) in ci_identifiers {
            if let Some(ci_value) = ci_value {
                map.insert(ci_name.to_owned(), ci_value.to_owned());
            }
        }
    }

    map
}

pub struct SystemInfo {
    pub username: Option<String>,
    pub hostname: Option<String>,
    pub os: String,
    pub os_version: Option<String>,
    pub system_fingerprint: Option<String>,
    pub arch: String,
}

pub fn system_info() -> SystemInfo {
    let hostname = hostname();
    let username = username().ok().flatten();

    SystemInfo {
        hostname,
        username,
        os: os_type(),
        os_version: os_version(),
        system_fingerprint: system_fingerprint(),
        arch: env::consts::ARCH.to_owned(),
    }
}

/// The operating system - "linux" "darwin" "windows" etc.
fn os_type() -> String {
    if cfg!(target_os = "linux") {
        "linux".to_owned()
    } else if cfg!(target_os = "macos") {
        "darwin".to_owned()
    } else if cfg!(target_os = "windows") {
        "windows".to_owned()
    } else {
        "unknown".to_owned()
    }
}

#[cfg(target_os = "windows")]
fn os_version() -> Option<String> {
    winver::WindowsVersion::detect().map(|v| v.to_string())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn os_version() -> Option<String> {
    sys_info::os_release().ok()
}

pub fn hostname() -> Option<String> {
    static CELL: OnceLock<Option<String>> = OnceLock::new();

    CELL.get_or_init(|| {
        hostname::get()
            .ok()
            .map(|res| res.to_string_lossy().into_owned())
    })
    .clone()
}

pub fn ces_id() -> Option<String> {
    None
}

pub fn devx_session_id() -> Option<String> {
    None
}

pub fn environment() -> Option<String> {
    None
}

pub fn username() -> bz_error::Result<Option<String>> {
    Ok(None)
}

pub fn system_fingerprint() -> Option<String> {
    None
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn os_version_produces_reasonable_windows_version() {
        let data = collect(&DaemonId::new());
        // This logic used to use the `GetVersionExW` win32 API, which
        // always returns the value below on recent versions of windows. See
        // https://learn.microsoft.com/en-us/windows/win32/api/sysinfoapi/nf-sysinfoapi-getversionexw
        // for more details.
        assert_ne!(data["os_version"], "6.2.9200");
        // This is true for both Windows 10 and Windows 11: https://learn.microsoft.com/en-us/windows/win32/sysinfo/operating-system-version
        assert!(data["os_version"].starts_with("10.0."));
    }
}
