/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::str::FromStr;

use clap::Parser;

#[derive(Debug, Parser)]
pub struct Config {
    /// add a list of environment variables using format: --env VAR1=Value1 VAR2='Value 2'
    #[clap(long)]
    pub env: Vec<String>,

    /// Max number of seconds allowed to run a test.
    #[clap(long, default_value = "600")]
    pub timeout: u64,

    /// Ignored arg included for backwards compatibility.
    #[clap(long, hide = true)]
    buck_test_info: String,

    /// Passthrough argments to test binary.
    /// Available as a workaround for when test features are available.
    #[clap(long, num_args=1.., allow_hyphen_values = true)]
    pub test_arg: Vec<String>,

    /// Bazel-compatible --runs_per_test value.
    #[clap(long = "runs_per_test", alias = "runs-per-test")]
    pub runs_per_test: Option<u32>,

    /// Bazel-compatible --test_filter value.
    #[clap(long = "test_filter", alias = "test-filter")]
    pub test_filter: Option<String>,

    /// Bazel-compatible --test_runner_fail_fast flag.
    #[clap(long = "test_runner_fail_fast", alias = "test-runner-fail-fast")]
    pub test_runner_fail_fast: bool,

    /// Bazel-compatible --zip_undeclared_test_outputs flag.
    #[clap(
        long = "zip_undeclared_test_outputs",
        alias = "zip-undeclared-test-outputs",
        alias = "zip_undeclared_outputs",
        alias = "zip-undeclared-outputs"
    )]
    pub zip_undeclared_test_outputs: bool,

    /// Force Bazel manifest-only runfiles behavior.
    #[clap(long = "runfiles_manifest_only", alias = "runfiles-manifest-only")]
    pub runfiles_manifest_only: bool,

    /// Force Bazel coverage-mode test env behavior.
    #[clap(long = "coverage_enabled", alias = "coverage-enabled", hide = true)]
    pub coverage_enabled: bool,
}

/// Uiltity that can be used to parse Env values from CLI arguments.
#[derive(Debug, PartialEq, Clone)]
pub struct EnvValue {
    pub name: String,
    pub value: String,
}

impl EnvValue {
    pub fn new(name: &str, value: &str) -> EnvValue {
        EnvValue {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }
}

impl FromStr for EnvValue {
    type Err = bz_error::Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.split_once('=') {
            Some((key, value)) => Ok(EnvValue::new(key, value)),
            None => Err(EnvValueParseError::IncorrectSyntax(input.to_owned()).into()),
        }
    }
}

#[derive(Debug, bz_error::Error, PartialEq)]
#[buck2(tag = Input)]
pub enum EnvValueParseError {
    #[error("Incorrect syntax for env value. Please use name=value. Input: `{0}`")]
    IncorrectSyntax(String),
}
