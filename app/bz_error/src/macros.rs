/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt::Arguments;

#[doc(hidden)]
#[cold]
#[track_caller]
pub fn bz_error_impl(tag: crate::ErrorTag, args: Arguments) -> crate::Error {
    let caller = std::panic::Location::caller();
    let source_location = crate::source_location::SourceLocation::new(caller.file(), caller.line());
    crate::Error::new(format!("{args}"), tag, source_location, None)
}

#[doc(hidden)]
#[cold]
#[track_caller]
pub fn internal_error_impl(args: Arguments) -> crate::Error {
    bz_error_impl(
        crate::ErrorTag::InternalError,
        format_args!("{args} (internal error)"),
    )
}

#[macro_export]
macro_rules! bz_error {
    ($tags:expr, $format:expr) => {
        $crate::bz_error!($tags, $format,)
    };
    ($tags:expr, $format:expr, $($arg:tt)*) => {
        $crate::macros::bz_error_impl($tags, format_args!($format, $($arg)*))
    };
}

/// Indicates a bug in buck2.
#[macro_export]
macro_rules! internal_error {
    ($format:expr) => {
        $crate::internal_error!($format,)
    };
    ($format:expr , $($arg:tt)*) => {
        $crate::macros::internal_error_impl(format_args!($format, $($arg)*))
    };
}
