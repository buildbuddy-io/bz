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

is_absolute() {
  case "$1" in
    /*) return 0 ;;
    [A-Za-z]:/*) return 0 ;;
    [A-Za-z]:\\*) return 0 ;;
    *) return 1 ;;
  esac
}

absolutize_var() {
  var_name="$1"
  eval "var_value=\${$var_name:-}"
  if [ -n "$var_value" ] && ! is_absolute "$var_value"; then
    eval "$var_name=\"\$PWD/\$var_value\""
  fi
}

cache_status="$1"
shift
absolutize_var cache_status

EXEC_ROOT="$PWD"
export BAZEL_TEST=1

absolutize_var TEST_PREMATURE_EXIT_FILE
absolutize_var TEST_WARNINGS_OUTPUT_FILE
absolutize_var TEST_LOG
absolutize_var TEST_LOGSPLITTER_OUTPUT_FILE
absolutize_var TEST_INFRASTRUCTURE_FAILURE_FILE
absolutize_var TEST_UNUSED_RUNFILES_LOG_FILE
absolutize_var TEST_UNDECLARED_OUTPUTS_DIR
absolutize_var TEST_UNDECLARED_OUTPUTS_MANIFEST
absolutize_var TEST_UNDECLARED_OUTPUTS_ZIP
absolutize_var TEST_UNDECLARED_OUTPUTS_ANNOTATIONS
absolutize_var TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR
absolutize_var TEST_SRCDIR
absolutize_var TEST_TMPDIR
absolutize_var XML_OUTPUT_FILE
absolutize_var TEST_SHARD_STATUS_FILE
absolutize_var RUNFILES_DIR
absolutize_var JAVA_RUNFILES
absolutize_var PYTHON_RUNFILES

if [ -z "${HOME:-}" ] || ! is_absolute "$HOME"; then
  export HOME="$TEST_TMPDIR"
fi
if [ -z "${USER:-}" ]; then
  USER="$(whoami 2>/dev/null || true)"
  export USER
fi

mkdir -p "$(dirname "$XML_OUTPUT_FILE")" \
  "$(dirname "$TEST_LOG")" \
  "$TEST_TMPDIR" \
  "$TEST_UNDECLARED_OUTPUTS_DIR" \
  "$TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR" \
  "$(dirname "$TEST_WARNINGS_OUTPUT_FILE")" \
  "$(dirname "$TEST_UNUSED_RUNFILES_LOG_FILE")" \
  "$(dirname "$TEST_LOGSPLITTER_OUTPUT_FILE")" \
  "$(dirname "$TEST_INFRASTRUCTURE_FAILURE_FILE")" \
  "$(dirname "$cache_status")"

if [ -n "${TEST_SHARD_STATUS_FILE:-}" ]; then
  mkdir -p "$(dirname "$TEST_SHARD_STATUS_FILE")"
fi

export -n TEST_UNDECLARED_OUTPUTS_MANIFEST 2>/dev/null || true
export -n TEST_UNDECLARED_OUTPUTS_ZIP 2>/dev/null || true
export -n TEST_UNDECLARED_OUTPUTS_ANNOTATIONS 2>/dev/null || true

if [ -n "${TEST_TOTAL_SHARDS+x}" ] && [ "${TEST_TOTAL_SHARDS:-0}" != "0" ]; then
  export GTEST_SHARD_INDEX="$TEST_SHARD_INDEX"
  export GTEST_TOTAL_SHARDS="$TEST_TOTAL_SHARDS"
  export GTEST_SHARD_STATUS_FILE="$TEST_SHARD_STATUS_FILE"
fi
export GTEST_TMP_DIR="$TEST_TMPDIR"
export GUNIT_OUTPUT="xml:$XML_OUTPUT_FILE"

RUNFILES_MANIFEST_FILE="$TEST_SRCDIR/MANIFEST"
if [ "${RUNFILES_MANIFEST_ONLY:-}" = "1" ] && [ -e "$RUNFILES_MANIFEST_FILE" ]; then
  export RUNFILES_MANIFEST_FILE
  export RUNFILES_MANIFEST_ONLY
fi

if [ -n "${RUNFILES_DIR:-}" ] && [ ! -d "$RUNFILES_DIR" ]; then
  echo >&2 "ERROR: RUNFILES_DIR does not exist. This can happen when using --nobuild_runfile_manifests with local execution. Use a different execution strategy, or build with runfile manifests."
  exit 1
fi

rlocation() {
  if is_absolute "$1"; then
    printf '%s\n' "$1"
  elif [ -e "$TEST_SRCDIR/$1" ]; then
    printf '%s\n' "$TEST_SRCDIR/$1"
  elif [ -e "$RUNFILES_MANIFEST_FILE" ]; then
    grep "^$1 " "$RUNFILES_MANIFEST_FILE" | sed 's/[^ ]* //'
  fi
}

DIR="$TEST_SRCDIR"
if [ -n "${TEST_WORKSPACE:-}" ]; then
  DIR="$DIR/$TEST_WORKSPACE"
fi
if [ -n "${RUNTEST_PRESERVE_CWD:-}" ]; then
  DIR="$PWD"
fi
if [ -z "${COVERAGE_DIR:-}" ]; then
  cd "$DIR" || { echo "Could not chdir $DIR"; exit 1; }
fi

echo "-----------------------------------------------------------------------------"
PATH=".:$PATH"

EXE="${1#./}"
shift
if is_absolute "$EXE"; then
  TEST_PATH="$EXE"
elif [ -n "${TEST_WORKSPACE:-}" ]; then
  TEST_PATH="$(rlocation "$TEST_WORKSPACE/$EXE")"
else
  TEST_PATH="$(rlocation "$EXE")"
fi
if [ -z "$TEST_PATH" ] && [ -n "${RUNFILES_DIR:-}" ] && [ -n "${TEST_BINARY:-}" ] && [ -x "$RUNFILES_DIR/$TEST_BINARY" ]; then
  TEST_PATH="$RUNFILES_DIR/$TEST_BINARY"
fi
if [ -z "$TEST_PATH" ]; then
  TEST_PATH="$EXE"
fi

rm -f "$TEST_PREMATURE_EXIT_FILE" "$TEST_INFRASTRUCTURE_FAILURE_FILE" "$TEST_SHARD_STATUS_FILE"
: > "$TEST_PREMATURE_EXIT_FILE"
"$TEST_PATH" "$@" > "$TEST_LOG" 2>&1
exit_code=$?
rm -f "$TEST_PREMATURE_EXIT_FILE"

if [ "$exit_code" -eq 0 ] && [ -n "${TEST_TOTAL_SHARDS+x}" ] && [ "${TEST_TOTAL_SHARDS:-0}" != "0" ] && [ ! -f "$TEST_SHARD_STATUS_FILE" ]; then
  {
    echo
    echo "Sharding requested, but the test runner did not advertise support for it by touching TEST_SHARD_STATUS_FILE. Either remove the 'shard_count' attribute or use a test runner that supports sharding."
  } >> "$TEST_LOG"
  exit_code=1
fi

if [ -n "${TEST_UNDECLARED_OUTPUTS_DIR:-}" ] && [ -n "${TEST_UNDECLARED_OUTPUTS_MANIFEST:-}" ]; then
  undeclared_outputs="$(find -L "$TEST_UNDECLARED_OUTPUTS_DIR" -type f 2>/dev/null | sort)"
  if [ -n "$undeclared_outputs" ]; then
    while IFS= read -r undeclared_output; do
      rel_path="${undeclared_output#$TEST_UNDECLARED_OUTPUTS_DIR/}"
      file_size="$(stat -f%z "$undeclared_output" 2>/dev/null || stat -c%s "$undeclared_output" 2>/dev/null || echo 0)"
      file_type="$(file -L -b --mime-type "$undeclared_output" 2>/dev/null || echo application/octet-stream)"
      printf '%s\t%s\t%s\n' "$rel_path" "$file_size" "$file_type"
    done > "$TEST_UNDECLARED_OUTPUTS_MANIFEST" <<EOF_UNDECLARED
$undeclared_outputs
EOF_UNDECLARED
    [ -s "$TEST_UNDECLARED_OUTPUTS_MANIFEST" ] || rm -f "$TEST_UNDECLARED_OUTPUTS_MANIFEST"
  fi
fi

if [ -n "${TEST_UNDECLARED_OUTPUTS_ANNOTATIONS:-}" ] && [ -d "$TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR" ]; then
  cat "$TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR"/*.part > "$TEST_UNDECLARED_OUTPUTS_ANNOTATIONS" 2>/dev/null || true
  cat "$TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR"/*.pb > "${TEST_UNDECLARED_OUTPUTS_ANNOTATIONS}.pb" 2>/dev/null || true
fi

if [ -n "${TEST_UNDECLARED_OUTPUTS_ZIP:-}" ] && cd "$TEST_UNDECLARED_OUTPUTS_DIR" 2>/dev/null; then
  set -- *
  if [ "$1" != "*" ]; then
    zip -qr "$TEST_UNDECLARED_OUTPUTS_ZIP" -- "$@" 2>> "$TEST_LOG" || exit_code=1
    rm -rf -- "$@"
  fi
  cd "$EXEC_ROOT" || true
fi

if [ ! -f "$XML_OUTPUT_FILE" ]; then
  test_name="${TEST_BINARY#./}"
  test_name="${test_name#../}"
  if [ -z "$test_name" ]; then
    test_name="$TEST_TARGET"
  fi
  if [ -n "${TEST_TOTAL_SHARDS+x}" ] && [ "${TEST_TOTAL_SHARDS:-0}" != "0" ]; then
    shard_num=$((TEST_SHARD_INDEX + 1))
    test_name="${test_name}_shard_${shard_num}/${TEST_TOTAL_SHARDS}"
  fi
  errors=0
  error_msg=""
  if [ "$exit_code" -ne 0 ]; then
    errors=1
    error_msg="<error message=\"exited with error code $exit_code\"></error>"
  fi
  {
    printf '<?xml version="1.0" encoding="UTF-8"?>\n'
    printf '<testsuites>\n'
    printf '  <testsuite name="%s" tests="1" failures="0" errors="%s">\n' "$test_name" "$errors"
    printf '    <testcase name="%s" status="run" duration="0" time="0">%s</testcase>\n' "$test_name" "$error_msg"
    printf '    <system-out><![CDATA[\n'
    sed 's|]]>|]]>]]<![CDATA[>|g' "$TEST_LOG" 2>/dev/null
    printf ']]></system-out>\n'
    printf '  </testsuite>\n'
    printf '</testsuites>\n'
  } > "$XML_OUTPUT_FILE"
fi

if [ "$exit_code" -eq 0 ]; then
  printf '\010\001\020\001\030\001' > "$cache_status"
else
  printf '\010\001\020\000\030\004' > "$cache_status"
fi

: > "$TEST_WARNINGS_OUTPUT_FILE"
: > "$TEST_UNUSED_RUNFILES_LOG_FILE"
mkdir -p "$(dirname "$TEST_LOGSPLITTER_OUTPUT_FILE")"
: > "$TEST_LOGSPLITTER_OUTPUT_FILE"
: > "$TEST_INFRASTRUCTURE_FAILURE_FILE"
: > "${TEST_UNDECLARED_OUTPUTS_ANNOTATIONS}.pb"

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
        let run_count = spec.runs_per_test.max(1);
        futures::stream::iter(
            (0..shard_runs).flat_map(|shard_index| {
                (0..run_count).map(move |run_index| (shard_index, run_index))
            }),
        )
        .map(|(shard_index, run_index)| {
            let spec = spec.clone();
            async move {
                let name =
                    bazel_test_name(&spec.target, shard_index, shard_runs, run_index, run_count);
                let target_handle = spec.target.handle.to_owned();
                let execution_response = self
                    .execute_bazel_test_from_spec(
                        spec,
                        shard_index,
                        shard_runs,
                        run_index,
                        run_count,
                    )
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
        run_index: u32,
        run_count: u32,
    ) -> bz_error::Result<ExecuteResponse> {
        let target_name = target_name(&spec.target);
        let stage = TestStage::Testing {
            suite: spec.target.target.clone(),
            testcases: vec![target_name.clone()],
            variant: bazel_test_suffix(shard_index, shard_runs, run_index, run_count),
            repeat_count: (run_count > 1).then_some(run_index as usize + 1),
        };

        let mut command = vec![
            verbatim_arg("/bin/bash"),
            verbatim_arg("-c"),
            verbatim_arg(BAZEL_TEST_SETUP_SCRIPT),
            verbatim_arg("bazel-test-setup"),
            declared_output_arg(bazel_output_path(
                shard_index,
                shard_runs,
                run_index,
                run_count,
                "test.cache_status",
            )),
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

        add_bazel_test_environment(
            &mut env,
            &spec,
            shard_index,
            shard_runs,
            run_index,
            run_count,
        );

        let host_sharing_requirements = HostSharingRequirements::default();
        let pre_create_dirs = bazel_pre_create_dirs(shard_index, shard_runs, run_index, run_count);
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

fn bazel_test_name(
    target: &ConfiguredTarget,
    shard_index: u32,
    shard_runs: u32,
    run_index: u32,
    run_count: u32,
) -> String {
    let name = target_name(target);
    match bazel_test_suffix(shard_index, shard_runs, run_index, run_count) {
        Some(suffix) => format!("{name} {suffix}"),
        None => name,
    }
}

fn bazel_test_suffix(
    shard_index: u32,
    shard_runs: u32,
    run_index: u32,
    run_count: u32,
) -> Option<String> {
    if shard_runs > 1 && run_count > 1 {
        Some(format!(
            "(shard {} of {}, run {} of {})",
            shard_index + 1,
            shard_runs,
            run_index + 1,
            run_count
        ))
    } else if shard_runs > 1 {
        Some(format!("(shard {} of {})", shard_index + 1, shard_runs))
    } else if run_count > 1 {
        Some(format!("(run {} of {})", run_index + 1, run_count))
    } else {
        None
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
    run_index: u32,
    run_count: u32,
) {
    let test_tmpdir =
        bazel_output_path(shard_index, shard_runs, run_index, run_count, "test_tmpdir");
    let test_log = bazel_output_path(shard_index, shard_runs, run_index, run_count, "test.log");
    let test_xml = bazel_output_path(shard_index, shard_runs, run_index, run_count, "test.xml");
    let test_outputs = bazel_output_path(
        shard_index,
        shard_runs,
        run_index,
        run_count,
        "test.outputs",
    );
    let test_outputs_manifest_dir = bazel_output_path(
        shard_index,
        shard_runs,
        run_index,
        run_count,
        "test.outputs_manifest",
    );

    add_string_env(env, "TZ", "UTC");
    add_declared_env(env, "TEST_TMPDIR", test_tmpdir);
    add_string_env(env, "RUN_UNDER_RUNFILES", "1");

    add_string_env(env, "TEST_TARGET", target_name(&spec.target));
    add_string_env(env, "TEST_SIZE", spec.size.clone());
    add_string_env(env, "TEST_TIMEOUT", spec.timeout_seconds.to_string());
    add_string_env(
        env,
        "TEST_BINARY",
        if spec.executable_runfiles_path.is_empty() {
            spec.target.target.clone()
        } else {
            spec.executable_runfiles_path.clone()
        },
    );
    if run_count > 1 {
        add_string_env(env, "TEST_RANDOM_SEED", (run_index + 1).to_string());
        add_string_env(env, "TEST_RUN_NUMBER", (run_index + 1).to_string());
    }
    if !spec.test_filter.is_empty() {
        add_string_env(env, "TESTBRIDGE_TEST_ONLY", spec.test_filter.clone());
    }
    if spec.test_runner_fail_fast {
        add_string_env(env, "TESTBRIDGE_TEST_RUNNER_FAIL_FAST", "1");
    }
    if spec.runfiles_manifest_only {
        add_string_env(env, "RUNFILES_MANIFEST_ONLY", "1");
    }

    add_declared_env(env, "TEST_LOG", test_log);
    add_declared_env(env, "XML_OUTPUT_FILE", test_xml);
    add_declared_env(
        env,
        "TEST_WARNINGS_OUTPUT_FILE",
        bazel_output_path(
            shard_index,
            shard_runs,
            run_index,
            run_count,
            "test.warnings",
        ),
    );
    add_declared_env(
        env,
        "TEST_UNUSED_RUNFILES_LOG_FILE",
        bazel_output_path(
            shard_index,
            shard_runs,
            run_index,
            run_count,
            "test.unused_runfiles_log",
        ),
    );
    add_declared_env(
        env,
        "TEST_LOGSPLITTER_OUTPUT_FILE",
        bazel_output_path(
            shard_index,
            shard_runs,
            run_index,
            run_count,
            "test.raw_splitlogs/test.splitlogs",
        ),
    );
    add_declared_env(
        env,
        "TEST_PREMATURE_EXIT_FILE",
        bazel_output_path(
            shard_index,
            shard_runs,
            run_index,
            run_count,
            "test.exited_prematurely",
        ),
    );
    add_declared_env(
        env,
        "TEST_INFRASTRUCTURE_FAILURE_FILE",
        bazel_output_path(
            shard_index,
            shard_runs,
            run_index,
            run_count,
            "test.infrastructure_failure",
        ),
    );
    if spec.zip_undeclared_outputs {
        add_declared_env(
            env,
            "TEST_UNDECLARED_OUTPUTS_ZIP",
            format!("{test_outputs}/outputs.zip"),
        );
    }
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
    if spec.coverage_enabled {
        add_string_env(env, "RUNTEST_PRESERVE_CWD", "1");
        add_string_env(env, "IS_COVERAGE_SPAWN", "0");
    }

    if shard_runs > 1 {
        add_string_env(env, "TEST_SHARD_INDEX", shard_index.to_string());
        add_string_env(env, "TEST_TOTAL_SHARDS", shard_runs.to_string());
        add_declared_env(
            env,
            "TEST_SHARD_STATUS_FILE",
            bazel_output_path(shard_index, shard_runs, run_index, run_count, "test.shard"),
        );
    }
}

fn bazel_pre_create_dirs(
    shard_index: u32,
    shard_runs: u32,
    run_index: u32,
    run_count: u32,
) -> Vec<DeclaredOutput> {
    [
        bazel_output_path(shard_index, shard_runs, run_index, run_count, "test_tmpdir"),
        bazel_output_path(
            shard_index,
            shard_runs,
            run_index,
            run_count,
            "test.outputs",
        ),
        bazel_output_path(
            shard_index,
            shard_runs,
            run_index,
            run_count,
            "test.outputs_manifest",
        ),
        bazel_output_path(
            shard_index,
            shard_runs,
            run_index,
            run_count,
            "test.raw_splitlogs",
        ),
    ]
    .into_iter()
    .map(|path| DeclaredOutput::unchecked_new(path, RemoteStorageConfig::default()))
    .collect()
}

fn bazel_output_path(
    shard_index: u32,
    shard_runs: u32,
    run_index: u32,
    run_count: u32,
    name: &str,
) -> String {
    let shard_dir =
        (shard_runs > 1).then(|| format!("shard_{}_of_{}", shard_index + 1, shard_runs));
    let run_dir = (run_count > 1).then(|| format!("run_{}_of_{}", run_index + 1, run_count));
    match (shard_dir, run_dir) {
        (Some(shard_dir), Some(run_dir)) => format!("{shard_dir}_{run_dir}/{name}"),
        (Some(dir), None) | (None, Some(dir)) => format!("{dir}/{name}"),
        (None, None) => name.to_owned(),
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
