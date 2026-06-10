/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt::Debug;

use allocative::Allocative;
use bz_build_api_derive::internal_provider;
use either::Either;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::values::Freeze;
use starlark::values::FreezeError;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictType;
use starlark::values::list::AllocList;

use crate as bz_build_api;
use crate::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use crate::interpreter::rule_defs::provider::builtin::external_runner_test_info::TestCommandMember;
use crate::interpreter::rule_defs::provider::builtin::external_runner_test_info::check_all;
use crate::interpreter::rule_defs::provider::builtin::external_runner_test_info::iter_opt_str_list;
use crate::interpreter::rule_defs::provider::builtin::external_runner_test_info::iter_test_command;
use crate::interpreter::rule_defs::provider::builtin::external_runner_test_info::iter_test_env;
use crate::interpreter::rule_defs::provider::builtin::external_runner_test_info::unwrap_all;

/// Internal provider describing a Bazel `rule(test = True)` target.
///
/// This is deliberately separate from `ExternalRunnerTestInfo`: Bazel tests are
/// native test targets with Bazel test command/environment semantics, not Buck
/// external-runner tests.
#[internal_provider(bazel_test_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[freeze(validator = validate_bazel_test_info, bounds = "V: ValueLike<'freeze>")]
#[repr(C)]
pub struct BazelTestInfoGen<V: ValueLifetimeless> {
    /// Base test command: executable followed by rule `args`.
    command: ValueOfUncheckedGeneric<V, Vec<Either<String, FrozenValue>>>,
    /// Rule `env` values.
    env: ValueOfUncheckedGeneric<V, DictType<String, FrozenValue>>,
    /// Rule tags. These are used for test filtering and reporting.
    labels: ValueOfUncheckedGeneric<V, Vec<String>>,
    /// Test binary path relative to the runfiles tree.
    executable_runfiles_path: ValueOfUncheckedGeneric<V, String>,
    /// Bazel test `size` attr.
    size: ValueOfUncheckedGeneric<V, String>,
    /// Bazel test timeout in seconds.
    timeout_seconds: ValueOfUncheckedGeneric<V, i32>,
    /// Explicit shard count after Bazel attr coercion. Zero means unsharded.
    shard_count: ValueOfUncheckedGeneric<V, i32>,
}

impl FrozenBazelTestInfo {
    pub fn command<'v>(&self) -> impl Iterator<Item = TestCommandMember<'v>> {
        unwrap_all(iter_test_command(self.command.get().to_value()))
    }

    pub fn env<'v>(&self) -> impl Iterator<Item = (&'v str, &'v dyn CommandLineArgLike<'v>)> {
        unwrap_all(iter_test_env(self.env.get().to_value()))
    }

    pub fn labels(&self) -> impl Iterator<Item = &str> {
        unwrap_all(iter_opt_str_list(self.labels.get().to_value(), "labels"))
    }

    pub fn executable_runfiles_path(&self) -> &str {
        self.executable_runfiles_path
            .to_value()
            .get()
            .unpack_str()
            .unwrap()
    }

    pub fn size(&self) -> &str {
        self.size.to_value().get().unpack_str().unwrap()
    }

    pub fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
            .get()
            .to_value()
            .unpack_i32()
            .unwrap()
            .try_into()
            .unwrap_or(0)
    }

    pub fn shard_count(&self) -> u32 {
        self.shard_count
            .get()
            .to_value()
            .unpack_i32()
            .unwrap()
            .try_into()
            .unwrap_or(0)
    }
}

pub fn new_bazel_test_info<'v>(
    command: Vec<Value<'v>>,
    environment: Vec<(String, String)>,
    labels: Vec<String>,
    executable_runfiles_path: String,
    size: String,
    timeout_seconds: i32,
    shard_count: i32,
    heap: Heap<'v>,
) -> bz_error::Result<BazelTestInfo<'v>> {
    let res = BazelTestInfo {
        command: ValueOfUnchecked::new(heap.alloc(AllocList(command))),
        env: ValueOfUnchecked::new(heap.alloc(AllocDict(environment))),
        labels: ValueOfUnchecked::new(heap.alloc(AllocList(labels))),
        executable_runfiles_path: ValueOfUnchecked::new(heap.alloc(executable_runfiles_path)),
        size: ValueOfUnchecked::new(heap.alloc(size)),
        timeout_seconds: ValueOfUnchecked::new(heap.alloc(timeout_seconds)),
        shard_count: ValueOfUnchecked::new(heap.alloc(shard_count)),
    };
    validate_bazel_test_info(&res)?;
    Ok(res)
}

fn validate_bazel_test_info<'v, V>(info: &BazelTestInfoGen<V>) -> bz_error::Result<()>
where
    V: ValueLike<'v>,
{
    check_all(iter_test_command(info.command.get().to_value()))?;
    check_all(iter_test_env(info.env.get().to_value()))?;
    check_all(iter_opt_str_list(info.labels.get().to_value(), "labels"))?;
    Ok(())
}

#[starlark_module]
fn bazel_test_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenBazelTestInfo)]
    fn BazelTestInfo<'v>(
        #[starlark(require = named)] command: Value<'v>,
        #[starlark(require = named, default = AllocDict::EMPTY)] env: Value<'v>,
        #[starlark(require = named, default = AllocList::EMPTY)] labels: Value<'v>,
        #[starlark(require = named, default = "")] executable_runfiles_path: &'v str,
        #[starlark(require = named, default = "medium")] size: &'v str,
        #[starlark(require = named, default = 300)] timeout_seconds: i32,
        #[starlark(require = named, default = 0)] shard_count: i32,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<BazelTestInfo<'v>> {
        let heap = eval.heap();
        let res = BazelTestInfo {
            command: ValueOfUnchecked::new(command),
            env: ValueOfUnchecked::new(env),
            labels: ValueOfUnchecked::new(labels),
            executable_runfiles_path: ValueOfUnchecked::new(heap.alloc(executable_runfiles_path)),
            size: ValueOfUnchecked::new(heap.alloc(size)),
            timeout_seconds: ValueOfUnchecked::new(heap.alloc(timeout_seconds)),
            shard_count: ValueOfUnchecked::new(heap.alloc(shard_count)),
        };
        validate_bazel_test_info(&res)?;
        Ok(res)
    }
}
