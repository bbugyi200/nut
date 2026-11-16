use std::panic::{AssertUnwindSafe, catch_unwind};

use super::checks;
use super::snapshot::Snapshot;
use super::types::{CatalogEntry, Check, Section, Status, skipped, unknown};
use crate::host::Host;

pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "tool.config",
        section: Section::Tool,
        title: "Host configuration parses",
        docs: "The selected host config exists and parses as schema 1. Discovery still runs when the file is missing.",
        needs_root: false,
    },
    CatalogEntry {
        id: "tool.binary",
        section: Section::Tool,
        title: "Installed binary is root-owned",
        docs: "The running `nut` binary should be the root-owned `/usr/local/bin/nut` that matches this version.",
        needs_root: false,
    },
    CatalogEntry {
        id: "tool.completion",
        section: Section::Tool,
        title: "Shell completion is current",
        docs: "Installed bash/zsh adapters match this build. Missing adapters are a warning, not a failure.",
        needs_root: false,
    },
    CatalogEntry {
        id: "hw.usb",
        section: Section::Hardware,
        title: "Exactly one matching USB UPS",
        docs: "Sysfs lists exactly one USB device matching the UPS profile, and its `/dev/bus/usb` node is group `nut`. Skipped for dummy-ups.",
        needs_root: false,
    },
    CatalogEntry {
        id: "hw.data",
        section: Section::Hardware,
        title: "UPS data is fresh",
        docs: "upsd answers LIST VAR without DATA-STALE or DRIVER-NOT-CONNECTED.",
        needs_root: false,
    },
    CatalogEntry {
        id: "hw.online",
        section: Section::Hardware,
        title: "UPS is safely on line",
        docs: "Status includes OL and none of OB, LB, or FSD.",
        needs_root: false,
    },
    CatalogEntry {
        id: "hw.delays",
        section: Section::Hardware,
        title: "Shutdown and start delays match the profile",
        docs: "Live `ups.delay.shutdown` and `ups.delay.start` match the resolved profile (60/0 on athena).",
        needs_root: false,
    },
    CatalogEntry {
        id: "hw.shutdown-return",
        section: Section::Hardware,
        title: "shutdown.return is advertised",
        docs: "LIST CMD includes `shutdown.return` so the UPS can restore outlets after a clean halt.",
        needs_root: false,
    },
    CatalogEntry {
        id: "hw.rb",
        section: Section::Hardware,
        title: "Replace-battery flag is clear",
        docs: "The RB status flag is a warning. Doctor never starts a battery test.",
        needs_root: false,
    },
    CatalogEntry {
        id: "hw.charge",
        section: Section::Hardware,
        title: "Battery charge is at least 90%",
        docs: "Charge below 90% is a warning. The 95% runtime rule lives in `protect.runtime`.",
        needs_root: false,
    },
    CatalogEntry {
        id: "hw.battery-age",
        section: Section::Hardware,
        title: "Battery age versus install date",
        docs: "When `battery.installed` is set, warn once the pack is older than three years.",
        needs_root: false,
    },
    CatalogEntry {
        id: "hw.self-test",
        section: Section::Hardware,
        title: "Last UPS self-test",
        docs: "Reports `ups.test.result`. Doctor never initiates a test.",
        needs_root: false,
    },
    CatalogEntry {
        id: "svc.driver",
        section: Section::Services,
        title: "NUT driver is active",
        docs: "`nut-driver@<ups>.service` is active.",
        needs_root: false,
    },
    CatalogEntry {
        id: "svc.server",
        section: Section::Services,
        title: "NUT server is active",
        docs: "`nut-server.service` is active.",
        needs_root: false,
    },
    CatalogEntry {
        id: "svc.monitor",
        section: Section::Services,
        title: "NUT monitor is active",
        docs: "`nut-monitor.service` is active.",
        needs_root: false,
    },
    CatalogEntry {
        id: "svc.enabled",
        section: Section::Services,
        title: "NUT units are enabled",
        docs: "The driver, server, monitor, and `nut.target` are enabled.",
        needs_root: false,
    },
    CatalogEntry {
        id: "svc.restarts",
        section: Section::Services,
        title: "Driver NRestarts are stable",
        docs: "With `--observe SECS`, the driver stays active and NRestarts does not increase. Skipped unless observe is set.",
        needs_root: false,
    },
    CatalogEntry {
        id: "svc.listen",
        section: Section::Services,
        title: "upsd listens only on loopback",
        docs: "Port 3493 listeners in `/proc/net/tcp` and `/proc/net/tcp6` are loopback addresses.",
        needs_root: false,
    },
    CatalogEntry {
        id: "svc.client",
        section: Section::Services,
        title: "A loopback monitor client is logged in",
        docs: "LIST CLIENT includes a loopback address for this UPS.",
        needs_root: false,
    },
    CatalogEntry {
        id: "svc.version",
        section: Section::Services,
        title: "NUT version policy",
        docs: "Require NUT 2.8.1 or newer. 2.8.3 and later warn so CyberPower default-delay changes can be reviewed.",
        needs_root: false,
    },
    CatalogEntry {
        id: "protect.runtime",
        section: Section::Protection,
        title: "Early shutdown fits remaining runtime",
        docs: "When charge is at least 95%, `early_after + shutdown_budget + offdelay + 60s` must fit the live runtime (athena's 420 s rule).",
        needs_root: false,
    },
    CatalogEntry {
        id: "protect.poweroff-wait",
        section: Section::Protection,
        title: "POWEROFF_WAIT headroom",
        docs: "`POWEROFF_WAIT` must be at least full-charge runtime plus 10 minutes so the reboot net cannot fire on battery.",
        needs_root: false,
    },
    CatalogEntry {
        id: "protect.hook",
        section: Section::Protection,
        title: "Event hook is installed",
        docs: "`/usr/local/sbin/nut-upssched-cmd` (or the legacy athena hook) exists and is executable.",
        needs_root: false,
    },
    CatalogEntry {
        id: "protect.mail",
        section: Section::Protection,
        title: "Mail path exists",
        docs: "`/usr/sbin/sendmail` is present so the hook can submit notification mail.",
        needs_root: false,
    },
    CatalogEntry {
        id: "protect.pwrstatd",
        section: Section::Protection,
        title: "PowerPanel is not running",
        docs: "`pwrstatd` must not be running. Doctor never stops it.",
        needs_root: false,
    },
    CatalogEntry {
        id: "protect.killpower",
        section: Section::Protection,
        title: "No stale /etc/killpower",
        docs: "A leftover `/etc/killpower` is reported and never cleared.",
        needs_root: false,
    },
    CatalogEntry {
        id: "root.drift",
        section: Section::Root,
        title: "Rendered files match live NUT config",
        docs: "Compare rendered files with live paths, secrets masked. Skipped without root.",
        needs_root: true,
    },
    CatalogEntry {
        id: "root.credentials",
        section: Section::Root,
        title: "upsd and upsmon credentials agree",
        docs: "The upsmon password in `upsd.users` matches the MONITOR line. Secrets are never printed.",
        needs_root: true,
    },
    CatalogEntry {
        id: "root.placeholders",
        section: Section::Root,
        title: "No password placeholders remain",
        docs: "Live files must not contain `@UPSMON_PASSWORD@` or `@OPERATOR_PASSWORD@`.",
        needs_root: true,
    },
    CatalogEntry {
        id: "root.instcmds",
        section: Section::Root,
        title: "No instcmds = ALL grant",
        docs: "Operator and upsmon accounts must not grant `instcmds = ALL`.",
        needs_root: true,
    },
    CatalogEntry {
        id: "root.perms",
        section: Section::Root,
        title: "NUT file owners and modes",
        docs: "`/etc/nut/*` is `root:nut` 0640; the hook is `root:root` 0755.",
        needs_root: true,
    },
    CatalogEntry {
        id: "boot.efi-order",
        section: Section::Boot,
        title: "EFI boot order starts with Linux",
        docs: "Reuses the BIOS boot-readiness audit.",
        needs_root: false,
    },
    CatalogEntry {
        id: "boot.nofail",
        section: Section::Boot,
        title: "Listed mounts have nofail",
        docs: "Host-config `boot.nofail_mounts` carry `nofail` in fstab.",
        needs_root: false,
    },
    CatalogEntry {
        id: "boot.fstab",
        section: Section::Boot,
        title: "findmnt --verify is clean",
        docs: "Reuses the BIOS boot-readiness audit.",
        needs_root: false,
    },
    CatalogEntry {
        id: "boot.journal",
        section: Section::Boot,
        title: "journald is persistent",
        docs: "Reuses the BIOS boot-readiness audit.",
        needs_root: false,
    },
    CatalogEntry {
        id: "boot.panic",
        section: Section::Boot,
        title: "kernel.panic is set",
        docs: "Advisory: a hang will not reboot when kernel.panic=0.",
        needs_root: false,
    },
    CatalogEntry {
        id: "boot.watchdog",
        section: Section::Boot,
        title: "Watchdog is available",
        docs: "Advisory hardware watchdog check from the BIOS audit.",
        needs_root: false,
    },
    CatalogEntry {
        id: "bios.profile",
        section: Section::Boot,
        title: "Board profile matches DMI",
        docs: "DMI board_vendor/board_name match a board profile, or the generic fallback.",
        needs_root: false,
    },
    CatalogEntry {
        id: "bios.attest",
        section: Section::Boot,
        title: "BIOS attestation freshness",
        docs: "A changed BIOS version or efivar fingerprint is a warning, never a failure.",
        needs_root: false,
    },
    CatalogEntry {
        id: "proof.outage",
        section: Section::Proof,
        title: "Last outage drill",
        docs: "Shows the last passing outage drill, or that one has never been recorded. Freshness against BIOS/NUT/config hashes lands with `nut drill`.",
        needs_root: false,
    },
];

pub fn catalog() -> &'static [CatalogEntry] {
    CATALOG
}

pub fn find_entry(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|entry| entry.id == id)
}

pub fn matches_filter(entry: &CatalogEntry, filter: &str) -> bool {
    let filter = filter.trim();
    if filter.is_empty() {
        return true;
    }
    entry.id == filter
        || entry.id.starts_with(&format!("{filter}."))
        || entry.section.slug() == filter
        || (filter == "bios" && entry.id.starts_with("bios."))
        || (filter == "boot" && entry.id.starts_with("bios."))
}

pub fn validate_filters(filters: &[String]) -> Result<(), String> {
    for filter in filters {
        if !CATALOG.iter().any(|entry| matches_filter(entry, filter)) {
            return Err(format!(
                "unknown doctor check or section {filter:?}; try `nut doctor --list`"
            ));
        }
    }
    Ok(())
}

pub fn run_checks<H: Host>(host: &H, snapshot: &Snapshot, filters: &[String]) -> Vec<Check> {
    let selected: Vec<&CatalogEntry> = CATALOG
        .iter()
        .filter(|entry| {
            filters.is_empty() || filters.iter().any(|filter| matches_filter(entry, filter))
        })
        .collect();
    selected
        .into_iter()
        .map(|entry| execute(entry, host, snapshot))
        .collect()
}

fn execute<H: Host>(entry: &CatalogEntry, host: &H, snapshot: &Snapshot) -> Check {
    if entry.needs_root && !snapshot.is_root() {
        return skipped(entry, "root-only", Some("sudo nut doctor".to_owned()));
    }
    catch_unwind(AssertUnwindSafe(|| checks::run(entry, host, snapshot)))
        .unwrap_or_else(|_| unknown(entry, "check panicked; remaining checks still ran"))
}

pub fn summarize(checks: &[Check]) -> Summary {
    let mut summary = Summary::default();
    for check in checks {
        match check.status {
            Status::Pass => summary.passed += 1,
            Status::Warn => summary.warnings += 1,
            Status::Fail => summary.failed += 1,
            Status::Unknown => summary.unknown += 1,
            Status::Skipped => summary.skipped += 1,
        }
    }
    summary.root_only_skipped = checks
        .iter()
        .filter(|check| check.needs_root && check.status == Status::Skipped)
        .count();
    summary
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    pub passed: usize,
    pub warnings: usize,
    pub failed: usize,
    pub unknown: usize,
    pub skipped: usize,
    pub root_only_skipped: usize,
}

impl Summary {
    pub fn line(&self, ascii: bool) -> String {
        let sep = if ascii { " | " } else { " · " };
        let mut parts = vec![
            format!("{} passed", self.passed),
            format!(
                "{} warning{}",
                self.warnings,
                if self.warnings == 1 { "" } else { "s" }
            ),
            format!("{} failed", self.failed),
        ];
        if self.unknown > 0 {
            parts.push(format!("{} unknown", self.unknown));
        }
        if self.skipped > 0 {
            let extra = if self.root_only_skipped > 0 {
                " (root-only: sudo nut doctor)".to_owned()
            } else {
                String::new()
            };
            parts.push(format!("{} skipped{extra}", self.skipped));
        }
        parts.join(sep)
    }
}
