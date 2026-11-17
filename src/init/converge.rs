#![forbid(unsafe_code)]

//! Safe service convergence for `nut init`.
//!
//! [`converge`] validates the reviewed plan, coordinates transaction and
//! resource operations under the shared writer lock, applies the service
//! dependency graph under fresh safe-online checks, verifies with a fast
//! structured doctor pass, and finalizes the state record. It never spawns
//! `nut doctor` terminal output, never runs power commands, and never
//! overrides the online safety gate.
//!
//! Operation IDs match the reviewed plan step IDs, so the durable journal
//! stays aligned with the reviewed plan. File operations keep their exact
//! before/after fingerprints; every operation (including services) is an
//! external journal entry reconciled from live state after a crash.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use thiserror::Error;

use super::observe::{
    FSTAB_PATH, LEGACY_HOOK, LIVE_HOOK, LIVE_NUT_CONF, LIVE_UPS_CONF, LIVE_UPSD_USERS,
    LIVE_UPSMON_CONF, LIVE_UPSSCHED_CONF, OPERATOR_SECRET_PATH, PENDING_PATH, STATE_PATH,
    UpsEvidence, collect,
};
use super::plan::{Plan, PlanOptions, PlanStepKind, plan};
use super::resources::{
    Resource, ResourceError, apply_resource, bind_resource, reserve_resource, resume_resource,
};
use super::secrets::{escape_nut_secret, parse_monitor_password, parse_upsd_users};
use super::transaction::{
    BeginRequest, Checkpoint, CheckpointState, MembershipAddition, OperationIntent, OutputRecord,
    ServiceSnapshot, StateRecord, Transaction, TransactionError, TransactionOutcome,
    acquire_writer_lock, begin_locked, checkpoint, finalize, materialize_secret, operation_record,
    original_file, recover, recover_with_observations,
};
use crate::doctor::{DoctorReport, ReportOptions, report, report_excluding};
use crate::host::{FileMetadata, Host, HostError, ProcessOutput, ProcessRequest};
use crate::render::{OPERATOR_PLACEHOLDER, UPSMON_PLACEHOLDER};

const UPSD_ADDRESS: &str = "127.0.0.1";
const UPSD_PORT: u16 = 3493;
const SHORT_TIMEOUT: Duration = Duration::from_secs(10);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(60);
const SECRET_ENTROPY_BYTES: usize = 32;
/// Ubuntu's NUT 2.8.1 runs `upsd -F` and `upsmon -F` as `Type=simple`
/// units, so `systemctl start|restart` returns before upsd listens or upsmon
/// logs in. Readiness gaps get this bounded settle window (about 15 s);
/// explicit unsafe evidence never waits, and the final verdict is unchanged.
const SETTLE_ATTEMPTS: u32 = 30;
const SETTLE_INTERVAL: Duration = Duration::from_millis(500);
/// Doctor checks that only mean "a just-started unit is not ready yet".
/// `hw.online` joins them only when `hw.data` also failed (no reading at all);
/// an explicit OB/LB/FSD reading with data present never waits.
const SETTLING_CHECKS: &[&str] = &[
    "svc.driver",
    "svc.server",
    "svc.monitor",
    "svc.listen",
    "svc.client",
    "hw.data",
];

/// Recognized `systemctl is-enabled` states, matching the transaction manifest
/// validator. Anything else fails closed.
const RECOGNIZED_ENABLED: &[&str] = &[
    "enabled",
    "enabled-runtime",
    "disabled",
    "disabled-runtime",
    "static",
    "indirect",
    "masked",
    "masked-runtime",
    "generated",
    "transient",
    "linked",
    "linked-runtime",
    "alias",
    "not-found",
];

/// Options for [`converge`]. The expected hash must come from a reviewed plan;
/// converge recomputes the plan itself and never trusts a caller-supplied hash.
pub struct ConvergeOptions {
    pub expect_plan: String,
    pub config: Option<PathBuf>,
    pub ups: Option<String>,
    pub rotate_secrets: bool,
}

/// Outcome of a successful convergence run. Contains no secret material.
#[derive(Clone, Debug)]
pub struct ConvergeReport {
    pub plan_hash: String,
    pub backup_stamp: Option<String>,
    pub files_applied: Vec<PathBuf>,
    pub services_applied: Vec<String>,
    pub resumed: bool,
    pub state_finalized: bool,
    pub doctor: DoctorReport,
}

#[derive(Debug, Error)]
pub enum ConvergeError {
    #[error("refusing to converge as non-root; use the reviewed privileged apply flow")]
    NotRoot,
    #[error("init plan is incomplete and has no usable apply hash: {0}")]
    IncompletePlan(String),
    #[error("re-observed plan hash does not match the reviewed --expect-plan; refusing to write")]
    HashMismatch,
    #[error(
        "a pending transaction exists for a different plan hash; review a fresh plan instead of resuming"
    )]
    PendingConflict,
    #[error("UPS is not safely online: {0}")]
    UnsafeUps(String),
    #[error(
        "post-apply verification failed; backup {stamp} retained ({detail}); recover with `nut rollback {stamp}`"
    )]
    VerificationFailed { stamp: String, detail: String },
    #[error("already converged but verification is unhealthy: {0}")]
    Unhealthy(String),
    #[error("service effect failed for {unit}: {detail}")]
    ServiceFailed { unit: String, detail: String },
    #[error("service {unit} is in an unstable state: {state}")]
    UnstableService { unit: String, state: String },
    #[error("required service state is unavailable for {unit}: {detail}")]
    ServiceUnknown { unit: String, detail: String },
    #[error("another init or rollback holds the writer lock: {0}")]
    LockHeld(String),
    #[error("converge refused: {0}")]
    Refused(&'static str),
    #[error(transparent)]
    Transaction(#[from] TransactionError),
    #[error(transparent)]
    Resource(#[from] ResourceError),
    #[error(transparent)]
    Host(#[from] HostError),
}

/// Validate root, applicability, and the reviewed hash, then converge under
/// the shared writer lock. An empty plan performs no writes at all: no lock,
/// no backup, no entropy, no state change.
pub fn converge<H: Host>(
    host: &H,
    options: &ConvergeOptions,
) -> Result<ConvergeReport, ConvergeError> {
    if host.euid() != 0 {
        return Err(ConvergeError::NotRoot);
    }
    if host.read_protected(Path::new(PENDING_PATH))?.is_some() {
        return resume(host, options);
    }
    fresh(host, options)
}

/// Empty plans converge without consulting the expected hash: there are no
/// target mutations to bind, so a second identical init reports zero changes
/// instead of refusing on a stale hash.
fn empty_report(plan_hash: String, doctor: DoctorReport) -> ConvergeReport {
    ConvergeReport {
        plan_hash,
        backup_stamp: None,
        files_applied: Vec::new(),
        services_applied: Vec::new(),
        resumed: false,
        state_finalized: false,
        doctor,
    }
}

fn fresh<H: Host>(host: &H, options: &ConvergeOptions) -> Result<ConvergeReport, ConvergeError> {
    let observation = collect(host, options.config.as_deref(), options.ups.as_deref());
    let planned = plan(
        &observation,
        &PlanOptions {
            rotate_secrets: options.rotate_secrets,
        },
    );
    if planned.is_empty() {
        let hash = require_applicable(&planned)?;
        let doctor = verify(host, &observation);
        if !doctor.verified() {
            return Err(ConvergeError::Unhealthy(doctor_detail(&doctor)));
        }
        return Ok(empty_report(hash, doctor));
    }
    let hash = require_applicable(&planned)?;
    if hash != options.expect_plan {
        return Err(ConvergeError::HashMismatch);
    }
    let lock = acquire_lock(host)?;
    // Re-observe and re-validate under the lock before any target mutation,
    // entropy generation, or backup creation.
    let observation = collect(host, options.config.as_deref(), options.ups.as_deref());
    let planned = plan(
        &observation,
        &PlanOptions {
            rotate_secrets: options.rotate_secrets,
        },
    );
    if planned.is_empty() {
        let hash = require_applicable(&planned)?;
        let doctor = verify(host, &observation);
        if !doctor.verified() {
            return Err(ConvergeError::Unhealthy(doctor_detail(&doctor)));
        }
        return Ok(empty_report(hash, doctor));
    }
    let hash = require_applicable(&planned)?;
    if hash != options.expect_plan {
        return Err(ConvergeError::HashMismatch);
    }
    apply_fresh(host, options, &observation, &planned, lock)
}

/// Fast structured doctor pass shared by the no-op path and post-apply
/// verification. Observation sleeps stay opt-in; converge reuses the fast
/// path and never parses terminal output.
fn verify<H: Host>(host: &H, observation: &super::observe::Observation) -> DoctorReport {
    report(
        host,
        &ReportOptions {
            config_path: observation.config_path.as_deref(),
            ups: observation
                .resolved
                .as_ref()
                .map(|resolved| resolved.ups_name.as_str()),
            address: UPSD_ADDRESS,
            port: UPSD_PORT,
        },
    )
}

/// Resume an interrupted transaction whose journal survived a crash.
/// The pending plan hash must match the reviewed expectation; unrelated
/// changes require a fresh reviewed plan, not an automatic overwrite.
fn resume<H: Host>(host: &H, options: &ConvergeOptions) -> Result<ConvergeReport, ConvergeError> {
    let observation = collect(host, options.config.as_deref(), options.ups.as_deref());
    // First recovery pass lists the recorded operations without observations;
    // the fingerprints below reconcile each one against live state. Each
    // recovery call holds the writer lock only for its own duration; the
    // pending journal and its plan hash serialize retries.
    let listing = recover(host)?.ok_or(ConvergeError::Refused(
        "pending transaction vanished during recovery",
    ))?;
    drop(listing.transaction);
    let ids: Vec<String> = listing
        .operations
        .iter()
        .map(|operation| operation.id.clone())
        .collect();
    if ids.is_empty() {
        return Err(ConvergeError::Refused(
            "pending journal holds no operations",
        ));
    }
    let observations = resume_observations(host, &observation, &listing.operations)?;
    let recovered = recover_with_observations(host, &observations)?.ok_or(
        ConvergeError::Refused("pending transaction vanished during recovery"),
    )?;
    if recovered.transaction.plan_hash != options.expect_plan {
        return Err(ConvergeError::PendingConflict);
    }
    apply_resumed(host, options, &observation, recovered.transaction, &ids)
}

fn require_applicable(plan: &Plan) -> Result<String, ConvergeError> {
    if !plan.complete {
        return Err(ConvergeError::IncompletePlan(plan.explanations.join("; ")));
    }
    plan.hash.clone().ok_or(ConvergeError::IncompletePlan(
        "plan has no authoritative hash".into(),
    ))
}

fn acquire_lock<H: Host>(host: &H) -> Result<super::transaction::WriterLock, ConvergeError> {
    acquire_writer_lock(host).map_err(|error| match error {
        TransactionError::Host(host_error) => ConvergeError::LockHeld(host_error.to_string()),
        other => ConvergeError::Transaction(other),
    })
}

fn sha256hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn sha256bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest(value: &str) -> String {
    sha256hex(value)
}

/// Require OL and the absence of OB, LB, and FSD. Missing, stale, or
/// contradictory evidence refuses: an empty status, a missing status, a
/// read error, or a mixed OL/unsafe flag set never counts as safe.
/// OL with CHRG and other benign flags is allowed.
fn require_safe_online(
    vars: &BTreeMap<String, String>,
    vars_error: &Option<String>,
) -> Result<(), ConvergeError> {
    if vars_error.is_some() {
        return Err(ConvergeError::UnsafeUps(
            "UPS evidence is unavailable or stale".into(),
        ));
    }
    let Some(status) = vars.get("ups.status") else {
        return Err(ConvergeError::UnsafeUps("ups.status is unavailable".into()));
    };
    let flags: Vec<String> = status
        .split_whitespace()
        .map(|flag| flag.to_ascii_uppercase())
        .collect();
    if flags.is_empty() {
        return Err(ConvergeError::UnsafeUps("ups.status is empty".into()));
    }
    if !flags.iter().any(|flag| flag == "OL") {
        return Err(ConvergeError::UnsafeUps(format!(
            "UPS is not on line power: {status}"
        )));
    }
    if flags
        .iter()
        .any(|flag| matches!(flag.as_str(), "OB" | "LB" | "FSD"))
    {
        return Err(ConvergeError::UnsafeUps(format!(
            "UPS reports an unsafe online transition: {status}"
        )));
    }
    Ok(())
}

fn require_observation_safe(ups: &UpsEvidence) -> Result<(), ConvergeError> {
    require_safe_online(&ups.vars, &ups.vars_error)
}

/// Fresh safe-online check against the live UPS, used immediately before
/// disruptive service effects so a loss of OL between steps stops the apply.
/// Unavailable evidence (upsd not listening yet, stale or unconnected driver
/// data) is re-observed within the bounded settle window because the server
/// may have just been started; an explicit unsafe status refuses at once.
fn require_fresh_safe_online<H: Host>(host: &H, ups_name: &str) -> Result<(), ConvergeError> {
    let mut attempt = 1;
    loop {
        let observed = host.observe_ups(UPSD_ADDRESS, UPSD_PORT, ups_name);
        let result = require_safe_online(&observed.vars, &observed.vars_error);
        if result.is_err() && evidence_unavailable(&observed) && attempt < SETTLE_ATTEMPTS {
            host.sleep(SETTLE_INTERVAL);
            attempt += 1;
            continue;
        }
        return result;
    }
}

/// Whether a UPS reading carries no status evidence at all, as opposed to an
/// explicit (possibly unsafe) status.
fn evidence_unavailable(observed: &crate::host::UpsObservation) -> bool {
    observed.vars_error.is_some()
        || observed
            .vars
            .get("ups.status")
            .is_none_or(|status| status.trim().is_empty())
}

/// Whether every verification failure is a readiness gap of a just-started
/// unit rather than evidence of an unsafe or misconfigured host.
fn only_settling_failures(doctor: &DoctorReport) -> bool {
    let no_data = doctor
        .failures
        .iter()
        .any(|failure| failure.id == "hw.data");
    !doctor.failures.is_empty()
        && doctor.failures.iter().all(|failure| {
            SETTLING_CHECKS.contains(&failure.id.as_str()) || (no_data && failure.id == "hw.online")
        })
}

fn systemctl<H: Host>(
    host: &H,
    args: &[&str],
    timeout: Duration,
) -> Result<ProcessOutput, ConvergeError> {
    let owned: Vec<OsString> = args.iter().map(OsString::from).collect();
    Ok(host.run_bounded(&ProcessRequest {
        program: OsStr::new("systemctl"),
        args: &owned,
        timeout,
        secret: false,
        inherit_tty: false,
    })?)
}

fn systemctl_text<H: Host>(host: &H, args: &[&str]) -> Result<String, ConvergeError> {
    let output = systemctl(host, args, SHORT_TIMEOUT)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Query and normalize one unit. Transitional states refuse: effects cannot
/// be reconciled while a unit is activating or deactivating.
fn query_service<H: Host>(host: &H, unit: &str) -> Result<(String, String), ConvergeError> {
    let active_raw = systemctl_text(host, &["is-active", unit])?;
    if active_raw.is_empty() {
        return Err(ConvergeError::ServiceUnknown {
            unit: unit.into(),
            detail: "systemctl is-active returned no state".into(),
        });
    }
    let active = normalize_active(&active_raw, unit)?.to_owned();
    let enabled_raw = systemctl_text(host, &["is-enabled", unit])?;
    if enabled_raw.is_empty() {
        return Err(ConvergeError::ServiceUnknown {
            unit: unit.into(),
            detail: "systemctl is-enabled returned no state".into(),
        });
    }
    let enabled = normalize_enabled(&enabled_raw, unit)?.to_owned();
    Ok((active, enabled))
}

fn normalize_active(raw: &str, unit: &str) -> Result<&'static str, ConvergeError> {
    match raw {
        "active" => Ok("active"),
        "failed" => Ok("failed"),
        "inactive" | "unknown" | "not-found" => Ok("inactive"),
        transitional @ ("activating" | "deactivating" | "reloading" | "maintenance"
        | "refreshing") => Err(ConvergeError::UnstableService {
            unit: unit.into(),
            state: transitional.into(),
        }),
        other => Err(ConvergeError::ServiceUnknown {
            unit: unit.into(),
            detail: format!("unrecognized active state {other:?}"),
        }),
    }
}

fn normalize_enabled(raw: &str, unit: &str) -> Result<&'static str, ConvergeError> {
    RECOGNIZED_ENABLED
        .iter()
        .copied()
        .find(|state| *state == raw)
        .ok_or_else(|| ConvergeError::ServiceUnknown {
            unit: unit.into(),
            detail: format!("unrecognized enablement state {raw:?}"),
        })
}

fn service_fingerprint(unit: &str, active: &str, enabled: &str) -> String {
    digest(&format!("service:{unit}:active={active}:enabled={enabled}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceAction {
    Enable,
    Start,
    Restart,
}

#[derive(Clone, Debug)]
struct ServiceOp {
    id: String,
    unit: String,
    action: ServiceAction,
}

/// Parse one plan service step. Only the four managed units are accepted, and
/// `nut.target` is never started directly: enabling it is the only operation.
fn parse_service_step(id: &str, managed: &BTreeSet<String>) -> Result<ServiceOp, ConvergeError> {
    let (action, unit) = if let Some(unit) = id.strip_prefix("service:enable:") {
        (ServiceAction::Enable, unit)
    } else if let Some(unit) = id.strip_prefix("service:start:") {
        (ServiceAction::Start, unit)
    } else if let Some(unit) = id.strip_prefix("service:restart:") {
        (ServiceAction::Restart, unit)
    } else {
        return Err(ConvergeError::Refused("unknown service operation"));
    };
    if !managed.contains(unit) {
        return Err(ConvergeError::Refused(
            "service operation targets an unmanaged unit",
        ));
    }
    if unit == "nut.target" && action != ServiceAction::Enable {
        return Err(ConvergeError::Refused(
            "refusing to start nut.target directly; enable it only",
        ));
    }
    Ok(ServiceOp {
        id: id.into(),
        unit: unit.into(),
        action,
    })
}

fn managed_units(ups_name: &str) -> BTreeSet<String> {
    BTreeSet::from([
        format!("nut-driver@{ups_name}.service"),
        "nut-server.service".to_owned(),
        "nut-monitor.service".to_owned(),
        "nut.target".to_owned(),
    ])
}

/// Whether any protection unit was already running: only then may applying
/// disrupt the stack, and only under a fresh safe reading. An initially
/// absent stack can install, configure, start its driver and server to obtain
/// its first reading.
fn stack_configured(observation: &super::observe::Observation) -> bool {
    ["nut-driver@", "nut-server.service", "nut-monitor.service"]
        .iter()
        .any(|prefix| {
            observation.services.iter().any(|(unit, state)| {
                unit.starts_with(prefix) && state.active.as_deref() == Some("active")
            })
        })
}

/// One planned resource operation, in apply order. IDs match the reviewed
/// plan step IDs so the journal stays aligned with the reviewed plan.
#[derive(Clone, Debug)]
enum ResourceSpec {
    Package { name: String },
    Group,
    Membership { user: String, group: String },
    File { path: PathBuf },
    Fstab { mount: PathBuf },
    Device { sysfs: PathBuf },
    Retire,
}

impl ResourceSpec {
    fn id(&self) -> String {
        match self {
            Self::Package { name } => format!("package:{name}"),
            Self::Group => "group:nut-operators".to_owned(),
            Self::Membership { user, group } => format!("membership:{group}:{user}"),
            Self::File { path } => format!("file:{}", path.display()),
            Self::Fstab { mount } => format!("fstab:{}", mount.display()),
            Self::Device { sysfs } => format!("device:{}", sysfs.display()),
            Self::Retire => format!("retirement:{LEGACY_HOOK}"),
        }
    }
}

struct PlannedWork {
    resources: Vec<ResourceSpec>,
    services: Vec<ServiceOp>,
    membership_additions: Vec<MembershipAddition>,
}

/// Decompose a reviewed plan into ordered operations. Apply order groups by
/// kind: packages and groups first (later steps need their gids), then
/// memberships, fstab, and device repairs, then files, then legacy
/// retirement, with services last in plan order.
fn decompose(plan: &Plan, ups_name: &str) -> Result<PlannedWork, ConvergeError> {
    let managed = managed_units(ups_name);
    let mut packages = Vec::new();
    let mut group = false;
    let mut memberships = Vec::new();
    let mut fstab_mounts = Vec::new();
    let mut device = None;
    let mut files = Vec::new();
    let mut retire = false;
    let mut services = Vec::new();
    let mut membership_additions = Vec::new();
    for step in &plan.steps {
        match step.kind {
            PlanStepKind::Package => {
                let name = step
                    .id
                    .strip_prefix("package:")
                    .ok_or(ConvergeError::Refused("malformed package step"))?;
                if name.is_empty() {
                    return Err(ConvergeError::Refused("malformed package step"));
                }
                packages.push(name.to_owned());
            }
            PlanStepKind::Membership => {
                if step.id == "group:nut-operators" {
                    group = true;
                } else if let Some(rest) = step.id.strip_prefix("membership:") {
                    let (group_name, user) = rest
                        .split_once(':')
                        .ok_or(ConvergeError::Refused("malformed membership step"))?;
                    if !matches!(group_name, "nut-operators" | "systemd-journal") || user.is_empty()
                    {
                        return Err(ConvergeError::Refused(
                            "membership step has an unmanaged group",
                        ));
                    }
                    memberships.push((group_name.to_owned(), user.to_owned()));
                    membership_additions.push(MembershipAddition {
                        user: user.to_owned(),
                        group: group_name.to_owned(),
                    });
                } else {
                    return Err(ConvergeError::Refused("malformed membership step"));
                }
            }
            PlanStepKind::Fstab => {
                let mount = step
                    .id
                    .strip_prefix("fstab:")
                    .ok_or(ConvergeError::Refused("malformed fstab step"))?;
                if mount.is_empty() {
                    return Err(ConvergeError::Refused("malformed fstab step"));
                }
                fstab_mounts.push(PathBuf::from(mount));
            }
            PlanStepKind::DeviceRepair => {
                let sysfs = step
                    .id
                    .strip_prefix("device:")
                    .ok_or(ConvergeError::Refused("malformed device step"))?;
                if sysfs.is_empty() {
                    return Err(ConvergeError::Refused("malformed device step"));
                }
                device = Some(PathBuf::from(sysfs));
            }
            PlanStepKind::File => {
                let path = step
                    .id
                    .strip_prefix("file:")
                    .ok_or(ConvergeError::Refused("malformed file step"))?;
                if path.is_empty() {
                    return Err(ConvergeError::Refused("malformed file step"));
                }
                files.push(PathBuf::from(path));
            }
            PlanStepKind::Retirement => {
                retire = true;
            }
            PlanStepKind::Service => {
                services.push(parse_service_step(&step.id, &managed)?);
            }
            PlanStepKind::State => {}
        }
    }
    let mut resources = Vec::new();
    for name in packages {
        resources.push(ResourceSpec::Package { name });
    }
    if group {
        resources.push(ResourceSpec::Group);
    }
    for (group_name, user) in memberships {
        resources.push(ResourceSpec::Membership {
            user,
            group: group_name,
        });
    }
    for mount in fstab_mounts {
        resources.push(ResourceSpec::Fstab { mount });
    }
    if let Some(sysfs) = device {
        resources.push(ResourceSpec::Device { sysfs });
    }
    for path in files {
        resources.push(ResourceSpec::File { path });
    }
    if retire {
        resources.push(ResourceSpec::Retire);
    }
    Ok(PlannedWork {
        resources,
        services,
        membership_additions,
    })
}

/// Operation specs rebuilt from journaled IDs for crash recovery.
fn spec_from_id(id: &str) -> Result<ResourceSpecOrService, ConvergeError> {
    if let Some(name) = id.strip_prefix("package:") {
        if name.is_empty() {
            return Err(ConvergeError::Refused("malformed package operation"));
        }
        return Ok(ResourceSpecOrService::Resource(ResourceSpec::Package {
            name: name.into(),
        }));
    }
    if id == "group:nut-operators" {
        return Ok(ResourceSpecOrService::Resource(ResourceSpec::Group));
    }
    if let Some(rest) = id.strip_prefix("membership:") {
        let (group, user) = rest
            .split_once(':')
            .ok_or(ConvergeError::Refused("malformed membership operation"))?;
        if !matches!(group, "nut-operators" | "systemd-journal") || user.is_empty() {
            return Err(ConvergeError::Refused(
                "membership operation has an unmanaged group",
            ));
        }
        return Ok(ResourceSpecOrService::Resource(ResourceSpec::Membership {
            user: user.into(),
            group: group.into(),
        }));
    }
    if let Some(path) = id.strip_prefix("file:") {
        if path.is_empty() {
            return Err(ConvergeError::Refused("malformed file operation"));
        }
        return Ok(ResourceSpecOrService::Resource(ResourceSpec::File {
            path: path.into(),
        }));
    }
    if let Some(mount) = id.strip_prefix("fstab:") {
        if mount.is_empty() {
            return Err(ConvergeError::Refused("malformed fstab operation"));
        }
        return Ok(ResourceSpecOrService::Resource(ResourceSpec::Fstab {
            mount: mount.into(),
        }));
    }
    if let Some(sysfs) = id.strip_prefix("device:") {
        if sysfs.is_empty() {
            return Err(ConvergeError::Refused("malformed device operation"));
        }
        return Ok(ResourceSpecOrService::Resource(ResourceSpec::Device {
            sysfs: sysfs.into(),
        }));
    }
    if id == format!("retirement:{LEGACY_HOOK}") {
        return Ok(ResourceSpecOrService::Resource(ResourceSpec::Retire));
    }
    if id.starts_with("service:") {
        let unit = id
            .rsplit(':')
            .next()
            .ok_or(ConvergeError::Refused("malformed service operation"))?;
        let managed: BTreeSet<String> = {
            // The unit allowlist is structural; the UPS name only scopes the
            // driver instance. Accept any well-formed driver instance here and
            // let the managed-set check in the fresh path cover planning.
            let mut set = BTreeSet::from([
                "nut-server.service".to_owned(),
                "nut-monitor.service".to_owned(),
                "nut.target".to_owned(),
            ]);
            if unit.starts_with("nut-driver@") && unit.ends_with(".service") {
                set.insert(unit.to_owned());
            }
            set
        };
        let op = parse_service_step(id, &managed)?;
        return Ok(ResourceSpecOrService::Service(op));
    }
    Err(ConvergeError::Refused("journal holds an unknown operation"))
}

enum ResourceSpecOrService {
    Resource(ResourceSpec),
    Service(ServiceOp),
}

/// Raw credential values with no Debug output. Surface bytes only through
/// explicit accessors that render them into managed files.
struct ResolvedSecrets {
    upsmon: Vec<u8>,
    operator: Vec<u8>,
}

/// Resolve adopted or generated credential values. Adopted values come from
/// the immutable backup preimages when present, falling back to live files
/// only when the target did not exist before this transaction. Generated
/// values materialize once into the backup directory; a retry reuses the
/// same blob instead of drawing new entropy.
fn secret_needs(paths: &BTreeSet<PathBuf>) -> (bool, bool) {
    let needs_upsmon = paths
        .iter()
        .any(|path| path == Path::new(LIVE_UPSD_USERS) || path == Path::new(LIVE_UPSMON_CONF));
    let needs_operator = paths
        .iter()
        .any(|path| path == Path::new(LIVE_UPSD_USERS) || path == Path::new(OPERATOR_SECRET_PATH));
    (needs_upsmon, needs_operator)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecretMode {
    /// Adopt the reviewed credential; a missing credential refuses instead
    /// of installing unreviewed secrets.
    Adopt,
    /// Materialize a fresh credential, reusing this transaction's blob.
    Generate,
    /// Resume path: reuse, adopt, then generate, because the crashed run may
    /// have been interrupted before recording which intent it served.
    AdoptOrGenerate,
}

/// Secret modes from the reviewed plan intents: generated and rotated
/// credentials materialize, adopted ones never draw entropy.
fn secret_modes(intents: &[super::plan::SecretIntentView]) -> (SecretMode, SecretMode) {
    let mut upsmon = SecretMode::Adopt;
    let mut operator = SecretMode::Adopt;
    for intent in intents {
        match intent {
            super::plan::SecretIntentView::GenerateUpsmon => upsmon = SecretMode::Generate,
            super::plan::SecretIntentView::GenerateOperator => operator = SecretMode::Generate,
            // Rotation replaces the credential pair together.
            super::plan::SecretIntentView::RotatePair => {
                upsmon = SecretMode::Generate;
                operator = SecretMode::Generate;
            }
            super::plan::SecretIntentView::AdoptUpsmon
            | super::plan::SecretIntentView::AdoptOperator => {}
        }
    }
    (upsmon, operator)
}

fn resolve_secrets<H: Host>(
    host: &H,
    transaction: &Transaction,
    observation: &super::observe::Observation,
    needs_upsmon: bool,
    needs_operator: bool,
    upsmon_mode: SecretMode,
    operator_mode: SecretMode,
) -> Result<ResolvedSecrets, ConvergeError> {
    // Reuse an already-materialized secret from this transaction first: a
    // retry resumes the same secret instead of drawing new entropy.
    let upsmon = if needs_upsmon {
        resolve_one(
            host,
            transaction,
            observation,
            super::transaction::SecretPurpose::Upsmon,
            upsmon_mode,
            adopt_upsmon,
        )?
    } else {
        Vec::new()
    };
    let operator = if needs_operator {
        resolve_one(
            host,
            transaction,
            observation,
            super::transaction::SecretPurpose::Operator,
            operator_mode,
            adopt_operator,
        )?
    } else {
        Vec::new()
    };
    if needs_upsmon && upsmon.is_empty() || needs_operator && operator.is_empty() {
        return Err(ConvergeError::Refused("a required credential is missing"));
    }
    Ok(ResolvedSecrets { upsmon, operator })
}

/// Resolve one credential: reuse an already-materialized secret from this
/// transaction, adopt the reviewed live credential, or materialize a new one
/// when the reviewed intent generates or rotates it. A failed adoption
/// refuses in [`SecretMode::Adopt`]: the reviewed credential changed, so
/// converge installs nothing unreviewed. Resume uses
/// [`SecretMode::AdoptOrGenerate`] because the crashed run may have been
/// interrupted before recording which intent it served.
fn resolve_one<H: Host>(
    host: &H,
    transaction: &Transaction,
    observation: &super::observe::Observation,
    purpose: super::transaction::SecretPurpose,
    mode: SecretMode,
    adopt: impl FnOnce(&H, &Transaction, &super::observe::Observation) -> Result<Vec<u8>, ConvergeError>,
) -> Result<Vec<u8>, ConvergeError> {
    use super::transaction::peek_secret;
    if let Some(existing) = peek_secret(host, transaction, purpose)? {
        return Ok(existing);
    }
    match mode {
        SecretMode::Generate => {}
        SecretMode::Adopt => return adopt(host, transaction, observation),
        SecretMode::AdoptOrGenerate => {
            if let Ok(adopted) = adopt(host, transaction, observation) {
                return Ok(adopted);
            }
        }
    }
    Ok(
        materialize_secret(host, transaction, purpose, SECRET_ENTROPY_BYTES)?
            .expose()
            .to_vec(),
    )
}

/// Preimage bytes for adoption: the immutable backup when the target is
/// backed up (absent markers yield no bytes), live files when the path was
/// never a backup target. Other backup errors fail closed.
fn preimage_or_live<H: Host>(
    host: &H,
    transaction: &Transaction,
    observation: &super::observe::Observation,
    path: &str,
) -> Result<Option<Vec<u8>>, ConvergeError> {
    match original_file(host, transaction, Path::new(path)) {
        Ok(None) => Ok(None),
        Ok(Some((bytes, _))) => Ok(Some(bytes)),
        Err(TransactionError::Invalid("resource is missing its backup")) => Ok(observation
            .live_files
            .get(path)
            .and_then(|state| state.present_bytes())
            .map(|bytes| bytes.to_vec())),
        Err(error) => Err(error.into()),
    }
}

/// Adopted upsmon value: prefer the immutable preimage, then live files. The
/// reviewed plan verified that both credential records agree.
fn adopt_upsmon<H: Host>(
    host: &H,
    transaction: &Transaction,
    observation: &super::observe::Observation,
) -> Result<Vec<u8>, ConvergeError> {
    if let Some(bytes) = preimage_or_live(host, transaction, observation, LIVE_UPSD_USERS)?
        && let Some(value) = parse_upsd_users(&String::from_utf8_lossy(&bytes)).upsmon
    {
        return Ok(value.value().as_bytes().to_vec());
    }
    let ups_name = observation
        .resolved
        .as_ref()
        .map(|resolved| resolved.ups_name.as_str())
        .unwrap_or("cp1500");
    for path in [LIVE_UPSD_USERS, LIVE_UPSMON_CONF] {
        if let Some(bytes) = preimage_or_live(host, transaction, observation, path)? {
            let text = String::from_utf8_lossy(&bytes);
            if let Some(value) = parse_upsd_users(&text).upsmon {
                return Ok(value.value().as_bytes().to_vec());
            }
            if let Ok(value) = parse_monitor_password(&text, ups_name) {
                return Ok(value.as_bytes().to_vec());
            }
        }
    }
    Err(ConvergeError::Refused(
        "no adopted upsmon credential exists",
    ))
}

/// Adopted operator value: prefer the immutable preimages, then live files.
/// The reviewed plan verified that both sources agree.
fn adopt_operator<H: Host>(
    host: &H,
    transaction: &Transaction,
    observation: &super::observe::Observation,
) -> Result<Vec<u8>, ConvergeError> {
    for path in [OPERATOR_SECRET_PATH, LIVE_UPSD_USERS] {
        if let Some(bytes) = preimage_or_live(host, transaction, observation, path)?
            && let Some(value) = operator_value(&bytes)
        {
            return Ok(value);
        }
    }
    Err(ConvergeError::Refused(
        "no adopted operator credential exists",
    ))
}

/// One password candidate from operator.secret or upsd.users bytes.
fn operator_value(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if !trimmed.is_empty() && !trimmed.contains('\n') {
        // operator.secret format: one password plus a newline.
        if parse_upsd_users(&text).operator.is_none() {
            return Some(trimmed.as_bytes().to_vec());
        }
    }
    parse_upsd_users(&text)
        .operator
        .map(|password| password.value().as_bytes().to_vec())
}

fn gid_of<H: Host>(host: &H, group: &str) -> Result<u32, ConvergeError> {
    let owned: Vec<OsString> = [OsString::from("group"), OsString::from(group)].to_vec();
    let output = host
        .run_bounded(&ProcessRequest {
            program: OsStr::new("getent"),
            args: &owned,
            timeout: SHORT_TIMEOUT,
            secret: true,
            inherit_tty: false,
        })
        .map_err(|_| ConvergeError::Refused("group lookup failed"))?;
    if output.status != Some(0) {
        return Err(ConvergeError::Refused("a managed group is missing"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<_> = text.trim_end().split(':').collect();
    if fields.len() != 4 || fields[0] != group {
        return Err(ConvergeError::Refused("invalid group record"));
    }
    fields[2]
        .parse()
        .map_err(|_| ConvergeError::Refused("invalid group id"))
}

fn managed_metadata<H: Host>(host: &H, path: &Path) -> Result<FileMetadata, ConvergeError> {
    if path == Path::new(LIVE_HOOK) {
        Ok(FileMetadata::file(0, 0, 0o755))
    } else if path == Path::new("/etc/nut-cli/nut.toml") {
        Ok(FileMetadata::file(0, 0, 0o644))
    } else if path == Path::new(OPERATOR_SECRET_PATH) {
        Ok(FileMetadata::file(0, gid_of(host, "nut-operators")?, 0o640))
    } else {
        Ok(FileMetadata::file(0, gid_of(host, "nut")?, 0o640))
    }
}

/// Desired bytes for one managed file. Secret-bearing files substitute the
/// resolved credentials into the rendered templates; every other file
/// installs its rendered bytes, except the host config which installs its
/// validated source bytes.
fn desired_file_bytes(
    path: &Path,
    rendered: &crate::render::RenderedFiles,
    secrets: &ResolvedSecrets,
    nut_toml_source: &[u8],
) -> Result<Vec<u8>, ConvergeError> {
    let escaped_upsmon = || {
        escape_nut_secret(
            std::str::from_utf8(&secrets.upsmon)
                .map_err(|_| ConvergeError::Refused("adopted credential is not valid text"))?,
        )
        .map_err(|_| ConvergeError::Refused("adopted credential cannot be safely rendered"))
    };
    let escaped_operator = || {
        escape_nut_secret(
            std::str::from_utf8(&secrets.operator)
                .map_err(|_| ConvergeError::Refused("adopted credential is not valid text"))?,
        )
        .map_err(|_| ConvergeError::Refused("adopted credential cannot be safely rendered"))
    };
    if path == Path::new(LIVE_UPSD_USERS) {
        Ok(rendered
            .upsd_users
            .replace(UPSMON_PLACEHOLDER, &escaped_upsmon()?)
            .replace(OPERATOR_PLACEHOLDER, &escaped_operator()?)
            .into_bytes())
    } else if path == Path::new(LIVE_UPSMON_CONF) {
        Ok(rendered
            .upsmon_conf
            .replace(UPSMON_PLACEHOLDER, &escaped_upsmon()?)
            .into_bytes())
    } else if path == Path::new(OPERATOR_SECRET_PATH) {
        let mut bytes = secrets.operator.clone();
        bytes.push(b'\n');
        Ok(bytes)
    } else if path == Path::new("/etc/nut-cli/nut.toml") {
        Ok(nut_toml_source.to_vec())
    } else if path == Path::new(LIVE_NUT_CONF) {
        Ok(rendered.nut_conf.as_bytes().to_vec())
    } else if path == Path::new(LIVE_UPS_CONF) {
        Ok(rendered.ups_conf.as_bytes().to_vec())
    } else if path == Path::new(LIVE_UPSSCHED_CONF) {
        Ok(rendered.upssched_conf.as_bytes().to_vec())
    } else if path == Path::new(LIVE_HOOK) {
        Ok(rendered.hook.as_bytes().to_vec())
    } else {
        Err(ConvergeError::Refused(
            "file operation targets an unmanaged path",
        ))
    }
}

/// Construct (but do not yet prepare) one resource. File payloads need the
/// resolved secrets and fresh group IDs; everything else builds from live
/// host state and the locked observation.
fn build_resource<H: Host>(
    host: &H,
    observation: &super::observe::Observation,
    secrets: &ResolvedSecrets,
    nut_toml_source: &[u8],
    spec: &ResourceSpec,
) -> Result<Resource, ConvergeError> {
    match spec {
        ResourceSpec::Package { name } => {
            let candidate = observation
                .packages
                .get(name)
                .and_then(|state| state.candidate_version.clone())
                .ok_or(ConvergeError::Refused("a package has no install candidate"))?;
            Ok(Resource::packages_resume(
                host,
                &BTreeMap::from([(name.clone(), candidate)]),
            )?)
        }
        ResourceSpec::Group => Ok(Resource::operators_group()),
        ResourceSpec::Membership { user, group } => Ok(Resource::membership(user, group)?),
        ResourceSpec::File { path } => {
            let rendered = observation
                .rendered
                .as_ref()
                .ok_or(ConvergeError::Refused("rendered files are unavailable"))?;
            let bytes = desired_file_bytes(path, rendered, secrets, nut_toml_source)?;
            Ok(Resource::file(
                path.clone(),
                bytes,
                managed_metadata(host, path)?,
            )?)
        }
        ResourceSpec::Fstab { mount } => {
            let before = observation
                .fstab
                .present_bytes()
                .ok_or(ConvergeError::Refused("/etc/fstab is unavailable"))?;
            let metadata = match &observation.fstab {
                super::observe::FileState::Present { metadata, .. } => metadata.clone(),
                _ => None,
            }
            .ok_or(ConvergeError::Refused("/etc/fstab metadata is unavailable"))?;
            Ok(Resource::fstab(
                before,
                std::slice::from_ref(mount),
                metadata,
            )?)
        }
        ResourceSpec::Device { sysfs } => {
            let resolved = observation
                .resolved
                .as_ref()
                .ok_or(ConvergeError::Refused("resolved config is unavailable"))?;
            let selected = observation
                .usb
                .selected
                .as_ref()
                .ok_or(ConvergeError::Refused("no USB device is selected"))?;
            let gid = gid_of(host, "nut")?;
            Ok(Resource::device(
                sysfs.clone(),
                &selected.vendor_hex(),
                &selected.product_hex(),
                resolved.ups.usb_match.serial.clone(),
                gid,
                false,
            )?
            .ok_or(ConvergeError::Refused("USB device repair is unavailable"))?)
        }
        ResourceSpec::Retire => {
            let hook = observation
                .legacy_hook
                .present_bytes()
                .ok_or(ConvergeError::Refused("legacy hook bytes are unavailable"))?
                .to_vec();
            let rendered = observation
                .rendered
                .as_ref()
                .ok_or(ConvergeError::Refused("rendered files are unavailable"))?;
            Ok(Resource::retire_legacy(
                hook,
                rendered.upssched_conf.as_bytes().to_vec(),
            ))
        }
    }
}

/// Drive one service operation to Completed. Records the outcome only after
/// verifying the unit actually reached the desired state; a successful exit
/// code alone never counts. Returns whether this run executed the effect.
/// Whether the unit already satisfies the operation's postcondition. Used
/// to skip redundant enables and to confirm completed operations still hold.
fn service_postcondition<H: Host>(host: &H, op: &ServiceOp) -> Result<bool, ConvergeError> {
    let (active, enabled) = query_service(host, &op.unit)?;
    Ok(match op.action {
        ServiceAction::Enable => matches!(enabled.as_str(), "enabled" | "enabled-runtime"),
        ServiceAction::Start | ServiceAction::Restart => active == "active",
    })
}

/// Journal fingerprint for a bound service operation. This marks execution
/// rather than live state: a restart leaves the same active/enabled values
/// behind, so the fingerprint can never equal a live reading. Reconciliation
/// is authoritative through checkpoints and postcondition checks, never
/// through fingerprint comparison.
fn execution_marker(id: &str) -> String {
    digest(&format!("service-executed:{id}"))
}

fn drive_service<H: Host>(
    host: &H,
    transaction: &Transaction,
    op: &ServiceOp,
    ups_name: &str,
    was_configured: bool,
) -> Result<bool, ConvergeError> {
    let (record, state) = operation_record(host, transaction, &op.id)?;
    if record == reserve_resource(&op.id) {
        // Never bound: a crash happened before binding, or this is the fresh
        // path binding now. A satisfied enable postcondition completes
        // without effects; restarts always execute so the unit picks up the
        // new configuration even when it was already active.
        if op.action == ServiceAction::Enable && service_postcondition(host, op)? {
            checkpoint(host, transaction, &op.id, Checkpoint::Started)?;
            checkpoint(host, transaction, &op.id, Checkpoint::Completed)?;
            return Ok(false);
        }
        let (active, enabled) = query_service(host, &op.unit)?;
        super::transaction::bind_resource_operation(
            host,
            transaction,
            &reserve_resource(&op.id),
            &OperationIntent {
                id: op.id.clone(),
                target: None,
                before: service_fingerprint(&op.unit, &active, &enabled),
                after: execution_marker(&op.id),
            },
            &[],
        )?;
        checkpoint(host, transaction, &op.id, Checkpoint::Started)?;
    } else {
        match state {
            CheckpointState::Completed => {
                if service_postcondition(host, op)? {
                    return Ok(false);
                }
                return Err(ConvergeError::Refused(
                    "a completed service operation drifted outside this transaction",
                ));
            }
            // Re-executing a started operation is safe: systemctl actions are
            // idempotent and the post-state is always verified.
            CheckpointState::Planned => {
                checkpoint(host, transaction, &op.id, Checkpoint::Started)?;
            }
            CheckpointState::Started => {}
        }
    }
    // Enabling is never disruptive. Monitor start/restart always needs a
    // fresh safe reading; driver/server restarts need one only when they
    // would disrupt an already configured stack. An install from empty may
    // start its driver and server to obtain its first reading.
    // The driver enumerator is a dependency of driver configuration, never an
    // unconditional restart: converge runs exactly the reviewed plan steps.
    let disruptive = match op.action {
        ServiceAction::Enable => false,
        ServiceAction::Start | ServiceAction::Restart => {
            op.unit == "nut-monitor.service" || (was_configured && op.unit != "nut.target")
        }
    };
    if disruptive {
        require_fresh_safe_online(host, ups_name)?;
    }
    let verb = match op.action {
        ServiceAction::Enable => "enable",
        ServiceAction::Start => "start",
        ServiceAction::Restart => "restart",
    };
    let output = systemctl(host, &[verb, &op.unit], SYSTEMCTL_TIMEOUT)?;
    if output.status != Some(0) {
        return Err(ConvergeError::ServiceFailed {
            unit: op.unit.clone(),
            detail: format!("systemctl {verb} exited {}", output.status.unwrap_or(-1)),
        });
    }
    let (active_after, enabled_after) = query_service(host, &op.unit)?;
    match op.action {
        ServiceAction::Enable => {
            if !matches!(enabled_after.as_str(), "enabled" | "enabled-runtime") {
                return Err(service_not_reached(op, &active_after, &enabled_after));
            }
        }
        ServiceAction::Start | ServiceAction::Restart => {
            if active_after != "active" {
                return Err(service_not_reached(op, &active_after, &enabled_after));
            }
        }
    }
    checkpoint(host, transaction, &op.id, Checkpoint::Completed)?;
    Ok(true)
}

fn service_not_reached(op: &ServiceOp, active: &str, enabled: &str) -> ConvergeError {
    ConvergeError::ServiceFailed {
        unit: op.unit.clone(),
        detail: format!("unit reached active={active} enabled={enabled}, not its desired state"),
    }
}

fn ups_name_of(observation: &super::observe::Observation) -> Result<&str, ConvergeError> {
    observation
        .resolved
        .as_ref()
        .map(|resolved| resolved.ups_name.as_str())
        .ok_or(ConvergeError::Refused("resolved config is unavailable"))
}

fn nut_toml_source<H: Host>(
    host: &H,
    observation: &super::observe::Observation,
) -> Result<Vec<u8>, ConvergeError> {
    let path = observation
        .config_path
        .as_deref()
        .ok_or(ConvergeError::Refused("host config path is unavailable"))?;
    host.read_file(path)
        .map_err(|_| ConvergeError::Refused("host config is unreadable"))?
        .ok_or(ConvergeError::Refused("host config is missing"))
}

/// Snapshot every managed unit for the backup manifest and compensation.
/// Transitional states refuse; unknown enablement records as `not-found`,
/// the stable systemd state for a unit file that is absent or unreadable.
fn snapshot_services(
    observation: &super::observe::Observation,
    ups_name: &str,
) -> Result<Vec<ServiceSnapshot>, ConvergeError> {
    let driver = format!("nut-driver@{ups_name}.service");
    let mut snapshots = Vec::new();
    for unit in [
        driver.as_str(),
        "nut-server.service",
        "nut-monitor.service",
        "nut.target",
    ] {
        let state = observation
            .services
            .get(unit)
            .ok_or(ConvergeError::ServiceUnknown {
                unit: unit.into(),
                detail: "service was not observed".into(),
            })?;
        if state.probe_failed.is_some() {
            return Err(ConvergeError::ServiceUnknown {
                unit: unit.into(),
                detail: "service probe failed".into(),
            });
        }
        let active_raw = state
            .active
            .as_deref()
            .ok_or(ConvergeError::ServiceUnknown {
                unit: unit.into(),
                detail: "service activity is unavailable".into(),
            })?;
        let active = normalize_active(active_raw, unit)?.to_owned();
        let enabled = match state.enabled.as_deref() {
            Some(raw) if RECOGNIZED_ENABLED.contains(&raw) => raw.to_owned(),
            Some(_) | None => "not-found".to_owned(),
        };
        snapshots.push(ServiceSnapshot {
            unit: unit.into(),
            active,
            enabled,
        });
    }
    Ok(snapshots)
}

/// Current fingerprints for every journaled target-less operation, so crash
/// recovery can reconcile them against live state. File operations reconcile
/// from disk and need no entry.
fn resume_observations<H: Host>(
    host: &H,
    observation: &super::observe::Observation,
    operations: &[super::transaction::OperationRecovery],
) -> Result<BTreeMap<String, String>, ConvergeError> {
    let secrets = ResolvedSecrets {
        upsmon: Vec::new(),
        operator: Vec::new(),
    };
    let nut_toml = Vec::new();
    let mut out = BTreeMap::new();
    for operation in operations {
        let id = operation.id.as_str();
        // Bound file operations reconcile from disk; only the never-bound
        // reservation placeholder needs no observation either.
        if id.starts_with("file:") || id.starts_with("fstab:") || id.starts_with("retirement:") {
            continue;
        }
        let fingerprint = match spec_from_id(id)? {
            ResourceSpecOrService::Service(op) => {
                let (active, enabled) = query_service(host, &op.unit)?;
                service_fingerprint(&op.unit, &active, &enabled)
            }
            ResourceSpecOrService::Resource(spec) => {
                let resource = build_resource(host, observation, &secrets, &nut_toml, &spec)?;
                resource.current_fingerprint(host)?
            }
        };
        out.insert(id.to_owned(), fingerprint);
    }
    Ok(out)
}

struct ApplyOutcome {
    files_applied: Vec<PathBuf>,
    services_applied: Vec<String>,
    snapshots: Vec<ServiceSnapshot>,
}

/// Fresh apply: begin one transaction for the locked plan, then drive every
/// operation, verify with doctor, and finalize the state record.
fn apply_fresh<H: Host>(
    host: &H,
    options: &ConvergeOptions,
    observation: &super::observe::Observation,
    plan: &Plan,
    lock: super::transaction::WriterLock,
) -> Result<ConvergeReport, ConvergeError> {
    let ups_name = ups_name_of(observation)?.to_owned();
    if stack_configured(observation) {
        require_observation_safe(&observation.ups)?;
    }
    let work = decompose(plan, &ups_name)?;
    for id in work
        .resources
        .iter()
        .map(ResourceSpec::id)
        .chain(work.services.iter().map(|op| op.id.clone()))
    {
        if id.len() > 64 {
            return Err(ConvergeError::Refused(
                "journal operation ID exceeds the size limit",
            ));
        }
    }
    // Targets are known before any resource is prepared: file paths come
    // from the plan, package files from the pre-begin package builders.
    let mut targets: BTreeSet<PathBuf> = BTreeSet::from([PathBuf::from(STATE_PATH)]);
    for spec in &work.resources {
        match spec {
            ResourceSpec::Package { name } => {
                let candidate = observation
                    .packages
                    .get(name)
                    .and_then(|state| state.candidate_version.clone())
                    .ok_or(ConvergeError::Refused("a package has no install candidate"))?;
                let resource =
                    Resource::packages_resume(host, &BTreeMap::from([(name.clone(), candidate)]))?;
                targets.extend(resource.backup_targets());
            }
            ResourceSpec::File { path } => {
                targets.insert(path.clone());
            }
            ResourceSpec::Fstab { .. } => {
                targets.insert(PathBuf::from(FSTAB_PATH));
            }
            ResourceSpec::Retire => {
                targets.insert(PathBuf::from(LEGACY_HOOK));
            }
            ResourceSpec::Group | ResourceSpec::Membership { .. } | ResourceSpec::Device { .. } => {
            }
        }
    }
    let snapshots = snapshot_services(observation, &ups_name)?;
    let operations: Vec<OperationIntent> = work
        .resources
        .iter()
        .map(ResourceSpec::id)
        .chain(work.services.iter().map(|op| op.id.clone()))
        .map(|id| reserve_resource(&id))
        .collect();
    let request = BeginRequest {
        plan_hash: options.expect_plan.clone(),
        targets: targets.into_iter().collect(),
        services: snapshots.clone(),
        membership_additions: work.membership_additions.clone(),
        created_groups: if work
            .resources
            .iter()
            .any(|spec| matches!(spec, ResourceSpec::Group))
        {
            vec!["nut-operators".to_owned()]
        } else {
            Vec::new()
        },
        operations,
    };
    let Some(transaction) = begin_locked(host, lock, &request)? else {
        return Err(ConvergeError::Refused("empty plan produced a transaction"));
    };
    let file_paths: BTreeSet<PathBuf> = work
        .resources
        .iter()
        .filter_map(|spec| match spec {
            ResourceSpec::File { path } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let (needs_upsmon, needs_operator) = secret_needs(&file_paths);
    let (upsmon_mode, operator_mode) = secret_modes(&plan.view().secret_intents);
    let secrets = resolve_secrets(
        host,
        &transaction,
        observation,
        needs_upsmon,
        needs_operator,
        upsmon_mode,
        operator_mode,
    )?;
    let nut_toml = nut_toml_source(host, observation)?;
    let mut outcome = ApplyOutcome {
        files_applied: Vec::new(),
        services_applied: Vec::new(),
        snapshots,
    };
    for spec in &work.resources {
        // Construction is read-only; the second build supplies the owned
        // resource that `prepare` consumes and `drive_resource` reconciles.
        let prepared = build_resource(host, observation, &secrets, &nut_toml, spec)?
            .prepare(host, &spec.id())?;
        let resource = build_resource(host, observation, &secrets, &nut_toml, spec)?;
        let executed = drive_resource(host, &transaction, spec, resource, prepared)?;
        if executed {
            match spec {
                ResourceSpec::File { path } => outcome.files_applied.push(path.clone()),
                ResourceSpec::Retire => outcome.files_applied.push(PathBuf::from(LEGACY_HOOK)),
                _ => {}
            }
        }
    }
    for op in &work.services {
        if drive_service(
            host,
            &transaction,
            op,
            &ups_name,
            stack_configured(observation),
        )? {
            outcome.services_applied.push(op.unit.clone());
        }
    }
    finish(
        host,
        &transaction,
        observation,
        &options.expect_plan,
        &outcome,
        false,
    )
}

/// Resumed apply: reconcile every journaled operation in journal order.
fn apply_resumed<H: Host>(
    host: &H,
    options: &ConvergeOptions,
    observation: &super::observe::Observation,
    transaction: Transaction,
    ids: &[String],
) -> Result<ConvergeReport, ConvergeError> {
    let ups_name = ups_name_of(observation)?.to_owned();
    if stack_configured(observation) {
        require_observation_safe(&observation.ups)?;
    }
    let file_paths: BTreeSet<PathBuf> = ids
        .iter()
        .filter_map(|id| spec_from_id(id).ok())
        .filter_map(|spec| match spec {
            ResourceSpecOrService::Resource(ResourceSpec::File { path }) => Some(path),
            _ => None,
        })
        .collect();
    let (needs_upsmon, needs_operator) = secret_needs(&file_paths);
    let secrets = resolve_secrets(
        host,
        &transaction,
        observation,
        needs_upsmon,
        needs_operator,
        SecretMode::AdoptOrGenerate,
        SecretMode::AdoptOrGenerate,
    )?;
    let nut_toml = nut_toml_source(host, observation)?;
    let snapshots = snapshot_services(observation, &ups_name)?;
    let mut outcome = ApplyOutcome {
        files_applied: Vec::new(),
        services_applied: Vec::new(),
        snapshots,
    };
    for id in ids {
        match spec_from_id(id)? {
            ResourceSpecOrService::Resource(spec) => {
                let prepared = build_resource(host, observation, &secrets, &nut_toml, &spec)?
                    .prepare(host, id)?;
                let resource = build_resource(host, observation, &secrets, &nut_toml, &spec)?;
                if drive_resource(host, &transaction, &spec, resource, prepared)? {
                    match &spec {
                        ResourceSpec::File { path } => outcome.files_applied.push(path.clone()),
                        ResourceSpec::Retire => {
                            outcome.files_applied.push(PathBuf::from(LEGACY_HOOK));
                        }
                        _ => {}
                    }
                }
            }
            ResourceSpecOrService::Service(op) => {
                if drive_service(
                    host,
                    &transaction,
                    &op,
                    &ups_name,
                    stack_configured(observation),
                )? {
                    outcome.services_applied.push(op.unit.clone());
                }
            }
        }
    }
    finish(
        host,
        &transaction,
        observation,
        &options.expect_plan,
        &outcome,
        true,
    )
}

/// Post-apply verification with a fast structured doctor pass, then state
/// finalization. On verification failure, compensate touched files and
/// service states from the recorded preimages, keep the backup and the
/// recovery record, and report the exact rollback command.
fn finish<H: Host>(
    host: &H,
    transaction: &Transaction,
    observation: &super::observe::Observation,
    expect_plan: &str,
    outcome: &ApplyOutcome,
    resumed: bool,
) -> Result<ConvergeReport, ConvergeError> {
    let ups_name = ups_name_of(observation)?.to_owned();
    // The state record and its finalized backup reference do not exist until
    // this verification succeeds, so those two post-init checks are not
    // applicable at this boundary. All checks run normally for later doctor
    // invocations and for no-op init plans.
    let verify_applied = || {
        report_excluding(
            host,
            &ReportOptions {
                config_path: observation.config_path.as_deref(),
                ups: observation
                    .resolved
                    .as_ref()
                    .map(|resolved| resolved.ups_name.as_str()),
                address: UPSD_ADDRESS,
                port: UPSD_PORT,
            },
            &["state.drift", "state.backup"],
        )
    };
    let mut doctor = verify_applied();
    // A just-started Type=simple upsd/upsmon may not be listening or logged
    // in yet. Re-verify within the settle window only while every failure is
    // such a readiness gap; any other failure compensates immediately.
    let mut attempt = 1;
    while attempt < SETTLE_ATTEMPTS && only_settling_failures(&doctor) {
        host.sleep(SETTLE_INTERVAL);
        doctor = verify_applied();
        attempt += 1;
    }
    if !doctor.verified() {
        let problems = compensate(
            host,
            transaction,
            &outcome.files_applied,
            &outcome.services_applied,
            &outcome.snapshots,
            &ups_name,
        );
        let mut detail = doctor_detail(&doctor);
        if problems.is_empty() {
            detail.push_str("; touched files and service states were restored from preimages");
        } else {
            detail.push_str("; compensation problems: ");
            detail.push_str(&problems.join("; "));
        }
        return Err(ConvergeError::VerificationFailed {
            stamp: transaction.stamp.clone(),
            detail,
        });
    }
    let created_groups = super::transaction::created_groups_for_transaction(host, transaction)?;
    let state = state_record(
        host,
        observation,
        transaction,
        &outcome.files_applied,
        &created_groups,
        resumed,
    )?;
    finalize(host, transaction, &state)?;
    Ok(ConvergeReport {
        plan_hash: expect_plan.to_owned(),
        backup_stamp: Some(transaction.stamp.clone()),
        files_applied: outcome.files_applied.clone(),
        services_applied: outcome.services_applied.clone(),
        resumed,
        state_finalized: true,
        doctor,
    })
}

fn doctor_detail(doctor: &DoctorReport) -> String {
    doctor
        .failures
        .iter()
        .map(|failure| {
            if failure.unavailable {
                format!(
                    "{}: required evidence unavailable ({})",
                    failure.id, failure.evidence
                )
            } else {
                format!("{}: {}", failure.id, failure.evidence)
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Restore touched files and service states from the recorded preimages.
/// Returns human-readable descriptions of anything that could not be
/// restored; the backup and the pending journal always stay in place so
/// `nut rollback <stamp>` can finish the recovery.
fn compensate<H: Host>(
    host: &H,
    transaction: &Transaction,
    files: &[PathBuf],
    services: &[String],
    snapshots: &[ServiceSnapshot],
    ups_name: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    for path in files.iter().rev() {
        match original_file(host, transaction, path) {
            Ok(None) => {
                // The target did not exist before this transaction.
                match host.path_exists(path) {
                    Ok(true) => {
                        if let Err(error) = host.remove_protected(path) {
                            problems.push(format!("cannot remove {}: {error}", path.display()));
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        problems.push(format!("cannot inspect {}: {error}", path.display()))
                    }
                }
            }
            Ok(Some((bytes, metadata))) => {
                if let Err(error) = host.replace_file_atomic(path, &bytes, &metadata) {
                    problems.push(format!("cannot restore {}: {error}", path.display()));
                }
            }
            Err(error) => problems.push(format!(
                "cannot read preimage of {}: {error}",
                path.display()
            )),
        }
    }
    let mut restored_units = BTreeSet::new();
    for unit in services.iter().rev() {
        if !restored_units.insert(unit.clone()) {
            continue;
        }
        let Some(snapshot) = snapshots.iter().find(|snapshot| &snapshot.unit == unit) else {
            problems.push(format!("no recorded service state for {unit}"));
            continue;
        };
        let (active, enabled) = match query_service(host, unit) {
            Ok(state) => state,
            Err(error) => {
                problems.push(format!("cannot re-observe {unit}: {error}"));
                continue;
            }
        };
        // Restore enablement first so a subsequent start observes the
        // recorded target state.
        if enabled != snapshot.enabled {
            let enable = matches!(snapshot.enabled.as_str(), "enabled" | "enabled-runtime");
            let verb = if enable { "enable" } else { "disable" };
            match systemctl(host, &[verb, unit], SHORT_TIMEOUT) {
                Ok(output) if output.status == Some(0) => {}
                Ok(output) => problems.push(format!(
                    "systemctl {verb} {unit} exited {}",
                    output.status.unwrap_or(-1)
                )),
                Err(error) => problems.push(format!("systemctl {verb} {unit} failed: {error}")),
            }
        }
        if active != snapshot.active {
            if snapshot.active == "active" {
                // Restarting a stopped prior monitor needs a fresh safe
                // reading. If safety prevents restoring its prior running
                // state, report that explicitly with the rollback command.
                if let Err(error) = require_fresh_safe_online(host, ups_name) {
                    problems.push(format!(
                        "cannot restore {unit} to running: {error}; recover with `nut rollback {}`",
                        transaction.stamp
                    ));
                    continue;
                }
                match systemctl(host, &["start", unit], SYSTEMCTL_TIMEOUT) {
                    Ok(output) if output.status == Some(0) => {}
                    Ok(output) => problems.push(format!(
                        "systemctl start {unit} exited {}",
                        output.status.unwrap_or(-1)
                    )),
                    Err(error) => problems.push(format!("systemctl start {unit} failed: {error}")),
                }
            } else if let Err(error) = systemctl(host, &["stop", unit], SYSTEMCTL_TIMEOUT) {
                problems.push(format!("systemctl stop {unit} failed: {error}"));
            }
        }
    }
    problems
}

/// Canonical profile identity for the state record: profile ID, driver,
/// USB identity, and delay budgets. No secrets, no timestamps.
fn profile_identity(observation: &super::observe::Observation) -> Result<String, ConvergeError> {
    let resolved = observation
        .resolved
        .as_ref()
        .ok_or(ConvergeError::Refused("resolved config is unavailable"))?;
    let mut usb_ids: Vec<String> = resolved
        .profile
        .usb_ids
        .iter()
        .map(|id| format!("{:04x}:{:04x}", id.vendor, id.product))
        .collect();
    usb_ids.sort();
    Ok(format!(
        "{}|{}|{}|{}|{}|{}s|{}s|{}",
        resolved.profile.id.as_str(),
        resolved.driver.value,
        resolved.port.value,
        resolved.vendorid.value,
        resolved.productid.value,
        resolved.offdelay.value.0.as_secs(),
        resolved.ondelay.value.0.as_secs(),
        usb_ids.join(","),
    ))
}

fn state_record<H: Host>(
    host: &H,
    observation: &super::observe::Observation,
    transaction: &Transaction,
    files: &[PathBuf],
    created_groups: &[String],
    resumed: bool,
) -> Result<StateRecord, ConvergeError> {
    let ups_name = ups_name_of(observation)?.to_owned();
    let installed = host
        .read_protected(Path::new("/etc/nut-cli/nut.toml"))?
        .ok_or(ConvergeError::Refused(
            "installed host config is missing after apply",
        ))?;
    let mut outputs = BTreeMap::new();
    let mut absent_outputs = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for path in files {
        if !seen.insert(path) || path == Path::new(STATE_PATH) {
            continue;
        }
        if let Some(bytes) = host.read_protected(path)? {
            let metadata = host
                .metadata(path)?
                .ok_or(ConvergeError::Refused("applied file metadata is missing"))?;
            outputs.insert(
                path.clone(),
                OutputRecord {
                    sha256: sha256bytes(&bytes),
                    metadata: (&metadata).into(),
                },
            );
        } else {
            absent_outputs.insert(path.clone());
        }
    }
    Ok(StateRecord {
        schema_version: 1,
        config_hash: sha256bytes(&installed),
        profile_hash: digest(&profile_identity(observation)?),
        outputs,
        absent_outputs,
        managed_identities: BTreeMap::from([("ups".to_owned(), ups_name)]),
        created_groups: created_groups.to_vec(),
        backup_reference: transaction.stamp.clone(),
        transaction_outcome: if resumed {
            TransactionOutcome::Recovered
        } else {
            TransactionOutcome::Applied
        },
    })
}

/// Drive one resource operation to Completed, reconciling crash-recovery
/// state from live disk. Returns whether this run performed the effect as
/// opposed to reconciling an already-completed operation.
fn drive_resource<H: Host>(
    host: &H,
    transaction: &Transaction,
    spec: &ResourceSpec,
    resource: Resource,
    prepared: Option<super::resources::PreparedResource>,
) -> Result<bool, ConvergeError> {
    let id = spec.id();
    if id.len() > 64 {
        return Err(ConvergeError::Refused(
            "journal operation ID exceeds the size limit",
        ));
    }
    let (record, state) = operation_record(host, transaction, &id)?;
    if record == reserve_resource(&id) {
        // Never bound: a crash happened before binding, or this is the fresh
        // path preparing to bind now.
        let Some(mut ready) = prepared else {
            // Already satisfied without ever being bound.
            checkpoint(host, transaction, &id, Checkpoint::Started)?;
            checkpoint(host, transaction, &id, Checkpoint::Completed)?;
            return Ok(false);
        };
        bind_resource(host, transaction, &mut ready)?;
        apply_resource(host, transaction, &ready)?;
        return Ok(true);
    }
    // Already bound: reconcile from live state first.
    let current = resource.current_fingerprint(host)?;
    match (state, current == record.after) {
        (CheckpointState::Completed, true) => Ok(false),
        (CheckpointState::Completed, false) => Err(ConvergeError::Refused(
            "a completed operation drifted outside this transaction",
        )),
        (CheckpointState::Started, true) => {
            if matches!(spec, ResourceSpec::Group) {
                super::transaction::record_created_group(host, transaction, "nut-operators")?;
            }
            checkpoint(host, transaction, &id, Checkpoint::Completed)?;
            Ok(false)
        }
        (CheckpointState::Planned, true) => {
            checkpoint(host, transaction, &id, Checkpoint::Started)?;
            checkpoint(host, transaction, &id, Checkpoint::Completed)?;
            Ok(false)
        }
        _ => {
            if prepared.is_none() {
                return Err(ConvergeError::Refused(
                    "a journaled operation is satisfied but disagrees with its record",
                ));
            }
            let resumed = resume_resource(host, transaction, &id, resource)?;
            apply_resource(host, transaction, &resumed)?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::observe::collect as collect_observation;
    use super::super::plan::{PlanOptions, plan as plan_init};
    use super::super::transaction::acquire_writer_lock;
    use super::*;
    use crate::host::{FakeHost, HostEffect, ProcessOutput, UpsObservation};
    use std::collections::BTreeMap;

    const SENTINEL: &str = "SENTINEL-ADOPTED-9f8e7d6c5b4a3c2d1e0f";

    fn out(status: i32, stdout: &str) -> ProcessOutput {
        ProcessOutput {
            status: Some(status),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn ups_vars(status: &str) -> UpsObservation {
        UpsObservation {
            vars: BTreeMap::from([
                ("ups.status".into(), status.into()),
                ("battery.charge".into(), "100".into()),
                ("battery.runtime".into(), "1800".into()),
                ("ups.delay.shutdown".into(), "60".into()),
                ("ups.delay.start".into(), "0".into()),
                ("ups.test.result".into(), "No test initiated".into()),
            ]),
            cmds: vec!["shutdown.return".into()],
            clients: vec!["127.0.0.1".into()],
            greeting: Some("Network UPS Tools upsd 2.8.1".into()),
            nut_version: Some("2.8.1".into()),
            ..UpsObservation::default()
        }
    }

    fn ol() -> UpsObservation {
        ups_vars("OL CHRG")
    }

    fn ob() -> UpsObservation {
        ups_vars("OB DISCHRG")
    }

    fn systemctl_active(host: FakeHost, unit: &str, state: &str) -> FakeHost {
        host.with_program_args("systemctl", ["is-active", unit], out(0, state))
    }

    fn systemctl_enabled(host: FakeHost, unit: &str, state: &str) -> FakeHost {
        host.with_program_args("systemctl", ["is-enabled", unit], out(0, state))
    }

    fn systemctl_show(host: FakeHost, unit: &str, active: &str) -> FakeHost {
        host.with_program_args(
            "systemctl",
            ["show", unit, "-p", "ActiveState", "-p", "NRestarts"],
            out(0, &format!("ActiveState={active}\nNRestarts=0\n")),
        )
    }

    /// Fully stubbed host: converged services, installed packages, valid USB
    /// identity, and every read-only doctor dependency. Managed NUT files
    /// start absent so the first converge has work to do.
    fn fixture_host() -> FakeHost {
        let mut host = FakeHost::default()
            .with_euid(0)
            .with_hostname("athena")
            .with_env("HOME", "/root")
            .with_env("NUT_CONFIG", "/etc/nut-cli/nut.toml")
            .with_env("LANG", "C.UTF-8")
            .with_env("TERM", "xterm")
            .with_current_exe("/usr/local/bin/nut")
            .with_metadata(
                "/usr/local/bin/nut",
                FileMetadata {
                    uid: 0,
                    gid: 0,
                    mode: 0o755,
                    is_file: true,
                    is_dir: false,
                },
            )
            .with_file(
                "/etc/nut-cli/nut.toml",
                include_bytes!("../../tests/fixtures/hosts/athena/nut.toml").to_vec(),
            )
            .with_file("/etc/group", b"nut:x:100:\nnut-operators:x:101:bryan\n".to_vec())
            .with_file(
                "/etc/passwd",
                b"bryan:x:1000:1000::/home/bryan:/bin/bash\n".to_vec(),
            )
            .with_file(
                "/etc/fstab",
                b"UUID=abc / ext4 defaults 0 1\nUUID=def /mnt/poseidon ext4 defaults,nofail 0 2\nUUID=ghi /mnt/hades ext4 defaults,nofail 0 2\n"
                    .to_vec(),
            )
            .with_file("/sys/bus/usb/devices/1-4/idVendor", b"0764".to_vec())
            .with_file("/sys/bus/usb/devices/1-4/idProduct", b"0601".to_vec())
            .with_file("/sys/bus/usb/devices/1-4/busnum", b"1".to_vec())
            .with_file("/sys/bus/usb/devices/1-4/devnum", b"4".to_vec())
            .with_file("/dev/bus/usb/001/004", b"".to_vec())
            .with_metadata(
                "/dev/bus/usb/001/004",
                FileMetadata { uid: 0, gid: 100, mode: 0o660, is_file: true, is_dir: false },
            )
            .with_program_args("dpkg-query", ["-W", "-f=${Version}", "nut-client"], out(0, "2.8.1-1"))
            .with_program_args("dpkg-query", ["-W", "-f=${Version}", "nut-server"], out(0, "2.8.1-1"))
            .with_program_args("getent", ["group", "nut"], out(0, "nut:x:100:\n"))
            .with_program_args(
                "getent",
                ["group", "nut-operators"],
                out(0, "nut-operators:x:101:bryan\n"),
            )
            .with_program("systemctl", out(0, ""))
            .with_program_args("pgrep", ["-x", "pwrstatd"], ProcessOutput {
                status: Some(1),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
            .with_program_args("findmnt", ["--verify"], out(0, ""))
            .with_program(
                "efibootmgr",
                out(0, "BootCurrent: 0002\nBootOrder: 0002,0001\nBoot0002* debian\nBoot0001* Windows Boot Manager\n"),
            )
            .with_file("/usr/sbin/sendmail", b"".to_vec())
            .with_dir("/var/log/journal")
            .with_file("/proc/sys/kernel/panic", b"0\n".to_vec())
            .with_file("/sys/class/dmi/id/board_vendor", b"ASUSTeK COMPUTER INC.\n".to_vec())
            .with_file("/sys/class/dmi/id/board_name", b"PRIME TRX40-PRO\n".to_vec())
            .with_file("/sys/class/dmi/id/bios_version", b"1101\n".to_vec())
            .with_file(
                "/proc/net/tcp",
                b"  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:0DA5 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000000000000000 0 0 0 10 -1\n"
                    .to_vec(),
            )
            .with_file(
                "/proc/net/tcp6",
                b"  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"
                    .to_vec(),
            );
        for unit in [
            "nut-driver@cp1500.service",
            "nut-server.service",
            "nut-monitor.service",
            "nut.target",
        ] {
            host = systemctl_active(host, unit, "active");
            host = systemctl_enabled(host, unit, "enabled");
        }
        for unit in [
            "nut-driver@cp1500.service",
            "nut-server.service",
            "nut-monitor.service",
        ] {
            host = systemctl_show(host, unit, "active");
        }
        host
    }

    fn with_observations(host: FakeHost, count: usize, observation: UpsObservation) -> FakeHost {
        let mut host = host;
        for _ in 0..count {
            host = host.with_ups_observation(observation.clone());
        }
        host
    }

    fn expect_plan(host: &FakeHost, rotate: bool) -> String {
        let observation = collect_observation(host, None, None);
        let planned = plan_init(
            &observation,
            &PlanOptions {
                rotate_secrets: rotate,
            },
        );
        assert!(
            planned.complete,
            "plan incomplete: {:?}",
            planned.explanations
        );
        planned.hash.expect("complete plan has a hash")
    }

    fn options(hash: String, rotate: bool) -> ConvergeOptions {
        ConvergeOptions {
            expect_plan: hash,
            config: None,
            ups: None,
            rotate_secrets: rotate,
        }
    }

    fn has_effect(effects: &[HostEffect], matches: impl Fn(&HostEffect) -> bool) -> bool {
        effects.iter().any(matches)
    }

    fn is_power_command(effect: &HostEffect) -> bool {
        matches!(effect, HostEffect::Run { program, .. } if {
            let program = program.to_string_lossy();
            [
                "upscmd", "upsset", "upsrw", "upslog", "upssched", "upsdrvctl", "usbhid-ups",
                "dummy-ups",
            ]
            .iter()
            .any(|name| program == *name)
        })
    }

    #[test]
    fn safe_online_gate_cases() {
        let ok = |status: &str| {
            require_safe_online(
                &BTreeMap::from([("ups.status".to_owned(), status.to_owned())]),
                &None,
            )
        };
        assert!(ok("OL CHRG").is_ok());
        assert!(ok("OL").is_ok());
        assert!(ok("OL BYPASS").is_ok());
        assert!(ok("OB DISCHRG").is_err());
        assert!(ok("OL OB").is_err());
        assert!(ok("OL LB").is_err());
        assert!(ok("OL FSD").is_err());
        assert!(ok("LB").is_err());
        assert!(ok("").is_err());
        assert!(
            require_safe_online(&BTreeMap::new(), &None)
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );
        assert!(
            require_safe_online(
                &BTreeMap::from([("ups.status".to_owned(), "OL".to_owned())]),
                &Some("stale reading".to_owned()),
            )
            .is_err()
        );
    }

    #[test]
    fn non_root_refuses_before_any_write() {
        let host = FakeHost::default().with_euid(1000);
        let error = converge(&host, &options("a".repeat(64), false)).unwrap_err();
        assert!(matches!(error, ConvergeError::NotRoot));
        assert!(host.effects().is_empty());
    }

    #[test]
    fn hash_mismatch_writes_nothing() {
        let host = with_observations(fixture_host(), 2, ol());
        let error = converge(&host, &options("b".repeat(64), false)).unwrap_err();
        assert!(matches!(error, ConvergeError::HashMismatch));
        let effects = host.effects();
        assert!(!has_effect(&effects, |effect| matches!(
            effect,
            HostEffect::Lock { .. }
                | HostEffect::ReplaceFile { .. }
                | HostEffect::Random { .. }
                | HostEffect::CreateDir { .. }
        )));
    }

    #[test]
    fn fresh_install_converges_and_second_init_is_noop() {
        // One UPS reading for the reviewed plan, one per collect, one per
        // service gate, one for verification, and two for the no-op rerun.
        let host = with_observations(fixture_host(), 9, ol());
        let hash = expect_plan(&host, false);
        let report = converge(&host, &options(hash.clone(), false))
            .unwrap_or_else(|error| panic!("converge failed: {error:?}"));
        assert_eq!(report.plan_hash, hash);
        assert!(report.backup_stamp.is_some());
        assert!(!report.resumed);
        assert!(report.state_finalized);
        assert!(report.doctor.verified(), "{:?}", report.doctor.failures);
        // Seven managed files: the host config needs no copy because the
        // source and the installed destination are identical.
        assert_eq!(report.files_applied.len(), 7, "{:?}", report.files_applied);
        assert_eq!(
            report.services_applied.len(),
            3,
            "{:?}",
            report.services_applied
        );
        assert!(!has_effect(&host.effects(), is_power_command));

        for path in [
            "/etc/nut/nut.conf",
            "/etc/nut/ups.conf",
            "/etc/nut/upsd.users",
            "/etc/nut/upsmon.conf",
            "/etc/nut/upssched.conf",
        ] {
            let bytes = host
                .read_protected(std::path::Path::new(path))
                .unwrap()
                .expect("managed file is missing");
            assert!(!bytes.is_empty(), "{path}");
            assert_eq!(
                host.metadata(std::path::Path::new(path)).unwrap(),
                Some(FileMetadata::file(0, 100, 0o640)),
                "{path}"
            );
        }
        // The host config source doubles as the installed file, so its
        // source metadata is preserved.
        assert_eq!(
            host.metadata(std::path::Path::new("/etc/nut-cli/nut.toml"))
                .unwrap(),
            Some(FileMetadata::file(0, 0, 0o644))
        );
        let hook = host
            .read_protected(std::path::Path::new("/usr/local/sbin/nut-upssched-cmd"))
            .unwrap()
            .expect("hook is missing");
        assert!(!hook.is_empty());
        assert_eq!(
            host.metadata(std::path::Path::new("/usr/local/sbin/nut-upssched-cmd"))
                .unwrap(),
            Some(FileMetadata::file(0, 0, 0o755))
        );
        let secret = host
            .read_protected(std::path::Path::new("/etc/nut-cli/operator.secret"))
            .unwrap()
            .expect("operator secret is missing");
        assert_eq!(secret.len(), 65, "64 hex chars plus a newline");
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::PENDING_PATH))
                .unwrap()
                .is_none()
        );
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::STATE_PATH))
                .unwrap()
                .is_some()
        );
        // One stamp draw plus one draw per generated secret.
        assert_eq!(
            host.effects()
                .iter()
                .filter(|effect| matches!(effect, HostEffect::Random { .. }))
                .count(),
            3
        );

        let before = host.effects().len();
        let rerun = converge(&host, &options(hash, false))
            .unwrap_or_else(|error| panic!("no-op rerun failed: {error:?}"));
        assert!(rerun.backup_stamp.is_none());
        assert!(rerun.files_applied.is_empty());
        assert!(rerun.services_applied.is_empty());
        assert!(!rerun.state_finalized);
        // Zero target writes, unit restarts, generated secrets, backups,
        // memberships, udev actions, and state mtime changes: no writes at
        // all beyond read-only queries.
        for effect in &host.effects()[before..] {
            assert!(
                !matches!(
                    effect,
                    HostEffect::ReplaceFile { .. }
                        | HostEffect::Remove { .. }
                        | HostEffect::CreateDir { .. }
                        | HostEffect::SetMetadata { .. }
                        | HostEffect::Lock { .. }
                        | HostEffect::Random { .. }
                        | HostEffect::WriteFile { .. }
                ),
                "no-op rerun wrote: {effect:?}"
            );
        }
    }

    #[test]
    fn adopted_credentials_repair_grants_without_rotation() {
        let rendered = {
            let probe = with_observations(fixture_host(), 1, ol());
            let observation = collect_observation(&probe, None, None);
            observation.rendered.expect("rendered files").clone()
        };
        let upsd_users = rendered
            .upsd_users
            .replace(UPSMON_PLACEHOLDER, SENTINEL)
            .replace(OPERATOR_PLACEHOLDER, SENTINEL)
            // Legacy grant the renderer removes.
            .replace("[nut-operator]\n", "[nut-operator]\n    actions = SET\n");
        let upsmon_conf = rendered.upsmon_conf.replace(UPSMON_PLACEHOLDER, SENTINEL);
        let meta = FileMetadata::file(0, 100, 0o640);
        let host = with_observations(fixture_host(), 7, ol())
            .with_file("/etc/nut/upsd.users", upsd_users.into_bytes())
            .with_metadata("/etc/nut/upsd.users", meta.clone())
            .with_file("/etc/nut/upsmon.conf", upsmon_conf.into_bytes())
            .with_metadata("/etc/nut/upsmon.conf", meta)
            .with_file(
                "/etc/nut-cli/operator.secret",
                format!("{SENTINEL}\n").into_bytes(),
            )
            .with_metadata(
                "/etc/nut-cli/operator.secret",
                FileMetadata::file(0, 101, 0o640),
            );
        let hash = expect_plan(&host, false);
        let report = converge(&host, &options(hash, false))
            .unwrap_or_else(|error| panic!("converge failed: {error:?}"));
        assert!(report.doctor.verified(), "{:?}", report.doctor.failures);
        // Only the backup stamp drew entropy: both credentials were adopted,
        // not generated.
        assert_eq!(
            host.effects()
                .iter()
                .filter(|effect| matches!(effect, HostEffect::Random { .. }))
                .count(),
            1
        );
        let installed = String::from_utf8(
            host.read_protected(std::path::Path::new("/etc/nut/upsd.users"))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(installed.contains(SENTINEL), "adopted password is retained");
        assert!(!installed.contains("actions"), "legacy grant is repaired");
        assert!(installed.contains("test.battery.start.quick"));
    }

    #[test]
    fn rotate_secrets_regenerates_both_credentials() {
        let host = with_observations(fixture_host(), 7, ol());
        let hash = expect_plan(&host, true);
        let report = converge(&host, &options(hash, true))
            .unwrap_or_else(|error| panic!("converge failed: {error:?}"));
        assert!(report.doctor.verified(), "{:?}", report.doctor.failures);
        let operator = String::from_utf8(
            host.read_protected(std::path::Path::new("/etc/nut-cli/operator.secret"))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(operator.len(), 65);
        let trimmed = operator.trim().to_owned();
        assert!(trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let upsd_users = String::from_utf8(
            host.read_protected(std::path::Path::new("/etc/nut/upsd.users"))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(
            upsd_users.contains(&trimmed),
            "rotated pair is installed consistently"
        );
    }

    #[test]
    fn unsafe_online_transition_refuses_before_mutation() {
        let host = with_observations(fixture_host(), 3, ob());
        let hash = expect_plan(&host, false);
        let error = converge(&host, &options(hash, false)).unwrap_err();
        assert!(
            matches!(error, ConvergeError::UnsafeUps(_)),
            "unexpected error: {error:?}"
        );
        // The pre-apply gate runs before any target mutation: no file
        // writes and no disruptive systemctl actions.
        assert!(!has_effect(&host.effects(), |effect| matches!(
            effect,
            HostEffect::ReplaceFile { .. }
                | HostEffect::Remove { .. }
                | HostEffect::CreateDir { .. }
                | HostEffect::Random { .. }
        )));
        assert!(!has_effect(&host.effects(), |effect| matches!(
            effect,
            HostEffect::Run { program, args }
                if program == "systemctl"
                    && args.first().is_some_and(|action| {
                        ["enable", "start", "restart", "stop"]
                            .iter()
                            .any(|verb| action == verb)
                    })
        )));
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::PENDING_PATH))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn inactive_monitor_is_never_started_without_safe_online() {
        let host = with_observations(
            systemctl_active(fixture_host(), "nut-monitor.service", "inactive"),
            5,
            ol(),
        );
        // The loss of OL between steps arrives at the monitor gate.
        let host = host.with_ups_observation(ob());
        let hash = expect_plan(&host, false);
        let error = converge(&host, &options(hash, false)).unwrap_err();
        assert!(
            matches!(error, ConvergeError::UnsafeUps(_)),
            "unexpected error: {error:?}"
        );
        // Driver and server converged, but the monitor was never touched.
        assert!(!has_effect(&host.effects(), |effect| matches!(
            effect,
            HostEffect::Run { program, args }
                if program == "systemctl"
                    && args.len() == 2
                    && args[1] == "nut-monitor.service"
                    && ["start", "restart"]
                        .iter()
                        .any(|verb| &args[0] == verb)
        )));
        // Files applied before the gate stay journaled for resume, and the
        // pending transaction is retained for `nut rollback`.
        assert!(
            host.read_protected(std::path::Path::new("/etc/nut/nut.conf"))
                .unwrap()
                .is_some()
        );
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::PENDING_PATH))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn service_effect_must_reach_desired_state_not_just_exit_zero() {
        // Every unit reports inactive, so the driver restart runs without a
        // stack to disrupt — but the unit never becomes active.
        let mut host = fixture_host();
        for unit in [
            "nut-driver@cp1500.service",
            "nut-server.service",
            "nut-monitor.service",
            "nut.target",
        ] {
            host = systemctl_active(host, unit, "inactive");
        }
        let host = with_observations(host, 3, ol());
        let hash = expect_plan(&host, false);
        let error = converge(&host, &options(hash, false)).unwrap_err();
        assert!(
            matches!(error, ConvergeError::ServiceFailed { .. }),
            "unexpected error: {error:?}"
        );
        // The failed effect leaves its checkpoint behind for resume, and the
        // pending transaction is retained rather than finalized.
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::PENDING_PATH))
                .unwrap()
                .is_some()
        );
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::STATE_PATH))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn failed_verification_compensates_and_keeps_backup() {
        // Six OL readings carry planning, locking, and service gates; the
        // verification pass then observes an on-battery UPS.
        let host = with_observations(fixture_host(), 6, ol()).with_ups_observation(ob());
        let hash = expect_plan(&host, false);
        let error = converge(&host, &options(hash, false)).unwrap_err();
        let (stamp, detail) = match &error {
            ConvergeError::VerificationFailed { stamp, detail } => (stamp.clone(), detail.clone()),
            other => panic!("unexpected error: {other:?}"),
        };
        assert!(detail.contains("hw.online"), "{detail}");
        assert!(
            error.to_string().contains(&format!("nut rollback {stamp}")),
            "{error}"
        );
        // Touched files are restored to their absent preimages. The host
        // config is untouched: its source and destination are identical.
        for path in [
            "/etc/nut/nut.conf",
            "/etc/nut/upsd.users",
            "/etc/nut-cli/operator.secret",
        ] {
            assert!(
                host.read_protected(std::path::Path::new(path))
                    .unwrap()
                    .is_none(),
                "{path} was not compensated"
            );
        }
        // Services already matched their snapshots, so no stop or disable ran.
        assert!(!has_effect(&host.effects(), |effect| matches!(
            effect,
            HostEffect::Run { program, args }
                if program == "systemctl"
                    && args.first().is_some_and(|action| action == "stop" || action == "disable")
        )));
        // The backup and the recovery record stay in place for rollback, and
        // no state was ever finalized.
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::PENDING_PATH))
                .unwrap()
                .is_some()
        );
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::STATE_PATH))
                .unwrap()
                .is_none()
        );
        // An explicit on-battery reading is not a readiness gap: no settle wait.
        assert!(host.slept().is_zero());
    }

    fn upsd_not_listening() -> UpsObservation {
        UpsObservation::unavailable(
            "cannot connect to upsd at 127.0.0.1:3493: Connection refused (os error 111)",
        )
    }

    #[test]
    fn monitor_gate_waits_for_a_just_started_upsd() {
        // Type=simple upsd: the monitor gate first finds nothing listening,
        // then the fresh OL reading; verification follows.
        let host = with_observations(fixture_host(), 5, ol())
            .with_ups_observation(upsd_not_listening())
            .with_ups_observation(upsd_not_listening());
        let host = with_observations(host, 2, ol());
        let hash = expect_plan(&host, false);
        let report = converge(&host, &options(hash, false)).expect("converges after settle");
        assert!(report.state_finalized);
        assert_eq!(host.slept(), SETTLE_INTERVAL * 2);
    }

    #[test]
    fn monitor_gate_refuses_when_upsd_never_answers() {
        let host = with_observations(fixture_host(), 5, ol());
        let hash = expect_plan(&host, false);
        let error = converge(&host, &options(hash, false)).unwrap_err();
        assert!(
            matches!(error, ConvergeError::UnsafeUps(_)),
            "unexpected error: {error:?}"
        );
        assert_eq!(host.slept(), SETTLE_INTERVAL * (SETTLE_ATTEMPTS - 1));
        assert!(!has_effect(&host.effects(), |effect| matches!(
            effect,
            HostEffect::Run { program, args }
                if program == "systemctl"
                    && args.len() == 2
                    && args[1] == "nut-monitor.service"
                    && ["start", "restart"].iter().any(|verb| &args[0] == verb)
        )));
    }

    #[test]
    fn verification_waits_for_monitor_login() {
        // Type=simple upsmon: the first verification pass sees no logged-in
        // client; the next pass sees the login and finalizes state.
        let mut no_login = ol();
        no_login.clients.clear();
        let host = with_observations(fixture_host(), 6, ol())
            .with_ups_observation(no_login)
            .with_ups_observation(ol());
        let hash = expect_plan(&host, false);
        let report = converge(&host, &options(hash, false)).expect("verifies after login");
        assert!(report.state_finalized);
        assert_eq!(host.slept(), SETTLE_INTERVAL);
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::STATE_PATH))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn settling_failures_exclude_unsafe_evidence() {
        let failure = |id: &str| crate::doctor::DoctorFailure {
            id: id.into(),
            title: id.into(),
            evidence: String::new(),
            fix: None,
            unavailable: false,
        };
        let report = |ids: &[&str]| DoctorReport {
            failures: ids.iter().map(|id| failure(id)).collect(),
            ..DoctorReport::default()
        };
        assert!(!only_settling_failures(&report(&[])));
        assert!(only_settling_failures(&report(&["svc.client"])));
        assert!(only_settling_failures(&report(&[
            "hw.data",
            "hw.online",
            "svc.listen"
        ])));
        // OB with data present, or a config/permission failure, never waits.
        assert!(!only_settling_failures(&report(&["hw.online"])));
        assert!(!only_settling_failures(&report(&[
            "svc.client",
            "root.perms"
        ])));
    }

    #[test]
    fn interruption_resumes_without_new_secrets() {
        // Manifest plus journal, two secret-blob publications plus their
        // journal updates, then a crash on the first file apply.
        let host = with_observations(fixture_host(), 7, ol());
        host.fail_after("replace_file", 7, "crash before first apply");
        let hash = expect_plan(&host, false);
        let error = converge(&host, &options(hash.clone(), false)).unwrap_err();
        assert!(
            matches!(error, ConvergeError::Resource(_)),
            "unexpected error: {error:?}"
        );
        assert!(
            host.read_protected(std::path::Path::new(super::super::observe::PENDING_PATH))
                .unwrap()
                .is_some()
        );
        // Stamp plus both generated secrets drew entropy before the crash.
        assert_eq!(
            host.effects()
                .iter()
                .filter(|effect| matches!(effect, HostEffect::Random { .. }))
                .count(),
            3
        );
        // The restarted host keeps stubs and files but drops queues, locks,
        // and effects; queue the readings the resume needs.
        let restarted = with_observations(host.snapshot_filesystem(), 5, ol());
        let report = converge(&restarted, &options(hash, false))
            .unwrap_or_else(|error| panic!("resume failed: {error:?}"));
        assert!(report.resumed);
        assert!(report.state_finalized);
        assert!(report.doctor.verified(), "{:?}", report.doctor.failures);
        // No new entropy: both secrets were reused from the pending backup.
        assert!(!has_effect(&restarted.effects(), |effect| matches!(
            effect,
            HostEffect::Random { .. }
        )));
        assert!(
            restarted
                .read_protected(std::path::Path::new(super::super::observe::PENDING_PATH))
                .unwrap()
                .is_none()
        );
        assert!(
            restarted
                .read_protected(std::path::Path::new("/etc/nut/upsd.users"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn lock_contention_writes_nothing() {
        let host = with_observations(fixture_host(), 2, ol());
        let hash = expect_plan(&host, false);
        let _guard = acquire_writer_lock(&host).expect("test holds the lock");
        let error = converge(&host, &options(hash, false)).unwrap_err();
        assert!(
            matches!(error, ConvergeError::LockHeld(_)),
            "unexpected error: {error:?}"
        );
        let effects = host.effects();
        // Exactly one lock: the test's own guard. Converge never acquired it.
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, HostEffect::Lock { .. }))
                .count(),
            1
        );
        assert!(!has_effect(&effects, |effect| matches!(
            effect,
            HostEffect::ReplaceFile { .. }
                | HostEffect::Remove { .. }
                | HostEffect::Random { .. }
                | HostEffect::CreateDir { .. }
        )));
    }
}
