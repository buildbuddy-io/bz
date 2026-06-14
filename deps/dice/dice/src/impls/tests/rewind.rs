/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Tests for `DiceComputations::rewind_keys`: discarding cached results at the
//! current version so that re-requests recompute, without committing a new
//! transaction.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use allocative::Allocative;
use async_trait::async_trait;
use derivative::Derivative;
use derive_more::Display;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;
use pagable::PagablePanic;
use pagable::pagable_typetag;
use tokio::sync::Semaphore;

use crate::Dice;
use crate::DiceKeyDyn;
use crate::api::computations::DiceComputations;
use crate::api::cycles::DetectCycles;
use crate::api::injected::InjectedKey;
use crate::api::key::Key;
use crate::api::key::NoValueSerialize;
use crate::api::key::ValueSerialize;

/// Returns the number of times it has been computed. Recomputing thus produces a
/// new, non-equal value (like an action whose re-execution re-creates external
/// state and is observable).
#[derive(Clone, Dupe, Debug, Derivative, Allocative, Display, PagablePanic)]
#[derivative(PartialEq, Eq, Hash)]
#[display("counting")]
#[allocative(skip)]
#[pagable_typetag(DiceKeyDyn)]
struct CountingKey {
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    computes: Arc<AtomicUsize>,
}

#[async_trait]
impl Key for CountingKey {
    type Value = usize;

    async fn compute(
        &self,
        _ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        self.computes.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Always returns the same value, but counts how many times the compute ran
/// (like a deterministic action whose re-execution is wanted only for its side
/// effects).
#[derive(Clone, Dupe, Debug, Derivative, Allocative, Display, PagablePanic)]
#[derivative(PartialEq, Eq, Hash)]
#[display("constant")]
#[allocative(skip)]
#[pagable_typetag(DiceKeyDyn)]
struct ConstantValueKey {
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    computes: Arc<AtomicUsize>,
}

/// Blocks until released, while returning the order in which each compute
/// started. This lets tests observe whether a request joined an existing
/// in-flight task or spawned a post-rewind task.
#[derive(Clone, Dupe, Debug, Derivative, Allocative, Display, PagablePanic)]
#[derivative(PartialEq, Eq, Hash)]
#[display("blocking")]
#[allocative(skip)]
#[pagable_typetag(DiceKeyDyn)]
struct BlockingKey {
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    computes: Arc<AtomicUsize>,
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    started: Arc<Semaphore>,
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    release: Arc<Semaphore>,
}

#[async_trait]
impl Key for BlockingKey {
    type Value = usize;

    async fn compute(
        &self,
        _ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let compute_id = self.computes.fetch_add(1, Ordering::SeqCst) + 1;
        self.started.add_permits(1);
        let _permit = self.release.acquire().await.unwrap();
        compute_id
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[async_trait]
impl Key for ConstantValueKey {
    type Value = usize;

    async fn compute(
        &self,
        _ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        self.computes.fetch_add(1, Ordering::SeqCst);
        42
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Computes the dep and counts its own computes, so tests can observe whether a
/// dependent re-ran after its dep was rewound.
#[derive(Clone, Dupe, Debug, Derivative, Allocative, Display, PagablePanic)]
#[derivative(PartialEq, Eq, Hash)]
#[display("dependent")]
#[allocative(skip)]
#[pagable_typetag(DiceKeyDyn)]
struct DependentKey {
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    dep: CountingKey,
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    computes: Arc<AtomicUsize>,
}

#[async_trait]
impl Key for DependentKey {
    type Value = usize;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        self.computes.fetch_add(1, Ordering::SeqCst);
        ctx.compute(&self.dep).await.unwrap()
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// The core recovery shape: a consumer that, mid-compute, finds its dep's value
/// stale, rewinds it, and re-requests it within the same compute.
#[derive(Clone, Dupe, Debug, Derivative, Allocative, Display, PagablePanic)]
#[derivative(PartialEq, Eq, Hash)]
#[display("rewinding-consumer")]
#[allocative(skip)]
#[pagable_typetag(DiceKeyDyn)]
struct RewindingConsumerKey {
    #[derivative(Hash = "ignore", PartialEq = "ignore")]
    dep: CountingKey,
}

#[async_trait]
impl Key for RewindingConsumerKey {
    type Value = (usize, usize);

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let before = ctx.compute(&self.dep).await.unwrap();
        let rewound = ctx.rewind_keys([self.dep.dupe()]).await;
        assert_eq!(rewound, 1);
        let after = ctx.compute(&self.dep).await.unwrap();
        (before, after)
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Dupe, Debug, Display, Eq, Hash, PartialEq, Allocative, PagablePanic)]
#[display("{:?}", self)]
#[pagable_typetag(DiceKeyDyn)]
struct Injected(i32);

#[async_trait]
impl InjectedKey for Injected {
    type Value = i32;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[tokio::test]
async fn rewind_forces_recompute_at_same_version() -> anyhow::Result<()> {
    let dice = Dice::builder().build(DetectCycles::Disabled);
    let key = CountingKey {
        computes: Arc::new(AtomicUsize::new(0)),
    };

    let mut ctx = dice.updater().commit().await;

    assert_eq!(ctx.compute(&key).await?, 1);
    // Cached: no recompute.
    assert_eq!(ctx.compute(&key).await?, 1);
    assert_eq!(key.computes.load(Ordering::SeqCst), 1);

    assert_eq!(ctx.rewind_keys([key.dupe()]).await, 1);

    // Recomputed at the same version, in the same transaction.
    assert_eq!(ctx.compute(&key).await?, 2);
    assert_eq!(ctx.compute(&key).await?, 2);
    assert_eq!(key.computes.load(Ordering::SeqCst), 2);

    // Rewinding again recomputes again.
    assert_eq!(ctx.rewind_keys([key.dupe()]).await, 1);
    assert_eq!(ctx.compute(&key).await?, 3);

    Ok(())
}

#[tokio::test]
async fn rewind_within_a_computing_key() -> anyhow::Result<()> {
    let dice = Dice::builder().build(DetectCycles::Disabled);
    let dep = CountingKey {
        computes: Arc::new(AtomicUsize::new(0)),
    };
    let consumer = RewindingConsumerKey { dep: dep.dupe() };

    let mut ctx = dice.updater().commit().await;

    // The consumer reads the dep, rewinds it mid-compute, and observes the
    // recomputed value on re-request.
    assert_eq!(ctx.compute(&consumer).await?, (1, 2));
    assert_eq!(dep.computes.load(Ordering::SeqCst), 2);

    Ok(())
}

#[tokio::test]
async fn rewind_detaches_pending_task_for_next_request() -> anyhow::Result<()> {
    let dice = Dice::builder().build(DetectCycles::Disabled);
    let key = BlockingKey {
        computes: Arc::new(AtomicUsize::new(0)),
        started: Arc::new(Semaphore::new(0)),
        release: Arc::new(Semaphore::new(0)),
    };

    let mut first_ctx = dice.updater().commit().await;
    let first_key = key.dupe();
    let first = tokio::spawn(async move { first_ctx.compute(&first_key).await });

    let _permit = key.started.acquire().await?;
    assert_eq!(key.computes.load(Ordering::SeqCst), 1);

    let mut second_ctx = dice.updater().commit().await;
    assert_eq!(second_ctx.rewind_keys([key.dupe()]).await, 1);

    let second_key = key.dupe();
    let second = tokio::spawn(async move { second_ctx.compute(&second_key).await });

    let _permit = key.started.acquire().await?;
    assert_eq!(key.computes.load(Ordering::SeqCst), 2);

    key.release.add_permits(2);

    assert_eq!(first.await??, 1);
    assert_eq!(second.await??, 2);

    Ok(())
}

#[tokio::test]
async fn rewind_dependents_revalidate_at_later_version() -> anyhow::Result<()> {
    let dice = Dice::builder().build(DetectCycles::Disabled);
    let dep = CountingKey {
        computes: Arc::new(AtomicUsize::new(0)),
    };
    let dependent = DependentKey {
        dep: dep.dupe(),
        computes: Arc::new(AtomicUsize::new(0)),
    };

    {
        let mut ctx = dice.updater().commit().await;
        assert_eq!(ctx.compute(&dependent).await?, 1);

        assert_eq!(ctx.rewind_keys([dep.dupe()]).await, 1);

        // The dep recomputes when re-requested...
        assert_eq!(ctx.compute(&dep).await?, 2);
        // ...but the dependent's already-produced result is not retracted at this
        // version: it stays cached and keeps returning the pre-rewind value.
        assert_eq!(ctx.compute(&dependent).await?, 1);
        assert_eq!(dependent.computes.load(Ordering::SeqCst), 1);
    }

    // At the next version, the dependent re-validates against the recomputed dep
    // and recomputes because the dep's value changed.
    {
        let mut ctx = dice.updater().commit().await;
        assert_eq!(ctx.compute(&dependent).await?, 2);
        assert_eq!(dependent.computes.load(Ordering::SeqCst), 2);
        // The dep itself was already recomputed at the previous version; its fresh
        // value is reused here.
        assert_eq!(dep.computes.load(Ordering::SeqCst), 2);
    }

    Ok(())
}

#[tokio::test]
async fn rewind_recomputes_even_when_value_is_equal() -> anyhow::Result<()> {
    let dice = Dice::builder().build(DetectCycles::Disabled);
    let dep = ConstantValueKey {
        computes: Arc::new(AtomicUsize::new(0)),
    };

    let mut ctx = dice.updater().commit().await;
    assert_eq!(ctx.compute(&dep).await?, 42);
    assert_eq!(dep.computes.load(Ordering::SeqCst), 1);

    assert_eq!(ctx.rewind_keys([dep.dupe()]).await, 1);

    // The recompute runs (side effects re-executed) even though the value is
    // unchanged — that is the point of force-dirty semantics: dep-based reuse
    // would skip the side effects.
    assert_eq!(ctx.compute(&dep).await?, 42);
    assert_eq!(dep.computes.load(Ordering::SeqCst), 2);

    Ok(())
}

#[tokio::test]
async fn rewind_skips_injected_keys() -> anyhow::Result<()> {
    let dice = Dice::builder().build(DetectCycles::Disabled);

    let mut updater = dice.updater();
    updater.changed_to(vec![(Injected(0), 7)])?;
    let mut ctx = updater.commit().await;

    assert_eq!(ctx.compute(&Injected(0)).await?, 7);

    // Injected keys cannot recompute; the rewind skips them rather than wedging
    // or panicking.
    assert_eq!(ctx.rewind_keys([Injected(0)]).await, 0);
    assert_eq!(ctx.compute(&Injected(0)).await?, 7);

    Ok(())
}

#[tokio::test]
async fn rewind_never_computed_key_is_harmless() -> anyhow::Result<()> {
    let dice = Dice::builder().build(DetectCycles::Disabled);
    let key = CountingKey {
        computes: Arc::new(AtomicUsize::new(0)),
    };

    let mut ctx = dice.updater().commit().await;

    assert_eq!(ctx.rewind_keys([key.dupe()]).await, 1);
    assert_eq!(ctx.compute(&key).await?, 1);
    assert_eq!(key.computes.load(Ordering::SeqCst), 1);

    Ok(())
}
