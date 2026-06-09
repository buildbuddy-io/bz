def _filegroup_executable_check_impl(ctx):
    executable = ctx.attr.dep[DefaultInfo].files_to_run.executable
    if ctx.attr.should_have_executable and executable == None:
        fail("expected singleton filegroup executable")
    if not ctx.attr.should_have_executable and executable != None:
        fail("expected non-singleton filegroup to have no executable")
    return []

filegroup_executable_check = rule(
    implementation = _filegroup_executable_check_impl,
    attrs = {
        "dep": attr.label(),
        "should_have_executable": attr.bool(),
    },
)
