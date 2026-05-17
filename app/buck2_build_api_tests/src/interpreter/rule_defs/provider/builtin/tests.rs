/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![cfg(test)]

use buck2_build_api::interpreter::rule_defs::provider::registration::register_builtin_providers;
use buck2_interpreter_for_build::interpreter::testing::Tester;
use buck2_interpreter_for_build::label::testing::label_creator;
use indoc::indoc;

/// Test `equals` in generated code for providers.
#[test]
fn test_equals() -> buck2_error::Result<()> {
    let mut tester = Tester::new()?;

    tester.additional_globals(register_builtin_providers);
    tester.additional_globals(label_creator);

    tester.run_starlark_bzl_test(indoc!(
        r#"
            def test():
                x = target_label("root//bar:baz")
                y = target_label("root//quux:corge")
                a0 = ConstraintSettingInfo(label = x)
                a1 = ConstraintSettingInfo(label = x)
                b = ConstraintSettingInfo(label = y)
                assert_eq(a0, a1)
                assert_ne(a0, b)
        "#
    ))?;

    Ok(())
}

#[test]
fn test_builtin_provider_callables_are_hashable() -> buck2_error::Result<()> {
    let mut tester = Tester::new()?;

    tester.additional_globals(register_builtin_providers);

    tester.run_starlark_bzl_test(indoc!(
        r#"
            providers = {
                DefaultInfo: "default",
                ToolchainInfo: "toolchain",
            }

            def test():
                assert_eq("default", providers[DefaultInfo])
                assert_eq("toolchain", providers[ToolchainInfo])
        "#
    ))?;

    Ok(())
}

#[test]
fn test_output_group_info_supports_group_indexing() -> buck2_error::Result<()> {
    let mut tester = Tester::new()?;

    tester.additional_globals(register_builtin_providers);

    tester.run_starlark_bzl_test(indoc!(
        r#"
            def test():
                groups = OutputGroupInfo(
                    _hidden_top_level_INTERNAL_ = ["force"],
                    files = ["out"],
                )

                assert_eq(True, "_hidden_top_level_INTERNAL_" in groups)
                assert_eq(False, "missing" in groups)
                assert_eq(["force"], groups["_hidden_top_level_INTERNAL_"])
                assert_eq(["out"], groups["files"])
        "#
    ))?;

    Ok(())
}

#[test]
fn test_run_environment_info() -> buck2_error::Result<()> {
    let mut tester = Tester::new()?;

    tester.additional_globals(register_builtin_providers);

    tester.run_starlark_bzl_test(indoc!(
        r#"
            def test():
                default = RunEnvironmentInfo()
                assert_eq({}, default.environment)
                assert_eq([], default.inherited_environment)

                env = RunEnvironmentInfo(
                    environment = {"GOOS": "darwin"},
                    inherited_environment = ("PATH", "HOME"),
                )
                assert_eq({"GOOS": "darwin"}, env.environment)
                assert_eq(["PATH", "HOME"], env.inherited_environment)
        "#
    ))?;

    Ok(())
}
