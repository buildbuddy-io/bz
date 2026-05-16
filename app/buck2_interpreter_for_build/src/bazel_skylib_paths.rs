/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::starlark_value;
use starlark::values::tuple::AllocTuple;
use starlark::values::tuple::UnpackTuple;

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct BazelSkylibPaths;

impl fmt::Display for BazelSkylibPaths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("struct(...)")
    }
}

starlark::starlark_simple_value!(BazelSkylibPaths);

#[starlark_value(type = "struct")]
impl<'v> StarlarkValue<'v> for BazelSkylibPaths {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_skylib_paths_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        [
            "basename",
            "dirname",
            "is_absolute",
            "join",
            "normalize",
            "is_normalized",
            "relativize",
            "replace_extension",
            "split_extension",
            "starts_with",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct BazelRulesCcIsPathAbsolute;

impl fmt::Display for BazelRulesCcIsPathAbsolute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<function is_path_absolute>")
    }
}

starlark::starlark_simple_value!(BazelRulesCcIsPathAbsolute);

#[starlark_value(type = "function")]
impl<'v> StarlarkValue<'v> for BazelRulesCcIsPathAbsolute {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let positions = args.positions(eval.heap())?.collect::<Vec<_>>();
        let named = args.names_map()?;
        let path = if let [path] = positions.as_slice()
            && named.is_empty()
        {
            *path
        } else if positions.is_empty() && named.len() == 1 {
            let (name, path) = named
                .iter()
                .next()
                .expect("checked named argument map length");
            if name.as_str() != "path" {
                return Err(paths_error(format!(
                    "is_path_absolute() got unexpected named argument `{}`",
                    name.as_str()
                )));
            }
            *path
        } else {
            return Err(paths_error(
                "is_path_absolute() expects exactly one `path` argument".to_owned(),
            ));
        };
        let Some(path) = path.unpack_str() else {
            return Err(paths_error(format!(
                "is_path_absolute() expected str, got `{}`",
                path.get_type()
            )));
        };
        Ok(Value::new_bool(rules_cc_is_path_absolute(path)))
    }
}

fn paths_error(message: String) -> starlark::Error {
    buck2_error::buck2_error!(buck2_error::ErrorTag::Input, "{}", message).into()
}

fn paths_basename(path: &str) -> &str {
    path.rsplit_once('/')
        .map(|(_prefix, basename)| basename)
        .unwrap_or(path)
}

fn paths_dirname(path: &str) -> String {
    match path.rsplit_once('/') {
        None => String::new(),
        Some(("", _basename)) => "/".to_owned(),
        Some((prefix, _basename)) => prefix.trim_end_matches('/').to_owned(),
    }
}

fn paths_is_absolute(path: &str) -> bool {
    path.starts_with('/') || (path.len() > 2 && path.as_bytes()[1] == b':')
}

fn rules_cc_is_path_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'/' || bytes[2] == b'\\'))
}

fn paths_join(path: &str, others: impl IntoIterator<Item = String>) -> String {
    let mut result = path.to_owned();
    for path in others {
        if paths_is_absolute(&path) {
            result = path;
        } else if result.is_empty() || result.ends_with('/') {
            result.push_str(&path);
        } else {
            result.push('/');
            result.push_str(&path);
        }
    }
    result
}

fn paths_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }

    let initial_slashes = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let is_relative = initial_slashes == 0;

    let mut components = Vec::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if components.last().is_some_and(|last| *last != "..") {
                components.pop();
            } else if is_relative {
                components.push(component);
            }
        } else {
            components.push(component);
        }
    }

    let mut normalized = components.join("/");
    if !is_relative {
        normalized = format!("{}{}", "/".repeat(initial_slashes), normalized);
    }
    if normalized.is_empty() {
        ".".to_owned()
    } else {
        normalized
    }
}

fn paths_is_normalized(path: &str, look_for_same_level_references: bool) -> bool {
    enum State {
        Base,
        Separator,
        Dot,
        DotDot,
    }

    let mut state = State::Separator;
    for c in path.chars() {
        let is_separator = c == '/';
        match state {
            State::Base => {
                state = if is_separator {
                    State::Separator
                } else {
                    State::Base
                };
            }
            State::Separator => {
                state = if is_separator {
                    State::Separator
                } else if c == '.' {
                    State::Dot
                } else {
                    State::Base
                };
            }
            State::Dot => {
                state = if is_separator {
                    if look_for_same_level_references {
                        return false;
                    }
                    State::Separator
                } else if c == '.' {
                    State::DotDot
                } else {
                    State::Base
                };
            }
            State::DotDot => {
                if is_separator {
                    return false;
                }
                state = State::Base;
            }
        }
    }

    match state {
        State::Dot => !look_for_same_level_references,
        State::DotDot => false,
        State::Base | State::Separator => true,
    }
}

fn paths_relativize(path: &str, start: &str) -> starlark::Result<String> {
    let segments = paths_normalize(path)
        .split('/')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut start_segments = paths_normalize(start)
        .split('/')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if start_segments == ["."] {
        start_segments.clear();
    }
    let start_length = start_segments.len();

    if path.starts_with('/') != start.starts_with('/') || segments.len() < start_length {
        return Err(paths_error(format!(
            "Path `{path}` is not beneath `{start}`"
        )));
    }

    for (ancestor_segment, segment) in start_segments.iter().zip(segments.iter()) {
        if ancestor_segment != segment {
            return Err(paths_error(format!(
                "Path `{path}` is not beneath `{start}`"
            )));
        }
    }

    let length = segments.len() - start_length;
    let result_segments = if length == 0 {
        &segments[..]
    } else {
        &segments[segments.len() - length..]
    };
    Ok(result_segments.join("/"))
}

fn paths_split_extension(path: &str) -> (&str, &str) {
    let basename = paths_basename(path);
    let Some(last_dot_in_basename) = basename.rfind('.') else {
        return (path, "");
    };

    if last_dot_in_basename == 0 {
        return (path, "");
    }

    let dot_distance_from_end = basename.len() - last_dot_in_basename;
    path.split_at(path.len() - dot_distance_from_end)
}

fn paths_alloc_split_extension<'v>(path: &str, heap: Heap<'v>) -> Value<'v> {
    let (root, extension) = paths_split_extension(path);
    heap.alloc(AllocTuple([
        heap.alloc_str(root).to_value(),
        heap.alloc_str(extension).to_value(),
    ]))
}

fn paths_starts_with(path_a: &str, path_b: &str) -> bool {
    if path_b.is_empty() {
        return true;
    }
    let norm_a = paths_normalize(path_a);
    let norm_b = paths_normalize(path_b);
    if norm_b.len() > norm_a.len() || !norm_a.starts_with(&norm_b) {
        return false;
    }
    norm_a.len() == norm_b.len() || norm_a.as_bytes()[norm_b.len()] == b'/'
}

#[starlark_module]
fn bazel_skylib_paths_methods(builder: &mut MethodsBuilder) {
    fn basename(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path: &str,
    ) -> starlark::Result<String> {
        Ok(paths_basename(path).to_owned())
    }

    fn dirname(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path: &str,
    ) -> starlark::Result<String> {
        Ok(paths_dirname(path))
    }

    fn is_absolute(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path: &str,
    ) -> starlark::Result<bool> {
        Ok(paths_is_absolute(path))
    }

    fn join(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path: &str,
        #[starlark(args)] others: UnpackTuple<String>,
    ) -> starlark::Result<String> {
        Ok(paths_join(path, others.items))
    }

    fn normalize(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path: &str,
    ) -> starlark::Result<String> {
        Ok(paths_normalize(path))
    }

    fn is_normalized(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path: &str,
        #[starlark(default = true)] look_for_same_level_references: bool,
    ) -> starlark::Result<bool> {
        Ok(paths_is_normalized(path, look_for_same_level_references))
    }

    fn relativize(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path: &str,
        start: &str,
    ) -> starlark::Result<String> {
        paths_relativize(path, start)
    }

    fn replace_extension(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path: &str,
        new_extension: &str,
    ) -> starlark::Result<String> {
        Ok(format!("{}{}", paths_split_extension(path).0, new_extension))
    }

    fn split_extension<'v>(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(paths_alloc_split_extension(path, eval.heap()))
    }

    fn starts_with(
        #[starlark(this)] _this: &BazelSkylibPaths,
        path_a: &str,
        path_b: &str,
    ) -> starlark::Result<bool> {
        Ok(paths_starts_with(path_a, path_b))
    }
}
