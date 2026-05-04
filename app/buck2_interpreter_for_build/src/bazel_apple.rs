/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::FrozenValue;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::starlark_value;
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

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelAppleToolchain;

impl fmt::Display for BazelAppleToolchain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("apple_toolchain")
    }
}

starlark::starlark_simple_value!(BazelAppleToolchain);

#[starlark_value(type = "apple_toolchain")]
impl<'v> StarlarkValue<'v> for BazelAppleToolchain {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_apple_toolchain_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "developer_dir".to_owned(),
            "platform_developer_framework_dir".to_owned(),
            "sdk_dir".to_owned(),
        ]
    }
}

#[starlark_module]
fn bazel_apple_toolchain_methods(builder: &mut MethodsBuilder) {
    fn sdk_dir(#[starlark(this)] _this: &BazelAppleToolchain) -> starlark::Result<&'static str> {
        Ok("__BAZEL_XCODE_SDKROOT__")
    }

    fn developer_dir(
        #[starlark(this)] _this: &BazelAppleToolchain,
    ) -> starlark::Result<&'static str> {
        Ok("__BAZEL_XCODE_DEVELOPER_DIR__")
    }

    fn platform_developer_framework_dir<'v>(
        #[starlark(this)] _this: &BazelAppleToolchain,
        apple_fragment: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let platform = apple_fragment.get_attr_error("single_arch_platform", eval.heap())?;
        let name_in_plist = platform.get_attr_error("name_in_plist", eval.heap())?;
        let name_in_plist = name_in_plist.unpack_str().unwrap_or("MacOSX");
        Ok(format!(
            "__BAZEL_XCODE_DEVELOPER_DIR__/Platforms/{name_in_plist}.platform/Developer/Library/Frameworks"
        ))
    }
}

#[starlark_module]
fn bazel_apple_common_module(builder: &mut GlobalsBuilder) {
    fn apple_toolchain() -> starlark::Result<BazelAppleToolchain> {
        Ok(BazelAppleToolchain)
    }
}

pub(crate) fn register_bazel_apple_common(builder: &mut GlobalsBuilder) {
    builder.namespace("apple_common", |apple| {
        bazel_apple_common_module(apple);

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
