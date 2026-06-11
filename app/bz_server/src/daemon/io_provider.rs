/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::sync::Arc;

use bz_common::cas_digest::CasDigestConfig;
use bz_common::io::IoProvider;
use bz_common::io::fs::FsIoProvider;
use bz_common::io::trace::TracingIoProvider;
use bz_common::legacy_configs::configs::LegacyBuckConfig;
use bz_core::fs::project::ProjectRoot;

pub async fn create_io_provider(
    _fb: fbinit::FacebookInit,
    project_fs: ProjectRoot,
    _root_config: &LegacyBuckConfig,
    cas_digest_config: CasDigestConfig,
    trace_io: bool,
    _use_eden_thrift_read: bool,
) -> bz_error::Result<Arc<dyn IoProvider>> {
    if trace_io {
        Ok(Arc::new(TracingIoProvider::new(Box::new(
            FsIoProvider::new(project_fs, cas_digest_config),
        ))))
    } else {
        Ok(Arc::new(FsIoProvider::new(project_fs, cas_digest_config)))
    }
}
