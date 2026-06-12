/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! A sink for forwarding events to an optional remote event service.
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use fbinit::FacebookInit;

mod workspace {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::BuckEvent;
    use crate::Event;
    use crate::EventSink;
    use crate::EventSinkStats;
    use crate::EventSinkWithStats;

    pub enum RemoteEventSink {}

    impl RemoteEventSink {
        pub async fn send_now(&self, _event: BuckEvent) -> bz_error::Result<()> {
            Ok(())
        }
        pub async fn send_messages_now(&self, _events: Vec<BuckEvent>) -> bz_error::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl EventSink for RemoteEventSink {
        fn send(&self, _event: Event) {}
    }

    impl EventSinkWithStats for RemoteEventSink {
        fn to_event_sync(self: Arc<Self>) -> Arc<dyn EventSink> {
            self as _
        }

        fn stats(&self) -> EventSinkStats {
            match *self {}
        }
    }

    #[derive(Default)]
    pub struct RemoteEventSinkConfig {
        pub buffer_size: usize,
        pub retry_backoff: Duration,
        pub retry_attempts: usize,
        pub message_batch_size: Option<usize>,
        pub thrift_timeout: Duration,
    }
}

pub use workspace::*;

fn new_remote_event_sink_if_available(
    fb: FacebookInit,
    config: RemoteEventSinkConfig,
) -> bz_error::Result<Option<RemoteEventSink>> {
    let _ = (fb, config);
    Ok(None)
}

pub fn new_remote_event_sink_if_enabled(
    fb: FacebookInit,
    config: RemoteEventSinkConfig,
) -> bz_error::Result<Option<RemoteEventSink>> {
    if is_enabled() {
        new_remote_event_sink_if_available(fb, config)
    } else {
        Ok(None)
    }
}

/// Whether remote event logging is enabled for this process.
static REMOTE_EVENT_SINK_ENABLED: AtomicBool = AtomicBool::new(true);

/// Returns whether this process should write to a remote sink, if one is supported.
pub fn is_enabled() -> bool {
    REMOTE_EVENT_SINK_ENABLED.load(Ordering::Relaxed)
}

/// Disables remote event logging for this process.
pub fn disable() {
    REMOTE_EVENT_SINK_ENABLED.store(false, Ordering::Relaxed);
}
