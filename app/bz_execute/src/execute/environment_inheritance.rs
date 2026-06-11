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
use std::sync::OnceLock;

use dupe::Dupe;

#[cfg(unix)]
const ENV_ALLOW_LIST: &[&str] = &[
    "PATH",
    "USER",
    "LOGNAME",
    "HOME",
    "TMPDIR",
    // Generally needed to keep systemd working
    "XDG_RUNTIME_DIR",
];

// The standard (built-in) variables.
// https://ss64.com/nt/syntax-variables.html
#[cfg(windows)]
const ENV_ALLOW_LIST: &[&str] = &[
    "ALLUSERSPROFILE",
    "APPDATA",
    "COMPUTERNAME",
    "COMSPEC",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PATH",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_ARCHITEW6432",
    "PROCESSOR_IDENTIFIER",
    "PROCESSOR_LEVEL",
    "PROCESSOR_REVISION",
    "PSModulePath",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "Public",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERDOMAIN",
    "USERNAME",
    "USERPROFILE",
    "UserDnsDomain",
    "WINDIR",
];

#[derive(Copy, Clone, Dupe, Debug)]
pub struct EnvironmentInheritance {
    clear: bool,
    values: &'static [(&'static str, OsString)],
    exclusions: &'static [&'static str],
}

impl EnvironmentInheritance {
    pub fn test_allowlist() -> Self {
        // Keep this as a list of lists so more allowlists can be added without
        // changing the merge logic below.
        let allowlists = &[ENV_ALLOW_LIST];

        // We create this *once* since getenv is actually not cheap (being O(n) of the environment
        // size).
        static TEST_CELL: OnceLock<Vec<(&'static str, OsString)>> = OnceLock::new();

        let values = TEST_CELL.get_or_init(|| {
            let mut ret = Vec::new();
            for list in allowlists.iter() {
                for key in list.iter() {
                    if let Some(value) = std::env::var_os(key) {
                        ret.push((*key, value));
                    }
                }
            }
            ret
        });

        Self {
            clear: true,
            values,
            exclusions: &[],
        }
    }

    /// Exclude some vars that are known to cause issues. In an ideal world we should do a
    /// migration to lock this down everywhere.
    pub fn local_command_exclusions() -> Self {
        Self {
            clear: false,
            values: &[],
            exclusions: &[
                "PYTHONPATH",
                "PYTHONHOME",
                "PYTHONSTARTUP",
                "LD_LIBRARY_PATH",
                "LD_PRELOAD",
            ],
        }
    }

    pub fn empty() -> Self {
        Self {
            values: &[],
            exclusions: &[],
            clear: true,
        }
    }

    pub fn values(&self) -> impl Iterator<Item = (&'static str, &'static OsString)> + use<> {
        self.values.iter().map(|(k, v)| (*k, v))
    }

    pub fn exclusions(&self) -> impl Iterator<Item = &'static str> + use<> {
        self.exclusions.iter().copied()
    }

    pub fn clear(&self) -> bool {
        self.clear
    }
}
