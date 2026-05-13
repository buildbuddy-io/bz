# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
# You may select, at your option, one of the above-listed licenses.

def _collect_files(values):
    files = []
    for value in values:
        files.extend(value.files.to_list())
    return files

def _join_paths(files):
    return " ".join([file.path for file in files])

def _normalize_output_name(name):
    if name.startswith(":") and len(name) > 1:
        return name[1:]
    return name

def _rule_output_dir(ctx, outs, output_names):
    if outs and output_names:
        output_name = _normalize_output_name(output_names[0])
        suffix = "/" + output_name
        if outs[0].path.endswith(suffix):
            return outs[0].path[:-len(suffix)]

    if ctx.label.package:
        return ctx.bin_dir.path + "/" + ctx.label.package
    return ctx.bin_dir.path

def _expand_make_variables(ctx, command, srcs, outs, output_names):
    out_paths = _join_paths(outs)
    src_paths = _join_paths(srcs)
    substitutions = {
        "BINDIR": ctx.bin_dir.path,
        "GENDIR": ctx.genfiles_dir.path,
        "OUTS": out_paths,
        "SRCS": src_paths,
    }

    if "$@" in command:
        if len(outs) != 1:
            fail("genrule `$@` expansion requires exactly one output, got {}".format(len(outs)))
        substitutions["@"] = outs[0].path

    if "$<" in command:
        if len(srcs) != 1:
            fail("genrule `$<` expansion requires exactly one source, got {}".format(len(srcs)))
        substitutions["<"] = srcs[0].path

    if "$(@D)" in command or "$(RULEDIR)" in command:
        if len(outs) == 0:
            fail("genrule output directory expansion requires at least one output")
        rule_dir = _rule_output_dir(ctx, outs, output_names)
        at_d = outs[0].dirname if len(outs) == 1 else rule_dir
        substitutions["@D"] = at_d
        substitutions["RULEDIR"] = rule_dir

    if ctx.attr.stamp == 1:
        command = command.replace("bazel-out/stable-status.txt", ctx.info_file.path)
        command = command.replace("bazel-out/volatile-status.txt", ctx.version_file.path)

    return ctx.expand_make_variables("cmd", command, substitutions)

def _dollar_escape_placeholder(command):
    placeholder = "__BUCK_BAZEL_GENRULE_DOLLAR__"
    for _ in range(100):
        if placeholder not in command:
            return placeholder
        placeholder = "_" + placeholder
    fail("could not find a genrule dollar escape placeholder that is absent from command")

def _selected_command(ctx):
    if ctx.attr.cmd_bash:
        return ctx.attr.cmd_bash
    if ctx.attr.cmd:
        return ctx.attr.cmd
    if ctx.attr.cmd_bat or ctx.attr.cmd_ps:
        fail("genrule cmd_bat/cmd_ps are only selected for Windows execution")
    fail("missing value for `cmd` attribute, you can also set `cmd_bash` on non-Windows platforms")

def _bazel_genrule_impl(ctx):
    outs = ctx.outputs.outs
    if len(outs) == 0:
        fail("genrule requires at least one output")
    if ctx.attr.executable and len(outs) != 1:
        fail("genrule(executable = True) requires exactly one output")

    srcs = _collect_files(ctx.attr.srcs)
    tools = _collect_files(ctx.attr.tools) + _collect_files(ctx.attr.exec_tools) + _collect_files(ctx.attr.toolchains)

    command = _selected_command(ctx)
    dollar_escape_placeholder = _dollar_escape_placeholder(command)
    command = command.replace("$$", dollar_escape_placeholder)
    command = ctx.expand_location(
        command,
        ctx.attr.srcs + ctx.attr.tools + ctx.attr.exec_tools + ctx.attr.toolchains,
    )
    command = _expand_make_variables(ctx, command, srcs, outs, ctx.attr.outs)
    command = command.replace(dollar_escape_placeholder, "$")

    inputs = srcs
    if ctx.attr.stamp == 1:
        inputs = inputs + [ctx.info_file, ctx.version_file]

    ctx.actions.run_shell(
        command = command,
        inputs = depset(inputs),
        tools = depset(tools),
        outputs = outs,
        mnemonic = "Genrule",
    )

    files = depset(outs)
    if ctx.attr.executable:
        return [DefaultInfo(files = files, executable = outs[0])]
    return [DefaultInfo(files = files)]

bazel_genrule = rule(
    implementation = _bazel_genrule_impl,
    attrs = {
        "cmd": attr.string(default = ""),
        "cmd_bash": attr.string(default = ""),
        "cmd_bat": attr.string(default = ""),
        "cmd_ps": attr.string(default = ""),
        "data": attr.label_list(allow_files = True),
        "exec_properties": attr.string_dict(default = {}),
        "exec_tools": attr.label_list(allow_files = True, cfg = "exec"),
        "executable": attr.bool(default = False),
        "heuristic_label_expansion": attr.bool(default = False),
        "local": attr.bool(default = False),
        "message": attr.string(default = ""),
        "output_licenses": attr.string_list(default = []),
        "output_to_bindir": attr.bool(default = False),
        "outs": attr.output_list(mandatory = True),
        "stamp": attr.int(default = 0, values = [-1, 0, 1]),
        "srcs": attr.label_list(allow_files = True),
        "toolchains": attr.label_list(allow_files = False),
        "tools": attr.label_list(allow_files = True, cfg = "exec"),
    },
)
