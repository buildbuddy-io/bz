/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::time::Duration;
use std::time::SystemTime;

use bz_event_log::read::EventLogPathBuf;
use bz_event_log::stream_value::StreamValue;
use bz_event_log::utils::Invocation;
use bz_events::BuckEvent;
use bz_util::truncate::truncate;
use bz_wrapper_common::invocation_id::TraceId;
use chrono::DateTime;
use chrono::Local;
use futures::TryStreamExt;
use humantime::format_duration;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Tier0)]
enum BuildInfoError {
    #[error("Failed to read event log")]
    EventLogReadFail,
}

struct LogInfo {
    revision: Option<String>,
    daemon_uptime_s: Option<u64>,
    timestamp_end: Option<SystemTime>,
    re_session_id: Option<String>,
}

pub(crate) struct BuildInfo {
    uuid: TraceId,
    pub timestamp: DateTime<Local>,
    pub command: String,
    working_dir: String,
    pub bz_revision: String,
    pub command_duration: Option<Duration>,
    pub daemon_uptime_s: Option<u64>,
    pub re_session_id: Option<String>,
}

impl fmt::Display for BuildInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "bz UI: https://www.internalfb.com/buck2/{}
timestamp: {}
command: {}
working dir: {}
bz_revision: {}
command duration: {}
daemon uptime: {}
RE session id: {}
        ",
            self.uuid,
            self.timestamp.format("%c %Z"),
            self.command,
            self.working_dir,
            self.bz_revision,
            seconds_to_string(self.command_duration.map(|d| d.as_secs())),
            seconds_to_string(self.daemon_uptime_s),
            self.re_session_id
                .as_ref()
                .map_or_else(|| "", |s| s.as_str()),
        )
    }
}

pub(crate) async fn get(log: &EventLogPathBuf) -> bz_error::Result<BuildInfo> {
    let (invocation, events) = log.unpack_stream().await?;
    let mut filtered_events = events.try_filter_map(|log| {
        let maybe_buck_event = match log {
            StreamValue::Result(_) | StreamValue::PartialResult(_) => None,
            StreamValue::Event(buck_event) => Some(buck_event),
        };
        futures::future::ready(Ok(maybe_buck_event))
    });

    let first_event: BuckEvent = filtered_events
        .try_next()
        .await?
        .ok_or(BuildInfoError::EventLogReadFail)?
        .try_into()?;

    let mut info = LogInfo {
        revision: None,
        daemon_uptime_s: None,
        timestamp_end: None,
        re_session_id: None,
    };
    loop {
        let res = match filtered_events.try_next().await {
            Ok(Some(event)) => extract_info(&mut info, event),
            Ok(None) => break,
            Err(e) => Err(e),
        };
        if let Err(e) = res {
            bz_client_ctx::eprintln!("Error found when iterating through logs: {:#}", e)?;
            break;
        }
    }

    let timestamp_start = first_event.timestamp();
    let duration = {
        if let Some(end) = info.timestamp_end {
            Some(end.duration_since(timestamp_start)?)
        } else {
            None
        }
    };

    let t_start: DateTime<Local> = timestamp_start.into();

    let output = BuildInfo {
        uuid: first_event.trace_id()?,
        timestamp: t_start,
        command: format_cmd(&invocation),
        working_dir: invocation.working_dir,
        bz_revision: info.revision.unwrap_or_else(|| "".to_owned()),
        command_duration: duration,
        daemon_uptime_s: info.daemon_uptime_s,
        re_session_id: info.re_session_id,
    };

    Ok(output)
}

fn extract_info(info: &mut LogInfo, event: Box<bz_data::BuckEvent>) -> bz_error::Result<()> {
    if let Some(bz_data::buck_event::Data::SpanStart(span)) = &event.data
        && let Some(bz_data::span_start_event::Data::Command(action)) = &span.data
    {
        if info.revision.is_none()
            && action.metadata.contains_key("bz_revision")
            && let Some(bz_revision) = action.metadata.get("bz_revision")
        {
            info.revision.get_or_insert(bz_revision.clone());
        }
    } else if let Some(bz_data::buck_event::Data::Instant(span)) = &event.data {
        if let Some(bz_data::instant_event::Data::Snapshot(snapshot)) = &span.data {
            info.daemon_uptime_s.get_or_insert(snapshot.daemon_uptime_s);
        } else if let Some(bz_data::instant_event::Data::ReSession(session)) = &span.data {
            info.re_session_id.get_or_insert(session.session_id.clone());
        }
    }
    if let Some(timestamp) = event.timestamp {
        info.timestamp_end = Some(SystemTime::try_from(timestamp)?)
    };
    Ok(())
}

fn seconds_to_string(seconds: Option<u64>) -> String {
    if let Some(seconds) = seconds {
        let duration = Duration::from_secs(seconds);
        format_duration(duration).to_string()
    } else {
        "".to_owned()
    }
}

pub fn format_cmd(cmd: &Invocation) -> String {
    truncate(&cmd.display_expanded_command_line(), 256)
}
