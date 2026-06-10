/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::time::Duration;

use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_test_api::data::ArgValue;
use bz_test_api::data::ArgValueContent;
use bz_test_api::data::BazelTestSpec;
use bz_test_api::data::ConfiguredTarget;
use bz_test_api::data::ConfiguredTargetHandle;
use bz_test_api::data::DeclaredOutput;
use bz_test_api::data::ExecuteResponse;
use bz_test_api::data::ExecutionResult2;
use bz_test_api::data::ExecutionStatus;
use bz_test_api::data::ExternalRunnerSpec;
use bz_test_api::data::ExternalRunnerSpecValue;
use bz_test_api::data::OutputName;
use bz_test_api::data::RemoteStorageConfig;
use bz_test_api::data::RequiredLocalResources;
use bz_test_api::data::TestResult;
use bz_test_api::data::TestStage;
use bz_test_api::data::TestStatus;
use bz_test_api::grpc::TestOrchestratorClient;
use clap::Parser;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use host_sharing::HostSharingRequirements;
use parking_lot::Mutex;
use sorted_vector_map::SortedVectorMap;

use crate::config::Config;
use crate::config::EnvValue;
use crate::executor::TestSpec;

pub type SpecReceiver = UnboundedReceiver<TestSpec>;

const BAZEL_TEST_SETUP_SCRIPT: &str = r#"set +e
test_executable="$1"
shift
mkdir -p "$TEST_TMPDIR" "$TEST_UNDECLARED_OUTPUTS_DIR" "$(dirname "$TEST_UNDECLARED_OUTPUTS_MANIFEST")" "$(dirname "$TEST_LOG")" "$(dirname "$XML_OUTPUT_FILE")"
rm -f "$TEST_PREMATURE_EXIT_FILE" "$TEST_INFRASTRUCTURE_FAILURE_FILE"
: > "$TEST_PREMATURE_EXIT_FILE"
if [ -n "$RUNFILES_DIR" ] && [ -n "$TEST_BINARY" ] && [ -x "$RUNFILES_DIR/$TEST_BINARY" ]; then
  "$RUNFILES_DIR/$TEST_BINARY" "$@" > "$TEST_LOG" 2>&1
else
  "$test_executable" "$@" > "$TEST_LOG" 2>&1
fi
exit_code=$?
rm -f "$TEST_PREMATURE_EXIT_FILE"
if [ ! -f "$XML_OUTPUT_FILE" ]; then
  if [ "$exit_code" -eq 0 ]; then
    printf '<testsuite name="%s" tests="1"></testsuite>\n' "$TEST_TARGET" > "$XML_OUTPUT_FILE"
  else
    printf '<testsuite name="%s" tests="1" failures="1"><testcase name="%s"><failure message="test failed"/></testcase></testsuite>\n' "$TEST_TARGET" "$TEST_TARGET" > "$XML_OUTPUT_FILE"
  fi
fi
: > "$TEST_CACHE_STATUS"
cat "$TEST_LOG"
exit "$exit_code"
"#;

/// Internal test runner implementation for Buck2.
///
/// This is a basic test runner intended to be used by the open-source Buck2 build
/// if no external test runner is provided. This ensures that `bz test` works
/// out-of-the-box for open-source users.
///
/// **This is intended for open-source use only.**
pub struct Buck2TestRunner {
    orchestrator_client: TestOrchestratorClient,
    spec_receiver: Mutex<Option<SpecReceiver>>,
    config: Config,
}

impl Buck2TestRunner {
    pub fn new(
        orchestrator_client: TestOrchestratorClient,
        spec_receiver: SpecReceiver,
        args: Vec<String>,
    ) -> bz_error::Result<Self> {
        let config = Config::try_parse_from(args)
            .buck_error_context("Error parsing test runner arguments")?;
        Ok(Self {
            orchestrator_client,
            spec_receiver: Mutex::new(Some(spec_receiver)),
            config,
        })
    }

    pub async fn run_all_tests(&self) -> bz_error::Result<()> {
        let receiver;
        {
            let mut maybe_receiver = self.spec_receiver.lock();
            receiver = maybe_receiver
                .take()
                .ok_or_else(|| internal_error!("Spec channel has already been consumed"))?;
            drop(maybe_receiver);
        }
        let run_verdict = receiver
            .map(|spec| async move {
                self.execute_and_report_spec(spec)
                    .await
                    .buck_error_context("Test execution request failed")
            })
            // Use an arbitrarily large buffer -- execution throttling will be handled by the Buck2
            // executor, so no need to hold back on requests here.
            .buffer_unordered(10000)
            // If any individual test failed, consider the entire run to have failed.
            .try_fold(
                RunVerdict::Pass,
                |mut run_verdict, spec_verdict| async move {
                    if spec_verdict != RunVerdict::Pass {
                        run_verdict = RunVerdict::Fail;
                    }
                    bz_error::Ok(run_verdict)
                },
            )
            .await;

        self.orchestrator_client
            .end_of_test_results(run_verdict?.exit_code())
            .await
    }

    async fn execute_and_report_spec(&self, spec: TestSpec) -> bz_error::Result<RunVerdict> {
        match spec {
            TestSpec::External(spec) => self.execute_and_report_external_spec(spec).await,
            TestSpec::Bazel(spec) => self.execute_and_report_bazel_spec(spec).await,
        }
    }

    async fn execute_and_report_external_spec(
        &self,
        spec: ExternalRunnerSpec,
    ) -> bz_error::Result<RunVerdict> {
        let name = target_name(&spec.target);
        let target_handle = spec.target.handle.to_owned();

        let execution_response = self.execute_test_from_spec(spec).await?;

        let execution_result = match execution_response {
            ExecuteResponse::Result(r) => r,
            ExecuteResponse::Cancelled(_) => {
                return Ok(RunVerdict::from_status(TestStatus::OMITTED));
            }
        };

        let test_result = get_test_result(name, target_handle, execution_result);
        let test_status = test_result.status.clone();

        self.report_test_result(test_result)
            .await
            .buck_error_context("Test result reporting failed")?;

        Ok(RunVerdict::from_status(test_status))
    }

    async fn execute_and_report_bazel_spec(
        &self,
        spec: BazelTestSpec,
    ) -> bz_error::Result<RunVerdict> {
        let shard_runs = spec.shard_count.max(1);
        futures::stream::iter(0..shard_runs)
            .map(|shard_index| {
                let spec = spec.clone();
                async move {
                    let name = bazel_test_name(&spec.target, shard_index, shard_runs);
                    let target_handle = spec.target.handle.to_owned();
                    let execution_response = self
                        .execute_bazel_test_from_spec(spec, shard_index, shard_runs)
                        .await?;

                    let execution_result = match execution_response {
                        ExecuteResponse::Result(r) => r,
                        ExecuteResponse::Cancelled(_) => {
                            return Ok(RunVerdict::from_status(TestStatus::OMITTED));
                        }
                    };

                    let test_result = get_test_result(name, target_handle, execution_result);
                    let test_status = test_result.status.clone();

                    self.report_test_result(test_result)
                        .await
                        .buck_error_context("Test result reporting failed")?;

                    Ok(RunVerdict::from_status(test_status))
                }
            })
            .buffer_unordered(10000)
            .try_fold(
                RunVerdict::Pass,
                |mut run_verdict, shard_verdict| async move {
                    if shard_verdict != RunVerdict::Pass {
                        run_verdict = RunVerdict::Fail;
                    }
                    bz_error::Ok(run_verdict)
                },
            )
            .await
    }

    async fn execute_test_from_spec(
        &self,
        spec: ExternalRunnerSpec,
    ) -> bz_error::Result<ExecuteResponse> {
        let stage = TestStage::Testing {
            suite: spec.target.target,
            testcases: Vec::new(),
            variant: None,
            repeat_count: None,
        };

        let config_args = self.config.test_arg.iter().map(|arg| ArgValue {
            content: ArgValueContent::ExternalRunnerSpecValue(ExternalRunnerSpecValue::Verbatim(
                arg.to_owned(),
            )),
            format: None,
        });

        let command = spec
            .command
            .into_iter()
            .map(|spec_value| ArgValue {
                content: ArgValueContent::ExternalRunnerSpecValue(spec_value),
                format: None,
            })
            .chain(config_args)
            .collect();

        let config_env: Vec<_> = self
            .config
            .env
            .iter()
            .map(|s| s.parse())
            .collect::<bz_error::Result<_>>()?;
        let config_env = config_env.iter().map(|EnvValue { name, value }| {
            (
                name.to_owned(),
                ArgValue {
                    content: ArgValueContent::ExternalRunnerSpecValue(
                        ExternalRunnerSpecValue::Verbatim(value.to_owned()),
                    ),
                    format: None,
                },
            )
        });

        let env = spec
            .env
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    ArgValue {
                        content: ArgValueContent::ExternalRunnerSpecValue(value),
                        format: None,
                    },
                )
            })
            .chain(config_env)
            .collect();

        let target_handle = spec.target.handle;
        let host_sharing_requirements = HostSharingRequirements::default();
        let pre_create_dirs = Vec::new();
        let executor_override = None;

        self.orchestrator_client
            .execute2(
                stage,
                target_handle,
                command,
                env,
                Duration::from_secs(self.config.timeout),
                host_sharing_requirements,
                pre_create_dirs,
                executor_override,
                RequiredLocalResources { resources: vec![] },
                false,
            )
            .await
    }

    async fn execute_bazel_test_from_spec(
        &self,
        spec: BazelTestSpec,
        shard_index: u32,
        shard_runs: u32,
    ) -> bz_error::Result<ExecuteResponse> {
        let target_name = target_name(&spec.target);
        let stage = TestStage::Testing {
            suite: spec.target.target.clone(),
            testcases: vec![target_name.clone()],
            variant: (shard_runs > 1)
                .then(|| format!("shard {} of {}", shard_index + 1, shard_runs)),
            repeat_count: None,
        };

        let mut command = vec![
            verbatim_arg("/bin/sh"),
            verbatim_arg("-c"),
            verbatim_arg(BAZEL_TEST_SETUP_SCRIPT),
            verbatim_arg("bazel-test-setup"),
        ];
        command.extend(spec.command.iter().cloned().map(spec_value_arg));
        command.extend(
            self.config
                .test_arg
                .iter()
                .map(|arg| verbatim_arg(arg.as_str())),
        );

        let mut env = spec
            .env
            .iter()
            .map(|(key, value)| (key.to_owned(), spec_value_arg(value.clone())))
            .collect::<SortedVectorMap<_, _>>();
        for EnvValue { name, value } in self.config_env()? {
            env.insert(name, verbatim_arg(value));
        }

        add_bazel_test_environment(&mut env, &spec, shard_index, shard_runs);

        let host_sharing_requirements = HostSharingRequirements::default();
        let pre_create_dirs = bazel_pre_create_dirs(shard_index, shard_runs);
        let executor_override = None;

        self.orchestrator_client
            .execute2(
                stage,
                spec.target.handle,
                command,
                env,
                Duration::from_secs(spec.timeout_seconds),
                host_sharing_requirements,
                pre_create_dirs,
                executor_override,
                RequiredLocalResources { resources: vec![] },
                false,
            )
            .await
    }

    fn config_env(&self) -> bz_error::Result<Vec<EnvValue>> {
        self.config
            .env
            .iter()
            .map(|s| s.parse())
            .collect::<bz_error::Result<_>>()
    }

    async fn report_test_result(&self, test_result: TestResult) -> bz_error::Result<()> {
        self.orchestrator_client
            .report_test_result(test_result)
            .await
    }
}

fn get_test_result(
    name: String,
    target: ConfiguredTargetHandle,
    execution_result: ExecutionResult2,
) -> TestResult {
    let status = match execution_result.status {
        ExecutionStatus::Finished { exitcode } => match exitcode {
            0 => TestStatus::PASS,
            _ => TestStatus::FAIL,
        },
        ExecutionStatus::TimedOut { .. } => TestStatus::TIMEOUT,
    };
    TestResult {
        target,
        name,
        status,
        msg: None,
        duration: Some(execution_result.execution_time),
        details: format!(
            "---- STDOUT ----\n{:?}\n---- STDERR ----\n{:?}\n",
            execution_result.stdout, execution_result.stderr
        ),
        max_memory_used_bytes: execution_result.max_memory_used_bytes,
    }
}

fn target_name(target: &ConfiguredTarget) -> String {
    format!("{}//{}:{}", target.cell, target.package, target.target)
}

fn bazel_test_name(target: &ConfiguredTarget, shard_index: u32, shard_runs: u32) -> String {
    let name = target_name(target);
    if shard_runs > 1 {
        format!("{name} (shard {} of {})", shard_index + 1, shard_runs)
    } else {
        name
    }
}

fn verbatim_arg(value: impl Into<String>) -> ArgValue {
    spec_value_arg(ExternalRunnerSpecValue::Verbatim(value.into()))
}

fn spec_value_arg(value: ExternalRunnerSpecValue) -> ArgValue {
    ArgValue {
        content: ArgValueContent::ExternalRunnerSpecValue(value),
        format: None,
    }
}

fn declared_output_arg(name: String) -> ArgValue {
    ArgValue {
        content: ArgValueContent::DeclaredOutput(OutputName::unchecked_new(name)),
        format: None,
    }
}

fn add_string_env(
    env: &mut SortedVectorMap<String, ArgValue>,
    key: &str,
    value: impl Into<String>,
) {
    env.insert(key.to_owned(), verbatim_arg(value));
}

fn add_declared_env(env: &mut SortedVectorMap<String, ArgValue>, key: &str, path: String) {
    env.insert(key.to_owned(), declared_output_arg(path));
}

fn add_bazel_test_environment(
    env: &mut SortedVectorMap<String, ArgValue>,
    spec: &BazelTestSpec,
    shard_index: u32,
    shard_runs: u32,
) {
    let runfiles_dir = ".";
    let test_tmpdir = bazel_output_path(shard_index, shard_runs, "test_tmpdir");
    let test_log = bazel_output_path(shard_index, shard_runs, "test.log");
    let test_xml = bazel_output_path(shard_index, shard_runs, "test.xml");
    let test_outputs = bazel_output_path(shard_index, shard_runs, "test.outputs");
    let test_outputs_manifest_dir =
        bazel_output_path(shard_index, shard_runs, "test.outputs_manifest");

    add_string_env(env, "TZ", "UTC");
    add_string_env(env, "TEST_SRCDIR", runfiles_dir);
    add_string_env(env, "JAVA_RUNFILES", runfiles_dir);
    add_string_env(env, "PYTHON_RUNFILES", runfiles_dir);
    add_string_env(env, "RUNFILES_DIR", runfiles_dir);
    add_declared_env(env, "TEST_TMPDIR", test_tmpdir);
    add_string_env(env, "RUN_UNDER_RUNFILES", "1");

    add_string_env(env, "TEST_TARGET", target_name(&spec.target));
    add_string_env(env, "TEST_SIZE", spec.size.clone());
    add_string_env(env, "TEST_TIMEOUT", spec.timeout_seconds.to_string());
    add_string_env(env, "TEST_WORKSPACE", spec.target.cell.clone());
    add_string_env(
        env,
        "TEST_BINARY",
        if spec.executable_runfiles_path.is_empty() {
            spec.target.target.clone()
        } else {
            spec.executable_runfiles_path.clone()
        },
    );

    add_declared_env(env, "TEST_LOG", test_log);
    add_declared_env(env, "XML_OUTPUT_FILE", test_xml);
    add_declared_env(
        env,
        "TEST_WARNINGS_OUTPUT_FILE",
        bazel_output_path(shard_index, shard_runs, "test.warnings"),
    );
    add_declared_env(
        env,
        "TEST_UNUSED_RUNFILES_LOG_FILE",
        bazel_output_path(shard_index, shard_runs, "test.unused_runfiles_log"),
    );
    add_declared_env(
        env,
        "TEST_LOGSPLITTER_OUTPUT_FILE",
        bazel_output_path(shard_index, shard_runs, "test.splitlogs"),
    );
    add_declared_env(
        env,
        "TEST_PREMATURE_EXIT_FILE",
        bazel_output_path(shard_index, shard_runs, "test.premature_exit"),
    );
    add_declared_env(
        env,
        "TEST_INFRASTRUCTURE_FAILURE_FILE",
        bazel_output_path(shard_index, shard_runs, "test.infrastructure_failure"),
    );
    add_declared_env(env, "TEST_UNDECLARED_OUTPUTS_DIR", test_outputs);
    add_declared_env(
        env,
        "TEST_UNDECLARED_OUTPUTS_MANIFEST",
        format!("{test_outputs_manifest_dir}/MANIFEST"),
    );
    add_declared_env(
        env,
        "TEST_UNDECLARED_OUTPUTS_ANNOTATIONS",
        format!("{test_outputs_manifest_dir}/ANNOTATIONS"),
    );
    add_declared_env(
        env,
        "TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR",
        test_outputs_manifest_dir,
    );
    add_declared_env(
        env,
        "TEST_CACHE_STATUS",
        bazel_output_path(shard_index, shard_runs, "test.cache_status"),
    );

    if shard_runs > 1 {
        add_string_env(env, "TEST_SHARD_INDEX", shard_index.to_string());
        add_string_env(env, "TEST_TOTAL_SHARDS", shard_runs.to_string());
        add_declared_env(
            env,
            "TEST_SHARD_STATUS_FILE",
            bazel_output_path(shard_index, shard_runs, "test.shard_status"),
        );
    }
}

fn bazel_pre_create_dirs(shard_index: u32, shard_runs: u32) -> Vec<DeclaredOutput> {
    [
        bazel_output_path(shard_index, shard_runs, "test_tmpdir"),
        bazel_output_path(shard_index, shard_runs, "test.outputs"),
        bazel_output_path(shard_index, shard_runs, "test.outputs_manifest"),
    ]
    .into_iter()
    .map(|path| DeclaredOutput::unchecked_new(path, RemoteStorageConfig::default()))
    .collect()
}

fn bazel_output_path(shard_index: u32, shard_runs: u32, name: &str) -> String {
    if shard_runs > 1 {
        format!("shard_{}_of_{}/{}", shard_index + 1, shard_runs, name)
    } else {
        name.to_owned()
    }
}

#[derive(Debug, PartialEq)]
enum RunVerdict {
    Pass,
    Fail,
}

impl RunVerdict {
    fn from_status(status: TestStatus) -> Self {
        match status {
            TestStatus::PASS => RunVerdict::Pass,
            TestStatus::SKIP
            | TestStatus::OMITTED
            | TestStatus::FAIL
            | TestStatus::FATAL
            | TestStatus::TIMEOUT
            | TestStatus::INFRA_FAILURE
            | TestStatus::UNKNOWN
            | TestStatus::RERUN
            | TestStatus::LISTING_SUCCESS
            | TestStatus::LISTING_FAILED => RunVerdict::Fail,
        }
    }

    fn exit_code(&self) -> i32 {
        match self {
            RunVerdict::Pass => 0,
            RunVerdict::Fail => 32,
        }
    }
}
