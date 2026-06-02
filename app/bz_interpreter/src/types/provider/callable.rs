/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt::Debug;
use std::sync::Arc;

use buck2_core::provider::id::ProviderId;
use starlark::any::ProvidesStaticType;
use starlark::values::Value;
use starlark::values::ValueLike;

pub trait ProviderCallableLike {
    fn id(&self) -> buck2_error::Result<&Arc<ProviderId>>;
}

unsafe impl<'v> ProvidesStaticType<'v> for &'v dyn ProviderCallableLike {
    type StaticType = &'static dyn ProviderCallableLike;
}

/// Implemented by providers (builtin or user defined).
pub trait ProviderLike<'v>: Debug {
    /// The ID. Guaranteed to be set on the `ProviderCallable` before constructing this object.
    fn id(&self) -> &Arc<ProviderId>;

    /// Returns a list of all the keys and values.
    // TODO(cjhopman): I'd rather return an iterator. I couldn't get that to work, though.
    fn items(&self) -> Vec<(&str, Value<'v>)>;
}

unsafe impl<'v> ProvidesStaticType<'v> for &'v dyn ProviderLike<'v> {
    type StaticType = &'static dyn ProviderLike<'static>;
}

pub trait ValueAsProviderCallableLike<'v> {
    fn as_provider_callable(&self) -> Option<&'v dyn ProviderCallableLike>;
}

impl<'v, V: ValueLike<'v>> ValueAsProviderCallableLike<'v> for V {
    fn as_provider_callable(&self) -> Option<&'v dyn ProviderCallableLike> {
        self.to_value().request_value::<&dyn ProviderCallableLike>()
    }
}
