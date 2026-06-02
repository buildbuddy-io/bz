/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use once_cell::sync::Lazy;
use regex::Regex;

/// A key / value entry from --agent-context. Used to track agent intent,
/// retry state, and prior errors for SSR (Second Solve Rate) analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentContextEntry {
    pub key: String,
    pub value: String,
}

impl AgentContextEntry {
    pub fn to_proto(&self) -> buck2_data::AgentContextEntry {
        buck2_data::AgentContextEntry {
            key: self.key.clone(),
            value: self.value.clone(),
        }
    }
}

/// Parse a single `key=value` agent context entry.
/// Used as a clap `value_parser`.
pub fn parse_agent_context(value: &str) -> buck2_error::Result<AgentContextEntry> {
    const REGEX_TEXT: &str = "^[a-z][a-z0-9]*(_[a-z][a-z0-9]*)*$";
    static REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(REGEX_TEXT).unwrap());

    let (key, val) = value
        .split_once('=')
        .ok_or_else(|| AgentContextError::InvalidFormat(value.to_owned()))?;

    if !REGEX.is_match(key) {
        return Err(AgentContextError::InvalidKey(key.to_owned()).into());
    }

    Ok(AgentContextEntry {
        key: key.to_owned(),
        value: val.to_owned(),
    })
}

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
pub enum AgentContextError {
    #[error("Invalid agent-context format: `{0}`. Each entry must be a `key=value` pair.")]
    InvalidFormat(String),

    #[error("Invalid agent-context key: `{0}`. Keys must be snake_case identifiers.")]
    InvalidKey(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_entry() {
        assert_eq!(
            parse_agent_context("intent=build").unwrap(),
            AgentContextEntry {
                key: "intent".to_owned(),
                value: "build".to_owned(),
            }
        );
    }

    #[test]
    fn test_parse_invalid_no_equals() {
        assert!(parse_agent_context("intent").is_err());
    }

    #[test]
    fn test_parse_invalid_key() {
        assert!(parse_agent_context("Invalid=build").is_err());
        assert!(parse_agent_context("123=build").is_err());
    }

    #[test]
    fn test_parse_value_with_special_chars() {
        let entry = parse_agent_context("prior_error=kotlin_unresolved_reference").unwrap();
        assert_eq!(entry.value, "kotlin_unresolved_reference");
    }
}
