/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::borrow::Cow;

use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::visibility::VisibilityPattern;
use buck2_node::visibility::VisibilityWithinViewBuilder;

pub(crate) enum NormalizedVisibilityPattern<'a> {
    Public,
    Private,
    Pattern(Cow<'a, str>),
}

pub(crate) fn normalize_visibility_pattern(pattern: &str) -> NormalizedVisibilityPattern<'_> {
    match pattern {
        VisibilityPattern::PUBLIC | "//visibility:public" => NormalizedVisibilityPattern::Public,
        "//visibility:private" => NormalizedVisibilityPattern::Private,
        _ => {
            if let Some(package) = pattern.strip_suffix(":__pkg__") {
                NormalizedVisibilityPattern::Pattern(Cow::Owned(format!("{package}:")))
            } else if let Some(package) = pattern.strip_suffix(":__subpackages__") {
                let pattern = if package.is_empty() {
                    "...".to_owned()
                } else if package.ends_with("//") {
                    format!("{package}...")
                } else {
                    format!("{package}/...")
                };
                NormalizedVisibilityPattern::Pattern(Cow::Owned(pattern))
            } else {
                NormalizedVisibilityPattern::Pattern(Cow::Borrowed(pattern))
            }
        }
    }
}

pub(crate) fn add_visibility_pattern(
    builder: &mut VisibilityWithinViewBuilder,
    ctx: &dyn AttrCoercionContext,
    pattern: &str,
) -> buck2_error::Result<()> {
    match normalize_visibility_pattern(pattern) {
        NormalizedVisibilityPattern::Public => builder.add_public(),
        NormalizedVisibilityPattern::Private => {}
        NormalizedVisibilityPattern::Pattern(pattern) => {
            builder.add(VisibilityPattern(ctx.coerce_target_pattern(&pattern)?));
        }
    }
    Ok(())
}
