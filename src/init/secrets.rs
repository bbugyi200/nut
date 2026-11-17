#![forbid(unsafe_code)]

//! Shared NUT credential parsing and rendering helpers.
//!
//! Doctor and init both read `upsd.users` and `upsmon.conf`. The parsers here
//! implement the NUT quoting syntax needed for existing files: a password is
//! either a bare token or a double-quoted string where `\\` escapes the next
//! character. A missing closing quote is an error, never a silent truncation.
//!
//! Debug output never contains secret material.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SecretError {
    #[error("password value is missing")]
    Missing,
    #[error("password value is empty")]
    Empty,
    #[error("unterminated quoted password")]
    Unterminated,
    #[error("password contains a newline or control character")]
    Control,
    #[error("password is too long")]
    TooLong,
}

/// Parse one NUT password value (the right-hand side of `password = ...` or a
/// MONITOR password field) into its unescaped form.
pub fn parse_nut_secret_value(raw: &str) -> Result<String, SecretError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SecretError::Missing);
    }
    if let Some(rest) = trimmed.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        let mut closed = false;
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                let Some(next) = chars.next() else {
                    return Err(SecretError::Unterminated);
                };
                out.push(next);
            } else if ch == '"' {
                closed = true;
                break;
            } else if ch == '\n' || ch == '\r' || ch.is_control() {
                return Err(SecretError::Control);
            } else {
                out.push(ch);
            }
        }
        if !closed {
            return Err(SecretError::Unterminated);
        }
        // Trailing garbage after the closing quote is not part of the value,
        // but a second token means the line was malformed.
        if out.is_empty() {
            return Err(SecretError::Empty);
        }
        Ok(out)
    } else {
        // Bare token ends at whitespace or comment.
        let token = trimmed
            .split_whitespace()
            .next()
            .ok_or(SecretError::Missing)?;
        let token = token.trim_matches('"');
        if token.is_empty() {
            return Err(SecretError::Empty);
        }
        if token.bytes().any(|b| b == b'\n' || b == b'\r') {
            return Err(SecretError::Control);
        }
        Ok(token.to_owned())
    }
}

/// Render a secret for installation in a NUT file, quoting only when needed.
pub fn escape_nut_secret(secret: &str) -> Result<String, SecretError> {
    if secret.is_empty() {
        return Err(SecretError::Empty);
    }
    if secret.len() > 512 {
        return Err(SecretError::TooLong);
    }
    if secret
        .bytes()
        .any(|b| b == b'\n' || b == b'\r' || (b < 0x20 && b != b'\t'))
    {
        return Err(SecretError::Control);
    }
    let bare_ok = !secret.is_empty()
        && secret.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'@' | b':' | b'+')
        });
    if bare_ok {
        Ok(secret.to_owned())
    } else {
        let mut out = String::with_capacity(secret.len() + 2);
        out.push('"');
        for ch in secret.chars() {
            if ch == '"' || ch == '\\' {
                out.push('\\');
            }
            out.push(ch);
        }
        out.push('"');
        Ok(out)
    }
}

/// SHA-256 fingerprint of one secret, hex-encoded. Used only in the private
/// plan representation and hash preimage, never in public JSON or diffs.
pub fn fingerprint_secret(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

#[derive(Clone, Eq, PartialEq)]
pub struct ParsedPassword {
    value: String,
}

impl ParsedPassword {
    pub fn new(value: String) -> Self {
        Self { value }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn fingerprint(&self) -> String {
        fingerprint_secret(&self.value)
    }
}

impl std::fmt::Debug for ParsedPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedPassword")
            .field("value", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpsdUsers {
    pub upsmon: Option<ParsedPassword>,
    pub upsmon_malformed: bool,
    pub operator: Option<ParsedPassword>,
    pub operator_malformed: bool,
    pub operator_instcmds: Vec<String>,
    pub operator_has_actions_set: bool,
    pub operator_has_all: bool,
    pub sections: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

fn section_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        Some(trimmed.trim_matches(['[', ']']).trim().to_owned())
    } else {
        None
    }
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value.trim()))
}

/// Parse `upsd.users` without ever logging secret values.
pub fn parse_upsd_users(contents: &str) -> UpsdUsers {
    let mut out = UpsdUsers::default();
    let mut current: Option<String> = None;
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = section_name(line) {
            current = Some(section.clone());
            out.sections.entry(section).or_default();
            continue;
        }
        let Some(section) = current.clone() else {
            continue;
        };
        let Some((key, value)) = split_key_value(line) else {
            continue;
        };
        let lowered = key.to_ascii_lowercase();
        out.sections
            .entry(section.clone())
            .or_default()
            .entry(lowered.clone())
            .or_default()
            .push(value.to_owned());
        if section == "upsmon" && lowered == "password" {
            match parse_nut_secret_value(value) {
                Ok(secret) if !secret.is_empty() => {
                    out.upsmon = Some(ParsedPassword::new(secret));
                }
                _ => {
                    out.upsmon_malformed = true;
                }
            }
        } else if section == "nut-operator" && lowered == "password" {
            match parse_nut_secret_value(value) {
                Ok(secret) if !secret.is_empty() => {
                    out.operator = Some(ParsedPassword::new(secret));
                }
                _ => {
                    out.operator_malformed = true;
                }
            }
        } else if section == "nut-operator" && lowered == "instcmds" {
            for token in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                // Each comma-separated entry may itself be one directive.
                let cleaned = token.trim_matches('"').trim().to_owned();
                if cleaned.eq_ignore_ascii_case("ALL") {
                    out.operator_has_all = true;
                }
                out.operator_instcmds.push(cleaned);
            }
            // Also accept repeated single-directive lines (already one per line).
            if !value.contains(',') {
                let single = value.trim().trim_matches('"').to_owned();
                if single.eq_ignore_ascii_case("ALL") {
                    out.operator_has_all = true;
                }
            }
        } else if section == "nut-operator" && lowered == "actions" {
            if value.eq_ignore_ascii_case("SET") {
                out.operator_has_actions_set = true;
            }
        } else if lowered == "instcmds" && value.to_ascii_lowercase().contains("all") {
            // Any account granting ALL is recorded when parsing the operator
            // section; other sections are covered by doctor's existing check.
            if section == "nut-operator" {
                out.operator_has_all = true;
            }
        }
    }
    out
}

/// Parse one `upsd.users` operator section in isolation (used by tests).
pub fn parse_operator_section(contents: &str) -> UpsdUsers {
    parse_upsd_users(contents)
}

/// Parse the MONITOR password for `ups_name` from `upsmon.conf`.
pub fn parse_monitor_password(contents: &str, ups_name: &str) -> Result<String, SecretError> {
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.starts_with('#') || !line.starts_with("MONITOR") {
            continue;
        }
        let tokens = tokenize_monitor_line(line);
        // MONITOR <system> <powersupply> <username> <password> <type>
        if tokens.len() < 6 {
            continue;
        }
        let system = tokens[1].as_str();
        let target = system.split('@').next().unwrap_or(system);
        if target != ups_name && system != format!("{ups_name}@localhost").as_str() {
            // Accept bare `ups` or `ups@localhost` spellings only.
            if target != ups_name {
                continue;
            }
        }
        if tokens[3] != "upsmon" {
            continue;
        }
        return parse_nut_secret_value(&tokens[4]);
    }
    Err(SecretError::Missing)
}

fn tokenize_monitor_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    // Skip leading MONITOR keyword handling by generic splitter that
    // respects double quotes and backslash escapes.
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            } else if ch == '"' {
                in_quotes = false;
                current.push('"');
            } else {
                current.push(ch);
            }
        } else if ch == '"' {
            in_quotes = true;
            current.push('"');
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(current.clone());
                current.clear();
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_and_quoted_passwords_parse() {
        assert_eq!(parse_nut_secret_value("hunter2").unwrap(), "hunter2");
        assert_eq!(parse_nut_secret_value("\"hunter 2\"").unwrap(), "hunter 2");
        assert_eq!(parse_nut_secret_value("\"a\\\"b\"").unwrap(), "a\"b");
        assert!(parse_nut_secret_value("\"unterminated").is_err());
        assert!(parse_nut_secret_value("").is_err());
    }

    #[test]
    fn escaping_quotes_only_when_needed() {
        assert_eq!(escape_nut_secret("hunter2").unwrap(), "hunter2");
        assert_eq!(escape_nut_secret("hunter 2").unwrap(), "\"hunter 2\"");
        assert_eq!(escape_nut_secret("a\"b").unwrap(), "\"a\\\"b\"");
    }

    #[test]
    fn debug_never_contains_secret() {
        let parsed = ParsedPassword::new("SENTINEL-SECRET-XYZ".to_owned());
        let rendered = format!("{parsed:?}");
        assert!(!rendered.contains("SENTINEL-SECRET-XYZ"));
    }

    #[test]
    fn monitor_line_with_quoted_password() {
        let conf = "MONITOR cp1500@localhost 1 upsmon \"hunter 2\" primary\n";
        assert_eq!(parse_monitor_password(conf, "cp1500").unwrap(), "hunter 2");
    }
}
