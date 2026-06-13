/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;

use bz_common::convert::ProstDurationExt;
use bz_error::internal_error;
use bz_event_observer::display;
use bz_event_observer::display::TargetDisplayOptions;
use bz_events::BuckEvent;
use bz_events::span::SpanId;
use bz_wrapper_common::invocation_id::TraceId;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

const PROFILE_QUEUE_BOUND: usize = 1_000_000;
const CRITICAL_PATH_THREAD_ID: u64 = 0;
const BUILD_GRAPH_CRITICAL_PATH_THREAD_ID: u64 = 1;
const SLOWEST_PATH_THREAD_ID: u64 = 2;
const WORKER_THREAD_ID_START: u64 = 100;
const FALLBACK_THREAD_ID_START: u64 = 1_000_000_000;
const OTHER_THREAD_ID_START: u64 = 1_000_000;
const WORKER_THREAD_NAME_WIDTH: usize = 2;

pub struct ChromeTraceProfileWriter {
    path: PathBuf,
    sender: mpsc::SyncSender<TraceMessage>,
    join: Option<thread::JoinHandle<Result<(), String>>>,
    profile_start: SystemTime,
    command_line: String,
    next_thread_id: u64,
    fallback_thread_ids: HashMap<thread::ThreadId, u64>,
    trace_thread_ids: HashMap<u64, u64>,
    next_runtime_worker_display_id: u64,
    open_tasks: HashMap<SpanId, OpenTask>,
    span_thread_ids: HashMap<SpanId, u64>,
    previous_counter_values: HashMap<String, TimestampAndAmount>,
}

struct OpenTask {
    name: String,
    category: &'static str,
    start: SystemTime,
    thread_id: u64,
    args: Value,
}

enum TraceMessage {
    Event(Value),
    Finish,
}

struct TimestampAndAmount {
    timestamp: SystemTime,
    amount: u64,
}

struct ThreadDisplayMetadata {
    trace_thread_id: u64,
    name: String,
    sort_index: u64,
}

impl ChromeTraceProfileWriter {
    pub fn new(
        command_name: String,
        invocation_id: TraceId,
        profile_start: SystemTime,
        argv: Vec<String>,
        workspace_directory: String,
    ) -> Self {
        let path = temporary_profile_path(&invocation_id);
        let command_line = shell_join(&argv);
        let other_data = json!({
            "bazel_version": "bz",
            "build_id": invocation_id.to_string(),
            "command": command_name,
            "output_base": workspace_directory,
            "date": millis_since_epoch(profile_start).to_string(),
            "profile_start_ts": millis_since_epoch(profile_start),
        });
        let (sender, receiver) = mpsc::sync_channel(PROFILE_QUEUE_BOUND);
        let join_path = path.clone();
        let join = thread::Builder::new()
            .name("profile-writer-thread".to_owned())
            .spawn(move || {
                ProfileFileWriter::new(join_path, profile_start, other_data).run(receiver)
            })
            .ok();

        Self {
            path,
            sender,
            join,
            profile_start,
            command_line,
            next_thread_id: FALLBACK_THREAD_ID_START,
            fallback_thread_ids: HashMap::new(),
            trace_thread_ids: HashMap::new(),
            next_runtime_worker_display_id: 1,
            open_tasks: HashMap::new(),
            span_thread_ids: HashMap::new(),
            previous_counter_values: HashMap::new(),
        }
    }

    pub async fn handle_events(&mut self, events: &[Arc<BuckEvent>]) -> bz_error::Result<()> {
        for event in events {
            let thread_id = self.thread_id_for_event(event)?;
            self.handle_event(event, thread_id)?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn discard(mut self) {
        let _ignored = self.finish_writer();
        let _ignored = tokio::fs::remove_file(&self.path).await;
    }

    pub async fn finish(mut self) -> bz_error::Result<PathBuf> {
        let now = SystemTime::now();
        let mut open_tasks = self.open_tasks.drain().collect::<Vec<_>>();
        open_tasks.sort_by_key(|(span_id, _)| u64::from(*span_id));
        for (span_id, open) in open_tasks {
            let duration = now.duration_since(open.start).unwrap_or(Duration::ZERO);
            if !duration.is_zero() {
                self.send_event(complete_event(
                    self.profile_start,
                    open,
                    duration,
                    json!({"span_id": u64::from(span_id), "unfinished": true}),
                ))?;
            }
        }

        self.finish_writer()?;
        Ok(self.path)
    }

    fn thread_id_for_event(&mut self, event: &BuckEvent) -> bz_error::Result<u64> {
        if let Some(thread_id) = event.thread_id() {
            let thread_name = event.thread_name().unwrap_or("unnamed thread").to_owned();
            return self.trace_thread_id(thread_id, thread_name);
        }

        self.current_thread_id()
    }

    fn current_thread_id(&mut self) -> bz_error::Result<u64> {
        let thread = thread::current();
        let raw_id = thread.id();
        if let Some(id) = self.fallback_thread_ids.get(&raw_id) {
            let id = *id;
            let name = thread.name().unwrap_or("unnamed thread").to_owned();
            return self.trace_thread_id(id, name);
        }

        let id = self.next_thread_id;
        self.next_thread_id += 1;
        self.fallback_thread_ids.insert(raw_id, id);
        let name = thread.name().unwrap_or("unnamed thread").to_owned();
        self.trace_thread_id(id, name)
    }

    fn trace_thread_id(&mut self, raw_id: u64, name: String) -> bz_error::Result<u64> {
        if let Some(trace_id) = self.trace_thread_ids.get(&raw_id) {
            return Ok(*trace_id);
        }

        let metadata = self.display_thread_metadata(raw_id, &name);
        self.trace_thread_ids
            .insert(raw_id, metadata.trace_thread_id);
        self.send_event(thread_name_event(metadata.trace_thread_id, metadata.name))?;
        self.send_event(thread_sort_index_event(
            metadata.trace_thread_id,
            metadata.sort_index,
        ))?;
        Ok(metadata.trace_thread_id)
    }

    fn display_thread_metadata(&mut self, tid: u64, name: &str) -> ThreadDisplayMetadata {
        if name == "bz-worker" || name == "buck2-rt" {
            let worker_id = self.next_runtime_worker_display_id;
            self.next_runtime_worker_display_id += 1;
            let trace_thread_id = worker_thread_id(worker_id);
            ThreadDisplayMetadata {
                trace_thread_id,
                name: worker_thread_name(worker_id),
                sort_index: trace_thread_id,
            }
        } else if name.is_empty() || name == "unnamed thread" {
            let trace_thread_id = other_thread_id(tid);
            ThreadDisplayMetadata {
                trace_thread_id,
                name: format!("thread #{tid}"),
                sort_index: trace_thread_id,
            }
        } else {
            let trace_thread_id = other_thread_id(tid);
            ThreadDisplayMetadata {
                trace_thread_id,
                name: format!("{name} #{tid}"),
                sort_index: trace_thread_id,
            }
        }
    }

    fn handle_event(&mut self, event: &BuckEvent, thread_id: u64) -> bz_error::Result<()> {
        match event.data() {
            bz_data::buck_event::Data::SpanStart(start) => {
                let Some(span_id) = event.span_id() else {
                    return Ok(());
                };
                self.span_thread_ids.insert(span_id, thread_id);
                let task = task_from_span_start(
                    self.profile_start,
                    event.timestamp(),
                    thread_id,
                    span_id,
                    &self.command_line,
                    event,
                    start,
                );
                self.open_tasks.insert(span_id, task);
            }
            bz_data::buck_event::Data::SpanEnd(end) => {
                let Some(span_id) = event.span_id() else {
                    return Ok(());
                };
                let Some(open) = self.open_tasks.remove(&span_id) else {
                    return Ok(());
                };
                let duration = end
                    .duration
                    .as_ref()
                    .and_then(|duration| duration.try_into_duration().ok())
                    .unwrap_or_else(|| {
                        event
                            .timestamp()
                            .duration_since(open.start)
                            .unwrap_or(Duration::ZERO)
                    });
                if !duration.is_zero() {
                    self.send_event(complete_event(
                        self.profile_start,
                        open,
                        duration,
                        json!({"span_id": u64::from(span_id)}),
                    ))?;
                }
                self.handle_span_end_instant(end, event)?;
            }
            bz_data::buck_event::Data::Instant(instant) => {
                self.handle_instant(instant, event, thread_id)?;
            }
            bz_data::buck_event::Data::Record(_) => {}
        }
        Ok(())
    }

    fn handle_span_end_instant(
        &self,
        end: &bz_data::SpanEndEvent,
        event: &BuckEvent,
    ) -> bz_error::Result<()> {
        if let Some(bz_data::span_end_event::Data::Materialization(materialization)) =
            end.data.as_ref()
            && !materialization.success
        {
            self.send_event(instant_event(
                self.profile_start,
                "materialization_failure",
                event.timestamp(),
                CRITICAL_PATH_THREAD_ID,
                json!({
                    "file_count": materialization.file_count,
                    "total_bytes": materialization.total_bytes,
                    "path": materialization.path,
                    "action_digest": materialization.action_digest.as_ref().map(ToString::to_string),
                    "success": materialization.success,
                    "error": materialization.error,
                }),
            ))?;
        }
        Ok(())
    }

    fn handle_instant(
        &mut self,
        instant: &bz_data::InstantEvent,
        event: &BuckEvent,
        thread_id: u64,
    ) -> bz_error::Result<()> {
        match instant.data.as_ref() {
            Some(bz_data::instant_event::Data::Snapshot(snapshot)) => {
                self.write_snapshot_counters(event.timestamp(), snapshot)?;
            }
            Some(bz_data::instant_event::Data::BuildGraphInfo(info)) => {
                self.write_critical_path(
                    CRITICAL_PATH_THREAD_ID,
                    "critical_path",
                    "critical path component",
                    &info.critical_path2,
                )?;
                self.write_critical_path(
                    BUILD_GRAPH_CRITICAL_PATH_THREAD_ID,
                    "build_graph_critical_path",
                    "build graph critical path component",
                    &info.build_graph_critical_path,
                )?;
                self.write_critical_path(
                    SLOWEST_PATH_THREAD_ID,
                    "slowest_path",
                    "slowest path component",
                    &info.slowest_path,
                )?;
            }
            Some(bz_data::instant_event::Data::ResourceControlEvent(events)) => {
                self.send_event(counter_event(
                    self.profile_start,
                    "snapshot_counters",
                    event.timestamp(),
                    thread_id,
                    json!({"allprocs_memory_pressure": events.allprocs_memory_pressure}),
                ))?;
            }
            Some(bz_data::instant_event::Data::CommandPreempted(_)) => {
                self.send_event(instant_event(
                    self.profile_start,
                    "command_preempted",
                    event.timestamp(),
                    thread_id,
                    json!({}),
                ))?;
            }
            _ => {}
        }
        Ok(())
    }

    fn write_critical_path(
        &self,
        thread_id: u64,
        path_name: &'static str,
        category: &'static str,
        critical_path: &[bz_data::CriticalPathEntry2],
    ) -> bz_error::Result<()> {
        for entry in critical_path {
            let Some(event) = critical_path_component_event(
                self.profile_start,
                entry,
                self.original_thread_id_for_critical_path_entry(entry),
                thread_id,
                path_name,
                category,
            ) else {
                continue;
            };
            self.send_event(event)?;
        }
        Ok(())
    }

    fn original_thread_id_for_critical_path_entry(
        &self,
        entry: &bz_data::CriticalPathEntry2,
    ) -> Option<u64> {
        entry
            .span_ids
            .iter()
            .filter_map(|span_id| SpanId::from_u64_opt(*span_id))
            .find_map(|span_id| self.span_thread_ids.get(&span_id).copied())
    }

    fn write_snapshot_counters(
        &mut self,
        timestamp: SystemTime,
        snapshot: &bz_data::Snapshot,
    ) -> bz_error::Result<()> {
        let mut process_memory = Map::new();
        process_memory.insert(
            "max_rss_gigabyte".to_owned(),
            json!(snapshot.bz_max_rss as f64 / 1_000_000_000.0),
        );
        if let Some(malloc_bytes_active) = snapshot.malloc_bytes_active {
            process_memory.insert(
                "malloc_active_gigabyte".to_owned(),
                json!(malloc_bytes_active as f64 / 1_000_000_000.0),
            );
        }
        self.send_event(counter_event(
            self.profile_start,
            "process_memory",
            timestamp,
            CRITICAL_PATH_THREAD_ID,
            Value::Object(process_memory),
        ))?;
        self.send_event(counter_event(
            self.profile_start,
            "snapshot_counters",
            timestamp,
            CRITICAL_PATH_THREAD_ID,
            json!({
                "deferred_materializer_queue_size": snapshot.deferred_materializer_queue_size,
                "blocking_executor_io_queue_size": snapshot.blocking_executor_io_queue_size,
                "tokio_blocking_queue_depth": snapshot.tokio_blocking_queue_depth,
                "tokio_num_blocking_threads": snapshot.tokio_num_blocking_threads,
                "tokio_num_idle_blocking_threads": snapshot.tokio_num_idle_blocking_threads,
            }),
        ))?;
        let mut rate_counters = Map::new();
        self.insert_average_rate(
            &mut rate_counters,
            timestamp,
            "average_user_cpu_in_usecs_per_s",
            snapshot.bz_user_cpu_us,
        )?;
        self.insert_average_rate(
            &mut rate_counters,
            timestamp,
            "average_system_cpu_in_usecs_per_s",
            snapshot.bz_system_cpu_us,
        )?;
        if let Some(cpu_usage_system) = snapshot.host_cpu_usage_system_ms {
            self.insert_average_rate(
                &mut rate_counters,
                timestamp,
                "host_cpu_usage_system_in_msecs_per_s",
                cpu_usage_system,
            )?;
        }
        if let Some(cpu_usage_user) = snapshot.host_cpu_usage_user_ms {
            self.insert_average_rate(
                &mut rate_counters,
                timestamp,
                "host_cpu_usage_user_in_msecs_per_s",
                cpu_usage_user,
            )?;
        }
        for (nic, stats) in &snapshot.network_interface_stats {
            self.insert_average_rate(
                &mut rate_counters,
                timestamp,
                format!("{nic}_send_bytes"),
                stats.tx_bytes,
            )?;
            self.insert_average_rate(
                &mut rate_counters,
                timestamp,
                format!("{nic}_receive_bytes"),
                stats.rx_bytes,
            )?;
        }
        self.insert_average_rate(
            &mut rate_counters,
            timestamp,
            "re_upload_bytes",
            snapshot.re_upload_bytes,
        )?;
        self.insert_average_rate(
            &mut rate_counters,
            timestamp,
            "re_download_bytes",
            snapshot.re_download_bytes,
        )?;
        self.insert_average_rate(
            &mut rate_counters,
            timestamp,
            "http_download_bytes",
            snapshot.http_download_bytes,
        )?;
        if !rate_counters.is_empty() {
            self.send_event(counter_event(
                self.profile_start,
                "rate_of_change_counters",
                timestamp,
                CRITICAL_PATH_THREAD_ID,
                Value::Object(rate_counters),
            ))?;
        }
        Ok(())
    }

    fn insert_average_rate(
        &mut self,
        output: &mut Map<String, Value>,
        timestamp: SystemTime,
        key: impl Into<String>,
        amount: u64,
    ) -> bz_error::Result<()> {
        let key = key.into();
        if let Some(rate) = self.average_rate_per_second(timestamp, &key, amount)? {
            output.insert(key, json!(rate));
        }
        Ok(())
    }

    fn average_rate_per_second(
        &mut self,
        timestamp: SystemTime,
        key: &str,
        amount: u64,
    ) -> bz_error::Result<Option<u64>> {
        let rate = match self.previous_counter_values.get(key) {
            Some(previous) => {
                let Ok(duration) = timestamp.duration_since(previous.timestamp) else {
                    self.previous_counter_values
                        .insert(key.to_owned(), TimestampAndAmount { timestamp, amount });
                    return Ok(None);
                };
                match (duration.as_secs_f64(), amount.checked_sub(previous.amount)) {
                    (seconds, Some(delta)) if seconds > 0.0 => {
                        Some((delta as f64 / seconds) as u64)
                    }
                    _ => None,
                }
            }
            None => None,
        };
        self.previous_counter_values
            .insert(key.to_owned(), TimestampAndAmount { timestamp, amount });
        Ok(rate)
    }

    fn send_event(&self, event: Value) -> bz_error::Result<()> {
        self.sender
            .send(TraceMessage::Event(event))
            .map_err(|_| internal_error!("profile writer stopped"))
    }

    fn finish_writer(&mut self) -> bz_error::Result<()> {
        let _ignored = self.sender.send(TraceMessage::Finish);
        if let Some(join) = self.join.take() {
            match join.join() {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(internal_error!("profile writer failed: {}", error)),
                Err(_) => Err(internal_error!("profile writer panicked")),
            }
        } else {
            Ok(())
        }
    }
}

struct ProfileFileWriter {
    path: PathBuf,
    profile_start: SystemTime,
    other_data: Value,
    event_count: usize,
}

impl ProfileFileWriter {
    fn new(path: PathBuf, profile_start: SystemTime, other_data: Value) -> Self {
        Self {
            path,
            profile_start,
            other_data,
            event_count: 0,
        }
    }

    fn run(mut self, receiver: mpsc::Receiver<TraceMessage>) -> Result<(), String> {
        self.run_inner(receiver).map_err(|error| error.to_string())
    }

    fn run_inner(&mut self, receiver: mpsc::Receiver<TraceMessage>) -> std::io::Result<()> {
        let file = std::fs::File::create(&self.path)?;
        let mut writer = GzEncoder::new(BufWriter::new(file), Compression::default());

        Write::write_all(&mut writer, b"{\"otherData\":")?;
        serde_json::to_writer(&mut writer, &self.other_data)?;
        Write::write_all(&mut writer, b",\"traceEvents\":[\n")?;
        self.write_event(&mut writer, process_name_event())?;
        self.write_summary_thread_metadata(&mut writer, CRITICAL_PATH_THREAD_ID, "Critical Path")?;
        self.write_summary_thread_metadata(
            &mut writer,
            BUILD_GRAPH_CRITICAL_PATH_THREAD_ID,
            "Build Graph Critical Path",
        )?;
        self.write_summary_thread_metadata(&mut writer, SLOWEST_PATH_THREAD_ID, "Slowest Path")?;

        while let Ok(message) = receiver.recv() {
            match message {
                TraceMessage::Event(event) => self.write_event(&mut writer, event)?,
                TraceMessage::Finish => break,
            }
        }

        Write::write_all(&mut writer, b"\n]}\n")?;
        let mut writer = writer.finish()?;
        writer.flush()?;
        Ok(())
    }

    fn write_summary_thread_metadata<W: Write>(
        &mut self,
        writer: &mut W,
        thread_id: u64,
        name: &str,
    ) -> std::io::Result<()> {
        self.write_event(&mut *writer, thread_name_event(thread_id, name.to_owned()))?;
        self.write_event(&mut *writer, thread_sort_index_event(thread_id, thread_id))?;
        self.write_event(
            writer,
            instant_event(
                self.profile_start,
                "profile_start",
                self.profile_start,
                thread_id,
                json!({}),
            ),
        )
    }

    fn write_event<W: Write>(&mut self, writer: &mut W, event: Value) -> std::io::Result<()> {
        if self.event_count > 0 {
            Write::write_all(writer, b",\n")?;
        }
        Write::write_all(writer, b"  ")?;
        serde_json::to_writer(writer, &event)?;
        self.event_count += 1;
        Ok(())
    }
}

fn task_from_span_start(
    profile_start: SystemTime,
    timestamp: SystemTime,
    thread_id: u64,
    span_id: SpanId,
    command_line: &str,
    event: &BuckEvent,
    start: &bz_data::SpanStartEvent,
) -> OpenTask {
    let (name, category) = span_start_name_and_category(command_line, event, start);
    OpenTask {
        name,
        category,
        start: timestamp,
        thread_id,
        args: json!({
            "span_id": u64::from(span_id),
            "start_ts": timestamp_micros(profile_start, timestamp),
        }),
    }
}

fn span_start_name_and_category(
    command_line: &str,
    event: &BuckEvent,
    start: &bz_data::SpanStartEvent,
) -> (String, &'static str) {
    let Some(data) = start.data.as_ref() else {
        return ("unknown span".to_owned(), "unknown");
    };

    let category = span_start_category(data);
    match start.data.as_ref() {
        Some(bz_data::span_start_event::Data::Command(_)) => (command_line.to_owned(), "command"),
        Some(bz_data::span_start_event::Data::CommandCritical(_)) => {
            ("command critical".to_owned(), "command")
        }
        Some(bz_data::span_start_event::Data::Analysis(analysis)) => {
            let name = analysis
                .target
                .as_ref()
                .and_then(|target| {
                    display::display_analysis_target(
                        target,
                        TargetDisplayOptions::for_chrome_trace(),
                    )
                    .ok()
                })
                .map(|target| format!("analysis {target}"))
                .unwrap_or_else(|| "analysis".to_owned());
            (name, "analysis")
        }
        Some(bz_data::span_start_event::Data::Load(load)) => {
            (format!("load {}", load.module_id), "loading")
        }
        Some(bz_data::span_start_event::Data::LoadPackage(load_package)) => {
            (format!("listing {}", load_package.path), "loading")
        }
        Some(bz_data::span_start_event::Data::ActionExecution(action)) => {
            let name = display::display_action_identity(
                action.key.as_ref(),
                action.name.as_ref(),
                TargetDisplayOptions::for_chrome_trace(),
            )
            .unwrap_or_else(|_| "action execution".to_owned());
            (name, "action")
        }
        Some(bz_data::span_start_event::Data::ExecutorStage(stage)) => {
            let name = stage
                .stage
                .as_ref()
                .and_then(display::display_executor_stage)
                .unwrap_or("executor stage");
            (name.to_owned(), "executor")
        }
        Some(bz_data::span_start_event::Data::ReUpload(_)) => ("re_upload".to_owned(), "remote"),
        Some(bz_data::span_start_event::Data::FinalMaterialization(_)) => {
            ("materialization".to_owned(), "materialization")
        }
        Some(bz_data::span_start_event::Data::FileWatcher(_)) => {
            ("file_watcher_sync".to_owned(), "file_watcher")
        }
        Some(bz_data::span_start_event::Data::DiceCriticalSection(_)) => {
            ("dice critical section".to_owned(), "dice")
        }
        Some(bz_data::span_start_event::Data::BxlEnsureArtifacts(_)) => {
            ("ensuring BXL artifacts".to_owned(), "bxl")
        }
        _ => (
            display::display_event(event, TargetDisplayOptions::for_chrome_trace())
                .unwrap_or_else(|_| span_start_fallback_name(data)),
            category,
        ),
    }
}

fn span_start_category(data: &bz_data::span_start_event::Data) -> &'static str {
    match data {
        bz_data::span_start_event::Data::Command(_)
        | bz_data::span_start_event::Data::CommandCritical(_) => "command",
        bz_data::span_start_event::Data::Analysis(_)
        | bz_data::span_start_event::Data::AnalysisResolveQueries(_)
        | bz_data::span_start_event::Data::AnalysisStage(_)
        | bz_data::span_start_event::Data::DynamicLambda(_) => "analysis",
        bz_data::span_start_event::Data::Load(_)
        | bz_data::span_start_event::Data::LoadPackage(_) => "loading",
        bz_data::span_start_event::Data::BzlmodRepo(_)
        | bz_data::span_start_event::Data::BzlmodModuleExtension(_) => "bzlmod",
        bz_data::span_start_event::Data::ActionExecution(_)
        | bz_data::span_start_event::Data::ActionErrorHandlerExecution(_)
        | bz_data::span_start_event::Data::MatchDepFiles(_)
        | bz_data::span_start_event::Data::CacheUpload(_)
        | bz_data::span_start_event::Data::DepFileUpload(_) => "action",
        bz_data::span_start_event::Data::ExecutorStage(_)
        | bz_data::span_start_event::Data::ReUpload(_) => "executor",
        bz_data::span_start_event::Data::FinalMaterialization(_)
        | bz_data::span_start_event::Data::Materialization(_)
        | bz_data::span_start_event::Data::DeferredPreparationStage(_)
        | bz_data::span_start_event::Data::CreateOutputSymlinks(_) => "materialization",
        bz_data::span_start_event::Data::FileWatcher(_) => "file_watcher",
        bz_data::span_start_event::Data::TestDiscovery(_)
        | bz_data::span_start_event::Data::TestStart(_) => "test",
        bz_data::span_start_event::Data::SharedTask(_)
        | bz_data::span_start_event::Data::DiceStateUpdate(_)
        | bz_data::span_start_event::Data::DiceStateUpdateStage(_)
        | bz_data::span_start_event::Data::DiceCriticalSection(_)
        | bz_data::span_start_event::Data::DiceBlockConcurrentCommand(_)
        | bz_data::span_start_event::Data::DiceSynchronizeSection(_)
        | bz_data::span_start_event::Data::DiceCleanup(_)
        | bz_data::span_start_event::Data::ExclusiveCommandWait(_) => "dice",
        bz_data::span_start_event::Data::BxlExecution(_)
        | bz_data::span_start_event::Data::BxlDiceInvocation(_)
        | bz_data::span_start_event::Data::BxlEnsureArtifacts(_) => "bxl",
        bz_data::span_start_event::Data::InstallEventInfo(_)
        | bz_data::span_start_event::Data::ConnectToInstaller(_) => "install",
        bz_data::span_start_event::Data::LocalResources(_)
        | bz_data::span_start_event::Data::ReleaseLocalResources(_) => "local_resources",
        bz_data::span_start_event::Data::CqueryUniverseBuild(_) => "query",
        bz_data::span_start_event::Data::ComputeDetailedAggregatedMetrics(_) => "metrics",
        bz_data::span_start_event::Data::Fake(_) => "fake",
    }
}

fn span_start_fallback_name(data: &bz_data::span_start_event::Data) -> String {
    match data {
        bz_data::span_start_event::Data::Command(_) => "command",
        bz_data::span_start_event::Data::ActionExecution(_) => "action execution",
        bz_data::span_start_event::Data::Analysis(_) => "analysis",
        bz_data::span_start_event::Data::AnalysisResolveQueries(_) => "analysis queries",
        bz_data::span_start_event::Data::Load(_) => "load",
        bz_data::span_start_event::Data::ExecutorStage(_) => "executor stage",
        bz_data::span_start_event::Data::TestDiscovery(_) => "test discovery",
        bz_data::span_start_event::Data::TestStart(_) => "test",
        bz_data::span_start_event::Data::FileWatcher(_) => "file watcher",
        bz_data::span_start_event::Data::FinalMaterialization(_) => "materialization",
        bz_data::span_start_event::Data::AnalysisStage(_) => "analysis stage",
        bz_data::span_start_event::Data::MatchDepFiles(_) => "dep files",
        bz_data::span_start_event::Data::LoadPackage(_) => "load package",
        bz_data::span_start_event::Data::SharedTask(_) => "shared task",
        bz_data::span_start_event::Data::CacheUpload(_) => "cache upload",
        bz_data::span_start_event::Data::CreateOutputSymlinks(_) => "create output symlinks",
        bz_data::span_start_event::Data::CommandCritical(_) => "command critical",
        bz_data::span_start_event::Data::InstallEventInfo(_) => "install event info",
        bz_data::span_start_event::Data::DiceStateUpdate(_) => "dice state update",
        bz_data::span_start_event::Data::Materialization(_) => "materialization",
        bz_data::span_start_event::Data::DiceCriticalSection(_) => "dice critical section",
        bz_data::span_start_event::Data::DiceBlockConcurrentCommand(_) => {
            "dice block concurrent command"
        }
        bz_data::span_start_event::Data::DiceSynchronizeSection(_) => "dice synchronize section",
        bz_data::span_start_event::Data::DiceCleanup(_) => "dice cleanup",
        bz_data::span_start_event::Data::ExclusiveCommandWait(_) => "exclusive command wait",
        bz_data::span_start_event::Data::DeferredPreparationStage(_) => "deferred preparation",
        bz_data::span_start_event::Data::DynamicLambda(_) => "dynamic lambda",
        bz_data::span_start_event::Data::BxlExecution(_) => "bxl execution",
        bz_data::span_start_event::Data::BxlDiceInvocation(_) => "bxl dice invocation",
        bz_data::span_start_event::Data::ReUpload(_) => "re upload",
        bz_data::span_start_event::Data::ConnectToInstaller(_) => "connect to installer",
        bz_data::span_start_event::Data::LocalResources(_) => "setup local resources",
        bz_data::span_start_event::Data::ReleaseLocalResources(_) => "release local resources",
        bz_data::span_start_event::Data::BxlEnsureArtifacts(_) => "ensure bxl artifacts",
        bz_data::span_start_event::Data::ActionErrorHandlerExecution(_) => "action error handler",
        bz_data::span_start_event::Data::CqueryUniverseBuild(_) => "cquery universe build",
        bz_data::span_start_event::Data::DepFileUpload(_) => "dep file upload",
        bz_data::span_start_event::Data::ComputeDetailedAggregatedMetrics(_) => {
            "compute detailed aggregated metrics"
        }
        bz_data::span_start_event::Data::DiceStateUpdateStage(_) => "dice state update stage",
        bz_data::span_start_event::Data::BzlmodRepo(_) => "bzlmod repository",
        bz_data::span_start_event::Data::BzlmodModuleExtension(_) => "bzlmod module extension",
        bz_data::span_start_event::Data::Fake(_) => "fake",
    }
    .to_owned()
}

fn critical_path_component_event(
    profile_start: SystemTime,
    entry: &bz_data::CriticalPathEntry2,
    original_thread_id: Option<u64>,
    thread_id: u64,
    path_name: &'static str,
    category: &'static str,
) -> Option<Value> {
    let start = profile_start.checked_add(Duration::from_nanos(entry.start_offset_ns?))?;
    let duration = critical_path_entry_duration(entry)?;
    if duration.is_zero() {
        return None;
    }

    let display = display::CriticalPathEntryDisplay::from_entry(
        entry,
        TargetDisplayOptions::for_chrome_trace(),
    )
    .ok()??;
    let entry_name = display.display_name();

    let mut args = Map::new();
    args.insert("path".to_owned(), json!(path_name));
    args.insert("kind".to_owned(), json!(display.kind));
    if !display.name.is_empty() {
        args.insert("name".to_owned(), json!(display.name));
    }
    if let Some(category) = display.category {
        args.insert("category".to_owned(), json!(category));
    }
    if let Some(identifier) = display.identifier {
        args.insert("identifier".to_owned(), json!(identifier));
    }
    if let Some(execution_kind) = display.execution_kind {
        args.insert("execution_kind".to_owned(), json!(execution_kind));
    }
    if let Some(original_thread_id) = original_thread_id {
        args.insert("tid".to_owned(), json!(original_thread_id));
    }
    if !entry.span_ids.is_empty() {
        args.insert("span_ids".to_owned(), json!(entry.span_ids));
    }
    if let Some(duration) = entry.duration.as_ref().and_then(duration_micros) {
        args.insert("critical_path_duration_us".to_owned(), json!(duration));
    }
    if let Some(duration) = entry.total_duration.as_ref().and_then(duration_micros) {
        args.insert("total_duration_us".to_owned(), json!(duration));
    }
    if let Some(duration) = entry
        .non_critical_path_duration
        .as_ref()
        .and_then(duration_micros)
    {
        args.insert("non_critical_path_duration_us".to_owned(), json!(duration));
    }
    if let Some(duration) = entry
        .potential_improvement_duration
        .as_ref()
        .and_then(duration_micros)
    {
        args.insert(
            "potential_improvement_duration_us".to_owned(),
            json!(duration),
        );
    }

    Some(json!({
        "cat": category,
        "name": entry_name,
        "ph": "X",
        "ts": timestamp_micros(profile_start, start),
        "dur": duration.as_micros() as u64,
        "pid": 1,
        "tid": thread_id,
        "args": Value::Object(args),
    }))
}

fn critical_path_entry_duration(entry: &bz_data::CriticalPathEntry2) -> Option<Duration> {
    let duration = entry
        .total_duration
        .as_ref()
        .or(entry.duration.as_ref())?
        .try_into_duration()
        .ok()?;
    let non_critical = entry
        .non_critical_path_duration
        .as_ref()
        .and_then(|duration| duration.try_into_duration().ok())
        .unwrap_or(Duration::ZERO);
    Some(duration.checked_add(non_critical).unwrap_or(duration))
}

fn duration_micros(duration: &prost_types::Duration) -> Option<u64> {
    duration
        .try_into_duration()
        .ok()
        .map(|duration| duration.as_micros() as u64)
}

fn complete_event(
    profile_start: SystemTime,
    mut task: OpenTask,
    duration: Duration,
    extra_args: Value,
) -> Value {
    merge_args(&mut task.args, extra_args);
    json!({
        "cat": task.category,
        "name": task.name,
        "ph": "X",
        "ts": timestamp_micros(profile_start, task.start),
        "dur": duration.as_micros() as u64,
        "pid": 1,
        "tid": task.thread_id,
        "args": task.args,
    })
}

fn instant_event(
    profile_start: SystemTime,
    name: &str,
    timestamp: SystemTime,
    thread_id: u64,
    args: Value,
) -> Value {
    json!({
        "name": name,
        "ph": "i",
        "s": "t",
        "ts": timestamp_micros(profile_start, timestamp),
        "pid": 1,
        "tid": thread_id,
        "args": args,
    })
}

fn counter_event(
    profile_start: SystemTime,
    name: &str,
    timestamp: SystemTime,
    thread_id: u64,
    args: Value,
) -> Value {
    json!({
        "name": name,
        "pid": 1,
        "tid": thread_id,
        "ph": "C",
        "ts": timestamp_micros(profile_start, timestamp),
        "args": args,
    })
}

fn process_name_event() -> Value {
    json!({
        "name": "process_name",
        "ph": "M",
        "pid": 1,
        "args": {"name": "bz"},
    })
}

fn thread_name_event(tid: u64, name: String) -> Value {
    json!({
        "name": "thread_name",
        "ph": "M",
        "pid": 1,
        "tid": tid,
        "args": {"name": name},
    })
}

fn thread_sort_index_event(tid: u64, sort_index: u64) -> Value {
    json!({
        "name": "thread_sort_index",
        "ph": "M",
        "pid": 1,
        "tid": tid,
        "args": {"sort_index": sort_index},
    })
}

fn worker_thread_name(worker_id: u64) -> String {
    format!(
        "bz-worker #{:0width$}",
        worker_id,
        width = WORKER_THREAD_NAME_WIDTH
    )
}

fn worker_thread_id(worker_id: u64) -> u64 {
    WORKER_THREAD_ID_START.saturating_add(worker_id.saturating_sub(1))
}

fn other_thread_id(tid: u64) -> u64 {
    OTHER_THREAD_ID_START.saturating_add(tid)
}

fn merge_args(base: &mut Value, extra: Value) {
    let Some(base) = base.as_object_mut() else {
        return;
    };
    let Some(extra) = extra.as_object() else {
        return;
    };
    for (key, value) in extra {
        base.insert(key.clone(), value.clone());
    }
}

fn shell_join(args: &[String]) -> String {
    shlex::try_join(args.iter().map(String::as_str)).unwrap_or_else(|_| args.join(" "))
}

fn temporary_profile_path(invocation_id: &TraceId) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("buck2-{invocation_id}-command.profile.gz"));
    path
}

fn millis_since_epoch(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn timestamp_micros(profile_start: SystemTime, timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(profile_start)
        .unwrap_or_default()
        .as_micros() as u64
}
