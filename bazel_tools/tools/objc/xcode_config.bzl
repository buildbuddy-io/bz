def _version_or_default(value, default):
    if value:
        return str(value)
    return default

def _xcode_config_impl(ctx):
    apple_fragment = ctx.fragments.apple
    macos_minimum_os = _version_or_default(apple_fragment.macos_minimum_os_flag, "0.0")
    macos_sdk_version = macos_minimum_os

    return [apple_common.XcodeVersionConfig(
        ios_sdk_version = "0.0",
        ios_minimum_os_version = "0.0",
        visionos_sdk_version = "0.0",
        visionos_minimum_os_version = "0.0",
        watchos_sdk_version = "0.0",
        watchos_minimum_os_version = "0.0",
        tvos_sdk_version = "0.0",
        tvos_minimum_os_version = "0.0",
        macos_sdk_version = macos_sdk_version,
        macos_minimum_os_version = macos_minimum_os,
        xcode_version = None,
        availability = "UNKNOWN",
        xcode_version_flag = None,
        include_xcode_execution_info = False,
    )]

xcode_config = rule(
    implementation = _xcode_config_impl,
    fragments = ["apple"],
)
