/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Shared, concurrent dice task cache that is shared between computations at the same version

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use allocative::Allocative;
use bz_hash::BuckHasherBuilder;
use dashmap::DashMap;
use dice_error::result::CancellationReason;
use dupe::Dupe;
use lock_free_hashtable::sharded::ShardedLockFreeRawTable;

use crate::arc::Arc;
use crate::impls::key::DiceKey;
use crate::impls::task::dice::DiceTask;
use crate::impls::value::DiceComputedValue;

#[derive(Allocative)]
struct Data {
    completed: ShardedLockFreeRawTable<Arc<DiceCompletedTask>, 64>,
    /// Completed tasks lazily moved into `completed` from this map.
    storage: DashMap<DiceKey, DiceTask, BuckHasherBuilder>,
    is_cancelled: AtomicBool,
    /// Keys whose completed result at this version has been discarded by `rewind`.
    /// The lock-free `completed` table does not support removal, so rewound keys are
    /// overlaid here and their `completed` entry (if any) must never be consulted
    /// again: `None` means the key must recompute when next requested, and
    /// `Some(value)` holds the result of the post-rewind recompute.
    rewound: DashMap<DiceKey, Option<DiceComputedValue>, BuckHasherBuilder>,
    /// Whether `rewound` has ever been written to, so that the overlay costs nothing
    /// until the first rewind (the overwhelmingly common case).
    has_rewound: AtomicBool,
}

#[derive(Allocative, Clone, Dupe)]
pub(crate) struct SharedCache {
    data: Arc<Data>,
}

#[derive(Allocative)]
struct DiceCompletedTask {
    key: DiceKey,
    value: DiceComputedValue,
}

/// Reference to the task in the cache.
pub(crate) enum DiceTaskRef<'a> {
    Computed(DiceComputedValue),
    Occupied(dashmap::mapref::entry::OccupiedEntry<'a, DiceKey, DiceTask>),
    Vacant(dashmap::mapref::entry::VacantEntry<'a, DiceKey, DiceTask>),
    TransactionCancelled,
}

impl DiceTaskRef<'_> {
    #[cfg(test)]
    pub(crate) fn testing_insert(self, task: DiceTask) {
        if let Self::Vacant(e) = self {
            e.insert(task);
        } else {
            panic!("inserting into non-vacant entry");
        }
    }
}

impl SharedCache {
    fn key_hash(key: DiceKey) -> u64 {
        (key.index as u64).wrapping_mul(0x9e3779b97f4a7c15)
    }

    /// Returns `Some(overlay)` if `key` has been rewound at this version, in which
    /// case the `completed` table must not be consulted for it. The overlay is the
    /// post-rewind result, if one has been computed yet.
    fn try_get_rewound(&self, key: DiceKey) -> Option<Option<DiceComputedValue>> {
        if self.data.has_rewound.load(Ordering::Acquire) {
            self.data
                .rewound
                .get(&key)
                .map(|overlay| overlay.value().as_ref().map(Dupe::dupe))
        } else {
            None
        }
    }

    fn try_get_computed(&self, key: DiceKey) -> Option<DiceComputedValue> {
        if let Some(overlay) = self.try_get_rewound(key) {
            return overlay;
        }
        let hash = Self::key_hash(key);
        self.data
            .completed
            .lookup(hash, |task| task.key == key)
            .map(|task| task.value.dupe())
    }

    pub(crate) fn get(&self, key: DiceKey) -> DiceTaskRef<'_> {
        if let Some(computed) = self.try_get_computed(key) {
            return DiceTaskRef::Computed(computed);
        }

        let entry = self.data.storage.entry(key);

        // Not we acquired the lock, check computed map again.
        let computed = self.try_get_computed(key);

        if let Some(computed) = computed {
            return DiceTaskRef::Computed(computed);
        }

        let working_entry = match entry {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                if let Some(Ok(result)) = e.get().get_finished_value() {
                    // Promote entry to computed.
                    // So lookup will be faster next time.
                    let rewound_overlay = if self.data.has_rewound.load(Ordering::Acquire) {
                        self.data.rewound.get_mut(&key)
                    } else {
                        None
                    };
                    if let Some(mut overlay) = rewound_overlay {
                        // The `completed` table may already hold the stale pre-rewind
                        // entry for this key, which can be neither removed nor
                        // replaced; the post-rewind result lives in the overlay.
                        *overlay = Some(result.dupe());
                    } else {
                        // TODO(nga): insert unique unchecked,
                        //   which `LockFreeRawTable` does not support yet.
                        let (_ignore, original) = self.data.completed.insert(
                            Self::key_hash(key),
                            Arc::new(DiceCompletedTask {
                                key,
                                value: result.dupe(),
                            }),
                            |a, b| a.key == b.key,
                            |task| Self::key_hash(task.key),
                        );
                        assert!(original.is_none());
                    }

                    // Must remove from dashmap after inserting into completed.
                    e.remove();
                    return DiceTaskRef::Computed(result);
                }
                DiceTaskRef::Occupied(e)
            }
            dashmap::mapref::entry::Entry::Vacant(e) => DiceTaskRef::Vacant(e),
        };

        if self.data.is_cancelled.load(Ordering::Acquire) {
            return DiceTaskRef::TransactionCancelled;
        }

        working_entry
    }

    /// Discards the completed result for `key` at this version so that the next
    /// request recomputes it. A currently-running task for `key` is not restarted:
    /// it began before the rewind and its (eventual) result is treated as the
    /// post-rewind result. Callers wanting strictly-after semantics must rewind
    /// again after observing a result they consider stale.
    pub(crate) fn rewind(&self, key: DiceKey) {
        self.data.rewound.insert(key, None);
        // `Release` pairs with the `Acquire` loads in `try_get_rewound`/`get`: a
        // reader that observes the flag also observes the tombstone inserted above.
        self.data.has_rewound.store(true, Ordering::Release);
        // A finished task still sitting in `storage` is the same stale result that
        // the tombstone above shadows in `completed`; drop it so the next request
        // spawns a fresh computation instead of promoting it into the overlay.
        self.data
            .storage
            .remove_if(&key, |_, task| matches!(task.get_finished_value(), Some(Ok(_))));
    }

    pub(crate) fn new() -> Self {
        SharedCache {
            data: Arc::new(Data {
                storage: DashMap::default(),
                completed: ShardedLockFreeRawTable::new(),
                is_cancelled: AtomicBool::new(false),
                rewound: DashMap::default(),
                has_rewound: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn active_tasks_count(&self) -> usize {
        self.data.storage.len() + self.data.completed.len()
    }

    /// This function gets the termination observer for all running tasks when transaction is
    /// cancelled and prevents further tasks from being added
    pub(crate) fn cancel_pending_tasks(self) -> Vec<DiceTask> {
        self.data.is_cancelled.store(true, Ordering::Release);
        self.data
            .storage
            .iter()
            .filter_map(|entry| {
                if entry.value().is_pending() {
                    entry.value().cancel(CancellationReason::TransactionDropped);
                    Some(entry.value().clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
impl SharedCache {
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }
}

pub(crate) mod introspection {
    use crate::impls::cache::SharedCache;
    use crate::impls::key::DiceKey;
    use crate::legacy::dice_futures::dice_task::DiceTaskStateForDebugging;

    impl SharedCache {
        pub(crate) fn iter_tasks(
            &self,
        ) -> impl Iterator<Item = (DiceKey, DiceTaskStateForDebugging)> {
            self.data
                .storage
                .iter()
                .map(|entry| (*entry.key(), entry.value().introspect_state()))
        }
    }
}

#[cfg(test)]
mod tests {
    use allocative::Allocative;
    use async_trait::async_trait;
    use derive_more::Display;
    use dice_error::result::CancellationReason;
    use dice_futures::cancellation::CancellationContext;
    use dice_futures::spawner::TokioSpawner;
    use dupe::Dupe;
    use futures::FutureExt;
    use pagable::Pagable;
    use pagable::pagable_typetag;

    use crate::DiceKeyDyn;
    use crate::api::computations::DiceComputations;
    use crate::api::key::Key;
    use crate::api::key::NoValueSerialize;
    use crate::api::key::ValueSerialize;
    use crate::impls::cache::DiceTaskRef;
    use crate::impls::cache::SharedCache;
    use crate::impls::key::DiceKey;
    use crate::impls::task::dice::DiceTask;
    use crate::impls::task::spawn_dice_task;
    use crate::testing_helpers::make_completed_task;

    #[derive(Allocative, Clone, Debug, Display, Eq, PartialEq, Hash, Pagable)]
    #[pagable_typetag(DiceKeyDyn)]
    struct K;

    #[async_trait]
    impl Key for K {
        type Value = usize;

        async fn compute(
            &self,
            _ctx: &mut DiceComputations,
            _cancellations: &CancellationContext,
        ) -> Self::Value {
            unimplemented!("test")
        }

        fn equality(_: &Self::Value, _: &Self::Value) -> bool {
            true
        }

        fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
            NoValueSerialize::<Self::Value>::new()
        }
    }

    async fn make_finished_cancelling_task(key: DiceKey) -> DiceTask {
        let finished_cancelling_tasks = spawn_dice_task(key, &TokioSpawner, &(), |handle| {
            async move {
                let _handle = handle;
                futures::future::pending().await
            }
            .boxed()
        });
        finished_cancelling_tasks.cancel(CancellationReason::ByTest);

        finished_cancelling_tasks.await_termination().await;

        finished_cancelling_tasks
    }

    fn make_never_finish_yet_to_cancel_task(key: DiceKey) -> DiceTask {
        spawn_dice_task(key, &TokioSpawner, &(), |handle| {
            async move {
                let _handle = handle;
                futures::future::pending().await
            }
            .boxed()
        })
    }

    #[tokio::test]
    async fn test_drain_task() {
        let cache = SharedCache::new();

        let completed_task1 = make_completed_task::<K>(DiceKey { index: 10 }, 1).await;
        let completed_task2 = make_completed_task::<K>(DiceKey { index: 20 }, 2).await;

        let finished_cancelling_tasks1 = make_finished_cancelling_task(DiceKey { index: 30 }).await;
        let finished_cancelling_tasks2 = make_finished_cancelling_task(DiceKey { index: 40 }).await;

        let yet_to_cancel_tasks1 = make_never_finish_yet_to_cancel_task(DiceKey { index: 50 });
        let yet_to_cancel_tasks2 = make_never_finish_yet_to_cancel_task(DiceKey { index: 60 });
        let yet_to_cancel_tasks3 = make_never_finish_yet_to_cancel_task(DiceKey { index: 70 });

        cache
            .get(DiceKey { index: 1 })
            .testing_insert(completed_task1);
        cache
            .get(DiceKey { index: 2 })
            .testing_insert(completed_task2);
        cache
            .get(DiceKey { index: 3 })
            .testing_insert(finished_cancelling_tasks1);
        cache
            .get(DiceKey { index: 4 })
            .testing_insert(finished_cancelling_tasks2);
        cache
            .get(DiceKey { index: 5 })
            .testing_insert(yet_to_cancel_tasks1);
        cache
            .get(DiceKey { index: 6 })
            .testing_insert(yet_to_cancel_tasks2);
        cache
            .get(DiceKey { index: 7 })
            .testing_insert(yet_to_cancel_tasks3);

        let pending_tasks = cache.dupe().cancel_pending_tasks();

        assert_eq!(pending_tasks.len(), 3);

        assert!(matches!(
            cache.get(DiceKey { index: 999 }),
            DiceTaskRef::TransactionCancelled
        ));
    }
}
