//! Shared completion menu: ordered groups plus shell-specific rendering.
//!
//! Rust decides what the menu contains and how it is laid out. Shell adapters
//! only hand it to the shell. Tests snapshot the virtual screen without a
//! terminal.

use crate::ui::layout::display_width;

/// One selectable row inside a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Text inserted on completion.
    pub value: String,
    /// Text shown in the menu (clap-help style for options).
    pub label: String,
    /// One-line explanation; never blank for a shown candidate.
    pub description: String,
    /// When true the adapter inserts no trailing space.
    pub nospace: bool,
}

/// One headed section of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// Machine tag, always `nut-` prefixed (for scoped zstyles).
    pub tag: String,
    /// People-words heading shown above the group.
    pub heading: String,
    /// Rows in display order.
    pub items: Vec<Item>,
}

/// A styled free-form hint for slots that take arbitrary text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub label: String,
    pub text: String,
}

/// What a Tab offers. Groups carry candidates; the other variants carry a
/// single directive or hint and never candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Menu {
    Groups(Vec<Group>),
    Hint(Hint),
    Files(Option<String>),
    Dirs,
    Message(String),
    Empty,
}

impl Menu {
    pub fn groups(groups: Vec<Group>) -> Self {
        Self::Groups(groups)
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Groups(groups) => groups.iter().all(|group| group.items.is_empty()),
            Self::Hint(_) | Self::Files(_) | Self::Dirs | Self::Message(_) => false,
            Self::Empty => true,
        }
    }
}

/// Normalize a description: controls to spaces, collapse runs, strip
/// backticks, trim. The zsh color pattern depends on single spacing.
pub fn normalize_description(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            pending_space = true;
        } else if ch == '`' {
            continue;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(ch);
        }
    }
    // Clap's default help flag carries a parenthetical pointer that adds
    // noise to a narrow menu.
    let trimmed = out
        .strip_suffix(" (see more with '--help')")
        .unwrap_or(&out);
    let mut owned = trimmed.to_owned();
    if owned == "Print help (see more with '--help')" {
        owned = "Print help".to_owned();
    }
    owned
}

/// Shared label column for the whole menu.
pub fn shared_column(menu: &[Group], columns: usize) -> usize {
    let columns = columns.max(20);
    let longest = menu
        .iter()
        .flat_map(|group| group.items.iter())
        .map(|item| display_width(&item.label))
        .max()
        .unwrap_or(0);
    let cap = (columns * 2 / 5).max(16);
    longest.min(cap)
}

fn ellipsis(ascii: bool) -> &'static str {
    if ascii { "..." } else { "…" }
}

fn truncate_to_width(text: &str, max_width: usize, ascii: bool) -> String {
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    let ell = ellipsis(ascii);
    let ell_width = display_width(ell);
    let budget = max_width.saturating_sub(ell_width);
    if budget == 0 {
        return ell.to_owned();
    }
    // Prefer a word boundary when it keeps at least 60% of the space.
    let mut best_byte: Option<usize> = None;
    for (byte, _) in text.char_indices() {
        let prefix = &text[..byte];
        if prefix.ends_with(' ') && display_width(prefix.trim_end()) <= budget {
            let kept = display_width(prefix.trim_end());
            if kept * 5 >= budget * 3 {
                best_byte = Some(byte);
            }
        }
        if display_width(prefix) > budget {
            break;
        }
    }
    let mut cut = if let Some(byte) = best_byte {
        text[..byte].trim_end().to_owned()
    } else {
        let mut acc = String::new();
        for ch in text.chars() {
            let next = format!("{acc}{ch}");
            if display_width(&next) > budget {
                break;
            }
            acc = next;
        }
        acc.trim_end().to_owned()
    };
    cut = cut.trim_end_matches([',', ';', ':']).to_owned();
    cut.push_str(ell);
    cut
}

/// Render one zsh display line: two-space indent, label padded to `col`,
/// two-space gutter, then the description (or label only in narrow panes).
pub fn zsh_display(
    label: &str,
    description: &str,
    col: usize,
    columns: usize,
    ascii: bool,
) -> String {
    let max_line = columns.saturating_sub(1).max(1);
    let label_width = display_width(label);
    if label_width + 4 >= max_line {
        return format!(
            "  {}",
            truncate_to_width(label, max_line.saturating_sub(2), ascii)
        );
    }
    let pad = col.saturating_sub(label_width);
    let prefix = format!("  {label}{}", " ".repeat(pad));
    let prefix_width = display_width(&prefix);
    if description.is_empty() {
        return prefix.trim_end().to_owned();
    }
    // Labels longer than `col` keep a 2-space gutter, but the line must
    // still fit: measure the actual prefix, not the shared column.
    if max_line <= prefix_width + 2 + 12 {
        return prefix.trim_end().to_owned();
    }
    let available = max_line - prefix_width - 2;
    let truncated = truncate_to_width(description, available, ascii);
    format!("{prefix}  {truncated}")
}

/// Render one bash display line: `value_label` padded to `col`, two-space
/// gutter, description, padded with spaces to `columns - 1` so readline lists
/// one entry per row. Always starts with the value itself.
pub fn bash_display(
    value_label: &str,
    description: &str,
    col: usize,
    columns: usize,
    ascii: bool,
) -> String {
    let max_line = columns.saturating_sub(1).max(1);
    let width = display_width(value_label);
    let padded = if width < col {
        format!("{value_label}{}", " ".repeat(col - width))
    } else {
        value_label.to_owned()
    };
    let line = if description.is_empty() {
        padded
    } else {
        let gutter_width = col + 2;
        if max_line <= gutter_width + 12 {
            padded.trim_end().to_owned()
        } else {
            let available = max_line - gutter_width;
            format!(
                "{padded}  {}",
                truncate_to_width(description, available, ascii)
            )
        }
    };
    let line_width = display_width(&line);
    if line_width < max_line {
        format!("{line}{}", " ".repeat(max_line - line_width))
    } else if line_width > max_line {
        truncate_to_width(&line, max_line, ascii)
    } else {
        line
    }
}

/// What zsh prints without color: headings plus zsh display lines. Unit
/// snapshots and the docs capture use this.
pub fn virtual_screen(menu: &Menu, columns: usize) -> String {
    let columns = columns.max(20);
    let mut out = String::new();
    match menu {
        Menu::Groups(groups) => {
            let col = shared_column(groups, columns);
            for group in groups {
                if group.items.is_empty() {
                    continue;
                }
                out.push_str(&group.heading);
                out.push('\n');
                for item in &group.items {
                    out.push_str(&zsh_display(
                        &item.label,
                        &item.description,
                        col,
                        columns,
                        false,
                    ));
                    out.push('\n');
                }
            }
        }
        Menu::Hint(hint) => {
            out.push_str(&hint.label);
            out.push_str("  ");
            out.push_str(&hint.text);
            out.push('\n');
        }
        Menu::Files(glob) => {
            out.push_str("Files");
            if let Some(glob) = glob {
                out.push(' ');
                out.push_str(glob);
            }
            out.push('\n');
        }
        Menu::Dirs => out.push_str("Directories\n"),
        Menu::Message(text) => {
            out.push_str(text);
            out.push('\n');
        }
        Menu::Empty => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Group> {
        vec![
            Group {
                tag: "nut-monitor".into(),
                heading: "Monitor".into(),
                items: vec![
                    Item {
                        value: "status".into(),
                        label: "status".into(),
                        description: "Show UPS status".into(),
                        nospace: false,
                    },
                    Item {
                        value: "battery".into(),
                        label: "battery".into(),
                        description: "Show battery charge and runtime, or run a guarded self-test"
                            .into(),
                        nospace: false,
                    },
                ],
            },
            Group {
                tag: "nut-act".into(),
                heading: "Act".into(),
                items: vec![Item {
                    value: "beeper".into(),
                    label: "beeper".into(),
                    description: "Show or change the UPS beeper".into(),
                    nospace: false,
                }],
            },
        ]
    }

    #[test]
    fn shared_column_caps_at_forty_percent() {
        let groups = sample();
        // Longest sample label is `battery` (7); cap is 40 at 100 cols.
        assert_eq!(shared_column(&groups, 100), 7);
        let narrow = shared_column(&groups, 50);
        assert!(narrow <= 20);
    }

    #[test]
    fn descriptions_never_carry_double_spaces_or_backticks() {
        assert_eq!(normalize_description("a  b\tc`d` "), "a b cd");
        assert_eq!(
            normalize_description("Print help (see more with '--help')"),
            "Print help"
        );
    }

    #[test]
    fn narrow_panes_drop_descriptions() {
        let line = zsh_display("status", "Show UPS status", 16, 30, false);
        assert!(display_width(&line) <= 29, "{line}");
    }
}
