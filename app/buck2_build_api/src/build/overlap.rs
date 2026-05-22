/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use dice::UserComputationData;

pub struct BuildOverlapTracker {
    state: Mutex<BuildOverlapState>,
}

#[derive(Default)]
struct BuildOverlapState {
    active_analyses: usize,
    analyses_started: usize,
    analyses_finished: usize,
    actions_started: usize,
    actions_started_while_analysis_active: usize,
    first_analysis_start: Option<Instant>,
    first_action_start: Option<Instant>,
    first_overlap: Option<BuildOverlapPoint>,
}

#[derive(Clone, Debug)]
pub struct BuildOverlapSummary {
    pub analyses_started: usize,
    pub analyses_finished: usize,
    pub actions_started: usize,
    pub actions_started_while_analysis_active: usize,
    pub first_action_after_first_analysis: Option<Duration>,
    pub first_overlap: Option<BuildOverlapPoint>,
}

#[derive(Clone, Debug)]
pub struct BuildOverlapPoint {
    pub action: String,
    pub active_analyses: usize,
    pub elapsed_since_first_analysis: Duration,
}

impl BuildOverlapTracker {
    fn new() -> Self {
        Self {
            state: Mutex::new(BuildOverlapState::default()),
        }
    }

    fn record_analysis_started(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let now = Instant::now();
        state.first_analysis_start.get_or_insert(now);
        state.analyses_started += 1;
        state.active_analyses += 1;
    }

    fn record_analysis_finished(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.analyses_finished += 1;
        state.active_analyses = state.active_analyses.saturating_sub(1);
    }

    fn record_action_started(&self, action: impl FnOnce() -> String) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let now = Instant::now();
        let first_analysis_start = state.first_analysis_start;
        state.first_action_start.get_or_insert(now);
        state.actions_started += 1;

        if state.active_analyses == 0 {
            return;
        }

        state.actions_started_while_analysis_active += 1;
        if state.first_overlap.is_none() {
            state.first_overlap = Some(BuildOverlapPoint {
                action: action(),
                active_analyses: state.active_analyses,
                elapsed_since_first_analysis: first_analysis_start
                    .map(|first| now.saturating_duration_since(first))
                    .unwrap_or_default(),
            });
        }
    }

    fn summary(&self) -> Option<BuildOverlapSummary> {
        let Ok(state) = self.state.lock() else {
            return None;
        };
        Some(BuildOverlapSummary {
            analyses_started: state.analyses_started,
            analyses_finished: state.analyses_finished,
            actions_started: state.actions_started,
            actions_started_while_analysis_active: state.actions_started_while_analysis_active,
            first_action_after_first_analysis: match (
                state.first_analysis_start,
                state.first_action_start,
            ) {
                (Some(analysis), Some(action)) => Some(action.saturating_duration_since(analysis)),
                _ => None,
            },
            first_overlap: state.first_overlap.clone(),
        })
    }
}

pub trait HasBuildOverlapTracker {
    fn init_build_overlap_tracker(&mut self);
    fn record_analysis_started_for_overlap(&self);
    fn record_analysis_finished_for_overlap(&self);
    fn record_action_started_for_overlap(&self, action: impl FnOnce() -> String);
    fn build_overlap_summary(&self) -> Option<BuildOverlapSummary>;
}

impl HasBuildOverlapTracker for UserComputationData {
    fn init_build_overlap_tracker(&mut self) {
        self.data.set(BuildOverlapTracker::new());
    }

    fn record_analysis_started_for_overlap(&self) {
        if let Ok(tracker) = self.data.get::<BuildOverlapTracker>() {
            tracker.record_analysis_started();
        }
    }

    fn record_analysis_finished_for_overlap(&self) {
        if let Ok(tracker) = self.data.get::<BuildOverlapTracker>() {
            tracker.record_analysis_finished();
        }
    }

    fn record_action_started_for_overlap(&self, action: impl FnOnce() -> String) {
        if let Ok(tracker) = self.data.get::<BuildOverlapTracker>() {
            tracker.record_action_started(action);
        }
    }

    fn build_overlap_summary(&self) -> Option<BuildOverlapSummary> {
        self.data
            .get::<BuildOverlapTracker>()
            .ok()
            .and_then(|tracker| tracker.summary())
    }
}
