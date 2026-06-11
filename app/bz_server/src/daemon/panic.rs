/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Daemon-only panic hooks.
//!
//! This module sets up a panic hook to send the panic message to open CLIs.

use std::env::temp_dir;
use std::panic;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use bz_cli_proto::unstable_dice_dump_request::DiceDumpFormat;

pub(crate) trait DaemonStatePanicDiceDump: Send + Sync + 'static {
    fn dice_dump(&self, path: &Path, format: DiceDumpFormat) -> bz_error::Result<()>;
}

fn get_panic_dump_dir() -> PathBuf {
    temp_dir().join("buck2-dumps")
}

async fn remove_old_panic_dumps() -> bz_error::Result<()> {
    const MAX_PANIC_AGE: Duration = Duration::from_hours(24); // 1 day
    let dump_dir = get_panic_dump_dir();
    let now = SystemTime::now();
    if let Ok(dir_result) = std::fs::read_dir(dump_dir) {
        let dumps = dir_result.filter_map(Result::ok).collect::<Vec<_>>();
        for record in dumps {
            let metadata = record.metadata()?;
            if now.duration_since(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH))?
                > MAX_PANIC_AGE
            {
                match metadata.is_dir() {
                    true => std::fs::remove_dir_all(record.path()),
                    false => std::fs::remove_file(record.path()),
                }
                .ok();
            }
        }
    }
    Ok(())
}

/// Initializes the panic hook.
pub(crate) fn initialize(_daemon_state: Arc<dyn DaemonStatePanicDiceDump>) {
    let hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        hook(info);
    }));
    tokio::spawn(remove_old_panic_dumps());
}
