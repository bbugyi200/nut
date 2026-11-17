mod checks;
mod engine;
mod snapshot;
mod types;

use std::io::Write;
use std::time::Duration;

use serde_json::{Value, json};

use crate::cli::{ColorChoice, DoctorArgs};
use crate::error::NutError;
use crate::host::Host;
use crate::ui::theme::{Theme, Tone};
use crate::ui::{UiContext, UiOptions};

pub use engine::{catalog, find_entry};
pub use types::{CatalogEntry, Check, Section, Status};
pub(crate) use types::{pass, skipped, unknown, warn};

use engine::{Summary, run_checks, summarize, validate_filters};
use snapshot::{GatherOptions, Snapshot};

pub fn run<H: Host, W: Write>(
    args: DoctorArgs,
    options: DoctorOptions<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<i32, NutError> {
    if args.list {
        return write_list(host, options, stdout);
    }
    if let Err(error) = validate_filters(&args.checks) {
        return Err(NutError::usage(error));
    }
    let observe = args.observe.map(Duration::from_secs);
    let address = args.upsd_address.as_deref().unwrap_or("127.0.0.1");
    let port = args.upsd_port.unwrap_or(3493);
    let snapshot = Snapshot::gather(
        host,
        GatherOptions {
            config_path: options.config_path,
            observe,
            ups: args.ups.as_deref(),
            address,
            port,
        },
    );
    let checks = run_checks(host, &snapshot, &args.checks);
    let summary = summarize(&checks);
    if options.json {
        write_json(stdout, &snapshot, &checks, &summary, args.strict)?;
    } else {
        let ui = UiContext::new(
            UiOptions {
                color: options.color,
                ascii: options.ascii,
            },
            host,
            host.stdout_is_terminal(),
        );
        write_report(stdout, &snapshot, &checks, &summary, &ui)?;
    }
    Ok(exit_code(&summary, args.strict))
}

pub struct DoctorOptions<'a> {
    pub config_path: Option<&'a std::path::Path>,
    pub json: bool,
    pub ascii: bool,
    pub color: ColorChoice,
}

pub fn catalog_markdown() -> String {
    let mut out = String::from(
        "# Doctor checks\n\nGenerated from the nut doctor registry. Do not edit by hand; run `just docs-gen`.\n",
    );
    let mut current = None;
    for entry in catalog() {
        if current != Some(entry.section) {
            current = Some(entry.section);
            out.push_str(&format!("\n## {}\n", entry.section.heading()));
        }
        out.push_str(&format!(
            "\n### `{}`\n\n{}\n\nNeeds root: {}\n",
            entry.id,
            entry.docs,
            if entry.needs_root { "yes" } else { "no" }
        ));
    }
    out
}

pub fn catalog_ids() -> Vec<(String, String)> {
    catalog()
        .iter()
        .map(|entry| (entry.id.to_owned(), entry.title.to_owned()))
        .collect()
}

fn write_list<H: Host, W: Write>(
    host: &H,
    options: DoctorOptions<'_>,
    stdout: &mut W,
) -> Result<i32, NutError> {
    if options.json {
        let checks: Vec<Value> = catalog()
            .iter()
            .map(|entry| {
                json!({
                    "id": entry.id,
                    "section": entry.section,
                    "title": entry.title,
                    "docs": entry.docs,
                    "needs_root": entry.needs_root,
                })
            })
            .collect();
        serde_json::to_writer_pretty(
            &mut *stdout,
            &json!({ "schema_version": 1, "checks": checks }),
        )
        .map_err(write_error)?;
        writeln!(stdout).map_err(write_error)?;
        return Ok(0);
    }
    let ui = UiContext::new(
        UiOptions {
            color: options.color,
            ascii: options.ascii,
        },
        host,
        host.stdout_is_terminal(),
    );
    let mut current = None;
    for entry in catalog() {
        if current != Some(entry.section) {
            current = Some(entry.section);
            writeln!(
                stdout,
                "{}",
                Theme::paint(Tone::Info, entry.section.heading(), ui.color_enabled)
            )
            .map_err(write_error)?;
        }
        writeln!(stdout, "  {:<22} {}", entry.id, entry.title).map_err(write_error)?;
    }
    Ok(0)
}

fn write_report<W: Write>(
    stdout: &mut W,
    snapshot: &Snapshot,
    checks: &[Check],
    summary: &Summary,
    ui: &UiContext,
) -> Result<(), NutError> {
    let when = chrono::DateTime::<chrono::Local>::from(snapshot.now).format("%Y-%m-%d %H:%M");
    let ascii = ui.glyphs.arrow == "->";
    let dot = if ascii { " | " } else { " · " };
    writeln!(
        stdout,
        " {}",
        Theme::paint(
            Tone::Info,
            &format!("nut doctor{dot}{}{dot}{when}", snapshot.hostname),
            ui.color_enabled
        )
    )
    .map_err(write_error)?;
    let mut current = None;
    for check in checks {
        if current != Some(check.section) {
            current = Some(check.section);
            writeln!(stdout, " {}", check.section.heading()).map_err(write_error)?;
        }
        let glyph = Theme::paint(
            check.status.tone(),
            check.status.glyph(&ui.glyphs),
            ui.color_enabled,
        );
        let glyph_plain = check.status.glyph(&ui.glyphs);
        let prefix = format!("   {glyph} {:<22} ", check.id);
        let prefix_width = 3 + glyph_plain.chars().count() + 1 + 22 + 1;
        write_prefixed(stdout, &prefix, prefix_width, &check.evidence, ui.width)?;
        if let Some(fix) = &check.fix {
            let arrow = ui.glyphs.arrow;
            let hang = format!(
                "{} {arrow} ",
                " ".repeat(prefix_width.saturating_sub(arrow.chars().count() + 2))
            );
            let hang_width = hang.chars().count();
            write_prefixed(stdout, &hang, hang_width, fix, ui.width)?;
        }
    }
    write_prefixed(stdout, " ", 1, &summary.line(ascii), ui.width)?;
    Ok(())
}

fn write_prefixed<W: Write>(
    stdout: &mut W,
    prefix: &str,
    prefix_width: usize,
    text: &str,
    width: usize,
) -> Result<(), NutError> {
    let width = width.max(8);
    let indent = prefix_width.min(width.saturating_sub(1));
    let hang = " ".repeat(indent);
    let mut current = prefix.to_owned();
    let mut column = prefix_width;
    let mut at_line_start = true;
    for word in text.split_whitespace() {
        if !at_line_start && column + 1 + word.chars().count() > width {
            writeln!(stdout, "{}", current.trim_end()).map_err(write_error)?;
            current = hang.clone();
            column = indent;
            at_line_start = true;
        }
        if !at_line_start {
            current.push(' ');
            column += 1;
        }
        for character in word.chars() {
            if column >= width {
                writeln!(stdout, "{}", current.trim_end()).map_err(write_error)?;
                current = hang.clone();
                column = indent;
            }
            current.push(character);
            column += 1;
            at_line_start = false;
        }
    }
    writeln!(stdout, "{}", current.trim_end()).map_err(write_error)
}

fn write_json<W: Write>(
    stdout: &mut W,
    snapshot: &Snapshot,
    checks: &[Check],
    summary: &Summary,
    strict: bool,
) -> Result<(), NutError> {
    let document = json!({
        "schema_version": 1,
        "host": snapshot.hostname,
        "ups": snapshot.ups_name,
        "observed_at_unix_seconds": snapshot.now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
        "strict": strict,
        "observe_seconds": snapshot.observe.map(|duration| duration.as_secs()),
        "summary": {
            "passed": summary.passed,
            "warnings": summary.warnings,
            "failed": summary.failed,
            "unknown": summary.unknown,
            "skipped": summary.skipped,
        },
        "checks": checks,
    });
    serde_json::to_writer_pretty(&mut *stdout, &document).map_err(write_error)?;
    writeln!(stdout).map_err(write_error)?;
    Ok(())
}

fn exit_code(summary: &Summary, strict: bool) -> i32 {
    if summary.failed > 0 || summary.unknown > 0 || (strict && summary.warnings > 0) {
        1
    } else {
        0
    }
}

fn write_error(error: impl std::fmt::Display) -> NutError {
    NutError::operational(format!("cannot write doctor output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{FakeHost, FileMetadata, ProcessOutput};
    use crate::run_with_host;
    use std::time::{Duration, UNIX_EPOCH};

    fn athena_toml() -> &'static [u8] {
        include_bytes!("../../tests/fixtures/hosts/athena/nut.toml")
    }

    fn output(stdout: &[u8]) -> ProcessOutput {
        ProcessOutput {
            status: Some(0),
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    fn enabled() -> ProcessOutput {
        ProcessOutput {
            status: Some(0),
            stdout: b"enabled\n".to_vec(),
            stderr: Vec::new(),
        }
    }

    fn fixture_host() -> FakeHost {
        let nut_meta = FileMetadata {
            uid: 0,
            gid: 42,
            mode: 0o640,
            is_file: true,
            is_dir: false,
        };
        let hook_meta = FileMetadata {
            uid: 0,
            gid: 0,
            mode: 0o755,
            is_file: true,
            is_dir: false,
        };
        FakeHost::default()
            .with_hostname("athena")
            .with_euid(1000)
            .with_env("HOME", "/home/bryan")
            .with_env("LANG", "C.UTF-8")
            .with_env("TERM", "xterm")
            .with_current_exe("/home/bryan/nut")
            .at(UNIX_EPOCH + Duration::from_secs(1_763_323_380))
            .with_file("/config.toml", athena_toml())
            .with_file("/etc/group", b"root:x:0:\nnut:x:42:bryan\n")
            .with_file("/sys/bus/usb/devices/1-4/idVendor", b"0764\n")
            .with_file("/sys/bus/usb/devices/1-4/idProduct", b"0601\n")
            .with_file("/sys/bus/usb/devices/1-4/busnum", b"1\n")
            .with_file("/sys/bus/usb/devices/1-4/devnum", b"4\n")
            .with_file("/dev/bus/usb/001/004", b"")
            .with_metadata(
                "/dev/bus/usb/001/004",
                FileMetadata {
                    uid: 0,
                    gid: 42,
                    mode: 0o660,
                    is_file: true,
                    is_dir: false,
                },
            )
            .with_file("/usr/local/sbin/athena-upssched-cmd", b"#!/bin/sh\n")
            .with_metadata("/usr/local/sbin/athena-upssched-cmd", hook_meta)
            .with_file("/usr/sbin/sendmail", b"")
            .with_file("/etc/fstab", b"UUID=a /mnt/poseidon ext4 defaults,nofail 0 2\nUUID=b /mnt/hades ntfs nls=utf8,umask=0222,uid=1000,gid=1000,ro,nofail 0 0\n")
            .with_dir("/var/log/journal")
            .with_file("/proc/sys/kernel/panic", b"0\n")
            .with_file("/sys/class/dmi/id/board_vendor", b"ASUSTeK COMPUTER INC.\n")
            .with_file("/sys/class/dmi/id/board_name", b"PRIME TRX40-PRO\n")
            .with_file("/sys/class/dmi/id/bios_version", b"1101\n")
            .with_file(
                "/proc/net/tcp",
                b"  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:0DA5 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000000000000000 0 0 0 10 -1\n",
            )
            .with_file("/proc/net/tcp6", b"  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n")
            .with_program_args("pgrep", ["-x", "pwrstatd"], ProcessOutput {
                status: Some(1),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
            .with_program_args("systemctl", ["is-enabled", "nut-driver@cp1500.service"], enabled())
            .with_program_args("systemctl", ["is-enabled", "nut-server.service"], enabled())
            .with_program_args("systemctl", ["is-enabled", "nut-monitor.service"], enabled())
            .with_program_args("systemctl", ["is-enabled", "nut.target"], enabled())
            .with_program_args(
                "systemctl",
                ["show", "nut-driver@cp1500.service", "-p", "ActiveState", "-p", "NRestarts"],
                output(b"ActiveState=active\nNRestarts=0\n"),
            )
            .with_program_args(
                "systemctl",
                ["show", "nut-server.service", "-p", "ActiveState", "-p", "NRestarts"],
                output(b"ActiveState=active\nNRestarts=0\n"),
            )
            .with_program_args(
                "systemctl",
                ["show", "nut-monitor.service", "-p", "ActiveState", "-p", "NRestarts"],
                output(b"ActiveState=active\nNRestarts=0\n"),
            )
            .with_program(
                "efibootmgr",
                output(b"BootCurrent: 0002\nBootOrder: 0002,0001\nBoot0002* debian\nBoot0001* Windows Boot Manager\n"),
            )
            .with_program_args("findmnt", ["--verify"], output(b""))
            .with_metadata("/etc/nut/nut.conf", nut_meta.clone())
            .with_metadata("/etc/nut/ups.conf", nut_meta.clone())
            .with_metadata("/etc/nut/upssched.conf", nut_meta.clone())
            .with_metadata("/etc/nut/upsd.users", nut_meta.clone())
            .with_metadata("/etc/nut/upsmon.conf", nut_meta)
    }

    fn execute(args: &[&str], host: &FakeHost) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(args.iter().copied(), host, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn list_prints_stable_ids_and_json_catalog() {
        let host = FakeHost::default()
            .with_env("LANG", "C.UTF-8")
            .with_env("TERM", "xterm");
        let (code, text, err) = execute(&["nut", "doctor", "--list"], &host);
        assert_eq!(code, 0, "{err}");
        assert!(text.contains("hw.usb"));
        assert!(text.contains("protect.poweroff-wait"));
        assert!(text.contains("proof.outage"));
        let (code, text, err) = execute(&["nut", "--json", "doctor", "--list"], &host);
        assert_eq!(code, 0, "{err}");
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json["checks"].as_array().unwrap().len() >= 30);
    }

    #[test]
    fn unknown_filter_is_usage() {
        let (code, _, err) = execute(&["nut", "doctor", "no-such-check"], &FakeHost::default());
        assert_eq!(code, 2);
        assert!(err.contains("unknown doctor check"));
    }

    #[test]
    fn root_checks_skip_and_proof_is_pending_without_upsd() {
        let host = fixture_host();
        let (code, text, err) = execute(
            &[
                "nut",
                "--ascii",
                "--config",
                "/config.toml",
                "doctor",
                "--upsd-port",
                "1",
                "tool",
                "hw.usb",
                "protect.hook",
                "protect.pwrstatd",
                "protect.killpower",
                "root",
                "proof",
            ],
            &host,
        );
        assert_eq!(code, 0, "{err}\n{text}");
        assert!(text.contains("root-only"), "{text}");
        assert!(text.contains("never drilled"), "{text}");
        assert!(text.contains("sudo nut doctor"), "{text}");
        assert!(host.slept().is_zero());
    }

    #[test]
    fn doctor_screens_at_supported_widths_and_ascii() {
        for width in [60, 80, 120] {
            for ascii in [false, true] {
                let host = fixture_host().with_env("COLUMNS", width.to_string());
                let mut args = vec![
                    "nut",
                    "--config",
                    "/config.toml",
                    "doctor",
                    "--upsd-port",
                    "1",
                ];
                if ascii {
                    args.insert(1, "--ascii");
                }
                let (_code, output, error) = execute(&args, &host);
                assert!(error.is_empty(), "{error}");
                assert!(
                    output.lines().all(|line| line.chars().count() <= width),
                    "{output}"
                );
                assert!(!output.contains('\u{1b}'));
                if ascii {
                    assert!(output.is_ascii(), "{output}");
                }
                insta::assert_snapshot!(
                    format!("doctor_{width}_{}", if ascii { "ascii" } else { "unicode" }),
                    output
                );
            }
        }
    }

    #[test]
    fn observe_sleeps_without_blocking_the_fake_host() {
        let host = fixture_host();
        let (code, text, err) = execute(
            &[
                "nut",
                "--ascii",
                "--config",
                "/config.toml",
                "doctor",
                "--upsd-port",
                "1",
                "--observe",
                "30",
                "svc.restarts",
            ],
            &host,
        );
        assert_eq!(code, 0, "{err}\n{text}");
        assert_eq!(host.slept(), Duration::from_secs(30));
        assert!(text.contains("NRestarts"), "{text}");
    }

    #[test]
    fn panic_in_one_check_becomes_unknown() {
        let checks = engine::run_checks(
            &FakeHost::default(),
            &Snapshot::gather(
                &FakeHost::default(),
                GatherOptions {
                    config_path: None,
                    observe: None,
                    ups: None,
                    address: "127.0.0.1",
                    port: 1,
                },
            ),
            &["tool.config".into()],
        );
        assert_eq!(checks.len(), 1);
        assert_ne!(checks[0].status, Status::Fail);
    }

    #[test]
    fn catalog_markdown_lists_every_id() {
        let markdown = catalog_markdown();
        for entry in catalog() {
            assert!(
                markdown.contains(&format!("`{}`", entry.id)),
                "{}",
                entry.id
            );
        }
        assert_eq!(catalog().len(), 43);
        let mut ids: Vec<_> = catalog().iter().map(|entry| entry.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), catalog().len());
    }

    #[test]
    fn doctor_checks_markdown_matches_registry() {
        let on_disk = include_str!("../../docs/doctor-checks.md");
        assert_eq!(
            on_disk,
            catalog_markdown(),
            "docs/doctor-checks.md is stale; run `just docs-gen`"
        );
    }

    #[test]
    #[ignore = "run via just docs-gen"]
    fn write_doctor_checks_markdown() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/doctor-checks.md");
        std::fs::write(path, catalog_markdown()).unwrap();
    }

    #[test]
    fn usb_without_host_config_matches_catalog_ids_not_every_device() {
        let host = FakeHost::default()
            .with_hostname("athena")
            .with_euid(1000)
            .with_env("LANG", "C.UTF-8")
            .with_env("TERM", "xterm")
            .with_file("/etc/group", b"root:x:0:\nnut:x:42:bryan\n")
            .with_file("/sys/bus/usb/devices/1-4/idVendor", b"0764\n")
            .with_file("/sys/bus/usb/devices/1-4/idProduct", b"0601\n")
            .with_file("/sys/bus/usb/devices/1-4/busnum", b"1\n")
            .with_file("/sys/bus/usb/devices/1-4/devnum", b"4\n")
            .with_file("/dev/bus/usb/001/004", b"")
            .with_metadata(
                "/dev/bus/usb/001/004",
                FileMetadata {
                    uid: 0,
                    gid: 42,
                    mode: 0o660,
                    is_file: true,
                    is_dir: false,
                },
            )
            .with_file("/sys/bus/usb/devices/1-5/idVendor", b"1d6b\n")
            .with_file("/sys/bus/usb/devices/1-5/idProduct", b"0002\n")
            .with_file("/sys/bus/usb/devices/1-5/busnum", b"1\n")
            .with_file("/sys/bus/usb/devices/1-5/devnum", b"1\n");
        let (code, text, err) = execute(
            &[
                "nut",
                "--ascii",
                "--json",
                "doctor",
                "--upsd-port",
                "1",
                "hw.usb",
            ],
            &host,
        );
        assert_eq!(code, 0, "{err}\n{text}");
        let json: Value = serde_json::from_str(&text).unwrap();
        let usb = json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "hw.usb")
            .unwrap();
        assert_eq!(usb["status"], "pass", "{json}");
        assert!(
            usb["evidence"].as_str().unwrap().contains("0764:0601"),
            "{usb}"
        );
    }

    #[test]
    fn poweroff_wait_warns_against_live_runtime_from_fake_upsd() {
        use crate::host::UpsObservation;
        use std::collections::BTreeMap;

        let host = fixture_host().with_ups_observation(UpsObservation {
            vars: BTreeMap::from([
                ("ups.status".into(), "OL".into()),
                ("battery.charge".into(), "100".into()),
                ("battery.runtime".into(), "1550".into()),
                ("ups.delay.shutdown".into(), "60".into()),
                ("ups.delay.start".into(), "0".into()),
                ("ups.test.result".into(), "No test initiated".into()),
            ]),
            cmds: vec!["shutdown.return".into()],
            clients: vec!["127.0.0.1".into()],
            greeting: Some("Network UPS Tools upsd 2.8.1".into()),
            nut_version: Some("2.8.1".into()),
            ..UpsObservation::default()
        });
        let (code, text, err) = execute(
            &[
                "nut",
                "--ascii",
                "--json",
                "--config",
                "/config.toml",
                "doctor",
                "protect.poweroff-wait",
                "hw.delays",
                "hw.online",
                "svc.version",
            ],
            &host,
        );
        assert_eq!(code, 0, "{err}\n{text}");
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schema_version"], 1);
        let checks = json["checks"].as_array().unwrap();
        let version = checks
            .iter()
            .find(|check| check["id"] == "svc.version")
            .unwrap();
        assert_eq!(version["status"], "pass", "{json}");
        let poweroff = checks
            .iter()
            .find(|check| check["id"] == "protect.poweroff-wait")
            .unwrap();
        assert_eq!(poweroff["status"], "warn", "{json}");
        assert!(
            poweroff["evidence"]
                .as_str()
                .unwrap()
                .contains("full-charge runtime + 10m"),
            "{poweroff}"
        );
        assert!(
            poweroff["fix"]
                .as_str()
                .unwrap()
                .contains("shutdown.poweroff_wait"),
            "{poweroff}"
        );
    }
}
