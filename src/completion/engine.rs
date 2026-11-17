//! Presenter for `__complete` requests.
//!
//! The engine walks clap's public `Command`/`Arg` API and tracks the
//! subcommand node, path, present argument IDs, the pending value
//! argument (honoring `allow_hyphen_values`), a `--` escape, filled
//! positionals, and typed var-arg values. Then it applies these rules:
//!
//! 1. **Root empty word → commands only**, grouped by
//!    `cli::COMMAND_GROUPS` in that table's order, so it matches
//!    `nut --help`. Group names are the headings lowercased.
//! 2. **Plain word below the root:** subcommands (group `commands`) if
//!    the command has any. Otherwise offer the next positional's values
//!    if that slot has a value source. Otherwise offer options.
//! 3. **Lone-action guard.** If rule 2 would offer exactly one
//!    subcommand of a command that also runs bare (`nut battery`,
//!    `nut notify`), offer that command's options as well.
//! 4. **`-` word → options with short/long pairs**, emitted adjacent
//!    with identical descriptions (short first). A `--…` word offers
//!    long forms only.
//! 5. **Option groups.** At the root, every option goes in `options`.
//!    Below the root, the command's own arguments (including its auto
//!    `--help`) go in `options` and propagated globals go in
//!    `global options`. Each group is sorted alphabetically by long
//!    name, with short-only arguments sorted by their short character.
//! 6. **Option filtering.** Never offer hidden arguments; refused flags
//!    (`drill --yes`, `bios setup --yes`); options already present
//!    (unless `Append`/`Count`); options that conflict, in either
//!    direction, with a present option (`catch_unwind` around
//!    `get_arg_conflicts_with`); or `--help`/`-h` and `--version`/`-V`
//!    once any word follows the subcommand path.
//! 7. **Value slots win.** A pending option always completes its value,
//!    including a cursor starting with `-` for `allow_hyphen_values`
//!    options. `--opt=<Tab>` and short attached forms keep their prefix
//!    through `!prefix N`.
//! 8. **After `--`** only positional values are offered.
//! 9. **Var-arg dedupe.** `doctor` does not re-offer check IDs or
//!    section slugs already typed.
//! 10. **Descriptions** come from clap's short help: first line, drop
//!     backticks, drop a trailing period. The auto `help` arg is always
//!     "Print help" and `version` is "Print version". Every offered row
//!     has a non-empty description. Within a group, descriptions are
//!     unique except for a short/long pair of the same argument.
//! 11. **Empty is explained.** A slot that cannot offer candidates
//!     emits one `!message` instead of an empty menu, except where
//!     nothing at all can follow.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};

use clap::{Arg, ArgAction, Command, CommandFactory, ValueHint};

use super::catalog::{self, PathDirective};
use super::protocol::{self, Request};
use super::providers::{Context, Provider, Slot};
use crate::cli::{COMMAND_GROUPS, Cli};

pub(super) fn complete(request: &Request, provider: &dyn Provider) -> Vec<String> {
    let mut root = Cli::command();
    root.build();
    let Some((cursor, rest)) = request.words.split_last() else {
        return Vec::new();
    };
    let before: &[OsString] = if rest.len() > 1 { &rest[1..] } else { &[] };
    let _ = (request.shell, &request.suffix);
    let globals = global_ids(&root);
    let mut walk = Walk::new(&root, before, provider, globals);
    for (index, word) in before.iter().enumerate() {
        walk.step(word, index);
    }
    walk.finish(cursor)
}

struct Walk<'a> {
    root: &'a Command,
    node: &'a Command,
    provider: &'a dyn Provider,
    before: &'a [OsString],
    path: Vec<String>,
    present: Vec<String>,
    pending: Option<String>,
    escaped: bool,
    positional_words: usize,
    path_words: usize,
    typed: Vec<String>,
    globals: HashSet<String>,
}

impl<'a> Walk<'a> {
    fn new(
        root: &'a Command,
        before: &'a [OsString],
        provider: &'a dyn Provider,
        globals: HashSet<String>,
    ) -> Self {
        Self {
            root,
            node: root,
            provider,
            before,
            path: Vec::new(),
            present: Vec::new(),
            pending: None,
            escaped: false,
            positional_words: 0,
            path_words: 0,
            typed: Vec::new(),
            globals,
        }
    }

    fn push_present(&mut self, id: &str) {
        if !self.present.iter().any(|seen| seen == id) {
            self.present.push(id.to_owned());
        }
    }

    fn step(&mut self, word: &OsString, index: usize) {
        let text = word.to_string_lossy();
        if self.escaped {
            self.fill_positional(text.as_ref());
            return;
        }
        if let Some(id) = self.pending.take() {
            let hyphen_ok = find_by_id(self.node, &id).is_some_and(Arg::is_allow_hyphen_values_set);
            if hyphen_ok || !(text.starts_with('-') && text != "-") {
                return;
            }
        }
        if text == "--" {
            self.escaped = true;
            return;
        }
        if let Some(stripped) = text.strip_prefix("--") {
            let (name, inline) = match stripped.split_once('=') {
                Some((name, inline)) => (name, Some(inline)),
                None => (stripped, None),
            };
            if let Some(arg) = find_long(self.node, name) {
                let id = arg.get_id().to_string();
                self.push_present(&id);
                if takes_values(arg) && inline.is_none() {
                    self.pending = Some(id);
                }
            }
            return;
        }
        if text.starts_with('-') && text != "-" {
            let chars: Vec<char> = text[1..].chars().collect();
            let mut index = 0;
            while index < chars.len() {
                let Some(arg) = find_short(self.node, chars[index]) else {
                    break;
                };
                let id = arg.get_id().to_string();
                self.push_present(&id);
                if takes_values(arg) {
                    let rest: String = chars[index + 1..].iter().collect();
                    if rest.strip_prefix('=').unwrap_or(&rest).is_empty() {
                        self.pending = Some(id);
                    }
                    break;
                }
                index += 1;
            }
            return;
        }
        if let Some(next) = self.node.find_subcommand(word) {
            self.node = next;
            self.path.push(next.get_name().to_owned());
            self.path_words = index + 1;
            self.positional_words = 0;
            self.typed.clear();
            return;
        }
        self.fill_positional(text.as_ref());
    }

    fn fill_positional(&mut self, text: &str) {
        if let Some(arg) = peek_positional(self.node, self.positional_words) {
            if is_var_arg(arg) {
                self.typed.push(text.to_owned());
            } else {
                self.positional_words += 1;
            }
        }
    }

    fn finish(&self, cursor: &OsStr) -> Vec<String> {
        let cursor_text = cursor.to_string_lossy();
        if let Some(stripped) = cursor_text.strip_prefix("--")
            && let Some((name, _)) = stripped.split_once('=')
            && !name.is_empty()
            && let Some(arg) = find_long(self.node, name)
            && takes_values(arg)
        {
            let prefix = format!("--{name}=");
            return self.value_lines(arg, Some(&prefix));
        }
        if cursor_text.starts_with('-') && !cursor_text.starts_with("--") && cursor_text != "-" {
            let rest = &cursor_text[1..];
            if let Some(name) = rest.chars().next()
                && let Some(arg) = find_short(self.node, name)
                && takes_values(arg)
                && (rest.chars().count() > 1 || rest.contains('='))
            {
                let keep = cursor_text.find('=').map_or(2, |index| index + 1);
                let prefix: String = cursor_text.chars().take(keep).collect();
                return self.value_lines(arg, Some(&prefix));
            }
        }
        if let Some(id) = &self.pending
            && let Some(arg) = find_by_id(self.node, id)
            && self.pending_takes_cursor(&cursor_text, arg)
        {
            return self.value_lines(arg, None);
        }
        if self.escaped {
            return self.text_lines();
        }
        if cursor_text.starts_with('-') {
            return self.option_lines(cursor_text.starts_with("--"));
        }
        self.commands_or_value_lines()
    }

    fn pending_takes_cursor(&self, cursor: &str, arg: &Arg) -> bool {
        if !cursor.starts_with('-') {
            return true;
        }
        arg.is_allow_hyphen_values_set() && !cursor.starts_with("--")
    }

    fn option_lines(&self, longs_only: bool) -> Vec<String> {
        let others_exist = self.before.len() > self.path_words;
        let mut local = Vec::new();
        let mut global = Vec::new();
        for arg in self.node.get_arguments() {
            if arg.is_hide_set() || arg.is_positional() {
                continue;
            }
            let id = arg.get_id().to_string();
            if catalog::is_refused(&self.path, &id) {
                continue;
            }
            if self.present.iter().any(|seen| seen == &id)
                && !matches!(arg.get_action(), ArgAction::Append | ArgAction::Count)
            {
                continue;
            }
            if self.conflicts_with_present(arg) {
                continue;
            }
            if others_exist && (id == "help" || id == "version") {
                continue;
            }
            if arg.get_long().is_none() && arg.get_short().is_none() {
                continue;
            }
            if self.globals.contains(&id) && !self.path.is_empty() {
                global.push(arg);
            } else {
                local.push(arg);
            }
        }
        sort_options(&mut local);
        sort_options(&mut global);
        let mut lines = Vec::new();
        self.emit_option_group(&mut lines, &local, "options", longs_only);
        if !self.path.is_empty() {
            self.emit_option_group(&mut lines, &global, "global options", longs_only);
        }
        lines
    }

    fn emit_option_group(
        &self,
        lines: &mut Vec<String>,
        args: &[&Arg],
        group: &str,
        longs_only: bool,
    ) {
        for arg in args {
            let help = arg_description(arg);
            if !longs_only && let Some(short) = arg.get_short() {
                push_candidate(lines, &format!("-{short}"), &help, group);
            }
            if let Some(long) = arg.get_long() {
                push_candidate(lines, &format!("--{long}"), &help, group);
            }
        }
    }

    fn commands_or_value_lines(&self) -> Vec<String> {
        let subs = visible_subcommands(self.node);
        if self.path.is_empty() {
            return self.root_command_lines();
        }
        if !subs.is_empty() {
            let mut lines = self.subcommand_lines(&subs);
            if subs.len() == 1 && !self.node.is_subcommand_required_set() {
                lines.extend(self.option_lines(false));
            }
            return lines;
        }
        if let Some(positional) = peek_positional(self.node, self.positional_words) {
            let lines = self.value_lines(positional, None);
            if !is_free_text_only(&lines, positional) {
                return lines;
            }
        }
        self.option_lines(false)
    }

    fn root_command_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for &(heading, names) in COMMAND_GROUPS {
            let group = heading.to_lowercase();
            for name in names {
                let Some(subcommand) = self.root.find_subcommand(name) else {
                    continue;
                };
                if subcommand.is_hide_set() {
                    continue;
                }
                push_candidate(&mut lines, name, &command_description(subcommand), &group);
            }
        }
        lines
    }

    fn subcommand_lines(&self, subs: &[&Command]) -> Vec<String> {
        let mut lines = Vec::new();
        for subcommand in subs {
            push_candidate(
                &mut lines,
                subcommand.get_name(),
                &command_description(subcommand),
                "commands",
            );
        }
        lines
    }

    fn value_lines(&self, arg: &Arg, attached: Option<&str>) -> Vec<String> {
        let id = arg.get_id().to_string();
        let path: Vec<&str> = self.path.iter().map(String::as_str).collect();
        let context = Context {
            command: self.path.clone(),
            argument: id.clone(),
            words: std::iter::once(OsString::from("nut"))
                .chain(self.before.iter().cloned())
                .chain(std::iter::once(OsString::new()))
                .collect(),
        };
        if let Some(slot) = self.provider.values(&context) {
            return self.slot_lines(attached, slot);
        }
        if path == ["doctor"] && id == "checks" {
            let rows = catalog::doctor_candidates(&self.typed);
            if rows.is_empty() {
                return self
                    .directive_lines(attached, &[protocol::message_line(&free_text_message(arg))]);
            }
            return self.slot_lines(attached, Slot::Candidates(rows));
        }
        if let Some(rows) = catalog::suggestion_candidates(&path, &id) {
            return self.slot_lines(attached, Slot::Candidates(rows));
        }
        let possible: Vec<_> = arg
            .get_possible_values()
            .into_iter()
            .filter(|value| !value.is_hide_set())
            .collect();
        if !possible.is_empty() {
            let group = value_group(arg);
            let rows = possible
                .iter()
                .filter_map(|value| {
                    let name = value.get_name();
                    let help = value
                        .get_help()
                        .map(ToString::to_string)
                        .filter(|text| !text.is_empty())
                        .or_else(|| catalog::value_description(&path, &id, name).map(str::to_owned))
                        .unwrap_or_default();
                    let help = normalize_description(&help);
                    protocol::candidate_line(OsStr::new(name), &help, &group, false)
                })
                .collect::<Vec<_>>();
            return self.directive_lines(attached, &rows);
        }
        match catalog::path_directive(&path, &id) {
            Some(PathDirective::Files) => {
                return self.directive_lines(attached, &[protocol::files_line(None)]);
            }
            Some(PathDirective::Dirs) => {
                return self.directive_lines(attached, &[protocol::dirs_line()]);
            }
            None => {}
        }
        if let Some(line) = hint_line(arg) {
            return self.directive_lines(attached, &[line]);
        }
        self.directive_lines(attached, &[protocol::message_line(&free_text_message(arg))])
    }

    fn slot_lines(&self, attached: Option<&str>, slot: Slot) -> Vec<String> {
        match slot {
            Slot::Message(message) => {
                self.directive_lines(attached, &[protocol::message_line(&message)])
            }
            Slot::Candidates(rows) => {
                let body: Vec<String> = rows
                    .into_iter()
                    .filter_map(|row| {
                        protocol::candidate_line(
                            OsStr::new(&row.value),
                            &row.description,
                            &row.group,
                            row.nospace,
                        )
                    })
                    .collect();
                self.directive_lines(attached, &body)
            }
        }
    }

    fn directive_lines(&self, attached: Option<&str>, body: &[String]) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(prefix) = attached {
            lines.push(protocol::prefix_line(prefix.chars().count()));
        }
        lines.extend(body.iter().cloned());
        lines
    }

    fn text_lines(&self) -> Vec<String> {
        match peek_positional(self.node, self.positional_words) {
            Some(arg) => self.value_lines(arg, None),
            None => Vec::new(),
        }
    }

    fn conflicts_with_present(&self, candidate: &Arg) -> bool {
        let candidate_id = candidate.get_id().to_string();
        let conflicts_of = |arg: &Arg| -> Vec<String> {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.node.get_arg_conflicts_with(arg)
            }))
            .unwrap_or_default()
            .iter()
            .map(|arg| arg.get_id().to_string())
            .collect()
        };
        if conflicts_of(candidate)
            .iter()
            .any(|id| self.present.iter().any(|seen| seen == id))
        {
            return true;
        }
        self.present.iter().any(|seen| {
            find_by_id(self.node, seen)
                .is_some_and(|seen_arg| conflicts_of(seen_arg).iter().any(|id| id == &candidate_id))
        })
    }
}

fn global_ids(root: &Command) -> HashSet<String> {
    root.get_arguments()
        .filter(|arg| arg.is_global_set())
        .map(|arg| arg.get_id().to_string())
        .collect()
}

fn sort_options(args: &mut Vec<&Arg>) {
    args.sort_by_key(|arg| option_sort_key(arg));
}

fn option_sort_key(arg: &Arg) -> String {
    arg.get_long()
        .map(str::to_owned)
        .or_else(|| arg.get_short().map(|short| short.to_string()))
        .unwrap_or_default()
}

fn visible_subcommands(command: &Command) -> Vec<&Command> {
    command
        .get_subcommands()
        .filter(|child| !child.is_hide_set())
        .collect()
}

fn peek_positional(command: &Command, filled: usize) -> Option<&Arg> {
    let mut remaining = filled;
    for arg in command.get_positionals() {
        if is_var_arg(arg) {
            return Some(arg);
        }
        if remaining == 0 {
            return Some(arg);
        }
        remaining -= 1;
    }
    None
}

fn is_var_arg(arg: &Arg) -> bool {
    arg.is_trailing_var_arg_set()
        || matches!(arg.get_action(), ArgAction::Append)
        || arg
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1)
}

fn find_long<'a>(command: &'a Command, name: &str) -> Option<&'a Arg> {
    command.get_arguments().find(|arg| {
        arg.get_long() == Some(name)
            || arg
                .get_visible_aliases()
                .is_some_and(|aliases| aliases.contains(&name))
    })
}

fn find_short(command: &Command, short: char) -> Option<&Arg> {
    command.get_arguments().find(|arg| {
        arg.get_short() == Some(short)
            || arg
                .get_visible_short_aliases()
                .is_some_and(|aliases| aliases.contains(&short))
    })
}

fn find_by_id<'a>(command: &'a Command, id: &str) -> Option<&'a Arg> {
    command.get_arguments().find(|arg| arg.get_id() == id)
}

fn takes_values(arg: &Arg) -> bool {
    arg.get_action().takes_values() || arg.get_num_args().is_some_and(|range| range.takes_values())
}

fn arg_description(arg: &Arg) -> String {
    match arg.get_id().as_str() {
        "help" => "Print help".into(),
        "version" => "Print version".into(),
        _ => normalize_description(&arg.get_help().map(ToString::to_string).unwrap_or_default()),
    }
}

fn command_description(command: &Command) -> String {
    normalize_description(
        &command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

fn normalize_description(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default().trim();
    let without_ticks: String = first.chars().filter(|ch| *ch != '`').collect();
    without_ticks.trim_end_matches('.').trim().to_owned()
}

fn value_group(arg: &Arg) -> String {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.to_string().to_lowercase())
        .unwrap_or_else(|| "values".to_owned())
}

fn hint_line(arg: &Arg) -> Option<String> {
    match arg.get_value_hint() {
        ValueHint::DirPath => Some(protocol::dirs_line()),
        ValueHint::FilePath | ValueHint::AnyPath | ValueHint::ExecutablePath => {
            Some(protocol::files_line(None))
        }
        _ => None,
    }
}

fn free_text_message(arg: &Arg) -> String {
    let name = arg
        .get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.to_string())
        .unwrap_or_else(|| arg.get_id().to_string().to_uppercase());
    let help = arg_description(arg);
    if help.is_empty() {
        name
    } else {
        format!("{name} \u{2014} {help}")
    }
}

fn is_free_text_only(lines: &[String], arg: &Arg) -> bool {
    matches!(
        lines,
        [line] if line == &protocol::message_line(&free_text_message(arg))
    )
}

fn push_candidate(lines: &mut Vec<String>, value: &str, description: &str, group: &str) {
    if let Some(line) = protocol::candidate_line(OsStr::new(value), description, group, false) {
        lines.push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::providers::LiveProviders;
    use crate::host::FakeHost;
    use std::ffi::OsString;

    struct NullProvider;

    impl Provider for NullProvider {
        fn values(&self, _: &Context) -> Option<Slot> {
            None
        }
    }

    fn request(words: &[&str]) -> Request {
        protocol::parse_request(&{
            let mut argv = vec![
                OsString::from("zsh"),
                OsString::from("--protocol"),
                OsString::from("1"),
                OsString::from("--"),
            ];
            argv.extend(words.iter().map(OsString::from));
            argv
        })
        .expect("test request parses")
    }

    fn lines(words: &[&str]) -> Vec<String> {
        complete(&request(words), &NullProvider)
    }

    fn live_lines(words: &[&str]) -> Vec<String> {
        let host = FakeHost::default();
        complete(&request(words), &LiveProviders(&host))
    }

    fn values_of(output: &[String]) -> Vec<&str> {
        output
            .iter()
            .filter(|line| !line.starts_with('!'))
            .map(|line| line.split('\t').next().unwrap_or_default())
            .collect()
    }

    fn groups_of(output: &[String]) -> Vec<&str> {
        output
            .iter()
            .filter(|line| !line.starts_with('!'))
            .map(|line| line.split('\t').nth(2).unwrap_or_default())
            .collect()
    }

    fn snapshot(name: &str, words: &[&str]) {
        let output = if matches!(
            words,
            ["nut", "bios", "profile", "show", _] | ["nut", "rollback", _]
        ) {
            live_lines(words)
        } else {
            lines(words)
        };
        insta::assert_snapshot!(name, output.join("\n"));
    }

    #[test]
    fn presentation_snapshots() {
        snapshot("root_empty", &["nut", ""]);
        snapshot("root_dash", &["nut", "-"]);
        snapshot("root_long", &["nut", "--"]);
        snapshot("status_dash", &["nut", "status", "-"]);
        snapshot("status_long", &["nut", "status", "--"]);
        snapshot("status_watch_dash", &["nut", "status", "--watch", "-"]);
        snapshot("events_dash", &["nut", "events", "-"]);
        snapshot("events_outages_dash", &["nut", "events", "--outages", "-"]);
        snapshot("events_since_empty", &["nut", "events", "--since", ""]);
        snapshot("events_since_dash", &["nut", "events", "--since", "-"]);
        snapshot("events_boot_empty", &["nut", "events", "--boot", ""]);
        snapshot("drill_empty", &["nut", "drill", ""]);
        snapshot("drill_dash", &["nut", "drill", "-"]);
        snapshot("bios_setup_dash", &["nut", "bios", "setup", "-"]);
        snapshot("battery_empty", &["nut", "battery", ""]);
        snapshot("doctor_empty", &["nut", "doctor", ""]);
        snapshot("doctor_hw_usb", &["nut", "doctor", "hw.usb", ""]);
        snapshot("color_empty", &["nut", "--color", ""]);
        snapshot("color_attached", &["nut", "--color="]);
        snapshot(
            "completion_install_empty",
            &["nut", "completion", "install", ""],
        );
        snapshot("bios_profile_show", &["nut", "bios", "profile", "show", ""]);
        snapshot("status_upsd_port", &["nut", "status", "--upsd-port", ""]);
    }

    #[test]
    fn root_empty_offers_commands_only_in_help_order() {
        let output = lines(&["nut", ""]);
        let values = values_of(&output);
        assert!(!values.iter().any(|value| value.starts_with('-')));
        assert_eq!(
            values,
            [
                "status",
                "battery",
                "events",
                "beeper",
                "notify",
                "init",
                "rollback",
                "config",
                "doctor",
                "bios",
                "drill",
                "completion",
            ]
        );
        let mut groups = Vec::new();
        for group in groups_of(&output) {
            if groups.last() != Some(&group) {
                groups.push(group);
            }
        }
        assert_eq!(groups, ["monitor", "act", "set up", "prove", "shell"]);
    }

    #[test]
    fn lone_dash_pairs_short_and_long_with_identical_descriptions() {
        let output = lines(&["nut", "status", "-"]);
        let rows: Vec<Vec<&str>> = output
            .iter()
            .map(|line| line.split('\t').collect())
            .collect();
        let short = rows
            .iter()
            .position(|row| row[0] == "-h")
            .expect("short help");
        assert_eq!(rows[short + 1][0], "--help");
        assert_eq!(rows[short][1], rows[short + 1][1]);
        assert_eq!(rows[short][1], "Print help");
        assert!(values_of(&output).contains(&"--json"));
        assert!(values_of(&output).contains(&"-j"));
    }

    #[test]
    fn double_dash_offers_long_forms_only() {
        let output = lines(&["nut", "status", "--"]);
        let values = values_of(&output);
        assert!(values.contains(&"--watch"));
        assert!(
            values
                .iter()
                .all(|value| !value.starts_with('-') || value.starts_with("--"))
        );
    }

    #[test]
    fn present_and_conflicting_options_are_dropped() {
        let watch = lines(&["nut", "status", "--watch", "-"]);
        assert!(!values_of(&watch).contains(&"--watch"));
        assert!(!values_of(&watch).contains(&"--help"));
        let outages = lines(&["nut", "events", "--outages", "-"]);
        assert!(!values_of(&outages).contains(&"--raw"));
        assert!(!values_of(&outages).contains(&"--outages"));
    }

    #[test]
    fn refused_flags_are_never_offered() {
        assert!(!values_of(&lines(&["nut", "drill", "-"])).contains(&"--yes"));
        assert!(!values_of(&lines(&["nut", "bios", "setup", "-"])).contains(&"--yes"));
    }

    #[test]
    fn hyphen_values_complete_on_a_dash_cursor() {
        let output = lines(&["nut", "events", "--since", "-"]);
        assert!(values_of(&output).contains(&"-1h"));
        assert!(groups_of(&output).iter().all(|group| *group == "times"));
    }

    #[test]
    fn lone_action_guard_offers_options_with_the_single_subcommand() {
        let battery = lines(&["nut", "battery", ""]);
        assert!(values_of(&battery).contains(&"test"));
        assert!(values_of(&battery).contains(&"--ups"));
        let notify = lines(&["nut", "notify", ""]);
        assert!(values_of(&notify).contains(&"test"));
        assert!(values_of(&notify).contains(&"--help"));
    }

    #[test]
    fn doctor_dedupes_typed_checks() {
        let output = lines(&["nut", "doctor", "hw.usb", ""]);
        assert!(!values_of(&output).contains(&"hw.usb"));
        assert!(values_of(&output).contains(&"hw"));
    }

    #[test]
    fn attached_color_keeps_prefix() {
        let output = lines(&["nut", "--color="]);
        assert_eq!(output[0], "!prefix 8");
        assert!(values_of(&output).contains(&"always"));
    }

    #[test]
    fn free_text_slot_emits_a_message() {
        let output = lines(&["nut", "status", "--upsd-port", ""]);
        assert_eq!(output.len(), 1);
        assert!(output[0].starts_with("!message PORT \u{2014} "));
        assert!(!output[0].contains('`'));
        assert!(!output[0].contains("see more"));
    }

    #[test]
    fn globals_are_marked_after_build() {
        let mut tree = Cli::command();
        tree.build();
        let ids = global_ids(&tree);
        assert!(ids.contains("color"));
        assert!(ids.contains("config"));
        assert!(ids.contains("json"));
        assert!(!ids.contains("help"));
        let status = tree.find_subcommand("status").expect("status");
        let color = find_by_id(status, "color").expect("color on status");
        assert!(color.is_global_set());
        let watch = find_by_id(status, "watch").expect("watch");
        assert!(!watch.is_global_set());
    }

    #[test]
    fn tree_wide_invariants() {
        let errors = catalog::drift_errors();
        assert!(errors.is_empty(), "{}", errors.join("\n"));
        let host = FakeHost::default();
        let provider = LiveProviders(&host);
        let mut tree = Cli::command();
        tree.build();
        let mut failures = Vec::new();
        walk_invariants(&tree, &[], &provider, &mut failures);
        let root = complete(&request(&["nut", ""]), &provider);
        if root.iter().any(|line| {
            line.split('\t')
                .next()
                .is_some_and(|value| value.starts_with('-'))
        }) {
            failures.push("root empty word offered option rows".into());
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    fn walk_invariants(
        command: &Command,
        path: &[&str],
        provider: &dyn Provider,
        failures: &mut Vec<String>,
    ) {
        let mut words = vec!["nut".to_owned()];
        words.extend(path.iter().map(|name| (*name).to_owned()));
        for cursor in ["", "-", "--"] {
            let mut request_words: Vec<&str> = words.iter().map(String::as_str).collect();
            request_words.push(cursor);
            check_output(
                &request_words,
                &complete(&request(&request_words), provider),
                failures,
            );
        }
        for arg in command.get_arguments() {
            if arg.is_hide_set() || !takes_values(arg) || arg.is_positional() {
                continue;
            }
            let id = arg.get_id().to_string();
            if catalog::is_refused(
                &path
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect::<Vec<_>>(),
                &id,
            ) {
                continue;
            }
            let Some(long) = arg.get_long() else {
                continue;
            };
            let flag = format!("--{long}");
            let mut request_words: Vec<&str> = words.iter().map(String::as_str).collect();
            let flag_ref = flag.as_str();
            request_words.push(flag_ref);
            request_words.push("");
            check_output(
                &request_words,
                &complete(&request(&request_words), provider),
                failures,
            );
        }
        if let Some(positional) = command.get_positionals().next()
            && takes_values(positional)
            && !command.get_subcommands().any(|child| !child.is_hide_set())
        {
            let mut request_words: Vec<&str> = words.iter().map(String::as_str).collect();
            request_words.push("");
            check_output(
                &request_words,
                &complete(&request(&request_words), provider),
                failures,
            );
        }
        for child in command
            .get_subcommands()
            .filter(|child| !child.is_hide_set())
        {
            let mut next = path.to_vec();
            next.push(child.get_name());
            walk_invariants(child, &next, provider, failures);
        }
    }

    fn check_output(words: &[&str], output: &[String], failures: &mut Vec<String>) {
        let label = words.join(" ");
        for line in output {
            if line.starts_with('!') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() != 4 {
                failures.push(format!("{label}: expected 4 fields in {line}"));
                continue;
            }
            let (value, description, group, _) = (fields[0], fields[1], fields[2], fields[3]);
            if description.is_empty() {
                failures.push(format!("{label}: empty description for {value}"));
            }
            if description.contains('`') {
                failures.push(format!("{label}: backtick in {value} description"));
            }
            if description.to_ascii_lowercase().contains("see more") {
                failures.push(format!("{label}: see more in {value} description"));
            }
            if !group_is_allowed(group) {
                failures.push(format!("{label}: group {group:?} is not lowercase"));
            }
            let command_path: Vec<String> = words
                .iter()
                .skip(1)
                .take_while(|word| !word.starts_with('-') && !word.is_empty())
                .map(|word| (*word).to_owned())
                .collect();
            if (value == "--yes" || value == "-y") && catalog::is_refused(&command_path, "yes") {
                failures.push(format!("{label}: offered refused {value}"));
            }
        }
        let mut last_group = "";
        let mut seen: HashSet<String> = HashSet::new();
        let rows: Vec<Vec<&str>> = output
            .iter()
            .filter(|line| !line.starts_with('!'))
            .map(|line| line.split('\t').collect())
            .collect();
        for (index, row) in rows.iter().enumerate() {
            if row.len() < 3 {
                continue;
            }
            let group = row[2];
            if group != last_group {
                seen.clear();
                last_group = group;
            }
            let description = row[1];
            if !seen.insert(description.to_owned()) {
                let pair = index
                    .checked_sub(1)
                    .and_then(|prev| rows.get(prev))
                    .is_some_and(|prev| {
                        prev.len() >= 2
                            && prev[1] == description
                            && same_option_pair(prev[0], row[0])
                    });
                if !pair {
                    failures.push(format!(
                        "{label}: duplicate description {description:?} in {group}"
                    ));
                }
            }
        }
    }

    fn same_option_pair(left: &str, right: &str) -> bool {
        matches!(
            (left.starts_with("--"), right.starts_with("--")),
            (false, true) | (true, false)
        ) && left.starts_with('-')
            && right.starts_with('-')
    }

    fn group_is_allowed(group: &str) -> bool {
        group.chars().all(|ch| !ch.is_ascii_uppercase())
            || group
                .split(|ch: char| !ch.is_ascii_alphabetic())
                .all(|token| {
                    token.is_empty()
                        || token.chars().all(|ch| !ch.is_ascii_uppercase())
                        || token == "UPS"
                        || token == "BIOS"
                })
    }
}
