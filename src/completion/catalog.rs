//! Static completion tables: refused flags, value descriptions, suggestions,
//! and value-group names. A drift test walks the clap tree and fails when a
//! table entry names a path, argument, or value that does not exist.

use super::providers::Candidate;
use crate::doctor::{self, Section};

#[cfg(test)]
use crate::cli::Cli;
#[cfg(test)]
use clap::{Arg, Command, CommandFactory};

/// Flags that exist on the clap tree but must never be offered.
pub const REFUSED: &[Refused] = &[
    Refused {
        path: &["drill"],
        arg: "yes",
    },
    Refused {
        path: &["bios", "setup"],
        arg: "yes",
    },
];

pub struct Refused {
    pub path: &'static [&'static str],
    pub arg: &'static str,
}

/// Path-specific value rows that beat clap possible values.
pub const SUGGESTIONS: &[Suggestion] = &[
    Suggestion {
        path: &[],
        arg: "color",
        group: "color modes",
        values: &[
            (
                "auto",
                "Color only on a terminal; honors NO_COLOR and CLICOLOR_FORCE",
            ),
            ("always", "Always color, even through a pipe"),
            ("never", "Never color"),
        ],
    },
    Suggestion {
        path: &["completion", "install"],
        arg: "shell",
        group: "shells",
        values: &[
            ("zsh", "Grouped menus with descriptions"),
            ("bash", "Plain candidate lists"),
        ],
    },
    Suggestion {
        path: &["completion", "uninstall"],
        arg: "shell",
        group: "shells",
        values: &[
            ("zsh", "Grouped menus with descriptions"),
            ("bash", "Plain candidate lists"),
        ],
    },
    Suggestion {
        path: &["events"],
        arg: "since",
        group: "times",
        values: &[
            ("-1h", "The last hour"),
            ("-6h", "The last six hours"),
            ("today", "Since midnight"),
            ("yesterday", "Since midnight yesterday"),
            ("-7d", "The last seven days"),
        ],
    },
    Suggestion {
        path: &["events"],
        arg: "boot",
        group: "boots",
        values: &[
            ("0", "Current boot"),
            ("-1", "Previous boot"),
            ("-2", "Two boots ago"),
        ],
    },
];

pub struct Suggestion {
    pub path: &'static [&'static str],
    pub arg: &'static str,
    pub group: &'static str,
    pub values: &'static [(&'static str, &'static str)],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathDirective {
    Files,
    Dirs,
}

const PATH_DIRECTIVES: &[(&[&str], &str, PathDirective)] = &[
    (&[], "config", PathDirective::Files),
    (&["config", "new"], "output", PathDirective::Files),
    (&["completion", "install"], "target", PathDirective::Dirs),
    (&["config", "render"], "output", PathDirective::Dirs),
];

/// Group header for `--ups` on every command that takes it.
pub const UPS_GROUP: &str = "UPS devices";
/// Group header for `bios profile show`.
pub const BOARD_GROUP: &str = "board profiles";
/// Group header for `rollback` stamps.
pub const BACKUP_GROUP: &str = "backups";

pub fn is_refused(path: &[String], arg: &str) -> bool {
    REFUSED.iter().any(|entry| {
        entry.arg == arg
            && path
                .iter()
                .map(String::as_str)
                .eq(entry.path.iter().copied())
    })
}

pub fn suggestions(path: &[&str], arg: &str) -> Option<&'static Suggestion> {
    SUGGESTIONS.iter().find(|entry| {
        entry.arg == arg
            && (entry.path.is_empty() || path.iter().copied().eq(entry.path.iter().copied()))
    })
}

pub fn path_directive(path: &[&str], arg: &str) -> Option<PathDirective> {
    PATH_DIRECTIVES
        .iter()
        .find_map(|(entry_path, entry_arg, directive)| {
            if *entry_arg != arg {
                return None;
            }
            if entry_path.is_empty() || path.iter().copied().eq(entry_path.iter().copied()) {
                Some(*directive)
            } else {
                None
            }
        })
}

pub fn value_description(path: &[&str], arg: &str, value: &str) -> Option<&'static str> {
    suggestions(path, arg).and_then(|entry| {
        entry
            .values
            .iter()
            .find(|(name, _)| *name == value)
            .map(|(_, description)| *description)
    })
}

pub fn suggestion_candidates(path: &[&str], arg: &str) -> Option<Vec<Candidate>> {
    let entry = suggestions(path, arg)?;
    Some(
        entry
            .values
            .iter()
            .map(|(value, description)| Candidate {
                value: (*value).to_owned(),
                description: (*description).to_owned(),
                group: entry.group.to_owned(),
                nospace: false,
            })
            .collect(),
    )
}

pub fn section_group(section: Section) -> &'static str {
    match section {
        Section::Tool => "this tool",
        Section::Hardware => "hardware",
        Section::Services => "services",
        Section::Protection => "protection",
        Section::Root => "root-only",
        Section::Boot => "boot & BIOS",
        Section::Proof => "proof",
    }
}

pub fn doctor_candidates(typed: &[String]) -> Vec<Candidate> {
    let typed: std::collections::HashSet<&str> = typed.iter().map(String::as_str).collect();
    let mut rows = Vec::new();
    for section in [
        Section::Tool,
        Section::Hardware,
        Section::Services,
        Section::Protection,
        Section::Root,
        Section::Boot,
        Section::Proof,
    ] {
        let entries: Vec<_> = doctor::catalog()
            .iter()
            .filter(|entry| entry.section == section)
            .collect();
        if entries.is_empty() {
            continue;
        }
        let group = section_group(section);
        let slug = section.slug();
        if !typed.contains(slug) {
            rows.push(Candidate {
                value: slug.to_owned(),
                description: format!("All {} checks in this section", entries.len()),
                group: group.to_owned(),
                nospace: false,
            });
        }
        for entry in entries {
            if typed.contains(entry.id) {
                continue;
            }
            rows.push(Candidate {
                value: entry.id.to_owned(),
                description: entry.title.to_owned(),
                group: group.to_owned(),
                nospace: false,
            });
        }
    }
    rows
}

#[cfg(test)]
pub fn find_command<'a>(root: &'a Command, path: &[&str]) -> Option<&'a Command> {
    let mut node = root;
    for name in path {
        node = node.find_subcommand(name)?;
    }
    Some(node)
}

#[cfg(test)]
pub fn find_arg<'a>(command: &'a Command, id: &str) -> Option<&'a Arg> {
    command.get_arguments().find(|arg| arg.get_id() == id)
}

/// Walk every catalog table against the clap tree. Returns human-readable
/// drift errors; empty means the tables match the tree.
#[cfg(test)]
pub fn drift_errors() -> Vec<String> {
    let mut tree = Cli::command();
    tree.build();
    let mut errors = Vec::new();
    for entry in REFUSED {
        match find_command(&tree, entry.path) {
            None => errors.push(format!(
                "refused flag {} on missing command path {}",
                entry.arg,
                entry.path.join(" ")
            )),
            Some(command) => {
                if find_arg(command, entry.arg).is_none() {
                    errors.push(format!(
                        "refused flag {} is not an argument of {}",
                        entry.arg,
                        entry.path.join(" ")
                    ));
                }
            }
        }
    }
    for entry in SUGGESTIONS {
        match find_command(&tree, entry.path) {
            None => errors.push(format!(
                "suggestion table path {} is missing",
                entry.path.join(" ")
            )),
            Some(command) => match find_arg(command, entry.arg) {
                None => errors.push(format!(
                    "suggestion argument {} is missing on {}",
                    entry.arg,
                    display_path(entry.path)
                )),
                Some(arg) => {
                    let possible: Vec<String> = arg
                        .get_possible_values()
                        .iter()
                        .filter(|value| !value.is_hide_set())
                        .map(|value| value.get_name().to_owned())
                        .collect();
                    if !possible.is_empty() {
                        for (name, _) in entry.values {
                            if !possible.iter().any(|value| value == name) {
                                errors.push(format!(
                                    "suggestion {name} is not a possible value of {} {}",
                                    display_path(entry.path),
                                    entry.arg
                                ));
                            }
                        }
                    }
                }
            },
        }
    }
    for (path, arg, _) in PATH_DIRECTIVES {
        match find_command(&tree, path) {
            None => errors.push(format!(
                "path-directive command {} is missing",
                path.join(" ")
            )),
            Some(command) => {
                if find_arg(command, arg).is_none() {
                    errors.push(format!(
                        "path-directive argument {arg} is missing on {}",
                        display_path(path)
                    ));
                }
            }
        }
    }
    walk_possible_values(&tree, &[], &mut errors);
    errors
}

#[cfg(test)]
fn display_path(path: &[&str]) -> String {
    if path.is_empty() {
        "nut".to_owned()
    } else {
        format!("nut {}", path.join(" "))
    }
}

#[cfg(test)]
fn walk_possible_values(command: &Command, path: &[&str], errors: &mut Vec<String>) {
    for arg in command.get_arguments() {
        if arg.is_hide_set() {
            continue;
        }
        let id = arg.get_id();
        for value in arg.get_possible_values() {
            if value.is_hide_set() {
                continue;
            }
            let name = value.get_name();
            let clap_help = value
                .get_help()
                .map(ToString::to_string)
                .unwrap_or_default();
            if clap_help.is_empty() && value_description(path, id.as_str(), name).is_none() {
                errors.push(format!(
                    "possible value {name} of {} {id} has no description from clap or the table",
                    display_path(path)
                ));
            }
        }
    }
    for child in command
        .get_subcommands()
        .filter(|child| !child.is_hide_set())
    {
        let mut next = path.to_vec();
        next.push(child.get_name());
        walk_possible_values(child, &next, errors);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_tables_match_the_clap_tree() {
        let errors = drift_errors();
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

    #[test]
    fn refused_flags_still_exist() {
        let mut tree = Cli::command();
        tree.build();
        for entry in REFUSED {
            let command = find_command(&tree, entry.path)
                .unwrap_or_else(|| panic!("missing {}", entry.path.join(" ")));
            assert!(
                find_arg(command, entry.arg).is_some(),
                "{} --{} vanished from clap",
                entry.path.join(" "),
                entry.arg
            );
        }
    }

    #[test]
    fn doctor_groups_follow_catalog_order() {
        let rows = doctor_candidates(&[]);
        let mut groups = Vec::new();
        for row in &rows {
            if groups.last() != Some(&row.group.as_str()) {
                groups.push(row.group.as_str());
            }
        }
        assert_eq!(
            groups,
            [
                "this tool",
                "hardware",
                "services",
                "protection",
                "root-only",
                "boot & BIOS",
                "proof",
            ]
        );
        assert!(
            rows.iter()
                .any(|row| row.value == "hw" && row.description.starts_with("All "))
        );
        assert!(
            rows.iter()
                .any(|row| row.value == "hw.usb"
                    && row.description == "Exactly one matching USB UPS")
        );
        let without_usb = doctor_candidates(&["hw.usb".into()]);
        assert!(without_usb.iter().all(|row| row.value != "hw.usb"));
        assert!(without_usb.iter().any(|row| row.value == "hw"));
    }
}
