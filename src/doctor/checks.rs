use std::ffi::OsString;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::snapshot::{Snapshot, file_meta, loopback_listeners, parse_version_triple, unit_state};
use super::types::{CatalogEntry, Check, fail, pass, skipped, unknown, warn};
use crate::completion::Shell;
use crate::config::ResolvedConfig;
use crate::drill::DrillKind;
use crate::firmware::audit::{BootCheck, CheckStatus, audit_boot};
use crate::firmware::{
    AttestationFreshness, DmiInfo, efivar_fingerprint, observe_attestation, resolve_profile,
};
use crate::host::Host;
use crate::init::{
    FileMetadataRecord, PENDING_PATH, TransactionError, backup_summary, parse_upsd_users,
    read_state_record,
};
use crate::render::{
    DiffStatus, HOOK_PATH, LIVE_PATHS, OPERATOR_PLACEHOLDER, UPSMON_PLACEHOLDER, diff_live, render,
};
use crate::ui::layout::format_duration;

const INSTALLED_BINARY: &str = "/usr/local/bin/nut";
const LEGACY_HOOK: &str = "/usr/local/sbin/athena-upssched-cmd";
const SENDMAIL: &str = "/usr/sbin/sendmail";
const KILLPOWER: &str = "/etc/killpower";
const BATTERY_AGE_WARN_SECS: u64 = 3 * 365 * 24 * 60 * 60;
const POWEROFF_HEADROOM: Duration = Duration::from_secs(10 * 60);
const EXTRA_OFF_MARGIN: Duration = Duration::from_secs(60);

pub fn run<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    match entry.id {
        "tool.config" => tool_config(entry, snapshot),
        "tool.binary" => tool_binary(entry, host, snapshot),
        "tool.completion" => tool_completion(entry, host),
        "hw.usb" => hw_usb(entry, host, snapshot),
        "hw.data" => hw_data(entry, snapshot),
        "hw.online" => hw_online(entry, snapshot),
        "hw.delays" => hw_delays(entry, snapshot),
        "hw.shutdown-return" => hw_shutdown_return(entry, snapshot),
        "hw.rb" => hw_rb(entry, snapshot),
        "hw.charge" => hw_charge(entry, snapshot),
        "hw.battery-age" => hw_battery_age(entry, snapshot),
        "hw.self-test" => hw_self_test(entry, snapshot),
        "svc.driver" => svc_unit(entry, host, &driver_unit(snapshot), "driver"),
        "svc.server" => svc_unit(entry, host, "nut-server.service", "server"),
        "svc.monitor" => svc_unit(entry, host, "nut-monitor.service", "monitor"),
        "svc.enabled" => svc_enabled(entry, host, snapshot),
        "svc.restarts" => svc_restarts(entry, host, snapshot),
        "svc.listen" => svc_listen(entry, host),
        "svc.client" => svc_client(entry, snapshot),
        "svc.version" => svc_version(entry, snapshot),
        "protect.runtime" => protect_runtime(entry, snapshot),
        "protect.poweroff-wait" => protect_poweroff_wait(entry, host, snapshot),
        "protect.hook" => protect_hook(entry, host),
        "protect.mail" => protect_mail(entry, host),
        "protect.pwrstatd" => protect_pwrstatd(entry, host),
        "protect.killpower" => protect_killpower(entry, host),
        "root.drift" => root_drift(entry, host, snapshot),
        "root.credentials" => root_credentials(entry, host, snapshot),
        "root.placeholders" => root_placeholders(entry, host),
        "root.instcmds" => root_instcmds(entry, host),
        "root.perms" => root_perms(entry, host, snapshot),
        "state.drift" => state_drift(entry, host),
        "state.backup" => state_backup(entry, host),
        "boot.efi-order" | "boot.nofail" | "boot.fstab" | "boot.journal" | "boot.panic"
        | "boot.watchdog" => boot_from_audit(entry, host, snapshot),
        "bios.profile" => bios_profile(entry, host, snapshot),
        "bios.attest" => bios_attest(entry, host, snapshot),
        "proof.blip" => proof_check(entry, DrillKind::Blip, host, snapshot),
        "proof.ac-loss" => proof_check(entry, DrillKind::AcLoss, host, snapshot),
        "proof.cycle" => proof_check(entry, DrillKind::Cycle, host, snapshot),
        "proof.outage" => proof_check(entry, DrillKind::Outage, host, snapshot),
        other => unknown(entry, format!("no implementation for {other}")),
    }
}

fn tool_config(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    if let Some(path) = &snapshot.config_path
        && snapshot.config.is_some()
    {
        return pass(entry, format!("parsed {} as schema 1", path.display()));
    }
    if let Some(error) = &snapshot.config_error {
        if error.contains("missing") {
            return warn(
                entry,
                "no host config; using live discovery",
                "nut config new",
            );
        }
        return fail(
            entry,
            error.clone(),
            "fix the host config, then: nut doctor",
        );
    }
    warn(
        entry,
        "no host config; using live discovery",
        "nut config new",
    )
}

fn tool_binary<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let Some(path) = snapshot.binary.as_ref() else {
        return unknown(entry, "cannot resolve the running nut binary");
    };
    let installed = Path::new(INSTALLED_BINARY);
    let meta = match file_meta(host, if path == installed { path } else { installed }) {
        Ok(value) => value,
        Err(error) => return unknown(entry, error),
    };
    if path != installed {
        return warn(
            entry,
            format!(
                "running unpackaged {} ({})",
                path.display(),
                env!("CARGO_PKG_VERSION")
            ),
            "sudo install -o root -g root -m 0755 target/release/nut /usr/local/bin/nut",
        );
    }
    match meta {
        Some(meta) if meta.uid == 0 && meta.executable() => pass(
            entry,
            format!(
                "{} is root-owned and matches {}",
                path.display(),
                env!("CARGO_PKG_VERSION")
            ),
        ),
        Some(meta) => fail(
            entry,
            format!("{} uid {} mode {:04o}", path.display(), meta.uid, meta.mode),
            "sudo install -o root -g root -m 0755 nut /usr/local/bin/nut",
        ),
        None => fail(
            entry,
            format!("{} is missing", installed.display()),
            "sudo install -o root -g root -m 0755 nut /usr/local/bin/nut",
        ),
    }
}

fn tool_completion<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    let Some(state) = completion_state_path(host) else {
        return warn(
            entry,
            "completion is not installed",
            "nut completion install",
        );
    };
    let Some(bytes) = host.read_file(&state).ok().flatten() else {
        return warn(
            entry,
            "completion is not installed",
            "nut completion install",
        );
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return warn(
            entry,
            "completion manifest is unreadable",
            "nut completion install --force",
        );
    };
    let mut stale = Vec::new();
    let mut present = 0;
    for shell in [Shell::Zsh, Shell::Bash] {
        let Some(path) = value["adapters"][shell.name()]["path"]
            .as_str()
            .map(PathBuf::from)
        else {
            continue;
        };
        present += 1;
        let expected = format!("{:x}", Sha256::digest(shell.adapter().as_bytes()));
        match host.read_file(&path) {
            Ok(Some(bytes)) if format!("{:x}", Sha256::digest(&bytes)) == expected => {}
            _ => stale.push(shell.name().to_owned()),
        }
    }
    if present == 0 {
        return warn(
            entry,
            "completion is not installed",
            "nut completion install",
        );
    }
    if stale.is_empty() {
        pass(entry, format!("{present} adapter(s) match this build"))
    } else {
        warn(
            entry,
            format!("{} adapter(s) are stale", stale.join(", ")),
            "nut completion install --force",
        )
    }
}

fn completion_state_path<H: Host>(host: &H) -> Option<PathBuf> {
    let root = host
        .env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| host.home_dir().map(|home| home.join(".local/state")))?;
    Some(root.join("nut-cli/completion.json"))
}

fn hw_usb<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    if snapshot.dummy_ups {
        return skipped(entry, "USB checks are not applicable for dummy-ups", None);
    }
    if let Some(error) = &snapshot.usb_error {
        return unknown(entry, error.clone());
    }
    match snapshot.usb.as_slice() {
        [] => fail(
            entry,
            "no USB device matches the UPS profile",
            "connect the UPS, then: sudo nut init",
        ),
        [device] => {
            let nut_gid = snapshot.group_gid(host, "nut");
            match (device.node.as_ref(), device.gid, nut_gid) {
                (Some(node), Some(gid), Some(want)) if gid == want => pass(
                    entry,
                    format!(
                        "one {}, device node {} group nut",
                        device.id,
                        node.display()
                    ),
                ),
                (Some(node), Some(gid), Some(_)) => fail(
                    entry,
                    format!("{} group id {gid} is not nut", node.display()),
                    "sudo nut init",
                ),
                (Some(node), _, _) => unknown(
                    entry,
                    format!("{} exists; nut group is unknown", node.display()),
                ),
                _ => fail(
                    entry,
                    format!(
                        "one matching USB {} ({}) has no device node",
                        device.id,
                        device.sysfs.display()
                    ),
                    "sudo nut init",
                ),
            }
        }
        many => fail(
            entry,
            format!("{} matching USB devices", many.len()),
            "disconnect extra UPS devices or pin match.productid",
        ),
    }
}

fn hw_data(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    if let Some(error) = &snapshot.vars_error {
        let lower = error.to_ascii_lowercase();
        if lower.contains("data-stale") {
            return fail(
                entry,
                "UPS data is stale",
                "check nut-driver; then: nut doctor",
            );
        }
        if lower.contains("driver-not-connected") {
            return fail(
                entry,
                "driver is not connected",
                "sudo systemctl restart nut-driver@".to_owned() + &snapshot.ups_name,
            );
        }
        return fail(entry, error.clone(), "check nut-server and the driver");
    }
    if snapshot.vars.is_empty() {
        return fail(
            entry,
            "UPS data is unavailable",
            "check nut-server; then: nut doctor",
        );
    }
    if snapshot
        .vars
        .get("ups.status")
        .is_some_and(|status| super::status_awaits_driver(status))
    {
        return fail(
            entry,
            "upsd is waiting for the driver's first data dump (ups.status WAIT)",
            "check nut-driver; then: nut doctor",
        );
    }
    pass(
        entry,
        format!(
            "{} variables from {}",
            snapshot.vars.len(),
            snapshot.ups_name
        ),
    )
}

fn hw_online(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    if !snapshot.vars.contains_key("ups.status") {
        return fail(
            entry,
            "ups.status is unavailable",
            "check nut-server; then: nut doctor",
        );
    }
    let flags = snapshot.status_flags();
    let safe = flags.iter().any(|flag| flag == "OL")
        && !flags
            .iter()
            .any(|flag| matches!(flag.as_str(), "OB" | "LB" | "FSD"));
    if safe {
        pass(entry, format!("status {}", flags.join(" ")))
    } else {
        fail(
            entry,
            format!("status {} is not safely online", flags.join(" ")),
            "restore mains power, then: nut doctor",
        )
    }
}

fn hw_delays(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    let Some(off) = snapshot.vars.get("ups.delay.shutdown") else {
        return unknown(entry, "ups.delay.shutdown is unavailable");
    };
    let Some(on) = snapshot.vars.get("ups.delay.start") else {
        return unknown(entry, "ups.delay.start is unavailable");
    };
    let (want_off, want_on) = match snapshot.resolved.as_ref() {
        Some(resolved) => (
            resolved.offdelay.value.as_secs().to_string(),
            resolved.ondelay.value.as_secs().to_string(),
        ),
        None => ("60".into(), "0".into()),
    };
    if off == &want_off && on == &want_on {
        pass(
            entry,
            format!("shutdown {off}s, start {on}s (profile {want_off} / {want_on})"),
        )
    } else {
        fail(
            entry,
            format!("shutdown {off}s, start {on}s (profile {want_off} / {want_on})"),
            "sudo nut init",
        )
    }
}

fn hw_shutdown_return(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    if snapshot.dummy_ups {
        return skipped(entry, "dummy-ups does not advertise instant commands", None);
    }
    if let Some(error) = &snapshot.cmds_error {
        return fail(entry, error.clone(), "check nut-server; then: nut doctor");
    }
    if snapshot
        .cmds
        .iter()
        .any(|cmd| cmd.trim() == "shutdown.return")
    {
        pass(entry, "shutdown.return is advertised")
    } else {
        fail(
            entry,
            "shutdown.return is not advertised",
            "confirm the UPS supports outlet restore",
        )
    }
}

fn hw_rb(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    if snapshot.status_flags().iter().any(|flag| flag == "RB") {
        warn(
            entry,
            "replace-battery flag is set",
            "replace the cartridge, then: nut doctor",
        )
    } else if !snapshot.vars.contains_key("ups.status") {
        unknown(entry, "ups.status is unavailable")
    } else {
        pass(entry, "replace-battery flag is clear")
    }
}

fn hw_charge(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    match snapshot.float_var("battery.charge") {
        Some(charge) if charge >= 90.0 => pass(entry, format!("charge {charge:.0}%")),
        Some(charge) => warn(
            entry,
            format!("charge {charge:.0}% is under 90%"),
            "allow the UPS to recharge, then: nut doctor",
        ),
        None => warn(
            entry,
            "battery charge is unavailable",
            "check the driver; then: nut doctor",
        ),
    }
}

fn hw_battery_age(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    let Some(installed) = snapshot
        .config
        .as_ref()
        .and_then(|config| config.battery.as_ref())
        .map(|battery| battery.installed.as_str())
    else {
        return skipped(entry, "battery.installed is not set", None);
    };
    let Ok(date) = chrono::NaiveDate::parse_from_str(installed, "%Y-%m-%d") else {
        return unknown(
            entry,
            format!("battery.installed {installed:?} is not YYYY-MM-DD"),
        );
    };
    let installed_unix = date
        .and_hms_opt(0, 0, 0)
        .and_then(|datetime| datetime.and_utc().timestamp().try_into().ok())
        .unwrap_or(0);
    let now = snapshot
        .now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = now.saturating_sub(installed_unix);
    if age >= BATTERY_AGE_WARN_SECS {
        warn(
            entry,
            format!("battery installed {installed} is over 3 years old"),
            "plan a cartridge replacement",
        )
    } else {
        pass(entry, format!("battery installed {installed}"))
    }
}

fn hw_self_test(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    match snapshot.vars.get("ups.test.result").map(String::as_str) {
        Some(result)
            if result.eq_ignore_ascii_case("ok")
                || result.to_ascii_lowercase().contains("pass") =>
        {
            pass(entry, format!("last self-test: {result}"))
        }
        Some(result) if result.to_ascii_lowercase().contains("fail") => fail(
            entry,
            format!("last self-test: {result}"),
            "replace the cartridge after a failed self-test",
        ),
        Some(result) => warn(
            entry,
            format!("last self-test: {result}"),
            "nut battery test",
        ),
        None => warn(
            entry,
            "no self-test result is published",
            "nut battery test",
        ),
    }
}

fn driver_unit(snapshot: &Snapshot) -> String {
    format!("nut-driver@{}.service", snapshot.ups_name)
}

fn svc_unit<H: Host>(entry: &CatalogEntry, host: &H, unit: &str, label: &str) -> Check {
    let state = unit_state(host, unit);
    if state.active.as_deref() == Some("active") {
        pass(entry, format!("{unit} active"))
    } else {
        fail(
            entry,
            format!(
                "{label} is {}",
                state.active.as_deref().unwrap_or("unavailable")
            ),
            format!("sudo systemctl start {unit}"),
        )
    }
}

fn svc_enabled<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let units = [
        driver_unit(snapshot),
        "nut-server.service".into(),
        "nut-monitor.service".into(),
        "nut.target".into(),
    ];
    let mut missing = Vec::new();
    for unit in &units {
        if unit_state(host, unit).enabled != Some(true) {
            missing.push(unit.as_str());
        }
    }
    if missing.is_empty() {
        pass(entry, "driver, server, monitor, and nut.target are enabled")
    } else {
        fail(
            entry,
            format!("{} not enabled", missing.join(", ")),
            "sudo systemctl enable nut.target nut-server nut-monitor ".to_owned()
                + &driver_unit(snapshot),
        )
    }
}

fn svc_restarts<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let Some(window) = snapshot.observe.filter(|duration| !duration.is_zero()) else {
        return skipped(
            entry,
            "pass -o/--observe SECS to watch driver NRestarts",
            Some("nut doctor --observe 30".to_owned()),
        );
    };
    let unit = driver_unit(snapshot);
    let before = unit_state(host, &unit);
    host.sleep(window);
    let after = unit_state(host, &unit);
    let stable = before.active.as_deref() == Some("active")
        && after.active.as_deref() == Some("active")
        && before.nrestarts.is_some()
        && before.nrestarts == after.nrestarts;
    if stable {
        pass(
            entry,
            format!(
                "driver active, NRestarts {} over {}",
                after.nrestarts.as_deref().unwrap_or("0"),
                format_duration(window)
            ),
        )
    } else {
        fail(
            entry,
            format!(
                "driver {} NRestarts {} → {}",
                after.active.as_deref().unwrap_or("unavailable"),
                before.nrestarts.as_deref().unwrap_or("?"),
                after.nrestarts.as_deref().unwrap_or("?")
            ),
            format!("sudo journalctl -u {unit}"),
        )
    }
}

fn svc_listen<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    match loopback_listeners(host) {
        Ok(listeners) if listeners.is_empty() => fail(
            entry,
            "nothing listens on port 3493",
            "sudo systemctl start nut-server",
        ),
        Ok(listeners) if listeners.iter().all(IpAddr::is_loopback) => pass(
            entry,
            format!(
                "port 3493 listens on {}",
                listeners
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Ok(listeners) => fail(
            entry,
            format!(
                "port 3493 also listens on {}",
                listeners
                    .iter()
                    .filter(|ip| !ip.is_loopback())
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "bind upsd to localhost only, then: sudo nut init",
        ),
        Err(error) => unknown(entry, error),
    }
}

fn svc_client(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    if let Some(error) = &snapshot.clients_error {
        return fail(entry, error.clone(), "check nut-monitor; then: nut doctor");
    }
    let loopback = snapshot.clients.iter().any(|client| {
        client
            .trim()
            .trim_matches(|ch| ch == '[' || ch == ']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
            || client.contains("127.0.0.1")
            || client.contains("::1")
    });
    if loopback {
        pass(entry, "a loopback client is logged in")
    } else {
        fail(
            entry,
            "no loopback monitor client is logged in",
            "sudo systemctl restart nut-monitor",
        )
    }
}

fn svc_version(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    let Some(version) = snapshot.nut_version.as_deref() else {
        return unknown(
            entry,
            snapshot
                .greeting
                .clone()
                .unwrap_or_else(|| "upsd did not publish a version".to_owned()),
        );
    };
    let Some(triple) = parse_version_triple(version) else {
        return unknown(entry, format!("cannot parse NUT version {version}"));
    };
    if triple < (2, 8, 1) {
        return fail(
            entry,
            format!("NUT {version} is older than 2.8.1"),
            "install nut-client and nut-server 2.8.1 or newer",
        );
    }
    if triple >= (2, 8, 3) {
        return warn(
            entry,
            format!("NUT {version} changed CyberPower default delays; review offdelay/ondelay"),
            "nut config show",
        );
    }
    pass(entry, format!("NUT {version}"))
}

fn protect_runtime(entry: &CatalogEntry, snapshot: &Snapshot) -> Check {
    let Some(charge) = snapshot.float_var("battery.charge") else {
        return warn(
            entry,
            "battery charge/runtime measurements are unavailable",
            "check the driver; then: nut doctor",
        );
    };
    let Some(runtime) = snapshot.float_var("battery.runtime") else {
        return warn(
            entry,
            "battery charge/runtime measurements are unavailable",
            "check the driver; then: nut doctor",
        );
    };
    let required = required_runtime(snapshot);
    if charge < 95.0 {
        return pass(
            entry,
            format!("charge {charge:.0}% is below 95%; {required}s rule not applied"),
        );
    }
    if runtime >= required as f64 {
        pass(
            entry,
            format!(
                "runtime {} fits the {required}s shutdown budget",
                format_duration(Duration::from_secs(runtime as u64))
            ),
        )
    } else {
        warn(
            entry,
            format!(
                "runtime {} is under the {required}s shutdown budget at {charge:.0}%",
                format_duration(Duration::from_secs(runtime as u64))
            ),
            "reduce load or raise shutdown.early_after after measuring runtime",
        )
    }
}

fn required_runtime(snapshot: &Snapshot) -> u64 {
    let (early, budget, offdelay) = match snapshot.resolved.as_ref() {
        Some(resolved) => (
            resolved.shutdown.early_after.as_secs(),
            resolved.shutdown.shutdown_budget.as_secs(),
            resolved.offdelay.value.as_secs(),
        ),
        None => (180, 120, 60),
    };
    early + budget + offdelay + EXTRA_OFF_MARGIN.as_secs()
}

fn protect_poweroff_wait<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let Some(runtime) = snapshot.float_var("battery.runtime") else {
        return unknown(entry, "battery.runtime is unavailable");
    };
    let charge = snapshot.float_var("battery.charge");
    if charge.is_some_and(|value| value < 95.0) {
        return skipped(
            entry,
            format!(
                "charge {:.0}% is below 95%; POWEROFF_WAIT uses full-charge runtime",
                charge.unwrap()
            ),
            None,
        );
    }
    let required = Duration::from_secs(runtime as u64) + POWEROFF_HEADROOM;
    let live = live_poweroff_wait(host, snapshot.resolved.as_ref());
    match live {
        Some(actual) if actual >= required => pass(
            entry,
            format!(
                "{} covers full-charge runtime + 10m ({})",
                format_duration(actual),
                format_duration(required)
            ),
        ),
        Some(actual) => warn(
            entry,
            format!(
                "{} is under full-charge runtime + 10m ({})",
                format_duration(actual),
                format_duration(required)
            ),
            format!(
                "set shutdown.poweroff_wait = \"{}\", then: sudo nut init",
                suggested_poweroff_wait(required)
            ),
        ),
        None => warn(
            entry,
            format!(
                "full-charge runtime + 10m is {}; live POWEROFF_WAIT needs root",
                format_duration(required)
            ),
            "sudo nut doctor",
        ),
    }
}

fn live_poweroff_wait<H: Host>(host: &H, resolved: Option<&ResolvedConfig>) -> Option<Duration> {
    if let Ok(Some(bytes)) = host.read_file(Path::new("/etc/nut/nut.conf")) {
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some(value) = line.strip_prefix("POWEROFF_WAIT") {
                let value = value
                    .trim()
                    .trim_start_matches('=')
                    .trim()
                    .trim_matches('"');
                if let Ok(parsed) = crate::duration::DurationValue::parse(value) {
                    return Some(parsed.as_duration());
                }
            }
        }
    }
    resolved.map(|resolved| resolved.shutdown.poweroff_wait.as_duration())
}

fn suggested_poweroff_wait(required: Duration) -> String {
    let minutes = required.as_secs().div_ceil(60);
    let raised = minutes.max(45);
    format!("{raised}m")
}

fn protect_hook<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    for path in [HOOK_PATH, LEGACY_HOOK] {
        match file_meta(host, Path::new(path)) {
            Ok(Some(meta)) if meta.executable() => {
                if path == HOOK_PATH {
                    return pass(entry, format!("{path} is executable"));
                }
                return warn(
                    entry,
                    format!("legacy hook {path} is still in place"),
                    "sudo nut init",
                );
            }
            Ok(Some(_)) => {
                return fail(entry, format!("{path} is not executable"), "sudo nut init");
            }
            Ok(None) => {}
            Err(error) => return unknown(entry, error),
        }
    }
    fail(entry, "no upssched hook is installed", "sudo nut init")
}

fn protect_mail<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    match host.path_exists(Path::new(SENDMAIL)) {
        Ok(true) => pass(entry, format!("{SENDMAIL} exists")),
        Ok(false) => fail(
            entry,
            format!("{SENDMAIL} is missing"),
            "install a sendmail provider",
        ),
        Err(error) => unknown(entry, error.to_string()),
    }
}

fn protect_pwrstatd<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    match host.run_program(
        "pgrep".as_ref(),
        &[OsString::from("-x"), OsString::from("pwrstatd")],
    ) {
        Ok(output) if output.status == Some(1) => pass(entry, "pwrstatd is not running"),
        Ok(output) if output.status == Some(0) => fail(
            entry,
            "PowerPanel pwrstatd is running",
            "stop pwrstatd before using NUT",
        ),
        Ok(output) => unknown(
            entry,
            format!("pgrep exited {}", output.status.unwrap_or(-1)),
        ),
        Err(error) => unknown(entry, error.to_string()),
    }
}

fn protect_killpower<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    match host.path_exists(Path::new(KILLPOWER)) {
        Ok(false) => pass(entry, "/etc/killpower is absent"),
        Ok(true) => fail(
            entry,
            "/etc/killpower is present; doctor will not clear it",
            "inspect /etc/killpower, then remove it by hand if the UPS is on line",
        ),
        Err(error) => unknown(entry, error.to_string()),
    }
}

fn root_drift<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let Some(resolved) = snapshot.resolved.as_ref() else {
        return skipped(
            entry,
            "host config is required for drift",
            Some("nut config new".into()),
        );
    };
    let files = match render(resolved) {
        Ok(files) => files,
        Err(error) => {
            return fail(
                entry,
                error.to_string(),
                "fix the host config, then: sudo nut init",
            );
        }
    };
    match diff_live(host, &files) {
        Ok(report) => {
            let mut differs = Vec::new();
            let mut missing = Vec::new();
            for file in &report.files {
                match &file.status {
                    DiffStatus::Identical | DiffStatus::NeedsRoot => {}
                    DiffStatus::Missing => missing.push(file.live_path),
                    DiffStatus::Differs { .. } => differs.push(file.live_path),
                }
            }
            if differs.is_empty() && missing.is_empty() {
                pass(entry, "rendered files match live NUT config")
            } else {
                fail(
                    entry,
                    format!(
                        "drift: {}",
                        differs
                            .into_iter()
                            .chain(missing)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    "sudo nut init",
                )
            }
        }
        Err(error) => unknown(entry, error.to_string()),
    }
}

fn root_credentials<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let users = read_text(host, Path::new("/etc/nut/upsd.users"));
    let monitor = read_text(host, Path::new("/etc/nut/upsmon.conf"));
    match (users, monitor) {
        (Ok(Some(users)), Ok(Some(monitor))) => {
            if credentials_agree(&users, &monitor, &snapshot.ups_name) {
                pass(entry, "upsd and upsmon credentials agree")
            } else {
                fail(
                    entry,
                    "upsd and upsmon credentials disagree",
                    "sudo nut init",
                )
            }
        }
        (Err(error), _) | (_, Err(error)) => unknown(entry, error),
        _ => fail(
            entry,
            "upsd.users or upsmon.conf is missing",
            "sudo nut init",
        ),
    }
}

fn credentials_agree(users: &str, monitor: &str, ups: &str) -> bool {
    let mut section = None;
    let mut password = None;
    for line in users.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = Some(trimmed.trim_matches(['[', ']']).to_owned());
            continue;
        }
        if section.as_deref() == Some("upsmon")
            && let Some(value) = trimmed.strip_prefix("password")
        {
            let value = value
                .trim()
                .trim_start_matches('=')
                .trim()
                .trim_matches('"');
            if !value.is_empty() {
                password = Some(value.to_owned());
            }
        }
    }
    let mut monitors = Vec::new();
    for line in monitor.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || !trimmed.starts_with("MONITOR") {
            continue;
        }
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.len() >= 6 {
            monitors.push((tokens[1], tokens[2], tokens[3], tokens[4], tokens[5]));
        }
    }
    matches!(
        (password.as_deref(), monitors.as_slice()),
        (Some(password), [(target, "1", "upsmon", secret, "primary")])
            if (*target == format!("{ups}@localhost") || *target == ups)
                && !password.is_empty()
                && *secret == password
    )
}

fn root_placeholders<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    for (_, path) in LIVE_PATHS.iter().take(5) {
        match read_text(host, Path::new(path)) {
            Ok(Some(text))
                if text.contains(UPSMON_PLACEHOLDER) || text.contains(OPERATOR_PLACEHOLDER) =>
            {
                return fail(
                    entry,
                    format!("{path} still contains a password placeholder"),
                    "sudo nut init",
                );
            }
            Ok(_) => {}
            Err(error) => return unknown(entry, error),
        }
    }
    pass(entry, "no password placeholders remain")
}

fn root_instcmds<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    match read_text(host, Path::new("/etc/nut/upsd.users")) {
        Ok(Some(text)) => {
            let all_granted = text.lines().any(|line| {
                let line = line.trim().to_ascii_lowercase();
                line.starts_with("instcmds") && line.contains("all")
            });
            if all_granted {
                fail(
                    entry,
                    "instcmds = ALL is granted",
                    "replace ALL with the operator allowlist, then: sudo nut init",
                )
            } else {
                let parsed = parse_upsd_users(&text);
                let expected = [
                    "beeper.disable",
                    "beeper.enable",
                    "beeper.mute",
                    "test.battery.start.quick",
                    "test.battery.stop",
                ];
                let section = parsed.sections.get("nut-operator");
                let Some(section) = section else {
                    return fail(
                        entry,
                        "nut-operator account is missing",
                        "restore the exact operator allowlist with: sudo nut init",
                    );
                };
                let allowed_keys = ["password", "instcmds"];
                let has_extra_key = section
                    .keys()
                    .any(|key| !allowed_keys.contains(&key.as_str()));
                let password_count = section.get("password").map_or(0, Vec::len);
                let mut grants = parsed.operator_instcmds.clone();
                grants.sort();
                let mut expected = expected.map(str::to_owned).to_vec();
                expected.sort();
                let exact = !parsed.operator_malformed
                    && parsed.operator.is_some()
                    && password_count == 1
                    && !parsed.operator_has_actions_set
                    && !parsed.operator_has_all
                    && !has_extra_key
                    && grants == expected;
                if exact {
                    pass(entry, "nut-operator has exactly the five approved commands")
                } else {
                    fail(
                        entry,
                        "nut-operator grants or account directives differ from the approved allowlist",
                        "restore the exact operator allowlist with: sudo nut init",
                    )
                }
            }
        }
        Ok(None) => fail(entry, "upsd.users is missing", "sudo nut init"),
        Err(error) => unknown(entry, error),
    }
}

fn root_perms<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let nut_gid = snapshot.group_gid(host, "nut");
    let operators_gid = snapshot.group_gid(host, "nut-operators");
    let mut problems = Vec::new();
    for (name, path, mode, group) in [
        ("nut.conf", "/etc/nut/nut.conf", 0o640, "nut"),
        ("ups.conf", "/etc/nut/ups.conf", 0o640, "nut"),
        ("upssched.conf", "/etc/nut/upssched.conf", 0o640, "nut"),
        ("upsd.users", "/etc/nut/upsd.users", 0o640, "nut"),
        ("upsmon.conf", "/etc/nut/upsmon.conf", 0o640, "nut"),
        ("hook", HOOK_PATH, 0o755, "root"),
        ("host config", "/etc/nut-cli/nut.toml", 0o644, "root"),
        (
            "operator secret",
            "/etc/nut-cli/operator.secret",
            0o640,
            "nut-operators",
        ),
    ] {
        match file_meta(host, Path::new(path)) {
            Ok(None) => problems.push(format!("{name} missing")),
            Ok(Some(meta)) => {
                let want_gid = match group {
                    "nut" => nut_gid,
                    "nut-operators" => operators_gid,
                    _ => Some(0),
                };
                let gid_ok = match (group, want_gid, meta.gid) {
                    (_, Some(want), gid) => gid == want,
                    _ => false,
                };
                if !(meta.is_file && meta.uid == 0 && gid_ok && meta.mode == mode) {
                    problems.push(format!(
                        "{path} uid {} gid {} mode {:04o}",
                        meta.uid, meta.gid, meta.mode
                    ));
                }
            }
            Err(error) => return unknown(entry, error),
        }
    }
    if problems.is_empty() {
        pass(entry, "NUT files are root-owned with the expected modes")
    } else {
        fail(entry, problems.join("; "), "sudo nut init")
    }
}

fn state_drift<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    if host.euid() != 0 {
        return skipped(entry, "state checks require root", None);
    }
    match host.path_exists(Path::new(PENDING_PATH)) {
        Ok(true) => {
            return fail(
                entry,
                "pending init transaction requires recovery before state can be trusted",
                "resume the reviewed plan with: sudo nut init --yes --expect-plan HASH",
            );
        }
        Err(_) => return unknown(entry, "pending transaction state is unreadable"),
        Ok(false) => {}
    }
    let state = match read_state_record(host) {
        Ok(Some(state)) => state,
        Ok(None) => return skipped(entry, "no previous init state record", None),
        Err(TransactionError::Host(error)) if error.is_permission_denied() => {
            return unknown(entry, "root state record is unreadable");
        }
        Err(TransactionError::Host(_)) => return unknown(entry, "root state record is unreadable"),
        Err(_) => {
            return fail(
                entry,
                "root state record is corrupt or unsafe",
                "inspect the protected state and backup before running `sudo nut init`",
            );
        }
    };

    let config_path = Path::new("/etc/nut-cli/nut.toml");
    let config = match host.read_protected(config_path) {
        Ok(Some(bytes)) => Some(bytes),
        Ok(None) => {
            return fail(
                entry,
                "installed host config is missing",
                "restore the selected backup with `sudo nut rollback`",
            );
        }
        Err(_) => return unknown(entry, "installed host config is unreadable"),
    };
    let mut drift = Vec::new();
    if config
        .as_deref()
        .is_some_and(|bytes| format!("{:x}", Sha256::digest(bytes)) != state.config_hash)
    {
        drift.push(config_path.display().to_string());
    }
    for (path, expected) in &state.outputs {
        match (host.read_protected(path), host.metadata(path)) {
            (Ok(Some(bytes)), Ok(Some(metadata)))
                if format!("{:x}", Sha256::digest(&bytes)) == expected.sha256
                    && FileMetadataRecord::from(&metadata) == expected.metadata
                    && metadata.is_file
                    && !metadata.is_dir => {}
            (Err(_), _) | (_, Err(_)) => {
                return unknown(
                    entry,
                    format!("recorded output {} is unreadable", path.display()),
                );
            }
            _ => drift.push(path.display().to_string()),
        }
    }
    for path in &state.absent_outputs {
        match (host.read_protected(path), host.metadata(path)) {
            (Ok(None), Ok(None)) => {}
            (Err(_), _) | (_, Err(_)) => {
                return unknown(
                    entry,
                    format!("recorded absent output {} is unreadable", path.display()),
                );
            }
            _ => drift.push(path.display().to_string()),
        }
    }
    drift.sort();
    drift.dedup();
    if drift.is_empty() {
        pass(
            entry,
            format!(
                "state {} matches its recorded config and outputs",
                state.backup_reference
            ),
        )
    } else {
        fail(
            entry,
            format!("applied-state drift: {}", drift.join(", ")),
            "review current files, then restore with `sudo nut rollback` or reapply a reviewed init plan",
        )
    }
}

fn state_backup<H: Host>(entry: &CatalogEntry, host: &H) -> Check {
    if host.euid() != 0 {
        return skipped(entry, "backup checks require root", None);
    }
    match host.path_exists(Path::new(PENDING_PATH)) {
        Ok(true) => {
            return fail(
                entry,
                "pending init transaction has not finalized its backup reference",
                "resume the reviewed plan with: sudo nut init --yes --expect-plan HASH",
            );
        }
        Err(_) => return unknown(entry, "pending transaction state is unreadable"),
        Ok(false) => {}
    }
    let state = match read_state_record(host) {
        Ok(Some(state)) => state,
        Ok(None) => return skipped(entry, "no previous init state record", None),
        Err(TransactionError::Host(error)) if error.is_permission_denied() => {
            return unknown(entry, "root state record is unreadable");
        }
        Err(TransactionError::Host(_)) => return unknown(entry, "root state record is unreadable"),
        Err(_) => {
            return fail(
                entry,
                "root state record is corrupt or unsafe",
                "inspect the protected state and backup before rollback",
            );
        }
    };
    match backup_summary(host, &state.backup_reference) {
        Ok(summary) => pass(
            entry,
            format!(
                "backup {} contains {} validated file preimages",
                summary.stamp,
                summary.files.len()
            ),
        ),
        Err(TransactionError::Host(error)) if error.is_permission_denied() => {
            unknown(entry, "applied-state backup is unreadable")
        }
        Err(_) => fail(
            entry,
            format!(
                "backup {} is missing, corrupt, or unsafe",
                state.backup_reference
            ),
            "do not delete backup data; inspect it before rollback",
        ),
    }
}

fn boot_from_audit<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let checks = audit_boot(host, snapshot.config.as_ref());
    let Some(found) = checks.iter().find(|check| check.id == entry.id) else {
        return unknown(entry, "boot audit did not produce this check");
    };
    from_boot(entry, found)
}

fn from_boot(entry: &CatalogEntry, check: &BootCheck) -> Check {
    match check.status {
        CheckStatus::Pass => pass(entry, check.evidence.clone()),
        CheckStatus::Warn => warn(
            entry,
            check.evidence.clone(),
            check.fix.clone().unwrap_or_else(|| "nut bios".into()),
        ),
        CheckStatus::Fail => fail(
            entry,
            check.evidence.clone(),
            check.fix.clone().unwrap_or_else(|| "nut bios".into()),
        ),
        CheckStatus::Unknown => unknown(entry, check.evidence.clone()),
    }
}

fn bios_profile<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let dmi = match DmiInfo::read(host) {
        Ok(dmi) => dmi,
        Err(error) => return unknown(entry, error.to_string()),
    };
    let profile = resolve_profile(&snapshot.catalog, snapshot.config.as_ref(), &dmi);
    if profile.id.as_str() == "generic" {
        warn(
            entry,
            format!(
                "generic profile for {} {}",
                dmi.board_vendor, dmi.board_name
            ),
            "nut bios profile new",
        )
    } else {
        pass(
            entry,
            format!(
                "{} matches {} {}",
                profile.id, dmi.board_vendor, dmi.board_name
            ),
        )
    }
}

fn bios_attest<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    let dmi = match DmiInfo::read(host) {
        Ok(dmi) => dmi,
        Err(error) => return unknown(entry, error.to_string()),
    };
    let profile = resolve_profile(&snapshot.catalog, snapshot.config.as_ref(), &dmi);
    let fingerprint = efivar_fingerprint(host, &profile.fingerprint_efivars)
        .ok()
        .flatten();
    let (_, state) = match observe_attestation(host, &dmi, fingerprint.as_deref()) {
        Ok(observed) => observed,
        Err(error) => return unknown(entry, error.to_string()),
    };
    match state {
        AttestationFreshness::NeedsRoot => skipped(
            entry,
            "BIOS evidence is in the root-only state directory",
            Some("sudo nut doctor".into()),
        ),
        AttestationFreshness::Fresh => pass(
            entry,
            format!(
                "attestation is fresh for BIOS {}",
                dmi.bios_version.as_deref().unwrap_or("unknown")
            ),
        ),
        AttestationFreshness::Pending => {
            warn(entry, "BIOS attestation is pending", "sudo nut bios attest")
        }
        AttestationFreshness::Missing => warn(
            entry,
            "BIOS settings are not attested",
            "nut bios guide, then: sudo nut bios attest",
        ),
        AttestationFreshness::Stale { reason } => warn(entry, reason, "sudo nut bios attest"),
    }
}

fn proof_check<H: Host>(
    entry: &CatalogEntry,
    kind: DrillKind,
    host: &H,
    snapshot: &Snapshot,
) -> Check {
    crate::drill::proof_check(
        entry,
        kind,
        host,
        snapshot.nut_version.as_deref(),
        snapshot.config_path.as_deref(),
        snapshot.config.as_ref(),
    )
}

fn read_text<H: Host>(host: &H, path: &Path) -> Result<Option<String>, String> {
    match host.read_file(path) {
        Ok(Some(bytes)) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Ok(None) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}
