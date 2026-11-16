use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use serde::Serialize;

use crate::cli::{BatteryArgs, ColorChoice, StatusArgs};
use crate::config::{self, ConfigError};
use crate::duration::DurationValue;
use crate::error::NutError;
use crate::host::Host;
use crate::ui::{UiContext, UiOptions, layout, theme};
use crate::upsd::{UpsdClient, UpsdError};

const DEFAULT_UPSD_ADDRESS: &str = "127.0.0.1";
const DEFAULT_UPSD_PORT: u16 = 3493;
const DEFAULT_UPS_NAME: &str = "primary";
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
pub struct OutputOptions<'a> {
    pub config_path: Option<&'a Path>,
    pub json: bool,
    pub verbose: bool,
    pub color: ColorChoice,
    pub ascii: bool,
}

#[derive(Clone, Debug, Serialize)]
struct StatusDocument {
    schema_version: u32,
    host: Option<String>,
    ups: String,
    upsd_address: String,
    upsd_port: u16,
    observed_at_unix_seconds: u64,
    status: StatusView,
    battery: BatteryView,
    power: PowerView,
    timeline: Option<TimelineView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_variables: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_outage: Option<crate::journal::Story>,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_battery_since_us: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
struct StatusView {
    flags: Vec<String>,
    summary: String,
}

#[derive(Clone, Debug, Serialize)]
struct BatteryView {
    charge_percent: Option<f64>,
    runtime_seconds: Option<u64>,
    test_result: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct PowerView {
    load_percent: Option<f64>,
    input_voltage: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
struct TimelineView {
    shutdown_after_on_battery_seconds: u64,
    estimated_runtime_seconds: Option<u64>,
    estimated_runtime_after_shutdown_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
struct Connection {
    host: Option<String>,
    ups: String,
    address: String,
    port: u16,
    shutdown_after: Option<DurationValue>,
}

pub fn run_status<H: Host, W: Write>(
    args: StatusArgs,
    options: OutputOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    let connection = resolve_connection(
        host,
        options.config_path,
        args.ups.as_deref(),
        args.upsd_address.as_deref(),
        args.upsd_port,
    )?;
    let ui = UiContext::new(
        UiOptions {
            color: options.color,
            ascii: options.ascii,
        },
        host,
        host.stdout_is_terminal(),
    );
    loop {
        let mut document = fetch_status(host, &connection, args.all)?;
        if connection.port == DEFAULT_UPSD_PORT {
            add_journal(host, options.config_path, &mut document);
        }
        if args.watch && !options.json && host.stdout_is_terminal() {
            stdout
                .write_all(b"\x1b[H\x1b[2J")
                .map_err(|error| NutError::operational(format!("cannot write output: {error}")))?;
        }
        if options.json {
            write_json(stdout, &document)?;
        } else {
            render_status(stdout, &document, &ui, options.verbose)?;
        }
        if !args.watch {
            return Ok(());
        }
        thread::sleep(WATCH_INTERVAL);
    }
}

pub fn run_battery<H: Host, W: Write>(
    args: BatteryArgs,
    options: OutputOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    let connection = resolve_connection(
        host,
        options.config_path,
        args.ups.as_deref(),
        args.upsd_address.as_deref(),
        args.upsd_port,
    )?;
    let document = fetch_status(host, &connection, false)?;
    if options.json {
        let output = serde_json::json!({
            "schema_version": 1,
            "host": document.host,
            "ups": document.ups,
            "observed_at_unix_seconds": document.observed_at_unix_seconds,
            "status": document.status,
            "battery": document.battery,
        });
        serde_json::to_writer_pretty(&mut *stdout, &output)
            .map_err(|error| NutError::operational(format!("cannot write JSON output: {error}")))?;
        writeln!(stdout).map_err(output_error)?;
        return Ok(());
    }
    let ui = UiContext::new(
        UiOptions {
            color: options.color,
            ascii: options.ascii,
        },
        host,
        host.stdout_is_terminal(),
    );
    render_battery(stdout, &document, &ui)
}

fn resolve_connection<H: Host>(
    host: &H,
    override_path: Option<&Path>,
    ups_override: Option<&str>,
    address_override: Option<&str>,
    port_override: Option<u16>,
) -> Result<Connection, NutError> {
    let config_requested = override_path.is_some() || host.env("NUT_CONFIG").is_some();
    let config = match config::load_config(host, override_path) {
        Ok((_, _, config)) => Some(config),
        Err(ConfigError::NotFound(_)) | Err(ConfigError::NoHome) if !config_requested => None,
        Err(error) => return Err(NutError::operational(error.to_string())),
    };
    let (host_name, configured_ups, shutdown_after) = match config.as_ref() {
        Some(config) => {
            let (ups_name, _) = config
                .ups_entry()
                .map_err(|error| NutError::operational(error.to_string()))?;
            (
                Some(config.host.name.clone()),
                Some(ups_name.to_owned()),
                Some(config.shutdown.early_after),
            )
        }
        None => (None, None, None),
    };
    let ups = ups_override
        .map(str::to_owned)
        .or(configured_ups)
        .unwrap_or_else(|| DEFAULT_UPS_NAME.to_owned());
    let address = address_override
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_UPSD_ADDRESS.to_owned());
    let port = port_override.unwrap_or(DEFAULT_UPSD_PORT);
    Ok(Connection {
        host: host_name,
        ups,
        address,
        port,
        shutdown_after,
    })
}

fn fetch_status<H: Host>(
    host: &H,
    connection: &Connection,
    include_all: bool,
) -> Result<StatusDocument, NutError> {
    let variables = UpsdClient::connect(&connection.address, connection.port, &connection.ups)
        .and_then(|mut client| client.list_vars())
        .map_err(map_upsd_error)?;
    let status = variables.get("ups.status").ok_or_else(|| {
        NutError::operational("upsd did not report the required ups.status variable")
    })?;
    let flags = status
        .split_ascii_whitespace()
        .map(|flag| flag.to_ascii_uppercase())
        .collect::<Vec<_>>();
    let summary = summarize_status(&flags);
    let charge = optional_number(&variables, "battery.charge")?;
    let runtime = optional_integer(&variables, "battery.runtime")?;
    let load = optional_number(&variables, "ups.load")?;
    let voltage = optional_number(&variables, "input.voltage")?;
    let shutdown_seconds = connection
        .shutdown_after
        .map(|delay| delay.as_duration().as_secs());
    let timeline = shutdown_seconds.map(|shutdown_after_on_battery_seconds| TimelineView {
        shutdown_after_on_battery_seconds,
        estimated_runtime_seconds: runtime,
        estimated_runtime_after_shutdown_seconds: runtime
            .map(|seconds| seconds.saturating_sub(shutdown_after_on_battery_seconds)),
    });
    let observed_at_unix_seconds = host
        .now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(StatusDocument {
        schema_version: 1,
        host: connection.host.clone(),
        ups: connection.ups.clone(),
        upsd_address: connection.address.clone(),
        upsd_port: connection.port,
        observed_at_unix_seconds,
        status: StatusView { flags, summary },
        battery: BatteryView {
            charge_percent: charge,
            runtime_seconds: runtime,
            test_result: variables.get("ups.test.result").cloned(),
        },
        power: PowerView {
            load_percent: load,
            input_voltage: voltage,
        },
        timeline,
        raw_variables: include_all.then_some(variables),
        last_outage: None,
        on_battery_since_us: None,
    })
}

fn add_journal<H: Host>(host: &H, config_path: Option<&Path>, document: &mut StatusDocument) {
    let Ok(history) = crate::journal::read(host, &Default::default(), false) else {
        return;
    };
    let boots = crate::journal::boots(host).unwrap_or_default();
    let notify = crate::events::notify_seconds(host, config_path).unwrap_or(30);
    let stories = crate::journal::pair(history.entries, &boots, notify);
    let relevant = |story: &&crate::journal::Story| {
        story.is_power() && story.ups.as_deref().is_none_or(|ups| ups == document.ups)
    };
    let last = stories.iter().rev().find(relevant);
    if document.status.flags.iter().any(|flag| flag == "OB") {
        document.on_battery_since_us = last
            .filter(|story| story.end_us.is_none() && story.incomplete == Some("still on battery"))
            .and_then(|story| story.start_us);
    }
    document.last_outage = last.cloned();
    if document.status.flags.iter().any(|flag| flag == "OL")
        && let Some(story) = &mut document.last_outage
        && story.end_us.is_none()
        && story.incomplete == Some("still on battery")
    {
        story.incomplete = Some("journal ends mid-outage; live UPS is on line");
    }
}

fn optional_number(
    variables: &BTreeMap<String, String>,
    name: &str,
) -> Result<Option<f64>, NutError> {
    variables
        .get(name)
        .map(|value| {
            let number = value.parse::<f64>().map_err(|_| {
                NutError::operational(format!("upsd returned an invalid number for {name}"))
            })?;
            if !number.is_finite() {
                return Err(NutError::operational(format!(
                    "upsd returned a non-finite number for {name}"
                )));
            }
            Ok(number)
        })
        .transpose()
}

fn optional_integer(
    variables: &BTreeMap<String, String>,
    name: &str,
) -> Result<Option<u64>, NutError> {
    variables
        .get(name)
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                NutError::operational(format!("upsd returned an invalid number for {name}"))
            })
        })
        .transpose()
}

fn summarize_status(flags: &[String]) -> String {
    let contains = |wanted: &str| flags.iter().any(|flag| flag == wanted);
    let mut parts = Vec::new();
    if contains("OL") {
        parts.push("On line".to_owned());
    } else if contains("OB") {
        parts.push("On battery".to_owned());
    } else if contains("OFF") {
        parts.push("UPS output off".to_owned());
    } else {
        parts.push("Power source unknown".to_owned());
    }
    for (flag, phrase) in [
        ("FSD", "forced shutdown"),
        ("LB", "low battery"),
        ("RB", "replace battery"),
        ("OVER", "overloaded"),
        ("BYPASS", "bypass mode"),
        ("BOOST", "voltage boost"),
        ("CAL", "calibrating"),
        ("TEST", "under test"),
        ("NOCOMM", "communication lost"),
        ("ALARM", "requires attention"),
    ] {
        if contains(flag) {
            parts.push(phrase.to_owned());
        }
    }
    if contains("DISCHRG") {
        parts.push("discharging".to_owned());
    } else if contains("CHRG") {
        parts.push("charging".to_owned());
    }
    if contains("COMM") {
        parts.push("communication available".to_owned());
    }
    if parts.len() == 1 && parts[0] == "On line" {
        return "On line".to_owned();
    }
    parts.join(", ")
}

fn write_json<W: Write>(stdout: &mut W, document: &StatusDocument) -> Result<(), NutError> {
    serde_json::to_writer_pretty(&mut *stdout, document)
        .map_err(|error| NutError::operational(format!("cannot write JSON output: {error}")))?;
    writeln!(stdout).map_err(output_error)
}

fn render_status<W: Write>(
    stdout: &mut W,
    document: &StatusDocument,
    ui: &UiContext,
    verbose: bool,
) -> Result<(), NutError> {
    let title = format!("nut status · {}", document.ups);
    writeln!(
        stdout,
        "{}",
        theme::Theme::paint(theme::Tone::Info, &title, ui.color_enabled)
    )
    .map_err(output_error)?;
    if let Some(host) = &document.host {
        write_label(stdout, "Host", host, ui.width)?;
    }
    let status_tone = status_tone(&document.status.flags);
    let state = theme::Theme::paint(status_tone, &document.status.summary, ui.color_enabled);
    write_label(stdout, "State", &state, ui.width)?;
    if let Some(charge) = document.battery.charge_percent {
        let value = format!("{}%  {}", format_percent(charge), bar(charge, ui));
        write_label(stdout, "Battery charge", &value, ui.width)?;
    }
    if let Some(runtime) = document.battery.runtime_seconds {
        write_label(
            stdout,
            "Runtime",
            &format!(
                "{} estimated",
                layout::format_duration(Duration::from_secs(runtime))
            ),
            ui.width,
        )?;
    }
    if let Some(load) = document.power.load_percent {
        let value = format!("{}%  {}", format_percent(load), bar(load, ui));
        write_label(stdout, "Load", &value, ui.width)?;
    }
    if let Some(voltage) = document.power.input_voltage {
        write_label(
            stdout,
            "Input voltage",
            &format!("{voltage:.1} V"),
            ui.width,
        )?;
    }
    if let Some(test_result) = &document.battery.test_result {
        write_label(stdout, "Battery test", test_result, ui.width)?;
    }
    if let Some(timeline) = &document.timeline {
        writeln!(stdout).map_err(output_error)?;
        writeln!(
            stdout,
            "If power fails now, the host shutdown timer is {} on battery.",
            layout::format_duration(Duration::from_secs(
                timeline.shutdown_after_on_battery_seconds
            ))
        )
        .map_err(output_error)?;
        if let Some(runtime) = timeline.estimated_runtime_seconds {
            writeln!(
                stdout,
                "The UPS estimates {} at the current load.",
                layout::format_duration(Duration::from_secs(runtime))
            )
            .map_err(output_error)?;
            if let Some(remaining) = timeline.estimated_runtime_after_shutdown_seconds {
                writeln!(
                    stdout,
                    "Estimated runtime after that timer: {}.",
                    layout::format_duration(Duration::from_secs(remaining))
                )
                .map_err(output_error)?;
            }
        }
    }
    if let Some(start) = document.on_battery_since_us {
        let elapsed = document
            .observed_at_unix_seconds
            .saturating_sub(start / 1_000_000);
        write_label(
            stdout,
            "On battery for",
            &layout::format_duration(Duration::from_secs(elapsed)),
            ui.width,
        )?;
    }
    if let Some(story) = &document.last_outage {
        write_history_label(
            stdout,
            "Last outage",
            &format!(
                "{} · {}",
                crate::events::time_label(
                    story.timestamp_us(),
                    document.observed_at_unix_seconds.saturating_mul(1_000_000),
                    verbose
                ),
                crate::events::summary(story)
            ),
            ui.width,
        )?;
    }
    if let Some(variables) = &document.raw_variables {
        writeln!(stdout).map_err(output_error)?;
        writeln!(stdout, "Raw NUT variables").map_err(output_error)?;
        for (name, value) in variables {
            write_label(stdout, name, value, ui.width)?;
        }
    }
    if verbose {
        writeln!(
            stdout,
            "Observed at (Unix seconds)  {}",
            document.observed_at_unix_seconds
        )
        .map_err(output_error)?;
        writeln!(
            stdout,
            "upsd                       {}:{}",
            document.upsd_address, document.upsd_port
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn render_battery<W: Write>(
    stdout: &mut W,
    document: &StatusDocument,
    ui: &UiContext,
) -> Result<(), NutError> {
    let title = format!("nut battery · {}", document.ups);
    writeln!(
        stdout,
        "{}",
        theme::Theme::paint(theme::Tone::Info, &title, ui.color_enabled)
    )
    .map_err(output_error)?;
    if let Some(charge) = document.battery.charge_percent {
        let value = format!("{}%  {}", format_percent(charge), bar(charge, ui));
        write_label(stdout, "Charge", &value, ui.width)?;
    } else {
        write_label(stdout, "Charge", "Not reported", ui.width)?;
    }
    if let Some(runtime) = document.battery.runtime_seconds {
        write_label(
            stdout,
            "Runtime",
            &format!(
                "{} estimated",
                layout::format_duration(Duration::from_secs(runtime))
            ),
            ui.width,
        )?;
    } else {
        write_label(stdout, "Runtime", "Not reported", ui.width)?;
    }
    write_label(stdout, "UPS state", &document.status.summary, ui.width)?;
    if let Some(test_result) = &document.battery.test_result {
        write_label(stdout, "Last self-test", test_result, ui.width)?;
    }
    Ok(())
}

fn write_label<W: Write>(
    stdout: &mut W,
    label: &str,
    value: &str,
    width: usize,
) -> Result<(), NutError> {
    writeln!(stdout, "{}", layout::label_value(label, value, width)).map_err(output_error)
}

fn write_history_label<W: Write>(
    stdout: &mut W,
    label: &str,
    value: &str,
    width: usize,
) -> Result<(), NutError> {
    const LABEL_WIDTH: usize = 16;
    write!(stdout, "{label:<LABEL_WIDTH$}").map_err(output_error)?;
    let mut column = LABEL_WIDTH;
    for word in value.split_whitespace() {
        if column > LABEL_WIDTH && column + word.chars().count() + 1 > width {
            write!(stdout, "\n{}", " ".repeat(LABEL_WIDTH)).map_err(output_error)?;
            column = LABEL_WIDTH;
        }
        if column > LABEL_WIDTH {
            write!(stdout, " ").map_err(output_error)?;
            column += 1;
        }
        write!(stdout, "{word}").map_err(output_error)?;
        column += word.chars().count();
    }
    writeln!(stdout).map_err(output_error)
}

fn output_error(error: std::io::Error) -> NutError {
    NutError::operational(format!("cannot write output: {error}"))
}

fn format_percent(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn bar(value: f64, ui: &UiContext) -> String {
    layout::render_bar(value / 100.0, 12, ui.glyphs.bar_full == "#")
}

fn status_tone(flags: &[String]) -> theme::Tone {
    if flags
        .iter()
        .any(|flag| matches!(flag.as_str(), "FSD" | "OFF"))
    {
        theme::Tone::Fail
    } else if flags
        .iter()
        .any(|flag| matches!(flag.as_str(), "LB" | "RB" | "OVER" | "NOCOMM"))
    {
        theme::Tone::Warn
    } else {
        theme::Tone::Ok
    }
}

fn map_upsd_error(error: UpsdError) -> NutError {
    NutError::operational(format!(
        "{error}; check that upsd is running and the configured UPS name is correct"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_upsd::{FakeUpsdFixture, serve_client};
    use crate::host::FakeHost;
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn journal_summary_is_optional_and_current_battery_start_requires_live_ob() {
        let host = crate::journal::tests::fixture_host();
        let mut document = StatusDocument {
            schema_version: 1,
            host: None,
            ups: "primary".into(),
            upsd_address: "127.0.0.1".into(),
            upsd_port: 3493,
            observed_at_unix_seconds: 1_700_000_900,
            status: StatusView {
                flags: vec!["OB".into()],
                summary: "On battery".into(),
            },
            battery: BatteryView {
                charge_percent: None,
                runtime_seconds: None,
                test_result: None,
            },
            power: PowerView {
                load_percent: None,
                input_voltage: None,
            },
            timeline: None,
            raw_variables: None,
            last_outage: None,
            on_battery_since_us: None,
        };
        add_journal(&host, None, &mut document);
        assert_eq!(document.on_battery_since_us, Some(1_700_000_700_000_000));
        assert!(document.last_outage.is_some());
        let ui = UiContext::new(
            UiOptions {
                color: crate::cli::ColorChoice::Never,
                ascii: true,
            },
            &host,
            false,
        );
        let mut text = Vec::new();
        render_status(&mut text, &document, &ui, false).unwrap();
        assert!(String::from_utf8(text).unwrap().contains("Last outage"));
        document.status.flags = vec!["OL".into()];
        document.on_battery_since_us = None;
        add_journal(&host, None, &mut document);
        assert!(document.on_battery_since_us.is_none());
        document.last_outage = None;
        add_journal(&FakeHost::default(), None, &mut document);
        assert!(document.last_outage.is_none());
    }

    #[test]
    fn decodes_status_flags_as_short_sentences() {
        assert_eq!(
            summarize_status(&["OB".into(), "DISCHRG".into()]),
            "On battery, discharging"
        );
        assert_eq!(
            summarize_status(&["OL".into(), "CHRG".into()]),
            "On line, charging"
        );
        assert_eq!(
            summarize_status(&["OB".into(), "LB".into(), "FSD".into()]),
            "On battery, forced shutdown, low battery"
        );
    }

    #[test]
    fn percentages_are_compact_and_safe_for_bars() {
        assert_eq!(format_percent(100.0), "100");
        assert_eq!(format_percent(32.5), "32.5");
        assert_eq!(layout::render_bar(3.0, 4, true), "####");
    }

    #[test]
    fn status_json_uses_scripted_upsd_and_configured_shutdown_timeline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let fixture = FakeUpsdFixture {
            ups_name: "demo".into(),
            description: "Scripted demo UPS".into(),
            variables: BTreeMap::from([
                ("ups.status".into(), "OB DISCHRG".into()),
                ("battery.charge".into(), "82".into()),
                ("battery.runtime".into(), "1550".into()),
                ("ups.load".into(), "28".into()),
            ]),
            ..Default::default()
        };
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, &fixture).unwrap();
        });
        let config = "schema = 1\n\n[host]\nname = \"demo\"\n\n[ups.demo]\nprofile = \"dummy\"\n\n[shutdown]\nnotify_after = \"30s\"\nearly_after = \"180s\"\npoweroff_wait = \"30m\"\nshutdown_budget = \"2m\"\n\n[notify]\nfrom = \"root@example.invalid\"\nto = \"operator@example.invalid\"\ncommbad_after = \"60s\"\n";
        let host = FakeHost::default()
            .with_file("/test/host.toml", config.as_bytes())
            .with_env("LANG", "C.UTF-8");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = [
            "nut".to_owned(),
            "--json".into(),
            "--config".into(),
            "/test/host.toml".into(),
            "status".into(),
            "--upsd-port".into(),
            port.to_string(),
        ];
        let code = crate::run_with_host(args, &host, &mut stdout, &mut stderr);
        server.join().unwrap();
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["schema_version"], 1);
        assert_eq!(output["status"]["summary"], "On battery, discharging");
        assert_eq!(output["battery"]["runtime_seconds"], 1550);
        assert_eq!(
            output["timeline"]["estimated_runtime_after_shutdown_seconds"],
            1370
        );
        assert!(output.get("raw_variables").is_none());
    }
}
