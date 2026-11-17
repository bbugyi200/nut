use std::ffi::OsString;
use std::path::Path;

use serde::Serialize;

use crate::config::HostConfig;
use crate::host::Host;

const LINUX_BOOT_MARKERS: &[&str] = &[
    "debian",
    "ubuntu",
    "linux",
    "grub",
    "systemd-boot",
    "fedora",
    "arch",
    "gentoo",
    "nixos",
    "rhel",
    "centos",
    "opensuse",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BootCheck {
    pub id: String,
    pub status: CheckStatus,
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

pub fn audit_boot<H: Host>(host: &H, config: Option<&HostConfig>) -> Vec<BootCheck> {
    vec![
        efi_boot_order(host),
        nofail_mounts(host, config),
        findmnt_verify(host),
        journal_persistent(host),
        kernel_panic(host),
        watchdog(host),
    ]
}

fn efi_boot_order<H: Host>(host: &H) -> BootCheck {
    let output = match host.run_program("efibootmgr".as_ref(), &[]) {
        Ok(output) => output,
        Err(error) => {
            return unknown("boot.efi-order", &format!("cannot run efibootmgr: {error}"));
        }
    };
    if output.status != Some(0) {
        return unknown(
            "boot.efi-order",
            "efibootmgr is not available; install efibootmgr to audit the EFI boot order",
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(order) = text
        .lines()
        .find_map(|line| line.strip_prefix("BootOrder:"))
    else {
        return unknown(
            "boot.efi-order",
            "efibootmgr did not print a BootOrder line",
        );
    };
    let Some(first) = order
        .split(',')
        .next()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return fail(
            "boot.efi-order",
            "EFI BootOrder is empty",
            Some("put the Linux boot entry first with efibootmgr -o"),
        );
    };
    let needle = format!("Boot{first}");
    let description = text
        .lines()
        .find(|line| line.starts_with(&needle))
        .map(str::trim)
        .unwrap_or(first);
    let lower = description.to_ascii_lowercase();
    if LINUX_BOOT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        pass("boot.efi-order", &format!("{description} is first"))
    } else {
        fail(
            "boot.efi-order",
            &format!("{description} is first; a Linux entry should lead the EFI boot order"),
            Some("put the Linux boot entry first with efibootmgr -o"),
        )
    }
}

fn nofail_mounts<H: Host>(host: &H, config: Option<&HostConfig>) -> BootCheck {
    let Some(config) = config else {
        return pass("boot.nofail", "no host config mounts are listed");
    };
    if config.boot.nofail_mounts.is_empty() {
        return pass("boot.nofail", "no host config mounts are listed");
    }
    let Some(fstab) = crate::firmware::read_trimmed(host, Path::new("/etc/fstab"))
        .ok()
        .flatten()
    else {
        return unknown("boot.nofail", "/etc/fstab is unreadable");
    };
    let mut missing = Vec::new();
    let mut lacking = Vec::new();
    for mount in &config.boot.nofail_mounts {
        let wanted = mount.to_string_lossy();
        match fstab_options(&fstab, wanted.as_ref()) {
            Some(options) if has_option(&options, "nofail") => {}
            Some(_) => lacking.push(wanted.into_owned()),
            None => missing.push(wanted.into_owned()),
        }
    }
    if lacking.is_empty() && missing.is_empty() {
        return pass(
            "boot.nofail",
            &format!(
                "{} listed mount(s) carry nofail",
                config.boot.nofail_mounts.len()
            ),
        );
    }
    let mut evidence = Vec::new();
    if !lacking.is_empty() {
        evidence.push(format!("{} lack nofail", lacking.join(", ")));
    }
    if !missing.is_empty() {
        evidence.push(format!("{} missing from fstab", missing.join(", ")));
    }
    fail(
        "boot.nofail",
        &evidence.join("; "),
        Some("add nofail to those fstab lines, then: findmnt --verify"),
    )
}

fn findmnt_verify<H: Host>(host: &H) -> BootCheck {
    let output = match host.run_program("findmnt".as_ref(), &[OsString::from("--verify")]) {
        Ok(output) => output,
        Err(error) => return unknown("boot.fstab", &format!("cannot run findmnt: {error}")),
    };
    match output.status {
        Some(0) => pass("boot.fstab", "findmnt --verify passed"),
        Some(127) => unknown("boot.fstab", "findmnt is not available"),
        Some(code) => fail(
            "boot.fstab",
            &format!("findmnt --verify exited {code}"),
            Some("run findmnt --verify and fix the reported fstab lines"),
        ),
        None => unknown("boot.fstab", "findmnt --verify did not return a status"),
    }
}

pub fn journal_persistent<H: Host>(host: &H) -> BootCheck {
    let journal_dir = Path::new("/var/log/journal");
    if host.path_exists(journal_dir).unwrap_or(false) {
        return pass("boot.journal", "/var/log/journal is present");
    }
    let conf = crate::firmware::read_trimmed(host, Path::new("/etc/systemd/journald.conf"))
        .ok()
        .flatten()
        .unwrap_or_default();
    if conf.lines().any(|line| {
        let line = line.trim();
        line.eq_ignore_ascii_case("Storage=persistent") || line.eq_ignore_ascii_case("Storage=auto")
    }) {
        return warn(
            "boot.journal",
            "journald Storage is persistent but /var/log/journal is missing",
            Some(
                "sudo mkdir -p /var/log/journal && sudo systemd-tmpfiles --create --prefix /var/log/journal",
            ),
        );
    }
    fail(
        "boot.journal",
        "journald is not persistent; outage stories vanish across reboot",
        Some("set Storage=persistent in /etc/systemd/journald.conf"),
    )
}

fn kernel_panic<H: Host>(host: &H) -> BootCheck {
    match crate::firmware::read_trimmed(host, Path::new("/proc/sys/kernel/panic")) {
        Ok(Some(value)) if value != "0" => pass("boot.panic", &format!("kernel.panic={value}")),
        Ok(Some(_)) => warn(
            "boot.panic",
            "kernel.panic=0; a hang will not reboot",
            Some("set kernel.panic=10, for example via /etc/sysctl.d"),
        ),
        Ok(None) => unknown("boot.panic", "/proc/sys/kernel/panic is unreadable"),
        Err(error) => unknown("boot.panic", &error.to_string()),
    }
}

fn watchdog<H: Host>(host: &H) -> BootCheck {
    let device = host
        .path_exists(Path::new("/dev/watchdog0"))
        .unwrap_or(false)
        || host
            .path_exists(Path::new("/dev/watchdog"))
            .unwrap_or(false);
    let runtime = systemd_runtime_watchdog(host);
    match (device, runtime) {
        (true, Some(secs)) if secs > 0 => pass(
            "boot.watchdog",
            &format!("watchdog device present, RuntimeWatchdogSec={secs}s"),
        ),
        (true, _) => warn(
            "boot.watchdog",
            "watchdog device present but RuntimeWatchdogSec is not set",
            Some("set RuntimeWatchdogSec=60 in /etc/systemd/system.conf"),
        ),
        (false, Some(secs)) if secs > 0 => warn(
            "boot.watchdog",
            &format!("RuntimeWatchdogSec={secs}s but no /dev/watchdog device"),
            None,
        ),
        _ => warn(
            "boot.watchdog",
            "no hardware watchdog is active",
            Some("enable RuntimeWatchdogSec=60 if the board has a watchdog"),
        ),
    }
}

fn systemd_runtime_watchdog<H: Host>(host: &H) -> Option<u64> {
    let conf = crate::firmware::read_trimmed(host, Path::new("/etc/systemd/system.conf"))
        .ok()
        .flatten()?;
    for line in conf.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("RuntimeWatchdogSec=") {
            return parse_watchdog_secs(value);
        }
    }
    None
}

fn parse_watchdog_secs(value: &str) -> Option<u64> {
    let value = value.trim();
    if value == "off" || value == "0" {
        return Some(0);
    }
    let (digits, unit) = value.split_at(
        value
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(value.len()),
    );
    let number = digits.parse::<u64>().ok()?;
    let multiplier = match unit {
        "" | "s" => 1,
        "min" | "m" => 60,
        "h" => 3600,
        _ => return None,
    };
    number.checked_mul(multiplier)
}

fn fstab_options(fstab: &str, mountpoint: &str) -> Option<String> {
    for line in fstab.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 4 && fields[1] == mountpoint {
            return Some(fields[3].to_owned());
        }
    }
    None
}

fn has_option(options: &str, wanted: &str) -> bool {
    options.split(',').any(|option| option == wanted)
}

fn pass(id: &str, evidence: &str) -> BootCheck {
    BootCheck {
        id: id.to_owned(),
        status: CheckStatus::Pass,
        evidence: evidence.to_owned(),
        fix: None,
    }
}

fn warn(id: &str, evidence: &str, fix: Option<&str>) -> BootCheck {
    BootCheck {
        id: id.to_owned(),
        status: CheckStatus::Warn,
        evidence: evidence.to_owned(),
        fix: fix.map(ToOwned::to_owned),
    }
}

fn fail(id: &str, evidence: &str, fix: Option<&str>) -> BootCheck {
    BootCheck {
        id: id.to_owned(),
        status: CheckStatus::Fail,
        evidence: evidence.to_owned(),
        fix: fix.map(ToOwned::to_owned),
    }
}

fn unknown(id: &str, evidence: &str) -> BootCheck {
    BootCheck {
        id: id.to_owned(),
        status: CheckStatus::Unknown,
        evidence: evidence.to_owned(),
        fix: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostConfig;
    use crate::host::{FakeHost, ProcessOutput};

    fn config_with_mount(path: &str) -> HostConfig {
        HostConfig::parse(&format!(
            r#"
schema = 1
[host]
name = "athena"
[ups.cp1500]
profile = "cyberpower-cp1500pfclcd"
port = "auto"
[shutdown]
notify_after = "30s"
early_after = "3m"
poweroff_wait = "30m"
shutdown_budget = "2m"
[notify]
from = "root@example.invalid"
to = "ups@example.invalid"
commbad_after = "60s"
[boot]
nofail_mounts = ["{path}"]
"#
        ))
        .unwrap()
    }

    #[test]
    fn boot_audit_checks_linux_first_nofail_journal_and_advisories() {
        let config = config_with_mount("/mnt/poseidon");
        let host = FakeHost::default()
            .with_program(
                "efibootmgr",
                ProcessOutput {
                    status: Some(0),
                    stdout: b"BootCurrent: 0002\nBootOrder: 0002,0001\nBoot0002* debian\nBoot0001* Windows Boot Manager\n".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "findmnt",
                ["--verify"],
                ProcessOutput {
                    status: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            )
            .with_file("/etc/fstab", b"UUID=abc /mnt/poseidon ext4 defaults,nofail 0 2\n")
            .with_dir("/var/log/journal")
            .with_file("/proc/sys/kernel/panic", b"0\n")
            .with_file("/dev/watchdog0", b"");
        let checks = audit_boot(&host, Some(&config));
        assert_eq!(
            checks
                .iter()
                .map(|check| check.id.as_str())
                .collect::<Vec<_>>(),
            [
                "boot.efi-order",
                "boot.nofail",
                "boot.fstab",
                "boot.journal",
                "boot.panic",
                "boot.watchdog"
            ]
        );
        assert_eq!(checks[0].status, CheckStatus::Pass);
        assert_eq!(checks[1].status, CheckStatus::Pass);
        assert_eq!(checks[2].status, CheckStatus::Pass);
        assert_eq!(checks[3].status, CheckStatus::Pass);
        assert_eq!(checks[4].status, CheckStatus::Warn);
        assert_eq!(checks[5].status, CheckStatus::Warn);
    }
}
