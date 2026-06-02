/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use async_trait::async_trait;
use bz_downward_api::DownwardApi;
use bz_hash::StdBuckHashMap;
use tracing::Level;

pub struct BuckTestDownwardApi;

#[async_trait]
impl DownwardApi for BuckTestDownwardApi {
    async fn console(&self, _level: Level, msg: String) -> bz_error::Result<()> {
        // TODO(brasselsprouts): use the level and hook it up with our superconsole
        eprintln!("{}", msg);
        Ok(())
    }

    async fn log(&self, _level: Level, _msg: String) -> bz_error::Result<()> {
        unimplemented!("TODO(bobyf)")
    }

    async fn external(&self, _data: StdBuckHashMap<String, String>) -> bz_error::Result<()> {
        unimplemented!("need buck event stream to implement")
    }
}
