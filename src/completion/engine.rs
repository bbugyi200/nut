//! Slot resolution without `clap_complete`: direct `Command`/`Arg`
//! introspection builds the same `Menu` the protocol serializes.
use std::ffi::OsString;

use clap::{Arg, ArgAction, Command, CommandFactory, ValueHint};

use super::menu::{Group, Hint, Item, Menu, normalize_description};
use super::protocol::Request;
use super::providers::{Context, Provider, ProviderOutput};
use crate::cli::{COMMAND_GROUPS, Cli};

/// Explicitly refused flags: always offered nowhere.
#[allow(dead_code)]
const REFUSED: &[(&[&str], &str)] = &[(&["drill"], "yes"), (&["bios", "setup"], "yes")];

fn is_refused(path: &[String], arg_id: &str) -> bool {
    REFUSED.iter().any(|(ref_path, ref_arg)| {
        *ref_arg == arg_id
            && ref_path.len() == path.len()
            && ref_path
                .iter()
                .zip(path.iter())
                .all(|(expected, actual)| *expected == actual.as_str())
    })
}

#[cfg(test)]
pub(super) fn refused_table() -> &'static [(&'static [&'static str], &'static str)] {
    REFUSED
}

fn tag_for_heading(heading: &str) -> String {
    match heading {
        "Monitor" => "nut-monitor".into(),
        "Act" => "nut-act".into(),
        "Set up" => "nut-set-up".into(),
        "Prove" => "nut-prove".into(),
        "Shell" => "nut-shell".into(),
        "Commands" => "nut-commands".into(),
        "Options" => "nut-options".into(),
        "Global options" => "nut-global-options".into(),
        _ => format!(
            "nut-{}",
            heading
                .to_lowercase()
                .replace(|ch: char| !ch.is_ascii_alphanumeric(), "-")
                .trim_matches('-')
        ),
    }
}

fn help_text(arg: &Arg) -> String {
    let raw = arg
        .get_help()
        .map(ToString::to_string)
        .or_else(|| arg.get_long_help().map(ToString::to_string))
        .unwrap_or_default();
    normalize_description(&raw)
}

fn about_text(command: &Command) -> String {
    let raw = command
        .get_about()
        .map(ToString::to_string)
        .unwrap_or_default();
    let normalized = normalize_description(&raw);
    if normalized.is_empty() && command.get_name() == "help" {
        return "Print help".into();
    }
    normalized
}

fn value_name(arg: &Arg) -> Option<String> {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.as_str().to_owned())
}

fn takes_values(arg: &Arg) -> bool {
    arg.get_action().takes_values()
}

fn option_label(arg: &Arg, short_slot: bool) -> String {
    let long = arg
        .get_long()
        .map(|long| format!("--{long}"))
        .unwrap_or_default();
    let mut label = if short_slot {
        match arg.get_short() {
            Some(short) => format!("-{short}, {long}"),
            None => format!("    {long}"),
        }
    } else {
        long
    };
    if takes_values(arg)
        && let Some(name) = value_name(arg)
    {
        label.push_str(&format!(" <{name}>"));
    }
    label
}

fn option<'a>(command: &'a Command, word: &str) -> Option<(&'a Arg, bool)> {
    if let Some(long) = word.strip_prefix("--") {
        let (name, attached) = long
            .split_once('=')
            .map_or((long, false), |(name, _)| (name, true));
        // `--` alone is the options terminator, not an option.
        if name.is_empty() {
            return None;
        }
        command
            .get_arguments()
            .find(|arg| arg.get_long() == Some(name))
            .map(|arg| (arg, attached))
    } else {
        let mut short = word.strip_prefix('-')?.chars();
        let name = short.next()?;
        command
            .get_arguments()
            .find(|arg| arg.get_short() == Some(name))
            .map(|arg| (arg, short.next().is_some()))
    }
}

struct Parse {
    command_path: Vec<String>,
    pending: Option<String>,
    positional: usize,
    options_open: bool,
    present: Vec<String>,
}

fn parse_words(tree: &Command, before: &[OsString]) -> (Parse, Command) {
    let mut command = tree.clone();
    let mut path = Vec::new();
    let mut pending: Option<String> = None;
    let mut positional = 1;
    let mut options_open = true;
    let mut present = Vec::new();
    for word in before.iter().skip(1) {
        let text = word.to_string_lossy().into_owned();
        if let Some(id) = pending.take() {
            let _ = id;
            continue;
        }
        if options_open && text == "--" {
            options_open = false;
            continue;
        }
        if options_open && text.starts_with('-') && text.len() > 1 {
            if let Some((arg, attached)) = option(&command, &text) {
                let id = arg.get_id().to_string();
                if !present.contains(&id) {
                    present.push(id.clone());
                }
                if takes_values(arg) && !attached {
                    pending = Some(id);
                }
                continue;
            }
            // Unknown dash word: treat as positional noise, not a subcommand.
            continue;
        }
        if options_open && let Some(subcommand) = command.find_subcommand(&text) {
            path.push(subcommand.get_name().to_owned());
            command = subcommand.clone();
            positional = 1;
            continue;
        }
        // Positional word.
        if let Some(arg) = command
            .get_positionals()
            .find(|arg| arg.get_index() == Some(positional))
        {
            let multi = arg
                .get_num_args()
                .is_some_and(|range| range.max_values() > 1);
            if !multi {
                positional += 1;
            }
        } else {
            positional += 1;
        }
    }
    (
        Parse {
            command_path: path,
            pending,
            positional,
            options_open,
            present,
        },
        command,
    )
}

fn find_arg<'a>(command: &'a Command, id: &str) -> Option<&'a Arg> {
    command.get_arguments().find(|arg| arg.get_id() == id)
}

/// Build the menu plus an optional `!prefix` keep for the request.
pub(super) fn build_menu(
    tree: &Command,
    request: &Request,
    provider: &dyn Provider,
) -> (Option<usize>, Menu) {
    let Some((cursor, before)) = request.words.split_last() else {
        return (None, Menu::Empty);
    };
    let cursor = cursor.to_string_lossy().into_owned();
    let (parse, command) = parse_words(tree, before);
    // Attached option value (`--opt=` or `-oVALUE`).
    let mut keep: Option<usize> = None;
    let mut slot: Option<String> = parse.pending.clone();
    if slot.is_none()
        && parse.options_open
        && cursor.starts_with('-')
        && cursor.len() > 1
        && let Some((arg, true)) = option(&command, &cursor)
        && takes_values(arg)
    {
        keep = Some(cursor.find('=').map_or(2, |index| index + 1));
        slot = Some(arg.get_id().to_string());
    } else if slot.is_none() && !(parse.options_open && cursor.starts_with('-')) {
        slot = command
            .get_positionals()
            .find(|arg| arg.get_index() == Some(parse.positional))
            .map(|arg| arg.get_id().to_string());
    }
    if let Some(arg_id) = slot {
        let Some(arg) = find_arg(&command, &arg_id) else {
            return (None, Menu::Empty);
        };
        let context = Context {
            command: parse.command_path.clone(),
            argument: arg_id.clone(),
            words: request.words.clone(),
        };
        if let Some(output) = provider.values(&context) {
            match output {
                ProviderOutput::Groups(groups)
                    if groups.iter().any(|group| !group.items.is_empty()) =>
                {
                    return (
                        keep,
                        Menu::Groups(
                            groups
                                .into_iter()
                                .filter(|group| !group.items.is_empty())
                                .map(|group| Group {
                                    tag: group.tag,
                                    heading: group.heading,
                                    items: group
                                        .items
                                        .into_iter()
                                        .map(|item| Item {
                                            label: item.value.clone(),
                                            value: item.value,
                                            description: normalize_description(&item.description),
                                            nospace: false,
                                        })
                                        .collect(),
                                })
                                .collect(),
                        ),
                    );
                }
                ProviderOutput::Hint(hint) => {
                    return (
                        keep,
                        Menu::Hint(Hint {
                            label: hint.label,
                            text: normalize_description(&hint.text),
                        }),
                    );
                }
                _ => {}
            }
        }
        let possible: Vec<_> = arg.get_possible_values();
        if !possible.is_empty() {
            let (heading, tag) = super::providers::heading_for(&arg_id)
                .map(|(heading, tag)| (heading.to_owned(), tag.to_owned()))
                .unwrap_or_else(|| ("Values".into(), "nut-values".into()));
            // Multi-value positionals leave out values already given.
            let given: std::collections::BTreeSet<String> = request
                .words
                .iter()
                .skip(1)
                .map(|word| word.to_string_lossy().into_owned())
                .collect();
            let items: Vec<Item> = possible
                .into_iter()
                .filter(|value| !given.contains(value.get_name()))
                .map(|value| {
                    let description = value
                        .get_help()
                        .map(ToString::to_string)
                        .map(|help| normalize_description(&help))
                        .unwrap_or_default();
                    Item {
                        value: value.get_name().to_owned(),
                        label: value.get_name().to_owned(),
                        description,
                        nospace: false,
                    }
                })
                .collect();
            if !items.is_empty() {
                return (
                    keep,
                    Menu::Groups(vec![Group {
                        tag,
                        heading,
                        items,
                    }]),
                );
            }
        }
        match arg.get_value_hint() {
            ValueHint::DirPath => return (keep, Menu::Dirs),
            ValueHint::FilePath | ValueHint::AnyPath => return (keep, Menu::Files(None)),
            _ => {}
        }
        let label = value_name(arg).unwrap_or_else(|| arg_id.to_uppercase());
        let text = help_text(arg);
        let text = if text.is_empty() {
            format!("Value for {arg_id}")
        } else {
            text
        };
        return (keep, Menu::Hint(Hint { label, text }));
    }
    if parse.options_open && cursor.starts_with('-') {
        // `-` and `--…` are long-form; `-x…` (single dash, non-dash next) is short-form.
        let short_form = cursor.len() > 1
            && !cursor.starts_with("--")
            && cursor.chars().nth(1).is_some_and(|ch| ch != '-');
        return (None, option_menu(tree, &command, &parse, short_form));
    }
    // Bare word.
    let visible: Vec<_> = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .collect();
    if !visible.is_empty() {
        if parse.command_path.is_empty() {
            let mut groups = Vec::new();
            for (heading, names) in COMMAND_GROUPS {
                let items: Vec<Item> = names
                    .iter()
                    .filter_map(|name| {
                        visible
                            .iter()
                            .find(|subcommand| subcommand.get_name() == *name)
                    })
                    .map(|subcommand| Item {
                        value: subcommand.get_name().to_owned(),
                        label: subcommand.get_name().to_owned(),
                        description: about_text(subcommand),
                        nospace: false,
                    })
                    .collect();
                if !items.is_empty() {
                    groups.push(Group {
                        tag: tag_for_heading(heading),
                        heading: (*heading).to_owned(),
                        items,
                    });
                }
            }
            return (None, Menu::Groups(groups));
        }
        return (
            None,
            Menu::Groups(vec![Group {
                tag: "nut-commands".into(),
                heading: "Commands".into(),
                items: visible
                    .iter()
                    .map(|subcommand| Item {
                        value: subcommand.get_name().to_owned(),
                        label: subcommand.get_name().to_owned(),
                        description: about_text(subcommand),
                        nospace: false,
                    })
                    .collect(),
            }]),
        );
    }
    // Positional slot remains.
    if command
        .get_positionals()
        .any(|arg| arg.get_index() == Some(parse.positional))
    {
        let arg_id = command
            .get_positionals()
            .find(|arg| arg.get_index() == Some(parse.positional))
            .map(|arg| arg.get_id().to_string())
            .unwrap_or_default();
        if let Some(arg) = find_arg(&command, &arg_id) {
            let context = Context {
                command: parse.command_path.clone(),
                argument: arg_id.clone(),
                words: request.words.clone(),
            };
            if let Some(output) = provider.values(&context) {
                match output {
                    ProviderOutput::Groups(groups)
                        if groups.iter().any(|group| !group.items.is_empty()) =>
                    {
                        return (
                            None,
                            Menu::Groups(
                                groups
                                    .into_iter()
                                    .filter(|group| !group.items.is_empty())
                                    .map(|group| Group {
                                        tag: group.tag,
                                        heading: group.heading,
                                        items: group
                                            .items
                                            .into_iter()
                                            .map(|item| Item {
                                                label: item.value.clone(),
                                                value: item.value,
                                                description: normalize_description(
                                                    &item.description,
                                                ),
                                                nospace: false,
                                            })
                                            .collect(),
                                    })
                                    .collect(),
                            ),
                        );
                    }
                    ProviderOutput::Hint(hint) => {
                        return (
                            None,
                            Menu::Hint(Hint {
                                label: hint.label,
                                text: normalize_description(&hint.text),
                            }),
                        );
                    }
                    _ => {}
                }
            }
            let possible = arg.get_possible_values();
            if !possible.is_empty() {
                let (heading, tag) = super::providers::heading_for(&arg_id)
                    .map(|(heading, tag)| (heading.to_owned(), tag.to_owned()))
                    .unwrap_or_else(|| ("Values".into(), "nut-values".into()));
                return (
                    None,
                    Menu::Groups(vec![Group {
                        tag,
                        heading,
                        items: possible
                            .into_iter()
                            .map(|value| {
                                let description = value
                                    .get_help()
                                    .map(ToString::to_string)
                                    .map(|help| normalize_description(&help))
                                    .unwrap_or_default();
                                Item {
                                    value: value.get_name().to_owned(),
                                    label: value.get_name().to_owned(),
                                    description,
                                    nospace: false,
                                }
                            })
                            .collect(),
                    }]),
                );
            }
            match arg.get_value_hint() {
                ValueHint::DirPath => return (None, Menu::Dirs),
                ValueHint::FilePath | ValueHint::AnyPath => return (None, Menu::Files(None)),
                _ => {}
            }
            let label = value_name(arg).unwrap_or_else(|| arg_id.to_uppercase());
            let text = help_text(arg);
            return (
                None,
                Menu::Hint(Hint {
                    label,
                    text: if text.is_empty() {
                        format!("Value for {arg_id}")
                    } else {
                        text
                    },
                }),
            );
        }
    }
    // Leaf: Tab is never dead.
    (None, option_menu(tree, &command, &parse, false))
}

fn visible_option(command: &Command, parse: &Parse, arg: &Arg) -> bool {
    if arg.is_hide_set() {
        return false;
    }
    let id = arg.get_id();
    if id == "help" || id == "version" {
        return true;
    }
    if is_refused(&parse.command_path, id.as_str()) {
        return false;
    }
    let present = parse.present.iter().any(|present| present == id);
    if present && !matches!(arg.get_action(), ArgAction::Append | ArgAction::Count) {
        return false;
    }
    // Conflicts in either direction.
    for present_id in &parse.present {
        if present_id == id {
            continue;
        }
        let Some(present_arg) = find_arg(command, present_id) else {
            continue;
        };
        let candidate_conflicts = command.get_arg_conflicts_with(arg);
        if candidate_conflicts
            .iter()
            .any(|conflict| conflict.get_id() == present_id)
        {
            return false;
        }
        let present_conflicts = command.get_arg_conflicts_with(present_arg);
        if present_conflicts
            .iter()
            .any(|conflict| conflict.get_id() == id)
        {
            return false;
        }
    }
    true
}

fn option_menu(tree: &Command, command: &Command, parse: &Parse, short_form: bool) -> Menu {
    let is_root = parse.command_path.is_empty();
    let mut candidates: Vec<&Arg> = command
        .get_arguments()
        .filter(|arg| visible_option(command, parse, arg))
        .collect();
    // Clap yields help/version alongside the rest; keep them last at the root.
    candidates.sort_by_key(|arg| {
        let id = arg.get_id();
        if id == "help" {
            2
        } else if id == "version" {
            3
        } else {
            0
        }
    });
    if short_form {
        candidates.retain(|arg| arg.get_short().is_some());
        if candidates.is_empty() {
            return Menu::Empty;
        }
    }
    if is_root {
        let any_short = candidates.iter().any(|arg| arg.get_short().is_some());
        let items: Vec<Item> = candidates
            .into_iter()
            .map(|arg| {
                let value = format!("--{}", arg.get_long().unwrap_or_default());
                let (value, label) = if short_form {
                    let short = format!("-{}", arg.get_short().unwrap_or('?'));
                    (short.clone(), option_label(arg, true))
                } else {
                    (value, option_label(arg, any_short))
                };
                Item {
                    value,
                    label,
                    description: help_text(arg),
                    nospace: false,
                }
            })
            .filter(|item| !item.value.is_empty() && item.value != "--")
            .collect();
        if items.is_empty() {
            return Menu::Empty;
        }
        return Menu::Groups(vec![Group {
            tag: "nut-options".into(),
            heading: "Options".into(),
            items,
        }]);
    }
    let root = tree.clone();
    let _ = root;
    let locals: Vec<&&Arg> = candidates
        .iter()
        .filter(|arg| !arg.is_global_set() && arg.get_id() != "help" && arg.get_id() != "version")
        .collect();
    let globals: Vec<&&Arg> = candidates
        .iter()
        .filter(|arg| arg.is_global_set())
        .collect();
    let help: Vec<&&Arg> = candidates
        .iter()
        .filter(|arg| arg.get_id() == "help" || arg.get_id() == "version")
        .collect();
    let any_short = candidates.iter().any(|arg| arg.get_short().is_some());
    let mut groups = Vec::new();
    let mut local_items: Vec<Item> = locals
        .into_iter()
        .map(|arg| {
            let value = format!("--{}", arg.get_long().unwrap_or_default());
            let (value, label) = if short_form {
                let short = format!("-{}", arg.get_short().unwrap_or('?'));
                (short.clone(), option_label(arg, true))
            } else {
                (value, option_label(arg, any_short))
            };
            Item {
                value,
                label,
                description: help_text(arg),
                nospace: false,
            }
        })
        .filter(|item| !item.value.is_empty() && item.value != "--")
        .collect();
    for arg in help {
        let value = format!("--{}", arg.get_long().unwrap_or_default());
        if value == "--" || value.is_empty() {
            continue;
        }
        let (value, label) = if short_form {
            let Some(short) = arg.get_short() else {
                continue;
            };
            (format!("-{short}"), option_label(arg, true))
        } else {
            (value, option_label(arg, any_short))
        };
        local_items.push(Item {
            value,
            label,
            description: help_text(arg),
            nospace: false,
        });
    }
    if !local_items.is_empty() {
        groups.push(Group {
            tag: "nut-options".into(),
            heading: "Options".into(),
            items: local_items,
        });
    }
    let global_items: Vec<Item> = globals
        .into_iter()
        .filter_map(|arg| {
            let value = format!("--{}", arg.get_long().unwrap_or_default());
            let (value, label) = if short_form {
                let short = arg.get_short()?;
                (format!("-{short}"), option_label(arg, true))
            } else {
                (value, option_label(arg, any_short))
            };
            Some(Item {
                value,
                label,
                description: help_text(arg),
                nospace: false,
            })
        })
        .filter(|item| !item.value.is_empty() && item.value != "--")
        .collect();
    if !global_items.is_empty() {
        groups.push(Group {
            tag: "nut-global-options".into(),
            heading: "Global options".into(),
            items: global_items,
        });
    }
    if groups.is_empty() {
        return Menu::Empty;
    }
    Menu::Groups(groups)
}

pub(super) fn complete(request: &Request, provider: &dyn Provider) -> (Option<usize>, Menu) {
    let mut tree = Cli::command();
    tree.build();
    build_menu(&tree, request, provider)
}

/// The root `nut <Tab>` menu, grouped like `nut --help` for docs captures.
pub fn root_menu() -> Menu {
    let mut tree = Cli::command();
    tree.build();
    let visible: Vec<_> = tree
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .collect();
    let mut groups = Vec::new();
    for (heading, names) in COMMAND_GROUPS {
        let items: Vec<Item> = names
            .iter()
            .filter_map(|name| {
                visible
                    .iter()
                    .find(|subcommand| subcommand.get_name() == *name)
            })
            .map(|subcommand| Item {
                value: subcommand.get_name().to_owned(),
                label: subcommand.get_name().to_owned(),
                description: about_text(subcommand),
                nospace: false,
            })
            .collect();
        if !items.is_empty() {
            groups.push(Group {
                tag: tag_for_heading(heading),
                heading: (*heading).to_owned(),
                items,
            });
        }
    }
    Menu::Groups(groups)
}

#[cfg(test)]
mod tests {
    use super::super::protocol::Request;
    use super::*;
    use crate::completion::providers::{Provider, ProviderOutput};
    use crate::host::FakeHost;

    struct EmptyProvider;
    impl Provider for EmptyProvider {
        fn values(
            &self,
            _context: &crate::completion::providers::Context,
        ) -> Option<ProviderOutput> {
            None
        }
    }

    fn menu_for(words: &[&str]) -> (Option<usize>, Menu) {
        let mut tree = Cli::command();
        tree.build();
        let request = Request {
            shell: crate::completion::Shell::Zsh,
            protocol: 2,
            suffix: None,
            columns: 100,
            words: words.iter().map(OsString::from).collect(),
        };
        build_menu(&tree, &request, &EmptyProvider)
    }

    fn screen(words: &[&str], columns: usize) -> String {
        let (_, menu) = menu_for(words);
        crate::completion::menu::virtual_screen(&menu, columns)
    }

    #[test]
    fn refused_entries_exist_in_clap_tree() {
        let mut tree = Cli::command();
        tree.build();
        for (path, arg_id) in refused_table() {
            let mut command = &tree;
            for name in *path {
                command = command
                    .find_subcommand(name)
                    .unwrap_or_else(|| panic!("refused path {path:?} missing {name}"));
            }
            assert!(
                command.get_arguments().any(|arg| arg.get_id() == *arg_id),
                "refused {path:?} {arg_id} missing from clap tree"
            );
        }
    }

    #[test]
    fn representative_menus_match_target_look() {
        // Root lists only the 12 commands (commands_first), grouped like --help.
        let root = screen(&["nut", ""], 100);
        for heading in ["Monitor", "Act", "Set up", "Prove", "Shell"] {
            assert!(root.contains(heading), "{root}");
        }
        insta::assert_snapshot!("menu_root_100", root);
        insta::assert_snapshot!("menu_dash_100", screen(&["nut", "-"], 100));
        insta::assert_snapshot!("menu_dash_c_100", screen(&["nut", "-c"], 100));
        insta::assert_snapshot!("menu_status_empty_100", screen(&["nut", "status", ""], 100));
        insta::assert_snapshot!("menu_status_dash_100", screen(&["nut", "status", "-"], 100));
        insta::assert_snapshot!(
            "menu_events_outages_100",
            screen(&["nut", "events", "-o", "--"], 100)
        );
        insta::assert_snapshot!(
            "menu_status_watch_100",
            screen(&["nut", "status", "--watch", "--"], 100)
        );
        insta::assert_snapshot!("menu_drill_dash_100", screen(&["nut", "drill", "-"], 100));
        insta::assert_snapshot!("menu_bios_empty_100", screen(&["nut", "bios", ""], 100));
    }

    #[test]
    fn width_cases_keep_shared_column_and_truncate() {
        let root50 = screen(&["nut", ""], 50);
        let root30 = screen(&["nut", ""], 30);
        for line in root50.lines().chain(root30.lines()) {
            assert!(crate::ui::layout::display_width(line) <= 49, "wrap: {line}");
        }
        insta::assert_snapshot!("menu_root_50", root50);
        insta::assert_snapshot!("menu_root_30", root30);
    }

    #[test]
    fn tree_descriptions_are_never_blank() {
        use clap::CommandFactory;
        let mut tree = Cli::command();
        tree.build();
        fn walk(command: &clap::Command) {
            for sub in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
                let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
                assert!(!about.is_empty(), "{} has no description", sub.get_name());
                walk(sub);
            }
            for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
                let help = arg
                    .get_help()
                    .map(ToString::to_string)
                    .or_else(|| arg.get_long_help().map(ToString::to_string))
                    .unwrap_or_default();
                assert!(
                    !normalize_description(&help).is_empty(),
                    "{} has no description",
                    arg.get_id()
                );
                for value in arg.get_possible_values() {
                    let help = value
                        .get_help()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    assert!(
                        !normalize_description(&help).is_empty(),
                        "{} value {} has no description",
                        arg.get_id(),
                        value.get_name()
                    );
                }
            }
        }
        walk(&tree);
    }

    #[test]
    fn tags_are_nut_prefixed_and_headings_never_generic() {
        let cases: &[&[&str]] = &[
            &["nut", ""],
            &["nut", "-"],
            &["nut", "status", "-"],
            &["nut", "doctor", ""],
            &["nut", "--color", ""],
        ];
        for words in cases {
            let (_, menu) = menu_for(words);
            let Menu::Groups(groups) = menu else {
                continue;
            };
            for group in groups {
                assert!(
                    super::super::protocol::validate_tag(&group.tag),
                    "bad tag {}",
                    group.tag
                );
                assert!(!group.heading.is_empty() && group.heading != "Values");
            }
        }
    }

    #[test]
    fn rendering_invariants_hold_from_30_to_120_columns() {
        let cases: &[&[&str]] = &[
            &["nut", ""],
            &["nut", "-"],
            &["nut", "status", "-"],
            &["nut", "doctor", ""],
        ];
        for columns in (30..=120).step_by(10) {
            for words in cases {
                let (_, menu) = menu_for(words);
                let Menu::Groups(groups) = &menu else {
                    continue;
                };
                let col = crate::completion::menu::shared_column(groups, columns);
                for group in groups {
                    for item in &group.items {
                        let zsh = crate::completion::menu::zsh_display(
                            &item.label,
                            &item.description,
                            col,
                            columns,
                            true,
                        );
                        assert!(
                            crate::ui::layout::display_width(&zsh) <= columns - 1,
                            "{zsh}"
                        );
                        // Every zsh display is indented two spaces; descriptions
                        // themselves never carry two spaces (color pattern).
                        assert!(zsh.starts_with("  "), "{zsh}");
                        assert!(!item.description.contains("  "), "{}", item.description);
                        let bash = crate::completion::menu::bash_display(
                            &item.value,
                            &item.description,
                            col,
                            columns,
                            true,
                        );
                        assert!(
                            crate::ui::layout::display_width(&bash) <= columns - 1,
                            "{bash}"
                        );
                        assert!(
                            bash.starts_with(&item.value),
                            "bash display must start with value: {bash}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn live_providers_cover_representative_slots() {
        let host = FakeHost::default();
        let provider = crate::completion::providers::LiveProviders(&host);
        for (command, argument, words) in [
            (
                vec!["doctor".to_owned()],
                "checks",
                vec!["nut", "doctor", ""],
            ),
            (
                vec!["rollback".to_owned()],
                "stamp",
                vec!["nut", "rollback", ""],
            ),
            (vec![], "since", vec!["nut", "events", "--since", ""]),
            (vec![], "color", vec!["nut", "--color", ""]),
        ] {
            let context = crate::completion::providers::Context {
                command,
                argument: argument.to_owned(),
                words: words.iter().map(OsString::from).collect(),
            };
            // Color falls through to clap; the rest are provider-owned.
            let owned = provider.values(&context);
            if argument != "color" {
                assert!(owned.is_some(), "{argument} must be provider-owned");
            }
        }
    }
}
