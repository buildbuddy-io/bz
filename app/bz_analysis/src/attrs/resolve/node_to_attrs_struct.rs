/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_node::attrs::inspect_options::AttrInspectOptions;
use bz_node::nodes::configured::ConfiguredTargetNodeRef;
use starlark::values::ValueOfUnchecked;
use starlark::values::structs::AllocStruct;
use starlark::values::structs::StructRef;

use crate::attrs::resolve::configured_attr::ConfiguredAttrExt;
use crate::attrs::resolve::ctx::AttrResolutionContext;

/// Prepare `ctx.attrs` for rule impl.
pub(crate) fn node_to_attrs_struct<'v>(
    node: ConfiguredTargetNodeRef,
    ctx: &mut dyn AttrResolutionContext<'v>,
) -> bz_error::Result<ValueOfUnchecked<'v, StructRef<'static>>> {
    let attrs_iter = node.attrs(AttrInspectOptions::All);
    let mut resolved_attrs = Vec::with_capacity(attrs_iter.size_hint().0);
    let is_bazel_rule = node.is_bazel_rule();
    for a in attrs_iter {
        let value = if is_bazel_rule {
            a.value.resolve_bazel(node.label().pkg(), ctx)?
        } else {
            a.value.resolve_single(node.label().pkg(), ctx)?
        };
        resolved_attrs.push((a.name, value));
    }
    Ok(ctx
        .heap()
        .alloc_typed_unchecked(AllocStruct(resolved_attrs))
        .cast())
}
