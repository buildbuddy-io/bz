/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use starlark::environment::GlobalsBuilder;
use starlark::values::FrozenValue;
use starlark::values::structs::AllocStruct;

fn apple_platform(
    globals: &GlobalsBuilder,
    name: &'static str,
    platform_type: &'static str,
    is_device: bool,
    name_in_plist: &'static str,
) -> AllocStruct<[(&'static str, FrozenValue); 4]> {
    AllocStruct([
        ("name", globals.alloc(name)),
        ("platform_type", globals.alloc(platform_type)),
        ("is_device", globals.alloc(is_device)),
        ("name_in_plist", globals.alloc(name_in_plist)),
    ])
}

pub(crate) fn register_bazel_apple_common(builder: &mut GlobalsBuilder) {
    builder.namespace("apple_common", |apple| {
        apple.namespace("platform_type", |platform_type| {
            platform_type.set("ios", "ios");
            platform_type.set("macos", "macos");
            platform_type.set("tvos", "tvos");
            platform_type.set("visionos", "visionos");
            platform_type.set("watchos", "watchos");
        });

        apple.namespace("platform", |platform| {
            platform.set(
                "ios_device",
                apple_platform(platform, "ios_device", "ios", true, "iPhoneOS"),
            );
            platform.set(
                "ios_simulator",
                apple_platform(platform, "ios_simulator", "ios", false, "iPhoneSimulator"),
            );
            platform.set(
                "macos",
                apple_platform(platform, "macos", "macos", true, "MacOSX"),
            );
            platform.set(
                "tvos_device",
                apple_platform(platform, "tvos_device", "tvos", true, "AppleTVOS"),
            );
            platform.set(
                "tvos_simulator",
                apple_platform(
                    platform,
                    "tvos_simulator",
                    "tvos",
                    false,
                    "AppleTVSimulator",
                ),
            );
            platform.set(
                "visionos_device",
                apple_platform(platform, "visionos_device", "visionos", true, "XROS"),
            );
            platform.set(
                "visionos_simulator",
                apple_platform(
                    platform,
                    "visionos_simulator",
                    "visionos",
                    false,
                    "XRSimulator",
                ),
            );
            platform.set(
                "watchos_device",
                apple_platform(platform, "watchos_device", "watchos", true, "WatchOS"),
            );
            platform.set(
                "watchos_simulator",
                apple_platform(
                    platform,
                    "watchos_simulator",
                    "watchos",
                    false,
                    "WatchSimulator",
                ),
            );
        });
    });
}
