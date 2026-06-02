# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Avoid some copy-paste
def _app(s):
    return "//bz/app/" + s.replace("buck2", "bz") + ":" + s

# These crates should only implement late bindings and not be depended on
# directly
LATE_BINDING_ONLY_CRATES = [
    _app("bz_anon_target"),
    _app("bz_cmd_audit_server"),
    _app("bz_cmd_query_server"),
    _app("bz_cmd_targets_server"),
    _app("bz_bxl"),
    _app("bz_query_impls"),
]

# These crates may only be depended on from `app/bz`
TOP_LEVEL_ONLY_CRATES = [
    _app("bz_cmd_debug_client"),
    _app("bz_cmd_log_client"),
]

# Unordered pairs where neither crate may depend on the other
BANNED_DEP_PATHS = [
    (_app("bz_common"), _app("bz_directory")),
    (_app("bz_common"), "//bz/starlark-rust/starlark:starlark"),
    (_app("bz_build_api"), _app("bz_execute_impl")),
    (_app("bz_build_api"), _app("bz_interpreter_for_build")),
    (_app("bz_server"), _app("bz_server_commands")),
    (_app("bz_bxl"), _app("bz_configured")),
]
