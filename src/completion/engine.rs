//! The only module importing clap_complete. Public clap introspection determines
//! value slots; upstream owns command/enum candidates and option syntax.
use std::ffi::OsString;

use clap::{Arg, Command, CommandFactory, ValueHint};
use clap_complete::engine;

use super::protocol::{self, Request};
use super::providers::{Candidate, Context, Provider};
use crate::cli::{COMMAND_GROUPS, Cli};

pub(super) fn complete(request: &Request, provider: &dyn Provider) -> Vec<String> {
    let mut tree = Cli::command();
    tree.build();
    let Some((cursor, before)) = request.words.split_last() else {
        return vec![];
    };
    // Shell adapters retain the suffix themselves; don't treat it as another word.
    let _ = (request.shell, &request.suffix);
    let mut command = &tree;
    let mut path = Vec::new();
    let mut pending: Option<&Arg> = None;
    let mut positional = 1;
    let mut options = true;
    for word in before.iter().skip(1) {
        let word = word.to_string_lossy();
        if pending.take().is_some() {
            continue;
        }
        if options && word == "--" {
            options = false;
            continue;
        }
        if options && word.starts_with('-') {
            if let Some((arg, attached)) = option(command, &word)
                && arg.get_action().takes_values()
                && !attached
            {
                pending = Some(arg);
            }
        } else if options && let Some(subcommand) = command.find_subcommand(word.as_ref()) {
            path.push(subcommand.get_name().to_owned());
            command = subcommand;
            positional = 1;
        } else if let Some(arg) = command
            .get_positionals()
            .find(|arg| arg.get_index() == Some(positional))
            && !arg
                .get_num_args()
                .is_some_and(|range| range.max_values() > 1)
        {
            positional += 1;
        }
    }
    let cursor = cursor.to_string_lossy();
    let mut keep = 0;
    let mut slot = pending;
    if slot.is_none() && options && cursor.starts_with('-') {
        if let Some((arg, true)) = option(command, &cursor)
            && arg.get_action().takes_values()
        {
            keep = cursor.find('=').map_or(2, |index| index + 1);
            slot = Some(arg);
        }
    } else if slot.is_none() {
        slot = command
            .get_positionals()
            .find(|arg| arg.get_index() == Some(positional));
    }
    let mut lines = Vec::new();
    if keep > 0 {
        lines.push(protocol::prefix_line(cursor[..keep].chars().count()));
    }
    if let Some(arg) = slot {
        let context = Context {
            command: path,
            argument: arg.get_id().to_string(),
            words: request.words.clone(),
        };
        if let Some(values) = provider.values(&context) {
            if values.is_empty() {
                return Vec::new();
            }
            append(&mut lines, values);
            return lines;
        }
        match arg.get_value_hint() {
            ValueHint::DirPath => {
                lines.push(protocol::dirs_line());
                return lines;
            }
            ValueHint::FilePath | ValueHint::AnyPath => {
                lines.push(protocol::files_line(None));
                return lines;
            }
            _ => {}
        }
    }
    let mut words = request.words.clone();
    let index = words.len() - 1;
    // Present all candidates to zsh for native matching and descriptions. Bash
    // filters in its adapter. Attached options retain only their option prefix.
    words[index] = if keep > 0 {
        cursor[..keep].into()
    } else if cursor.starts_with('-') && slot.is_none() {
        "--".into()
    } else {
        OsString::new()
    };
    let candidates = engine::complete(&mut tree.clone(), words, index, None).unwrap_or_default();
    append(
        &mut lines,
        candidates
            .into_iter()
            .filter(|candidate| !candidate.is_hide_set())
            .map(|candidate| {
                let value = candidate.get_value().to_string_lossy();
                let value = if keep > 0 {
                    value.strip_prefix(&cursor[..keep]).unwrap_or(&value)
                } else {
                    &value
                };
                let group = if path_is_root(command) {
                    COMMAND_GROUPS
                        .iter()
                        .find(|(_, commands)| commands.contains(&value))
                        .map(|(group, _)| *group)
                } else {
                    None
                }
                .unwrap_or(if value.starts_with('-') {
                    "Options"
                } else {
                    "Values"
                });
                Candidate {
                    value: value.to_owned(),
                    description: candidate
                        .get_help()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    group: group.to_owned(),
                    nospace: false,
                }
            })
            .collect(),
    );
    // Upstream deduplicates short/long forms by argument id. Preserve the short
    // forms from the same clap metadata, so `nut -<TAB>` is useful too.
    if slot.is_none() && cursor.starts_with('-') && !cursor.starts_with("--") {
        append(
            &mut lines,
            command
                .get_arguments()
                .filter(|arg| !arg.is_hide_set())
                .filter_map(|arg| {
                    Some(Candidate {
                        value: format!("-{}", arg.get_short()?),
                        description: arg.get_help().map(ToString::to_string).unwrap_or_default(),
                        group: "Options".into(),
                        nospace: false,
                    })
                })
                .collect(),
        );
    }
    if lines.iter().all(|line| line.starts_with('!')) {
        Vec::new()
    } else {
        lines
    }
}

fn path_is_root(command: &Command) -> bool {
    command.get_name() == "nut"
}

fn option<'a>(command: &'a Command, word: &str) -> Option<(&'a Arg, bool)> {
    if let Some(long) = word.strip_prefix("--") {
        let (name, attached) = long
            .split_once('=')
            .map_or((long, false), |(name, _)| (name, true));
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

fn append(lines: &mut Vec<String>, values: Vec<Candidate>) {
    lines.extend(values.into_iter().filter_map(|candidate| {
        protocol::candidate_line(
            candidate.value.as_ref(),
            &candidate.description,
            &candidate.group,
            candidate.nospace,
        )
    }));
}
