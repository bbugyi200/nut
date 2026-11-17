use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::cli::{DrillAction, EventsArgs};
use crate::config::{self, ConfigError, HostConfig};
use crate::doctor::{CatalogEntry, Check, pass, skipped, unknown, warn};
use crate::error::NutError;
use crate::firmware::audit::{CheckStatus, journal_persistent};
use crate::firmware::{DmiInfo, efivar_fingerprint, read_trimmed, resolve_profile};
use crate::host::{FileMetadata, Host};
use crate::journal;
use crate::profiles::{embedded_catalog, load_catalog};
use crate::render::{LIVE_PATHS, mask_secrets};
use crate::ui::layout::format_duration;
use crate::upsd::UpsdClient;

const STATE_DIR: &str = "/var/lib/nut-cli";
const DRILLS_DIR: &str = "/var/lib/nut-cli/drills";
const PENDING_PATH: &str = "/var/lib/nut-cli/drills/pending.json";
const BACKUPS_DIR: &str = "/var/backups/nut-cli";
const DEFAULT_UPS: &str = "cp1500";
const BLIP_MINIMUM: Duration = Duration::from_secs(60);
const BLIP_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Drill records live in the same root-only state tree that `nut init`
/// protects, so they use its private directory and file metadata.
const PRIVATE_DIR: FileMetadata = FileMetadata {
    uid: 0,
    gid: 0,
    mode: 0o700,
    is_file: false,
    is_dir: true,
};
const PRIVATE_FILE: FileMetadata = FileMetadata {
    uid: 0,
    gid: 0,
    mode: 0o600,
    is_file: true,
    is_dir: false,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DrillKind {
    Blip,
    AcLoss,
    Cycle,
    Outage,
}

impl DrillKind {
    pub const ALL: [Self; 4] = [Self::Blip, Self::AcLoss, Self::Cycle, Self::Outage];

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Blip => "blip",
            Self::AcLoss => "ac-loss",
            Self::Cycle => "cycle",
            Self::Outage => "outage",
        }
    }

    const fn expected_host(self) -> &'static str {
        match self {
            Self::Blip => "stay running through the interruption",
            Self::AcLoss | Self::Cycle | Self::Outage => "boot automatically after power returned",
        }
    }

    const fn steps(self) -> &'static [&'static str] {
        match self {
            Self::Blip => &[
                "Unplug the UPS from mains now. Leave it disconnected for 60 seconds.",
                "Reconnect mains after 60 seconds. The live watch records the OB runtime and waits for OL and journal mail events.",
            ],
            Self::AcLoss => &[
                "At the keyboard, run `sudo systemctl poweroff`.",
                "After athena is fully off, switch the PSU off for 30 seconds, then back on.",
                "Do not press the case power button. Run `sudo nut drill report` after boot.",
            ],
            Self::Cycle => &[
                "With mains present, run `sudo upsmon -c fsd` at the keyboard.",
                "Wait for the UPS outlets to turn off and return. Do not press the case power button.",
                "Run `sudo nut drill report` after boot.",
            ],
            Self::Outage => &[
                "Unplug the UPS from mains, then run `sudo upsmon -c fsd` at the keyboard.",
                "Wait for the UPS outlets to turn off, reconnect mains, and wait for athena to boot.",
                "Do not press the case power button. Run `sudo nut drill report` after boot.",
            ],
        }
    }

    fn from_action(action: DrillAction) -> Option<Self> {
        match action {
            DrillAction::Blip => Some(Self::Blip),
            DrillAction::AcLoss => Some(Self::AcLoss),
            DrillAction::Cycle => Some(Self::Cycle),
            DrillAction::Outage => Some(Self::Outage),
            DrillAction::Report => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidencePins {
    pub bios_version: Option<String>,
    pub efivar_fingerprint: Option<String>,
    pub nut_version: Option<String>,
    pub config_hash: Option<String>,
    pub battery_installed: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JournalObservation {
    pub timestamp_us: u64,
    pub source: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Attestations {
    pub outlet_behavior: bool,
    pub host_behavior: bool,
    pub no_button_press: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications_arrived: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DrillRecord {
    pub schema_version: u32,
    pub drill: DrillKind,
    pub hostname: String,
    pub ups: String,
    pub started_unix: u64,
    pub started_boot_id: Option<String>,
    pub charge_percent_at_start: f64,
    pub runtime_at_start_seconds: Option<u64>,
    pub runtime_at_on_battery_seconds: Option<u64>,
    pub observed_blip_seconds: Option<u64>,
    pub pins: EvidencePins,
    #[serde(default)]
    pub journal: Vec<JournalObservation>,
    #[serde(default)]
    pub reported_unix: Option<u64>,
    #[serde(default)]
    pub passed: Option<bool>,
    #[serde(default)]
    pub attestations: Option<Attestations>,
}

#[derive(Clone, Debug)]
pub struct DrillOptions<'a> {
    pub config_path: Option<&'a Path>,
    pub json: bool,
    pub ascii: bool,
    pub yes: bool,
    pub ups: Option<&'a str>,
    pub upsd_address: Option<&'a str>,
    pub upsd_port: Option<u16>,
}

#[derive(Clone, Debug)]
struct DrillContext {
    config_path: PathBuf,
    config: Option<HostConfig>,
    ups: String,
    address: String,
    port: u16,
}

#[derive(Debug)]
struct LiveReadings {
    status: Vec<String>,
    charge: f64,
    runtime: Option<u64>,
    nut_version: String,
}

pub fn run<H: Host, W: Write>(
    action: Option<DrillAction>,
    options: DrillOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<i32, NutError> {
    if options.yes {
        return Err(NutError::operational(
            "nut drill refuses --yes; drills require a person at the keyboard",
        ));
    }
    match action {
        None => {
            show_table(
                host,
                options.config_path,
                options.ascii,
                options.json,
                stdout,
            )?;
            Ok(0)
        }
        Some(DrillAction::Report) => {
            report(host, options.ascii, stdout)?;
            Ok(0)
        }
        Some(action) => {
            let kind = DrillKind::from_action(action).expect("non-report action maps to a drill");
            begin(kind, options, host, stdout)?;
            Ok(0)
        }
    }
}

fn begin<H: Host, W: Write>(
    kind: DrillKind,
    options: DrillOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    require_root_tty(host)?;
    confirm_hostname(host)?;
    if load_record(host, Path::new(PENDING_PATH))?.is_some() {
        return Err(NutError::operational(
            "a drill report is already pending; finish it with `sudo nut drill report`",
        ));
    }

    let context = drill_context(host, options)?;
    let readings = live_readings(&context)?;
    require_safe_ups(&readings)?;
    require_persistent_journal(host)?;

    let pins = collect_pins(
        host,
        Some(&context.config_path),
        context.config.as_ref(),
        Some(&readings.nut_version),
    )
    .map_err(NutError::operational)?;
    write_backup_warning(host, stdout)?;

    let started_unix = unix_seconds(host.now());
    let boot_id = read_trimmed(host, Path::new("/proc/sys/kernel/random/boot_id"))
        .ok()
        .flatten();
    let mut record = DrillRecord {
        schema_version: 1,
        drill: kind,
        hostname: host.hostname().unwrap_or_else(|_| "unknown".to_owned()),
        ups: context.ups.clone(),
        started_unix,
        started_boot_id: boot_id,
        charge_percent_at_start: readings.charge,
        runtime_at_start_seconds: readings.runtime,
        runtime_at_on_battery_seconds: None,
        observed_blip_seconds: None,
        pins,
        journal: Vec::new(),
        reported_unix: None,
        passed: None,
        attestations: None,
    };
    save_record(host, Path::new(PENDING_PATH), &record)?;

    writeln!(stdout, "nut drill {} · {}", kind.slug(), record.hostname).map_err(output_error)?;
    writeln!(
        stdout,
        "  Charge  {:.0}% · Status  {}",
        readings.charge,
        readings.status.join(" ")
    )
    .map_err(output_error)?;
    for (index, step) in kind.steps().iter().enumerate() {
        writeln!(stdout, "  {}. {step}", index + 1).map_err(output_error)?;
    }
    writeln!(
        stdout,
        "  Evidence saved before the drill in {PENDING_PATH}."
    )
    .map_err(output_error)?;
    writeln!(
        stdout,
        "  After the drill, run `sudo nut drill report` to record the result."
    )
    .map_err(output_error)?;

    if kind == DrillKind::Blip {
        watch_blip(host, &context, &mut record, stdout)?;
    }
    Ok(())
}

fn require_root_tty<H: Host>(host: &H) -> Result<(), NutError> {
    if host.euid() != 0 {
        return Err(NutError::operational(
            "drills need root; run `sudo nut drill <name>`",
        ));
    }
    if !host.stdin_is_terminal() || !host.stdout_is_terminal() {
        return Err(NutError::operational(
            "drills need a terminal on both stdin and stdout",
        ));
    }
    Ok(())
}

fn confirm_hostname<H: Host>(host: &H) -> Result<(), NutError> {
    let hostname = host
        .hostname()
        .map_err(|error| NutError::operational(error.to_string()))?;
    let entered = host
        .prompt_line(&format!("Type {hostname:?} to confirm this host: "))
        .map_err(|error| NutError::operational(error.to_string()))?;
    if entered.trim() != hostname {
        return Err(NutError::operational("hostname confirmation did not match"));
    }
    Ok(())
}

fn drill_context<H: Host>(host: &H, options: DrillOptions<'_>) -> Result<DrillContext, NutError> {
    let (path, _) = config::config_path(host, options.config_path)
        .map_err(|error| NutError::operational(error.to_string()))?;
    let requested = options.config_path.is_some() || host.env("NUT_CONFIG").is_some();
    let config = match config::load_config(host, options.config_path) {
        Ok((_, _, config)) => Some(config),
        Err(ConfigError::NotFound(_)) if !requested => None,
        Err(error) => return Err(NutError::operational(error.to_string())),
    };
    let configured_ups = config
        .as_ref()
        .map(|config| {
            config
                .ups_entry()
                .map(|(name, _)| name.to_owned())
                .map_err(|error| NutError::operational(error.to_string()))
        })
        .transpose()?;
    let ups = options
        .ups
        .map(str::to_owned)
        .or(configured_ups)
        .unwrap_or_else(|| DEFAULT_UPS.into());
    Ok(DrillContext {
        config_path: path,
        config,
        ups,
        address: options.upsd_address.unwrap_or("127.0.0.1").to_owned(),
        port: options.upsd_port.unwrap_or(3493),
    })
}

fn live_readings(context: &DrillContext) -> Result<LiveReadings, NutError> {
    let mut client = UpsdClient::connect(&context.address, context.port, &context.ups)
        .map_err(|error| NutError::operational(error.to_string()))?;
    let variables = client
        .list_vars()
        .map_err(|error| NutError::operational(error.to_string()))?;
    let version = client
        .greeting()
        .map(str::to_owned)
        .or_else(|| client.version().ok())
        .and_then(parse_nut_version)
        .ok_or_else(|| NutError::operational("upsd did not report a readable NUT version"))?;
    let status = variables
        .get("ups.status")
        .ok_or_else(|| NutError::operational("upsd did not report ups.status"))?
        .split_ascii_whitespace()
        .map(|flag| flag.to_ascii_uppercase())
        .collect::<Vec<_>>();
    let charge = variables
        .get("battery.charge")
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
        .ok_or_else(|| NutError::operational("upsd did not report a valid battery.charge"))?;
    let runtime = variables
        .get("battery.runtime")
        .and_then(|value| value.parse::<u64>().ok());
    Ok(LiveReadings {
        status,
        charge,
        runtime,
        nut_version: version,
    })
}

fn require_safe_ups(readings: &LiveReadings) -> Result<(), NutError> {
    if !readings.status.iter().any(|flag| flag == "OL")
        || readings
            .status
            .iter()
            .any(|flag| matches!(flag.as_str(), "OB" | "LB" | "FSD"))
    {
        return Err(NutError::operational(format!(
            "UPS is not safely on line ({}); drills require OL without OB, LB, or FSD",
            readings.status.join(" ")
        )));
    }
    if readings.charge < 95.0 {
        return Err(NutError::operational(format!(
            "battery charge is {:.0}%; drills require at least 95%",
            readings.charge
        )));
    }
    Ok(())
}

fn require_persistent_journal<H: Host>(host: &H) -> Result<(), NutError> {
    let check = journal_persistent(host);
    if check.status == CheckStatus::Pass {
        return Ok(());
    }
    let hint = check
        .fix
        .unwrap_or_else(|| "sudo nut doctor boot.journal".into());
    Err(NutError::operational(format!(
        "{}; drills need persistent journald. Fix it with: {hint}",
        check.evidence
    )))
}

fn write_backup_warning<H: Host, W: Write>(host: &H, stdout: &mut W) -> Result<(), NutError> {
    let names = match host.list_dir(Path::new(BACKUPS_DIR)) {
        Ok(names) => names,
        Err(_) => {
            writeln!(
                stdout,
                "  !! Last config backup age unknown (no backup directory)."
            )
            .map_err(output_error)?;
            return Ok(());
        }
    };
    let mut latest: Option<(u64, String)> = None;
    for name in names {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            continue;
        }
        let path = Path::new(BACKUPS_DIR).join(&name);
        let args: [OsString; 3] = ["-c".into(), "%Y".into(), path.as_os_str().to_owned()];
        let output = match host.run_program(OsStr::new("stat"), &args) {
            Ok(output) if output.status == Some(0) => output,
            _ => continue,
        };
        let Some(epoch) = std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
        else {
            continue;
        };
        if latest.as_ref().is_none_or(|(old, _)| epoch > *old) {
            latest = Some((epoch, name));
        }
    }
    match latest {
        Some((epoch, stamp)) => {
            let now = unix_seconds(host.now());
            let age = Duration::from_secs(now.saturating_sub(epoch));
            writeln!(
                stdout,
                "  !! Last config backup: {} ago ({stamp}).",
                format_duration(age)
            )
            .map_err(output_error)?;
        }
        None => {
            writeln!(
                stdout,
                "  !! No config backup found; RAID0 has no redundancy."
            )
            .map_err(output_error)?;
        }
    }
    Ok(())
}

fn watch_blip<H: Host, W: Write>(
    host: &H,
    context: &DrillContext,
    record: &mut DrillRecord,
    stdout: &mut W,
) -> Result<(), NutError> {
    writeln!(
        stdout,
        "  Waiting for OB, then OL. Reconnect mains after 60 seconds on battery."
    )
    .map_err(output_error)?;
    let start = host.now();
    let mut on_battery_at = None;
    loop {
        let mut client = UpsdClient::connect(&context.address, context.port, &context.ups)
            .map_err(|error| NutError::operational(error.to_string()))?;
        let variables = client
            .list_vars()
            .map_err(|error| NutError::operational(error.to_string()))?;
        let status = variables
            .get("ups.status")
            .ok_or_else(|| NutError::operational("upsd stopped reporting ups.status"))?
            .split_ascii_whitespace()
            .map(|flag| flag.to_ascii_uppercase())
            .collect::<Vec<_>>();
        let now = host.now();
        if status.iter().any(|flag| flag == "OB") && on_battery_at.is_none() {
            on_battery_at = Some(now);
            record.runtime_at_on_battery_seconds = variables
                .get("battery.runtime")
                .and_then(|value| value.parse().ok());
            writeln!(
                stdout,
                "  On battery · runtime {}",
                record
                    .runtime_at_on_battery_seconds
                    .map_or_else(|| "unknown".into(), format_duration_seconds)
            )
            .map_err(output_error)?;
            save_record(host, Path::new(PENDING_PATH), record)?;
        }
        if let Some(on_battery_at) = on_battery_at
            && status.iter().any(|flag| flag == "OL")
        {
            let duration = now.duration_since(on_battery_at).unwrap_or_default();
            record.observed_blip_seconds = Some(duration.as_secs());
            save_record(host, Path::new(PENDING_PATH), record)?;
            writeln!(
                stdout,
                "  OB → OL observed after {}.",
                format_duration(duration)
            )
            .map_err(output_error)?;
            if duration < BLIP_MINIMUM {
                return Err(NutError::operational(format!(
                    "blip lasted {}; the drill expects at least 60 seconds. Report the result with `sudo nut drill report`.",
                    format_duration(duration)
                )));
            }
            return Ok(());
        }
        if now.duration_since(start).unwrap_or_default() >= BLIP_TIMEOUT {
            return Err(NutError::operational(
                "blip watch timed out; the pending record remains for `sudo nut drill report`",
            ));
        }
        host.sleep(POLL_INTERVAL);
    }
}

fn report<H: Host, W: Write>(host: &H, ascii: bool, stdout: &mut W) -> Result<(), NutError> {
    require_root_tty(host)?;
    confirm_hostname(host)?;
    let mut record = load_record(host, Path::new(PENDING_PATH))?
        .ok_or_else(|| NutError::operational("no drill report is pending"))?;
    let current_boot_id = read_trimmed(host, Path::new("/proc/sys/kernel/random/boot_id"))
        .ok()
        .flatten();
    let boot = if record.started_boot_id.is_some() && record.started_boot_id != current_boot_id {
        -1
    } else {
        0
    };
    record.journal = read_journal_observations(host, Some(boot))?;

    let outlet_question = match record.drill {
        DrillKind::Blip => "Did the UPS return to OL after mains was reconnected? [y/N] ",
        DrillKind::AcLoss | DrillKind::Cycle | DrillKind::Outage => {
            "Did the UPS outlets click off and return as expected? [y/N] "
        }
    };
    let host_question = format!(
        "Did {} {}? [y/N] ",
        record.hostname,
        record.drill.expected_host()
    );
    let outlet_behavior = ask_yes_no(host, outlet_question)?;
    let host_behavior = ask_yes_no(host, &host_question)?;
    let no_button_press = ask_yes_no(host, "Was the case power button left untouched? [y/N] ")?;
    let notifications_arrived = if record.drill == DrillKind::Blip {
        Some(ask_yes_no(
            host,
            "Did the on-battery and online notifications arrive? [y/N] ",
        )?)
    } else {
        None
    };
    let attestations = Attestations {
        outlet_behavior,
        host_behavior,
        no_button_press,
        notifications_arrived,
    };
    let blip_long_enough = record.drill != DrillKind::Blip
        || record
            .observed_blip_seconds
            .is_some_and(|seconds| seconds >= BLIP_MINIMUM.as_secs());
    let passed = outlet_behavior
        && host_behavior
        && no_button_press
        && notifications_arrived.unwrap_or(true)
        && blip_long_enough;
    record.reported_unix = Some(unix_seconds(host.now()));
    record.passed = Some(passed);
    record.attestations = Some(attestations);
    save_record(host, &record_path(record.drill), &record)?;
    write_private(host, Path::new(PENDING_PATH), b"")?;

    let glyph = if ascii {
        if passed { "ok" } else { "!!" }
    } else if passed {
        "✓"
    } else {
        "⚠"
    };
    writeln!(
        stdout,
        "{glyph} {} drill {}",
        record.drill.slug(),
        if passed { "passed" } else { "not proven" }
    )
    .map_err(output_error)?;
    writeln!(
        stdout,
        "  Read {} journal entries from boot {boot}.",
        record.journal.len()
    )
    .map_err(output_error)?;
    writeln!(stdout, "  Human observations are recorded as attestations.").map_err(output_error)?;
    if !passed {
        writeln!(
            stdout,
            "  Doctor will report this result; repeat with `sudo nut drill {}` after resolving it.",
            record.drill.slug()
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn ask_yes_no<H: Host>(host: &H, prompt: &str) -> Result<bool, NutError> {
    let answer = host
        .prompt_line(prompt)
        .map_err(|error| NutError::operational(error.to_string()))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn read_journal_observations<H: Host>(
    host: &H,
    boot: Option<i32>,
) -> Result<Vec<JournalObservation>, NutError> {
    let args = EventsArgs {
        boot,
        ..EventsArgs::default()
    };
    let history = journal::read(host, &args, true).map_err(|error| {
        NutError::operational(format!("cannot read the drill boot journal: {error:?}"))
    })?;
    let mut observations = history
        .entries
        .into_iter()
        .filter(|entry| {
            let message = entry.message.to_ascii_lowercase();
            [
                "on battery",
                "online",
                "shutdown",
                "fsd",
                "mail",
                "notification",
            ]
            .iter()
            .any(|needle| message.contains(needle))
        })
        .map(|entry| JournalObservation {
            timestamp_us: entry.timestamp_us,
            source: entry.source,
            message: mask_secrets(&entry.message).chars().take(500).collect(),
        })
        .collect::<Vec<_>>();
    if observations.len() > 50 {
        observations.drain(..observations.len() - 50);
    }
    Ok(observations)
}

pub fn write_pending_reminder<H: Host, W: Write>(
    host: &H,
    ascii: bool,
    stdout: &mut W,
) -> Result<(), NutError> {
    let Some(record) = load_record(host, Path::new(PENDING_PATH))? else {
        return Ok(());
    };
    let glyph = if ascii { "!!" } else { "⚠" };
    writeln!(
        stdout,
        "{glyph} Drill report pending: {} · run `sudo nut drill report`",
        record.drill.slug()
    )
    .map_err(output_error)
}

fn show_table<H: Host, W: Write>(
    host: &H,
    config_path: Option<&Path>,
    ascii: bool,
    json_output: bool,
    stdout: &mut W,
) -> Result<(), NutError> {
    let context = drill_context(
        host,
        DrillOptions {
            config_path,
            json: false,
            ascii,
            yes: false,
            ups: None,
            upsd_address: None,
            upsd_port: None,
        },
    )?;
    let version = live_readings(&context).ok().map(|live| live.nut_version);
    let pins = collect_pins(
        host,
        Some(&context.config_path),
        context.config.as_ref(),
        version.as_deref(),
    );
    let pending = load_record(host, Path::new(PENDING_PATH))?;
    let mut rows = Vec::new();
    for kind in DrillKind::ALL {
        let record = load_record(host, &record_path(kind))?;
        let state = match record.as_ref() {
            None => "never drilled".to_owned(),
            Some(record) => match (record.passed, pins.as_ref()) {
                (None, _) => "not reported".to_owned(),
                (Some(false), _) => "not proven".to_owned(),
                (Some(true), Ok(current)) => match stale_reasons(&record.pins, current) {
                    Some(reason) => format!("stale · {reason}"),
                    None if record.pins.nut_version.is_some() && current.nut_version.is_none() => {
                        "freshness unknown".to_owned()
                    }
                    None => "passed".to_owned(),
                },
                (Some(true), Err(_)) => "freshness unknown".to_owned(),
            },
        };
        rows.push(json!({
            "drill": kind.slug(),
            "state": state,
            "reported_unix": record.as_ref().and_then(|record| record.reported_unix),
            "started_unix": record.as_ref().map(|record| record.started_unix),
        }));
    }
    if json_output {
        serde_json::to_writer_pretty(
            &mut *stdout,
            &json!({
                "schema_version": 1,
                "pending": pending.as_ref().map(|record| record.drill.slug()),
                "proofs": rows,
            }),
        )
        .map_err(|error| NutError::operational(format!("cannot write drill JSON: {error}")))?;
        writeln!(stdout).map_err(output_error)?;
        return Ok(());
    }
    writeln!(stdout, "nut drill · proof table").map_err(output_error)?;
    if let Some(record) = pending {
        let pending_glyph = if ascii { "!!" } else { "⚠" };
        writeln!(
            stdout,
            "  {pending_glyph} {}  pending report · {}",
            record.drill.slug(),
            date(record.started_unix)
        )
        .map_err(output_error)?;
    }
    for row in rows {
        let kind = row["drill"].as_str().unwrap_or("unknown");
        let state = row["state"].as_str().unwrap_or("unknown");
        let timestamp = row["reported_unix"]
            .as_u64()
            .or_else(|| row["started_unix"].as_u64());
        if let Some(timestamp) = timestamp {
            writeln!(stdout, "  {kind:<8} {state} · {}", date(timestamp)).map_err(output_error)?;
        } else {
            writeln!(stdout, "  {kind:<8} {state}").map_err(output_error)?;
        }
    }
    Ok(())
}

fn collect_pins<H: Host>(
    host: &H,
    config_path: Option<&Path>,
    config: Option<&HostConfig>,
    nut_version: Option<&str>,
) -> Result<EvidencePins, String> {
    let dmi = DmiInfo::read(host).map_err(|error| error.to_string())?;
    let catalog = load_catalog(host)
        .or_else(|_| embedded_catalog())
        .map_err(|error| error.to_string())?;
    let board = resolve_profile(&catalog, config, &dmi);
    let fingerprint =
        efivar_fingerprint(host, &board.fingerprint_efivars).map_err(|error| error.to_string())?;
    let config_hash = config_hash(host, config_path)?;
    Ok(EvidencePins {
        bios_version: dmi.bios_version,
        efivar_fingerprint: fingerprint,
        nut_version: nut_version.map(str::to_owned),
        config_hash: Some(config_hash),
        battery_installed: config
            .and_then(|config| config.battery.as_ref())
            .map(|battery| battery.installed.clone()),
    })
}

fn config_hash<H: Host>(host: &H, config_path: Option<&Path>) -> Result<String, String> {
    let mut material = Vec::new();
    if let Some(path) = config_path {
        append_file_material(host, path, &mut material)?;
    }
    for (_, path) in LIVE_PATHS {
        let path = Path::new(path);
        if config_path != Some(path) {
            append_file_material(host, path, &mut material)?;
        }
    }
    let digest = Sha256::digest(&material);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn append_file_material<H: Host>(
    host: &H,
    path: &Path,
    material: &mut Vec<u8>,
) -> Result<(), String> {
    material.extend_from_slice(path.as_os_str().as_encoded_bytes());
    material.push(0);
    let contents = host.read_file(path).map_err(|error| error.to_string())?;
    if let Some(contents) = contents {
        material.extend_from_slice(b"present\0");
        let masked = mask_secrets(&String::from_utf8_lossy(&contents));
        material.extend_from_slice(masked.as_bytes());
    } else {
        material.extend_from_slice(b"absent");
    }
    material.push(0);
    Ok(())
}

fn parse_nut_version(text: String) -> Option<String> {
    text.split_ascii_whitespace()
        .find(|token| {
            let mut parts = token.split('.');
            parts
                .next()
                .is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                && parts
                    .next()
                    .is_some_and(|part| part.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|token| {
            token
                .split(|character: char| !character.is_ascii_digit() && character != '.')
                .next()
                .unwrap_or(token)
                .to_owned()
        })
}

fn load_record<H: Host>(host: &H, path: &Path) -> Result<Option<DrillRecord>, NutError> {
    let Some(bytes) = host.read_file(path)? else {
        return Ok(None);
    };
    parse_record(&bytes)
}

fn parse_record(bytes: &[u8]) -> Result<Option<DrillRecord>, NutError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let record = serde_json::from_slice(bytes)
        .map_err(|error| NutError::operational(format!("cannot parse drill record: {error}")))?;
    Ok(Some(record))
}

fn save_record<H: Host>(host: &H, path: &Path, record: &DrillRecord) -> Result<(), NutError> {
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| NutError::operational(format!("cannot encode drill record: {error}")))?;
    write_private(host, path, &bytes)
}

/// Publish a drill record atomically, without following links, as a
/// `root:root` 0600 file under the private 0700 state tree `nut init` manages.
fn write_private<H: Host>(host: &H, path: &Path, bytes: &[u8]) -> Result<(), NutError> {
    host.create_secure_dir(Path::new(STATE_DIR), &PRIVATE_DIR)?;
    host.create_secure_dir(Path::new(DRILLS_DIR), &PRIVATE_DIR)?;
    host.replace_file_atomic(path, bytes, &PRIVATE_FILE)?;
    Ok(())
}

pub fn proof_check<H: Host>(
    entry: &CatalogEntry,
    kind: DrillKind,
    host: &H,
    nut_version: Option<&str>,
    config_path: Option<&Path>,
    config: Option<&HostConfig>,
) -> Check {
    let path = record_path(kind);
    let never_drilled = || {
        skipped(
            entry,
            "never drilled",
            Some(format!("sudo nut drill {}", kind.slug())),
        )
    };
    let bytes = match host.read_file(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return never_drilled(),
        // `nut init` keeps /var/lib/nut-cli root-only, so a non-root doctor
        // cannot see proofs; skip like the other root-only state checks.
        Err(error) if error.is_permission_denied() => {
            return skipped(
                entry,
                "drill proofs are in the root-only state directory",
                Some("sudo nut doctor".to_owned()),
            );
        }
        Err(error) => return unknown(entry, error.to_string()),
    };
    let record = match parse_record(&bytes) {
        Ok(Some(record)) => record,
        Ok(None) => return never_drilled(),
        Err(error) => return unknown(entry, error.to_string()),
    };
    if record.reported_unix.is_none() {
        return warn(entry, "awaiting human report", "sudo nut drill report");
    }
    if record.passed != Some(true) {
        return warn(
            entry,
            format!("last {} drill was not proven", kind.slug()),
            format!("sudo nut drill {}", kind.slug()),
        );
    }
    let current = match collect_pins(host, config_path, config, nut_version) {
        Ok(current) => current,
        Err(error) => return unknown(entry, format!("cannot check proof freshness: {error}")),
    };
    if let Some(reason) = stale_reasons(&record.pins, &current) {
        return warn(
            entry,
            format!("stale proof: {reason}"),
            format!("sudo nut drill {}", kind.slug()),
        );
    }
    if record.pins.nut_version.is_some() && current.nut_version.is_none() {
        return unknown(
            entry,
            "upsd does not report a NUT version; proof freshness is unknown",
        );
    }
    pass(
        entry,
        format!(
            "last pass {} · proof is fresh",
            date(record.reported_unix.unwrap_or_default())
        ),
    )
}

fn stale_reasons(saved: &EvidencePins, current: &EvidencePins) -> Option<String> {
    let mut changes = Vec::new();
    if saved.bios_version != current.bios_version {
        changes.push("BIOS version changed");
    }
    if saved.efivar_fingerprint != current.efivar_fingerprint {
        changes.push("BIOS settings changed");
    }
    if saved.battery_installed != current.battery_installed {
        changes.push("battery install date changed");
    }
    if current.nut_version.is_some() && saved.nut_version != current.nut_version {
        changes.push("NUT version changed");
    }
    if saved.config_hash != current.config_hash {
        changes.push("NUT or host config changed");
    }
    (!changes.is_empty()).then(|| changes.join(", "))
}

fn record_path(kind: DrillKind) -> PathBuf {
    Path::new(DRILLS_DIR).join(format!("{}.json", kind.slug()))
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn date(seconds: u64) -> String {
    chrono::DateTime::<chrono::Local>::from(UNIX_EPOCH + Duration::from_secs(seconds))
        .format("%Y-%m-%d")
        .to_string()
}

fn format_duration_seconds(seconds: u64) -> String {
    format_duration(Duration::from_secs(seconds))
}

fn output_error(error: std::io::Error) -> NutError {
    NutError::operational(format!("cannot write output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{Status, find_entry};
    use crate::fake_upsd::{FakeUpsdFixture, serve_client};
    use crate::host::{FakeHost, ProcessOutput};
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::path::Path;
    use std::thread;

    fn pending_record(kind: DrillKind, pins: EvidencePins) -> DrillRecord {
        DrillRecord {
            schema_version: 1,
            drill: kind,
            hostname: "athena".into(),
            ups: "cp1500".into(),
            started_unix: 1_700_000_000,
            started_boot_id: Some("boot-before".into()),
            charge_percent_at_start: 100.0,
            runtime_at_start_seconds: Some(1550),
            runtime_at_on_battery_seconds: (kind == DrillKind::Blip).then_some(1500),
            observed_blip_seconds: (kind == DrillKind::Blip).then_some(60),
            pins,
            journal: Vec::new(),
            reported_unix: None,
            passed: None,
            attestations: None,
        }
    }

    fn report_host(prompts: impl IntoIterator<Item = &'static str>) -> FakeHost {
        FakeHost::default()
            .with_euid(0)
            .with_tty(true, true)
            .with_stdin_tty(true)
            .with_hostname("athena")
            .with_prompt_lines(prompts)
            .with_file("/proc/sys/kernel/random/boot_id", b"boot-after")
            .with_file("/sys/class/dmi/id/bios_version", b"1101")
            .with_dir("/var/log/journal")
            .with_program(
                "journalctl",
                ProcessOutput {
                    status: Some(0),
                    stdout: br#"{"__REALTIME_TIMESTAMP":"1700000010000000","__CURSOR":"cursor-1","_BOOT_ID":"boot-before","SYSLOG_IDENTIFIER":"upsmon","MESSAGE":"UPS cp1500@localhost on battery"}
"#.to_vec(),
                    stderr: Vec::new(),
                },
            )
    }

    #[test]
    fn evidence_pins_mark_each_safety_relevant_change_stale() {
        let saved = EvidencePins {
            bios_version: Some("1101".into()),
            efivar_fingerprint: Some("bios-a".into()),
            nut_version: Some("2.8.1".into()),
            config_hash: Some("config-a".into()),
            battery_installed: Some("2026-04-01".into()),
        };
        let changes = [
            (
                "BIOS version",
                EvidencePins {
                    bios_version: Some("1102".into()),
                    ..saved.clone()
                },
            ),
            (
                "BIOS settings",
                EvidencePins {
                    efivar_fingerprint: Some("bios-b".into()),
                    ..saved.clone()
                },
            ),
            (
                "battery install date",
                EvidencePins {
                    battery_installed: Some("2027-04-01".into()),
                    ..saved.clone()
                },
            ),
            (
                "NUT version",
                EvidencePins {
                    nut_version: Some("2.8.2".into()),
                    ..saved.clone()
                },
            ),
            (
                "config",
                EvidencePins {
                    config_hash: Some("config-b".into()),
                    ..saved.clone()
                },
            ),
        ];
        for (expected, current) in changes {
            assert!(stale_reasons(&saved, &current).unwrap().contains(expected));
        }
        assert_eq!(stale_reasons(&saved, &saved), None);
    }

    #[test]
    fn drill_kind_uses_stable_record_slugs() {
        assert_eq!(
            record_path(DrillKind::AcLoss),
            Path::new(DRILLS_DIR).join("ac-loss.json")
        );
        assert_eq!(
            DrillKind::ALL.map(DrillKind::slug),
            ["blip", "ac-loss", "cycle", "outage"]
        );
    }

    #[test]
    fn empty_drill_table_has_a_stable_snapshot() {
        let mut out = Vec::new();
        show_table(&FakeHost::default(), None, false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        insta::assert_snapshot!("drill_empty_table", text);
    }

    #[test]
    fn pending_report_is_reminded_in_the_status_view() {
        let host = FakeHost::default();
        save_record(
            &host,
            Path::new(PENDING_PATH),
            &pending_record(DrillKind::Outage, EvidencePins::default()),
        )
        .unwrap();
        let mut out = Vec::new();
        write_pending_reminder(&host, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("outage"));
        assert!(text.contains("sudo nut drill report"));
    }

    #[test]
    fn drill_refuses_unattended_and_missing_terminal_invocations() {
        let mut out = Vec::new();
        let err = run(
            Some(DrillAction::Blip),
            DrillOptions {
                config_path: None,
                json: false,
                ascii: false,
                yes: true,
                ups: None,
                upsd_address: None,
                upsd_port: None,
            },
            &FakeHost::default(),
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("refuses --yes"));

        assert!(
            require_root_tty(&FakeHost::default())
                .unwrap_err()
                .to_string()
                .contains("need root")
        );
        let no_stdin = FakeHost::default()
            .with_euid(0)
            .with_tty(true, true)
            .with_stdin_tty(false);
        assert!(
            require_root_tty(&no_stdin)
                .unwrap_err()
                .to_string()
                .contains("terminal")
        );
        let no_stdout = FakeHost::default()
            .with_euid(0)
            .with_tty(false, true)
            .with_stdin_tty(true);
        assert!(
            require_root_tty(&no_stdout)
                .unwrap_err()
                .to_string()
                .contains("terminal")
        );
    }

    #[test]
    fn hostname_confirmation_must_match_exactly() {
        let host = FakeHost::default()
            .with_hostname("athena")
            .with_prompt_lines(["apollo"]);
        assert!(
            confirm_hostname(&host)
                .unwrap_err()
                .to_string()
                .contains("did not match")
        );
    }

    #[test]
    fn ups_interlocks_reject_bad_status_and_low_charge() {
        for (status, charge) in [
            (vec!["OB"], 100.0),
            (vec!["OL", "OB"], 100.0),
            (vec!["OL", "LB"], 100.0),
            (vec!["OL", "FSD"], 100.0),
            (vec!["OL"], 94.9),
        ] {
            let readings = LiveReadings {
                status: status.into_iter().map(str::to_owned).collect(),
                charge,
                runtime: Some(1000),
                nut_version: "2.8.1".into(),
            };
            assert!(require_safe_ups(&readings).is_err(), "{readings:?}");
        }
        assert!(
            require_safe_ups(&LiveReadings {
                status: vec!["OL".into(), "CHRG".into()],
                charge: 95.0,
                runtime: Some(1000),
                nut_version: "2.8.1".into(),
            })
            .is_ok()
        );
    }

    #[test]
    fn persistent_journal_is_an_interlock() {
        let error = require_persistent_journal(&FakeHost::default()).unwrap_err();
        assert!(error.to_string().contains("persistent journald"));
    }

    #[test]
    fn a_pending_record_blocks_starting_another_drill() {
        let host = FakeHost::default()
            .with_euid(0)
            .with_tty(true, true)
            .with_stdin_tty(true)
            .with_hostname("athena")
            .with_prompt_lines(["athena"]);
        let record = pending_record(DrillKind::Outage, EvidencePins::default());
        save_record(&host, Path::new(PENDING_PATH), &record).unwrap();
        let error = begin(
            DrillKind::Cycle,
            DrillOptions {
                config_path: None,
                json: false,
                ascii: false,
                yes: false,
                ups: None,
                upsd_address: None,
                upsd_port: None,
            },
            &host,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("already pending"));
    }

    #[test]
    fn start_saves_evidence_before_showing_manual_power_steps() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut fixture = FakeUpsdFixture {
            ups_name: "cp1500".into(),
            description: "Test UPS".into(),
            variables: BTreeMap::from([
                ("ups.status".into(), "OL CHRG".into()),
                ("battery.charge".into(), "100".into()),
                ("battery.runtime".into(), "1550".into()),
            ]),
            ..Default::default()
        };
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, &mut fixture).unwrap();
        });
        let host = FakeHost::default()
            .with_euid(0)
            .with_tty(true, true)
            .with_stdin_tty(true)
            .with_hostname("athena")
            .with_prompt_lines(["athena"])
            .with_dir("/var/log/journal")
            .with_file("/sys/class/dmi/id/bios_version", b"1101");
        let mut out = Vec::new();
        let result = begin(
            DrillKind::AcLoss,
            DrillOptions {
                config_path: None,
                json: false,
                ascii: true,
                yes: false,
                ups: None,
                upsd_address: Some("127.0.0.1"),
                upsd_port: Some(port),
            },
            &host,
            &mut out,
        );
        server.join().unwrap();
        result.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Last config backup age unknown"));
        assert!(text.contains("Evidence saved before the drill"));
        assert!(text.contains("sudo systemctl poweroff"));
        assert!(text.contains("sudo nut drill report"));
        insta::assert_snapshot!("drill_start_ac_loss", text);
        let record = load_record(&host, Path::new(PENDING_PATH))
            .unwrap()
            .unwrap();
        assert_eq!(record.drill, DrillKind::AcLoss);
        assert_eq!(record.pins.nut_version.as_deref(), Some("2.8.1"));
        assert!(record.pins.config_hash.is_some());
        assert!(record.passed.is_none());
    }

    #[test]
    fn report_pins_each_drill_to_recovery_evidence_and_detects_staleness() {
        for kind in DrillKind::ALL {
            let prompts: Vec<&'static str> = if kind == DrillKind::Blip {
                vec!["athena", "y", "y", "y", "y"]
            } else {
                vec!["athena", "y", "y", "y"]
            };
            let host = report_host(prompts);
            let pins = collect_pins(&host, None, None, Some("2.8.1")).unwrap();
            let pending = pending_record(kind, pins);
            save_record(&host, Path::new(PENDING_PATH), &pending).unwrap();

            let mut output = Vec::new();
            report(&host, true, &mut output).unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("passed"), "{kind:?}: {text}");
            assert!(text.contains("1 journal entries"), "{kind:?}: {text}");
            if kind == DrillKind::Outage {
                insta::assert_snapshot!("drill_report_outage", text);
            }
            assert!(
                load_record(&host, Path::new(PENDING_PATH))
                    .unwrap()
                    .is_none()
            );

            let entry = find_entry(&format!("proof.{}", kind.slug())).unwrap();
            let check = proof_check(entry, kind, &host, Some("2.8.1"), None, None);
            assert_eq!(check.status, Status::Pass, "{kind:?}: {check:?}");

            host.write_file(Path::new("/sys/class/dmi/id/bios_version"), b"1102")
                .unwrap();
            let check = proof_check(entry, kind, &host, Some("2.8.1"), None, None);
            assert_eq!(check.status, Status::Warn, "{kind:?}: {check:?}");
            assert!(check.evidence.contains("BIOS version changed"));
        }
    }

    #[test]
    fn records_are_private_atomic_files_under_the_init_state_tree() {
        let host = FakeHost::default().with_euid(0);
        let record = pending_record(DrillKind::Blip, EvidencePins::default());
        save_record(&host, Path::new(PENDING_PATH), &record).unwrap();
        write_private(&host, Path::new(PENDING_PATH), b"").unwrap();

        for dir in [STATE_DIR, DRILLS_DIR] {
            let metadata = host.metadata(Path::new(dir)).unwrap().unwrap();
            assert!(metadata.is_dir, "{dir}");
            assert_eq!((metadata.uid, metadata.gid, metadata.mode), (0, 0, 0o700));
        }
        let metadata = host.metadata(Path::new(PENDING_PATH)).unwrap().unwrap();
        assert!(metadata.is_file);
        assert_eq!((metadata.uid, metadata.gid, metadata.mode), (0, 0, 0o600));
        let effects = host.effects();
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, crate::host::HostEffect::WriteFile { .. })),
            "{effects:?}"
        );
        assert!(
            load_record(&host, Path::new(PENDING_PATH))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unreadable_root_only_proofs_are_skipped_not_unknown() {
        for kind in DrillKind::ALL {
            let host = FakeHost::default().with_unreadable(record_path(kind));
            let entry = find_entry(&format!("proof.{}", kind.slug())).unwrap();
            let check = proof_check(entry, kind, &host, Some("2.8.1"), None, None);
            assert_eq!(check.status, Status::Skipped, "{kind:?}: {check:?}");
            assert!(check.evidence.contains("root-only"), "{check:?}");
            assert_eq!(check.fix.as_deref(), Some("sudo nut doctor"));
        }
    }
}
