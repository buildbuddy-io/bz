/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::slice;
use std::sync::Arc;

use allocative::Allocative;
use buck2_core::provider::id::ProviderId;
use dupe::Dupe;
use pagable::Pagable;
use strong_hash::StrongHash;

pub type ProviderIdGroup = Arc<Vec<Arc<ProviderId>>>;

#[derive(
    Debug, Eq, PartialEq, Hash, StrongHash, Clone, Dupe, Allocative, Pagable
)]
pub struct ProviderIdSet(Option<Arc<Vec<ProviderIdGroup>>>);

impl ProviderIdSet {
    pub const EMPTY: ProviderIdSet = ProviderIdSet(None);

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.provider_groups().is_empty()
    }

    #[inline]
    pub fn provider_groups(&self) -> &[ProviderIdGroup] {
        match &self.0 {
            None => &[],
            Some(groups) => groups,
        }
    }

    #[inline]
    pub fn providers(&self) -> &[Arc<ProviderId>] {
        match &self.0 {
            None => &[],
            Some(groups) if groups.len() == 1 => &groups[0],
            Some(_) => &[],
        }
    }

    pub fn any_of(groups: Vec<Vec<Arc<ProviderId>>>) -> Self {
        let mut groups = groups
            .into_iter()
            .map(|mut group| {
                group.sort_unstable();
                group.dedup();
                group
            })
            .collect::<Vec<_>>();
        if groups.iter().any(Vec::is_empty) {
            return ProviderIdSet::EMPTY;
        }
        groups.sort_unstable();
        groups.dedup();
        if groups.is_empty() {
            ProviderIdSet::EMPTY
        } else {
            ProviderIdSet(Some(Arc::new(groups.into_iter().map(Arc::new).collect())))
        }
    }
}

impl From<Vec<Arc<ProviderId>>> for ProviderIdSet {
    #[inline]
    fn from(v: Vec<Arc<ProviderId>>) -> Self {
        ProviderIdSet::any_of(vec![v])
    }
}

impl<'a> IntoIterator for &'a ProviderIdSet {
    type Item = &'a Arc<ProviderId>;
    type IntoIter = slice::Iter<'a, Arc<ProviderId>>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.providers().iter()
    }
}
