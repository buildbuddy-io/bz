use std::fmt::Debug;

use allocative::Allocative;
use bz_artifact::artifact::artifact_type::Artifact;
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
use starlark::values::ValueOf;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictType;
use starlark::values::list::AllocList;
use starlark::values::none::NoneOr;

use crate as bz_build_api;
use crate::artifact_groups::ArtifactGroup;
use crate::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use crate::interpreter::rule_defs::provider::builtin::default_info::BazelRunfiles;
use crate::interpreter::rule_defs::provider::builtin::default_info::FrozenBazelRunfiles;
use crate::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_empty_filenames;
use crate::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_empty_value;
use crate::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_for_each_artifact;
use crate::interpreter::rule_defs::provider::builtin::default_info::bazel_runfiles_for_each_entry;
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
    /// Bazel runfiles for the test executable.
    runfiles: ValueOfUncheckedGeneric<V, FrozenBazelRunfiles>,
    /// Whether this test should run with runfiles manifests only.
    runfiles_manifest_only: ValueOfUncheckedGeneric<V, bool>,
    /// Bazel runs_per_test value.
    runs_per_test: ValueOfUncheckedGeneric<V, i32>,
    /// Optional test filter.
    test_filter: ValueOfUncheckedGeneric<V, String>,
    /// Whether test runner fail-fast is enabled.
    test_runner_fail_fast: ValueOfUncheckedGeneric<V, bool>,
    /// Whether undeclared test outputs should be zipped.
    zip_undeclared_outputs: ValueOfUncheckedGeneric<V, bool>,
    /// Whether coverage mode is enabled.
    coverage_enabled: ValueOfUncheckedGeneric<V, bool>,
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

    pub fn for_each_runfiles_artifact(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup),
    ) -> bz_error::Result<()> {
        bazel_runfiles_for_each_artifact(self.runfiles.get(), processor)
    }

    pub fn for_each_runfiles_entry(
        &self,
        processor: &mut dyn FnMut(String, Artifact) -> bz_error::Result<()>,
    ) -> bz_error::Result<()> {
        bazel_runfiles_for_each_entry(self.runfiles.get(), processor)
    }

    pub fn runfiles_empty_filenames(&self) -> bz_error::Result<Vec<String>> {
        bazel_runfiles_empty_filenames(self.runfiles.get())
    }

    pub fn runfiles_manifest_only(&self) -> bool {
        self.runfiles_manifest_only
            .get()
            .to_value()
            .unpack_bool()
            .unwrap()
    }

    pub fn runs_per_test(&self) -> u32 {
        self.runs_per_test
            .get()
            .to_value()
            .unpack_i32()
            .unwrap()
            .try_into()
            .unwrap_or(1)
    }

    pub fn test_filter(&self) -> &str {
        self.test_filter.to_value().get().unpack_str().unwrap()
    }

    pub fn test_runner_fail_fast(&self) -> bool {
        self.test_runner_fail_fast
            .get()
            .to_value()
            .unpack_bool()
            .unwrap()
    }

    pub fn zip_undeclared_outputs(&self) -> bool {
        self.zip_undeclared_outputs
            .get()
            .to_value()
            .unpack_bool()
            .unwrap()
    }

    pub fn coverage_enabled(&self) -> bool {
        self.coverage_enabled
            .get()
            .to_value()
            .unpack_bool()
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
    runfiles: Value<'v>,
    runfiles_manifest_only: bool,
    runs_per_test: i32,
    test_filter: String,
    test_runner_fail_fast: bool,
    zip_undeclared_outputs: bool,
    coverage_enabled: bool,
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
        runfiles: ValueOfUnchecked::new(runfiles),
        runfiles_manifest_only: ValueOfUnchecked::new(heap.alloc(runfiles_manifest_only)),
        runs_per_test: ValueOfUnchecked::new(heap.alloc(runs_per_test)),
        test_filter: ValueOfUnchecked::new(heap.alloc(test_filter)),
        test_runner_fail_fast: ValueOfUnchecked::new(heap.alloc(test_runner_fail_fast)),
        zip_undeclared_outputs: ValueOfUnchecked::new(heap.alloc(zip_undeclared_outputs)),
        coverage_enabled: ValueOfUnchecked::new(heap.alloc(coverage_enabled)),
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
        #[starlark(require = named, default = NoneOr::None)] runfiles: NoneOr<
            ValueOf<'v, &'v BazelRunfiles<'v>>,
        >,
        #[starlark(require = named, default = false)] runfiles_manifest_only: bool,
        #[starlark(require = named, default = 1)] runs_per_test: i32,
        #[starlark(require = named, default = "")] test_filter: &'v str,
        #[starlark(require = named, default = false)] test_runner_fail_fast: bool,
        #[starlark(require = named, default = false)] zip_undeclared_outputs: bool,
        #[starlark(require = named, default = false)] coverage_enabled: bool,
        #[starlark(require = named, default = "medium")] size: &'v str,
        #[starlark(require = named, default = 300)] timeout_seconds: i32,
        #[starlark(require = named, default = 0)] shard_count: i32,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<BazelTestInfo<'v>> {
        let heap = eval.heap();
        let runfiles = runfiles
            .into_option()
            .map(|runfiles| runfiles.value)
            .unwrap_or_else(|| bazel_runfiles_empty_value(heap));
        let res = BazelTestInfo {
            command: ValueOfUnchecked::new(command),
            env: ValueOfUnchecked::new(env),
            labels: ValueOfUnchecked::new(labels),
            executable_runfiles_path: ValueOfUnchecked::new(heap.alloc(executable_runfiles_path)),
            runfiles: ValueOfUnchecked::new(runfiles),
            runfiles_manifest_only: ValueOfUnchecked::new(heap.alloc(runfiles_manifest_only)),
            runs_per_test: ValueOfUnchecked::new(heap.alloc(runs_per_test)),
            test_filter: ValueOfUnchecked::new(heap.alloc(test_filter)),
            test_runner_fail_fast: ValueOfUnchecked::new(heap.alloc(test_runner_fail_fast)),
            zip_undeclared_outputs: ValueOfUnchecked::new(heap.alloc(zip_undeclared_outputs)),
            coverage_enabled: ValueOfUnchecked::new(heap.alloc(coverage_enabled)),
            size: ValueOfUnchecked::new(heap.alloc(size)),
            timeout_seconds: ValueOfUnchecked::new(heap.alloc(timeout_seconds)),
            shard_count: ValueOfUnchecked::new(heap.alloc(shard_count)),
        };
        validate_bazel_test_info(&res)?;
        Ok(res)
    }
}
