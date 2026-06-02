/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::env;

use bz_client_ctx::client_ctx::ClientCommandContext;
use bz_client_ctx::common::BuckArgMatches;
use bz_client_ctx::exit_result::ExitResult;

/// Path to current executable.
#[derive(Debug, clap::Parser)]
pub struct ExeCommand {}

impl ExeCommand {
    pub fn exec(self, _matches: BuckArgMatches<'_>, _ctx: ClientCommandContext<'_>) -> ExitResult {
        bz_client_ctx::println!("{}", env::current_exe()?.display())?;
        ExitResult::success()
    }
}
