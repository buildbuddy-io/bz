def cc_toolchain_impl(ctx):
    return [
        DefaultInfo(),
        cc_common.CcToolchainInfo(
            all_files = depset(),
            ar_executable = "ar",
            compiler = ctx.attrs.compiler or "",
            target_gnu_system_name = "",
            toolchain_identifier = ctx.attrs.toolchain_identifier or "",
        ),
    ]

def cc_toolchain_suite_impl(ctx):
    _ = ctx.attrs.toolchains
    return [DefaultInfo()]
