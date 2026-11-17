//! Read-only, bounded system journal access and evidence-based outage stories.
pub mod story;

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::cli::EventsArgs;
use crate::host::{Host, HostError, ProcessOutput};

pub use story::{Story, StoryKind, pair};

pub const MAX_ENTRIES: usize = 2000;
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const FIELDS: &str = "MESSAGE,SYSLOG_IDENTIFIER,_SYSTEMD_UNIT,UNIT,OBJECT_SYSTEMD_UNIT,UPSNAME,BATTERY_CHARGE,NUT_EVENT";

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub timestamp_us: u64,
    pub boot_id: Option<String>,
    pub cursor: Option<String>,
    pub source: String,
    pub message: String,
    pub ups: Option<String>,
    pub charge_percent: Option<f64>,
    pub event: Option<String>,
    pub raw: Value,
}

impl Entry {
    pub fn parse(line: &[u8]) -> Result<Self, HostError> {
        let raw: Value = serde_json::from_slice(line)
            .map_err(|_| HostError::message("invalid journal JSON; history cannot be trusted"))?;
        let timestamp_us = integer(&raw["__REALTIME_TIMESTAMP"])
            .ok_or_else(|| HostError::message("journal entry has no valid timestamp"))?;
        let message = field(&raw["MESSAGE"]).unwrap_or_else(|| "[message unavailable]".into());
        let ups = field(&raw["UPSNAME"]).or_else(|| {
            let words: Vec<_> = message.split_whitespace().collect();
            words
                .windows(2)
                .find(|pair| pair[0].eq_ignore_ascii_case("ups"))
                .map(|pair| {
                    pair[1]
                        .trim_matches([':', '(', ')'])
                        .split('@')
                        .next()
                        .unwrap_or("")
                        .to_owned()
                })
                .filter(|name| {
                    !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "unavailable" | "is" | "communication"
                    )
                })
        });
        let charge_percent = field(&raw["BATTERY_CHARGE"])
            .and_then(|value| value.parse::<f64>().ok())
            .or_else(|| raw["BATTERY_CHARGE"].as_f64())
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value));
        Ok(Self {
            timestamp_us,
            boot_id: field(&raw["_BOOT_ID"]),
            cursor: field(&raw["__CURSOR"]),
            source: field(&raw["SYSLOG_IDENTIFIER"])
                .or_else(|| field(&raw["_SYSTEMD_UNIT"]))
                .unwrap_or_else(|| "journal".into()),
            message,
            ups,
            charge_percent,
            event: field(&raw["NUT_EVENT"]),
            raw,
        })
    }
}

fn field(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(bytes)
            if bytes
                .iter()
                .all(|byte| byte.as_u64().is_some_and(|n| n <= 255)) =>
        {
            let bytes: Vec<u8> = bytes
                .iter()
                .filter_map(|byte| byte.as_u64().map(|n| n as u8))
                .collect();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
        _ => None,
    }
}

fn integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

#[derive(Clone, Debug, Serialize)]
pub struct Boot {
    pub id: String,
    pub first_us: u64,
}

#[derive(Clone, Debug, Default)]
pub struct History {
    pub entries: VecDeque<Entry>,
    pub truncated: bool,
    bytes: usize,
}

impl History {
    pub fn push(&mut self, entry: Entry) {
        self.bytes += entry.raw.to_string().len();
        self.entries.push_back(entry);
        while self.entries.len() > MAX_ENTRIES || self.bytes > MAX_HISTORY_BYTES {
            if let Some(entry) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(entry.raw.to_string().len());
                self.truncated = true;
            }
        }
    }
}

#[derive(Debug)]
pub enum JournalError {
    Access,
    Read(String),
}

fn checked(output: ProcessOutput) -> Result<(), JournalError> {
    let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if [
        "permission denied",
        "not seeing messages",
        "no journal files were opened",
        "insufficient permissions",
    ]
    .iter()
    .any(|needle| error.contains(needle))
    {
        return Err(JournalError::Access);
    }
    if output.status != Some(0) {
        return Err(JournalError::Read(
            "journalctl failed; check journalctl --system --no-pager".into(),
        ));
    }
    Ok(())
}

fn read_error(error: HostError) -> JournalError {
    if error.is_permission_denied() {
        JournalError::Access
    } else {
        JournalError::Read(error.to_string())
    }
}

/// Probe the system journal separately so an empty filtered query is not mistaken for denied access.
pub fn check_access<H: Host>(host: &H) -> Result<(), JournalError> {
    let args: Vec<OsString> = [
        "--system",
        "--no-pager",
        "-o",
        "json",
        "--lines=1",
        "--output-fields=__CURSOR",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let mut seen = false;
    let output = host
        .stream_program(
            OsStr::new("journalctl"),
            &args,
            Some(READ_TIMEOUT),
            &mut |line| {
                seen |= !line.iter().all(u8::is_ascii_whitespace);
                Ok(())
            },
        )
        .map_err(read_error)?;
    checked(output)?;
    if !seen && host.euid() != 0 {
        return Err(JournalError::Access);
    }
    Ok(())
}

pub fn query_args(args: &EventsArgs, verbose: bool, after: Option<&str>) -> Vec<OsString> {
    // Hook notifications invoked by an operator live in per-user journals when
    // journald splits by UID. Keep the system-access probe, but query every
    // accessible journal so those entries survive adoption and follow reads.
    let mut result: Vec<OsString> = ["--no-pager", "-o", "json"]
        .into_iter()
        .map(Into::into)
        .collect();
    result.push(format!("--output-fields={FIELDS}").into());
    result.push(if after.is_some() {
        "--lines=all".into()
    } else {
        format!("--lines={}", MAX_ENTRIES + 1).into()
    });
    if let Some(since) = &args.since {
        result.push(format!("--since={since}").into());
    }
    if let Some(boot) = args.boot {
        result.push(format!("--boot={boot}").into());
    }
    if let Some(cursor) = after {
        result.push(format!("--after-cursor={cursor}").into());
        result.push("--follow".into());
    }
    // In follow mode verbose reads filter locally: journalctl intersects -u and
    // explicit tag matches, so combining them would silently hide hook history.
    if !verbose || after.is_none() {
        for tag in ["upsmon", "upssched", "nut-upssched", "athena-upssched"] {
            result.push(format!("SYSLOG_IDENTIFIER={tag}").into());
        }
    }

    result
}

pub fn read<H: Host>(host: &H, args: &EventsArgs, verbose: bool) -> Result<History, JournalError> {
    check_access(host)?;
    let mut history = History::default();
    let output = host
        .stream_program(
            OsStr::new("journalctl"),
            &query_args(args, verbose, None),
            Some(READ_TIMEOUT),
            &mut |line| {
                history.push(Entry::parse(line)?);
                Ok(())
            },
        )
        .map_err(read_error)?;
    checked(output)?;
    if verbose {
        let mut unit_args = query_args(args, false, None);
        unit_args.retain(|arg| !arg.to_string_lossy().starts_with("SYSLOG_IDENTIFIER="));
        for unit in ["nut-driver@*", "nut-server.service", "nut-monitor.service"] {
            unit_args.push(format!("--unit={unit}").into());
        }
        let mut unit_history = History::default();
        checked(
            host.stream_program(
                OsStr::new("journalctl"),
                &unit_args,
                Some(READ_TIMEOUT),
                &mut |line| {
                    unit_history.push(Entry::parse(line)?);
                    Ok(())
                },
            )
            .map_err(read_error)?,
        )?;
        let truncated = history.truncated || unit_history.truncated;
        let mut entries: Vec<_> = history
            .entries
            .into_iter()
            .chain(unit_history.entries)
            .collect();
        entries.sort_by_key(|entry| entry.timestamp_us);
        let mut cursors = std::collections::BTreeSet::new();
        history = History::default();
        history.truncated = truncated;
        for entry in entries {
            if entry
                .cursor
                .as_ref()
                .is_none_or(|cursor| cursors.insert(cursor.clone()))
            {
                history.push(entry);
            }
        }
    }
    Ok(history)
}

pub fn boots<H: Host>(host: &H) -> Result<Vec<Boot>, JournalError> {
    let args: Vec<OsString> = [
        "--system",
        "--no-pager",
        "--list-boots",
        "-o",
        "json",
        "--lines=1000",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let mut bytes = Vec::new();
    let output = host
        .stream_program(
            OsStr::new("journalctl"),
            &args,
            Some(READ_TIMEOUT),
            &mut |line| {
                if bytes.len() + line.len() > 1024 * 1024 {
                    return Err(HostError::message("boot history exceeds 1 MiB"));
                }
                bytes.extend_from_slice(line);
                Ok(())
            },
        )
        .map_err(read_error)?;
    checked(output)?;
    let values: Vec<Value> = serde_json::from_slice(&bytes)
        .map_err(|_| JournalError::Read("invalid boot history JSON".into()))?;
    let mut boots = Vec::new();
    for value in values {
        let id =
            field(&value["boot_id"]).ok_or_else(|| JournalError::Read("boot ID missing".into()))?;
        let first_us = integer(&value["first_entry"])
            .ok_or_else(|| JournalError::Read("boot timestamp missing".into()))?;
        boots.push(Boot { id, first_us });
    }
    boots.sort_by_key(|boot| boot.first_us);
    Ok(boots)
}

pub fn follow<H: Host>(
    host: &H,
    args: &EventsArgs,
    verbose: bool,
    cursor: &str,
    line: &mut dyn FnMut(&[u8]) -> Result<(), HostError>,
) -> Result<(), JournalError> {
    checked(
        host.stream_program(
            OsStr::new("journalctl"),
            &query_args(args, verbose, Some(cursor)),
            None,
            &mut |bytes| {
                if verbose {
                    let value: Value = serde_json::from_slice(bytes)
                        .map_err(|_| HostError::message("invalid journal JSON"))?;
                    let tag = field(&value["SYSLOG_IDENTIFIER"]).unwrap_or_default();
                    let nut = ["upsmon", "upssched", "nut-upssched", "athena-upssched"]
                        .contains(&tag.as_str())
                        || ["_SYSTEMD_UNIT", "UNIT", "OBJECT_SYSTEMD_UNIT"]
                            .iter()
                            .any(|key| {
                                field(&value[key]).is_some_and(|unit| {
                                    unit.starts_with("nut-driver@")
                                        || ["nut-server.service", "nut-monitor.service"]
                                            .contains(&unit.as_str())
                                })
                            });
                    if !nut {
                        return Ok(());
                    }
                }
                line(bytes)
            },
        )
        .map_err(read_error)?,
    )
}

/// Establish a cursor even when no NUT entries matched, avoiding a read/follow race.
pub fn tail_cursor<H: Host>(host: &H) -> Result<String, JournalError> {
    let args: Vec<OsString> = [
        "--no-pager",
        "-o",
        "json",
        "--lines=1",
        "--output-fields=__CURSOR",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let mut cursor = None;
    checked(
        host.stream_program(
            OsStr::new("journalctl"),
            &args,
            Some(READ_TIMEOUT),
            &mut |line| {
                let value: Value = serde_json::from_slice(line)
                    .map_err(|_| HostError::message("invalid journal cursor JSON"))?;
                cursor = field(&value["__CURSOR"]);
                Ok(())
            },
        )
        .map_err(read_error)?,
    )?;
    cursor.ok_or_else(|| {
        JournalError::Read(
            "no journal cursor available for follow; retry after the journal has entries".into(),
        )
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::host::FakeHost;

    pub fn output(stdout: &[u8]) -> ProcessOutput {
        ProcessOutput {
            status: Some(0),
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    pub fn fixture_host() -> FakeHost {
        FakeHost::default()
            .with_env("LANG", "C.UTF-8")
            .with_program(
                "journalctl",
                output(include_bytes!("../tests/fixtures/journal/stories.jsonl")),
            )
            .with_program_args(
                "journalctl",
                [
                    "--system",
                    "--no-pager",
                    "-o",
                    "json",
                    "--lines=1",
                    "--output-fields=__CURSOR",
                ],
                output(b"{\"__CURSOR\":\"tail\"}"),
            )
            .with_program_args(
                "journalctl",
                [
                    "--no-pager",
                    "-o",
                    "json",
                    "--lines=1",
                    "--output-fields=__CURSOR",
                ],
                output(b"{\"__CURSOR\":\"tail\"}"),
            )
            .with_program_args(
                "journalctl",
                [
                    "--system",
                    "--no-pager",
                    "--list-boots",
                    "-o",
                    "json",
                    "--lines=1000",
                ],
                output(include_bytes!("../tests/fixtures/journal/boots.json")),
            )
            .at(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_900))
    }

    #[test]
    fn query_values_are_single_argv_and_negative_boots_are_preserved() {
        let args = EventsArgs {
            since: Some("today; unsafe".into()),
            boot: Some(-1),
            ..Default::default()
        };
        let query = query_args(&args, false, None);
        assert!(query.contains(&"--since=today; unsafe".into()));
        assert!(query.contains(&"--boot=-1".into()));
        assert!(query.contains(&"SYSLOG_IDENTIFIER=athena-upssched".into()));
        assert!(query.contains(&"--lines=2001".into()));
        let stream = query_args(&args, false, Some("cursor"));
        assert!(stream.contains(&"--after-cursor=cursor".into()));
        assert!(stream.contains(&"--follow".into()));
    }

    #[test]
    fn verbose_union_deduplicates_tag_and_unit_entries() {
        let history = read(&fixture_host(), &Default::default(), true).unwrap();
        assert_eq!(history.entries.len(), 19);
        assert!(!history.truncated);
    }

    #[test]
    fn history_and_follow_preserve_user_journal_hook_notifications() {
        let args = EventsArgs::default();
        let records = [
            serde_json::json!({"__REALTIME_TIMESTAMP":"100", "SYSLOG_IDENTIFIER":"athena-upssched", "MESSAGE":"test-notify: test notification"}),
            serde_json::json!({"__REALTIME_TIMESTAMP":"200", "SYSLOG_IDENTIFIER":"nut-upssched", "MESSAGE":"test-notify: test notification"}),
        ]
        .map(|record| record.to_string())
        .join("\n");
        // Model journald SplitMode=uid: root can read these hook records, but
        // --system excludes the journal written by the unprivileged operator.
        for verbose in [false, true] {
            let mut host = FakeHost::default()
                .with_euid(0)
                .with_program("journalctl", output(b""));
            for after in [None, Some("cursor")] {
                let mut all_journals = query_args(&args, verbose, after);
                all_journals.retain(|arg| arg != "--system");
                host =
                    host.with_program_args("journalctl", all_journals, output(records.as_bytes()));
            }
            let history = read(&host, &args, verbose).unwrap();
            assert_eq!(history.entries.len(), 2);
            assert_eq!(history.entries[0].source, "athena-upssched");
            assert_eq!(history.entries[1].source, "nut-upssched");
            let mut followed = Vec::new();
            follow(&host, &args, verbose, "cursor", &mut |line| {
                followed.push(Entry::parse(line)?);
                Ok(())
            })
            .unwrap();
            assert_eq!(followed.len(), 2);
        }
    }

    #[test]
    fn follow_cursor_can_come_from_a_user_journal() {
        let host = FakeHost::default()
            .with_euid(0)
            .with_program("journalctl", output(b""))
            .with_program_args(
                "journalctl",
                [
                    "--no-pager",
                    "-o",
                    "json",
                    "--lines=1",
                    "--output-fields=__CURSOR",
                ],
                output(b"{\"__CURSOR\":\"user-hook-tail\"}"),
            );
        check_access(&host).unwrap();
        assert_eq!(tail_cursor(&host).unwrap(), "user-hook-tail");
    }

    #[test]
    fn follow_filters_units_and_tags_without_emitting_unrelated_system_logs() {
        let args = EventsArgs::default();
        let records = [
            serde_json::json!({"SYSLOG_IDENTIFIER":"secret-service", "MESSAGE":"private"}),
            serde_json::json!({"SYSLOG_IDENTIFIER":"athena-upssched", "MESSAGE":"onbatt"}),
            serde_json::json!({"UNIT":"nut-driver@primary.service", "MESSAGE":"Started driver"}),
        ]
        .map(|record| record.to_string())
        .join("\n");
        let host = FakeHost::default().with_program("journalctl", output(records.as_bytes()));
        let mut lines = Vec::new();
        follow(&host, &args, true, "cursor", &mut |line| {
            lines.push(line.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(lines.len(), 2);
        assert!(
            !String::from_utf8(lines.concat())
                .unwrap()
                .contains("private")
        );
    }

    #[test]
    fn parsing_accepts_byte_messages_and_rejects_ambiguous_timestamps() {
        let entry =
            Entry::parse(br#"{"__REALTIME_TIMESTAMP":"100", "MESSAGE":[79,78,66,65,84,84]}"#)
                .unwrap();
        assert_eq!(entry.message, "ONBATT");
        for bad in [
            br#"{"MESSAGE":"ONBATT"}"#.as_slice(),
            b"not json",
            br#"{"__REALTIME_TIMESTAMP":["1","2"]}"#,
        ] {
            assert!(Entry::parse(bad).is_err());
        }
    }

    #[test]
    fn both_entry_count_and_total_bytes_are_bounded_and_reported() {
        let mut history = History::default();
        let entry = Entry::parse(br#"{"__REALTIME_TIMESTAMP":"100","MESSAGE":"ONBATT"}"#).unwrap();
        for _ in 0..MAX_ENTRIES + 1 {
            history.push(entry.clone());
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        assert!(history.truncated);
        let large = Entry::parse(
            serde_json::json!({"__REALTIME_TIMESTAMP":"100", "MESSAGE":"x".repeat(64000)})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        for _ in 0..100 {
            history.push(large.clone());
        }
        assert!(history.bytes <= MAX_HISTORY_BYTES);
        assert!(history.entries.len() < 100);
    }

    #[test]
    fn detects_silently_empty_and_explicitly_denied_system_journal() {
        assert!(matches!(
            check_access(&FakeHost::default().with_program("journalctl", output(b""))),
            Err(JournalError::Access)
        ));
        assert!(
            check_access(
                &FakeHost::default()
                    .with_euid(0)
                    .with_program("journalctl", output(b""))
            )
            .is_ok()
        );
        let denied = ProcessOutput {
            status: Some(0),
            stdout: Vec::new(),
            stderr: b"Hint: You are currently not seeing messages from other users and the system."
                .to_vec(),
        };
        assert!(matches!(
            check_access(&FakeHost::default().with_program("journalctl", denied)),
            Err(JournalError::Access)
        ));
    }
}
