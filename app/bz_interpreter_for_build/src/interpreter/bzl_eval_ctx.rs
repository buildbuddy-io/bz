/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::cell::RefCell;

use bz_core::bzl::ImportPath;

#[derive(Debug)]
pub struct BzlEvalCtx {
    pub(crate) bzl_path: ImportPath,
    bzl_visibility: RefCell<Option<Vec<String>>>,
}

impl BzlEvalCtx {
    pub(crate) fn new(bzl_path: ImportPath) -> Self {
        Self {
            bzl_path,
            bzl_visibility: RefCell::new(None),
        }
    }

    pub(crate) fn set_bzl_visibility(&self, visibility: Vec<String>) -> bz_error::Result<()> {
        let mut bzl_visibility = self.bzl_visibility.borrow_mut();
        if bzl_visibility.is_some() {
            return Err(BzlEvalError::VisibilityAlreadySet.into());
        }
        *bzl_visibility = Some(visibility);
        Ok(())
    }
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum BzlEvalError {
    #[error("load visibility may not be set more than once")]
    VisibilityAlreadySet,
}
