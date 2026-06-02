/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_build_api::analysis::registry::AnalysisRegistry;
use bz_build_api::interpreter::rule_defs::context::AnalysisContext;
use bz_build_api::interpreter::rule_defs::context::BazelCppOptions;
use bz_build_api::interpreter::rule_defs::plugins::AnalysisPlugins;
use bz_build_api::interpreter::rule_defs::register_rule_defs;
use bz_core::configuration::data::ConfigurationData;
use bz_core::deferred::base_deferred_key::BaseDeferredKey;
use bz_core::execution_types::execution::ExecutionPlatformResolution;
use bz_core::fs::buck_out_path::BazelOutputRoot;
use bz_core::target::label::label::TargetLabel;
use bz_execute::digest_config::DigestConfig;
use bz_interpreter::file_type::StarlarkFileType;
use bz_interpreter::from_freeze::from_freeze_error;
use bz_interpreter::testing::Buck2TestHeapName;
use dupe::Dupe;
use indoc::indoc;
use maplit::hashmap;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::eval::ReturnFileLoader;
use starlark::syntax::AstModule;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::structs::AllocStruct;

fn run_ctx_test(
    content: &str,
    result_handler: impl FnOnce(starlark::Result<Value>) -> bz_error::Result<()>,
) -> bz_error::Result<()> {
    Module::with_temp_heap(|func_mod| {
        let globals = GlobalsBuilder::standard().with(register_rule_defs).build();
        let prelude = indoc!(
            r#"
         def assert_eq(a, b):
             if a != b:
                 fail("Expected {}, got {}".format(a, b))
         "#
        );
        let full_content = format!("{prelude}\n{content}");

        {
            let mut eval = Evaluator::new(&func_mod);
            let ast = AstModule::parse(
                "foo.bzl",
                full_content,
                &StarlarkFileType::Bzl.dialect(false),
            )
            .unwrap();
            eval.eval_module(ast, &globals).unwrap();
        };
        let frozen_func_mod = func_mod
            .freeze_named(Buck2TestHeapName::frozen_heap_name())
            .map_err(from_freeze_error)?;
        let test_function = frozen_func_mod.get("test").unwrap();

        let modules = hashmap!["func_mod" => &frozen_func_mod];

        Module::with_temp_heap(|env| {
            let file_loader = ReturnFileLoader { modules: &modules };
            let test_function = env.heap().access_owned_frozen_value(&test_function);
            let mut eval = Evaluator::new(&env);
            eval.set_loader(&file_loader);
            let label = TargetLabel::testing_parse("root//foo/bar:some_name")
                .configure(ConfigurationData::testing_new());
            let registry = AnalysisRegistry::new_from_owner(
                BaseDeferredKey::TargetLabel(label.dupe()),
                ExecutionPlatformResolution::unspecified(),
            )?;
            let attributes = eval
                .heap()
                .alloc_typed_unchecked(AllocStruct([("name", "some_name")]))
                .cast();
            let plugins = eval
                .heap()
                .alloc_typed(AnalysisPlugins::new(SmallMap::new()))
                .into();

            let ctx = eval.heap().alloc(AnalysisContext::prepare(
                eval.heap(),
                Some(attributes),
                None,
                None,
                Vec::new(),
                Some(label),
                Some(plugins),
                Vec::new(),
                SmallMap::new(),
                Vec::new(),
                BazelCppOptions::default(),
                BazelOutputRoot::Bin,
                false,
                None,
                None,
                registry,
                DigestConfig::testing_default(),
            ));

            let returned = eval.eval_function(test_function, &[ctx], &[]);
            result_handler(returned)
        })
    })
}

#[test]
fn ctx_instantiates() -> bz_error::Result<()> {
    let content = indoc!(
        r#"
         def test(ctx):
             assert_eq("foo/bar", ctx.label.package)
             assert_eq("some_name", ctx.label.name)
             assert_eq(None, ctx.label.sub_target)
             return ctx.attrs.name
         "#
    );
    run_ctx_test(content, |ret| {
        assert_eq!("some_name", ret.unwrap().unpack_str().unwrap());
        Ok(())
    })
}

#[test]
fn declare_output_declares_outputs() -> bz_error::Result<()> {
    let content = indoc!(
        r#"
         def test(c):
             out = c.actions.declare_output("foo/bar.cpp")
             return (out.basename, out.short_path)
         "#
    );

    run_ctx_test(content, |ret| {
        let a = <(&str, &str)>::unpack_value(ret.unwrap()).unwrap().unwrap();
        assert_eq!("bar.cpp", a.0);
        assert_eq!("foo/bar.cpp", a.1);
        Ok(())
    })
}

#[test]
fn declare_output_with_prefix() -> bz_error::Result<()> {
    let content = indoc!(
        r#"
         def test(c):
             out = c.actions.declare_output("out/test", "foo/bar.cpp")
             return (out.basename, out.short_path)
         "#
    );

    run_ctx_test(content, |ret| {
        let a = <(&str, &str)>::unpack_value(ret.unwrap()).unwrap().unwrap();
        assert_eq!("bar.cpp", a.0);
        assert_eq!("foo/bar.cpp", a.1);
        Ok(())
    })
}

#[test]
fn declare_output_dot() -> bz_error::Result<()> {
    let content = indoc!(
        r#"
         def test(c):
             return c.actions.declare_output("magic", ".")
         "#
    );

    let expect = "expected a normalized path";
    run_ctx_test(content, |ret| match ret {
        Err(e) if e.to_string().contains(expect) => Ok(()),
        _ => panic!("Expected a specific failure containing `{expect}`, got {ret:?}"),
    })
}

#[test]
fn declare_output_dot_bad() -> bz_error::Result<()> {
    let content = indoc!(
        r#"
         def test(c):
             return c.actions.declare_output("..")
         "#
    );

    let expect = "expected a normalized path";
    run_ctx_test(content, |ret| match ret {
        Err(e) if e.to_string().contains(expect) => Ok(()),
        _ => panic!("Expected a specific failure containing `{expect}`, got {ret:?}"),
    })
}

#[test]
fn declare_output_dotdot() -> bz_error::Result<()> {
    let content = indoc!(
        r#"
         def test(c):
             return c.actions.declare_output("foo/..")
         "#
    );

    let expect = "expected a normalized path";
    run_ctx_test(content, |ret| match ret {
        Err(e) if e.to_string().contains(expect) => Ok(()),
        _ => panic!("Expected a specific failure containing `{expect}`, got {ret:?}"),
    })
}

#[test]
fn declare_output_require_bound() -> bz_error::Result<()> {
    let content = indoc!(
        r#"
         def test(c):
             a = c.actions.declare_output("a")
             b = c.actions.declare_output("b")
             c.actions.run([a, b.as_output()], category = "test_category")
         "#
    );

    let expect = "must be bound by now";
    run_ctx_test(content, |ret| match ret {
        Err(e) if e.to_string().contains(expect) => Ok(()),
        _ => panic!("Expected a specific failure containing `{expect}`, got {ret:?}"),
    })
}
