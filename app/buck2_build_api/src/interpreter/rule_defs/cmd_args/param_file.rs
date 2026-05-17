/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use super::ParamFileFormat;

pub fn visit_bazel_param_file_content(
    args: impl IntoIterator<Item = impl AsRef<str>>,
    format: ParamFileFormat,
    mut visitor: impl FnMut(&[u8]),
) {
    for arg in args {
        let arg = arg.as_ref();
        match format {
            ParamFileFormat::Shell => {
                visitor(bazel_shell_escape(arg).as_bytes());
            }
            ParamFileFormat::GccQuoted => {
                visitor(bazel_gcc_param_file_escape(arg).as_bytes());
            }
            ParamFileFormat::Windows => {
                visitor(bazel_windows_param_file_escape(arg).as_bytes());
            }
            ParamFileFormat::Multiline | ParamFileFormat::FlagPerLine => {
                visitor(arg.as_bytes());
            }
        }
        visitor(b"\n");
    }
}

pub fn bazel_param_file_content(args: Vec<String>, format: ParamFileFormat) -> Vec<u8> {
    let mut content = Vec::new();
    visit_bazel_param_file_content(args, format, |bytes| content.extend_from_slice(bytes));
    content
}

fn bazel_shell_escape(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_owned();
    }

    if arg.bytes().all(bazel_shell_safe_char) {
        return arg.to_owned();
    }

    if !arg.starts_with('~')
        && arg
            .bytes()
            .all(|byte| bazel_shell_safe_char(byte) || byte == b'~')
    {
        return arg.to_owned();
    }

    let mut escaped = String::with_capacity(arg.len() + 2);
    escaped.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

fn bazel_shell_safe_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'@' | b'%' | b'-' | b'_' | b'+' | b':' | b',' | b'.' | b'/'
        )
}

fn bazel_gcc_param_file_escape(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_owned();
    }

    let mut escaped = String::with_capacity(arg.len());
    for ch in arg.chars() {
        if matches!(
            ch,
            '\'' | '"' | '\\' | ' ' | '\t' | '\r' | '\n' | '\x0c' | '\x0b'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn bazel_windows_param_file_escape(arg: &str) -> String {
    let needs_quotes = arg.chars().any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r'));
    let escaped = arg.replace('"', "\\\"");
    if needs_quotes {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}
