/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_data::AnalysisEnd;
use bz_data::AnalysisStageStart;
use bz_data::AnalysisStart;
use bz_data::ExecutorStageStart;
use bz_data::LoadBuildFileEnd;
use bz_data::LocalStage;
use bz_data::ReStage;
use bz_data::analysis_stage_start;
use bz_data::executor_stage_start;
use bz_data::instant_event;
use bz_data::local_stage;
use bz_data::re_stage;
use bz_data::span_end_event;
use bz_data::span_start_event;
use bz_events::BuckEvent;
use bz_events::span::SpanId;
use bz_hash::StdBuckHashMap;
use bz_hash::StdBuckHashSet;

use crate::last_command_execution_kind::get_last_command_execution_time;
use crate::unpack_event::UnpackedBuckEvent;
use crate::unpack_event::unpack_event;

#[derive(Debug, Clone, Copy)]
enum State {
    Started,
    Running,
    Finished,
}

#[derive(Debug, Default)]
pub struct SpanMap<T> {
    map: StdBuckHashMap<SpanId, (State, T)>,
    running: u64,
    finished: u64,
    cancelled: u64,

    min_started: u64,
    min_finished: u64,
}

impl<T> SpanMap<T> {
    fn started(&mut self, id: SpanId, data: T) {
        self.map.insert(id, (State::Started, data));
        self.cancelled = self.cancelled.saturating_sub(1);
    }

    fn cancelled(&mut self, id: SpanId) -> Option<T> {
        self.map.remove(&id).map(|(state, v)| {
            match state {
                State::Started => {}
                State::Running => {
                    self.running -= 1;
                }
                State::Finished => {
                    self.finished -= 1;
                }
            }
            self.cancelled += 1;
            v
        })
    }

    fn running(&mut self, id: SpanId) -> Option<&mut T> {
        if let Some((state, v)) = self.map.get_mut(&id) {
            if let State::Started = state {
                *state = State::Running;
                self.running += 1;
            }
            Some(v)
        } else {
            None
        }
    }

    fn finished(&mut self, id: SpanId) -> Option<&mut T> {
        match self.map.get_mut(&id) {
            Some((state, v)) => {
                match state {
                    State::Started => {
                        self.finished += 1;
                    }
                    State::Running => {
                        self.running -= 1;
                        self.finished += 1;
                    }
                    State::Finished => {}
                }

                *state = State::Finished;
                Some(v)
            }
            None => None,
        }
    }

    fn get_stats(&self) -> BuildProgressPhaseStatsItem {
        BuildProgressPhaseStatsItem {
            started: std::cmp::max(self.min_started, self.map.len() as u64 + self.cancelled),
            finished: std::cmp::max(self.finished, self.min_finished),
            running: self.running,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildProgressPhaseStatsItem {
    pub started: u64,
    pub finished: u64,
    pub running: u64,
}

impl BuildProgressPhaseStatsItem {
    pub fn mark_all_finished(&mut self) {
        self.finished = self.started;
        self.running = 0;
    }
}

/// Tracks some stats about what we've completed in this build.
#[derive(Default)]

pub struct BuildProgressStats {
    pub dirs_read: u64,
    pub targets: u64,

    pub actions_declared: u64,
    pub artifacts_declared: u64,

    pub remote_cache_checks_started: u64,
    pub remote_cache_checks_finished: u64,
    pub running_remote_cache_checks: u64,

    pub running_local: u64,
    pub running_remote: u64,

    pub exec_time_ms: u64,
    pub cached_exec_time_ms: u64,
}

/// Tracks stats about ongoing work in the main phases of the build.
#[derive(Debug, Clone)]
pub struct BuildProgressPhaseStats {
    pub loads: BuildProgressPhaseStatsItem,
    pub analyses: BuildProgressPhaseStatsItem,
    pub actions: BuildProgressPhaseStatsItem,
    pub validations: BuildProgressPhaseStatsItem,
}

#[derive(Default, Clone)]
struct TrackedActionSpan {
    key: Option<bz_data::ActionKey>,
    running_local: bool,
    running_remote: bool,
}

#[derive(Debug, Clone, Copy)]
struct ActionProgressCounts {
    enqueued: u64,
    completed: u64,
}

#[derive(Default)]
struct TrackedLoadSpan {}

#[derive(Default)]
struct TrackedAnalysisSpan {}

#[derive(Default)]
struct TrackedValidationSpan {}

#[derive(Default)]
pub struct BuildProgressStateTracker {
    stats: BuildProgressStats,

    loads: SpanMap<TrackedLoadSpan>,
    analyses: SpanMap<TrackedAnalysisSpan>,
    actions: SpanMap<TrackedActionSpan>,
    action_keys_started: StdBuckHashSet<bz_data::ActionKey>,
    action_keys_finished: StdBuckHashSet<bz_data::ActionKey>,
    action_exec_time_keys: StdBuckHashSet<bz_data::ActionKey>,
    action_progress_counts: Option<ActionProgressCounts>,
    validations: SpanMap<TrackedValidationSpan>,
    remote_cache_checks: StdBuckHashMap<SpanId, ()>,
}

impl BuildProgressStateTracker {
    pub fn new() -> Self {
        Self {
            ..Default::default()
        }
    }

    pub fn handle_event(&mut self, event: &BuckEvent) -> bz_error::Result<()> {
        let ev = unpack_event(event)?;

        self.handle_load(&ev)?;
        self.handle_analysis(&ev)?;
        self.handle_actions(&ev)?;
        self.handle_remote_cache_checks(&ev)?;

        match unpack_event(event)? {
            UnpackedBuckEvent::Instant(_, _, instant_event::Data::DiceStateSnapshot(snapshot)) => {
                if let Some(read_dir_states) = snapshot.key_states.get("ReadDirKey") {
                    self.stats.dirs_read = read_dir_states.finished as u64;
                }

                let mut analysis_min_started = 0;
                let mut analysis_min_finished = 0;

                if let Some(states) = snapshot.key_states.get("AnalysisKey") {
                    analysis_min_started += states.started as u64;
                    analysis_min_finished += states.finished as u64;
                }

                if let Some(states) = snapshot.key_states.get("AnonTargetKey") {
                    analysis_min_started += states.started as u64;
                    analysis_min_finished += states.finished as u64;
                }

                if let Some(states) = snapshot.key_states.get("DeferredCompute") {
                    analysis_min_started += states.started as u64;
                    analysis_min_finished += states.finished as u64;
                }

                self.analyses.min_started = analysis_min_started;
                self.analyses.min_finished = analysis_min_finished;

                if let Some(states) = snapshot.key_states.get("BuildKey") {
                    self.actions.min_started = states.started as u64;
                    self.actions.min_finished = states.finished as u64;
                }

                if let Some(states) = snapshot.key_states.get("SingleValidationKey") {
                    self.validations.min_started = states.started as u64;
                    self.validations.min_finished = states.finished as u64;
                }
            }
            UnpackedBuckEvent::Instant(
                _,
                _,
                instant_event::Data::ActionExecutionProgress(progress),
            ) => {
                self.action_progress_counts = Some(ActionProgressCounts {
                    enqueued: progress.enqueued,
                    completed: progress.completed,
                });
            }
            UnpackedBuckEvent::SpanEnd(
                BuckEvent {
                    span_id: Some(span_id),
                    ..
                },
                _,
                span_end_event::Data::SpanCancelled(..),
            ) => {
                self.loads.cancelled(*span_id);
                self.analyses.cancelled(*span_id);
                if let Some(v) = self.actions.cancelled(*span_id) {
                    self.action_finished(v);
                }
                self.remote_cache_check_cancelled(*span_id);
            }
            _ => {
                // ignored
            }
        }

        Ok(())
    }

    fn handle_load(&mut self, ev: &UnpackedBuckEvent) -> bz_error::Result<()> {
        match ev {
            UnpackedBuckEvent::SpanStart(
                BuckEvent {
                    span_id: Some(span_id),
                    ..
                },
                _,
                span_start_event::Data::Load(..),
            ) => {
                self.loads.started(*span_id, TrackedLoadSpan {});
                self.loads.running(*span_id);
            }
            UnpackedBuckEvent::SpanEnd(
                BuckEvent {
                    span_id: Some(span_id),
                    ..
                },
                _,
                span_end_event::Data::Load(LoadBuildFileEnd { target_count, .. }),
            ) => {
                self.loads.finished(*span_id);
                if let Some(c) = target_count {
                    self.stats.targets += c;
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_analysis(&mut self, ev: &UnpackedBuckEvent) -> bz_error::Result<()> {
        match ev {
            UnpackedBuckEvent::SpanStart(
                BuckEvent {
                    span_id: Some(span_id),
                    ..
                },
                _,
                span_start_event::Data::Analysis(AnalysisStart { .. }),
            ) => {
                self.analyses.started(*span_id, TrackedAnalysisSpan {});
            }
            UnpackedBuckEvent::SpanStart(
                BuckEvent {
                    parent_id: Some(parent_id),
                    ..
                },
                _,
                span_start_event::Data::AnalysisStage(AnalysisStageStart {
                    stage: Some(analysis_stage_start::Stage::EvaluateRule(..)),
                }),
            ) => {
                self.analyses.running(*parent_id);
            }
            UnpackedBuckEvent::SpanEnd(
                BuckEvent {
                    span_id: Some(span_id),
                    ..
                },
                _,
                span_end_event::Data::Analysis(AnalysisEnd {
                    declared_actions,
                    declared_artifacts,
                    ..
                }),
            ) => {
                self.stats.actions_declared += declared_actions.unwrap_or(0);
                self.stats.artifacts_declared += declared_artifacts.unwrap_or(0);
                self.analyses.finished(*span_id);
            }
            _ => {}
        }
        Ok(())
    }

    fn remote_cache_check_started(&mut self, span_id: SpanId) {
        if self.remote_cache_checks.insert(span_id, ()).is_none() {
            self.stats.remote_cache_checks_started += 1;
            self.stats.running_remote_cache_checks += 1;
        }
    }

    fn remote_cache_check_finished(&mut self, span_id: SpanId) {
        if self.remote_cache_checks.remove(&span_id).is_some() {
            self.stats.remote_cache_checks_finished += 1;
            self.stats.running_remote_cache_checks =
                self.stats.running_remote_cache_checks.saturating_sub(1);
        }
    }

    fn remote_cache_check_cancelled(&mut self, span_id: SpanId) {
        if self.remote_cache_checks.remove(&span_id).is_some() {
            self.stats.running_remote_cache_checks =
                self.stats.running_remote_cache_checks.saturating_sub(1);
        }
    }

    fn handle_remote_cache_checks(&mut self, ev: &UnpackedBuckEvent) -> bz_error::Result<()> {
        match ev {
            UnpackedBuckEvent::SpanStart(
                BuckEvent {
                    span_id: Some(span_id),
                    ..
                },
                _,
                span_start_event::Data::ExecutorStage(ExecutorStageStart {
                    stage: Some(executor_stage_start::Stage::CacheQuery(cache_query)),
                }),
            ) => {
                if bz_data::CacheType::try_from(cache_query.cache_type).is_ok() {
                    self.remote_cache_check_started(*span_id);
                }
            }
            UnpackedBuckEvent::SpanEnd(
                BuckEvent {
                    span_id: Some(span_id),
                    ..
                },
                _,
                span_end_event::Data::ExecutorStage(..),
            ) => {
                self.remote_cache_check_finished(*span_id);
            }
            _ => {}
        }

        Ok(())
    }

    fn action_finished(&mut self, data: TrackedActionSpan) {
        if data.running_local {
            self.stats.running_local -= 1;
        }
        if data.running_remote {
            self.stats.running_remote -= 1;
        }
    }

    fn handle_actions(&mut self, ev: &UnpackedBuckEvent) -> bz_error::Result<()> {
        match ev {
            UnpackedBuckEvent::SpanStart(
                BuckEvent {
                    span_id: Some(span_id),
                    ..
                },
                _,
                span_start_event::Data::ActionExecution(action),
            ) => {
                if let Some(key) = &action.key {
                    self.action_keys_started.insert(key.clone());
                }
                self.actions.started(
                    *span_id,
                    TrackedActionSpan {
                        key: action.key.clone(),
                        ..Default::default()
                    },
                );
            }
            UnpackedBuckEvent::SpanStart(
                BuckEvent {
                    parent_id: Some(parent_id),
                    ..
                },
                _,
                span_start_event::Data::ExecutorStage(ExecutorStageStart { stage: Some(stage) }),
            ) => {
                match stage {
                    executor_stage_start::Stage::Re(ReStage {
                        stage: Some(re_stage::Stage::Execute(..)),
                    }) => {
                        if let Some(data) = self.actions.running(*parent_id) {
                            data.running_remote = true;
                            self.stats.running_remote += 1;
                        }
                    }
                    executor_stage_start::Stage::Local(LocalStage {
                        stage: Some(local_stage::Stage::Execute(..)),
                    }) => {
                        if let Some(data) = self.actions.running(*parent_id) {
                            data.running_local = true;
                            self.stats.running_local += 1;
                        }
                    }
                    _ => {}
                };
            }
            UnpackedBuckEvent::SpanEnd(
                BuckEvent {
                    span_id: Some(span_id),
                    ..
                },
                _,
                span_end_event::Data::ActionExecution(end),
            ) => {
                if let Some(data) = self.actions.finished(*span_id) {
                    let data = data.clone();
                    if let Some(key) = &data.key {
                        self.action_keys_finished.insert(key.clone());
                    }
                    self.action_finished(data);
                } else if let Some(key) = &end.key {
                    self.action_keys_started.insert(key.clone());
                    self.action_keys_finished.insert(key.clone());
                }

                let exec_time = get_last_command_execution_time(end);
                if exec_time.exec_time_ms > 0 || exec_time.cached_exec_time_ms > 0 {
                    let should_count_time = match &end.key {
                        Some(key) => self.action_exec_time_keys.insert(key.clone()),
                        None => true,
                    };
                    if should_count_time {
                        self.stats.exec_time_ms += exec_time.exec_time_ms;
                        self.stats.cached_exec_time_ms += exec_time.cached_exec_time_ms;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn phase_stats(&self) -> BuildProgressPhaseStats {
        let mut actions = self.actions.get_stats();
        if let Some(action_progress) = self.action_progress_counts {
            actions.started = action_progress.enqueued;
            actions.finished = action_progress.completed;
        } else if !self.action_keys_started.is_empty() || !self.action_keys_finished.is_empty() {
            actions.finished = self.action_keys_finished.len() as u64;
            actions.started = std::cmp::max(
                self.action_keys_started.len() as u64,
                actions.finished + actions.running,
            );
        }
        BuildProgressPhaseStats {
            loads: self.loads.get_stats(),
            analyses: self.analyses.get_stats(),
            actions,
            validations: self.validations.get_stats(),
        }
    }

    pub fn progress_stats(&self) -> &BuildProgressStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use bz_events::BuckEvent;
    use bz_wrapper_common::invocation_id::TraceId;

    use super::*;

    fn instant_event(data: impl Into<instant_event::Data>) -> BuckEvent {
        BuckEvent::new(
            UNIX_EPOCH,
            TraceId::new(),
            None,
            None,
            bz_data::InstantEvent {
                data: Some(data.into()),
            }
            .into(),
        )
    }

    fn action_execution_start_event(span_id: SpanId) -> BuckEvent {
        BuckEvent::new(
            UNIX_EPOCH,
            TraceId::new(),
            Some(span_id),
            None,
            bz_data::SpanStartEvent {
                data: Some(bz_data::ActionExecutionStart::default().into()),
            }
            .into(),
        )
    }

    #[test]
    fn test_span_map() -> bz_error::Result<()> {
        let mut map: SpanMap<u64> = SpanMap::default();

        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 0,
                finished: 0,
                running: 0
            }
        );

        map.started(SpanId::from_u64(1).unwrap(), 1);

        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 1,
                finished: 0,
                running: 0
            }
        );

        assert_eq!(map.running(SpanId::from_u64(1).unwrap()).copied(), Some(1));

        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 1,
                finished: 0,
                running: 1
            }
        );

        assert!(map.finished(SpanId::from_u64(1).unwrap()).is_some());
        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 1,
                finished: 1,
                running: 0
            }
        );

        map.started(SpanId::from_u64(2).unwrap(), 2);
        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 2,
                finished: 1,
                running: 0
            }
        );

        map.cancelled(SpanId::from_u64(2).unwrap());
        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 2,
                finished: 1,
                running: 0
            }
        );

        // started shouldn't be incremented because we had a cancellation
        map.started(SpanId::from_u64(3).unwrap(), 3);
        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 2,
                finished: 1,
                running: 0
            }
        );

        // started should now increment
        map.started(SpanId::from_u64(4).unwrap(), 4);
        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 3,
                finished: 1,
                running: 0
            }
        );

        map.min_started = 8;

        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 8,
                finished: 1,
                running: 0
            }
        );

        map.min_finished = 4;
        assert_eq!(
            map.get_stats(),
            BuildProgressPhaseStatsItem {
                started: 8,
                finished: 4,
                running: 0
            }
        );

        Ok(())
    }

    #[test]
    fn action_spans_are_legacy_action_progress_fallback() -> bz_error::Result<()> {
        let mut tracker = BuildProgressStateTracker::new();

        tracker.handle_event(&action_execution_start_event(SpanId::from_u64(1).unwrap()))?;

        assert_eq!(
            tracker.phase_stats().actions,
            BuildProgressPhaseStatsItem {
                started: 1,
                finished: 0,
                running: 0,
            }
        );

        Ok(())
    }

    #[test]
    fn action_execution_progress_event_controls_action_denominator() -> bz_error::Result<()> {
        let mut tracker = BuildProgressStateTracker::new();

        tracker.handle_event(&instant_event(bz_data::ActionExecutionProgress {
            enqueued: 10,
            completed: 4,
        }))?;

        assert_eq!(
            tracker.phase_stats().actions,
            BuildProgressPhaseStatsItem {
                started: 10,
                finished: 4,
                running: 0,
            }
        );

        Ok(())
    }

    #[test]
    fn action_spans_do_not_change_denominator_after_progress_event() -> bz_error::Result<()> {
        let mut tracker = BuildProgressStateTracker::new();

        tracker.handle_event(&instant_event(bz_data::ActionExecutionProgress {
            enqueued: 10,
            completed: 4,
        }))?;
        tracker.handle_event(&action_execution_start_event(SpanId::from_u64(1).unwrap()))?;

        assert_eq!(
            tracker.phase_stats().actions,
            BuildProgressPhaseStatsItem {
                started: 10,
                finished: 4,
                running: 0,
            }
        );

        Ok(())
    }
}
