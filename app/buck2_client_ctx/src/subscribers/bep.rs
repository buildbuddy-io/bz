use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use buck2_cli_proto::BuildTarget;
use buck2_cli_proto::CommandResult;
use buck2_cli_proto::command_result;
use buck2_error::ExitCode;
use buck2_events::BuckEvent;
use prost::Message;
use prost_types::Any;
use prost_types::Timestamp;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::metadata::MetadataKey;
use tonic::metadata::MetadataValue;
use tonic::transport::ClientTlsConfig;
use tonic::transport::Endpoint;

use crate::client_ctx::ClientCommandContext;
use crate::exit_result::ExitResult;
use crate::subscribers::subscriber::EventSubscriber;

const BAZEL_BUILD_EVENT_TYPE_URL: &str = "type.googleapis.com/build_event_stream.BuildEvent";
const PUBLISH_BUILD_TOOL_EVENT_STREAM_PATH: &str =
    "/google.devtools.build.v1.PublishBuildEvent/PublishBuildToolEventStream";
const DEFAULT_PROGRESS_CHUNK_SIZE: usize = 1024 * 1024;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Tier0)]
enum BepError {
    #[error("Invalid BES backend `{0}`")]
    InvalidBackend(String),
    #[error("Invalid BES header `{0}`. Expected NAME=VALUE")]
    InvalidHeader(String),
    #[error("BEP upload failed: {0}")]
    Upload(String),
    #[error("BEP upload task failed to join: {0}")]
    Join(String),
}

#[derive(Clone)]
pub(crate) struct BuildEventProtocolConfig {
    backend: String,
    headers: Vec<String>,
    project_id: String,
    keywords: Vec<String>,
    timeout: Option<Duration>,
    results_url: Option<String>,
    invocation_id: String,
    build_id: String,
    command_name: String,
    argv: Vec<String>,
    target_patterns: Vec<String>,
    start_time: SystemTime,
    working_directory: String,
    workspace_directory: String,
}

impl BuildEventProtocolConfig {
    pub(crate) fn from_command<T: crate::streaming::StreamingCommand>(
        cmd: &T,
        ctx: &ClientCommandContext,
        paths: Option<&buck2_common::invocation_paths::InvocationPaths>,
    ) -> buck2_error::Result<Option<Self>> {
        let event_log_opts = cmd.event_log_opts();
        let Some(backend) = event_log_opts.bes_backend.as_ref().map(ToOwned::to_owned) else {
            return Ok(None);
        };
        let sanitized_argv = cmd.sanitize_argv(ctx.argv.clone());
        let argv = redact_bes_headers(sanitized_argv.argv);
        let target_patterns = cmd.build_event_protocol_target_patterns();
        let workspace_directory = paths
            .map(|p| p.project_root().root().to_string())
            .unwrap_or_else(|| ctx.working_dir.to_string());
        let keywords = keywords(T::COMMAND_NAME, &event_log_opts.bes_keywords);
        let project_id = event_log_opts.bes_instance_name.clone().unwrap_or_default();
        let timeout = event_log_opts.bes_timeout_duration()?;

        Ok(Some(Self {
            backend,
            headers: event_log_opts.bes_header.clone(),
            project_id,
            keywords,
            timeout,
            results_url: event_log_opts.bes_results_url.clone(),
            invocation_id: ctx.trace_id.to_string(),
            build_id: ctx.trace_id.to_string(),
            command_name: T::COMMAND_NAME.to_owned(),
            argv,
            target_patterns,
            start_time: ctx.start_time,
            working_directory: ctx.working_dir.to_string(),
            workspace_directory,
        }))
    }
}

fn keywords(command_name: &str, user_keywords: &[String]) -> Vec<String> {
    let mut out = vec![
        format!("command_name={command_name}"),
        "protocol_name=BEP".to_owned(),
        "tool=buck2".to_owned(),
    ];
    for value in user_keywords {
        for keyword in value.split(',') {
            let keyword = keyword.trim();
            if !keyword.is_empty() {
                out.push(format!("user_keyword={keyword}"));
            }
        }
    }
    out
}

fn redact_bes_headers(argv: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for arg in argv {
        if redact_next {
            out.push("<redacted>".to_owned());
            redact_next = false;
            continue;
        }

        match arg.as_str() {
            "--bes_header" | "--bes-header" => {
                out.push(arg);
                redact_next = true;
            }
            _ if arg.starts_with("--bes_header=") => {
                out.push("--bes_header=<redacted>".to_owned());
            }
            _ if arg.starts_with("--bes-header=") => {
                out.push("--bes-header=<redacted>".to_owned());
            }
            _ => out.push(arg),
        }
    }
    out
}

fn shell_join(args: &[String]) -> String {
    shlex::try_join(args.iter().map(String::as_str)).unwrap_or_else(|_| args.join(" "))
}

pub(crate) struct BuildEventProtocolSubscriber {
    sender: Option<mpsc::UnboundedSender<publish_build_event::PublishBuildToolEventStreamRequest>>,
    upload: Option<JoinHandle<buck2_error::Result<UploadSummary>>>,
    sequence_number: i64,
    config: BuildEventProtocolConfig,
    exit_code: Option<(String, u32)>,
    error_seen: bool,
    workspace_status_sent: bool,
    finished_sent: bool,
}

impl BuildEventProtocolSubscriber {
    pub(crate) fn new(config: BuildEventProtocolConfig) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let upload = tokio::spawn(upload_build_events(config.clone(), receiver));
        let mut this = Self {
            sender: Some(sender),
            upload: Some(upload),
            sequence_number: 0,
            config,
            exit_code: None,
            error_seen: false,
            workspace_status_sent: false,
            finished_sent: false,
        };

        this.send_bazel_event(this.started_event());
        this.send_bazel_event(this.options_parsed_event());
        this
    }

    fn next_sequence_number(&mut self) -> i64 {
        self.sequence_number += 1;
        self.sequence_number
    }

    fn send_request(&mut self, event: google_devtools_build_v1::BuildEvent, sequence_number: i64) {
        let Some(sender) = &self.sender else {
            return;
        };

        let mut request = publish_build_event::PublishBuildToolEventStreamRequest {
            ordered_build_event: Some(publish_build_event::OrderedBuildEvent {
                stream_id: Some(tool_stream_id(&self.config)),
                sequence_number,
                event: Some(event),
            }),
            notification_keywords: Vec::new(),
            project_id: self.config.project_id.clone(),
            check_preceding_lifecycle_events_present: false,
        };

        if sequence_number == 1 {
            request.notification_keywords = self.config.keywords.clone();
        }

        if sender.send(request).is_err() {
            self.sender = None;
        }
    }

    fn send_bazel_event(&mut self, event: build_event_stream::BuildEvent) {
        let sequence_number = self.next_sequence_number();
        let event_time = event_timestamp();
        let any = Any {
            type_url: BAZEL_BUILD_EVENT_TYPE_URL.to_owned(),
            value: event.encode_to_vec(),
        };
        self.send_request(
            google_devtools_build_v1::BuildEvent {
                event_time: Some(event_time),
                event: Some(google_devtools_build_v1::build_event::Event::BazelEvent(
                    any,
                )),
            },
            sequence_number,
        );
    }

    fn send_component_stream_finished(&mut self) {
        let sequence_number = self.next_sequence_number();
        self.send_request(
            google_devtools_build_v1::BuildEvent {
                event_time: Some(event_timestamp()),
                event: Some(
                    google_devtools_build_v1::build_event::Event::ComponentStreamFinished(
                        google_devtools_build_v1::build_event::BuildComponentStreamFinished {
                            r#type: google_devtools_build_v1::build_event::build_component_stream_finished::FinishType::Finished as i32,
                        },
                    ),
                ),
            },
            sequence_number,
        );
    }

    fn started_event(&self) -> build_event_stream::BuildEvent {
        let mut children = vec![
            options_parsed_id(),
            workspace_status_id(),
            build_finished_id(),
        ];
        if !self.config.target_patterns.is_empty() {
            children.push(pattern_id(self.config.target_patterns.clone()));
        }
        build_event_stream::BuildEvent {
            id: Some(started_id()),
            children,
            last_message: false,
            payload: Some(build_event_stream::build_event::Payload::Started(
                build_event_stream::BuildStarted {
                    uuid: self.config.invocation_id.clone(),
                    start_time_millis: millis_since_epoch(self.config.start_time),
                    start_time: Some(timestamp(self.config.start_time)),
                    build_tool_version: build_tool_version(),
                    options_description: shell_join(&self.config.argv),
                    command: self.config.command_name.clone(),
                    working_directory: self.config.working_directory.clone(),
                    workspace_directory: self.config.workspace_directory.clone(),
                    server_pid: std::process::id().into(),
                    host: host(),
                    user: user(),
                },
            )),
        }
    }

    fn options_parsed_event(&self) -> build_event_stream::BuildEvent {
        build_event_stream::BuildEvent {
            id: Some(options_parsed_id()),
            children: Vec::new(),
            last_message: false,
            payload: Some(build_event_stream::build_event::Payload::OptionsParsed(
                build_event_stream::OptionsParsed {
                    startup_options: Vec::new(),
                    explicit_startup_options: Vec::new(),
                    cmd_line: self.config.argv.clone(),
                    explicit_cmd_line: self.config.argv.clone(),
                    invocation_policy: None,
                    tool_tag: "buck2".to_owned(),
                },
            )),
        }
    }

    fn send_workspace_status(&mut self) {
        if self.workspace_status_sent {
            return;
        }
        self.workspace_status_sent = true;

        let mut items = Vec::new();
        push_item(&mut items, "USER", user());
        push_item(&mut items, "HOST", host());
        if !self.config.target_patterns.is_empty() {
            push_item(&mut items, "PATTERN", self.config.target_patterns.join(" "));
        }
        if std::env::var_os("CI").is_some() {
            push_item(&mut items, "ROLE", "CI".to_owned());
        }
        if let Some(repo_url) = first_env(&[
            "REPO_URL",
            "BUILDKITE_REPO",
            "GIT_REPOSITORY_URL",
            "GIT_URL",
            "CIRCLE_REPOSITORY_URL",
            "CI_REPOSITORY_URL",
            "GITHUB_REPOSITORY",
        ]) {
            push_item(&mut items, "REPO_URL", repo_url);
        }
        if let Some(branch) = first_env(&[
            "GIT_BRANCH",
            "BUILDKITE_BRANCH",
            "CIRCLE_BRANCH",
            "GITHUB_HEAD_REF",
            "GITHUB_REF",
        ]) {
            push_item(
                &mut items,
                "GIT_BRANCH",
                branch.trim_start_matches("refs/heads/").to_owned(),
            );
        }
        if let Some(commit) = first_env(&[
            "COMMIT_SHA",
            "GIT_COMMIT",
            "BUILDKITE_COMMIT",
            "CIRCLE_SHA1",
            "GITHUB_SHA",
            "CI_COMMIT_SHA",
        ]) {
            push_item(&mut items, "COMMIT_SHA", commit);
        }

        self.send_bazel_event(build_event_stream::BuildEvent {
            id: Some(workspace_status_id()),
            children: Vec::new(),
            last_message: false,
            payload: Some(build_event_stream::build_event::Payload::WorkspaceStatus(
                build_event_stream::WorkspaceStatus { item: items },
            )),
        });
    }

    fn emit_build_targets(&mut self, build_targets: &[BuildTarget], project_root: &str) {
        let labels = build_targets
            .iter()
            .map(|target| target.target.clone())
            .collect::<Vec<_>>();

        if !self.config.target_patterns.is_empty() || !labels.is_empty() {
            let patterns = if self.config.target_patterns.is_empty() {
                labels.clone()
            } else {
                self.config.target_patterns.clone()
            };
            let children = labels
                .iter()
                .map(|label| target_configured_id(label))
                .collect();
            self.send_bazel_event(build_event_stream::BuildEvent {
                id: Some(pattern_id(patterns)),
                children,
                last_message: false,
                payload: Some(build_event_stream::build_event::Payload::Expanded(
                    build_event_stream::PatternExpanded::default(),
                )),
            });
        }

        for target in build_targets {
            let completed_id = target_completed_id(target);
            self.send_bazel_event(build_event_stream::BuildEvent {
                id: Some(target_configured_id(&target.target)),
                children: vec![completed_id.clone()],
                last_message: false,
                payload: Some(build_event_stream::build_event::Payload::Configured(
                    build_event_stream::TargetConfigured {
                        target_kind: target_kind(target),
                        test_size: build_event_stream::TestSize::Unknown as i32,
                        tag: Vec::new(),
                    },
                )),
            });

            self.send_bazel_event(build_event_stream::BuildEvent {
                id: Some(completed_id),
                children: Vec::new(),
                last_message: false,
                payload: Some(build_event_stream::build_event::Payload::Completed(
                    build_event_stream::TargetComplete {
                        success: true,
                        output_group: Vec::new(),
                        important_output: target_outputs(target, project_root),
                        tag: Vec::new(),
                    },
                )),
            });
        }
    }

    fn send_progress_text(&mut self, stdout: Option<String>, stderr: Option<String>) {
        let stdout = stdout.unwrap_or_default();
        let stderr = stderr.unwrap_or_default();
        if stdout.is_empty() && stderr.is_empty() {
            return;
        }
        self.send_bazel_event(build_event_stream::BuildEvent {
            id: Some(progress_id(self.sequence_number as i32 + 1)),
            children: Vec::new(),
            last_message: false,
            payload: Some(build_event_stream::build_event::Payload::Progress(
                build_event_stream::Progress { stdout, stderr },
            )),
        });
    }

    fn send_progress_bytes(&mut self, stdout: Option<&[u8]>, stderr: Option<&[u8]>) {
        if let Some(stdout) = stdout {
            for chunk in stdout.chunks(DEFAULT_PROGRESS_CHUNK_SIZE) {
                self.send_progress_text(Some(String::from_utf8_lossy(chunk).into_owned()), None);
            }
        }
        if let Some(stderr) = stderr {
            for chunk in stderr.chunks(DEFAULT_PROGRESS_CHUNK_SIZE) {
                self.send_progress_text(None, Some(String::from_utf8_lossy(chunk).into_owned()));
            }
        }
    }

    fn send_finished(&mut self) {
        if self.finished_sent {
            return;
        }
        self.finished_sent = true;
        self.send_workspace_status();

        let (name, code) = self.exit_code.clone().unwrap_or_else(|| {
            if self.error_seen {
                ("UNKNOWN_FAILURE".to_owned(), 1)
            } else {
                ("SUCCESS".to_owned(), 0)
            }
        });
        let finish_time = SystemTime::now();

        self.send_bazel_event(build_event_stream::BuildEvent {
            id: Some(build_finished_id()),
            children: Vec::new(),
            last_message: true,
            payload: Some(build_event_stream::build_event::Payload::Finished(
                build_event_stream::BuildFinished {
                    overall_success: code == 0,
                    exit_code: Some(build_event_stream::build_finished::ExitCode {
                        name,
                        code: code as i32,
                    }),
                    finish_time_millis: millis_since_epoch(finish_time),
                    finish_time: Some(timestamp(finish_time)),
                },
            )),
        });
    }
}

#[async_trait]
impl EventSubscriber for BuildEventProtocolSubscriber {
    fn name(&self) -> &'static str {
        "build event protocol"
    }

    async fn handle_output(&mut self, raw_output: &[u8]) -> buck2_error::Result<()> {
        self.send_progress_bytes(Some(raw_output), None);
        Ok(())
    }

    async fn handle_tailer_stderr(&mut self, stderr: &str) -> buck2_error::Result<()> {
        self.send_progress_text(None, Some(format!("{stderr}\n")));
        Ok(())
    }

    async fn handle_events(
        &mut self,
        _event: &[std::sync::Arc<BuckEvent>],
    ) -> buck2_error::Result<()> {
        Ok(())
    }

    async fn handle_command_result(&mut self, result: &CommandResult) -> buck2_error::Result<()> {
        match result.result.as_ref() {
            Some(command_result::Result::BuildResponse(response)) => {
                self.emit_build_targets(&response.build_targets, &response.project_root);
                if !response.errors.is_empty() {
                    self.error_seen = true;
                }
            }
            Some(command_result::Result::TestResponse(response)) => {
                self.send_progress_text(
                    Some(response.executor_stdout.clone()),
                    Some(response.executor_stderr.clone()),
                );
                for message in &response.executor_info_messages {
                    self.send_progress_text(None, Some(format!("{message}\n")));
                }
                if !response.errors.is_empty() || response.executor_exit_code != 0 {
                    self.error_seen = true;
                }
            }
            Some(command_result::Result::BxlResponse(response)) => {
                if !response.errors.is_empty() {
                    self.error_seen = true;
                }
            }
            Some(command_result::Result::Error(_)) => {
                self.error_seen = true;
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_error(&mut self, _error: &buck2_error::Error) -> buck2_error::Result<()> {
        self.error_seen = true;
        Ok(())
    }

    fn handle_exit_result(&mut self, result: &ExitResult) {
        let code = result.exit_code().unwrap_or(if result.is_success() {
            ExitCode::Success
        } else {
            ExitCode::UnknownFailure
        });
        self.exit_code = Some((result.name().to_owned(), code.exit_code()));
    }

    async fn finalize(mut self: Box<Self>) -> buck2_error::Result<()> {
        self.send_finished();
        self.send_component_stream_finished();
        self.sender.take();

        let Some(upload) = self.upload.take() else {
            return Ok(());
        };

        let upload = async move {
            upload
                .await
                .map_err(|e| BepError::Join(e.to_string()).into())
                .and_then(|res| res)
        };

        let summary =
            match self.config.timeout {
                Some(timeout) if !timeout.is_zero() => tokio::time::timeout(timeout, upload)
                    .await
                    .map_err(|_| BepError::Upload(format!("timed out after {timeout:?}")))??,
                _ => upload.await?,
            };

        if let Some(results_url) = &self.config.results_url {
            let separator = if results_url.ends_with('/') { "" } else { "/" };
            crate::eprintln!(
                "BuildBuddy invocation: {}{}{}",
                results_url,
                separator,
                self.config.invocation_id
            )?;
        } else {
            tracing::info!(
                "Uploaded {} BEP events to {} (last ack: {:?})",
                summary.acked_events,
                self.config.backend,
                summary.last_ack
            );
        }

        Ok(())
    }
}

#[derive(Debug)]
struct UploadSummary {
    acked_events: u64,
    last_ack: Option<i64>,
}

async fn upload_build_events(
    config: BuildEventProtocolConfig,
    receiver: mpsc::UnboundedReceiver<publish_build_event::PublishBuildToolEventStreamRequest>,
) -> buck2_error::Result<UploadSummary> {
    let backend = BesBackend::parse(&config.backend)?;
    let mut endpoint = Endpoint::from_shared(backend.uri.clone())
        .map_err(|e| BepError::InvalidBackend(format!("{} ({e})", config.backend)))?;
    if backend.tls {
        endpoint = endpoint
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|e| BepError::Upload(e.to_string()))?;
    }

    let channel = endpoint
        .connect()
        .await
        .map_err(|e| BepError::Upload(e.to_string()))?;
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready()
        .await
        .map_err(|e| BepError::Upload(e.to_string()))?;

    let mut request = tonic::Request::new(UnboundedReceiverStream::new(receiver));
    add_headers(request.metadata_mut(), &config.headers)?;

    let path =
        tonic::codegen::http::uri::PathAndQuery::from_static(PUBLISH_BUILD_TOOL_EVENT_STREAM_PATH);
    let codec = tonic_prost::ProstCodec::<
        publish_build_event::PublishBuildToolEventStreamRequest,
        publish_build_event::PublishBuildToolEventStreamResponse,
    >::default();
    let mut response = grpc
        .streaming(request, path, codec)
        .await
        .map_err(|e| BepError::Upload(e.to_string()))?
        .into_inner();

    let mut acked_events = 0;
    let mut last_ack = None;
    while let Some(response) = response
        .message()
        .await
        .map_err(|e| BepError::Upload(e.to_string()))?
    {
        acked_events += 1;
        last_ack = Some(response.sequence_number);
    }

    Ok(UploadSummary {
        acked_events,
        last_ack,
    })
}

fn add_headers(
    metadata: &mut tonic::metadata::MetadataMap,
    headers: &[String],
) -> buck2_error::Result<()> {
    for header in headers {
        let (name, value) = header
            .split_once('=')
            .ok_or_else(|| BepError::InvalidHeader(header.clone()))?;
        let key = MetadataKey::from_bytes(name.trim().as_bytes())
            .map_err(|_| BepError::InvalidHeader(header.clone()))?;
        let value = MetadataValue::try_from(value.trim())
            .map_err(|_| BepError::InvalidHeader(header.clone()))?;
        metadata.append(key, value);
    }
    Ok(())
}

struct BesBackend {
    uri: String,
    tls: bool,
}

impl BesBackend {
    fn parse(value: &str) -> buck2_error::Result<Self> {
        if value.trim().is_empty() {
            return Err(BepError::InvalidBackend(value.to_owned()).into());
        }
        if let Some(rest) = value.strip_prefix("grpc://") {
            return Ok(Self {
                uri: format!("http://{rest}"),
                tls: false,
            });
        }
        if let Some(rest) = value.strip_prefix("grpcs://") {
            return Ok(Self {
                uri: format!("https://{rest}"),
                tls: true,
            });
        }
        if value.starts_with("http://") {
            return Ok(Self {
                uri: value.to_owned(),
                tls: false,
            });
        }
        if value.starts_with("https://") {
            return Ok(Self {
                uri: value.to_owned(),
                tls: true,
            });
        }
        Ok(Self {
            uri: format!("https://{value}"),
            tls: true,
        })
    }
}

fn tool_stream_id(config: &BuildEventProtocolConfig) -> google_devtools_build_v1::StreamId {
    google_devtools_build_v1::StreamId {
        build_id: config.build_id.clone(),
        invocation_id: config.invocation_id.clone(),
        component: google_devtools_build_v1::stream_id::BuildComponent::Tool as i32,
    }
}

fn timestamp(time: SystemTime) -> Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => Timestamp {
            seconds: duration.as_secs() as i64,
            nanos: duration.subsec_nanos() as i32,
        },
        Err(_) => Timestamp {
            seconds: 0,
            nanos: 0,
        },
    }
}

fn event_timestamp() -> Timestamp {
    timestamp(SystemTime::now())
}

fn millis_since_epoch(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn build_tool_version() -> String {
    buck2_build_info::revision()
        .map(str::to_owned)
        .unwrap_or_else(|| "0.0.0".to_owned())
}

fn user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

fn host() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_default()
}

fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
}

fn push_item(
    items: &mut Vec<build_event_stream::workspace_status::Item>,
    key: &str,
    value: String,
) {
    if !value.is_empty() {
        items.push(build_event_stream::workspace_status::Item {
            key: key.to_owned(),
            value,
        });
    }
}

fn started_id() -> build_event_stream::BuildEventId {
    build_event_stream::BuildEventId {
        id: Some(build_event_stream::build_event_id::Id::Started(
            build_event_stream::build_event_id::BuildStartedId {},
        )),
    }
}

fn options_parsed_id() -> build_event_stream::BuildEventId {
    build_event_stream::BuildEventId {
        id: Some(build_event_stream::build_event_id::Id::OptionsParsed(
            build_event_stream::build_event_id::OptionsParsedId {},
        )),
    }
}

fn workspace_status_id() -> build_event_stream::BuildEventId {
    build_event_stream::BuildEventId {
        id: Some(build_event_stream::build_event_id::Id::WorkspaceStatus(
            build_event_stream::build_event_id::WorkspaceStatusId {},
        )),
    }
}

fn pattern_id(pattern: Vec<String>) -> build_event_stream::BuildEventId {
    build_event_stream::BuildEventId {
        id: Some(build_event_stream::build_event_id::Id::Pattern(
            build_event_stream::build_event_id::PatternExpandedId { pattern },
        )),
    }
}

fn progress_id(opaque_count: i32) -> build_event_stream::BuildEventId {
    build_event_stream::BuildEventId {
        id: Some(build_event_stream::build_event_id::Id::Progress(
            build_event_stream::build_event_id::ProgressId { opaque_count },
        )),
    }
}

fn build_finished_id() -> build_event_stream::BuildEventId {
    build_event_stream::BuildEventId {
        id: Some(build_event_stream::build_event_id::Id::BuildFinished(
            build_event_stream::build_event_id::BuildFinishedId {},
        )),
    }
}

fn target_configured_id(label: &str) -> build_event_stream::BuildEventId {
    build_event_stream::BuildEventId {
        id: Some(build_event_stream::build_event_id::Id::TargetConfigured(
            build_event_stream::build_event_id::TargetConfiguredId {
                label: label.to_owned(),
                aspect: String::new(),
            },
        )),
    }
}

fn target_completed_id(target: &BuildTarget) -> build_event_stream::BuildEventId {
    build_event_stream::BuildEventId {
        id: Some(build_event_stream::build_event_id::Id::TargetCompleted(
            build_event_stream::build_event_id::TargetCompletedId {
                label: target.target.clone(),
                aspect: String::new(),
                configuration: Some(build_event_stream::build_event_id::ConfigurationId {
                    id: if target.configuration.is_empty() {
                        "buck2".to_owned()
                    } else {
                        target.configuration.clone()
                    },
                }),
            },
        )),
    }
}

fn target_kind(target: &BuildTarget) -> String {
    target
        .target_rule_type_name
        .as_deref()
        .map(|kind| {
            if kind.ends_with(" rule") {
                kind.to_owned()
            } else {
                format!("{kind} rule")
            }
        })
        .unwrap_or_else(|| "buck2 rule".to_owned())
}

fn target_outputs(target: &BuildTarget, project_root: &str) -> Vec<build_event_stream::File> {
    target
        .outputs
        .iter()
        .map(|output| {
            let uri = if project_root.is_empty() {
                output.path.clone()
            } else {
                let root = project_root.trim_end_matches('/');
                format!("file://{root}/{}", output.path)
            };
            build_event_stream::File {
                path_prefix: Vec::new(),
                name: output.path.clone(),
                file: Some(build_event_stream::file::File::Uri(uri)),
                digest: String::new(),
                length: 0,
            }
        })
        .collect()
}

#[allow(clippy::large_enum_variant)]
pub(crate) mod google_devtools_build_v1 {
    use prost_types::Timestamp;

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct StreamId {
        #[prost(string, tag = "1")]
        pub(crate) build_id: String,
        #[prost(enumeration = "stream_id::BuildComponent", tag = "3")]
        pub(crate) component: i32,
        #[prost(string, tag = "6")]
        pub(crate) invocation_id: String,
    }

    pub(crate) mod stream_id {
        #[derive(
            Clone,
            Copy,
            Debug,
            PartialEq,
            Eq,
            Hash,
            PartialOrd,
            Ord,
            ::prost::Enumeration
        )]
        #[repr(i32)]
        pub(crate) enum BuildComponent {
            UnknownComponent = 0,
            Controller = 1,
            Worker = 2,
            Tool = 3,
        }
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct BuildEvent {
        #[prost(message, optional, tag = "1")]
        pub(crate) event_time: Option<Timestamp>,
        #[prost(oneof = "build_event::Event", tags = "59, 60")]
        pub(crate) event: Option<build_event::Event>,
    }

    pub(crate) mod build_event {
        use prost_types::Any;

        #[derive(Clone, PartialEq, ::prost::Oneof)]
        pub(crate) enum Event {
            #[prost(message, tag = "59")]
            ComponentStreamFinished(BuildComponentStreamFinished),
            #[prost(message, tag = "60")]
            BazelEvent(Any),
        }

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct BuildComponentStreamFinished {
            #[prost(enumeration = "build_component_stream_finished::FinishType", tag = "1")]
            pub(crate) r#type: i32,
        }

        pub(crate) mod build_component_stream_finished {
            #[derive(
                Clone,
                Copy,
                Debug,
                PartialEq,
                Eq,
                Hash,
                PartialOrd,
                Ord,
                ::prost::Enumeration
            )]
            #[repr(i32)]
            pub(crate) enum FinishType {
                Unspecified = 0,
                Finished = 1,
                Expired = 2,
            }
        }
    }
}

pub(crate) mod publish_build_event {
    use super::google_devtools_build_v1;

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct OrderedBuildEvent {
        #[prost(message, optional, tag = "1")]
        pub(crate) stream_id: Option<google_devtools_build_v1::StreamId>,
        #[prost(int64, tag = "2")]
        pub(crate) sequence_number: i64,
        #[prost(message, optional, tag = "3")]
        pub(crate) event: Option<google_devtools_build_v1::BuildEvent>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct PublishBuildToolEventStreamRequest {
        #[prost(message, optional, tag = "4")]
        pub(crate) ordered_build_event: Option<OrderedBuildEvent>,
        #[prost(string, repeated, tag = "5")]
        pub(crate) notification_keywords: Vec<String>,
        #[prost(string, tag = "6")]
        pub(crate) project_id: String,
        #[prost(bool, tag = "7")]
        pub(crate) check_preceding_lifecycle_events_present: bool,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct PublishBuildToolEventStreamResponse {
        #[prost(message, optional, tag = "1")]
        pub(crate) stream_id: Option<google_devtools_build_v1::StreamId>,
        #[prost(int64, tag = "2")]
        pub(crate) sequence_number: i64,
    }
}

#[allow(clippy::large_enum_variant)]
pub(crate) mod build_event_stream {
    use prost_types::Timestamp;

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct BuildEventId {
        #[prost(oneof = "build_event_id::Id", tags = "2, 3, 4, 5, 9, 12, 14, 16")]
        pub(crate) id: Option<build_event_id::Id>,
    }

    pub(crate) mod build_event_id {
        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct ProgressId {
            #[prost(int32, tag = "1")]
            pub(crate) opaque_count: i32,
        }

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct BuildStartedId {}

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct OptionsParsedId {}

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct WorkspaceStatusId {}

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct PatternExpandedId {
            #[prost(string, repeated, tag = "1")]
            pub(crate) pattern: Vec<String>,
        }

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct ConfigurationId {
            #[prost(string, tag = "1")]
            pub(crate) id: String,
        }

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct TargetConfiguredId {
            #[prost(string, tag = "1")]
            pub(crate) label: String,
            #[prost(string, tag = "2")]
            pub(crate) aspect: String,
        }

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct TargetCompletedId {
            #[prost(string, tag = "1")]
            pub(crate) label: String,
            #[prost(string, tag = "2")]
            pub(crate) aspect: String,
            #[prost(message, optional, tag = "3")]
            pub(crate) configuration: Option<ConfigurationId>,
        }

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct BuildFinishedId {}

        #[derive(Clone, PartialEq, ::prost::Oneof)]
        pub(crate) enum Id {
            #[prost(message, tag = "2")]
            Progress(ProgressId),
            #[prost(message, tag = "3")]
            Started(BuildStartedId),
            #[prost(message, tag = "4")]
            Pattern(PatternExpandedId),
            #[prost(message, tag = "5")]
            TargetCompleted(TargetCompletedId),
            #[prost(message, tag = "9")]
            BuildFinished(BuildFinishedId),
            #[prost(message, tag = "12")]
            OptionsParsed(OptionsParsedId),
            #[prost(message, tag = "14")]
            WorkspaceStatus(WorkspaceStatusId),
            #[prost(message, tag = "16")]
            TargetConfigured(TargetConfiguredId),
        }
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct Progress {
        #[prost(string, tag = "1")]
        pub(crate) stdout: String,
        #[prost(string, tag = "2")]
        pub(crate) stderr: String,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct BuildStarted {
        #[prost(string, tag = "1")]
        pub(crate) uuid: String,
        #[prost(int64, tag = "2")]
        pub(crate) start_time_millis: i64,
        #[prost(string, tag = "3")]
        pub(crate) build_tool_version: String,
        #[prost(string, tag = "4")]
        pub(crate) options_description: String,
        #[prost(string, tag = "5")]
        pub(crate) command: String,
        #[prost(string, tag = "6")]
        pub(crate) working_directory: String,
        #[prost(string, tag = "7")]
        pub(crate) workspace_directory: String,
        #[prost(int64, tag = "8")]
        pub(crate) server_pid: i64,
        #[prost(message, optional, tag = "9")]
        pub(crate) start_time: Option<Timestamp>,
        #[prost(string, tag = "10")]
        pub(crate) host: String,
        #[prost(string, tag = "11")]
        pub(crate) user: String,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct OptionsParsed {
        #[prost(string, repeated, tag = "1")]
        pub(crate) startup_options: Vec<String>,
        #[prost(string, repeated, tag = "2")]
        pub(crate) explicit_startup_options: Vec<String>,
        #[prost(string, repeated, tag = "3")]
        pub(crate) cmd_line: Vec<String>,
        #[prost(string, repeated, tag = "4")]
        pub(crate) explicit_cmd_line: Vec<String>,
        #[prost(message, optional, tag = "5")]
        pub(crate) invocation_policy: Option<::prost_types::Any>,
        #[prost(string, tag = "6")]
        pub(crate) tool_tag: String,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct WorkspaceStatus {
        #[prost(message, repeated, tag = "1")]
        pub(crate) item: Vec<workspace_status::Item>,
    }

    pub(crate) mod workspace_status {
        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct Item {
            #[prost(string, tag = "1")]
            pub(crate) key: String,
            #[prost(string, tag = "2")]
            pub(crate) value: String,
        }
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct PatternExpanded {}

    #[derive(
        Clone,
        Copy,
        Debug,
        PartialEq,
        Eq,
        Hash,
        PartialOrd,
        Ord,
        ::prost::Enumeration
    )]
    #[repr(i32)]
    pub(crate) enum TestSize {
        Unknown = 0,
        Small = 1,
        Medium = 2,
        Large = 3,
        Enormous = 4,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct TargetConfigured {
        #[prost(string, tag = "1")]
        pub(crate) target_kind: String,
        #[prost(enumeration = "TestSize", tag = "2")]
        pub(crate) test_size: i32,
        #[prost(string, repeated, tag = "3")]
        pub(crate) tag: Vec<String>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct File {
        #[prost(string, tag = "1")]
        pub(crate) name: String,
        #[prost(oneof = "file::File", tags = "2, 3, 7")]
        pub(crate) file: Option<file::File>,
        #[prost(string, repeated, tag = "4")]
        pub(crate) path_prefix: Vec<String>,
        #[prost(string, tag = "5")]
        pub(crate) digest: String,
        #[prost(int64, tag = "6")]
        pub(crate) length: i64,
    }

    pub(crate) mod file {
        #[derive(Clone, PartialEq, ::prost::Oneof)]
        pub(crate) enum File {
            #[prost(string, tag = "2")]
            Uri(String),
            #[prost(bytes, tag = "3")]
            Contents(Vec<u8>),
            #[prost(string, tag = "7")]
            SymlinkTargetPath(String),
        }
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct OutputGroup {
        #[prost(string, tag = "1")]
        pub(crate) name: String,
        #[prost(bool, tag = "4")]
        pub(crate) incomplete: bool,
        #[prost(message, repeated, tag = "5")]
        pub(crate) inline_files: Vec<File>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct TargetComplete {
        #[prost(bool, tag = "1")]
        pub(crate) success: bool,
        #[prost(message, repeated, tag = "2")]
        pub(crate) output_group: Vec<OutputGroup>,
        #[prost(string, repeated, tag = "3")]
        pub(crate) tag: Vec<String>,
        #[prost(message, repeated, tag = "4")]
        pub(crate) important_output: Vec<File>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct BuildFinished {
        #[prost(bool, tag = "1")]
        pub(crate) overall_success: bool,
        #[prost(int64, tag = "2")]
        pub(crate) finish_time_millis: i64,
        #[prost(message, optional, tag = "3")]
        pub(crate) exit_code: Option<build_finished::ExitCode>,
        #[prost(message, optional, tag = "5")]
        pub(crate) finish_time: Option<Timestamp>,
    }

    pub(crate) mod build_finished {
        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct ExitCode {
            #[prost(string, tag = "1")]
            pub(crate) name: String,
            #[prost(int32, tag = "2")]
            pub(crate) code: i32,
        }
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct BuildEvent {
        #[prost(message, optional, tag = "1")]
        pub(crate) id: Option<BuildEventId>,
        #[prost(message, repeated, tag = "2")]
        pub(crate) children: Vec<BuildEventId>,
        #[prost(bool, tag = "20")]
        pub(crate) last_message: bool,
        #[prost(oneof = "build_event::Payload", tags = "3, 5, 6, 8, 13, 14, 16, 18")]
        pub(crate) payload: Option<build_event::Payload>,
    }

    pub(crate) mod build_event {
        use super::*;

        #[derive(Clone, PartialEq, ::prost::Oneof)]
        pub(crate) enum Payload {
            #[prost(message, tag = "3")]
            Progress(Progress),
            #[prost(message, tag = "5")]
            Started(BuildStarted),
            #[prost(message, tag = "6")]
            Expanded(PatternExpanded),
            #[prost(message, tag = "8")]
            Completed(TargetComplete),
            #[prost(message, tag = "13")]
            OptionsParsed(OptionsParsed),
            #[prost(message, tag = "14")]
            Finished(BuildFinished),
            #[prost(message, tag = "16")]
            WorkspaceStatus(WorkspaceStatus),
            #[prost(message, tag = "18")]
            Configured(TargetConfigured),
        }
    }
}

pub(crate) fn get_bep_subscriber<T: crate::streaming::StreamingCommand>(
    cmd: &T,
    ctx: &ClientCommandContext,
    paths: Option<&buck2_common::invocation_paths::InvocationPaths>,
) -> buck2_error::Result<Option<Box<dyn EventSubscriber>>> {
    Ok(BuildEventProtocolConfig::from_command(cmd, ctx, paths)?
        .map(BuildEventProtocolSubscriber::new)
        .map(|subscriber| Box::new(subscriber) as Box<dyn EventSubscriber>))
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;

    #[test]
    fn redacts_bes_header_values_from_argv() {
        let argv = vec![
            "buck2".to_owned(),
            "build".to_owned(),
            "--bes_header".to_owned(),
            "x-buildbuddy-api-key=secret".to_owned(),
            "--bes-header=authorization=Bearer secret".to_owned(),
            "//:target".to_owned(),
        ];

        assert_eq!(
            redact_bes_headers(argv),
            vec![
                "buck2",
                "build",
                "--bes_header",
                "<redacted>",
                "--bes-header=<redacted>",
                "//:target"
            ]
        );
    }

    #[test]
    fn parses_build_event_service_backends() {
        let backend = BesBackend::parse("grpc://localhost:1985").unwrap();
        assert_eq!(backend.uri, "http://localhost:1985");
        assert!(!backend.tls);

        let backend = BesBackend::parse("grpcs://remote.buildbuddy.io").unwrap();
        assert_eq!(backend.uri, "https://remote.buildbuddy.io");
        assert!(backend.tls);

        let backend = BesBackend::parse("remote.buildbuddy.io").unwrap();
        assert_eq!(backend.uri, "https://remote.buildbuddy.io");
        assert!(backend.tls);
    }

    #[test]
    fn splits_user_keywords_like_bazel() {
        assert_eq!(
            keywords(
                "build",
                &["ci, pull-request".to_owned(), "linux".to_owned()]
            ),
            vec![
                "command_name=build",
                "protocol_name=BEP",
                "tool=buck2",
                "user_keyword=ci",
                "user_keyword=pull-request",
                "user_keyword=linux"
            ]
        );
    }

    #[test]
    fn build_finished_uses_finish_time_field() {
        let finished = build_event_stream::BuildFinished {
            overall_success: true,
            finish_time_millis: 123_000,
            exit_code: Some(build_event_stream::build_finished::ExitCode {
                name: "SUCCESS".to_owned(),
                code: 0,
            }),
            finish_time: Some(Timestamp {
                seconds: 123,
                nanos: 456,
            }),
        };

        let bytes = finished.encode_to_vec();
        let decoded = build_event_stream::BuildFinished::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.finish_time.unwrap().seconds, 123);
    }
}
