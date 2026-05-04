/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Builtin providers.

pub mod analysis_failure_info;
pub mod analysis_test_result_info;
pub mod cc_info;
pub mod configuration_info;
pub mod constraint_setting_info;
pub mod constraint_value_info;
pub mod coverage_info;
pub mod declared_toolchain_info;
pub mod default_info;
pub mod dep_only_incompatible_info;
pub mod dep_only_incompatible_rollout;
pub mod execution_platform_info;
pub mod execution_platform_registration_info;
pub mod external_runner_test_info;
pub mod install_info;
pub mod java_info;
pub mod local_resource_info;
pub mod output_group_info;
pub mod package_specification_info;
pub mod platform_info;
pub mod run_environment_info;
pub mod run_info;
pub mod template_placeholder_info;
pub mod template_variable_info;
pub mod toolchain_info;
pub mod ty;
pub mod validation_info;
pub mod worker_info;
pub mod worker_run_info;
