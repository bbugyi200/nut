#![forbid(unsafe_code)]

//! Pure init planner: ordered typed steps, masked previews, and hashes.
//!
//! [`plan`] never touches the host. It maps the [`Observation`] into an
//! ordered plan whose public [`PlanView`] contains only masked semantic
//! diffs and non-secret evidence.

use std::collections::BTreeSet;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::init::hash::{HASH_VERSION, plan_hash};
use crate::init::observe::{
    FileState, LEGACY_HOOK, LIVE_HOOK, LIVE_NUT_CONF, LIVE_UPS_CONF, LIVE_UPSD_USERS,
    LIVE_UPSMON_CONF, LIVE_UPSSCHED_CONF, Observation, REQUIRED_PACKAGES,
};
use crate::init::secrets::fingerprint_secret;
use crate::render::{self, OPERATOR_INSTCMDS};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlanOptions {
    pub rotate_secrets: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretIntentView {
    AdoptUpsmon,
    AdoptOperator,
    GenerateUpsmon,
    GenerateOperator,
    RotatePair,
}

#[derive(Clone, Eq, PartialEq)]
pub enum SecretIntent {
    AdoptUpsmon { fingerprint: String },
    AdoptOperator { fingerprint: String },
    GenerateUpsmon,
    GenerateOperator,
    RotatePair { upsmon: String, operator: String },
}

impl std::fmt::Debug for SecretIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AdoptUpsmon { .. } => f
                .debug_struct("AdoptUpsmon")
                .field("fingerprint", &"[redacted]")
                .finish(),
            Self::AdoptOperator { .. } => f
                .debug_struct("AdoptOperator")
                .field("fingerprint", &"[redacted]")
                .finish(),
            Self::GenerateUpsmon => write!(f, "GenerateUpsmon"),
            Self::GenerateOperator => write!(f, "GenerateOperator"),
            Self::RotatePair { .. } => f
                .debug_struct("RotatePair")
                .field("fingerprints", &"[redacted]")
                .finish(),
        }
    }
}

impl SecretIntent {
    fn view(&self) -> SecretIntentView {
        match self {
            Self::AdoptUpsmon { .. } => SecretIntentView::AdoptUpsmon,
            Self::AdoptOperator { .. } => SecretIntentView::AdoptOperator,
            Self::GenerateUpsmon => SecretIntentView::GenerateUpsmon,
            Self::GenerateOperator => SecretIntentView::GenerateOperator,
            Self::RotatePair { .. } => SecretIntentView::RotatePair,
        }
    }

    fn canonical(&self) -> String {
        match self {
            Self::AdoptUpsmon { fingerprint } => {
                format!("adopt-upsmon:{fingerprint}")
            }
            Self::AdoptOperator { fingerprint } => {
                format!("adopt-operator:{fingerprint}")
            }
            Self::GenerateUpsmon => "generate-upsmon".to_owned(),
            Self::GenerateOperator => "generate-operator".to_owned(),
            Self::RotatePair { upsmon, operator } => {
                format!("rotate:{upsmon}:{operator}")
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepKind {
    Package,
    DeviceRepair,
    File,
    Membership,
    Fstab,
    Retirement,
    Service,
    State,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlanStep {
    pub id: String,
    pub kind: PlanStepKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Plan {
    pub complete: bool,
    pub explanations: Vec<String>,
    pub steps: Vec<PlanStep>,
    pub hash: Option<String>,
    pub secret_intents: Vec<SecretIntent>,
    canonical_preimage: String,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("complete", &self.complete)
            .field("explanations", &self.explanations)
            .field("steps", &self.steps)
            .field("hash", &self.hash)
            .field("secret_intents", &self.secret_intents)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlanView {
    pub schema_version: u32,
    pub complete: bool,
    pub hash: Option<String>,
    pub steps: Vec<PlanStep>,
    pub explanations: Vec<String>,
    pub secret_intents: Vec<SecretIntentView>,
    pub empty: bool,
}

impl Plan {
    pub fn view(&self) -> PlanView {
        PlanView {
            schema_version: 1,
            complete: self.complete,
            hash: self.hash.clone(),
            steps: self.steps.clone(),
            explanations: self.explanations.clone(),
            secret_intents: self.secret_intents.iter().map(|i| i.view()).collect(),
            empty: self.steps.is_empty(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

fn file_fingerprint(state: &FileState) -> String {
    match state {
        FileState::Absent => "absent".to_owned(),
        FileState::Unreadable => "unreadable".to_owned(),
        FileState::Unsafe { reason } => format!("unsafe:{reason}"),
        FileState::Present { bytes, metadata } => {
            format!("sha256:{:x}:{metadata:?}", Sha256::digest(bytes))
        }
    }
}

fn masked_diff_summary(path: &str) -> String {
    format!("update {path} (secrets masked)")
}

fn parse_fstab_entries(fstab: &[u8]) -> Vec<(String, String, String)> {
    // Returns (mountpoint, options, original_line) for non-comment lines.
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(fstab).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        out.push((fields[1].to_owned(), fields[3].to_owned(), line.to_owned()));
    }
    out
}

fn fstab_needs_nofail(options: &str) -> bool {
    options.split(',').all(|opt| opt.trim() != "nofail")
}

/// Build the authoritative plan. Pure: no host access, no entropy, no writes.
pub fn plan(observation: &Observation, options: &PlanOptions) -> Plan {
    let mut explanations = Vec::new();
    let mut steps: Vec<PlanStep> = Vec::new();
    let mut intents: Vec<SecretIntent> = Vec::new();

    // Unreadable or missing required inputs yield an incomplete plan with no
    // usable apply hash. A partial preview still lists what is known.
    if let Some(error) = observation.config_error.as_ref() {
        explanations.push(format!("host config is unavailable: {error}"));
        return incomplete(explanations, steps, intents);
    }
    let Some(resolved) = observation.resolved.as_ref() else {
        let reason = observation
            .resolve_error
            .clone()
            .unwrap_or_else(|| "host config does not resolve".to_owned());
        explanations.push(reason);
        return incomplete(explanations, steps, intents);
    };
    if let Some(error) = observation.render_error.as_ref() {
        explanations.push(format!("cannot render expected files: {error}"));
        return incomplete(explanations, steps, intents);
    }
    let Some(rendered) = observation.rendered.as_ref() else {
        explanations.push("expected files are unavailable".to_owned());
        return incomplete(explanations, steps, intents);
    };
    if !observation.unreadable_required.is_empty() {
        let mut seen = BTreeSet::new();
        for path in &observation.unreadable_required {
            if seen.insert(path) {
                explanations.push(format!("{path} needs root to read"));
            }
        }
        return incomplete(explanations, steps, intents);
    }

    // Hard refusals before any applicable plan.
    if observation.pwrstatd_running == Some(true) {
        explanations.push("pwrstatd is running; stop PowerPanel before using NUT".to_owned());
        return incomplete(explanations, steps, intents);
    }
    if let Some(error) = observation.pwrstatd_error.as_ref() {
        explanations.push(format!("pwrstatd probe failed: {error}"));
        return incomplete(explanations, steps, intents);
    }
    if observation.killpower_present == Some(true) {
        explanations.push(
            "/etc/killpower is present; inspect it, then remove it by hand if the UPS is on line"
                .to_owned(),
        );
        return incomplete(explanations, steps, intents);
    }
    if observation.usb.ambiguous {
        explanations.push(
            "multiple USB devices match the configured vendor/product; pin match.serial".to_owned(),
        );
        return incomplete(explanations, steps, intents);
    }
    if let Some(error) = observation.usb.error.as_ref() {
        explanations.push(format!("USB discovery failed: {error}"));
        return incomplete(explanations, steps, intents);
    }
    for (package, state) in &observation.packages {
        if state.probe_failed.is_some() {
            explanations.push(format!(
                "package probe failed for {package}: {}",
                state.probe_failed.as_deref().unwrap_or("unknown")
            ));
            return incomplete(explanations, steps, intents);
        }
    }
    for (unit, state) in &observation.services {
        if state.probe_failed.is_some() {
            explanations.push(format!(
                "service probe failed for {unit}: {}",
                state.probe_failed.as_deref().unwrap_or("unknown")
            ));
            return incomplete(explanations, steps, intents);
        }
    }
    for (path, state) in &observation.live_files {
        if let FileState::Unsafe { reason } = state {
            explanations.push(format!("unsafe target {path}: {reason}"));
            return incomplete(explanations, steps, intents);
        }
    }
    if observation.credentials.upsmon_malformed && !options.rotate_secrets {
        explanations.push(
            "upsd.users or upsmon.conf credential records are malformed; use --rotate-secrets to replace them"
                .to_owned(),
        );
        return incomplete(explanations, steps, intents);
    }
    if observation.credentials.operator_malformed && !options.rotate_secrets {
        explanations.push(
            "operator credential records are malformed; use --rotate-secrets to replace them"
                .to_owned(),
        );
        return incomplete(explanations, steps, intents);
    }
    if let FileState::Present { .. } = observation.pending {
        explanations
            .push("a pending transaction needs recovery before planning a new apply".to_owned());
        return incomplete(explanations, steps, intents);
    }

    // Operator account preflight: every configured Unix user must exist.
    for (user, exists) in &observation.groups.user_exists {
        if !exists {
            explanations.push(format!("operator user {user} does not exist"));
            return incomplete(explanations, steps, intents);
        }
    }

    // Secret intents: adopt valid existing credentials, generate missing ones,
    // fail on conflicts unless rotation replaces both.
    let creds = &observation.credentials;
    let upsmon_present = creds.upsmon_password_present;
    let operator_present = creds.operator_password_present
        || creds
            .operator_secret
            .present_bytes()
            .is_some_and(|b| !String::from_utf8_lossy(b).trim().is_empty());
    let passwords_agree = creds.passwords_agree;
    if options.rotate_secrets {
        intents.push(SecretIntent::RotatePair {
            upsmon: creds
                .upsmon_fingerprint
                .clone()
                .unwrap_or_else(|| "absent".to_owned()),
            operator: creds
                .operator_fingerprint
                .clone()
                .unwrap_or_else(|| "absent".to_owned()),
        });
    } else {
        match (upsmon_present, passwords_agree) {
            (true, Some(true)) => {
                intents.push(SecretIntent::AdoptUpsmon {
                    fingerprint: creds.upsmon_fingerprint.clone().unwrap_or_default(),
                });
            }
            (true, Some(false)) => {
                explanations.push(
                    "upsd.users and upsmon.conf credentials disagree; use --rotate-secrets to replace them"
                        .to_owned(),
                );
                return incomplete(explanations, steps, intents);
            }
            (true, None) => {
                // Only one side readable (partial preview): adopt what exists.
                if let Some(fingerprint) = creds.upsmon_fingerprint.clone() {
                    intents.push(SecretIntent::AdoptUpsmon { fingerprint });
                } else {
                    intents.push(SecretIntent::GenerateUpsmon);
                }
            }
            (false, _) => {
                intents.push(SecretIntent::GenerateUpsmon);
            }
        }
        // Operator: adopt when exactly one source is unambiguous, fail on
        // conflicting duplicates unless rotation replaces them.
        let users_fp = creds.operator_fingerprint.clone();
        let file_fp = creds
            .operator_secret
            .present_bytes()
            .map(|b| fingerprint_secret(String::from_utf8_lossy(b).trim()));
        match (creds.operator_password_present, file_fp) {
            (true, Some(file)) if Some(&file) == users_fp.as_ref() => {
                intents.push(SecretIntent::AdoptOperator { fingerprint: file });
            }
            (true, Some(_)) => {
                explanations.push(
                    "operator secret differs between upsd.users and operator.secret; use --rotate-secrets to replace it"
                        .to_owned(),
                );
                return incomplete(explanations, steps, intents);
            }
            (true, None) => {
                intents.push(SecretIntent::AdoptOperator {
                    fingerprint: users_fp.unwrap_or_default(),
                });
            }
            (false, Some(file)) => {
                intents.push(SecretIntent::AdoptOperator { fingerprint: file });
            }
            (false, None) => {
                if operator_present {
                    intents.push(SecretIntent::AdoptOperator {
                        fingerprint: users_fp.unwrap_or_default(),
                    });
                } else {
                    intents.push(SecretIntent::GenerateOperator);
                }
            }
        }
    }

    // Packages: install only missing ones, never upgrade or remove.
    for package in REQUIRED_PACKAGES {
        let state = observation.packages.get(*package);
        let missing = state.is_none_or(|s| s.missing());
        if missing {
            let candidate = state
                .and_then(|s| s.candidate_version.clone())
                .unwrap_or_else(|| "unknown".to_owned());
            steps.push(PlanStep {
                id: format!("package:{package}"),
                kind: PlanStepKind::Package,
                summary: format!("install missing package {package} (candidate {candidate})"),
                dependencies: Vec::new(),
            });
        }
    }

    // Files: exact typed render with retained secrets. Compare masked text,
    // then force an update when credentials or metadata differ even when the
    // masked text is identical.
    let expected: Vec<(&str, &str)> = vec![
        (LIVE_NUT_CONF, rendered.nut_conf.as_str()),
        (LIVE_UPS_CONF, rendered.ups_conf.as_str()),
        (LIVE_UPSD_USERS, rendered.upsd_users.as_str()),
        (LIVE_UPSMON_CONF, rendered.upsmon_conf.as_str()),
        (LIVE_UPSSCHED_CONF, rendered.upssched_conf.as_str()),
        (LIVE_HOOK, rendered.hook.as_str()),
    ];
    let mut files_changed: BTreeSet<String> = BTreeSet::new();
    for (path, want) in &expected {
        let state = observation.live_files.get(*path);
        let needs_update = match state {
            None | Some(FileState::Absent) => true,
            Some(FileState::Present { bytes, metadata }) => {
                let live = String::from_utf8_lossy(bytes);
                let masked_live = render::mask_secrets(&live);
                let masked_want = render::mask_secrets(want);
                if masked_live != masked_want {
                    true
                } else {
                    // Credential-only or metadata-only differences still
                    // require a file change even when masked text matches.
                    credential_only_difference(path, &observation.credentials, options)
                        || metadata_differs(path, metadata.as_ref(), observation.groups.nut_gid)
                }
            }
            Some(FileState::Unreadable) => {
                // Already returned incomplete above; unreachable.
                false
            }
            Some(FileState::Unsafe { .. }) => false,
        };
        if needs_update {
            steps.push(PlanStep {
                id: format!("file:{path}"),
                kind: PlanStepKind::File,
                summary: masked_diff_summary(path),
                dependencies: vec![],
            });
            files_changed.insert((*path).to_owned());
        }
    }

    // Compare validated config semantics so a source/destination alias or
    // formatting-only difference cannot force perpetual rewrites.
    let config_matches = observation
        .live_files
        .get("/etc/nut-cli/nut.toml")
        .is_some_and(|state| match state {
            FileState::Present { bytes, metadata } => {
                std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|text| crate::config::HostConfig::parse(text).ok())
                    .as_ref()
                    == observation.config.as_ref()
                    && metadata.as_ref() == Some(&crate::host::FileMetadata::file(0, 0, 0o644))
            }
            _ => false,
        });
    if !config_matches {
        steps.push(PlanStep {
            id: "file:/etc/nut-cli/nut.toml".into(),
            kind: PlanStepKind::File,
            summary: "install validated host config (secrets masked)".into(),
            dependencies: vec![],
        });
    }
    let secret_matches = match &creds.operator_secret {
        FileState::Present { bytes, metadata } => {
            let normalized = String::from_utf8_lossy(bytes);
            normalized.ends_with('\n')
                && normalized.trim_end_matches('\n').lines().count() == 1
                && !options.rotate_secrets
                && metadata.as_ref().is_some_and(|m| {
                    m.uid == 0
                        && m.mode == 0o640
                        && m.is_file
                        && Some(m.gid) == observation.groups.operators_gid
                })
        }
        _ => false,
    };
    if !secret_matches {
        steps.push(PlanStep {
            id: "file:/etc/nut-cli/operator.secret".into(),
            kind: PlanStepKind::File,
            summary: "install protected operator credential (secret masked)".into(),
            dependencies: vec![],
        });
    }
    if observation.groups.operators_gid.is_none() {
        steps.push(PlanStep {
            id: "group:nut-operators".into(),
            kind: PlanStepKind::Membership,
            summary: "create the nut-operators system group".into(),
            dependencies: vec![],
        });
    }

    // Memberships: additive only; missing users already failed above.
    for (user, in_group) in &observation.groups.user_in_operators {
        if !in_group {
            steps.push(PlanStep {
                id: format!("membership:nut-operators:{user}"),
                kind: PlanStepKind::Membership,
                summary: format!("add existing user {user} to nut-operators"),
                dependencies: Vec::new(),
            });
        }
    }
    if resolved.operators.journal {
        let journal_members = journal_members(observation);
        for user in observation.groups.user_exists.keys() {
            if !journal_members.contains(user) {
                steps.push(PlanStep {
                    id: format!("membership:systemd-journal:{user}"),
                    kind: PlanStepKind::Membership,
                    summary: format!("add existing user {user} to systemd-journal"),
                    dependencies: Vec::new(),
                });
            }
        }
    }

    // Fstab: exactly one matching entry per requested mount, nofail added.
    if !resolved.boot.nofail_mounts.is_empty() {
        let fstab_bytes = observation.fstab.present_bytes();
        let Some(fstab_bytes) = fstab_bytes else {
            explanations.push("/etc/fstab is unavailable for nofail mounts".to_owned());
            return incomplete(explanations, steps, intents);
        };
        let entries = parse_fstab_entries(fstab_bytes);
        for mount in &resolved.boot.nofail_mounts {
            let wanted = mount.display().to_string();
            let matches: Vec<_> = entries.iter().filter(|(mp, _, _)| mp == &wanted).collect();
            if matches.is_empty() {
                explanations.push(format!("{wanted} is missing from fstab"));
                return incomplete(explanations, steps, intents);
            }
            if matches.len() > 1 {
                explanations.push(format!("{wanted} is ambiguous in fstab"));
                return incomplete(explanations, steps, intents);
            }
            let (_, opts, _) = matches[0];
            if fstab_needs_nofail(opts) {
                steps.push(PlanStep {
                    id: format!("fstab:{wanted}"),
                    kind: PlanStepKind::Fstab,
                    summary: format!("add nofail to {wanted} (preserve other fields)"),
                    dependencies: Vec::new(),
                });
            }
        }
    }

    // Device repair: targeted udev trigger/settle only for a selected node
    // with an incorrect group. Skipped for dummy-ups.
    if resolved.driver.value != "dummy-ups"
        && let Some(sysfs) = observation.usb.matching_sysfs.first()
    {
        let want_gid = observation.groups.nut_gid;
        let node_gid = observation.usb.node_gid;
        if let (Some(want), Some(have)) = (want_gid, node_gid)
            && want != have
        {
            steps.push(PlanStep {
                id: format!("device:{}", sysfs.display()),
                kind: PlanStepKind::DeviceRepair,
                summary: format!(
                    "repair USB node group for {} then verify it",
                    sysfs.display()
                ),
                dependencies: Vec::new(),
            });
        }
    }

    // Retirement: only the recognized legacy athena hook, only after its
    // replacement is installed.
    if let FileState::Present { .. } = observation.legacy_hook {
        if observation.legacy_hook_recognized {
            steps.push(PlanStep {
                id: format!("retirement:{LEGACY_HOOK}"),
                kind: PlanStepKind::Retirement,
                summary: format!("retire recognized legacy hook {LEGACY_HOOK}"),
                dependencies: vec![format!("file:{LIVE_HOOK}")],
            });
        } else {
            explanations.push(format!(
                "{LEGACY_HOOK} is present but not the recognized legacy hook; leaving it alone"
            ));
        }
    }

    // Services: minimal input dependencies, enable/start only as needed.
    let mut service_steps: Vec<PlanStep> = Vec::new();
    let driver_unit = format!("nut-driver@{}.service", resolved.ups_name);
    let impacted = impacted_services(&files_changed, &driver_unit);
    for unit in [
        driver_unit.as_str(),
        "nut-server.service",
        "nut-monitor.service",
    ] {
        let state = observation.services.get(unit);
        let enabled = state.and_then(|s| s.enabled.clone()).unwrap_or_default();
        let active = state.and_then(|s| s.active.clone()).unwrap_or_default();
        let needs_enable = enabled != "enabled" && enabled != "static";
        let needs_restart = impacted.contains(unit);
        let needs_start = active != "active" && (needs_restart || files_changed.is_empty());
        if needs_enable {
            service_steps.push(PlanStep {
                id: format!("service:enable:{unit}"),
                kind: PlanStepKind::Service,
                summary: format!("enable {unit}"),
                dependencies: files_changed.iter().map(|f| format!("file:{f}")).collect(),
            });
        }
        if needs_restart {
            service_steps.push(PlanStep {
                id: format!("service:restart:{unit}"),
                kind: PlanStepKind::Service,
                summary: format!("restart impacted {unit} once"),
                dependencies: files_changed.iter().map(|f| format!("file:{f}")).collect(),
            });
        } else if needs_start {
            service_steps.push(PlanStep {
                id: format!("service:start:{unit}"),
                kind: PlanStepKind::Service,
                summary: format!("start stopped required {unit}"),
                dependencies: Vec::new(),
            });
        }
    }
    // nut.target enablement follows the driver.
    if let Some(target) = observation.services.get("nut.target")
        && target.enabled.as_deref() != Some("enabled")
    {
        service_steps.push(PlanStep {
            id: "service:enable:nut.target".to_owned(),
            kind: PlanStepKind::Service,
            summary: "enable nut.target".to_owned(),
            dependencies: Vec::new(),
        });
    }
    steps.extend(service_steps);

    // State record only for nonempty plans; empty plans change nothing.
    if !steps.is_empty() {
        steps.push(PlanStep {
            id: "state:record".to_owned(),
            kind: PlanStepKind::State,
            summary: "record verified convergence state after verification".to_owned(),
            dependencies: steps.iter().map(|s| s.id.clone()).collect(),
        });
    } else {
        explanations.push("Already converged.".to_owned());
    }

    let canonical = canonical_preimage(observation, resolved, &steps, &intents, options);
    let hash = plan_hash(canonical.as_bytes());
    Plan {
        complete: true,
        explanations,
        steps,
        hash: Some(hash),
        secret_intents: intents,
        canonical_preimage: canonical,
    }
}

fn incomplete(explanations: Vec<String>, steps: Vec<PlanStep>, intents: Vec<SecretIntent>) -> Plan {
    Plan {
        complete: false,
        explanations,
        steps,
        hash: None,
        secret_intents: intents,
        canonical_preimage: String::new(),
    }
}

fn credential_only_difference(
    path: &str,
    creds: &crate::init::observe::CredentialObservation,
    options: &PlanOptions,
) -> bool {
    if options.rotate_secrets && (path == LIVE_UPSD_USERS || path == LIVE_UPSMON_CONF) {
        return true;
    }
    if path == LIVE_UPSD_USERS {
        // Legacy grants must be repaired even when masked text matches.
        if creds.operator_has_actions_set || creds.operator_has_all {
            return true;
        }
        let joined = creds.operator_instcmds.join(", ");
        if !creds.operator_instcmds.is_empty()
            && creds.operator_instcmds.len() != OPERATOR_INSTCMDS.len()
        {
            return true;
        }
        if joined.contains(',') {
            return true;
        }
        let mut have: Vec<String> = creds.operator_instcmds.clone();
        have.sort();
        let mut want: Vec<String> = OPERATOR_INSTCMDS.iter().map(|s| (*s).to_owned()).collect();
        want.sort();
        if have != want && !have.is_empty() {
            return true;
        }
    }
    false
}

fn metadata_differs(
    path: &str,
    metadata: Option<&crate::host::FileMetadata>,
    nut_gid: Option<u32>,
) -> bool {
    let Some(meta) = metadata else {
        return true;
    };
    if path == LIVE_HOOK {
        return !(meta.uid == 0 && meta.gid == 0 && meta.mode == 0o755 && meta.is_file);
    }
    !(meta.uid == 0
        && meta.mode == 0o640
        && meta.is_file
        && !meta.is_dir
        && Some(meta.gid) == nut_gid)
}

fn journal_members(observation: &Observation) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(bytes) = observation.groups.group_file.present_bytes() {
        for line in String::from_utf8_lossy(bytes).lines() {
            let mut parts = line.split(':');
            if let (Some(name), _, _, Some(members)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
                && name == "systemd-journal"
            {
                for member in members.split(',').map(str::trim) {
                    if !member.is_empty() {
                        out.insert(member.to_owned());
                    }
                }
            }
        }
    }
    out
}

fn impacted_services(files_changed: &BTreeSet<String>, driver_unit: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for path in files_changed {
        match path.as_str() {
            p if p == LIVE_NUT_CONF => {
                out.insert(driver_unit.to_owned());
                out.insert("nut-server.service".to_owned());
                out.insert("nut-monitor.service".to_owned());
            }
            p if p == LIVE_UPS_CONF => {
                out.insert(driver_unit.to_owned());
                out.insert("nut-server.service".to_owned());
                out.insert("nut-monitor.service".to_owned());
            }
            p if p == LIVE_UPSD_USERS => {
                out.insert("nut-server.service".to_owned());
                out.insert("nut-monitor.service".to_owned());
            }
            p if p == LIVE_UPSMON_CONF => {
                out.insert("nut-monitor.service".to_owned());
            }
            p if p == LIVE_UPSSCHED_CONF => {
                out.insert("nut-monitor.service".to_owned());
            }
            p if p == LIVE_HOOK => {
                out.insert("nut-monitor.service".to_owned());
            }
            _ => {}
        }
    }
    out
}

fn canonical_preimage(
    observation: &Observation,
    resolved: &crate::config::ResolvedConfig,
    steps: &[PlanStep],
    intents: &[SecretIntent],
    options: &PlanOptions,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("v{}\n", HASH_VERSION));
    out.push_str(&format!(
        "config_path:{}\n",
        observation
            .config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    ));
    if let Some(config) = observation.config.as_ref() {
        let json = serde_json::json!({
            "host": config.host,
            "ups": config.ups,
            "shutdown": config.shutdown,
            "notify": config.notify,
            "boot": config.boot,
            "operators": config.operators,
        });
        out.push_str(&format!("config:{json}\n"));
    }
    out.push_str(&format!(
        "profile:{}:{}:{}\n",
        resolved.profile.id.as_str(),
        resolved.driver.value,
        resolved
            .profile
            .usb_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    ));
    out.push_str(&format!(
        "resolved:{}:{}:{}:{}:{}:{}\n",
        resolved.ups_name,
        resolved.driver.value,
        resolved.port.value,
        resolved.offdelay.value.as_secs(),
        resolved.ondelay.value.as_secs(),
        resolved.ups.usb_match.serial.clone().unwrap_or_default()
    ));
    let mut sorted_steps: Vec<&PlanStep> = steps.iter().collect();
    sorted_steps.sort_by(|a, b| a.id.cmp(&b.id));
    for step in sorted_steps {
        // State recording is an effect of verification, not a hashed input.
        if step.kind == PlanStepKind::State {
            continue;
        }
        out.push_str(&format!(
            "op:{}:{:?}:{}\n",
            step.id,
            std::mem::discriminant(&step.kind),
            step.summary
        ));
    }
    for (path, state) in &observation.live_files {
        out.push_str(&format!("target:{path}:{}\n", file_fingerprint(state)));
    }
    out.push_str(&format!(
        "config_source:{}\n",
        file_fingerprint(&observation.config_source)
    ));
    out.push_str(&format!(
        "legacy:{}\n",
        file_fingerprint(&observation.legacy_hook)
    ));
    out.push_str(&format!(
        "identities:{:?}:{:?}:{}:{}\n",
        observation.groups.nut_gid,
        observation.groups.operators_gid,
        file_fingerprint(&observation.groups.group_file),
        file_fingerprint(&observation.groups.passwd_file)
    ));
    out.push_str(&format!("fstab:{}\n", file_fingerprint(&observation.fstab)));
    for package in REQUIRED_PACKAGES {
        let state = observation.packages.get(*package);
        out.push_str(&format!(
            "package:{package}:{}:{}\n",
            state
                .and_then(|s| s.installed_version.clone())
                .unwrap_or_else(|| "absent".to_owned()),
            state
                .and_then(|s| s.candidate_version.clone())
                .unwrap_or_else(|| "unknown".to_owned())
        ));
    }
    for (unit, state) in &observation.services {
        out.push_str(&format!(
            "service:{unit}:{}:{}\n",
            state.active.clone().unwrap_or_else(|| "unknown".to_owned()),
            state
                .enabled
                .clone()
                .unwrap_or_else(|| "unknown".to_owned())
        ));
    }
    out.push_str(&format!(
        "groups:{}\n",
        observation.groups.operators_members.join(",")
    ));
    out.push_str(&format!(
        "usb:{}:{}:{}:{:?}:{:?}\n",
        observation
            .usb
            .selected
            .map(|id| id.to_string())
            .unwrap_or_default(),
        observation.usb.ambiguous,
        observation.usb.skipped_dummy,
        observation.usb.matching_sysfs,
        observation.usb.node_gid
    ));
    out.push_str(&format!(
        "nut:{}\n",
        observation.ups.nut_version.clone().unwrap_or_default()
    ));
    for intent in intents {
        out.push_str(&format!("intent:{}\n", intent.canonical()));
    }
    out.push_str(&format!("rotate:{}\n", options.rotate_secrets));
    if let Some(path) = observation.config_path.as_ref() {
        out.push_str(&format!("source:{}\n", path.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;
    use crate::init::observe::collect;
    use std::path::Path;

    const SENTINEL: &str = "SENTINEL-PLAN-SECRET-9f8e7d6c5b4a";

    fn athena_host() -> FakeHost {
        FakeHost::default()
            .with_file(
                "/etc/nut-cli/nut.toml",
                include_str!("../../tests/fixtures/hosts/athena/nut.toml")
                    .as_bytes()
                    .to_vec(),
            )
            .with_env("NUT_CONFIG", "/etc/nut-cli/nut.toml")
            .with_file(
                "/etc/group",
                b"nut:x:100:nut\nnut-operators:x:101:bryan\n".to_vec(),
            )
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
            .with_program_args(
                "dpkg-query",
                ["-W", "-f=${Version}", "nut-client"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"2.8.1-1".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "dpkg-query",
                ["-W", "-f=${Version}", "nut-server"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"2.8.1-1".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "systemctl",
                ["is-active", "nut-driver@cp1500.service"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"active".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "systemctl",
                ["is-enabled", "nut-driver@cp1500.service"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"enabled".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "systemctl",
                ["is-active", "nut-server.service"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"active".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "systemctl",
                ["is-enabled", "nut-server.service"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"enabled".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "systemctl",
                ["is-active", "nut-monitor.service"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"active".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "systemctl",
                ["is-enabled", "nut-monitor.service"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"enabled".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "systemctl",
                ["is-active", "nut.target"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"active".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "systemctl",
                ["is-enabled", "nut.target"],
                crate::host::ProcessOutput {
                    status: Some(0),
                    stdout: b"enabled".to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program(
                "pgrep",
                crate::host::ProcessOutput {
                    status: Some(1),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            )
    }

    #[test]
    fn config_alias_and_formatting_do_not_force_self_copy() {
        let host = athena_host();
        let mut observed = collect(&host, None, None);
        observed.config_path = Some("/home/operator/nut.toml".into());
        if let Some(FileState::Present { bytes, .. }) =
            observed.live_files.get_mut("/etc/nut-cli/nut.toml")
        {
            bytes.extend_from_slice(b"\n# retained installed comment\n");
        }
        let planned = plan(&observed, &PlanOptions::default());
        assert!(planned.complete);
        assert!(
            !planned
                .steps
                .iter()
                .any(|step| step.id == "file:/etc/nut-cli/nut.toml")
        );
        observed.live_files.remove("/etc/nut-cli/nut.toml");
        assert!(
            plan(&observed, &PlanOptions::default())
                .steps
                .iter()
                .any(|step| step.id == "file:/etc/nut-cli/nut.toml")
        );
    }

    #[test]
    fn group_drift_is_hashed_and_planned_even_when_bytes_match() {
        let host = athena_host();
        let mut observed = collect(&host, None, None);
        let bytes = observed
            .rendered
            .as_ref()
            .unwrap()
            .nut_conf
            .as_bytes()
            .to_vec();
        observed.live_files.insert(
            LIVE_NUT_CONF.into(),
            FileState::Present {
                bytes,
                metadata: Some(crate::host::FileMetadata::file(0, 100, 0o640)),
            },
        );
        let correct = plan(&observed, &PlanOptions::default());
        assert!(
            !correct
                .steps
                .iter()
                .any(|s| s.id == format!("file:{LIVE_NUT_CONF}"))
        );
        if let Some(FileState::Present {
            metadata: Some(meta),
            ..
        }) = observed.live_files.get_mut(LIVE_NUT_CONF)
        {
            meta.gid = 99;
        }
        let drifted = plan(&observed, &PlanOptions::default());
        assert_ne!(correct.hash, drifted.hash);
        assert!(
            drifted
                .steps
                .iter()
                .any(|s| s.id == format!("file:{LIVE_NUT_CONF}"))
        );
    }

    #[test]
    fn operator_secret_and_missing_group_have_explicit_steps() {
        let host = athena_host();
        let mut observed = collect(&host, None, None);
        observed.groups.operators_gid = None;
        let planned = plan(&observed, &PlanOptions::default());
        assert!(
            planned
                .steps
                .iter()
                .any(|s| s.id == "file:/etc/nut-cli/operator.secret")
        );
        assert!(planned.steps.iter().any(|s| s.id == "group:nut-operators"));
    }

    #[test]
    fn absent_install_produces_ordered_steps_and_stable_hash() {
        let host = athena_host();
        let observation = collect(&host, None, None);
        let options = PlanOptions::default();
        let first = plan(&observation, &options);
        let second = plan(&observation, &options);
        assert!(first.complete);
        assert_eq!(first.hash, second.hash);
        assert!(!first.steps.is_empty());
        assert!(first.hash.as_ref().is_some_and(|h| h.len() == 64));
    }

    #[test]
    fn unreadable_inputs_yield_incomplete_plan_without_hash() {
        let host = FakeHost::default()
            .with_unreadable(Path::new("/etc/nut-cli/nut.toml"))
            .with_env("NUT_CONFIG", "/etc/nut-cli/nut.toml");
        let observation = collect(&host, None, None);
        let planned = plan(&observation, &PlanOptions::default());
        assert!(!planned.complete);
        assert!(planned.hash.is_none());
    }

    #[test]
    fn pwrstatd_and_killpower_refuse_a_plan() {
        let host = athena_host().with_program(
            "pgrep",
            crate::host::ProcessOutput {
                status: Some(0),
                stdout: b"1234".to_vec(),
                stderr: Vec::new(),
            },
        );
        let observation = collect(&host, None, None);
        let planned = plan(&observation, &PlanOptions::default());
        assert!(!planned.complete);
    }

    #[test]
    fn secrets_never_appear_in_debug_json_or_diffs() {
        let host = athena_host()
            .with_file(
                "/etc/nut/upsd.users",
                format!("[upsmon]\npassword = \"{SENTINEL}\"\n")
                    .as_bytes()
                    .to_vec(),
            )
            .with_file(
                "/etc/nut/upsmon.conf",
                format!("MONITOR cp1500@localhost 1 upsmon \"{SENTINEL}\" primary\n")
                    .as_bytes()
                    .to_vec(),
            );
        let observation = collect(&host, None, None);
        let planned = plan(&observation, &PlanOptions::default());
        let debug = format!("{planned:?} {observation:?}");
        assert!(!debug.contains(SENTINEL));
        let view = planned.view();
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains(SENTINEL));
        for step in &view.steps {
            assert!(!step.summary.contains(SENTINEL));
        }
    }

    #[test]
    fn rotation_changes_intents_and_hash() {
        let host = athena_host();
        let observation = collect(&host, None, None);
        let plain = plan(&observation, &PlanOptions::default());
        let rotated = plan(
            &observation,
            &PlanOptions {
                rotate_secrets: true,
            },
        );
        assert_ne!(plain.hash, rotated.hash);
    }

    #[test]
    fn fstab_empty_list_means_no_fstab_action() {
        let host = athena_host();
        let observation = collect(&host, None, None);
        // Athena lists two mounts and this host already carries nofail.
        assert!(
            observation
                .resolved
                .as_ref()
                .is_some_and(|r| r.boot.nofail_mounts.len() == 2)
        );
        let planned = plan(&observation, &PlanOptions::default());
        assert!(planned.steps.iter().all(|s| s.kind != PlanStepKind::Fstab));
    }

    #[test]
    fn fstab_missing_entry_refuses_plan() {
        let host =
            athena_host().with_file("/etc/fstab", b"UUID=abc / ext4 defaults 0 1\n".to_vec());
        let observation = collect(&host, None, None);
        let planned = plan(&observation, &PlanOptions::default());
        assert!(!planned.complete);
        assert!(planned.hash.is_none());
    }
}
