//! Read-only extension seam: return Some to own an argument slot, or None
//! to let the presenter fall back to static tables, clap, and hints.
//! Providers never write files and never send UPS commands.
use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};

use crate::host::Host;
use crate::init::BACKUP_ROOT;
use crate::ui::layout::format_duration;

use super::catalog;

pub struct Context {
    pub command: Vec<String>,
    pub argument: String,
    pub words: Vec<OsString>,
}

#[derive(Debug)]
pub struct Candidate {
    pub value: String,
    pub description: String,
    pub group: String,
    pub nospace: bool,
}

pub enum Slot {
    Candidates(Vec<Candidate>),
    Message(String),
}

pub trait Provider {
    fn values(&self, context: &Context) -> Option<Slot>;
}

pub(super) struct LiveProviders<'a, H>(pub &'a H);

impl<H: Host> Provider for LiveProviders<'_, H> {
    fn values(&self, context: &Context) -> Option<Slot> {
        let path: Vec<&str> = context.command.iter().map(String::as_str).collect();
        match (path.as_slice(), context.argument.as_str()) {
            (_, "ups") => Some(ups_slot(&context.words)),
            (["bios", "profile", "show"], "id") => Some(board_slot(self.0)),
            (["rollback"], "stamp") => Some(rollback_slot(self.0)),
            _ => None,
        }
    }
}

fn ups_slot(words: &[OsString]) -> Slot {
    let address = option(words, "--upsd-address").unwrap_or_else(|| "127.0.0.1".into());
    let port = option(words, "--upsd-port").map_or(Some(3493), |port| port.parse().ok());
    let Some(port) = port else {
        return Slot::Message(format!("upsd at {address}:<port> did not answer"));
    };
    match crate::upsd::list_ups(&address, port, Duration::from_millis(150)) {
        Ok(rows) if rows.is_empty() => {
            Slot::Message(format!("upsd at {address}:{port} lists no UPS"))
        }
        Ok(rows) => Slot::Candidates(
            rows.into_iter()
                .map(|(value, description)| Candidate {
                    value,
                    description,
                    group: catalog::UPS_GROUP.to_owned(),
                    nospace: false,
                })
                .collect(),
        ),
        Err(_) => Slot::Message(format!("upsd at {address}:{port} did not answer")),
    }
}

fn board_slot<H: Host>(host: &H) -> Slot {
    match crate::firmware::load_board_catalog(host) {
        Ok(catalog) => Slot::Candidates(
            catalog
                .board_profiles()
                .map(|profile| Candidate {
                    value: profile.id.as_str().to_owned(),
                    description: profile.display_name().to_owned(),
                    group: catalog::BOARD_GROUP.to_owned(),
                    nospace: false,
                })
                .collect(),
        ),
        Err(_) => Slot::Message("no board profiles are available".into()),
    }
}

fn rollback_slot<H: Host>(host: &H) -> Slot {
    let path = Path::new(BACKUP_ROOT);
    match host.list_dir(path) {
        Err(error) if error.is_permission_denied() => {
            Slot::Message("listing backups needs root; try: sudo nut rollback --list".into())
        }
        Err(_) => Slot::Message("no configuration backups yet".into()),
        Ok(mut stamps) => {
            stamps.retain(|stamp| !stamp.starts_with('.'));
            stamps.sort_by(|left, right| right.cmp(left));
            if stamps.is_empty() {
                return Slot::Message("no configuration backups yet".into());
            }
            let now = host.now();
            let mut rows: Vec<Candidate> = stamps
                .iter()
                .enumerate()
                .map(|(index, stamp)| Candidate {
                    value: stamp.clone(),
                    description: stamp_description(stamp, now, index == 0),
                    group: catalog::BACKUP_GROUP.to_owned(),
                    nospace: false,
                })
                .collect();
            uniquify_descriptions(&mut rows);
            Slot::Candidates(rows)
        }
    }
}

fn stamp_description(stamp: &str, now: SystemTime, newest: bool) -> String {
    let prefix = stamp.split('-').next().unwrap_or(stamp);
    let parsed = DateTime::parse_from_str(&format!("{prefix}+0000"), "%Y%m%dT%H%M%SZ%z")
        .ok()
        .map(|when| when.with_timezone(&Local));
    let absolute = parsed
        .as_ref()
        .map(|when| when.format("%Y-%m-%d %H:%M:%S %:z").to_string())
        .unwrap_or_else(|| stamp.to_owned());
    let relative = parsed.and_then(|when| {
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(when.timestamp().max(0) as u64);
        now.duration_since(then).ok().map(relative_age)
    });
    let body = match relative {
        Some(age) => format!("{absolute} · {age}"),
        None => absolute,
    };
    if newest {
        format!("Newest · {body}")
    } else {
        body
    }
}

fn relative_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds >= 86400 {
        let days = seconds / 86400;
        format!("{days} {} ago", if days == 1 { "day" } else { "days" })
    } else {
        format!("{} ago", format_duration(age))
    }
}

fn uniquify_descriptions(rows: &mut [Candidate]) {
    let mut seen = std::collections::HashSet::new();
    for row in rows.iter_mut() {
        if !seen.insert(row.description.clone()) {
            row.description = format!("{} · {}", row.description, row.value);
            seen.insert(row.description.clone());
        }
    }
}

fn option(words: &[OsString], name: &str) -> Option<String> {
    let mut found = None;
    let mut words = words.iter().skip(1).peekable();
    while let Some(word) = words.next() {
        if word == "--" {
            break;
        }
        if word == name {
            found = words
                .next()
                .map(|value| value.to_string_lossy().into_owned());
        } else if let Some(value) = word.to_string_lossy().strip_prefix(&format!("{name}=")) {
            found = Some(value.to_owned());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;
    use std::time::SystemTime;

    fn context(command: &[&str], argument: &str) -> Context {
        Context {
            command: command.iter().map(|name| (*name).to_owned()).collect(),
            argument: argument.to_owned(),
            words: vec!["nut".into()],
        }
    }

    fn values(host: &FakeHost, command: &[&str], argument: &str) -> Slot {
        LiveProviders(host)
            .values(&context(command, argument))
            .expect("live provider owns this slot")
    }

    #[test]
    fn rollback_permission_denied_explains_root() {
        let host = FakeHost::default().with_unreadable(BACKUP_ROOT);
        match values(&host, &["rollback"], "stamp") {
            Slot::Message(message) => {
                assert!(message.contains("sudo nut rollback --list"), "{message}");
            }
            Slot::Candidates(_) => panic!("expected a message"),
        }
        assert!(host.effects().is_empty());
    }

    #[test]
    fn rollback_missing_directory_explains_empty() {
        let host = FakeHost::default();
        match values(&host, &["rollback"], "stamp") {
            Slot::Message(message) => assert!(message.contains("no configuration backups yet")),
            Slot::Candidates(_) => panic!("expected a message"),
        }
        assert!(host.effects().is_empty());
    }

    #[test]
    fn rollback_stamps_newest_first_with_unique_descriptions() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let host = FakeHost::default()
            .at(now)
            .with_dir(BACKUP_ROOT)
            .with_dir(format!("{BACKUP_ROOT}/20231114T120000Z-aaaaaaa1"))
            .with_dir(format!("{BACKUP_ROOT}/20231114T150000Z-bbbbbbb2"))
            .with_dir(format!("{BACKUP_ROOT}/20231114T150000Z-ccccccc3"));
        match values(&host, &["rollback"], "stamp") {
            Slot::Candidates(rows) => {
                assert_eq!(rows[0].value, "20231114T150000Z-ccccccc3");
                assert!(
                    rows[0].description.starts_with("Newest · "),
                    "{}",
                    rows[0].description
                );
                assert_eq!(rows[1].value, "20231114T150000Z-bbbbbbb2");
                assert_eq!(rows[2].value, "20231114T120000Z-aaaaaaa1");
                let descriptions: Vec<_> =
                    rows.iter().map(|row| row.description.as_str()).collect();
                let unique: std::collections::HashSet<_> = descriptions.iter().copied().collect();
                assert_eq!(unique.len(), descriptions.len(), "{descriptions:?}");
                assert!(rows.iter().all(|row| row.group == catalog::BACKUP_GROUP));
            }
            Slot::Message(message) => panic!("expected stamps, got {message}"),
        }
        assert!(host.effects().is_empty());
    }

    #[test]
    fn board_profiles_use_display_names() {
        let host = FakeHost::default();
        match values(&host, &["bios", "profile", "show"], "id") {
            Slot::Candidates(rows) => {
                assert!(
                    rows.iter().any(|row| row.value == "asus-prime-trx40-pro"),
                    "{rows:?}"
                );
                assert!(rows.iter().all(|row| !row.description.is_empty()));
                assert!(rows.iter().all(|row| row.group == catalog::BOARD_GROUP));
            }
            Slot::Message(message) => panic!("expected profiles, got {message}"),
        }
        assert!(host.effects().is_empty());
    }
}
