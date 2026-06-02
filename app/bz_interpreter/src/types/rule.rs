/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_util::late_binding::LateBinding;
use starlark::values::FrozenStringValue;
use starlark::values::FrozenValue;
use starlark_map::small_map::SmallMap;

#[derive(Clone, Debug)]
pub struct FrozenBazelAspectInfo {
    pub implementation: FrozenValue,
    pub attr_aspects: Vec<String>,
    pub required_providers: Vec<FrozenValue>,
    pub required_aspect_providers: Vec<FrozenValue>,
    pub requires: Vec<FrozenValue>,
    pub attrs: Vec<String>,
    pub toolchains: Vec<FrozenValue>,
}

/// `rule()`, `anon_rule()`, `bxl.anon_rule()` value `impl` field.
pub static FROZEN_RULE_GET_IMPL: LateBinding<fn(FrozenValue) -> bz_error::Result<FrozenValue>> =
    LateBinding::new("FROZEN_RULE_GET_IMPL");

pub static FROZEN_PROMISE_ARTIFACT_MAPPINGS_GET_IMPL: LateBinding<
    fn(FrozenValue) -> bz_error::Result<SmallMap<FrozenStringValue, FrozenValue>>,
> = LateBinding::new("FROZEN_PROMISE_ARTIFACT_MAPPINGS_GET_IMPL");

pub static FROZEN_BAZEL_ASPECTS_GET_IMPL: LateBinding<
    fn(FrozenValue) -> bz_error::Result<Vec<FrozenValue>>,
> = LateBinding::new("FROZEN_BAZEL_ASPECTS_GET_IMPL");

pub static FROZEN_BAZEL_ATTR_ASPECTS_GET_IMPL: LateBinding<
    fn(FrozenValue) -> bz_error::Result<SmallMap<String, Vec<FrozenValue>>>,
> = LateBinding::new("FROZEN_BAZEL_ATTR_ASPECTS_GET_IMPL");

pub static FROZEN_BAZEL_ASPECT_INFO_GET_IMPL: LateBinding<
    fn(FrozenValue) -> bz_error::Result<FrozenBazelAspectInfo>,
> = LateBinding::new("FROZEN_BAZEL_ASPECT_INFO_GET_IMPL");

pub const BAZEL_ASPECT_HIDDEN_ATTR_PREFIX: &str = "_bz_bazel_aspect_";

pub fn bazel_aspect_hidden_attr_name(
    rule_attr: &str,
    aspect_path: &str,
    aspect_attr: &str,
) -> String {
    fn push_sanitized(out: &mut String, value: &str) {
        for c in value.chars() {
            if c == '_' || c.is_ascii_alphanumeric() {
                out.push(c);
            } else {
                out.push('_');
            }
        }
    }

    let mut name = BAZEL_ASPECT_HIDDEN_ATTR_PREFIX.to_owned();
    push_sanitized(&mut name, rule_attr);
    name.push('_');
    push_sanitized(&mut name, aspect_path);
    name.push('_');
    push_sanitized(&mut name, aspect_attr);
    name
}

pub fn is_bazel_aspect_hidden_attr(name: &str) -> bool {
    name.starts_with(BAZEL_ASPECT_HIDDEN_ATTR_PREFIX)
}
