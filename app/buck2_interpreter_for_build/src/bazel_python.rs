/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;

use allocative::Allocative;
use fancy_regex::Regex;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::starlark_module;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum BazelPythonError {
    #[error("Invalid py_internal regex `{pattern}`: {error}")]
    InvalidRegex { pattern: String, error: String },
    #[error("Error matching py_internal regex `{pattern}`: {error}")]
    RegexMatch { pattern: String, error: String },
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelPyInternal;

impl fmt::Display for BazelPyInternal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("py_internal")
    }
}

starlark::starlark_simple_value!(BazelPyInternal);

#[starlark_value(type = "py_internal")]
impl<'v> StarlarkValue<'v> for BazelPyInternal {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_py_internal_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "get_current_os_name".to_owned(),
            "get_legacy_external_runfiles".to_owned(),
            "is_bzlmod_enabled".to_owned(),
            "regex_match".to_owned(),
        ]
    }
}

#[starlark_module]
fn bazel_py_internal_methods(builder: &mut MethodsBuilder) {
    fn regex_match<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        subject: &str,
        pattern: &str,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let normalized_pattern = pattern
            .strip_prefix("(?U)")
            .or_else(|| pattern.strip_prefix("(?u)"))
            .unwrap_or(pattern);
        let anchored = format!("^(?:{normalized_pattern})$");
        let regex = Regex::new(&anchored).map_err(|error| {
            buck2_error::Error::from(BazelPythonError::InvalidRegex {
                pattern: pattern.to_owned(),
                error: error.to_string(),
            })
        })?;
        regex
            .is_match(subject)
            .map_err(|error| BazelPythonError::RegexMatch {
                pattern: pattern.to_owned(),
                error: error.to_string(),
            })
            .map_err(|error| buck2_error::Error::from(error).into())
    }

    fn get_current_os_name(
        #[starlark(this)] _this: &BazelPyInternal,
    ) -> starlark::Result<&'static str> {
        Ok(match std::env::consts::OS {
            "macos" => "osx",
            "freebsd" => "freebsd",
            "openbsd" => "openbsd",
            "linux" => "linux",
            "windows" => "windows",
            _ => "unknown",
        })
    }

    fn get_legacy_external_runfiles<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(false)
    }

    fn is_bzlmod_enabled<'v>(
        #[starlark(this)] _this: &BazelPyInternal,
        #[starlark(default = NoneOr::None)] _ctx: NoneOr<Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(true)
    }
}

pub(crate) fn register_bazel_python_globals(builder: &mut GlobalsBuilder) {
    builder.set("py_internal", BazelPyInternal);
}
