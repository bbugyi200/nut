//! Read-only extension seam: return Some to own an argument slot with
//! headed groups or a hint, or None to let clap supply its enum/path
//! candidates.
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::host::Host;

pub struct Context {
    pub command: Vec<String>,
    pub argument: String,
    pub words: Vec<OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderItem {
    pub value: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderGroup {
    pub tag: String,
    pub heading: String,
    pub items: Vec<ProviderItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderHint {
    pub label: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderOutput {
    Groups(Vec<ProviderGroup>),
    Hint(ProviderHint),
}

pub trait Provider {
    fn values(&self, context: &Context) -> Option<ProviderOutput>;
}

pub(super) struct LiveProviders<'a, H>(pub &'a H);

/// Every argument id that can yield candidates, with its heading and tag.
/// No nut menu may fall back to a generic `Values` heading.
pub fn heading_for(argument: &str) -> Option<(&'static str, &'static str)> {
    match argument {
        "color" => Some(("Color modes", "nut-color-modes")),
        "shell" => Some(("Shells", "nut-shells")),
        "ups" => Some(("", "nut-ups")),
        "checks" => Some(("", "nut-checks")),
        "stamp" => Some(("Backups", "nut-backups")),
        "id" => Some(("Board profiles", "nut-board-profiles")),
        "since" => Some(("Since", "nut-since")),
        "boot" => Some(("Boots", "nut-boots")),
        _ => None,
    }
}

impl<H: Host> Provider for LiveProviders<'_, H> {
    fn values(&self, context: &Context) -> Option<ProviderOutput> {
        let path: Vec<&str> = context.command.iter().map(String::as_str).collect();
        match (path.as_slice(), context.argument.as_str()) {
            (_, "ups") => Some(ups_output(self.0, &context.words)),
            (["bios", "profile", "show"], "id") => Some(board_profiles_output(self.0)),
            (["doctor"], "checks") => Some(doctor_output(&context.words)),
            (["rollback"], "stamp") => Some(rollback_output(self.0)),
            (_, "since") => Some(since_output()),
            (_, "boot") => Some(boot_output()),
            (_, "shell") => Some(shells_output(self.0)),
            _ => None,
        }
    }
}

fn option_value(words: &[OsString], name: &str) -> Option<String> {
    let mut found = None;
    let mut words = words.iter().skip(1).peekable();
    while let Some(word) = words.next() {
        let text = word.to_string_lossy();
        if text == "--" {
            break;
        }
        if text.as_ref() == name {
            found = words
                .next()
                .map(|value| value.to_string_lossy().into_owned());
        } else if let Some(value) = text.strip_prefix(&format!("{name}=")) {
            found = Some(value.to_owned());
        }
    }
    found
}

fn short_option_value(words: &[OsString], short: char) -> Option<String> {
    let mut found = None;
    let mut words = words.iter().skip(1).peekable();
    while let Some(word) = words.next() {
        let text = word.to_string_lossy();
        if text == "--" {
            break;
        }
        if text.starts_with('-') && !text.starts_with("--") && text.len() > 1 {
            let mut chars = text[1..].chars();
            if let Some(first) = chars.next()
                && first == short
            {
                let rest: String = chars.collect();
                if rest.is_empty() {
                    found = words
                        .next()
                        .map(|value| value.to_string_lossy().into_owned());
                } else if let Some(stripped) = rest.strip_prefix('=') {
                    found = Some(stripped.to_owned());
                } else {
                    found = Some(rest);
                }
            }
        }
    }
    found
}

fn resolve_ups_address_port<H: Host>(host: &H, words: &[OsString]) -> (String, u16) {
    let config = option_value(words, "--config")
        .or_else(|| short_option_value(words, 'c'))
        .map(std::path::PathBuf::from);
    let address = option_value(words, "--upsd-address");
    let port = option_value(words, "--upsd-port").and_then(|port| port.parse().ok());
    let connection =
        crate::status::resolve_connection(host, config.as_deref(), None, address.as_deref(), port);
    match connection {
        Ok(connection) => (connection.address, connection.port),
        Err(_) => ("127.0.0.1".to_owned(), 3493),
    }
}

fn ups_output<H: Host>(host: &H, words: &[OsString]) -> ProviderOutput {
    let (address, port) = resolve_ups_address_port(host, words);
    let heading = format!("UPS on {address}:{port}");
    match crate::upsd::list_ups(&address, port, Duration::from_millis(150)) {
        Ok(rows) if !rows.is_empty() => ProviderOutput::Groups(vec![ProviderGroup {
            tag: "nut-ups".into(),
            heading,
            items: rows
                .into_iter()
                .map(|(value, description)| ProviderItem { value, description })
                .collect(),
        }]),
        _ => ProviderOutput::Hint(ProviderHint {
            label: "UPS".into(),
            text: format!("upsd at {address}:{port} did not answer within 150 ms"),
        }),
    }
}

fn doctor_output(words: &[OsString]) -> ProviderOutput {
    let given: std::collections::BTreeSet<String> = words
        .iter()
        .skip(1)
        .filter_map(|word| {
            let text = word.to_string_lossy().into_owned();
            if text.starts_with('-') || text == "doctor" || text.is_empty() {
                None
            } else {
                Some(text)
            }
        })
        .collect();
    let catalog = crate::doctor::catalog();
    let mut sections: Vec<(crate::doctor::Section, usize)> = Vec::new();
    for entry in catalog {
        if let Some((_, count)) = sections
            .iter_mut()
            .find(|(section, _)| *section == entry.section)
        {
            *count += 1;
        } else {
            sections.push((entry.section, 1));
        }
    }
    let mut groups = vec![ProviderGroup {
        tag: "nut-sections".into(),
        heading: "Sections".into(),
        items: sections
            .iter()
            .map(|(section, count)| ProviderItem {
                value: section.slug().into(),
                description: format!(
                    "{} · {} check{}",
                    section.heading(),
                    count,
                    if *count == 1 { "" } else { "s" }
                ),
            })
            .collect(),
    }];
    for (section, _) in &sections {
        let items: Vec<ProviderItem> = catalog
            .iter()
            .filter(|entry| entry.section == *section && !given.contains(entry.id))
            .map(|entry| ProviderItem {
                value: entry.id.to_owned(),
                description: entry.title.to_owned(),
            })
            .collect();
        if !items.is_empty() {
            groups.push(ProviderGroup {
                tag: format!("nut-checks-{}", section.slug()),
                heading: section.heading().to_owned(),
                items,
            });
        }
    }
    ProviderOutput::Groups(groups)
}

fn parse_stamp_time(stamp: &str) -> Option<chrono::DateTime<chrono::Local>> {
    let prefix = stamp.split(['-', '_']).next()?;
    let naive = chrono::NaiveDateTime::parse_from_str(prefix, "%Y%m%dT%H%M%SZ").ok()?;
    Some(
        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc)
            .with_timezone(&chrono::Local),
    )
}

fn relative_description<H: Host>(host: &H, stamp: &str, newest: bool) -> String {
    let now = host
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let stamp_secs = parse_stamp_time(stamp)
        .map(|date| date.timestamp())
        .unwrap_or(now);
    let age = (now - stamp_secs).max(0) as u64;
    let relative = if age < 7 * 86400 {
        if age >= 86400 {
            let days = age / 86400;
            format!("{} {} ago", days, if days == 1 { "day" } else { "days" })
        } else {
            format!(
                "{} ago",
                crate::ui::layout::format_duration(std::time::Duration::from_secs(age.max(1)))
            )
        }
    } else if let Some(date) = parse_stamp_time(stamp) {
        date.format("%Y-%m-%d %H:%M").to_string()
    } else {
        stamp.to_owned()
    };
    if newest {
        format!("{relative} · newest")
    } else {
        relative
    }
}

fn rollback_output<H: Host>(host: &H) -> ProviderOutput {
    let root = Path::new("/var/backups/nut-cli");
    let mut stamps = match host.list_dir(root) {
        Ok(stamps) if !stamps.is_empty() => stamps,
        _ => {
            return ProviderOutput::Hint(ProviderHint {
                label: "Backups".into(),
                text: "No readable backups under /var/backups/nut-cli".into(),
            });
        }
    };
    stamps.sort();
    stamps.reverse();
    let items = stamps
        .iter()
        .enumerate()
        .map(|(index, stamp)| ProviderItem {
            value: stamp.clone(),
            description: relative_description(host, stamp, index == 0),
        })
        .collect();
    ProviderOutput::Groups(vec![ProviderGroup {
        tag: "nut-backups".into(),
        heading: "Backups".into(),
        items,
    }])
}

fn since_output() -> ProviderOutput {
    ProviderOutput::Groups(vec![ProviderGroup {
        tag: "nut-since".into(),
        heading: "Since".into(),
        items: vec![
            ProviderItem {
                value: "today".into(),
                description: "Since midnight today".into(),
            },
            ProviderItem {
                value: "yesterday".into(),
                description: "Since midnight yesterday".into(),
            },
            ProviderItem {
                value: "-1h".into(),
                description: "The last hour".into(),
            },
            ProviderItem {
                value: "-24h".into(),
                description: "The last day".into(),
            },
            ProviderItem {
                value: "-7d".into(),
                description: "The last week".into(),
            },
        ],
    }])
}

fn boot_output() -> ProviderOutput {
    ProviderOutput::Groups(vec![ProviderGroup {
        tag: "nut-boots".into(),
        heading: "Boots".into(),
        items: vec![
            ProviderItem {
                value: "0".into(),
                description: "This boot".into(),
            },
            ProviderItem {
                value: "-1".into(),
                description: "The previous boot".into(),
            },
        ],
    }])
}

fn shells_output<H: Host>(host: &H) -> ProviderOutput {
    let current = host
        .env("SHELL")
        .and_then(|shell| {
            std::path::Path::new(&shell)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let mut zsh_desc = "Z shell".to_owned();
    let mut bash_desc = "GNU Bash".to_owned();
    if current == "zsh" {
        zsh_desc = "Z shell · your $SHELL".to_owned();
    } else if current == "bash" {
        bash_desc = "GNU Bash · your $SHELL".to_owned();
    }
    ProviderOutput::Groups(vec![ProviderGroup {
        tag: "nut-shells".into(),
        heading: "Shells".into(),
        items: vec![
            ProviderItem {
                value: "zsh".into(),
                description: zsh_desc,
            },
            ProviderItem {
                value: "bash".into(),
                description: bash_desc,
            },
        ],
    }])
}

fn board_profiles_output<H: Host>(host: &H) -> ProviderOutput {
    let items: Vec<ProviderItem> = crate::firmware::load_board_catalog(host)
        .ok()
        .map(|catalog| {
            catalog
                .board_profiles()
                .map(|profile| ProviderItem {
                    value: profile.id.as_str().to_owned(),
                    description: profile.display_name().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    ProviderOutput::Groups(vec![ProviderGroup {
        tag: "nut-board-profiles".into(),
        heading: "Board profiles".into(),
        items,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;
    use std::time::{Duration, SystemTime};

    fn context(command: &[&str], argument: &str, words: &[&str]) -> Context {
        Context {
            command: command.iter().map(ToString::to_string).collect(),
            argument: argument.to_owned(),
            words: words.iter().map(OsString::from).collect(),
        }
    }

    #[test]
    fn every_candidate_argument_has_a_heading() {
        use clap::CommandFactory;
        let mut command = crate::cli::Cli::command();
        command.build();
        let mut ids = std::collections::BTreeSet::new();
        fn walk(command: &clap::Command, ids: &mut std::collections::BTreeSet<String>) {
            for arg in command.get_arguments() {
                if !arg.get_possible_values().is_empty() {
                    ids.insert(arg.get_id().to_string());
                }
            }
            for child in command.get_subcommands() {
                walk(child, ids);
            }
        }
        walk(&command, &mut ids);
        for provider_arg in ["ups", "checks", "stamp", "id", "since", "boot", "shell"] {
            ids.insert(provider_arg.to_owned());
        }
        for id in ids {
            assert!(
                heading_for(&id).is_some(),
                "argument {id} yields candidates but has no heading-table entry"
            );
        }
    }

    #[test]
    fn doctor_groups_sections_and_dedupes_given_ids() {
        let host = FakeHost::default();
        let provider = LiveProviders(&host);
        let output = provider
            .values(&context(
                &["doctor"],
                "checks",
                &["nut", "doctor", "hw.usb", ""],
            ))
            .expect("doctor owns checks");
        let ProviderOutput::Groups(groups) = output else {
            panic!("doctor must return groups");
        };
        assert_eq!(groups[0].heading, "Sections");
        assert_eq!(groups[0].tag, "nut-sections");
        assert!(groups.iter().any(|group| group.tag == "nut-checks-hw"));
        // hw.usb already on the line is left out.
        let hw = groups
            .iter()
            .find(|group| group.tag == "nut-checks-hw")
            .unwrap();
        assert!(!hw.items.iter().any(|item| item.value == "hw.usb"));
        assert!(hw.items.iter().any(|item| item.value == "hw.data"));
    }

    #[test]
    fn rollback_orders_newest_first_with_relative_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_769_000_000);
        let host = FakeHost::default().at(now);
        // Stamps are parsed from the prefix alone; suffixes only order ties.
        let root = Path::new("/var/backups/nut-cli");
        let host = host
            .with_dir(root.to_path_buf())
            .with_file(root.join("20261110T100000Z-aaa"), vec![])
            .with_file(root.join("20261111T100000Z-bbb"), vec![]);
        let provider = LiveProviders(&host);
        let output = provider
            .values(&context(&["rollback"], "stamp", &["nut", "rollback", ""]))
            .expect("rollback owns stamps");
        let ProviderOutput::Groups(groups) = output else {
            panic!("rollback must return groups");
        };
        assert_eq!(groups[0].heading, "Backups");
        assert!(groups[0].items[0].value.starts_with("20261111"));
        assert!(
            groups[0].items[0].description.contains("newest"),
            "{}",
            groups[0].items[0].description
        );
        let empty = FakeHost::default();
        let output = LiveProviders(&empty).values(&context(
            &["rollback"],
            "stamp",
            &["nut", "rollback", ""],
        ));
        assert!(matches!(output, Some(ProviderOutput::Hint(_))));
    }

    #[test]
    fn ups_heading_uses_resolved_address_and_hints_offline() {
        let host = FakeHost::default();
        let output = LiveProviders(&host).values(&context(
            &["status"],
            "ups",
            &["nut", "status", "--upsd-address", "127.0.0.1", "--ups", ""],
        ));
        // No upsd running in the test env: a hint naming what was tried.
        match output {
            Some(ProviderOutput::Hint(hint)) => {
                assert_eq!(hint.label, "UPS");
                assert!(hint.text.contains("127.0.0.1"), "{}", hint.text);
            }
            Some(ProviderOutput::Groups(groups)) => {
                assert!(groups[0].heading.contains("127.0.0.1"));
            }
            None => panic!("ups must be owned"),
        }
    }
}
