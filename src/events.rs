use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use serde_json::json;

use crate::cli::EventsArgs;
use crate::config::{self, ConfigError};
use crate::error::NutError;
use crate::host::{Host, HostError};
use crate::journal::{self, Entry, JournalError, Story, StoryKind};
use crate::status::OutputOptions;
use crate::ui::{UiContext, UiOptions, layout, theme};

const ACCESS_GUIDANCE: &str = "System journal access needs root here. Run sudo nut events. Optional systemd-journal membership exposes all system logs; nut never changes it automatically.";

pub fn notify_seconds<H: Host>(host: &H, config_path: Option<&Path>) -> Result<u64, NutError> {
    match config::load_config(host, config_path) {
        Ok((_, _, config)) => Ok(config.shutdown.notify_after.as_duration().as_secs()),
        Err(ConfigError::NotFound(_)) | Err(ConfigError::NoHome)
            if config_path.is_none() && host.env("NUT_CONFIG").is_none() =>
        {
            Ok(30)
        }
        Err(error) => Err(NutError::operational(error.to_string())),
    }
}

fn journal_error(error: JournalError) -> NutError {
    match error {
        JournalError::Access => NutError::operational(ACCESS_GUIDANCE).with_hint("sudo nut events"),
        JournalError::Read(message) => {
            NutError::operational(message).with_hint("journalctl --system --no-pager")
        }
    }
}

pub fn run<H: Host, W: Write>(
    args: EventsArgs,
    options: OutputOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    let notify_seconds = notify_seconds(host, options.config_path)?;
    let ui = UiContext::new(
        UiOptions {
            color: options.color,
            ascii: options.ascii,
        },
        host,
        host.stdout_is_terminal(),
    );
    // Capture the tail before the history query. The history cursor takes precedence
    // when present; the earlier tail covers arrivals during an initially empty query.
    let fallback_cursor = if args.follow {
        journal::check_access(host)
            .and_then(|()| journal::tail_cursor(host))
            .map(Some)
    } else {
        Ok(None)
    };
    let result = fallback_cursor.and_then(|cursor| {
        journal::read(host, &args, options.verbose).map(|history| (cursor, history))
    });
    let (fallback_cursor, mut history) = match result {
        Ok(result) => result,
        Err(error) => {
            if options.json {
                let (access, message) = match &error {
                    JournalError::Access => ("denied", ACCESS_GUIDANCE),
                    JournalError::Read(message) => ("unavailable", message.as_str()),
                };
                write_json(
                    stdout,
                    &json!({"schema_version": 1, "access": access, "message": message, "stories": []}),
                )?;
            } else if matches!(error, JournalError::Access) {
                writeln!(stdout, "nut events\n{} System journal access needs root here.\n  {} sudo nut events\nOptional systemd-journal membership exposes all system logs;\nnut never changes it automatically.", ui.glyphs.warn, ui.glyphs.arrow).map_err(output_error)?;
            }
            return Err(journal_error(error));
        }
    };
    let boot_result = journal::boots(host);
    let boot_history_available = boot_result.is_ok();
    let boots = boot_result.unwrap_or_default();
    let now_us = host
        .now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64;
    let stories = selected(
        journal::pair(history.entries.iter().cloned(), &boots, notify_seconds),
        &args,
    );
    let raw: Vec<_> = history
        .entries
        .iter()
        .skip(
            history
                .entries
                .len()
                .saturating_sub(args.limit.unwrap_or(20) as usize),
        )
        .cloned()
        .collect();
    render(
        stdout,
        &stories,
        &raw,
        &args,
        options,
        &ui,
        now_us,
        history.truncated,
        boot_history_available,
        false,
    )?;
    if !args.follow {
        return Ok(());
    }
    let cursor = fallback_cursor
        .ok_or_else(|| NutError::operational("journal returned no cursor for follow"))?;
    let mut previous = fingerprints(&stories);
    journal::follow(host, &args, options.verbose, &cursor, &mut |line| {
        let entry = Entry::parse(line)?;
        // Re-read from the pre-query system cursor to cover events arriving between
        // the tag and unit snapshots; skip entries already included in the snapshot.
        if entry.cursor.is_some() && history.entries.iter().any(|old| old.cursor == entry.cursor) {
            return Ok(());
        }
        history.push(entry.clone());
        let stories = selected(
            journal::pair(history.entries.iter().cloned(), &boots, notify_seconds),
            &args,
        );
        let current = fingerprints(&stories);
        let changed: Vec<_> = stories
            .into_iter()
            .filter(|story| previous.get(&story_key(story)) != current.get(&story_key(story)))
            .collect();
        previous = current;
        if args.raw || !changed.is_empty() {
            let now = host
                .now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
                .min(u64::MAX as u128) as u64;
            render(
                stdout,
                &changed,
                &[entry],
                &args,
                options,
                &ui,
                now,
                history.truncated,
                boot_history_available,
                true,
            )
            .map_err(|error| HostError::message(error.to_string()))?;
        }
        Ok(())
    })
    .map_err(journal_error)
}

fn selected(mut stories: Vec<Story>, args: &EventsArgs) -> Vec<Story> {
    if args.outages || args.last_outage {
        stories.retain(Story::is_power);
    }
    let limit = if args.last_outage {
        1
    } else {
        args.limit.unwrap_or(20) as usize
    };
    stories.drain(..stories.len().saturating_sub(limit));
    stories
}

fn story_key(story: &Story) -> String {
    story
        .entries
        .first()
        .and_then(|entry| entry.cursor.clone())
        .unwrap_or_else(|| format!("{}:{:?}", story.timestamp_us(), story.ups))
}

fn fingerprints(stories: &[Story]) -> BTreeMap<String, String> {
    stories
        .iter()
        .map(|story| (story_key(story), json!(story).to_string()))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn render<W: Write>(
    stdout: &mut W,
    stories: &[Story],
    raw: &[Entry],
    args: &EventsArgs,
    options: OutputOptions<'_>,
    ui: &UiContext,
    now_us: u64,
    truncated: bool,
    boots_available: bool,
    update: bool,
) -> Result<(), NutError> {
    if options.json {
        let mut document = json!({"schema_version": 1, "access": "readable", "truncated": truncated, "boot_history_available": boots_available, "update": update});
        if args.raw {
            document["entries"] = json!(raw.iter().map(|entry| &entry.raw).collect::<Vec<_>>());
        } else {
            document["stories"] = json!(stories);
        }
        write_json(stdout, &document)?;
    } else {
        if !update {
            writeln!(
                stdout,
                "{}",
                theme::Theme::paint(theme::Tone::Info, "nut events", ui.color_enabled)
            )
            .map_err(output_error)?;
        }
        if truncated {
            wrap(
                stdout,
                "History is bounded; earlier entries were omitted.",
                ui.width,
            )?;
        }
        if !boots_available {
            wrap(
                stdout,
                "Boot history unavailable; recovery times are unknown.",
                ui.width,
            )?;
        }
        if args.raw {
            for entry in raw {
                wrap(
                    stdout,
                    &format!(
                        "{}  {}: {}",
                        time_label(entry.timestamp_us, now_us, options.verbose),
                        clean(&entry.source),
                        clean(&entry.message)
                    ),
                    ui.width,
                )?;
            }
        } else {
            for story in stories {
                let text = format!(
                    "{} {}  {}",
                    ui.glyphs.bullet,
                    time_label(story.timestamp_us(), now_us, options.verbose),
                    summary(story)
                );
                let text = if ui.glyphs.bullet == ">" {
                    text.replace('·', "|")
                } else {
                    text
                };
                wrap(stdout, &text, ui.width)?;
                if story.is_power() {
                    wrap(
                        stdout,
                        &format!(
                            "  Mail: {} | charge: {} -> {}",
                            story.notification,
                            charge(story.charge_start_percent),
                            charge(story.charge_end_percent)
                        ),
                        ui.width,
                    )?;
                }
            }
        }
        if raw.is_empty() && args.raw || stories.is_empty() && !args.raw {
            writeln!(
                stdout,
                "No matching NUT events in the retained journal window."
            )
            .map_err(output_error)?;
        }
        if args.follow && !update {
            writeln!(stdout, "Following journal updates; Ctrl-C to stop.").map_err(output_error)?;
        }
    }
    stdout.flush().map_err(output_error)
}

pub fn summary(story: &Story) -> String {
    let label = match story.kind {
        StoryKind::Outage => "Outage",
        StoryKind::Blip => "Blip",
        StoryKind::Shutdown => "Shutdown",
        StoryKind::Communication => "Communication lost",
        StoryKind::ReplaceBattery => "Replace battery",
        StoryKind::Other => "Journal",
    };
    let mut parts = vec![label.to_owned()];
    if let Some(seconds) = story.duration_seconds {
        parts.push(layout::format_duration(Duration::from_secs(seconds)));
    }
    if let Some(incomplete) = story.incomplete {
        parts.push(incomplete.to_owned());
    }
    if let Some(seconds) = story.recovery_seconds {
        parts.push(format!(
            "next boot after {}",
            layout::format_duration(Duration::from_secs(seconds))
        ));
    }
    if story.is_power() && story.end_us.is_some() && story.shutdown_us.is_none() {
        parts.push("no shutdown recorded".into());
    }
    if story.kind == StoryKind::Communication && story.end_us.is_some() {
        parts.push("restored".into());
    }
    if story.kind == StoryKind::Other
        && let Some(entry) = story.entries.first()
    {
        parts.push(clean(&entry.message));
    }
    parts.join(" · ")
}

pub fn time_label(timestamp_us: u64, now_us: u64, verbose: bool) -> String {
    let absolute = i64::try_from(timestamp_us / 1_000_000)
        .ok()
        .and_then(|seconds| Local.timestamp_opt(seconds, 0).single())
        .map(|date| date.format("%Y-%m-%d %H:%M:%S %:z").to_string())
        .unwrap_or_else(|| format!("{timestamp_us} us since epoch"));
    match now_us.checked_sub(timestamp_us).map(|us| us / 1_000_000) {
        Some(age) if age < 7 * 86400 => {
            let relative = if age >= 86400 {
                format!(
                    "{} {} ago",
                    age / 86400,
                    if age / 86400 == 1 { "day" } else { "days" }
                )
            } else {
                format!("{} ago", layout::format_duration(Duration::from_secs(age)))
            };
            if verbose {
                format!("{relative} ({absolute})")
            } else {
                relative
            }
        }
        _ => absolute,
    }
}

fn charge(value: Option<f64>) -> String {
    value.map_or_else(|| "unknown".into(), |value| format!("{value}%"))
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn wrap<W: Write>(stdout: &mut W, text: &str, width: usize) -> Result<(), NutError> {
    let indent = text
        .chars()
        .take_while(|c| *c == ' ')
        .count()
        .min(width / 2);
    write!(stdout, "{}", " ".repeat(indent)).map_err(output_error)?;
    let mut column = indent;
    for word in text.split_whitespace() {
        let size = word.chars().count();
        if column != 0 && column + size + 1 > width {
            writeln!(stdout).map_err(output_error)?;
            column = 0;
        }
        if column != 0 && column != indent {
            write!(stdout, " ").map_err(output_error)?;
            column += 1;
        }
        for character in word.chars() {
            if column == width {
                writeln!(stdout).map_err(output_error)?;
                column = 0;
            }
            write!(stdout, "{character}").map_err(output_error)?;
            column += 1;
        }
    }
    writeln!(stdout).map_err(output_error)
}

fn write_json<W: Write>(stdout: &mut W, document: &serde_json::Value) -> Result<(), NutError> {
    serde_json::to_writer(&mut *stdout, document)
        .map_err(|error| NutError::operational(format!("cannot write JSON: {error}")))?;
    writeln!(stdout).map_err(output_error)
}

fn output_error(error: std::io::Error) -> NutError {
    NutError::operational(format!("cannot write events: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;
    use crate::journal::tests::{fixture_host, output};

    fn execute(args: &[&str], host: &FakeHost) -> (i32, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(args.iter().copied(), host, &mut stdout, &mut stderr);
        (
            code,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn event_screens_at_supported_widths_and_ascii() {
        for width in [60, 80, 120] {
            for ascii in [false, true] {
                let host = fixture_host().with_env("COLUMNS", width.to_string());
                let mut args = vec!["nut", "events"];
                if ascii {
                    args.push("--ascii");
                }
                let (code, output, error) = execute(&args, &host);
                assert_eq!(code, 0, "{error}");
                assert!(output.lines().all(|line| line.chars().count() <= width));
                assert!(!output.contains('\u{1b}'));
                if ascii {
                    assert!(output.is_ascii());
                }
                insta::assert_snapshot!(
                    format!("events_{width}_{}", if ascii { "ascii" } else { "unicode" }),
                    output
                );
            }
        }
    }

    #[test]
    fn json_raw_outage_limit_and_usage_contracts() {
        let host = fixture_host();
        let (code, text, error) = execute(&["nut", "events", "-j", "-o", "-n", "2"], &host);
        assert_eq!(code, 0, "{error}");
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["stories"].as_array().unwrap().len(), 2);
        assert_eq!(json["stories"][0]["kind"], "shutdown");
        assert_eq!(json["stories"][1]["incomplete"], "still on battery");
        let (_, raw, _) = execute(&["nut", "events", "-j", "-r", "-n", "1"], &host);
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["entries"].as_array().unwrap().len(), 1);
        assert_eq!(
            json["entries"][0]["MESSAGE"],
            "UPS primary@localhost on battery"
        );
        for args in [
            vec!["nut", "events", "-n", "0"],
            vec!["nut", "events", "-n", "2001"],
            vec!["nut", "events", "-r", "-o"],
        ] {
            assert_eq!(execute(&args, &host).0, 2);
        }
        assert_eq!(
            execute(&["nut", "events", "-b", "-1", "-s", "-2h"], &host).0,
            0
        );
    }

    #[test]
    fn inaccessible_empty_and_malformed_histories_are_distinct() {
        let denied = FakeHost::default().with_program("journalctl", output(b""));
        let (code, text, _) = execute(&["nut", "events", "-j"], &denied);
        assert_eq!(code, 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["access"],
            "denied"
        );
        let (code, text, _) = execute(&["nut", "events", "--ascii"], &denied);
        assert_eq!(code, 1);
        assert!(text.contains("-> sudo nut events"));
        assert!(text.contains("systemd-journal"));
        let empty = fixture_host().with_program_args(
            "journalctl",
            journal::query_args(&Default::default(), false, None),
            output(b""),
        );
        let (code, text, error) = execute(&["nut", "events"], &empty);
        assert_eq!(code, 0, "{error}");
        assert!(text.contains("No matching NUT events"));
        let malformed = fixture_host().with_program_args(
            "journalctl",
            journal::query_args(&Default::default(), false, None),
            output(b"bad json"),
        );
        let (code, text, _) = execute(&["nut", "events", "-j"], &malformed);
        assert_eq!(code, 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["access"],
            "unavailable"
        );
    }

    #[test]
    fn follows_cursor_and_emits_completed_story_as_json_line() {
        let args = EventsArgs {
            follow: true,
            ..Default::default()
        };
        let record = serde_json::json!({"__REALTIME_TIMESTAMP":"1700000800000000", "__CURSOR":"new", "_BOOT_ID":"boot-b", "SYSLOG_IDENTIFIER":"upsmon", "MESSAGE":"UPS primary@localhost on line power"});
        let replay = format!(
            "{}\n{record}",
            include_str!("../tests/fixtures/journal/stories.jsonl")
                .lines()
                .last()
                .unwrap()
        );
        let host = fixture_host().with_program_args(
            "journalctl",
            journal::query_args(&args, false, Some("tail")),
            output(replay.as_bytes()),
        );
        let (code, text, error) = execute(&["nut", "events", "-f", "-j"], &host);
        assert_eq!(code, 0, "{error}");
        let documents: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[1]["update"], true);
        assert_eq!(documents[1]["stories"].as_array().unwrap().len(), 1);
        assert_eq!(documents[1]["stories"][0]["duration_seconds"], 100);
        assert_eq!(
            documents[1]["stories"][0]["incomplete"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn configured_notification_threshold_controls_blip_classification() {
        let host = fixture_host().with_file(
            "/config.toml",
            include_bytes!("../examples/host.toml").to_vec(),
        );
        assert_eq!(
            notify_seconds(&host, Some(Path::new("/config.toml"))).unwrap(),
            30
        );
        assert_eq!(notify_seconds(&FakeHost::default(), None).unwrap(), 30);
        assert!(notify_seconds(&FakeHost::default(), Some(Path::new("/missing.toml"))).is_err());
    }
}
