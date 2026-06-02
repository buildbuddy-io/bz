/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use crate::bz_env;

/// Are we running in CI?
pub fn is_ci() -> bz_error::Result<bool> {
    // The CI environment variable is consistently set by CI providers.
    //
    // - GitHub Actions: https://docs.github.com/en/actions/learn-github-actions/variables#default-environment-variables
    // - GitLab CI/CD: https://docs.gitlab.com/ee/ci/variables/predefined_variables.html
    // - CircleCI: https://circleci.com/docs/variables/#built-in-environment-variables
    // - many others
    //
    // Internally, CI should be setting SANDCASTLE env var.
    Ok(bz_env!("SANDCASTLE", applicability = internal)?.is_some() || bz_env!("CI", bool)?)
}

/// Returns a list of possible identifiers for the currently running CI job, in `(name, value)` form
///
/// Earlier items in the list are better identifiers
pub fn ci_identifiers()
-> bz_error::Result<impl Iterator<Item = (&'static str, Option<&'static str>)>> {
    Ok([
        (
            "sandcastle_job_info",
            bz_env!("SANDCASTLE_JOB_INFO", applicability = internal)?,
        ),
        (
            "skycastle_workflow_run_id",
            bz_env!("SKYCASTLE_WORKFLOW_RUN_ID", applicability = internal)?,
        ),
        (
            "sandcastle_alias",
            bz_env!("SANDCASTLE_ALIAS", applicability = internal)?,
        ),
        (
            "skycastle_workflow_alias",
            bz_env!("SKYCASTLE_WORKFLOW_ALIAS", applicability = internal)?,
        ),
        (
            "sandcastle_type",
            bz_env!("SANDCASTLE_TYPE", applicability = internal)?,
        ),
    ]
    .into_iter())
}
