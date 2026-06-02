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

use allocative::Allocative;
use buck2_core::buck2_env;
use buck2_data::*;
use buck2_events::dispatch::EventDispatcher;
use buck2_events::dispatch::Span;
use buck2_events::dispatch::with_dispatcher_async;
use buck2_hash::StdBuckHashMap;
use buck2_util::threads::thread_spawn;
use dice::DiceEvent;
use dice::DiceEventListener;
use dupe::Dupe;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::mpsc::UnboundedSender;

/// The BuckDiceTracker keeps track of the started/finished events for a dice computation and periodically sends a snapshot to the client.
///
/// There are too many events coming out of dice for us to forward them all to the client, so we need to aggregate
/// them in some way in the daemon.
///
/// The tracker will send a snapshot event every 500ms (only if there have been changes since the last snapshot).
///
/// A client won't necessarily get a final snapshot before a command returns.
#[derive(Allocative)]
pub struct BuckDiceTracker {
    #[allocative(skip)]
    event_forwarder: UnboundedSender<DiceEvent>,
}

impl BuckDiceTracker {
    pub fn new(events: EventDispatcher) -> buck2_error::Result<Self> {
        let (event_forwarder, receiver) = mpsc::unbounded();
        let snapshot_interval =
            buck2_env!("BUCK2_DICE_SNAPSHOT_INTERVAL_MS", type=u64, default = 500)
                .map(Duration::from_millis)?;
        let show_dice_key_progress_spans =
            buck2_env!("BUCK2_DICE_PROGRESS_KEY_SPANS", type=bool, default=false)?;

        thread_spawn("buck2-dice-tracker", move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(with_dispatcher_async(
                events.dupe(),
                Self::run_task(
                    events,
                    receiver,
                    snapshot_interval,
                    show_dice_key_progress_spans,
                ),
            ))
        })
        .unwrap();

        Ok(Self { event_forwarder })
    }

    async fn run_task(
        events: EventDispatcher,
        mut receiver: UnboundedReceiver<DiceEvent>,
        snapshot_interval: Duration,
        show_dice_key_progress_spans: bool,
    ) {
        let mut needs_update = false;
        let mut states = StdBuckHashMap::default();
        let mut active_key_spans: StdBuckHashMap<(&'static str, String), Vec<Span>> =
            StdBuckHashMap::default();
        let mut interval = tokio::time::interval(snapshot_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // This will loop until the sender side of the channel is dropped.
        loop {
            tokio::select! {
                ev = receiver.next() => {
                    needs_update = true;
                    match ev {
                        Some(DiceEvent::Started{key_type, key}) => {
                            states.entry(key_type).or_insert_with(DiceKeyState::default).started += 1;
                            if let Some(stage) = dice_key_progress_stage(show_dice_key_progress_spans, key_type, &key) {
                                let span = events.create_span(DiceStateUpdateStageStart { stage });
                                active_key_spans.entry((key_type, key)).or_default().push(span);
                            }
                        }
                        Some(DiceEvent::Finished{key_type, key}) => {
                            states.entry(key_type).or_insert_with(DiceKeyState::default).finished += 1;
                            let active_key = (key_type, key);
                            if let Some(spans) = active_key_spans.get_mut(&active_key) {
                                if let Some(span) = spans.pop() {
                                    span.end(DiceStateUpdateStageEnd {});
                                }
                                if spans.is_empty() {
                                    active_key_spans.remove(&active_key);
                                }
                            }
                        }
                        Some(DiceEvent::CheckDepsStarted{key_type}) => {
                            states.entry(key_type).or_insert_with(DiceKeyState::default).check_deps_started += 1;
                        }
                        Some(DiceEvent::CheckDepsFinished{key_type}) => {
                            states.entry(key_type).or_insert_with(DiceKeyState::default).check_deps_finished += 1;
                        }
                        Some(DiceEvent::ComputeStarted{key_type}) => {
                            states.entry(key_type).or_insert_with(DiceKeyState::default).compute_started += 1;
                        }
                        Some(DiceEvent::ComputeFinished{key_type}) => {
                            states.entry(key_type).or_insert_with(DiceKeyState::default).compute_finished += 1;
                        }
                        None => {
                            // This indicates that the sender side has been dropped and we can exit.
                            break;
                        }
                    }
                }
                _ = interval.tick() => {
                    if needs_update {
                        needs_update = false;
                        events.instant_event(DiceStateSnapshot {
                            key_states: states
                                .iter()
                                .map(|(k, v)| ((*k).to_owned(), *v))
                                .collect(),
                        });
                    }
                }
            }
        }
    }
}

fn dice_key_progress_stage(
    show_all_key_spans: bool,
    key_type: &'static str,
    key: &str,
) -> Option<String> {
    // Domain-level leaf work already has better progress spans: action execution,
    // repository/download work, package loading, file watcher sync, and buckconfig stages.
    // Keep every DICE key in the aggregate snapshot, but do not promote graph-plumbing
    // keys like configured targets and package declarations into visible rows by default.
    show_all_key_spans.then(|| format!("{key} ({key_type}) -- computing DICE key"))
}

impl DiceEventListener for BuckDiceTracker {
    fn event(&self, event: DiceEvent) {
        let _ = self.event_forwarder.unbounded_send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dice_key_progress_spans_are_hidden_by_default() {
        assert_eq!(
            dice_key_progress_stage(false, "BazelPackageKey", "PACKAGE(root//foo)"),
            None
        );
    }

    #[test]
    fn dice_key_progress_spans_can_be_enabled_for_debugging() {
        assert_eq!(
            dice_key_progress_stage(true, "BazelPackageKey", "PACKAGE(root//foo)"),
            Some("PACKAGE(root//foo) (BazelPackageKey) -- computing DICE key".to_owned())
        );
    }
}
