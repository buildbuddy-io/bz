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
    let proto_files = &["daemon.proto"];

    let buck_proto_srcs = env::var("BUCK_PROTO_SRCS");
    let includes = if let Ok(path) = &buck_proto_srcs {
        vec![path.as_str()]
    } else {
        vec![
            ".",
            "../bz_data",
            "../bz_subscription_proto",
            "../bz_host_sharing_proto",
        ]
    };

    let builder = bz_protoc_dev::configure();
    unsafe { builder.setup_protoc() }
        .type_attribute(".", "#[derive(::serde::Serialize, ::serde::Deserialize)] #[serde(rename_all = \"snake_case\")]")
        .type_attribute(".", "#[derive(::allocative::Allocative)]")
        .field_attribute("start_time", "#[serde(with = \"serialize_timestamp\")]")
        .field_attribute("timeout", "#[serde(rename = \"timeout_us\", with = \"bz_data::serialize_duration_as_micros\")]")
        .field_attribute("uptime", "#[serde(rename = \"uptime_us\", with = \"bz_data::serialize_duration_as_micros\")]")
        .field_attribute("delay", "#[serde(rename = \"delay_us\", with = \"bz_data::serialize_duration_as_micros\")]")
        .field_attribute("ProfileResponse.elapsed", "#[serde(rename = \"elapsed_us\", with = \"bz_data::serialize_duration_as_micros\")]")
        .boxed("CommandProgress.progress.event")
        .boxed("CommandProgress.progress.result")
        .boxed("CommandProgress.progress.partial_result")
        .field_attribute("expires_at", "#[serde(with = \"serialize_timestamp\")]")
        .extern_path(".buck.data", "::bz_data")
        .extern_path(".buck.subscription", "::bz_subscription_proto")
        .compile(proto_files, &includes)
}
