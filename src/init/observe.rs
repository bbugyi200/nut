#![forbid(unsafe_code)]

//! Read-only observation collection for `nut init`.
//!
//! [`collect`] gathers every input the pure planner needs through the
//! [`Host`] boundary. It never writes files, consumes entropy, creates locks,
//! or prompts. Absent, unreadable, malformed, and probe-failed inputs stay
//! distinct: an unknown is never reported as an absence.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{HostConfig, ResolvedConfig, load_config, resolve};
use crate::host::{FileMetadata, Host, HostError, ProcessRequest};
use crate::init::secrets::{parse_monitor_password, parse_upsd_users};
use crate::profiles::{UsbId, load_catalog};
use crate::render::{self, RenderedFiles};

pub const LIVE_NUT_CONF: &str = "/etc/nut/nut.conf";
pub const LIVE_UPS_CONF: &str = "/etc/nut/ups.conf";
pub const LIVE_UPSD_USERS: &str = "/etc/nut/upsd.users";
pub const LIVE_UPSMON_CONF: &str = "/etc/nut/upsmon.conf";
pub const LIVE_UPSSCHED_CONF: &str = "/etc/nut/upssched.conf";
pub const LIVE_HOOK: &str = "/usr/local/sbin/nut-upssched-cmd";
pub const LEGACY_HOOK: &str = "/usr/local/sbin/athena-upssched-cmd";
pub const FSTAB_PATH: &str = "/etc/fstab";
pub const OPERATOR_SECRET_PATH: &str = "/etc/nut-cli/operator.secret";
pub const PENDING_PATH: &str = "/var/lib/nut-cli/pending.json";
pub const STATE_PATH: &str = "/var/lib/nut-cli/state.json";
pub const KILLPOWER_PATH: &str = "/etc/killpower";
pub const REQUIRED_PACKAGES: &[&str] = &["nut-client", "nut-server"];

#[derive(Clone, Default, Eq, PartialEq)]
pub enum FileState {
    #[default]
    Absent,
    Unreadable,
    Unsafe {
        reason: String,
    },
    Present {
        bytes: Vec<u8>,
        metadata: Option<FileMetadata>,
    },
}

impl std::fmt::Debug for FileState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => write!(f, "Absent"),
            Self::Unreadable => write!(f, "Unreadable"),
            Self::Unsafe { reason } => f.debug_struct("Unsafe").field("reason", reason).finish(),
            Self::Present { bytes, metadata } => f
                .debug_struct("Present")
                .field("len", &bytes.len())
                .field("metadata", metadata)
                .finish(),
        }
    }
}

impl FileState {
    pub fn present_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Present { bytes, .. } => Some(bytes),
            _ => None,
        }
    }

    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageState {
    pub installed_version: Option<String>,
    pub candidate_version: Option<String>,
    pub probe_failed: Option<String>,
}

impl PackageState {
    pub fn missing(&self) -> bool {
        self.installed_version.is_none() && self.probe_failed.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServiceState {
    pub active: Option<String>,
    pub enabled: Option<String>,
    pub probe_failed: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GroupObservation {
    pub group_file: FileState,
    pub passwd_file: FileState,
    pub nut_gid: Option<u32>,
    pub operators_members: Vec<String>,
    pub operators_gid: Option<u32>,
    pub user_exists: BTreeMap<String, bool>,
    pub user_in_operators: BTreeMap<String, bool>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsbObservation {
    pub all_ids: Vec<(PathBuf, UsbId)>,
    pub matching_sysfs: Vec<PathBuf>,
    pub selected: Option<UsbId>,
    pub ambiguous: bool,
    pub skipped_dummy: bool,
    pub error: Option<String>,
    pub node_gid: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpsEvidence {
    pub vars: BTreeMap<String, String>,
    pub vars_error: Option<String>,
    pub cmds: Vec<String>,
    pub cmds_error: Option<String>,
    pub clients: Vec<String>,
    pub clients_error: Option<String>,
    pub nut_version: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CredentialObservation {
    pub upsd_users: FileState,
    pub upsmon_conf: FileState,
    pub operator_secret: FileState,
    pub upsmon_password_present: bool,
    pub upsmon_malformed: bool,
    pub operator_password_present: bool,
    pub operator_malformed: bool,
    pub operator_instcmds: Vec<String>,
    pub operator_has_actions_set: bool,
    pub operator_has_all: bool,
    pub passwords_agree: Option<bool>,
    pub upsmon_fingerprint: Option<String>,
    pub operator_fingerprint: Option<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Observation {
    pub config_path: Option<PathBuf>,
    pub config_source: FileState,
    pub config: Option<HostConfig>,
    pub config_error: Option<String>,
    pub resolved: Option<ResolvedConfig>,
    pub resolve_error: Option<String>,
    pub rendered: Option<RenderedFiles>,
    pub render_error: Option<String>,
    pub live_files: BTreeMap<String, FileState>,
    pub fstab: FileState,
    pub legacy_hook: FileState,
    pub legacy_hook_recognized: bool,
    pub pending: FileState,
    pub state_file: FileState,
    pub packages: BTreeMap<String, PackageState>,
    pub services: BTreeMap<String, ServiceState>,
    pub groups: GroupObservation,
    pub usb: UsbObservation,
    pub ups: UpsEvidence,
    pub pwrstatd_running: Option<bool>,
    pub pwrstatd_error: Option<String>,
    pub killpower_present: Option<bool>,
    pub killpower_error: Option<String>,
    pub credentials: CredentialObservation,
    pub unreadable_required: Vec<String>,
}

impl std::fmt::Debug for Observation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observation")
            .field("config_path", &self.config_path)
            .field("config_error", &self.config_error)
            .field("resolve_error", &self.resolve_error)
            .field("render_error", &self.render_error)
            .field("live_files", &self.live_files)
            .field("fstab", &self.fstab)
            .field("legacy_hook_recognized", &self.legacy_hook_recognized)
            .field("pending", &self.pending)
            .field("packages", &self.packages)
            .field("services", &self.services)
            .field("groups", &self.groups)
            .field("usb", &self.usb)
            .field("ups_vars", &self.ups.vars.keys().collect::<Vec<_>>())
            .field("ups_vars_error", &self.ups.vars_error)
            .field("pwrstatd_running", &self.pwrstatd_running)
            .field("killpower_present", &self.killpower_present)
            .field("unreadable_required", &self.unreadable_required)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ObservationError(pub String);

fn observe_file(host: &impl Host, path: &Path, protected: bool) -> FileState {
    let read = if protected {
        host.read_protected(path)
    } else {
        host.read_file(path)
    };
    match read {
        Ok(None) => FileState::Absent,
        Ok(Some(bytes)) => {
            let metadata = host.metadata(path).ok().flatten();
            FileState::Present { bytes, metadata }
        }
        Err(error) if error.is_permission_denied() => FileState::Unreadable,
        Err(error) => {
            let message = error.to_string();
            if message.contains("symlink")
                || message.contains("traversal")
                || message.contains("writable ancestor")
                || message.contains("hardlink")
                || message.contains("unsafe")
            {
                FileState::Unsafe { reason: message }
            } else if message.contains("permission denied") {
                FileState::Unreadable
            } else {
                FileState::Unsafe { reason: message }
            }
        }
    }
}

fn run_bounded_text(
    host: &impl Host,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<(Option<i32>, String, String), HostError> {
    let owned: Vec<OsString> = args.iter().map(OsString::from).collect();
    let request = ProcessRequest {
        program: std::ffi::OsStr::new(program),
        args: &owned,
        timeout,
        secret: false,
        inherit_tty: false,
    };
    let output = host.run_bounded(&request)?;
    Ok((
        output.status,
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

fn observe_package(host: &impl Host, package: &str) -> PackageState {
    let timeout = Duration::from_secs(5);
    let installed = match run_bounded_text(
        host,
        "dpkg-query",
        &["-W", "-f=${Version}", package],
        timeout,
    ) {
        Ok((Some(0), version, _)) if !version.is_empty() => Some(version),
        Ok((_, _, _)) => None,
        Err(error) => {
            return PackageState {
                installed_version: None,
                candidate_version: None,
                probe_failed: Some(error.to_string()),
            };
        }
    };
    if installed.is_some() {
        return PackageState {
            installed_version: installed,
            candidate_version: None,
            probe_failed: None,
        };
    }
    // Only query the candidate when the package is missing.
    let candidate = match run_bounded_text(host, "apt-cache", &["policy", package], timeout) {
        Ok((Some(0), stdout, _)) => parse_apt_candidate(&stdout),
        Ok(_) => None,
        Err(error) => {
            return PackageState {
                installed_version: None,
                candidate_version: None,
                probe_failed: Some(error.to_string()),
            };
        }
    };
    PackageState {
        installed_version: None,
        candidate_version: candidate,
        probe_failed: None,
    }
}

fn parse_apt_candidate(output: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Candidate:") {
            let value = rest.trim().to_owned();
            if value.is_empty() || value == "(none)" {
                return None;
            }
            return Some(value);
        }
    }
    None
}

fn observe_service(host: &impl Host, unit: &str) -> ServiceState {
    let timeout = Duration::from_secs(5);
    let active = match run_bounded_text(host, "systemctl", &["is-active", unit], timeout) {
        Ok((_, stdout, _)) if !stdout.is_empty() => Some(stdout),
        Ok(_) => Some("unknown".to_owned()),
        Err(error) => {
            return ServiceState {
                active: None,
                enabled: None,
                probe_failed: Some(error.to_string()),
            };
        }
    };
    let enabled = match run_bounded_text(host, "systemctl", &["is-enabled", unit], timeout) {
        Ok((_, stdout, _)) if !stdout.is_empty() => Some(stdout),
        Ok(_) => None,
        Err(error) => {
            return ServiceState {
                active,
                enabled: None,
                probe_failed: Some(error.to_string()),
            };
        }
    };
    ServiceState {
        active,
        enabled,
        probe_failed: None,
    }
}

fn parse_group_file(contents: &[u8]) -> BTreeMap<String, (u32, Vec<String>)> {
    let mut out = BTreeMap::new();
    for line in String::from_utf8_lossy(contents).lines() {
        let mut parts = line.split(':');
        let (Some(name), Some(_), Some(gid), Some(members)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let gid = gid.parse::<u32>().unwrap_or(u32::MAX);
        let list = members
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        out.insert(name.to_owned(), (gid, list));
    }
    out
}

fn parse_passwd_users(contents: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(contents)
        .lines()
        .filter_map(|line| line.split(':').next().map(ToOwned::to_owned))
        .collect()
}

/// Collect every input the planner needs. Never writes, never consumes
/// entropy, never locks, never prompts.
pub fn collect<H: Host>(
    host: &H,
    config_override: Option<&Path>,
    ups_override: Option<&str>,
) -> Observation {
    let mut unreadable_required = Vec::new();

    let (config_path, config, config_error) = match load_config(host, config_override) {
        Ok((path, _, config)) => (Some(path), Some(config), None),
        Err(crate::config::ConfigError::NotFound(path)) => (
            Some(path.clone()),
            None,
            Some(format!(
                "configuration file does not exist: {}",
                path.display()
            )),
        ),
        Err(error) => {
            let message = error.to_string();
            if message.contains("cannot read") || message.contains("permission") {
                unreadable_required.push("host config".to_owned());
            }
            (None, None, Some(message))
        }
    };

    let config_source = config_path
        .as_deref()
        .map(|path| observe_file(host, path, false))
        .unwrap_or_default();

    let catalog = load_catalog(host)
        .unwrap_or_else(|_| crate::profiles::embedded_catalog().expect("embedded catalog parses"));

    let (resolved, resolve_error) = match config.as_ref() {
        Some(config) => {
            let path = config_path.clone().unwrap_or_default();
            match resolve(path, config, &catalog) {
                Ok(resolved) => (Some(resolved), None),
                Err(error) => (None, Some(error.to_string())),
            }
        }
        None => (None, None),
    };

    let (rendered, render_error) = match resolved.as_ref() {
        Some(resolved) => match render::render(resolved) {
            Ok(files) => (Some(files), None),
            Err(error) => (None, Some(error.to_string())),
        },
        None => (None, None),
    };

    let mut live_files = BTreeMap::new();
    for path in [
        LIVE_NUT_CONF,
        LIVE_UPS_CONF,
        LIVE_UPSD_USERS,
        LIVE_UPSMON_CONF,
        LIVE_UPSSCHED_CONF,
        LIVE_HOOK,
        "/etc/nut-cli/nut.toml",
        OPERATOR_SECRET_PATH,
    ] {
        let state = observe_file(host, Path::new(path), true);
        if state == FileState::Unreadable {
            unreadable_required.push(path.to_owned());
        }
        live_files.insert(path.to_owned(), state);
    }

    let fstab = observe_file(host, Path::new(FSTAB_PATH), false);
    let legacy_hook = observe_file(host, Path::new(LEGACY_HOOK), false);
    let legacy_hook_recognized = legacy_hook.present_bytes().is_some_and(|old| {
        rendered
            .as_ref()
            .is_some_and(|files| super::resources::recognized_legacy(old, files.hook.as_bytes()))
    });
    let pending = observe_file(host, Path::new(PENDING_PATH), true);
    let state_file = observe_file(host, Path::new(STATE_PATH), true);
    if pending == FileState::Unreadable {
        unreadable_required.push(PENDING_PATH.to_owned());
    }

    let mut packages = BTreeMap::new();
    for package in REQUIRED_PACKAGES {
        packages.insert((*package).to_owned(), observe_package(host, package));
    }

    let ups_name = ups_override
        .map(ToOwned::to_owned)
        .or_else(|| resolved.as_ref().map(|r| r.ups_name.clone()))
        .unwrap_or_else(|| "cp1500".to_owned());
    let driver_unit = format!("nut-driver@{ups_name}.service");
    let mut services = BTreeMap::new();
    for unit in [
        driver_unit.as_str(),
        "nut-server.service",
        "nut-monitor.service",
        "nut.target",
    ] {
        services.insert(unit.to_owned(), observe_service(host, unit));
    }

    let group_file = observe_file(host, Path::new("/etc/group"), false);
    let passwd_file = observe_file(host, Path::new("/etc/passwd"), false);
    let mut groups = GroupObservation {
        group_file: group_file.clone(),
        passwd_file: passwd_file.clone(),
        ..GroupObservation::default()
    };
    if let (Some(group_bytes), Some(passwd_bytes)) =
        (group_file.present_bytes(), passwd_file.present_bytes())
    {
        let parsed = parse_group_file(group_bytes);
        let users = parse_passwd_users(passwd_bytes);
        if let Some((gid, members)) = parsed.get("nut") {
            groups.nut_gid = Some(*gid);
            let _ = members;
        }
        if let Some((gid, members)) = parsed.get("nut-operators") {
            groups.operators_gid = Some(*gid);
            groups.operators_members = members.clone();
        }
        if let Some(resolved) = resolved.as_ref() {
            for user in &resolved.operators.users {
                let exists = users.contains(user);
                groups.user_exists.insert(user.clone(), exists);
                groups.user_in_operators.insert(
                    user.clone(),
                    groups.operators_members.iter().any(|m| m == user),
                );
            }
        }
    } else if group_file == FileState::Unreadable || passwd_file == FileState::Unreadable {
        unreadable_required.push("/etc/group or /etc/passwd".to_owned());
    }

    let mut usb = UsbObservation::default();
    if let Some(resolved) = resolved.as_ref() {
        if resolved.driver.value == "dummy-ups" {
            usb.skipped_dummy = true;
        } else {
            match crate::profiles::discover_usb_ids(host) {
                Ok(found) => {
                    usb.all_ids = found.clone();
                    let wanted_vendor = resolved.vendorid.value.clone();
                    let wanted_product = resolved.productid.value.clone();
                    let mut matching = Vec::new();
                    for (sysfs, id) in found {
                        if id.vendor_hex() == wanted_vendor && id.product_hex() == wanted_product {
                            let serial_matches = match resolved.ups.usb_match.serial.as_deref() {
                                None => true,
                                Some(wanted) => host
                                    .read_file(&sysfs.join("serial"))
                                    .ok()
                                    .flatten()
                                    .is_some_and(|bytes| {
                                        String::from_utf8_lossy(&bytes).trim() == wanted
                                    }),
                            };
                            if serial_matches {
                                matching.push(sysfs);
                            }
                        }
                    }
                    if matching.len() == 1 {
                        usb.selected = Some(UsbId {
                            vendor: u16::from_str_radix(&wanted_vendor, 16).unwrap_or(0),
                            product: u16::from_str_radix(&wanted_product, 16).unwrap_or(0),
                        });
                        usb.matching_sysfs = matching;
                    } else if matching.len() > 1 {
                        usb.ambiguous = true;
                        usb.matching_sysfs = matching;
                    }
                    // Node group for the selected device, when known.
                    if let Some(sysfs) = usb.matching_sysfs.first() {
                        let number = |name: &str| {
                            host.read_file(&sysfs.join(name))
                                .ok()
                                .flatten()
                                .and_then(|b| {
                                    String::from_utf8_lossy(&b).trim().parse::<u16>().ok()
                                })
                                .filter(|n| (1..=999).contains(n))
                        };
                        if let (Some(bus), Some(device)) = (number("busnum"), number("devnum")) {
                            let node = PathBuf::from(format!("/dev/bus/usb/{bus:03}/{device:03}"));
                            if let Ok(Some(meta)) = host.metadata(&node) {
                                usb.node_gid = Some(meta.gid);
                            }
                        }
                    }
                }
                Err(error) => {
                    usb.error = Some(error.to_string());
                }
            }
        }
    }

    let raw = host.observe_ups("127.0.0.1", 3493, &ups_name);
    let ups = UpsEvidence {
        vars: raw.vars,
        vars_error: raw.vars_error,
        cmds: raw.cmds,
        cmds_error: raw.cmds_error,
        clients: raw.clients,
        clients_error: raw.clients_error,
        nut_version: raw.nut_version,
    };

    let (pwrstatd_running, pwrstatd_error) = match host.run_program(
        std::ffi::OsStr::new("pgrep"),
        &[OsString::from("-x"), OsString::from("pwrstatd")],
    ) {
        Ok(output) if output.status == Some(1) => (Some(false), None),
        Ok(output) if output.status == Some(0) => (Some(true), None),
        Ok(output) => (
            None,
            Some(format!("pgrep exited {}", output.status.unwrap_or(-1))),
        ),
        Err(error) => (None, Some(error.to_string())),
    };

    let (killpower_present, killpower_error) = match host.path_exists(Path::new(KILLPOWER_PATH)) {
        Ok(present) => (Some(present), None),
        Err(error) => (None, Some(error.to_string())),
    };

    let upsd_users_state = live_files
        .get(LIVE_UPSD_USERS)
        .cloned()
        .unwrap_or(FileState::Absent);
    let upsmon_state = live_files
        .get(LIVE_UPSMON_CONF)
        .cloned()
        .unwrap_or(FileState::Absent);
    let operator_secret_state = observe_file(host, Path::new(OPERATOR_SECRET_PATH), true);
    if operator_secret_state == FileState::Unreadable && resolved.is_some() {
        // Operator secret readability only matters when a plan needs it;
        // record it as required only when adoption or rotation is in play.
        // The planner refines this; collection just records the state.
    }
    let mut credentials = CredentialObservation {
        upsd_users: upsd_users_state.clone(),
        upsmon_conf: upsmon_state.clone(),
        operator_secret: operator_secret_state.clone(),
        ..CredentialObservation::default()
    };
    if let (Some(users_bytes), Some(monitor_bytes)) = (
        upsd_users_state.present_bytes(),
        upsmon_state.present_bytes(),
    ) {
        let users_text = String::from_utf8_lossy(users_bytes);
        let monitor_text = String::from_utf8_lossy(monitor_bytes);
        let parsed = parse_upsd_users(&users_text);
        credentials.upsmon_malformed = parsed.upsmon_malformed;
        credentials.operator_malformed = parsed.operator_malformed;
        credentials.operator_instcmds = parsed.operator_instcmds.clone();
        credentials.operator_has_actions_set = parsed.operator_has_actions_set;
        credentials.operator_has_all = parsed.operator_has_all;
        credentials.upsmon_password_present = parsed.upsmon.is_some();
        credentials.operator_password_present = parsed.operator.is_some();
        if let (Some(adopted_upsmon), Ok(monitor_secret)) = (
            parsed.upsmon.as_ref(),
            parse_monitor_password(&monitor_text, &ups_name),
        ) {
            credentials.passwords_agree = Some(adopted_upsmon.value() == monitor_secret);
            credentials.upsmon_fingerprint = Some(crate::init::secrets::fingerprint_secret(
                adopted_upsmon.value(),
            ));
        } else if parsed.upsmon.is_some() {
            credentials.passwords_agree = Some(false);
            if let Some(secret) = parsed.upsmon.as_ref() {
                credentials.upsmon_fingerprint =
                    Some(crate::init::secrets::fingerprint_secret(secret.value()));
            }
        }
        if let Some(secret) = parsed.operator.as_ref() {
            credentials.operator_fingerprint =
                Some(crate::init::secrets::fingerprint_secret(secret.value()));
        }
    } else {
        if upsd_users_state == FileState::Unreadable {
            credentials.upsmon_malformed = false;
        }
        // Operator secret file fingerprint, when readable, seeds rotation
        // resume without exposing the secret.
        if let Some(bytes) = operator_secret_state.present_bytes() {
            let text = String::from_utf8_lossy(bytes);
            let trimmed = text.lines().next().unwrap_or("").trim().to_owned();
            if !trimmed.is_empty() {
                credentials.operator_fingerprint =
                    Some(crate::init::secrets::fingerprint_secret(&trimmed));
                credentials.operator_password_present = true;
            }
        }
    }

    Observation {
        config_path,
        config_source,
        config,
        config_error,
        resolved,
        resolve_error,
        rendered,
        render_error,
        live_files,
        fstab,
        legacy_hook,
        legacy_hook_recognized,
        pending,
        state_file,
        packages,
        services,
        groups,
        usb,
        ups,
        pwrstatd_running,
        pwrstatd_error,
        killpower_present,
        killpower_error,
        credentials,
        unreadable_required,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;

    fn minimal_config() -> &'static str {
        include_str!("../../tests/fixtures/hosts/athena/nut.toml")
    }

    #[test]
    fn collection_touches_no_writes_or_entropy() {
        let host = FakeHost::default()
            .with_file(
                "/etc/nut-cli/nut.toml",
                minimal_config().as_bytes().to_vec(),
            )
            .with_env("NUT_CONFIG", "/etc/nut-cli/nut.toml");
        let observation = collect(&host, None, None);
        assert!(observation.resolved.is_some());
        for effect in host.effects() {
            match effect {
                crate::host::HostEffect::WriteFile { .. }
                | crate::host::HostEffect::ReplaceFile { .. }
                | crate::host::HostEffect::Remove { .. }
                | crate::host::HostEffect::SetMetadata { .. }
                | crate::host::HostEffect::CreateDir { .. }
                | crate::host::HostEffect::Lock { .. }
                | crate::host::HostEffect::Random { .. } => {
                    panic!("collection must not write or consume entropy: {effect:?}");
                }
                crate::host::HostEffect::Run { .. }
                | crate::host::HostEffect::ObserveUps { .. } => {}
            }
        }
    }

    #[test]
    fn dummy_profile_resolves_without_usb_ids() {
        let config = r#"
schema = 1
[host]
name = "test"
[ups.dev]
profile = "dummy"
[shutdown]
notify_after = "30s"
early_after = "3m"
poweroff_wait = "30m"
shutdown_budget = "2m"
[notify]
from = "root@example.invalid"
to = "ops@example.invalid"
commbad_after = "60s"
"#;
        let host =
            FakeHost::default().with_file("/etc/nut-cli/nut.toml", config.as_bytes().to_vec());
        let observation = collect(&host, Some(Path::new("/etc/nut-cli/nut.toml")), None);
        assert!(observation.resolve_error.is_none());
        let resolved = observation.resolved.expect("dummy resolves");
        assert_eq!(resolved.driver.value, "dummy-ups");
        assert!(observation.usb.skipped_dummy);
    }
}
