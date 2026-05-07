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

def _label_keys(label):
    package = label.package
    name = label.name
    keys = [":" + name]
    if package:
        canonical = "//{}:{}".format(package, name)
        keys.append(canonical)
        if package.split("/")[-1] == name:
            keys.append("//" + package)
    else:
        keys.append("//:" + name)

    repo_name = label.repo_name
    if repo_name and repo_name != "root":
        for key in keys[:]:
            if key.startswith("//"):
                keys.append("@{}{}".format(repo_name, key))
                keys.append("@@{}{}".format(repo_name, key))

    return keys

def _record_location_paths(location_paths, value):
    files = value.files.to_list()
    if not files:
        return

    if len(files) == 1:
        single = files[0].path
    else:
        single = None
    joined = _join_paths(files)

    if hasattr(value, "label"):
        for key in _label_keys(value.label):
            location_paths[key] = single
            location_paths[key + ".__locations"] = joined
    else:
        for file in files:
            location_paths[file.basename] = file.path
            location_paths[":" + file.basename] = file.path
            location_paths[file.short_path] = file.path

def _expand_location_macros(command, location_paths):
    for key, path in location_paths.items():
        if key.endswith(".__locations"):
            continue
        if path == None:
            continue
        for name in ["location", "execpath", "rootpath"]:
            command = command.replace("$({} {})".format(name, key), path)

    for key, paths in location_paths.items():
        if not key.endswith(".__locations"):
            continue
        label = key[:-len(".__locations")]
        for name in ["locations", "execpaths", "rootpaths"]:
            command = command.replace("$({} {})".format(name, label), paths)

    for name in ["location", "locations", "execpath", "execpaths", "rootpath", "rootpaths"]:
        if "$({} ".format(name) in command:
            fail("unresolved genrule `{}` macro in command: {}".format(name, command))

    return command

def _expand_make_variables(ctx, command, srcs, outs):
    out_paths = _join_paths(outs)
    src_paths = _join_paths(srcs)

    command = command.replace("$(SRCS)", src_paths)
    command = command.replace("$(OUTS)", out_paths)
    command = command.replace("$(BINDIR)", ctx.bin_dir.path)
    command = command.replace("$(GENDIR)", ctx.genfiles_dir.path)

    if "$@" in command:
        if len(outs) != 1:
            fail("genrule `$@` expansion requires exactly one output, got {}".format(len(outs)))
        command = command.replace("$@", outs[0].path)

    if "$<" in command:
        if len(srcs) != 1:
            fail("genrule `$<` expansion requires exactly one source, got {}".format(len(srcs)))
        command = command.replace("$<", srcs[0].path)

    if "$(@D)" in command or "$(RULEDIR)" in command:
        if len(outs) == 0:
            fail("genrule output directory expansion requires at least one output")
        output_dir = outs[0].dirname
        command = command.replace("$(@D)", output_dir)
        command = command.replace("$(RULEDIR)", output_dir)

    if ctx.attr.stamp == 1:
        command = command.replace("bazel-out/stable-status.txt", ctx.info_file.path)
        command = command.replace("bazel-out/volatile-status.txt", ctx.version_file.path)

    return command

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
    tools = _collect_files(ctx.attr.tools) + _collect_files(ctx.attr.exec_tools)

    location_paths = {}
    for value in ctx.attr.srcs:
        _record_location_paths(location_paths, value)
    for value in ctx.attr.tools:
        _record_location_paths(location_paths, value)
    for value in ctx.attr.exec_tools:
        _record_location_paths(location_paths, value)

    command = _selected_command(ctx)
    command = _expand_location_macros(command, location_paths)
    command = _expand_make_variables(ctx, command, srcs, outs)

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
