// Protocol adapted from bobs-org/bob-cli (MIT, Copyright 2026 Bryan M Bugyi).
//! Protocol 2: the contract between nut and every shell adapter.
//!
//! Request: `nut __complete <SHELL> --protocol <N> [--suffix <TEXT>]
//! [--columns <N>] -- <WORD>...`, where `SHELL` is `zsh` or `bash`, `WORD`s are
//! the command line's words up to the cursor (shell-unquoted, first word
//! ignored, last word is the cursor word's text before the cursor), and
//! `--suffix` is the cursor word's text after the cursor.
//!
//! Response (v2): UTF-8 lines on stdout, directives first, then groups and
//! candidates. Directives are `!prefix N`, `!files [GLOB]`, `!dirs`, and
//! `!message TEXT`, plus `!hint LABEL<TAB>TEXT` and `!group TAG<TAB>HEADING`
//! (which starts the group later candidates belong to). Candidates are
//! `VALUE<TAB>DISPLAY<TAB>space|nospace`. Exit 0, including on internal
//! failure (which yields empty output). A malformed request exits 2 with a
//! message on stderr.
//!
//! A v1 request gets a v1 response built from the same menu:
//! `value<TAB>description<TAB>heading<TAB>space`, with a hint mapped to
//! `!message LABEL  TEXT`.

use std::ffi::{OsStr, OsString};

/// The protocol version this binary speaks.
pub(crate) const PROTOCOL: u32 = 2;
/// The oldest protocol version this binary still answers.
pub(crate) const MIN_PROTOCOL: u32 = 1;

/// Emitted when the adapter speaks a protocol older than [`MIN_PROTOCOL`].
pub(crate) const BELOW_MIN_MESSAGE: &str =
    "nut shell completion is out of date \u{2014} run: nut completion install";
/// Emitted when the adapter speaks a protocol newer than [`PROTOCOL`].
pub(crate) const ABOVE_MAX_MESSAGE: &str =
    "this nut is older than its shell completion \u{2014} reinstall nut (just install)";

use super::Shell;
use super::menu::Menu;

/// A parsed `__complete` request.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    /// The requesting shell.
    pub shell: Shell,
    /// The adapter's protocol version (already range-checked by the caller).
    pub protocol: u32,
    /// Text of the cursor word after the cursor, if any (retained by adapters).
    #[allow(dead_code)]
    pub suffix: Option<OsString>,
    /// Terminal columns reported by the adapter (default 80, floor 20).
    pub columns: usize,
    /// Command-line words up to the cursor; first word ignored, last word
    /// is the cursor word's text before the cursor.
    pub words: Vec<OsString>,
}

/// Parse the argv after `__complete`. The error string is the stderr
/// message for the exit-2 malformed-request path.
pub(crate) fn parse_request(argv: &[OsString]) -> Result<Request, String> {
    let usage = "usage: nut __complete <zsh|bash> --protocol <N> [--suffix <TEXT>] [--columns <N>] -- <WORD>...";
    let mut rest = argv.iter();
    let shell_word = rest
        .next()
        .ok_or_else(|| format!("nut __complete: malformed request: missing shell\n{usage}"))?;
    let shell = match shell_word.to_string_lossy().as_ref() {
        "zsh" => Shell::Zsh,
        "bash" => Shell::Bash,
        other => {
            return Err(format!(
                "nut __complete: malformed request: \
                 unknown shell {other:?}, expected zsh or bash\n{usage}"
            ));
        }
    };
    let mut protocol: Option<u32> = None;
    let mut suffix: Option<OsString> = None;
    let mut columns: Option<usize> = None;
    let mut words: Option<Vec<OsString>> = None;
    let mut args = rest;
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if words.is_none() && text == "--" {
            words = Some(args.cloned().collect());
            break;
        }
        if words.is_some() {
            break;
        }
        let (flag, inline) = match text.split_once('=') {
            Some((flag, inline)) => (flag, Some(inline)),
            None => (text.as_ref(), None),
        };
        match flag {
            "--protocol" if protocol.is_none() => {
                let raw = match inline {
                    Some(inline) => inline.to_string(),
                    None => match args.next() {
                        Some(next) => next.to_string_lossy().into_owned(),
                        None => {
                            return Err(format!(
                                "nut __complete: malformed request: \
                                 --protocol needs a value\n{usage}"
                            ));
                        }
                    },
                };
                match raw.parse::<u32>() {
                    Ok(number) => protocol = Some(number),
                    Err(_) => {
                        return Err(format!(
                            "nut __complete: malformed request: \
                             bad --protocol value {raw:?}\n{usage}"
                        ));
                    }
                }
            }
            "--suffix" if suffix.is_none() => {
                suffix = Some(match inline {
                    Some(inline) => OsString::from(inline),
                    None => match args.next() {
                        Some(next) => next.clone(),
                        None => {
                            return Err(format!(
                                "nut __complete: malformed request: \
                                 --suffix needs a value\n{usage}"
                            ));
                        }
                    },
                });
            }
            "--columns" if columns.is_none() => {
                let raw = match inline {
                    Some(inline) => inline.to_string(),
                    None => match args.next() {
                        Some(next) => next.to_string_lossy().into_owned(),
                        None => {
                            return Err(format!(
                                "nut __complete: malformed request: \
                                 --columns needs a value\n{usage}"
                            ));
                        }
                    },
                };
                match raw.parse::<usize>() {
                    Ok(number) => columns = Some(number),
                    Err(_) => {
                        return Err(format!(
                            "nut __complete: malformed request: \
                             bad --columns value {raw:?}\n{usage}"
                        ));
                    }
                }
            }
            other => {
                return Err(format!(
                    "nut __complete: malformed request: \
                     unknown option {other:?}\n{usage}"
                ));
            }
        }
    }
    let words = words.ok_or_else(|| {
        format!("nut __complete: malformed request: missing -- separator\n{usage}")
    })?;
    if words.is_empty() {
        return Err(format!(
            "nut __complete: malformed request: no words after --\n{usage}"
        ));
    }
    let protocol = protocol
        .ok_or_else(|| format!("nut __complete: malformed request: missing --protocol\n{usage}"))?;
    Ok(Request {
        shell,
        protocol,
        suffix,
        columns: columns.unwrap_or(80).max(20),
        words,
    })
}

/// Check the request protocol against [`MIN_PROTOCOL`]..=[`PROTOCOL`].
/// Returns the skew message line when the adapter is out of range.
pub(crate) fn skew_message(protocol: u32) -> Option<String> {
    if protocol < MIN_PROTOCOL {
        Some(message_line(BELOW_MIN_MESSAGE))
    } else if protocol > PROTOCOL {
        Some(message_line(ABOVE_MAX_MESSAGE))
    } else {
        None
    }
}

/// A `!prefix <N>` directive: keep the first N Unicode scalar values of
/// the cursor-word prefix; candidates replace only the rest.
pub(crate) fn prefix_line(keep: usize) -> String {
    format!("!prefix {keep}")
}

/// The `!dirs` directive: complete directories natively.
pub(crate) fn dirs_line() -> String {
    "!dirs".to_string()
}

/// The `!files` directive, optionally filtered by glob.
pub(crate) fn files_line(glob: Option<&str>) -> String {
    match glob {
        Some(glob) => format!("!files {glob}"),
        None => "!files".to_string(),
    }
}

/// A `!message <text>` directive: show a hint, offer no candidates.
pub(crate) fn message_line(text: &str) -> String {
    format!("!message {}", sanitize_field(text))
}

/// A `!hint LABEL<TAB>TEXT` directive: a styled free-form slot.
pub(crate) fn hint_line(label: &str, text: &str) -> String {
    format!("!hint {}\t{}", sanitize_field(label), sanitize_field(text))
}

/// A `!group TAG<TAB>HEADING` directive: later candidates belong to it.
pub(crate) fn group_line(tag: &str, heading: &str) -> String {
    format!(
        "!group {}\t{}",
        sanitize_field(tag),
        sanitize_field(heading)
    )
}

/// Encode one v2 candidate line.
pub(crate) fn candidate_line_v2(value: &OsStr, display: &str, nospace: bool) -> Option<String> {
    let value = value.to_string_lossy();
    if value.is_empty() || value.chars().any(char::is_control) || value.starts_with('!') {
        return None;
    }
    Some(format!(
        "{value}\t{}\t{}",
        sanitize_field(display),
        if nospace { "nospace" } else { "space" },
    ))
}

/// Encode one v1 candidate line.
pub(crate) fn candidate_line(
    value: &OsStr,
    description: &str,
    group: &str,
    nospace: bool,
) -> Option<String> {
    let value = value.to_string_lossy();
    if value.is_empty() || value.chars().any(char::is_control) || value.starts_with('!') {
        return None;
    }
    let group = sanitize_field(group);
    let group = if group.is_empty() { "values" } else { &group };
    Some(format!(
        "{value}\t{}\t{group}\t{}",
        sanitize_field(description),
        if nospace { "nospace" } else { "space" },
    ))
}

/// Serialize a menu plus an optional prefix keep for the requesting shell.
/// V2 emits `!group` before the first candidate; v1 emits the legacy shape
/// from the same menu with a hint mapped to `!message`.
pub(crate) fn serialize(
    menu: &Menu,
    prefix_keep: Option<usize>,
    shell: Shell,
    protocol: u32,
    columns: usize,
    ascii: bool,
) -> Vec<String> {
    if matches!(menu, Menu::Empty) {
        return Vec::new();
    }
    let mut lines = Vec::new();
    if let Some(keep) = prefix_keep {
        lines.push(prefix_line(keep));
    }
    if protocol <= 1 {
        return serialize_v1(menu, &mut lines);
    }
    let _ = (shell, columns, ascii);
    match menu {
        Menu::Groups(groups) => {
            let col = super::menu::shared_column(groups, columns);
            for group in groups {
                if group.items.is_empty() {
                    continue;
                }
                lines.push(group_line(&group.tag, &group.heading));
                for item in &group.items {
                    let display = match shell {
                        Shell::Zsh => super::menu::zsh_display(
                            &item.label,
                            &item.description,
                            col,
                            columns,
                            ascii,
                        ),
                        Shell::Bash => {
                            // Bash value labels always start with the value itself:
                            // the value plus `<NAME>` for options taking values.
                            let value_label = if let Some(bracket) = item.label.find('<') {
                                format!("{} {}", item.value, &item.label[bracket..])
                            } else {
                                item.value.clone()
                            };
                            super::menu::bash_display(
                                &value_label,
                                &item.description,
                                col,
                                columns,
                                ascii,
                            )
                        }
                    };
                    if let Some(line) =
                        candidate_line_v2(OsStr::new(&item.value), &display, item.nospace)
                    {
                        lines.push(line);
                    }
                }
            }
        }
        Menu::Hint(hint) => lines.push(hint_line(&hint.label, &hint.text)),
        Menu::Files(glob) => lines.push(files_line(glob.as_deref())),
        Menu::Dirs => lines.push(dirs_line()),
        Menu::Message(text) => lines.push(message_line(text)),
        Menu::Empty => {}
    }
    lines.retain(|line| !line.starts_with('!') || is_directive(line));
    if lines.iter().all(|line| line.starts_with('!')) && !has_group_candidate(&lines) {
        // Directives alone (`!files`, `!hint`, `!message`, `!prefix`) are the
        // whole response; a bare `!group` with no candidates is nothing.
        if lines.iter().any(|line| line.starts_with("!group")) {
            return lines
                .into_iter()
                .filter(|line| !line.starts_with("!group"))
                .collect();
        }
    }
    lines
}

fn has_group_candidate(lines: &[String]) -> bool {
    let mut seen_group = false;
    for line in lines {
        if line.starts_with("!group") {
            seen_group = true;
        } else if !line.starts_with('!') && seen_group {
            return true;
        }
    }
    false
}

fn is_directive(line: &str) -> bool {
    line.starts_with("!prefix ")
        || line == "!dirs"
        || line == "!files"
        || line.starts_with("!files ")
        || line.starts_with("!message ")
        || line.starts_with("!hint ")
        || line.starts_with("!group ")
}

fn serialize_v1(menu: &Menu, lines: &mut Vec<String>) -> Vec<String> {
    match menu {
        Menu::Groups(groups) => {
            for group in groups {
                for item in &group.items {
                    if let Some(line) = candidate_line(
                        OsStr::new(&item.value),
                        &item.description,
                        &group.heading,
                        item.nospace,
                    ) {
                        lines.push(line);
                    }
                }
            }
        }
        Menu::Hint(hint) => lines.push(message_line(&format!("{}  {}", hint.label, hint.text))),
        Menu::Files(glob) => lines.push(files_line(glob.as_deref())),
        Menu::Dirs => lines.push(dirs_line()),
        Menu::Message(text) => lines.push(message_line(text)),
        Menu::Empty => {}
    }
    lines.clone()
}

/// Descriptions and groups travel in TAB-separated fields, so TAB, LF,
/// and CR all become spaces.
fn sanitize_field(text: &str) -> String {
    text.chars()
        .map(|char| if char.is_control() { ' ' } else { char })
        .collect()
}

#[allow(dead_code)]
pub(crate) fn validate_tag(tag: &str) -> bool {
    if !tag.starts_with("nut-") || tag.len() <= 4 {
        return false;
    }
    tag[4..]
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_full_request() {
        let request = parse_request(&argv(&[
            "zsh",
            "--protocol",
            "1",
            "--suffix",
            "fix",
            "--",
            "nut",
            "capture",
            "",
        ]))
        .expect("full request parses");
        assert_eq!(request.shell, Shell::Zsh);
        assert_eq!(request.protocol, 1);
        assert_eq!(request.suffix, Some(OsString::from("fix")));
        assert_eq!(request.columns, 80);
        assert_eq!(request.words, argv(&["nut", "capture", ""]));
    }

    #[test]
    fn parses_attached_forms_and_columns() {
        let request = parse_request(&argv(&[
            "bash",
            "--protocol=2",
            "--columns=100",
            "--",
            "nut",
            "",
        ]))
        .expect("attached protocol parses");
        assert_eq!(request.shell, Shell::Bash);
        assert_eq!(request.protocol, 2);
        assert_eq!(request.columns, 100);
        assert_eq!(request.suffix, None);
        let floored = parse_request(&argv(&[
            "zsh",
            "--protocol=2",
            "--columns=5",
            "--",
            "nut",
            "",
        ]))
        .expect("small columns floor");
        assert_eq!(floored.columns, 20);
    }

    #[test]
    fn rejects_malformed_requests() {
        for words in [
            vec![],
            vec!["fish"],
            vec!["zsh"],
            vec!["zsh", "--protocol", "1"],
            vec!["zsh", "--protocol", "1", "--"],
            vec!["zsh", "--bogus", "1", "--", "nut"],
            vec!["zsh", "--protocol", "x", "--", "nut"],
            vec!["zsh", "--protocol"],
            vec!["zsh", "--suffix"],
            vec!["zsh", "--protocol", "2", "--columns", "x", "--", "nut", ""],
            vec!["zsh", "--protocol", "2", "--columns"],
        ] {
            let argv = argv(&words);
            assert!(
                parse_request(&argv).is_err(),
                "expected malformed: {words:?}"
            );
        }
    }

    #[test]
    fn skew_messages_cover_both_directions() {
        assert_eq!(skew_message(0), Some(message_line(BELOW_MIN_MESSAGE)));
        assert_eq!(skew_message(1), None);
        assert_eq!(skew_message(2), None);
        assert_eq!(skew_message(99), Some(message_line(ABOVE_MAX_MESSAGE)));
    }

    #[test]
    fn encoder_sanitizes_fields_and_drops_bad_values() {
        assert_eq!(
            candidate_line(
                OsStr::new("human"),
                "Colored text for people",
                "format",
                false
            ),
            Some("human\tColored text for people\tformat\tspace".to_string())
        );
        assert_eq!(
            candidate_line(OsStr::new("x"), "a\tb\nc\rd", "", true),
            Some("x\ta b c d\tvalues\tnospace".to_string())
        );
        for bad in ["", "a\tb", "a\nb", "a\rb", "a\u{1b}b", "!dirs"] {
            assert_eq!(
                candidate_line(OsStr::new(bad), "d", "g", false),
                None,
                "expected drop: {bad:?}"
            );
        }
    }

    #[test]
    fn v2_serialization_uses_groups_and_displays() {
        let menu = Menu::Groups(vec![crate::completion::menu::Group {
            tag: "nut-monitor".into(),
            heading: "Monitor".into(),
            items: vec![crate::completion::menu::Item {
                value: "status".into(),
                label: "status".into(),
                description: "Show UPS status".into(),
                nospace: false,
            }],
        }]);
        let lines = serialize(&menu, None, Shell::Zsh, 2, 80, true);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("!group nut-monitor\tMonitor"));
        assert!(lines[1].starts_with("status\t"));
        let v1 = serialize(&menu, None, Shell::Zsh, 1, 80, true);
        assert_eq!(
            v1,
            vec!["status\tShow UPS status\tMonitor\tspace".to_string()]
        );
    }

    #[test]
    fn tags_must_be_nut_prefixed() {
        assert!(validate_tag("nut-monitor"));
        assert!(validate_tag("nut-global-options"));
        assert!(!validate_tag("Monitor"));
        assert!(!validate_tag("nut-"));
        assert!(!validate_tag("nut-Monitor"));
    }
}
