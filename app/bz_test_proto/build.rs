/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::env;
use std::io;

fn main() -> io::Result<()> {
    let proto_files = &["test.proto"];

    let buck_proto_srcs = env::var("BUCK_PROTO_SRCS");
    let includes = if let Ok(path) = &buck_proto_srcs {
        vec![path.as_str()]
    } else {
        vec![".", "../bz_data", "../bz_host_sharing_proto"]
    };

    let builder = bz_protoc_dev::configure();
    unsafe { builder.setup_protoc() }
        .type_attribute(
            "buck.test.ExecuteResponse2.response",
            "#[allow(clippy::large_enum_variant)]",
        )
        .extern_path(".buck.data", "::bz_data")
        .extern_path(".buck.host_sharing", "::bz_host_sharing_proto")
        .compile(proto_files, &includes)
}
