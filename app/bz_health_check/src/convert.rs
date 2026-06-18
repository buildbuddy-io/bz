/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

// gRPC to rust converters

use bz_error::BuckErrorContext;
use bz_error::internal_error;

use crate::interface::HealthCheckContextEvent;
use crate::interface::HealthCheckSnapshotData;
use crate::interface::HealthCheckType;
use crate::report::DisplayReport;
use crate::report::HealthIssue;
use crate::report::Message;
use crate::report::Remediation;
use crate::report::Report;
use crate::report::Severity;

impl TryFrom<i32> for Severity {
    type Error = bz_error::Error;
    fn try_from(s: i32) -> bz_error::Result<Self> {
        let severity = bz_health_check_proto::Severity::try_from(s)
            .buck_error_context("Invalid `severity`")?;
        Ok(match severity {
            bz_health_check_proto::Severity::Info => Severity::Info,
            bz_health_check_proto::Severity::Warning => Severity::Warning,
        })
    }
}

impl TryInto<i32> for Severity {
    type Error = bz_error::Error;
    fn try_into(self) -> bz_error::Result<i32> {
        Ok(match self {
            Severity::Info => bz_health_check_proto::Severity::Info,
            Severity::Warning => bz_health_check_proto::Severity::Warning,
        } as i32)
    }
}

impl TryFrom<bz_health_check_proto::Remediation> for Remediation {
    type Error = bz_error::Error;

    fn try_from(value: bz_health_check_proto::Remediation) -> bz_error::Result<Self> {
        Ok(
            match value
                .data
                .ok_or_else(|| internal_error!("Invalid `remediation`"))?
            {
                bz_health_check_proto::remediation::Data::Message(message) => {
                    Remediation::Message(message)
                }
                bz_health_check_proto::remediation::Data::Link(link) => Remediation::Link(link),
            },
        )
    }
}

impl TryInto<bz_health_check_proto::Remediation> for Remediation {
    type Error = bz_error::Error;

    fn try_into(self) -> bz_error::Result<bz_health_check_proto::Remediation> {
        let value = match self {
            Remediation::Message(message) => {
                bz_health_check_proto::remediation::Data::Message(message)
            }
            Remediation::Link(link) => bz_health_check_proto::remediation::Data::Link(link),
        };
        Ok(bz_health_check_proto::Remediation { data: Some(value) })
    }
}

impl TryFrom<i32> for HealthCheckType {
    type Error = bz_error::Error;

    fn try_from(value: i32) -> bz_error::Result<Self> {
        let value = bz_health_check_proto::HealthCheckType::try_from(value)
            .buck_error_context("Invalid `health_check_type`")?;
        Ok(match value {
            bz_health_check_proto::HealthCheckType::MemoryPressure => {
                HealthCheckType::MemoryPressure
            }
            bz_health_check_proto::HealthCheckType::LowDiskSpace => HealthCheckType::LowDiskSpace,
            bz_health_check_proto::HealthCheckType::SlowDownloadSpeed => {
                HealthCheckType::SlowDownloadSpeed
            }
            bz_health_check_proto::HealthCheckType::SlowBuild => HealthCheckType::SlowBuild,
            bz_health_check_proto::HealthCheckType::VpnEnabled => HealthCheckType::VpnEnabled,
            bz_health_check_proto::HealthCheckType::StableRevision => {
                HealthCheckType::StableRevision
            }
        })
    }
}

impl TryInto<i32> for HealthCheckType {
    type Error = bz_error::Error;

    fn try_into(self) -> bz_error::Result<i32> {
        Ok(match self {
            HealthCheckType::MemoryPressure => {
                bz_health_check_proto::HealthCheckType::MemoryPressure
            }
            HealthCheckType::LowDiskSpace => bz_health_check_proto::HealthCheckType::LowDiskSpace,
            HealthCheckType::SlowDownloadSpeed => {
                bz_health_check_proto::HealthCheckType::SlowDownloadSpeed
            }
            HealthCheckType::VpnEnabled => bz_health_check_proto::HealthCheckType::VpnEnabled,
            HealthCheckType::StableRevision => {
                bz_health_check_proto::HealthCheckType::StableRevision
            }
            HealthCheckType::SlowBuild => bz_health_check_proto::HealthCheckType::SlowBuild,
        } as i32)
    }
}

impl TryFrom<bz_health_check_proto::Message> for Message {
    type Error = bz_error::Error;

    fn try_from(value: bz_health_check_proto::Message) -> bz_error::Result<Self> {
        match value
            .data
            .ok_or_else(|| internal_error!("Invalid message format"))?
        {
            bz_health_check_proto::message::Data::Simple(text) => Ok(Message::Simple(text)),
            bz_health_check_proto::message::Data::Rich(rich_msg) => Ok(Message::Rich {
                header: rich_msg.header,
                body: rich_msg.body,
                footer: rich_msg.footer,
                compact: rich_msg.compact,
            }),
        }
    }
}

impl TryInto<bz_health_check_proto::Message> for Message {
    type Error = bz_error::Error;

    fn try_into(self) -> bz_error::Result<bz_health_check_proto::Message> {
        let data = match self {
            Message::Simple(text) => bz_health_check_proto::message::Data::Simple(text),
            Message::Rich {
                header,
                body,
                footer,
                compact,
            } => bz_health_check_proto::message::Data::Rich(bz_health_check_proto::RichMessage {
                header,
                body,
                footer,
                compact,
            }),
        };
        Ok(bz_health_check_proto::Message { data: Some(data) })
    }
}

impl TryFrom<bz_health_check_proto::HealthIssue> for HealthIssue {
    type Error = bz_error::Error;

    fn try_from(value: bz_health_check_proto::HealthIssue) -> bz_error::Result<Self> {
        Ok(HealthIssue {
            severity: value.severity.try_into()?,
            message: value
                .message
                .ok_or_else(|| internal_error!("Missing message"))?
                .try_into()?,
            remediation: value.remediation.map(|r| r.try_into()).transpose()?,
        })
    }
}

impl TryInto<bz_health_check_proto::HealthIssue> for HealthIssue {
    type Error = bz_error::Error;

    fn try_into(self) -> bz_error::Result<bz_health_check_proto::HealthIssue> {
        Ok(bz_health_check_proto::HealthIssue {
            severity: self.severity.try_into()?,
            message: Some(self.message.try_into()?),
            remediation: self.remediation.map(|r| r.try_into()).transpose()?,
        })
    }
}

impl TryFrom<bz_health_check_proto::DisplayReport> for DisplayReport {
    type Error = bz_error::Error;

    fn try_from(value: bz_health_check_proto::DisplayReport) -> bz_error::Result<Self> {
        Ok(DisplayReport {
            health_check_type: value.health_check_type.try_into()?,
            health_issue: value.health_issue.map(|i| i.try_into()).transpose()?,
        })
    }
}
impl TryInto<bz_health_check_proto::DisplayReport> for DisplayReport {
    type Error = bz_error::Error;

    fn try_into(self) -> bz_error::Result<bz_health_check_proto::DisplayReport> {
        Ok(bz_health_check_proto::DisplayReport {
            health_check_type: self.health_check_type.try_into()?,
            health_issue: self.health_issue.map(|i| i.try_into()).transpose()?,
        })
    }
}

impl TryFrom<bz_health_check_proto::Report> for Report {
    type Error = bz_error::Error;

    fn try_from(value: bz_health_check_proto::Report) -> bz_error::Result<Self> {
        Ok(Report {
            display_report: value.display_report.map(|d| d.try_into()).transpose()?,
            tag: value.tag,
        })
    }
}

impl TryInto<bz_health_check_proto::Report> for Report {
    type Error = bz_error::Error;

    fn try_into(self) -> bz_error::Result<bz_health_check_proto::Report> {
        Ok(bz_health_check_proto::Report {
            display_report: self.display_report.map(|d| d.try_into()).transpose()?,
            tag: self.tag,
        })
    }
}

impl TryInto<bz_health_check_proto::HealthCheckContextEvent> for HealthCheckContextEvent {
    type Error = bz_error::Error;

    fn try_into(self) -> bz_error::Result<bz_health_check_proto::HealthCheckContextEvent> {
        Ok(match self {
            HealthCheckContextEvent::BranchedFromRevision(rev) => {
                bz_health_check_proto::HealthCheckContextEvent {
                    data: Some(bz_health_check_proto::health_check_context_event::Data::BranchedFromRevision(rev)),
                }
            }
            HealthCheckContextEvent::CommandStart(cmd) => {
                bz_health_check_proto::HealthCheckContextEvent {
                    data: Some(bz_health_check_proto::health_check_context_event::Data::CommandStart(cmd.clone())),
                }
            }
            HealthCheckContextEvent::ParsedTargetPatterns(patterns) => {
                bz_health_check_proto::HealthCheckContextEvent {
                    data: Some(bz_health_check_proto::health_check_context_event::Data::ParsedTargetPatterns(patterns.clone())),
                }
            }
            HealthCheckContextEvent::HasExcessCacheMisses() => {
                bz_health_check_proto::HealthCheckContextEvent {
                    data: Some(bz_health_check_proto::health_check_context_event::Data::HasExcessCacheMisses(true)),
                }
            }
            HealthCheckContextEvent::ExperimentConfigurations(system_info) => {
                bz_health_check_proto::HealthCheckContextEvent {
                    data: Some(bz_health_check_proto::health_check_context_event::Data::ExperimentConfigurations(system_info.clone())),
                }
            }
            HealthCheckContextEvent::TestSlowBuildThreshold(secs) => {
                bz_health_check_proto::HealthCheckContextEvent {
                    data: Some(bz_health_check_proto::health_check_context_event::Data::TestSlowBuildThresholdSecs(secs)),
                }
            }
        })
    }
}

impl TryFrom<bz_health_check_proto::HealthCheckContextEvent> for HealthCheckContextEvent {
    type Error = bz_error::Error;
    fn try_from(value: bz_health_check_proto::HealthCheckContextEvent) -> bz_error::Result<Self> {
        Ok( match value.data.ok_or_else(|| internal_error!("Invalid `health_check_context_event`"))? {
            bz_health_check_proto::health_check_context_event::Data::BranchedFromRevision(rev) => {
                HealthCheckContextEvent::BranchedFromRevision(rev)
            }
            bz_health_check_proto::health_check_context_event::Data::CommandStart(cmd) => {
                HealthCheckContextEvent::CommandStart(cmd)
            }
            bz_health_check_proto::health_check_context_event::Data::ParsedTargetPatterns(patterns) => {
                HealthCheckContextEvent::ParsedTargetPatterns(patterns)
            }
            bz_health_check_proto::health_check_context_event::Data::HasExcessCacheMisses(_) => {
                HealthCheckContextEvent::HasExcessCacheMisses()
            }
            bz_health_check_proto::health_check_context_event::Data::ExperimentConfigurations(system_info) => {
                HealthCheckContextEvent::ExperimentConfigurations(system_info)
            }
            bz_health_check_proto::health_check_context_event::Data::TestSlowBuildThresholdSecs(secs) => {
                HealthCheckContextEvent::TestSlowBuildThreshold(secs)
            }
        }
    )
    }
}

impl TryFrom<bz_health_check_proto::HealthCheckSnapshotData> for HealthCheckSnapshotData {
    type Error = bz_error::Error;

    fn try_from(value: bz_health_check_proto::HealthCheckSnapshotData) -> bz_error::Result<Self> {
        use std::time::Duration;
        use std::time::UNIX_EPOCH;

        let proto_timestamp = value.timestamp.ok_or_else(|| {
            bz_error::bz_error!(
                bz_error::ErrorTag::HealthCheck,
                "Missing timestamp in HealthCheckSnapshotData"
            )
        })?;

        // Convert protobuf Timestamp to SystemTime
        let duration = Duration::new(proto_timestamp.seconds as u64, proto_timestamp.nanos as u32);
        let timestamp = UNIX_EPOCH + duration;

        Ok(HealthCheckSnapshotData { timestamp })
    }
}

impl TryInto<bz_health_check_proto::HealthCheckSnapshotData> for HealthCheckSnapshotData {
    type Error = bz_error::Error;

    fn try_into(self) -> bz_error::Result<bz_health_check_proto::HealthCheckSnapshotData> {
        // Convert SystemTime to protobuf Timestamp
        let duration_since_epoch = self
            .timestamp
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_e| {
                bz_error::bz_error!(
                    bz_error::ErrorTag::HealthCheck,
                    "Invalid timestamp in HealthCheckSnapshotData"
                )
            })?;

        let timestamp = Some(prost_types::Timestamp {
            seconds: duration_since_epoch.as_secs() as i64,
            nanos: duration_since_epoch.subsec_nanos() as i32,
        });

        Ok(bz_health_check_proto::HealthCheckSnapshotData { timestamp })
    }
}
