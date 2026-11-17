#![forbid(unsafe_code)]

//! Validated, resumable restoration of a completed `nut init` transaction.
//!
//! Rollback reuses the init writer lock and private backup format. It restores
//! only recorded files and membership additions, keeps installed packages and
//! package-owned users, and never starts a NUT service, or stops one while
//! the monitor runs, without a fresh OL observation.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::observe::{LIVE_UPS_CONF, PENDING_PATH, STATE_PATH};
use super::transaction::{
    BACKUP_ROOT, BackupFileSummary, BackupSummary, FileMetadataRecord, MembershipAddition,
    ROLLBACK_JOURNAL_NAME, RestoreFile, ServiceSnapshot, TransactionError, ValidatedBackup,
    acquire_writer_lock, backup_stamps, backup_summaries, load_backup, read_state_record,
};
use crate::host::{FileMetadata, Host, HostError, ProcessOutput, ProcessRequest};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const MONITOR_UNIT: &str = "nut-monitor.service";
const MAX_ROLLBACK_BYTES: usize = 1024 * 1024;
const RETAINED_PACKAGES: &str =
    "Installed distribution packages and package-owned users are retained.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RollbackOptions {
    pub upsd_address: String,
    pub upsd_port: u16,
}

impl Default for RollbackOptions {
    fn default() -> Self {
        Self {
            upsd_address: "127.0.0.1".to_owned(),
            upsd_port: 3493,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RollbackPlan {
    pub stamp: String,
    pub files: Vec<BackupFileSummary>,
    pub services: Vec<ServiceSnapshot>,
    pub membership_removals: Vec<MembershipAddition>,
    pub groups_maybe_removed: Vec<String>,
    pub package_boundary: &'static str,
    pub already_complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RollbackReport {
    pub stamp: String,
    pub files_restored: Vec<PathBuf>,
    pub memberships_removed: Vec<MembershipAddition>,
    pub services_changed: Vec<String>,
    pub groups_removed: Vec<String>,
    pub groups_retained: Vec<String>,
    pub already_complete: bool,
    pub package_boundary: &'static str,
}

#[derive(Debug, Error)]
pub enum RollbackError {
    #[error("rollback requires root privileges")]
    NotRoot,
    #[error("an init transaction is pending; recover it before rollback")]
    PendingTransaction,
    #[error("there are no complete init backups")]
    NoBackups,
    #[error("backup {0} is not the currently applied init state")]
    NotCurrent(String),
    #[error("rollback refused: {0}")]
    Refused(String),
    #[error("UPS is not safely online: {0}")]
    UnsafeUps(String),
    #[error("could not restore service {unit}: {detail}")]
    Service { unit: String, detail: String },
    #[error(transparent)]
    Transaction(#[from] TransactionError),
    #[error(transparent)]
    Host(#[from] HostError),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackProgress {
    schema_version: u32,
    stamp: String,
    groups_to_remove: BTreeSet<String>,
    completed_files: BTreeSet<PathBuf>,
    completed_memberships: BTreeSet<String>,
    completed_services: BTreeSet<String>,
    completed_groups: BTreeSet<String>,
    complete: bool,
}

impl RollbackProgress {
    fn new(stamp: &str, groups_to_remove: BTreeSet<String>) -> Self {
        Self {
            schema_version: 1,
            stamp: stamp.to_owned(),
            groups_to_remove,
            completed_files: BTreeSet::new(),
            completed_memberships: BTreeSet::new(),
            completed_services: BTreeSet::new(),
            completed_groups: BTreeSet::new(),
            complete: false,
        }
    }
}

/// Return redacted summaries for every complete backup, newest first.
pub fn list_backups<H: Host>(host: &H) -> Result<Vec<BackupSummary>, RollbackError> {
    Ok(backup_summaries(host)?)
}

/// Build an authoritative, read-only rollback preview. With no stamp, the
/// newest backup is selected; a corrupt newest backup is an error and never
/// causes fallback to an older transaction.
pub fn plan<H: Host>(host: &H, stamp: Option<&str>) -> Result<RollbackPlan, RollbackError> {
    refuse_pending(host)?;
    let backup = select_backup(host, stamp)?;
    let progress = read_progress(host, &backup)?;
    let state = read_state_record(host)?;
    let groups_to_remove = groups_for_state(&backup, state.as_ref(), progress.as_ref())?;
    let already_complete = progress
        .as_ref()
        .is_some_and(|record| progress_complete(record, &backup));
    if !already_complete {
        validate_selected_state(host, &backup, &state, progress.as_ref())?;
        preflight_targets(host, &backup, &state, progress.as_ref())?;
    }
    Ok(plan_from_backup(
        &backup,
        &groups_to_remove,
        already_complete,
    ))
}

/// Restore a selected completed init transaction under the shared writer
/// lock. Progress is checkpointed after every file, membership, group, and
/// service boundary, so the same command can safely resume after interruption.
pub fn restore<H: Host>(
    host: &H,
    stamp: Option<&str>,
    options: &RollbackOptions,
) -> Result<RollbackReport, RollbackError> {
    if host.euid() != 0 {
        return Err(RollbackError::NotRoot);
    }
    let selected_stamp = select_stamp(host, stamp)?;
    let lock = acquire_writer_lock(host)?;
    let _lock = lock;
    refuse_pending(host)?;
    let backup = load_backup(host, &selected_stamp)?;
    let state = read_state_record(host)?;
    let existing_progress = read_progress(host, &backup)?;
    let groups_to_remove = groups_for_state(&backup, state.as_ref(), existing_progress.as_ref())?;
    let mut progress = existing_progress
        .unwrap_or_else(|| RollbackProgress::new(&backup.summary.stamp, groups_to_remove));
    if progress_complete(&progress, &backup) {
        return Ok(empty_report(&backup.summary.stamp, true));
    }

    validate_selected_state(host, &backup, &state, Some(&progress))?;
    preflight_targets(host, &backup, &state, Some(&progress))?;
    save_progress(host, &backup.summary.stamp, &progress)?;

    let mut report = empty_report(&backup.summary.stamp, false);
    let state_path = Path::new(STATE_PATH);
    for file in files_in_restore_order(&backup)
        .into_iter()
        .filter(|file| file.path != state_path)
    {
        if progress.completed_files.contains(&file.path) {
            continue;
        }
        if !file_is_preimage(host, file)? {
            ensure_applied_output(host, file, state.as_ref(), &backup.summary.stamp)?;
            restore_file(host, file)?;
            report.files_restored.push(file.path.clone());
        }
        if !file_is_preimage(host, file)? {
            return Err(RollbackError::Refused(format!(
                "{} did not reach its backed-up state; resume with `nut rollback {}`",
                file.path.display(),
                backup.summary.stamp
            )));
        }
        progress.completed_files.insert(file.path.clone());
        save_progress(host, &backup.summary.stamp, &progress)?;
    }

    for addition in &backup.summary.membership_additions {
        let key = membership_key(addition);
        if progress.completed_memberships.contains(&key) {
            continue;
        }
        if group_has_member(host, &addition.group, &addition.user)? {
            run_checked(
                host,
                "gpasswd",
                &["-d", &addition.user, &addition.group],
                "remove the transaction's group membership",
            )?;
            if group_has_member(host, &addition.group, &addition.user)? {
                return Err(RollbackError::Refused(format!(
                    "membership {} in {} remains; resume with `nut rollback {}`",
                    addition.user, addition.group, backup.summary.stamp
                )));
            }
            report.memberships_removed.push(addition.clone());
        }
        progress.completed_memberships.insert(key);
        save_progress(host, &backup.summary.stamp, &progress)?;
    }

    for group in &progress.groups_to_remove {
        let key = format!("group:{group}");
        if progress.completed_groups.contains(&key) {
            continue;
        }
        if group_exists(host, group)? && group_is_unused(host, group)? {
            run_checked(
                host,
                "groupdel",
                &[group],
                "remove the unused created group",
            )?;
            if group_exists(host, group)? {
                return Err(RollbackError::Refused(format!(
                    "created group {group} remains; resume with `nut rollback {}`",
                    backup.summary.stamp
                )));
            }
            report.groups_removed.push(group.clone());
        } else if group_exists(host, group)? {
            report.groups_retained.push(group.clone());
        }
        progress.completed_groups.insert(key);
        save_progress(host, &backup.summary.stamp, &progress)?;
    }

    let mut services = backup.summary.services.clone();
    services
        .sort_by(|left, right| service_rank(left, right).then_with(|| left.unit.cmp(&right.unit)));
    for service in services {
        if progress.completed_services.contains(&service.unit) {
            continue;
        }
        if restore_service(host, &backup, &service, options)? {
            report.services_changed.push(service.unit.clone());
        }
        progress.completed_services.insert(service.unit.clone());
        save_progress(host, &backup.summary.stamp, &progress)?;
    }

    let state_file = backup
        .files
        .iter()
        .find(|file| file.path == state_path)
        .ok_or_else(|| RollbackError::Refused("backup has no state preimage".into()))?;
    if !progress.completed_files.contains(state_path) {
        if !file_is_preimage(host, state_file)? {
            ensure_applied_output(host, state_file, state.as_ref(), &backup.summary.stamp)?;
            restore_file(host, state_file)?;
            report.files_restored.push(state_path.to_path_buf());
        }
        if !file_is_preimage(host, state_file)? {
            return Err(target_conflict(state_file));
        }
        progress.completed_files.insert(state_path.to_path_buf());
        save_progress(host, &backup.summary.stamp, &progress)?;
    }

    progress.complete = true;
    save_progress(host, &backup.summary.stamp, &progress)?;
    Ok(report)
}

fn empty_report(stamp: &str, already_complete: bool) -> RollbackReport {
    RollbackReport {
        stamp: stamp.to_owned(),
        files_restored: Vec::new(),
        memberships_removed: Vec::new(),
        services_changed: Vec::new(),
        groups_removed: Vec::new(),
        groups_retained: Vec::new(),
        already_complete,
        package_boundary: RETAINED_PACKAGES,
    }
}

fn plan_from_backup(
    backup: &ValidatedBackup,
    groups_to_remove: &BTreeSet<String>,
    already_complete: bool,
) -> RollbackPlan {
    RollbackPlan {
        stamp: backup.summary.stamp.clone(),
        files: backup.summary.files.clone(),
        services: backup.summary.services.clone(),
        membership_removals: backup.summary.membership_additions.clone(),
        groups_maybe_removed: groups_to_remove.iter().cloned().collect(),
        package_boundary: RETAINED_PACKAGES,
        already_complete,
    }
}

fn groups_for_state(
    backup: &ValidatedBackup,
    state: Option<&super::transaction::StateRecord>,
    progress: Option<&RollbackProgress>,
) -> Result<BTreeSet<String>, RollbackError> {
    if let Some(progress) = progress {
        return Ok(progress.groups_to_remove.clone());
    }
    let Some(state) = state.filter(|state| state.backup_reference == backup.summary.stamp) else {
        return Ok(BTreeSet::new());
    };
    if state
        .created_groups
        .iter()
        .any(|group| !backup.summary.created_groups.contains(group))
    {
        return Err(RollbackError::Refused(
            "state record group history does not match its backup manifest".into(),
        ));
    }
    Ok(state.created_groups.iter().cloned().collect())
}

fn select_backup<H: Host>(
    host: &H,
    requested: Option<&str>,
) -> Result<ValidatedBackup, RollbackError> {
    let stamp = select_stamp(host, requested)?;
    Ok(load_backup(host, &stamp)?)
}

fn select_stamp<H: Host>(host: &H, requested: Option<&str>) -> Result<String, RollbackError> {
    if let Some(stamp) = requested {
        return Ok(stamp.to_owned());
    }
    backup_stamps(host)?
        .into_iter()
        .next()
        .ok_or(RollbackError::NoBackups)
}

fn refuse_pending<H: Host>(host: &H) -> Result<(), RollbackError> {
    if host.path_exists(Path::new(PENDING_PATH))? {
        Err(RollbackError::PendingTransaction)
    } else {
        Ok(())
    }
}

fn validate_selected_state(
    host: &impl Host,
    backup: &ValidatedBackup,
    state: &Option<super::transaction::StateRecord>,
    progress: Option<&RollbackProgress>,
) -> Result<(), RollbackError> {
    if state
        .as_ref()
        .is_some_and(|record| record.backup_reference == backup.summary.stamp)
    {
        if !state.as_ref().is_some_and(|record| {
            record
                .created_groups
                .iter()
                .all(|group| backup.summary.created_groups.contains(group))
        }) {
            return Err(RollbackError::Refused(
                "state record group history does not match its backup manifest".into(),
            ));
        }
        return Ok(());
    }
    let Some(progress) = progress else {
        return Err(RollbackError::NotCurrent(backup.summary.stamp.clone()));
    };
    if progress.complete || state_is_preimage(host, backup)? {
        let state_path = Path::new(STATE_PATH);
        let other_files_done = backup
            .files
            .iter()
            .all(|file| file.path == state_path || progress.completed_files.contains(&file.path));
        let all_non_file_work_done = progress.completed_memberships.len()
            == backup.summary.membership_additions.len()
            && progress.completed_services.len() == backup.summary.services.len()
            && progress.completed_groups.len() == progress.groups_to_remove.len();
        if progress.complete || (other_files_done && all_non_file_work_done) {
            return Ok(());
        }
    }
    Err(RollbackError::NotCurrent(backup.summary.stamp.clone()))
}

fn state_is_preimage(host: &impl Host, backup: &ValidatedBackup) -> Result<bool, RollbackError> {
    let file = backup
        .files
        .iter()
        .find(|file| file.path == Path::new(STATE_PATH))
        .ok_or_else(|| RollbackError::Refused("backup has no state preimage".into()))?;
    file_is_preimage(host, file)
}

fn preflight_targets(
    host: &impl Host,
    backup: &ValidatedBackup,
    state: &Option<super::transaction::StateRecord>,
    progress: Option<&RollbackProgress>,
) -> Result<(), RollbackError> {
    for file in &backup.files {
        if progress.is_some_and(|record| record.completed_files.contains(&file.path)) {
            if !file_is_preimage(host, file)? {
                return Err(target_conflict(file));
            }
            continue;
        }
        if file_is_preimage(host, file)? {
            continue;
        }
        if file.path == Path::new(STATE_PATH)
            && state
                .as_ref()
                .is_some_and(|record| record.backup_reference == backup.summary.stamp)
        {
            continue;
        }
        if output_matches(host, &file.path, state.as_ref())? {
            continue;
        }
        return Err(target_conflict(file));
    }
    Ok(())
}

fn target_conflict(file: &RestoreFile) -> RollbackError {
    RollbackError::Refused(format!(
        "{} differs from the transaction output or its preimage; refusing to overwrite it",
        file.path.display()
    ))
}

fn ensure_applied_output(
    host: &impl Host,
    file: &RestoreFile,
    state: Option<&super::transaction::StateRecord>,
    stamp: &str,
) -> Result<(), RollbackError> {
    if file.path == Path::new(STATE_PATH)
        && state.is_some_and(|record| record.backup_reference == stamp)
    {
        return Ok(());
    }
    if output_matches(host, &file.path, state)? {
        Ok(())
    } else {
        Err(target_conflict(file))
    }
}

fn output_matches(
    host: &impl Host,
    path: &Path,
    state: Option<&super::transaction::StateRecord>,
) -> Result<bool, RollbackError> {
    let Some(state) = state else {
        return Ok(false);
    };
    if let Some(expected) = state.outputs.get(path) {
        let Some(bytes) = host.read_protected(path)? else {
            return Ok(false);
        };
        let Some(metadata) = host.metadata(path)? else {
            return Ok(false);
        };
        return Ok(sha256(&bytes) == expected.sha256
            && FileMetadataRecord::from(&metadata) == expected.metadata
            && metadata.is_file
            && !metadata.is_dir);
    }
    if state.absent_outputs.contains(path) {
        return Ok(host.read_protected(path)?.is_none() && host.metadata(path)?.is_none());
    }
    Ok(false)
}

fn file_is_preimage(host: &impl Host, file: &RestoreFile) -> Result<bool, RollbackError> {
    match (&file.contents, &file.metadata) {
        (None, None) => {
            Ok(host.read_protected(&file.path)?.is_none() && host.metadata(&file.path)?.is_none())
        }
        (Some(expected), Some(expected_metadata)) => {
            let Some(actual) = host.read_protected(&file.path)? else {
                return Ok(false);
            };
            let Some(metadata) = host.metadata(&file.path)? else {
                return Ok(false);
            };
            Ok(actual == *expected
                && FileMetadataRecord::from(&metadata) == *expected_metadata
                && metadata.is_file
                && !metadata.is_dir)
        }
        _ => Err(RollbackError::Refused(
            "backup file preimage has inconsistent metadata".into(),
        )),
    }
}

fn restore_file(host: &impl Host, file: &RestoreFile) -> Result<(), RollbackError> {
    match (&file.contents, &file.metadata) {
        (None, None) => {
            if host.read_protected(&file.path)?.is_some() || host.metadata(&file.path)?.is_some() {
                host.remove_protected(&file.path)?;
            }
        }
        (Some(contents), Some(metadata)) => {
            host.replace_file_atomic(
                &file.path,
                contents,
                &FileMetadata {
                    uid: metadata.uid,
                    gid: metadata.gid,
                    mode: metadata.mode,
                    is_file: true,
                    is_dir: false,
                },
            )?;
        }
        _ => {
            return Err(RollbackError::Refused(
                "backup file preimage has inconsistent metadata".into(),
            ));
        }
    }
    Ok(())
}

fn files_in_restore_order(backup: &ValidatedBackup) -> Vec<&RestoreFile> {
    let mut files = backup.files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

fn restore_service<H: Host>(
    host: &H,
    backup: &ValidatedBackup,
    expected: &ServiceSnapshot,
    options: &RollbackOptions,
) -> Result<bool, RollbackError> {
    let (mut active, mut enabled) = query_service(host, &expected.unit)?;
    let mut changed = false;
    if enabled != expected.enabled {
        restore_enablement(host, &expected.unit, &enabled, &expected.enabled)?;
        let observed = query_service(host, &expected.unit)?;
        active = observed.0;
        enabled = observed.1;
        if enabled != expected.enabled {
            return Err(RollbackError::Service {
                unit: expected.unit.clone(),
                detail: format!("enablement remains {enabled}"),
            });
        }
        changed = true;
    }

    let expected_active = if expected.active == "failed" {
        "inactive"
    } else {
        expected.active.as_str()
    };
    if active == expected_active {
        return Ok(changed);
    }
    if expected_active == "active" && !restored_config_present(host)? {
        return Err(RollbackError::Service {
            unit: expected.unit.clone(),
            detail: "the backed-up NUT configuration is absent; rollback will not start a driver or monitor".into(),
        });
    }
    if expected_active == "active" || stop_needs_online_evidence(host, &expected.unit)? {
        ensure_safe_online(host, &backup.summary.stamp, options)?;
    }
    let verb = if expected_active == "active" {
        "start"
    } else {
        "stop"
    };
    run_checked(
        host,
        "systemctl",
        &[verb, &expected.unit],
        "restore service activity",
    )?;
    let observed = query_service(host, &expected.unit)?.0;
    if observed != expected_active {
        return Err(RollbackError::Service {
            unit: expected.unit.clone(),
            detail: format!("requested {expected_active}, observed {observed}"),
        });
    }
    changed = true;
    Ok(changed)
}

fn restore_enablement<H: Host>(
    host: &H,
    unit: &str,
    current: &str,
    desired: &str,
) -> Result<(), RollbackError> {
    if matches!(current, "masked" | "masked-runtime")
        && !matches!(desired, "masked" | "masked-runtime")
    {
        let args = if current == "masked-runtime" {
            vec!["--runtime", "unmask", unit]
        } else {
            vec!["unmask", unit]
        };
        run_checked(host, "systemctl", &args, "restore service masking")?;
    }
    let (verb, runtime) = match desired {
        "enabled" => ("enable", false),
        "enabled-runtime" => ("enable", true),
        "disabled" => ("disable", false),
        "disabled-runtime" => ("disable", true),
        "masked" => ("mask", false),
        "masked-runtime" => ("mask", true),
        "static" | "indirect" | "generated" | "transient" | "linked" | "linked-runtime"
        | "alias" | "not-found" => {
            return Err(RollbackError::Service {
                unit: unit.to_owned(),
                detail: format!(
                    "enablement is {current}; cannot restore its recorded {desired} state"
                ),
            });
        }
        other => {
            return Err(RollbackError::Service {
                unit: unit.to_owned(),
                detail: format!("unrecognized recorded enablement {other}"),
            });
        }
    };
    let args = if runtime {
        vec!["--runtime", verb, unit]
    } else {
        vec![verb, unit]
    };
    run_checked(host, "systemctl", &args, "restore service enablement")?;
    Ok(())
}

fn service_rank(left: &ServiceSnapshot, right: &ServiceSnapshot) -> std::cmp::Ordering {
    service_activity_rank(left)
        .cmp(&service_activity_rank(right))
        .then_with(|| service_enablement_rank(left).cmp(&service_enablement_rank(right)))
}

fn service_activity_rank(service: &ServiceSnapshot) -> usize {
    if service.active == "active" {
        10 + match service.unit.as_str() {
            unit if unit.starts_with("nut-driver@") => 0,
            "nut-server.service" => 1,
            "nut-monitor.service" => 2,
            "nut.target" => 3,
            _ => 4,
        }
    } else {
        match service.unit.as_str() {
            "nut-monitor.service" => 0,
            "nut-server.service" => 1,
            unit if unit.starts_with("nut-driver@") => 2,
            "nut.target" => 3,
            _ => 4,
        }
    }
}

fn service_enablement_rank(_service: &ServiceSnapshot) -> usize {
    0
}

/// Whether stopping `unit` happens while upsmon could act on it. Stopping
/// upsmon itself, or losing the driver or server under a running upsmon (a
/// communications loss it acts on), needs a fresh OL reading. Once the
/// monitor is down no shutdown authority remains, so the server, driver, and
/// target stop without one: upsd, the only evidence source, may already have
/// been stopped by this rollback. Any monitor state other than inactive or
/// failed counts as running.
fn stop_needs_online_evidence<H: Host>(host: &H, unit: &str) -> Result<bool, RollbackError> {
    if unit == MONITOR_UNIT {
        return Ok(true);
    }
    let monitor = process_text(host, "systemctl", &["is-active", MONITOR_UNIT])?;
    Ok(!matches!(monitor.as_str(), "inactive" | "failed"))
}

fn ensure_safe_online<H: Host>(
    host: &H,
    stamp: &str,
    options: &RollbackOptions,
) -> Result<(), RollbackError> {
    let state = read_state_record(host)?;
    let ups_name = state
        .as_ref()
        .and_then(|state| state.managed_identities.get("ups"))
        .map(String::as_str)
        .ok_or_else(|| RollbackError::UnsafeUps("the applied state has no UPS identity".into()))?;
    let observation = host.observe_ups(&options.upsd_address, options.upsd_port, ups_name);
    if let Some(error) = observation.vars_error {
        return Err(RollbackError::UnsafeUps(format!(
            "fresh status is unavailable ({error}); resume with `nut rollback {stamp}`"
        )));
    }
    let status = observation.vars.get("ups.status").ok_or_else(|| {
        RollbackError::UnsafeUps(format!(
            "ups.status is unavailable; resume with `nut rollback {stamp}`"
        ))
    })?;
    let flags: BTreeSet<_> = status.split_whitespace().collect();
    if !flags.contains("OL")
        || flags.contains("OB")
        || flags.contains("LB")
        || flags.contains("FSD")
        || status.contains("DATA-STALE")
        || status.contains("DRIVER-NOT-CONNECTED")
    {
        return Err(RollbackError::UnsafeUps(format!(
            "UPS reports {status}; resume with `nut rollback {stamp}`"
        )));
    }
    Ok(())
}

fn restored_config_present(host: &impl Host) -> Result<bool, RollbackError> {
    Ok(host.read_protected(Path::new(LIVE_UPS_CONF))?.is_some())
}

fn query_service<H: Host>(host: &H, unit: &str) -> Result<(String, String), RollbackError> {
    let active = process_text(host, "systemctl", &["is-active", unit])?;
    let enabled = process_text(host, "systemctl", &["is-enabled", unit])?;
    if !matches!(active.as_str(), "active" | "inactive" | "failed") {
        return Err(RollbackError::Service {
            unit: unit.to_owned(),
            detail: format!("active state is unavailable ({active})"),
        });
    }
    if !matches!(
        enabled.as_str(),
        "enabled"
            | "enabled-runtime"
            | "disabled"
            | "disabled-runtime"
            | "static"
            | "indirect"
            | "masked"
            | "masked-runtime"
            | "generated"
            | "transient"
            | "linked"
            | "linked-runtime"
            | "alias"
            | "not-found"
    ) {
        return Err(RollbackError::Service {
            unit: unit.to_owned(),
            detail: format!("enablement state is unavailable ({enabled})"),
        });
    }
    Ok((active, enabled))
}

fn process_text<H: Host>(host: &H, program: &str, args: &[&str]) -> Result<String, RollbackError> {
    let output = run(host, program, args)?;
    let text = String::from_utf8(output.stdout)
        .map_err(|_| RollbackError::Refused(format!("{program} returned invalid text")))?;
    Ok(text.trim().to_owned())
}

fn run_checked<H: Host>(
    host: &H,
    program: &str,
    args: &[&str],
    description: &str,
) -> Result<ProcessOutput, RollbackError> {
    let output = run(host, program, args)?;
    if output.status != Some(0) {
        return Err(RollbackError::Refused(format!(
            "could not {description} ({} exited {})",
            program,
            output.status.unwrap_or(-1)
        )));
    }
    Ok(output)
}

fn run<H: Host>(host: &H, program: &str, args: &[&str]) -> Result<ProcessOutput, RollbackError> {
    let args = args.iter().map(OsString::from).collect::<Vec<_>>();
    Ok(host.run_bounded(&ProcessRequest {
        program: OsStr::new(program),
        args: &args,
        timeout: PROCESS_TIMEOUT,
        secret: false,
        inherit_tty: false,
    })?)
}

fn group_record<H: Host>(
    host: &H,
    group: &str,
) -> Result<Option<(u32, BTreeSet<String>)>, RollbackError> {
    let output = run(host, "getent", &["group", group])?;
    match output.status {
        Some(2) => return Ok(None),
        Some(0) => {}
        status => {
            return Err(RollbackError::Refused(format!(
                "cannot inspect group {group} (getent exited {})",
                status.unwrap_or(-1)
            )));
        }
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| RollbackError::Refused(format!("group {group} has invalid text")))?;
    let line = text.trim_end();
    if line.contains('\n') {
        return Err(RollbackError::Refused(format!(
            "group {group} lookup is ambiguous"
        )));
    }
    let fields: Vec<_> = line.split(':').collect();
    if fields.len() != 4 || fields[0] != group {
        return Err(RollbackError::Refused(format!(
            "group {group} record is malformed"
        )));
    }
    let gid = fields[2]
        .parse::<u32>()
        .map_err(|_| RollbackError::Refused(format!("group {group} has an invalid ID")))?;
    let members = fields[3]
        .split(',')
        .filter(|member| !member.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(Some((gid, members)))
}

fn group_exists(host: &impl Host, group: &str) -> Result<bool, RollbackError> {
    Ok(group_record(host, group)?.is_some())
}

fn group_has_member(host: &impl Host, group: &str, user: &str) -> Result<bool, RollbackError> {
    Ok(group_record(host, group)?.is_some_and(|(_, members)| members.contains(user)))
}

fn group_is_unused<H: Host>(host: &H, group: &str) -> Result<bool, RollbackError> {
    let Some((gid, members)) = group_record(host, group)? else {
        return Ok(false);
    };
    if !members.is_empty() {
        return Ok(false);
    }
    let output = run(host, "getent", &["passwd"])?;
    if output.status != Some(0) {
        return Err(RollbackError::Refused(
            "cannot verify whether the created operator group is unused".into(),
        ));
    }
    let users = String::from_utf8(output.stdout)
        .map_err(|_| RollbackError::Refused("passwd database has invalid text".into()))?;
    for line in users.lines() {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() == 7 && fields[3].parse::<u32>().ok() == Some(gid) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn membership_key(addition: &MembershipAddition) -> String {
    format!("{}:{}", addition.group, addition.user)
}

fn read_progress(
    host: &impl Host,
    backup: &ValidatedBackup,
) -> Result<Option<RollbackProgress>, RollbackError> {
    let path = backup_path(&backup.summary.stamp).join(ROLLBACK_JOURNAL_NAME);
    let bytes = host.read_protected(&path)?;
    let metadata = host.metadata(&path)?;
    let progress = match (bytes, metadata) {
        (None, None) => return Ok(None),
        (Some(bytes), Some(metadata)) => {
            verify_private_file(&path, &metadata)?;
            if bytes.len() > MAX_ROLLBACK_BYTES {
                return Err(RollbackError::Refused(
                    "rollback progress record exceeds its size limit".into(),
                ));
            }
            serde_json::from_slice::<RollbackProgress>(&bytes)
                .map_err(|_| RollbackError::Refused("rollback progress record is corrupt".into()))?
        }
        _ => {
            return Err(RollbackError::Refused(
                "rollback progress contents and metadata disagree".into(),
            ));
        }
    };
    validate_progress(&progress, backup)?;
    Ok(Some(progress))
}

fn validate_progress(
    progress: &RollbackProgress,
    backup: &ValidatedBackup,
) -> Result<(), RollbackError> {
    if progress.schema_version != 1 || progress.stamp != backup.summary.stamp {
        return Err(RollbackError::Refused(
            "rollback progress record does not match its backup".into(),
        ));
    }
    let files: BTreeSet<_> = backup.files.iter().map(|file| file.path.clone()).collect();
    if !progress.completed_files.is_subset(&files) {
        return Err(RollbackError::Refused(
            "rollback progress names an unknown file".into(),
        ));
    }
    let memberships: BTreeSet<_> = backup
        .summary
        .membership_additions
        .iter()
        .map(membership_key)
        .collect();
    if !progress.completed_memberships.is_subset(&memberships) {
        return Err(RollbackError::Refused(
            "rollback progress names an unknown membership".into(),
        ));
    }
    let services: BTreeSet<_> = backup
        .summary
        .services
        .iter()
        .map(|service| service.unit.clone())
        .collect();
    if !progress.completed_services.is_subset(&services) {
        return Err(RollbackError::Refused(
            "rollback progress names an unknown service".into(),
        ));
    }
    let groups: BTreeSet<_> = backup
        .summary
        .created_groups
        .iter()
        .map(|group| format!("group:{group}"))
        .collect();
    if !progress.groups_to_remove.is_subset(&groups)
        || !progress
            .completed_groups
            .is_subset(&progress.groups_to_remove)
    {
        return Err(RollbackError::Refused(
            "rollback progress names an unknown group".into(),
        ));
    }
    if progress.complete && !all_progress_complete(progress, backup) {
        return Err(RollbackError::Refused(
            "rollback progress is marked complete but has unfinished work".into(),
        ));
    }
    Ok(())
}

fn progress_complete(progress: &RollbackProgress, backup: &ValidatedBackup) -> bool {
    progress.complete && all_progress_complete(progress, backup)
}

fn all_progress_complete(progress: &RollbackProgress, backup: &ValidatedBackup) -> bool {
    progress.completed_files.len() == backup.files.len()
        && progress.completed_memberships.len() == backup.summary.membership_additions.len()
        && progress.completed_services.len() == backup.summary.services.len()
        && progress.completed_groups.len() == progress.groups_to_remove.len()
}

fn save_progress(
    host: &impl Host,
    stamp: &str,
    progress: &RollbackProgress,
) -> Result<(), RollbackError> {
    let bytes = serde_json::to_vec_pretty(progress)
        .map_err(|_| RollbackError::Refused("cannot encode rollback progress".into()))?;
    if bytes.len() > MAX_ROLLBACK_BYTES {
        return Err(RollbackError::Refused(
            "rollback progress record exceeds its size limit".into(),
        ));
    }
    host.replace_file_atomic(
        &backup_path(stamp).join(ROLLBACK_JOURNAL_NAME),
        &bytes,
        &FileMetadata::file(0, 0, 0o600),
    )?;
    Ok(())
}

fn verify_private_file(path: &Path, metadata: &FileMetadata) -> Result<(), RollbackError> {
    if !metadata.is_file
        || metadata.is_dir
        || metadata.uid != 0
        || metadata.gid != 0
        || metadata.mode != 0o600
    {
        return Err(RollbackError::Refused(format!(
            "rollback progress file {} has unsafe metadata",
            path.display()
        )));
    }
    Ok(())
}

fn backup_path(stamp: &str) -> PathBuf {
    Path::new(BACKUP_ROOT).join(stamp)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{FakeHost, Host, HostEffect, ProcessOutput, UpsObservation};
    use crate::init::{
        BeginRequest, OutputRecord, StateRecord, TransactionOutcome, begin, finalize,
    };
    use std::collections::BTreeMap;
    use std::time::{Duration, SystemTime};

    const TARGET: &str = "/etc/nut/ups.conf";
    const TARGET_META: FileMetadata = FileMetadata {
        uid: 0,
        gid: 17,
        mode: 0o640,
        is_file: true,
        is_dir: false,
    };
    const TARGET_META_RECORD: FileMetadataRecord = FileMetadataRecord {
        uid: 0,
        gid: 17,
        mode: 0o640,
    };

    fn base(contents: Option<&[u8]>) -> FakeHost {
        let host = FakeHost::default()
            .with_euid(0)
            .at(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))
            .with_entropy((1u8..=64).collect::<Vec<_>>());
        match contents {
            Some(contents) => host
                .with_file(TARGET, contents)
                .with_metadata(TARGET, TARGET_META),
            None => host,
        }
    }

    fn complete_init(
        host: &FakeHost,
        after: Option<&[u8]>,
        services: Vec<ServiceSnapshot>,
        membership_additions: Vec<MembershipAddition>,
        created_groups: Vec<String>,
        actual_created_groups: Vec<String>,
    ) -> String {
        let tx = begin(
            host,
            &BeginRequest {
                plan_hash: "a".repeat(64),
                targets: vec![PathBuf::from(TARGET), PathBuf::from(STATE_PATH)],
                services,
                membership_additions,
                created_groups,
                operations: Vec::new(),
            },
        )
        .unwrap()
        .unwrap();
        match after {
            Some(bytes) => host
                .replace_file_atomic(Path::new(TARGET), bytes, &TARGET_META)
                .unwrap(),
            None => {
                if host.read_protected(Path::new(TARGET)).unwrap().is_some() {
                    host.remove_protected(Path::new(TARGET)).unwrap();
                }
            }
        }
        for group in &actual_created_groups {
            super::super::transaction::record_created_group(host, &tx, group).unwrap();
        }
        let mut outputs = BTreeMap::new();
        let mut absent_outputs = BTreeSet::new();
        if let Some(bytes) = after {
            outputs.insert(
                PathBuf::from(TARGET),
                OutputRecord {
                    sha256: sha256(bytes),
                    metadata: TARGET_META_RECORD,
                },
            );
        } else {
            absent_outputs.insert(PathBuf::from(TARGET));
        }
        let state = StateRecord {
            schema_version: 1,
            config_hash: "b".repeat(64),
            profile_hash: "c".repeat(64),
            outputs,
            absent_outputs,
            managed_identities: BTreeMap::from([("ups".to_owned(), "cp1500".to_owned())]),
            created_groups: actual_created_groups,
            backup_reference: tx.stamp.clone(),
            transaction_outcome: TransactionOutcome::Applied,
        };
        finalize(host, &tx, &state).unwrap();
        let stamp = tx.stamp.clone();
        drop(tx);
        stamp
    }

    fn output(status: i32, text: &str) -> ProcessOutput {
        ProcessOutput {
            status: Some(status),
            stdout: text.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn ups(status: &str) -> UpsObservation {
        UpsObservation {
            vars: BTreeMap::from([("ups.status".to_owned(), status.to_owned())]),
            ..UpsObservation::default()
        }
    }

    #[test]
    fn rollback_restores_bytes_metadata_and_absent_state_and_is_idempotent() {
        let host = base(Some(b"pre-init private bytes"));
        let stamp = complete_init(
            &host,
            Some(b"new managed bytes"),
            Vec::new(),
            vec![],
            vec![],
            vec![],
        );

        let preview = plan(&host, Some(&stamp)).unwrap();
        assert_eq!(preview.stamp, stamp);
        assert_eq!(preview.files.len(), 2);
        assert!(preview.package_boundary.contains("retained"));
        assert!(!format!("{preview:?}").contains("pre-init private bytes"));

        let report = restore(&host, Some(&stamp), &RollbackOptions::default()).unwrap();
        assert_eq!(
            report.files_restored,
            vec![PathBuf::from(TARGET), PathBuf::from(STATE_PATH)]
        );
        assert_eq!(
            host.read_protected(Path::new(TARGET)).unwrap().as_deref(),
            Some(&b"pre-init private bytes"[..])
        );
        assert_eq!(host.metadata(Path::new(TARGET)).unwrap(), Some(TARGET_META));
        assert_eq!(host.read_protected(Path::new(STATE_PATH)).unwrap(), None);
        assert!(host.path_exists(&backup_path(&stamp)).unwrap());

        let repeated = restore(&host, Some(&stamp), &RollbackOptions::default()).unwrap();
        assert!(repeated.already_complete);
        assert!(repeated.files_restored.is_empty());
    }

    #[test]
    fn rollback_removes_targets_that_were_absent_before_init() {
        let host = base(None);
        let stamp = complete_init(
            &host,
            Some(b"created by init"),
            Vec::new(),
            vec![],
            vec![],
            vec![],
        );

        let report = restore(&host, Some(&stamp), &RollbackOptions::default()).unwrap();
        assert_eq!(host.read_protected(Path::new(TARGET)).unwrap(), None);
        assert_eq!(host.metadata(Path::new(TARGET)).unwrap(), None);
        assert!(report.files_restored.contains(&PathBuf::from(TARGET)));
    }

    #[test]
    fn rollback_resumes_after_file_restore_before_progress_checkpoint() {
        let host = base(Some(b"before"));
        let stamp = complete_init(&host, Some(b"after"), Vec::new(), vec![], vec![], vec![]);
        host.fail_after("replace_file", 2, "simulated interruption after restore");
        assert!(restore(&host, Some(&stamp), &RollbackOptions::default()).is_err());
        assert_eq!(
            host.read_protected(Path::new(TARGET)).unwrap().as_deref(),
            Some(&b"before"[..])
        );

        let restarted = host.snapshot_filesystem();
        let report = restore(&restarted, Some(&stamp), &RollbackOptions::default()).unwrap();
        assert!(!report.already_complete);
        assert_eq!(
            restarted
                .read_protected(Path::new(TARGET))
                .unwrap()
                .as_deref(),
            Some(&b"before"[..])
        );
        assert_eq!(
            restarted.read_protected(Path::new(STATE_PATH)).unwrap(),
            None
        );
    }

    #[test]
    fn latest_selection_is_exact_and_a_corrupt_latest_backup_is_not_skipped() {
        let host = base(Some(b"before"));
        let first = complete_init(&host, Some(b"first"), Vec::new(), vec![], vec![], vec![]);
        let second = complete_init(&host, Some(b"second"), Vec::new(), vec![], vec![], vec![]);
        assert_eq!(plan(&host, None).unwrap().stamp, second);
        assert!(matches!(
            plan(&host, Some(&first)),
            Err(RollbackError::NotCurrent(_))
        ));

        let manifest_path = backup_path(&second).join("manifest.json");
        let bytes = host.read_protected(&manifest_path).unwrap().unwrap();
        let contents = String::from_utf8(bytes).unwrap();
        let tampered = contents.replace(TARGET, "/etc/nut/../shadow");
        host.replace_file_atomic(
            &manifest_path,
            tampered.as_bytes(),
            &FileMetadata::file(0, 0, 0o600),
        )
        .unwrap();
        assert!(plan(&host, None).is_err());
    }

    #[test]
    fn rollback_removes_only_recorded_membership_and_preserves_other_users() {
        let host = base(Some(b"before"));
        let member = MembershipAddition {
            user: "alice".to_owned(),
            group: "nut-operators".to_owned(),
        };
        let stamp = complete_init(
            &host,
            Some(b"after"),
            Vec::new(),
            vec![member.clone()],
            vec![],
            vec![],
        );
        let host = host
            .queue_program(
                "getent",
                ["group", "nut-operators"],
                output(0, "nut-operators:x:100:alice,bob\n"),
            )
            .queue_program("gpasswd", ["-d", "alice", "nut-operators"], output(0, ""))
            .queue_program(
                "getent",
                ["group", "nut-operators"],
                output(0, "nut-operators:x:100:bob\n"),
            );

        let report = restore(&host, Some(&stamp), &RollbackOptions::default()).unwrap();
        assert_eq!(report.memberships_removed, vec![member]);
        let effects = host.effects();
        assert!(effects.iter().any(|effect| matches!(effect, HostEffect::Run { program, args } if program == OsStr::new("gpasswd") && args == &[OsString::from("-d"), OsString::from("alice"), OsString::from("nut-operators")])));
        assert!(!effects.iter().any(|effect| matches!(effect, HostEffect::Run { program, .. } if program == OsStr::new("apt-get") || program == OsStr::new("userdel"))));
    }

    #[test]
    fn rollback_removes_a_created_operator_group_only_when_unused() {
        let host = base(Some(b"before"));
        let stamp = complete_init(
            &host,
            Some(b"after"),
            Vec::new(),
            vec![],
            vec!["nut-operators".to_owned()],
            vec!["nut-operators".to_owned()],
        );
        let host = host
            .queue_program(
                "getent",
                ["group", "nut-operators"],
                output(0, "nut-operators:x:100:\n"),
            )
            .queue_program(
                "getent",
                ["group", "nut-operators"],
                output(0, "nut-operators:x:100:\n"),
            )
            .queue_program(
                "getent",
                ["passwd"],
                output(0, "alice:x:1000:1000::/:/bin/sh\n"),
            )
            .queue_program("groupdel", ["nut-operators"], output(0, ""))
            .queue_program("getent", ["group", "nut-operators"], output(2, ""));

        let report = restore(&host, Some(&stamp), &RollbackOptions::default()).unwrap();
        assert_eq!(report.groups_removed, vec!["nut-operators"]);
        assert!(report.groups_retained.is_empty());
    }

    #[test]
    fn rollback_retains_a_created_group_that_still_has_users() {
        let host = base(Some(b"before"));
        let stamp = complete_init(
            &host,
            Some(b"after"),
            Vec::new(),
            vec![],
            vec!["nut-operators".to_owned()],
            vec!["nut-operators".to_owned()],
        );
        let host = host
            .queue_program(
                "getent",
                ["group", "nut-operators"],
                output(0, "nut-operators:x:100:bob\n"),
            )
            .queue_program(
                "getent",
                ["group", "nut-operators"],
                output(0, "nut-operators:x:100:bob\n"),
            )
            .queue_program(
                "getent",
                ["group", "nut-operators"],
                output(0, "nut-operators:x:100:bob\n"),
            );

        let report = restore(&host, Some(&stamp), &RollbackOptions::default()).unwrap();
        assert!(report.groups_removed.is_empty());
        assert_eq!(report.groups_retained, vec!["nut-operators"]);
        assert!(!host.effects().iter().any(|effect| matches!(effect, HostEffect::Run { program, .. } if program == OsStr::new("groupdel"))));
    }

    #[test]
    fn unsafe_ups_refuses_service_restore_and_safe_retry_resumes() {
        let service = ServiceSnapshot {
            unit: "nut-monitor.service".to_owned(),
            active: "active".to_owned(),
            enabled: "disabled".to_owned(),
        };
        let host = base(Some(b"before"))
            .with_program_args(
                "systemctl",
                ["is-active", "nut-monitor.service"],
                output(3, "inactive\n"),
            )
            .with_program_args(
                "systemctl",
                ["is-enabled", "nut-monitor.service"],
                output(1, "disabled\n"),
            )
            .with_ups_observation(ups("OB DISCHRG"));
        let stamp = complete_init(&host, Some(b"after"), vec![service], vec![], vec![], vec![]);

        let error = restore(&host, Some(&stamp), &RollbackOptions::default()).unwrap_err();
        assert!(matches!(error, RollbackError::UnsafeUps(_)));
        assert!(host.effects().iter().all(|effect| !matches!(effect, HostEffect::Run { program, args } if program == OsStr::new("systemctl") && args.first().is_some_and(|arg| arg == "start"))));
        assert_eq!(
            host.read_protected(Path::new(TARGET)).unwrap().as_deref(),
            Some(&b"before"[..])
        );

        let restarted = host
            .snapshot_filesystem()
            .with_ups_observation(ups("OL CHRG"))
            .queue_program(
                "systemctl",
                ["is-active", "nut-monitor.service"],
                output(3, "inactive\n"),
            )
            .queue_program(
                "systemctl",
                ["is-enabled", "nut-monitor.service"],
                output(1, "disabled\n"),
            )
            .queue_program("systemctl", ["start", "nut-monitor.service"], output(0, ""))
            .queue_program(
                "systemctl",
                ["is-active", "nut-monitor.service"],
                output(0, "active\n"),
            )
            .queue_program(
                "systemctl",
                ["is-enabled", "nut-monitor.service"],
                output(1, "disabled\n"),
            );
        let report = restore(&restarted, Some(&stamp), &RollbackOptions::default()).unwrap();
        assert_eq!(report.services_changed, vec!["nut-monitor.service"]);
        assert_eq!(
            restarted.read_protected(Path::new(STATE_PATH)).unwrap(),
            None
        );
    }

    fn inactive_snapshot(unit: &str) -> ServiceSnapshot {
        ServiceSnapshot {
            unit: unit.to_owned(),
            active: "inactive".to_owned(),
            enabled: "enabled".to_owned(),
        }
    }

    fn stops_effects(host: &FakeHost) -> Vec<String> {
        host.effects()
            .iter()
            .filter_map(|effect| match effect {
                HostEffect::Run { program, args }
                    if program == OsStr::new("systemctl")
                        && args.first().is_some_and(|arg| arg == "stop") =>
                {
                    args.get(1).map(|unit| unit.to_string_lossy().into_owned())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rollback_stops_server_and_driver_after_the_monitor_without_upsd() {
        // Before init every unit was inactive. Only the monitor stop needs a
        // fresh OL reading; once upsmon is down, rollback stops upsd and then
        // the driver although upsd no longer answers.
        let driver = "nut-driver@cp1500.service";
        let mut host = base(Some(b"before")).with_ups_observation(ups("OL CHRG"));
        for unit in [MONITOR_UNIT, "nut-server.service", driver] {
            host = host
                .queue_program("systemctl", ["is-active", unit], output(0, "active\n"))
                .queue_program("systemctl", ["is-enabled", unit], output(0, "enabled\n"));
            if unit != MONITOR_UNIT {
                host = host.queue_program(
                    "systemctl",
                    ["is-active", MONITOR_UNIT],
                    output(3, "inactive\n"),
                );
            }
            host = host
                .queue_program("systemctl", ["stop", unit], output(0, ""))
                .queue_program("systemctl", ["is-active", unit], output(3, "inactive\n"))
                .queue_program("systemctl", ["is-enabled", unit], output(0, "enabled\n"));
        }
        let services = vec![
            inactive_snapshot(driver),
            inactive_snapshot("nut-server.service"),
            inactive_snapshot(MONITOR_UNIT),
        ];
        let stamp = complete_init(&host, Some(b"after"), services, vec![], vec![], vec![]);

        let report = restore(&host, Some(&stamp), &RollbackOptions::default())
            .unwrap_or_else(|error| panic!("rollback failed: {error:?}"));
        assert_eq!(
            report.services_changed,
            vec![MONITOR_UNIT, "nut-server.service", driver]
        );
        assert_eq!(
            stops_effects(&host),
            vec![MONITOR_UNIT, "nut-server.service", driver]
        );
        assert_eq!(
            host.effects()
                .iter()
                .filter(|effect| matches!(effect, HostEffect::ObserveUps { .. }))
                .count(),
            1
        );
        assert_eq!(
            host.read_protected(Path::new(TARGET)).unwrap().as_deref(),
            Some(&b"before"[..])
        );
    }

    #[test]
    fn stopping_the_driver_under_a_running_monitor_needs_fresh_evidence() {
        // upsmon stays active (not part of this backup), so losing the driver
        // is a communications loss it acts on: an unreachable upsd refuses.
        let driver = "nut-driver@cp1500.service";
        let host = base(Some(b"before"))
            .queue_program("systemctl", ["is-active", driver], output(0, "active\n"))
            .queue_program("systemctl", ["is-enabled", driver], output(0, "enabled\n"))
            .queue_program(
                "systemctl",
                ["is-active", MONITOR_UNIT],
                output(0, "active\n"),
            );
        let stamp = complete_init(
            &host,
            Some(b"after"),
            vec![inactive_snapshot(driver)],
            vec![],
            vec![],
            vec![],
        );

        let error = restore(&host, Some(&stamp), &RollbackOptions::default()).unwrap_err();
        assert!(
            matches!(error, RollbackError::UnsafeUps(_)),
            "unexpected error: {error:?}"
        );
        assert!(stops_effects(&host).is_empty());
    }
}
