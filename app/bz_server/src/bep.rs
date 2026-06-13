use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use bz_cli_proto::BuildTarget;
use bz_cli_proto::ClientContext;
use bz_cli_proto::CommandResult;
use bz_cli_proto::command_result;
use bz_events::BuckEvent;
use bz_profile::chrome_trace::ChromeTraceProfileWriter;
use bz_wrapper_common::invocation_id::TraceId;
use prost::Message;
use prost_types::Any;
use prost_types::Timestamp;
use re_grpc_proto::build::bazel::remote::execution::v2::RequestMetadata;
use re_grpc_proto::build::bazel::remote::execution::v2::ToolDetails;
use re_grpc_proto::google::bytestream::WriteRequest;
use re_grpc_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::metadata::MetadataKey;
use tonic::metadata::MetadataValue;
use tonic::transport::ClientTlsConfig;
use tonic::transport::Endpoint;

const BAZEL_BUILD_EVENT_TYPE_URL: &str = "type.googleapis.com/build_event_stream.BuildEvent";
const PUBLISH_BUILD_TOOL_EVENT_STREAM_PATH: &str =
    "/google.devtools.build.v1.PublishBuildEvent/PublishBuildToolEventStream";
const DEFAULT_PROGRESS_CHUNK_SIZE: usize = 1024 * 1024;
const TERMINAL_PROGRESS_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const PROFILE_NAME: &str = "command.profile.gz";
const BYTESTREAM_UPLOAD_CHUNK_SIZE: usize = 2 * 1024 * 1024;
const BAZEL_REQUEST_METADATA_HEADER: &str = "build.bazel.remote.execution.v2.requestmetadata-bin";

fn bes_invocation_url(results_url: &str, invocation_id: &str) -> String {
    let separator = if results_url.ends_with('/') { "" } else { "/" };
    format!("{results_url}{separator}{invocation_id}")
}

fn bes_results_url_message(results_url: &str, invocation_id: &str, color: bool) -> String {
    let url = bes_invocation_url(results_url, invocation_id);
    if color {
        format!("\x1b[32mINFO:\x1b[0m Streaming build results to: \x1b[4;36m{url}\x1b[0m")
    } else {
        format!("INFO: Streaming build results to: {url}")
    }
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Tier0)]
enum BepError {
    #[error("Invalid BES backend `{0}`")]
    InvalidBackend(String),
    #[error("Invalid BES header `{0}`. Expected NAME=VALUE")]
    InvalidHeader(String),
    #[error("Invalid BES timeout `{0}`")]
    InvalidTimeout(String),
    #[error("BEP upload failed: {0}")]
    Upload(String),
    #[error("BEP timing profile upload failed: {0}")]
    ProfileUpload(String),
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
    trace_id: TraceId,
    sync: bool,
}

impl BuildEventProtocolConfig {
    fn from_client_context(
        ctx: &ClientContext,
        trace_id: TraceId,
        workspace_directory: String,
    ) -> bz_error::Result<Option<Self>> {
        let Some(options) = ctx.bes_options.as_ref() else {
            return Ok(None);
        };

        let timeout = options
            .timeout
            .as_ref()
            .map(proto_duration_to_duration)
            .transpose()?;
        let invocation_id = trace_id.to_string();
        Ok(Some(Self {
            backend: options.backend.clone(),
            headers: options.headers.clone(),
            project_id: options.instance_name.clone(),
            keywords: options.keywords.clone(),
            timeout,
            results_url: options.results_url.clone(),
            invocation_id: invocation_id.clone(),
            build_id: invocation_id,
            command_name: ctx.command_name.clone(),
            argv: redact_bes_headers(ctx.sanitized_argv.clone()),
            target_patterns: options.target_patterns.clone(),
            start_time: options
                .start_time
                .as_ref()
                .and_then(system_time_from_proto)
                .unwrap_or_else(SystemTime::now),
            working_directory: ctx.working_dir.clone(),
            workspace_directory,
            trace_id,
            sync: options.sync,
        }))
    }
}

fn proto_duration_to_duration(duration: &prost_types::Duration) -> bz_error::Result<Duration> {
    if duration.seconds < 0 || duration.nanos < 0 {
        return Err(BepError::InvalidTimeout(format!("{duration:?}")).into());
    }
    Ok(Duration::from_secs(duration.seconds as u64) + Duration::from_nanos(duration.nanos as u64))
}

fn system_time_from_proto(timestamp: &prost_types::Timestamp) -> Option<SystemTime> {
    if timestamp.seconds < 0 || timestamp.nanos < 0 {
        return None;
    }
    Some(
        UNIX_EPOCH
            + Duration::from_secs(timestamp.seconds as u64)
            + Duration::from_nanos(timestamp.nanos as u64),
    )
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

fn terminal_output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

struct TerminalProgressCoalescer {
    // Buffer terminal writes before creating a BES Progress event. BuildBuddy's
    // log writer applies ANSI cursor movement over each progress payload, so a
    // one-second batch lets cleared superconsole frames collapse into its
    // volatile tail instead of becoming durable build log lines.
    terminal: Vec<u8>,
    last_flush: Option<Instant>,
}

impl TerminalProgressCoalescer {
    fn new() -> Self {
        Self {
            terminal: Vec::new(),
            last_flush: None,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        // Keep a single terminal-ordered byte stream. Splitting stdout and stderr into
        // the two BEP Progress fields would let BuildBuddy reorder them when it
        // reconstructs the build log.
        self.terminal.extend_from_slice(bytes);
    }

    fn has_pending(&self) -> bool {
        !self.terminal.is_empty()
    }

    fn should_flush(&self, now: Instant) -> bool {
        self.has_pending()
            && self.last_flush.is_none_or(|last_flush| {
                now.duration_since(last_flush) >= TERMINAL_PROGRESS_FLUSH_INTERVAL
            })
    }

    fn take_next_chunk(&mut self, now: Instant) -> Option<Vec<u8>> {
        if !self.has_pending() {
            return None;
        }
        self.last_flush = Some(now);
        Some(take_progress_chunk(&mut self.terminal))
    }
}

fn take_progress_chunk(bytes: &mut Vec<u8>) -> Vec<u8> {
    if bytes.len() <= DEFAULT_PROGRESS_CHUNK_SIZE {
        std::mem::take(bytes)
    } else {
        let rest = bytes.split_off(DEFAULT_PROGRESS_CHUNK_SIZE);
        std::mem::replace(bytes, rest)
    }
}

pub(crate) struct BuildEventProtocolUploaderHandle {
    sender: mpsc::UnboundedSender<BuildEventProtocolMessage>,
    sync: bool,
    timeout: Option<Duration>,
}

impl BuildEventProtocolUploaderHandle {
    pub(crate) fn new(
        ctx: &ClientContext,
        trace_id: TraceId,
        workspace_directory: String,
    ) -> bz_error::Result<Option<Self>> {
        let Some(config) =
            BuildEventProtocolConfig::from_client_context(ctx, trace_id, workspace_directory)?
        else {
            return Ok(None);
        };
        let sync = config.sync;
        let timeout = config.timeout;
        let _backend = BesBackend::parse(&config.backend)?;
        validate_headers(&config.headers)?;
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run_build_event_protocol_uploader(config, receiver));
        Ok(Some(Self {
            sender,
            sync,
            timeout,
        }))
    }

    pub(crate) fn is_sync(&self) -> bool {
        self.sync
    }

    pub(crate) fn handle_buck_event(&self, event: &BuckEvent) {
        let _ignored = self
            .sender
            .send(BuildEventProtocolMessage::Buck(Box::new(event.clone())));
    }

    pub(crate) fn finish(&self, result: &CommandResult) {
        self.finish_owned(result.clone());
    }

    pub(crate) fn finish_owned(&self, result: CommandResult) {
        let done = self.sync.then(|| {
            let (sender, receiver) = std_mpsc::channel();
            (sender, receiver)
        });
        let (done_sender, done_receiver) = match done {
            Some((sender, receiver)) => (Some(sender), Some(receiver)),
            None => (None, None),
        };

        if self
            .sender
            .send(BuildEventProtocolMessage::Finish {
                result: Box::new(result),
                done: done_sender,
            })
            .is_err()
        {
            tracing::warn!("BEP uploader stopped before command completion");
            return;
        }

        let Some(done_receiver) = done_receiver else {
            return;
        };

        let wait = match self.timeout {
            Some(timeout) if !timeout.is_zero() => done_receiver
                .recv_timeout(timeout)
                .map_err(|error| BepError::Upload(error.to_string()).into()),
            _ => done_receiver
                .recv()
                .map_err(|error| BepError::Upload(error.to_string()).into()),
        };
        match wait {
            Ok(Ok(())) => {}
            Ok(Err(error)) | Err(error) => {
                tracing::warn!("BEP upload did not complete before command return: {error:#}");
            }
        }
    }
}

enum BuildEventProtocolMessage {
    Buck(Box<BuckEvent>),
    Finish {
        result: Box<CommandResult>,
        done: Option<std_mpsc::Sender<bz_error::Result<()>>>,
    },
}

async fn run_build_event_protocol_uploader(
    config: BuildEventProtocolConfig,
    mut receiver: mpsc::UnboundedReceiver<BuildEventProtocolMessage>,
) {
    let mut uploader = BuildEventProtocolUploader::new(config);
    while let Some(message) = receiver.recv().await {
        match message {
            BuildEventProtocolMessage::Buck(event) => {
                uploader.handle_buck_event(*event).await;
            }
            BuildEventProtocolMessage::Finish { result, done } => {
                let result = uploader.finish(&result).await;
                if let Some(done) = done {
                    let _ignored = done.send(result);
                } else if let Err(error) = result {
                    tracing::warn!("BEP upload failed after command return: {error:#}");
                }
                return;
            }
        }
    }
}

struct BuildEventProtocolUploader {
    sender: Option<mpsc::UnboundedSender<publish_build_event::PublishBuildToolEventStreamRequest>>,
    receiver:
        Option<mpsc::UnboundedReceiver<publish_build_event::PublishBuildToolEventStreamRequest>>,
    terminal_progress: TerminalProgressCoalescer,
    sequence_number: i64,
    progress_count: i32,
    config: BuildEventProtocolConfig,
    error_seen: bool,
    workspace_status_sent: bool,
    finished_sent: bool,
    profile_writer: Option<ChromeTraceProfileWriter>,
}

impl BuildEventProtocolUploader {
    fn new(config: BuildEventProtocolConfig) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let profile_writer = Some(ChromeTraceProfileWriter::new(
            config.command_name.clone(),
            config.trace_id.clone(),
            config.start_time,
            config.argv.clone(),
            config.workspace_directory.clone(),
        ));
        let mut this = Self {
            sender: Some(sender),
            receiver: Some(receiver),
            terminal_progress: TerminalProgressCoalescer::new(),
            sequence_number: 0,
            progress_count: 0,
            config,
            error_seen: false,
            workspace_status_sent: false,
            finished_sent: false,
            profile_writer,
        };

        this.send_bazel_event(this.started_event());
        this.send_bazel_event(this.options_parsed_event());
        this.send_workspace_status();
        this.queue_results_url_progress();
        this.flush_terminal_progress_now();
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
            progress_id(0),
            options_parsed_id(),
            workspace_status_id(),
            build_finished_id(),
            build_tool_logs_id(),
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
                    server_pid: std::process::id() as i64,
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

    fn next_progress_event(
        &mut self,
        stdout: String,
        stderr: String,
    ) -> build_event_stream::BuildEvent {
        let current_progress = self.progress_count;
        self.progress_count += 1;
        build_event_stream::BuildEvent {
            id: Some(progress_id(current_progress)),
            children: vec![progress_id(self.progress_count)],
            last_message: false,
            payload: Some(build_event_stream::build_event::Payload::Progress(
                build_event_stream::Progress { stdout, stderr },
            )),
        }
    }

    fn final_progress_event(&self) -> build_event_stream::BuildEvent {
        build_event_stream::BuildEvent {
            id: Some(progress_id(self.progress_count)),
            children: Vec::new(),
            last_message: false,
            payload: Some(build_event_stream::build_event::Payload::Progress(
                build_event_stream::Progress {
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )),
        }
    }

    fn send_progress_text(&mut self, stdout: Option<String>, stderr: Option<String>) {
        let stdout = stdout.unwrap_or_default();
        let stderr = stderr.unwrap_or_default();
        if stdout.is_empty() && stderr.is_empty() {
            return;
        }
        let event = self.next_progress_event(stdout, stderr);
        self.send_bazel_event(event);
    }

    fn send_terminal_progress_chunk(&mut self, bytes: Vec<u8>) {
        self.send_progress_text(None, Some(terminal_output_text(&bytes)));
    }

    fn maybe_flush_terminal_progress(&mut self) {
        let now = Instant::now();
        if let Some(bytes) = self
            .terminal_progress
            .should_flush(now)
            .then(|| self.terminal_progress.take_next_chunk(now))
            .flatten()
        {
            self.send_terminal_progress_chunk(bytes);
        }
    }

    fn flush_terminal_progress_now(&mut self) {
        let now = Instant::now();
        while let Some(bytes) = self.terminal_progress.take_next_chunk(now) {
            self.send_terminal_progress_chunk(bytes);
        }
    }

    fn queue_stderr_terminal_progress(&mut self, message: impl AsRef<str>) {
        let mut message = message.as_ref().to_owned();
        if !message.ends_with('\n') {
            message.push('\n');
        }
        self.terminal_progress.push(message.as_bytes());
    }

    fn queue_results_url_progress(&mut self) {
        if let Some(results_url) = &self.config.results_url {
            self.queue_stderr_terminal_progress(bes_results_url_message(
                results_url,
                &self.config.invocation_id,
                false,
            ));
        }
    }

    fn send_finished(&mut self) {
        if self.finished_sent {
            return;
        }
        self.finished_sent = true;
        self.send_workspace_status();
        self.send_bazel_event(self.final_progress_event());

        let (name, code) = if self.error_seen {
            ("UNKNOWN_FAILURE".to_owned(), 1)
        } else {
            ("SUCCESS".to_owned(), 0)
        };
        let finish_time = SystemTime::now();

        self.send_bazel_event(build_event_stream::BuildEvent {
            id: Some(build_finished_id()),
            children: Vec::new(),
            last_message: false,
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

    async fn build_tool_logs(&mut self) -> Vec<build_event_stream::File> {
        let upload = match self.config.timeout {
            Some(timeout) if !timeout.is_zero() => {
                match tokio::time::timeout(timeout, self.maybe_upload_timing_profile()).await {
                    Ok(result) => result,
                    Err(_) => {
                        Err(BepError::ProfileUpload(format!("timed out after {timeout:?}")).into())
                    }
                }
            }
            _ => self.maybe_upload_timing_profile().await,
        };

        match upload {
            Ok(Some(file)) => vec![file],
            Ok(None) => Vec::new(),
            Err(error) => {
                tracing::warn!("Failed to upload BEP timing profile: {error:#}");
                Vec::new()
            }
        }
    }

    async fn maybe_upload_timing_profile(
        &mut self,
    ) -> bz_error::Result<Option<build_event_stream::File>> {
        let Some(profile_writer) = self.profile_writer.take() else {
            return Ok(None);
        };

        let profile_path = profile_writer.path().to_owned();
        let profile_path = match profile_writer.finish().await {
            Ok(path) => path,
            Err(error) => {
                let _ignored = tokio::fs::remove_file(&profile_path).await;
                return Err(error);
            }
        };
        let upload_result = upload_timing_profile_to_backend(
            &self.config.backend,
            &self.config.headers,
            &self.config.project_id,
            &self.config.invocation_id,
            &profile_path,
        )
        .await;
        let _ignored = tokio::fs::remove_file(&profile_path).await;
        upload_result.map(Some)
    }

    fn send_build_tool_logs(&mut self, logs: Vec<build_event_stream::File>) {
        self.send_bazel_event(build_tool_logs_event(logs));
    }

    async fn handle_buck_event(&mut self, event: BuckEvent) {
        let event = Arc::new(event);
        if let Some(mut profile_writer) = self.profile_writer.take() {
            let events = [event.clone()];
            match profile_writer.handle_events(&events).await {
                Ok(()) => self.profile_writer = Some(profile_writer),
                Err(error) => {
                    tracing::warn!("Failed to write BEP timing profile: {error:#}");
                    profile_writer.discard().await;
                }
            }
        }
        self.handle_progress_event(&event);
        self.maybe_flush_terminal_progress();
    }

    fn handle_progress_event(&mut self, event: &BuckEvent) {
        let bz_data::buck_event::Data::Instant(instant) = event.data() else {
            return;
        };
        match instant.data.as_ref() {
            Some(bz_data::instant_event::Data::ConsoleMessage(message)) => {
                self.queue_stderr_terminal_progress(&message.message);
            }
            Some(bz_data::instant_event::Data::ConsoleWarning(message)) => {
                self.queue_stderr_terminal_progress(&message.message);
            }
            Some(bz_data::instant_event::Data::StreamingOutput(message)) => {
                self.send_progress_text(Some(message.message.clone()), None);
            }
            _ => {}
        }
    }

    fn handle_command_result(&mut self, result: &CommandResult) {
        match result.result.as_ref() {
            Some(command_result::Result::BuildResponse(response)) => {
                self.emit_build_targets(&response.build_targets, &response.project_root);
                if !response.errors.is_empty() {
                    self.error_seen = true;
                }
            }
            Some(command_result::Result::TestResponse(response)) => {
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
        self.maybe_flush_terminal_progress();
    }

    async fn finish(mut self, result: &CommandResult) -> bz_error::Result<()> {
        self.handle_command_result(result);
        self.queue_results_url_progress();
        self.flush_terminal_progress_now();
        self.send_finished();
        let build_tool_logs = self.build_tool_logs().await;
        self.flush_terminal_progress_now();
        self.send_build_tool_logs(build_tool_logs);
        self.send_component_stream_finished();
        self.sender.take();

        let Some(receiver) = self.receiver.take() else {
            return Ok(());
        };

        let upload = upload_build_events(self.config.clone(), receiver);

        let summary =
            match self.config.timeout {
                Some(timeout) if !timeout.is_zero() => tokio::time::timeout(timeout, upload)
                    .await
                    .map_err(|_| BepError::Upload(format!("timed out after {timeout:?}")))??,
                _ => upload.await?,
            };

        if let Some(results_url) = &self.config.results_url {
            tracing::info!(
                "Uploaded {} BEP events to {} ({}; last ack: {:?})",
                summary.acked_events,
                self.config.backend,
                bes_invocation_url(results_url, &self.config.invocation_id),
                summary.last_ack
            );
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
) -> bz_error::Result<UploadSummary> {
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

async fn upload_timing_profile(
    channel: tonic::transport::Channel,
    authority: &str,
    headers: &[String],
    instance_name: &str,
    invocation_id: &str,
    profile_path: &Path,
) -> bz_error::Result<build_event_stream::File> {
    let bytes = tokio::fs::read(profile_path)
        .await
        .map_err(|e| BepError::ProfileUpload(e.to_string()))?;
    let size = bytes.len() as i64;
    let digest = hex::encode(Sha256::digest(&bytes));
    let resource_name =
        bytestream_upload_resource_name(instance_name, invocation_id, &digest, size);
    let uri = bytestream_download_uri(authority, instance_name, &digest, size);

    let mut client = ByteStreamClient::new(channel);
    let requests = bytestream_write_requests(resource_name, bytes);
    let mut request = tonic::Request::new(tokio_stream::iter(requests));
    add_headers(request.metadata_mut(), headers)?;
    add_bazel_request_metadata(request.metadata_mut(), invocation_id);
    let response = client
        .write(request)
        .await
        .map_err(|e| BepError::ProfileUpload(e.to_string()))?
        .into_inner();
    if response.committed_size != size {
        return Err(BepError::ProfileUpload(format!(
            "uploaded {} bytes, expected {}",
            response.committed_size, size
        ))
        .into());
    }

    Ok(build_event_stream::File {
        name: PROFILE_NAME.to_owned(),
        file: Some(build_event_stream::file::File::Uri(uri)),
        path_prefix: Vec::new(),
        digest,
        length: size,
    })
}

async fn upload_timing_profile_to_backend(
    backend: &str,
    headers: &[String],
    instance_name: &str,
    invocation_id: &str,
    profile_path: &Path,
) -> bz_error::Result<build_event_stream::File> {
    let backend = BesBackend::parse(backend)?;
    let authority = backend.authority()?;
    let mut endpoint = Endpoint::from_shared(backend.uri.clone())
        .map_err(|e| BepError::InvalidBackend(format!("{} ({e})", backend.uri)))?;
    if backend.tls {
        endpoint = endpoint
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|e| BepError::ProfileUpload(e.to_string()))?;
    }

    upload_timing_profile(
        endpoint.connect_lazy(),
        &authority,
        headers,
        instance_name,
        invocation_id,
        profile_path,
    )
    .await
}

fn bytestream_upload_resource_name(
    instance_name: &str,
    invocation_id: &str,
    digest: &str,
    size: i64,
) -> String {
    let upload = format!("uploads/{invocation_id}/blobs/{digest}/{size}");
    if instance_name.is_empty() {
        upload
    } else {
        format!("{instance_name}/{upload}")
    }
}

fn bytestream_download_uri(
    authority: &str,
    instance_name: &str,
    digest: &str,
    size: i64,
) -> String {
    if instance_name.is_empty() {
        format!("bytestream://{authority}/blobs/{digest}/{size}")
    } else {
        format!("bytestream://{authority}/{instance_name}/blobs/{digest}/{size}")
    }
}

fn bes_upload_request_metadata(invocation_id: &str) -> RequestMetadata {
    RequestMetadata {
        tool_details: Some(ToolDetails {
            tool_name: "buck2".to_owned(),
            tool_version: bz_build_info::revision()
                .map(|revision| revision.to_owned())
                .unwrap_or_default(),
        }),
        action_id: "bes-upload".to_owned(),
        tool_invocation_id: invocation_id.to_owned(),
        correlated_invocations_id: invocation_id.to_owned(),
        action_mnemonic: String::new(),
        target_id: String::new(),
        configuration_id: String::new(),
    }
}

fn add_bazel_request_metadata(metadata: &mut tonic::metadata::MetadataMap, invocation_id: &str) {
    let mut encoded = Vec::new();
    bes_upload_request_metadata(invocation_id)
        .encode(&mut encoded)
        .expect("Encoding into a Vec cannot fail");
    metadata.insert_bin(
        BAZEL_REQUEST_METADATA_HEADER,
        MetadataValue::from_bytes(&encoded),
    );
}

fn bytestream_write_requests(resource_name: String, bytes: Vec<u8>) -> Vec<WriteRequest> {
    if bytes.is_empty() {
        return vec![WriteRequest {
            resource_name,
            write_offset: 0,
            finish_write: true,
            data: Vec::new(),
        }];
    }

    let mut offset = 0;
    let chunks = bytes.chunks(BYTESTREAM_UPLOAD_CHUNK_SIZE);
    let chunk_count = chunks.len();
    chunks
        .enumerate()
        .map(|(idx, chunk)| {
            let write_offset = offset;
            offset += chunk.len() as i64;
            WriteRequest {
                resource_name: resource_name.clone(),
                write_offset,
                finish_write: idx + 1 == chunk_count,
                data: chunk.to_vec(),
            }
        })
        .collect()
}

fn add_headers(
    metadata: &mut tonic::metadata::MetadataMap,
    headers: &[String],
) -> bz_error::Result<()> {
    for header in headers {
        let (key, value) = parse_header(header)?;
        metadata.append(key, value);
    }
    Ok(())
}

fn validate_headers(headers: &[String]) -> bz_error::Result<()> {
    for header in headers {
        let _ignored = parse_header(header)?;
    }
    Ok(())
}

fn parse_header(
    header: &str,
) -> bz_error::Result<(
    MetadataKey<tonic::metadata::Ascii>,
    MetadataValue<tonic::metadata::Ascii>,
)> {
    let (name, value) = header
        .split_once('=')
        .ok_or_else(|| BepError::InvalidHeader(header.to_owned()))?;
    let key = MetadataKey::from_bytes(name.trim().as_bytes())
        .map_err(|_| BepError::InvalidHeader(header.to_owned()))?;
    let value = MetadataValue::try_from(value.trim())
        .map_err(|_| BepError::InvalidHeader(header.to_owned()))?;
    Ok((key, value))
}

struct BesBackend {
    uri: String,
    tls: bool,
}

impl BesBackend {
    fn parse(value: &str) -> bz_error::Result<Self> {
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

    fn authority(&self) -> bz_error::Result<String> {
        let uri: tonic::codegen::http::Uri = self
            .uri
            .parse()
            .map_err(|e| BepError::InvalidBackend(format!("{} ({e})", self.uri)))?;
        uri.authority()
            .map(|authority| authority.as_str().to_owned())
            .ok_or_else(|| BepError::InvalidBackend(self.uri.clone()).into())
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
    bz_build_info::revision()
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

fn build_tool_logs_id() -> build_event_stream::BuildEventId {
    build_event_stream::BuildEventId {
        id: Some(build_event_stream::build_event_id::Id::BuildToolLogs(
            build_event_stream::build_event_id::BuildToolLogsId {},
        )),
    }
}

fn build_tool_logs_event(logs: Vec<build_event_stream::File>) -> build_event_stream::BuildEvent {
    build_event_stream::BuildEvent {
        id: Some(build_tool_logs_id()),
        children: Vec::new(),
        last_message: true,
        payload: Some(build_event_stream::build_event::Payload::BuildToolLogs(
            build_event_stream::BuildToolLogs { log: logs },
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
        #[prost(oneof = "build_event_id::Id", tags = "2, 3, 4, 5, 9, 12, 14, 16, 20")]
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

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub(crate) struct BuildToolLogsId {}

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
            #[prost(message, tag = "20")]
            BuildToolLogs(BuildToolLogsId),
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
    pub(crate) struct BuildToolLogs {
        #[prost(message, repeated, tag = "1")]
        pub(crate) log: Vec<File>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub(crate) struct BuildEvent {
        #[prost(message, optional, tag = "1")]
        pub(crate) id: Option<BuildEventId>,
        #[prost(message, repeated, tag = "2")]
        pub(crate) children: Vec<BuildEventId>,
        #[prost(bool, tag = "20")]
        pub(crate) last_message: bool,
        #[prost(
            oneof = "build_event::Payload",
            tags = "3, 5, 6, 8, 13, 14, 16, 18, 23"
        )]
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
            #[prost(message, tag = "23")]
            BuildToolLogs(BuildToolLogs),
        }
    }
}
