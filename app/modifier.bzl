# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

load("@prelude//cfg/modifier:asserts.bzl", "verify_normalized_modifier", "verify_normalized_target")
load(
    "@prelude//cfg/modifier:types.bzl",
    "Modifier",  # @unused Used in type annotation
    "ModifiersMatch",
)

def _modifiers_match(
        matcher: dict[str, Modifier]) -> ModifiersMatch:
    for key, sub_modifier in matcher.items():
        if key != "DEFAULT":
            verify_normalized_target(key)
        verify_normalized_modifier(sub_modifier)

    matcher["_type"] = "ModifiersMatch"
    return matcher

modifiers = struct(
    # modifiers.match is deprecated for modifiers.conditional
    match = _modifiers_match,
    conditional = _modifiers_match,
)

def bz_modifiers():
    return []

def disable_bz_modifiers():
    return []
