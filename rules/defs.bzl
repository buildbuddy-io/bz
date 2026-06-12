# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//decls:common.bzl", "buck")
load("@prelude//os_lookup:defs.bzl", "Os", "OsLookup")

def _bz_bundle_impl(ctx: AnalysisContext) -> list[Provider]:
    """
    Produce a directory layout that is similar to the one our release binary
    uses, this allows setting a path for Tpx relative to the bz binary directory.
    """
    target_is_windows = ctx.attrs._target_os_type[OsLookup].os == Os("windows")

    binary_extension = ".exe" if target_is_windows else ""
    bz_binary = "bz" + binary_extension
    bz_tpx_binary = "bz-tpx" + binary_extension
    bz_daemon_binary = "bz-daemon" + binary_extension
    bz_health_check_binary = "bz-health-check" + binary_extension

    copied_dir = {}
    materialisations = []

    bz = ctx.attrs.bz[DefaultInfo].default_outputs[0]
    copied_dir[bz_daemon_binary] = bz
    materialisations.extend(ctx.attrs.bz[DefaultInfo].other_outputs)

    bz_client = ctx.attrs.bz_client[DefaultInfo].default_outputs[0]
    copied_dir[bz_binary] = bz_client
    materialisations.extend(ctx.attrs.bz_client[DefaultInfo].other_outputs)

    if ctx.attrs.bz_health_check:
        bz_health_check = ctx.attrs.bz_health_check[DefaultInfo].default_outputs[0]
        copied_dir[bz_health_check_binary] = bz_health_check
        materialisations.extend(ctx.attrs.bz_health_check[DefaultInfo].other_outputs)

    if ctx.attrs.tpx:
        tpx = ctx.attrs.tpx[DefaultInfo].default_outputs[0]
        copied_dir[bz_tpx_binary] = ctx.actions.symlink_file(bz_tpx_binary, tpx, has_content_based_path = False)
        materialisations.extend(ctx.attrs.tpx[DefaultInfo].other_outputs)

    out = ctx.actions.copied_dir("out", copied_dir, has_content_based_path = False)

    return [DefaultInfo(out, other_outputs = materialisations), RunInfo(cmd_args(out.project("bz" + binary_extension), hidden = materialisations))]

_bz_bundle = rule(
    impl = _bz_bundle_impl,
    attrs = {
        "bz": attrs.dep(),
        "bz_client": attrs.dep(),
        "bz_health_check": attrs.option(attrs.dep(), default = None),
        "labels": attrs.list(attrs.string(), default = []),
        "tpx": attrs.option(attrs.dep(), default = None),
        "_target_os_type": buck.target_os_type_arg(),
    },
)

def bz_bundle(bz, bz_client, bz_health_check, tpx, **kwargs):
    _bz_bundle(
        bz = bz,
        bz_client = bz_client,
        bz_health_check = bz_health_check,
        tpx = tpx,
        **kwargs
    )

def _pagable_transition_impl(platform: PlatformInfo, refs: struct) -> PlatformInfo:
    val = refs.val[ConstraintValueInfo]
    new_cfg = ConfigurationInfo(
        constraints = platform.configuration.constraints | {val.setting.label: val},
        values = platform.configuration.values,
    )
    return PlatformInfo(
        label = platform.label,
        configuration = new_cfg,
    )

_pagable_transition = transition(
    impl = _pagable_transition_impl,
    refs = {
        "val": "//deps/starlark-rust/starlark:pagable[enabled]",
    },
)

def _pagable_alias_impl(ctx: AnalysisContext) -> list[Provider]:
    return ctx.attrs.actual.providers

_pagable_transition_alias = rule(
    impl = _pagable_alias_impl,
    attrs = {
        "actual": attrs.dep(),
        "labels": attrs.list(attrs.string(), default = []),
    },
    cfg = _pagable_transition,
)

def pagable_transition_alias(name: str, actual, labels):
    _pagable_transition_alias(
        name = name,
        actual = actual,
        labels = labels,
    )
