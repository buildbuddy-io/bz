/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::io::IsTerminal;

pub(crate) fn bes_invocation_url(results_url: &str, invocation_id: &str) -> String {
    let separator = if results_url.ends_with('/') { "" } else { "/" };
    format!("{results_url}{separator}{invocation_id}")
}

fn bes_results_url_message(results_url: &str, invocation_id: &str, color: bool) -> String {
    let url = bes_invocation_url(results_url, invocation_id);
    if color {
        format!("\x1b[32mINFO:\x1b[0m Streaming build results to: \x1b[4;36m{url}\x1b[0m")
    } else {
        format!("INFO: Streaming build results to: {url}")
    }
}

pub(crate) fn print_bes_results_url(
    results_url: &str,
    invocation_id: &str,
) -> bz_error::Result<()> {
    crate::eprintln!(
        "{}",
        bes_results_url_message(results_url, invocation_id, std::io::stderr().is_terminal())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bes_results_url_info_prefix_is_not_bold() {
        assert_eq!(
            bes_results_url_message("https://app.buildbuddy.dev/invocation", "abc", true),
            "\x1b[32mINFO:\x1b[0m Streaming build results to: \x1b[4;36mhttps://app.buildbuddy.dev/invocation/abc\x1b[0m"
        );
    }
}
