#![forbid(unsafe_code)]

//! Typed, checkpointed resource effects. The convergence coordinator owns
//! plan/hash validation, secret materialization, service safety and finalization.
//! No function in this module starts/restarts a service or remounts a filesystem.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use thiserror::Error;

use super::observe::{FSTAB_PATH, LEGACY_HOOK, LIVE_HOOK, LIVE_UPSSCHED_CONF, REQUIRED_PACKAGES};
use super::transaction::{
    self, Checkpoint, CheckpointState, OperationIntent, Transaction, TransactionError,
};
use crate::host::{FileMetadata, Host, HostError, ProcessOutput, ProcessRequest};

const POLICY: &str = "/usr/sbin/policy-rc.d";
const POLICY_BYTES: &[u8] = b"#!/bin/sh\n# Temporary nut-cli package-start guard.\ncase \"$1\" in nut|nut-*|/etc/init.d/nut*) exit 101 ;; esac\nexit 0\n";
const PACKAGE_FILES: &[&str] = &[
    "/etc/nut/nut.conf",
    "/etc/nut/ups.conf",
    "/etc/nut/upsd.users",
    "/etc/nut/upsmon.conf",
    "/etc/nut/upssched.conf",
    POLICY,
];

#[derive(Debug, Error)]
pub enum ResourceError {
    #[error(transparent)]
    Transaction(#[from] TransactionError),
    #[error(transparent)]
    Host(#[from] HostError),
    #[error("resource apply refused: {0}")]
    Refused(&'static str),
}

type Result<T> = std::result::Result<T, ResourceError>;

/// Private payloads never implement Debug or serialization.
pub struct Resource(Action);

enum Action {
    Packages(BTreeMap<String, String>),
    OperatorsGroup,
    Membership {
        user: String,
        group: String,
    },
    Device {
        sysfs: PathBuf,
        vendor: String,
        product: String,
        serial: Option<String>,
        gid: u32,
    },
    File {
        path: PathBuf,
        bytes: Vec<u8>,
        metadata: FileMetadata,
    },
    Fstab {
        bytes: Vec<u8>,
        metadata: FileMetadata,
    },
    Retire {
        hook: Vec<u8>,
        upssched: Vec<u8>,
    },
}

/// A resource and its exact before/after intent. Keep/reconstruct this from
/// reviewed inputs on recovery; never replace the recorded before fingerprint
/// with the current state of a started operation.
pub struct PreparedResource {
    resource: Resource,
    intent: OperationIntent,
}

impl PreparedResource {
    pub fn intent(&self) -> OperationIntent {
        self.intent.clone()
    }
    pub fn backup_targets(&self) -> Vec<PathBuf> {
        self.resource.backup_targets()
    }

    /// Current external fingerprint for `recover_with_observations`.
    pub fn observe(&self, host: &impl Host) -> Result<String> {
        Ok(self.fingerprint(self.resource.current(host)?))
    }
    fn fingerprint(&self, value: String) -> String {
        external_fingerprint(&self.resource, self.intent.target.is_none(), value)
    }
}

fn external_fingerprint(resource: &Resource, external: bool, value: String) -> String {
    if external {
        match &resource.0 {
            Action::File { path, .. } => {
                return digest(&format!("file:{}:{value}", path.display()));
            }
            Action::Fstab { .. } => return digest(&format!("file:{FSTAB_PATH}:{value}")),
            Action::Retire { .. } => return digest(&format!("file:{LEGACY_HOOK}:{value}")),
            _ => (),
        }
    }
    value
}

impl Resource {
    /// Install an exact, simulated closure of missing packages. Installed
    /// packages are never included, including dependencies proposed by apt.
    pub fn packages(host: &impl Host, requested: &BTreeMap<String, String>) -> Result<Self> {
        if requested.is_empty()
            || requested
                .keys()
                .any(|p| !REQUIRED_PACKAGES.contains(&p.as_str()))
        {
            return Err(ResourceError::Refused("invalid requested package set"));
        }
        for (name, version) in requested {
            validate_package(name, version)?;
            if package_version(host, name)?.is_some() {
                return Err(ResourceError::Refused(
                    "requested package is already installed",
                ));
            }
        }
        let solved = simulate(host, requested)?;
        if requested
            .iter()
            .any(|(name, version)| solved.get(name) != Some(version))
        {
            return Err(ResourceError::Refused("package candidate changed"));
        }
        for name in solved.keys() {
            if package_version(host, name)?.is_some() {
                return Err(ResourceError::Refused(
                    "package solution changes an installed package",
                ));
            }
        }
        Ok(Self(Action::Packages(solved)))
    }

    /// Include the dependency solution in the authoritative plan hash.
    pub fn package_versions(&self) -> Option<&BTreeMap<String, String>> {
        match &self.0 {
            Action::Packages(packages) => Some(packages),
            _ => None,
        }
    }

    pub fn operators_group() -> Self {
        Self(Action::OperatorsGroup)
    }

    pub fn membership(user: &str, group: &str) -> Result<Self> {
        validate_user(user)?;
        if !matches!(group, "nut-operators" | "systemd-journal") {
            return Err(ResourceError::Refused("unmanaged membership group"));
        }
        Ok(Self(Action::Membership {
            user: user.into(),
            group: group.into(),
        }))
    }

    /// Dummy profiles return no operation, even if stale USB data was supplied.
    pub fn device(
        sysfs: PathBuf,
        vendor: &str,
        product: &str,
        serial: Option<String>,
        gid: u32,
        dummy: bool,
    ) -> Result<Option<Self>> {
        if dummy {
            return Ok(None);
        }
        let selector = sysfs
            .strip_prefix("/sys/bus/usb/devices")
            .ok()
            .and_then(|p| p.to_str())
            .unwrap_or_default();
        if selector.is_empty()
            || !selector
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':'))
            || [vendor, product]
                .iter()
                .any(|id| id.len() != 4 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(ResourceError::Refused("invalid USB selector"));
        }
        Ok(Some(Self(Action::Device {
            sysfs,
            vendor: vendor.to_ascii_lowercase(),
            product: product.to_ascii_lowercase(),
            serial,
            gid,
        })))
    }

    pub fn file(path: PathBuf, bytes: Vec<u8>, metadata: FileMetadata) -> Result<Self> {
        if !matches!(
            path.to_str(),
            Some(
                "/etc/nut/nut.conf"
                    | "/etc/nut/ups.conf"
                    | "/etc/nut/upsd.users"
                    | "/etc/nut/upsmon.conf"
                    | "/etc/nut/upssched.conf"
                    | LIVE_HOOK
                    | "/etc/nut-cli/nut.toml"
                    | "/etc/nut-cli/operator.secret"
            )
        ) {
            return Err(ResourceError::Refused("unmanaged file target"));
        }
        let mode = if path == Path::new(LIVE_HOOK) {
            0o755
        } else if path == Path::new("/etc/nut-cli/nut.toml") {
            0o644
        } else {
            0o640
        };
        if metadata.uid != 0 || metadata.mode != mode || !metadata.is_file || metadata.is_dir {
            return Err(ResourceError::Refused("incorrect managed file metadata"));
        }
        Ok(Self(Action::File {
            path,
            bytes,
            metadata,
        }))
    }

    /// Build exactly the requested nofail edits, retaining all other bytes.
    pub fn fstab(before: &[u8], mounts: &[PathBuf], metadata: FileMetadata) -> Result<Self> {
        if !metadata.is_file || metadata.is_dir || metadata.uid != 0 || metadata.mode & 0o022 != 0 {
            return Err(ResourceError::Refused("unsafe fstab metadata"));
        }
        Ok(Self(Action::Fstab {
            bytes: nofail_candidate(before, mounts)?,
            metadata,
        }))
    }

    /// Both the replacement and its upssched reference must already be live.
    pub fn retire_legacy(hook: Vec<u8>, upssched: Vec<u8>) -> Self {
        Self(Action::Retire { hook, upssched })
    }

    /// Back up these targets before any package effects or secret generation.
    pub fn backup_targets(&self) -> Vec<PathBuf> {
        match &self.0 {
            Action::Packages(_) => PACKAGE_FILES.iter().map(PathBuf::from).collect(),
            Action::File { path, .. } => vec![path.clone()],
            Action::Fstab { .. } => vec![PathBuf::from(FSTAB_PATH)],
            Action::Retire { .. } => vec![PathBuf::from(LEGACY_HOOK)],
            _ => vec![],
        }
    }

    /// Prepare under the writer lock. A matching resource is a true no-op.
    pub fn prepare(self, host: &impl Host, id: &str) -> Result<Option<PreparedResource>> {
        let before = self.current(host)?;
        let after = self.desired();
        if before == after {
            return Ok(None);
        }
        let target = match &self.0 {
            Action::File { path, .. } => Some(path.clone()),
            Action::Fstab { .. } => Some(PathBuf::from(FSTAB_PATH)),
            Action::Retire { .. } => Some(PathBuf::from(LEGACY_HOOK)),
            _ => None,
        };
        Ok(Some(PreparedResource {
            resource: self,
            intent: OperationIntent {
                id: id.into(),
                target,
                before,
                after,
            },
        }))
    }

    fn desired(&self) -> String {
        match &self.0 {
            Action::File {
                bytes, metadata, ..
            }
            | Action::Fstab { bytes, metadata } => {
                transaction::predicted_file_fingerprint(bytes, metadata)
            }
            Action::Retire { .. } => "absent".into(),
            Action::Device {
                sysfs,
                vendor,
                product,
                serial,
                gid,
            } => device_fingerprint(sysfs, vendor, product, serial.as_deref(), *gid),
            Action::Packages(packages) => {
                package_fingerprint(packages.iter().map(|(n, v)| (n.as_str(), Some(v.as_str()))))
            }
            Action::OperatorsGroup => digest("group:present"),
            Action::Membership { user, group } => digest(&format!("member:{user}:{group}:true")),
        }
    }

    fn current(&self, host: &impl Host) -> Result<String> {
        match &self.0 {
            Action::File { path, .. } => file_state(host, path),
            Action::Fstab { .. } => file_state(host, Path::new(FSTAB_PATH)),
            Action::Retire { .. } => file_state(host, Path::new(LEGACY_HOOK)),
            Action::OperatorsGroup => Ok(digest(if group_id(host, "nut-operators")?.is_some() {
                "group:present"
            } else {
                "group:absent"
            })),
            Action::Membership { user, group } => Ok(digest(&format!(
                "member:{user}:{group}:{}",
                is_member(host, user, group)?
            ))),
            Action::Packages(packages) => {
                let versions = packages
                    .keys()
                    .map(|name| Ok((name.clone(), package_version(host, name)?)))
                    .collect::<Result<BTreeMap<_, _>>>()?;
                Ok(package_fingerprint(
                    versions.iter().map(|(n, v)| (n.as_str(), v.as_deref())),
                ))
            }
            Action::Device {
                sysfs,
                vendor,
                product,
                serial,
                ..
            } => {
                let node = usb_node(host, sysfs, vendor, product, serial.as_deref())?;
                let metadata = host
                    .metadata(&node)?
                    .ok_or(ResourceError::Refused("USB node is missing"))?;
                Ok(device_fingerprint(
                    sysfs,
                    vendor,
                    product,
                    serial.as_deref(),
                    metadata.gid,
                ))
            }
        }
    }
}

/// Reserve an operation before the transaction materializes credentials. Bind
/// it once with `bind_resource` before executing it. All possible file targets
/// must already be included in BeginRequest.targets.
pub fn reserve_resource(id: &str) -> OperationIntent {
    OperationIntent {
        id: id.into(),
        target: None,
        before: "absent".into(),
        after: digest(&format!("reserved-resource:{id}")),
    }
}

pub fn bind_resource(
    host: &impl Host,
    transaction: &Transaction,
    resource: &mut PreparedResource,
) -> Result<()> {
    let reserved = reserve_resource(&resource.intent.id);
    let mut bound = resource.intent.clone();
    bound.target = None;
    bound.before = external_fingerprint(&resource.resource, true, bound.before);
    bound.after = external_fingerprint(&resource.resource, true, bound.after);
    transaction::bind_resource_operation(
        host,
        transaction,
        &reserved,
        &bound,
        &resource.backup_targets(),
    )?;
    resource.intent = bound;
    Ok(())
}

/// Reconstruct a prepared operation from its durable intent after restart.
/// The supplied payload must match the recorded destination and outcome.
pub fn resume_resource(
    host: &impl Host,
    transaction: &Transaction,
    id: &str,
    resource: Resource,
) -> Result<PreparedResource> {
    let (intent, _) = transaction::operation_record(host, transaction, id)?;
    let expected = external_fingerprint(&resource, intent.target.is_none(), resource.desired());
    if intent.after != expected
        || intent
            .target
            .as_ref()
            .is_some_and(|p| !resource.backup_targets().contains(p))
    {
        return Err(ResourceError::Refused(
            "resumed resource differs from its recorded outcome",
        ));
    }
    Ok(PreparedResource { resource, intent })
}

/// Execute only an exact journaled operation. Retries reconcile actual state
/// before deciding to repeat an effect; a completed operation never writes.
pub fn apply_resource(
    host: &impl Host,
    transaction: &Transaction,
    resource: &PreparedResource,
) -> Result<()> {
    let (record, state) = transaction::operation_record(host, transaction, &resource.intent.id)?;
    if record != resource.intent {
        return Err(ResourceError::Refused(
            "resource does not match its journaled intent",
        ));
    }
    // Validate backup coverage for external/deferred operations as well.
    for target in resource.backup_targets() {
        transaction::original_file(host, transaction, &target)?;
    }
    if let Action::Membership { user, group } = &resource.resource.0 {
        transaction::require_membership_addition(host, transaction, user, group)?;
    }
    let current = resource.observe(host)?;
    if state == CheckpointState::Completed {
        if current != record.after {
            return Err(ResourceError::Refused("completed resource drifted"));
        }
        return Ok(());
    }
    if state == CheckpointState::Started && current == record.after {
        if matches!(resource.resource.0, Action::Packages(_)) {
            restore_policy(host, transaction)?;
        }
        if let Action::Fstab { bytes, .. } = &resource.resource.0 {
            cleanup_fstab(host, transaction, bytes)?;
        }
        transaction::checkpoint(host, transaction, &record.id, Checkpoint::Completed)?;
        return Ok(());
    }
    let partial_packages =
        state == CheckpointState::Started && matches!(resource.resource.0, Action::Packages(_));
    if current != record.before && !partial_packages {
        return Err(ResourceError::Refused(
            "resource changed outside this transaction",
        ));
    }
    transaction::checkpoint(host, transaction, &record.id, Checkpoint::Started)?;
    execute(host, transaction, &resource.resource)?;
    if resource.observe(host)? != record.after {
        return Err(ResourceError::Refused(
            "resource command did not reach its desired state",
        ));
    }
    transaction::checkpoint(host, transaction, &record.id, Checkpoint::Completed)?;
    Ok(())
}

fn execute(host: &impl Host, transaction: &Transaction, resource: &Resource) -> Result<()> {
    match &resource.0 {
        Action::Packages(packages) => install_packages(host, transaction, packages),
        Action::OperatorsGroup => {
            checked(
                host,
                "groupadd",
                &["--system".into(), "--".into(), "nut-operators".into()],
            )?;
            Ok(())
        }
        Action::Membership { user, group } => {
            checked(
                host,
                "usermod",
                &[
                    "--append".into(),
                    "--groups".into(),
                    group.into(),
                    "--".into(),
                    user.into(),
                ],
            )?;
            Ok(())
        }
        Action::Device {
            sysfs,
            vendor,
            product,
            serial,
            gid,
        } => {
            usb_node(host, sysfs, vendor, product, serial.as_deref())?;
            if group_id(host, "nut")? != Some(*gid) {
                return Err(ResourceError::Refused("nut group changed"));
            }
            checked(
                host,
                "udevadm",
                &[
                    "trigger".into(),
                    "--action=change".into(),
                    "--".into(),
                    sysfs.as_os_str().into(),
                ],
            )?;
            checked(host, "udevadm", &["settle".into(), "--timeout=10".into()])?;
            Ok(())
        }
        Action::File {
            path,
            bytes,
            metadata,
        } => {
            let expected_gid = match path.to_str() {
                Some(LIVE_HOOK | "/etc/nut-cli/nut.toml") => Some(0),
                Some("/etc/nut-cli/operator.secret") => group_id(host, "nut-operators")?,
                _ => group_id(host, "nut")?,
            };
            if expected_gid != Some(metadata.gid) {
                return Err(ResourceError::Refused(
                    "managed file group is unavailable or changed",
                ));
            }
            ensure_parent(host, path)?;
            host.replace_file_atomic(path, bytes, metadata)?;
            Ok(())
        }
        Action::Fstab { bytes, metadata } => publish_fstab(host, transaction, bytes, metadata),
        Action::Retire { hook, upssched } => {
            let old = host
                .read_protected(Path::new(LEGACY_HOOK))?
                .ok_or(ResourceError::Refused("legacy hook disappeared"))?;
            if !recognized_legacy(&old, hook)
                || host.read_protected(Path::new(LIVE_HOOK))?.as_deref() != Some(hook)
                || host.metadata(Path::new(LIVE_HOOK))? != Some(FileMetadata::file(0, 0, 0o755))
                || host
                    .read_protected(Path::new(LIVE_UPSSCHED_CONF))?
                    .as_deref()
                    != Some(upssched)
                || !String::from_utf8_lossy(upssched)
                    .lines()
                    .any(|l| l.trim() == format!("CMDSCRIPT {LIVE_HOOK}"))
                || String::from_utf8_lossy(upssched).contains(LEGACY_HOOK)
            {
                return Err(ResourceError::Refused(
                    "legacy hook identity or replacement references are unproven",
                ));
            }
            // Also reject a retained direct reference in upsmon.conf.
            let monitor = host
                .read_protected(Path::new("/etc/nut/upsmon.conf"))?
                .ok_or(ResourceError::Refused("monitor configuration is missing"))?;
            if String::from_utf8_lossy(&monitor).contains(LEGACY_HOOK) {
                return Err(ResourceError::Refused(
                    "legacy hook still referenced by monitor",
                ));
            }
            host.remove_protected(Path::new(LEGACY_HOOK))?;
            Ok(())
        }
    }
}

fn ensure_parent(host: &impl Host, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(ResourceError::Refused("missing parent"))?;
    if host.metadata(parent)?.is_none() {
        if !matches!(
            parent.to_str(),
            Some("/etc/nut" | "/etc/nut-cli" | "/usr/local/sbin")
        ) {
            return Err(ResourceError::Refused("unmanaged parent directory"));
        }
        host.create_secure_dir(parent, &FileMetadata::dir(0, 0, 0o755))?;
    }
    Ok(())
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn file_state(host: &impl Host, path: &Path) -> Result<String> {
    Ok(transaction::file_fingerprint(host, path)?.unwrap_or_else(|| "absent".into()))
}
fn run(host: &impl Host, program: &str, args: &[OsString]) -> Result<ProcessOutput> {
    // Package output can contain conffile bytes. Never include captured output
    // or supplied argv in errors, including from an injected host failure.
    host.run_bounded(&ProcessRequest {
        program: OsStr::new(program),
        args,
        timeout: Duration::from_secs(if program == "apt-get" { 300 } else { 15 }),
        secret: true,
        inherit_tty: false,
    })
    .map_err(|_| ResourceError::Refused("resource command failed or timed out"))
}
fn checked(host: &impl Host, program: &str, args: &[OsString]) -> Result<ProcessOutput> {
    let output = run(host, program, args)?;
    if output.status != Some(0) {
        return Err(ResourceError::Refused("resource command failed"));
    }
    Ok(output)
}
fn validate_user(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 32
        || !value.as_bytes()[0].is_ascii_alphabetic() && value.as_bytes()[0] != b'_'
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(ResourceError::Refused("invalid Unix account name"));
    }
    Ok(())
}
fn group_id(host: &impl Host, group: &str) -> Result<Option<u32>> {
    let out = run(host, "getent", &["group".into(), group.into()])?;
    if out.status == Some(2) {
        return Ok(None);
    }
    if out.status != Some(0) {
        return Err(ResourceError::Refused("group lookup failed"));
    }
    let text = std::str::from_utf8(&out.stdout)
        .map_err(|_| ResourceError::Refused("invalid group record"))?;
    let fields: Vec<_> = text.trim_end().split(':').collect();
    if fields.len() != 4 || fields[0] != group || text.trim_end().contains('\n') {
        return Err(ResourceError::Refused("invalid group record"));
    }
    Ok(Some(
        fields[2]
            .parse()
            .map_err(|_| ResourceError::Refused("invalid group id"))?,
    ))
}
fn is_member(host: &impl Host, user: &str, group: &str) -> Result<bool> {
    let gid =
        group_id(host, group)?.ok_or(ResourceError::Refused("membership group is missing"))?;
    let out = checked(host, "id", &["--groups".into(), "--".into(), user.into()])?;
    let text = std::str::from_utf8(&out.stdout)
        .map_err(|_| ResourceError::Refused("invalid user groups"))?;
    let groups = text
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| ResourceError::Refused("invalid user groups"))?;
    if groups.is_empty() {
        return Err(ResourceError::Refused("user is missing"));
    }
    Ok(groups.contains(&gid))
}
fn validate_package(name: &str, version: &str) -> Result<()> {
    if name.is_empty()
        || !name.as_bytes()[0].is_ascii_lowercase() && !name.as_bytes()[0].is_ascii_digit()
        || !name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b':')
        })
        || version.is_empty()
        || !version.as_bytes()[0].is_ascii_digit()
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.' | b':' | b'~'))
    {
        return Err(ResourceError::Refused("invalid package identity"));
    }
    Ok(())
}
fn package_version(host: &impl Host, name: &str) -> Result<Option<String>> {
    let out = run(
        host,
        "dpkg-query",
        &[
            "-W".into(),
            "-f=${db:Status-Status} ${Version}".into(),
            "--".into(),
            name.into(),
        ],
    )?;
    if out.status == Some(1) && out.stdout.is_empty() {
        return Ok(None);
    }
    if out.status != Some(0) {
        return Err(ResourceError::Refused("package lookup failed"));
    }
    let text = std::str::from_utf8(&out.stdout)
        .map_err(|_| ResourceError::Refused("invalid package record"))?;
    let fields: Vec<_> = text.split_whitespace().collect();
    match fields.as_slice() {
        ["installed", version] => {
            validate_package(name, version)?;
            Ok(Some((*version).into()))
        }
        ["not-installed" | "config-files", ..] => Ok(None),
        _ => Err(ResourceError::Refused(
            "package is partially installed; repair dpkg first",
        )),
    }
}
fn package_fingerprint<'a>(items: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> String {
    digest(
        &items
            .map(|(n, v)| format!("{n}={}\n", v.unwrap_or("absent")))
            .collect::<String>(),
    )
}
fn apt_args(packages: &BTreeMap<String, String>, simulation: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--no-upgrade",
        "--no-remove",
        "--no-install-recommends",
        "-o",
        "Dpkg::Options::=--force-confold",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(
        if simulation {
            "--simulate"
        } else {
            "--assume-yes"
        }
        .into(),
    );
    args.extend(["install".into(), "--".into()]);
    args.extend(packages.iter().map(|(n, v)| format!("{n}={v}").into()));
    args
}
fn simulate(
    host: &impl Host,
    requested: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let output = checked(host, "apt-get", &apt_args(requested, true))?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| ResourceError::Refused("invalid apt simulation"))?;
    let mut result = BTreeMap::new();
    let mut configured = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        match fields.first().copied() {
            Some("Remv" | "Purg") => {
                return Err(ResourceError::Refused("apt solution removes packages"));
            }
            Some("Inst") => {
                if fields.len() < 3 || !fields[2].starts_with('(') {
                    return Err(ResourceError::Refused(
                        "apt solution upgrades or is malformed",
                    ));
                }
                let name = fields[1];
                let version = fields[2].trim_start_matches('(').trim_end_matches(')');
                validate_package(name, version)?;
                if result.insert(name.into(), version.into()).is_some() {
                    return Err(ResourceError::Refused("duplicate apt operation"));
                }
            }
            Some("Conf") => {
                if fields.len() < 2 {
                    return Err(ResourceError::Refused("invalid apt configuration"));
                }
                configured.push(fields[1]);
            }
            _ => (),
        }
    }
    if configured.iter().any(|n| !result.contains_key(*n)) {
        return Err(ResourceError::Refused(
            "apt solution configures an unplanned package",
        ));
    }
    Ok(result)
}

fn install_packages(
    host: &impl Host,
    transaction: &Transaction,
    packages: &BTreeMap<String, String>,
) -> Result<()> {
    let mut missing = BTreeMap::new();
    for (name, version) in packages {
        match package_version(host, name)? {
            None => {
                missing.insert(name.clone(), version.clone());
            }
            Some(current) if current == *version => (),
            Some(_) => {
                return Err(ResourceError::Refused(
                    "installed package differs from the bound version",
                ));
            }
        }
    }
    if missing.is_empty() {
        return restore_policy(host, transaction);
    }
    if simulate(host, &missing)? != missing {
        return Err(ResourceError::Refused("apt dependency solution changed"));
    }
    suppress_package_starts(host, transaction)?;
    let applied = checked(host, "apt-get", &apt_args(&missing, false));
    // Restore even on a normal apt failure. A hard interruption leaves the
    // transaction and exact owned guard for the next attempt to reconcile.
    let restored = restore_policy(host, transaction);
    restored?;
    applied?;
    Ok(())
}
fn suppress_package_starts(host: &impl Host, transaction: &Transaction) -> Result<()> {
    let original = transaction::original_file(host, transaction, Path::new(POLICY))?;
    let current = host.read_protected(Path::new(POLICY))?;
    match original {
        None => {
            if let Some(bytes) = current {
                if bytes != POLICY_BYTES
                    || host.metadata(Path::new(POLICY))? != Some(FileMetadata::file(0, 0, 0o755))
                {
                    return Err(ResourceError::Refused("package policy changed"));
                }
            } else {
                host.replace_file_atomic(
                    Path::new(POLICY),
                    POLICY_BYTES,
                    &FileMetadata::file(0, 0, 0o755),
                )?;
            }
        }
        Some((bytes, metadata)) => {
            if current.as_deref() != Some(&bytes)
                || host.metadata(Path::new(POLICY))?.as_ref() != Some(&metadata)
                || metadata.uid != 0
                || metadata.mode & 0o022 != 0
                || !metadata.executable()
            {
                return Err(ResourceError::Refused(
                    "unsafe or changed existing package policy",
                ));
            }
            // Preserve a preexisting policy byte-for-byte. A policy that permits
            // NUT startup is refused; do not replace another administrator's policy.
            for unit in [
                "nut",
                "nut-server",
                "nut-client",
                "nut-server.service",
                "nut-monitor.service",
                "nut-driver.service",
                "nut-driver-enumerator.service",
                "nut-driver@.service",
            ] {
                for action in ["start", "restart", "try-restart"] {
                    if run(host, POLICY, &[unit.into(), action.into()])?.status != Some(101) {
                        return Err(ResourceError::Refused(
                            "existing package policy permits NUT startup",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}
/// Restore an interrupted install's temporary policy without repeating apt.
pub fn restore_package_policy(host: &impl Host, transaction: &Transaction) -> Result<()> {
    restore_policy(host, transaction)
}
fn restore_policy(host: &impl Host, transaction: &Transaction) -> Result<()> {
    let original = transaction::original_file(host, transaction, Path::new(POLICY))?;
    let current = host.read_protected(Path::new(POLICY))?;
    match (original, current) {
        (None, Some(bytes))
            if bytes == POLICY_BYTES
                && host.metadata(Path::new(POLICY))? == Some(FileMetadata::file(0, 0, 0o755)) =>
        {
            host.remove_protected(Path::new(POLICY))?;
            Ok(())
        }
        (None, None) => Ok(()),
        (Some((before, metadata)), Some(bytes))
            if before == bytes && host.metadata(Path::new(POLICY))?.as_ref() == Some(&metadata) =>
        {
            Ok(())
        }
        _ => Err(ResourceError::Refused(
            "package policy changed; retained for reviewed recovery",
        )),
    }
}

fn device_fingerprint(
    sysfs: &Path,
    vendor: &str,
    product: &str,
    serial: Option<&str>,
    gid: u32,
) -> String {
    digest(&format!(
        "usb:{}:{vendor}:{product}:{serial:?}:{gid}",
        sysfs.display()
    ))
}

fn usb_node(
    host: &impl Host,
    sysfs: &Path,
    vendor: &str,
    product: &str,
    serial: Option<&str>,
) -> Result<PathBuf> {
    let read = |name: &str| -> Result<String> {
        let bytes = host
            .read_file(&sysfs.join(name))?
            .ok_or(ResourceError::Refused("USB identity is unavailable"))?;
        Ok(std::str::from_utf8(&bytes)
            .map_err(|_| ResourceError::Refused("invalid USB identity"))?
            .trim()
            .into())
    };
    if !read("idVendor")?.eq_ignore_ascii_case(vendor)
        || !read("idProduct")?.eq_ignore_ascii_case(product)
        || serial.is_some_and(|s| read("serial").ok().as_deref() != Some(s))
    {
        return Err(ResourceError::Refused("selected USB identity changed"));
    }
    let bus = read("busnum")?
        .parse::<u16>()
        .ok()
        .filter(|v| (1..=999).contains(v))
        .ok_or(ResourceError::Refused("invalid USB bus number"))?;
    let device = read("devnum")?
        .parse::<u16>()
        .ok()
        .filter(|v| (1..=999).contains(v))
        .ok_or(ResourceError::Refused("invalid USB device number"))?;
    Ok(PathBuf::from(format!("/dev/bus/usb/{bus:03}/{device:03}")))
}

pub fn nofail_candidate(before: &[u8], mounts: &[PathBuf]) -> Result<Vec<u8>> {
    let text =
        std::str::from_utf8(before).map_err(|_| ResourceError::Refused("fstab is not UTF-8"))?;
    let mut lines: Vec<String> = text.split_inclusive('\n').map(Into::into).collect();
    let wanted: std::collections::BTreeSet<_> = mounts.iter().collect();
    for mount in wanted {
        let target = mount
            .to_str()
            .filter(|m| m.starts_with('/') && !m.chars().any(char::is_whitespace))
            .ok_or(ResourceError::Refused("invalid fstab mount"))?;
        let mut matched = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with('#') {
                continue;
            }
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.get(1) == Some(&target) {
                if fields.len() < 4 {
                    return Err(ResourceError::Refused("invalid fstab entry"));
                }
                matched.push((index, fields[3].to_owned()));
            }
        }
        if matched.len() != 1 {
            return Err(ResourceError::Refused(
                "fstab mount is missing or ambiguous",
            ));
        }
        let (index, options) = &matched[0];
        if options.split(',').any(|o| o == "nofail") {
            continue;
        }
        let line = &lines[*index];
        // Locate the fourth token, preserving all spacing and comments.
        let mut tokens = line
            .match_indices(|c: char| !c.is_whitespace())
            .map(|(i, _)| i)
            .filter(|i| *i == 0 || line.as_bytes()[i - 1].is_ascii_whitespace());
        let start = tokens
            .nth(3)
            .ok_or(ResourceError::Refused("invalid fstab entry"))?;
        let end = start + options.len();
        lines[*index].insert_str(end, ",nofail");
    }
    Ok(lines.concat().into_bytes())
}
fn publish_fstab(
    host: &impl Host,
    transaction: &Transaction,
    bytes: &[u8],
    metadata: &FileMetadata,
) -> Result<()> {
    let candidate = PathBuf::from(format!("/etc/.nut-cli-fstab-{}", transaction.stamp));
    match host.read_protected(&candidate)? {
        Some(existing) if existing != bytes => {
            return Err(ResourceError::Refused("fstab validation file changed"));
        }
        Some(_) => {
            if host.metadata(&candidate)? != Some(FileMetadata::file(0, 0, 0o600)) {
                return Err(ResourceError::Refused("fstab candidate metadata changed"));
            }
        }
        None => host.replace_file_atomic(&candidate, bytes, &FileMetadata::file(0, 0, 0o600))?,
    }
    let validation = checked(
        host,
        "findmnt",
        &[
            "--verify".into(),
            "--tab-file".into(),
            candidate.as_os_str().into(),
        ],
    );
    let result = validation.and_then(|output| {
        let text = format!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .to_ascii_lowercase();
        let words: Vec<_> = text.split_whitespace().collect();
        for (index, word) in words.iter().enumerate() {
            if let Ok(count) = word.parse::<u64>()
                && count != 0
                && words.get(index + 1).is_some_and(|w| {
                    w.starts_with("error")
                        || *w == "parse"
                            && words.get(index + 2).is_some_and(|w| w.starts_with("error"))
                })
            {
                return Err(ResourceError::Refused("findmnt reported fstab errors"));
            }
        }
        if text.contains("[e]") || text.contains("parse error at") {
            return Err(ResourceError::Refused("findmnt reported fstab errors"));
        }
        if host.read_protected(&candidate)?.as_deref() != Some(bytes) {
            return Err(ResourceError::Refused(
                "fstab candidate changed during verification",
            ));
        }
        host.replace_file_atomic(Path::new(FSTAB_PATH), bytes, metadata)?;
        Ok(())
    });
    cleanup_fstab(host, transaction, bytes)?;
    result
}

fn cleanup_fstab(host: &impl Host, transaction: &Transaction, bytes: &[u8]) -> Result<()> {
    let candidate = PathBuf::from(format!("/etc/.nut-cli-fstab-{}", transaction.stamp));
    match host.read_protected(&candidate)? {
        None => Ok(()),
        Some(current)
            if current == bytes
                && host.metadata(&candidate)? == Some(FileMetadata::file(0, 0, 0o600)) =>
        {
            host.remove_protected(&candidate)?;
            Ok(())
        }
        Some(_) => Err(ResourceError::Refused(
            "fstab candidate changed; retained for review",
        )),
    }
}

pub(crate) fn recognized_legacy(old: &[u8], replacement: &[u8]) -> bool {
    let normalize = |bytes: &[u8]| -> Option<String> {
        Some(
            std::str::from_utf8(bytes)
                .ok()?
                .lines()
                .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    let Some(old) = normalize(old) else {
        return false;
    };
    let Some(replacement) = normalize(replacement) else {
        return false;
    };
    old.contains("ATHENA_UPSSCHED_TEST_PATH")
        && old
            .replace("ATHENA_UPSSCHED_TEST_PATH", "NUT_UPSSCHED_TEST_PATH")
            .replace("athena-upssched", "nut-upssched")
            == replacement
}

#[cfg(test)]
mod tests;
