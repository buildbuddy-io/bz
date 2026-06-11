/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::OnceLock;

use fbinit::FacebookInit;

fn run_init() -> FacebookInit {
    // SAFETY: Only called within a oncelock
    unsafe { fbinit::perform_init() }
}

/// Gets an fbinit token.
///
/// This function is lazy and safe to call from multiple threads, however:
///  1. You should still prefer to explicit pass `FacebookInit` around where possible. Use of this
///     function is primarily intended for a very early point in buck2's lifecycle where fbinit
///     initialization is lazy.
///  2. This function may not be called before any forks, as it may spawn threads.
pub fn get_or_init_build_globals() -> FacebookInit {
    static FB: OnceLock<fbinit::FacebookInit> = OnceLock::new();

    *FB.get_or_init(run_init)
}
