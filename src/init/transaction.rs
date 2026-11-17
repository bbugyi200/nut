#![forbid(unsafe_code)]

//! Durable, secret-safe records for `nut init` transactions.
//!
//! A transaction's manifest and journal are published before its first target
//! effect. File preimages live in separate private blobs; journals and public
//! summaries contain references and hashes, never file contents.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::host::{ApplyLock, FileMetadata, Host, HostError};
use crate::init::observe::{
    FSTAB_PATH, LEGACY_HOOK, LIVE_HOOK, LIVE_NUT_CONF, LIVE_UPS_CONF, LIVE_UPSD_USERS,
    LIVE_UPSMON_CONF, LIVE_UPSSCHED_CONF, OPERATOR_SECRET_PATH, PENDING_PATH, STATE_PATH,
};

pub const BACKUP_ROOT: &str = "/var/backups/nut-cli";
/// The process-scoped writer lock shared by init and rollback.
pub const APPLY_LOCK_PATH: &str = "/var/lib/nut-cli.init.lock";
const STATE_DIR: &str = "/var/lib/nut-cli";
const MANIFEST_NAME: &str = "manifest.json";
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BACKUP_BYTES: usize = 64 * 1024 * 1024;
const MAX_OPERATIONS: usize = 512;
const MAX_TARGETS: usize = 128;
const MAX_SERVICES: usize = 32;
const MAX_MEMBERSHIPS: usize = 256;
const ROOT_PRIVATE_DIR: FileMetadata = FileMetadata {
    uid: 0,
    gid: 0,
    mode: 0o700,
    is_file: false,
    is_dir: true,
};
const ROOT_PRIVATE_FILE: FileMetadata = FileMetadata {
    uid: 0,
    gid: 0,
    mode: 0o600,
    is_file: true,
    is_dir: false,
};

#[derive(Debug, Error)]
pub enum TransactionError {
    #[error(transparent)]
    Host(#[from] HostError),
    #[error("invalid transaction record: {0}")]
    Invalid(&'static str),
    #[error("transaction is already in progress")]
    AlreadyPending,
    #[error("transaction does not match the pending journal")]
    WrongTransaction,
    #[error("a transaction operation is not at the requested checkpoint")]
    InvalidCheckpoint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeginRequest {
    /// The authoritative, reviewed plan hash. It is stored for recovery
    /// diagnostics but is never treated as proof that the caller re-observed.
    pub plan_hash: String,
    /// Every managed file that an operation may replace, remove, or create.
    /// Absent paths are included in the manifest as absent markers.
    pub targets: Vec<PathBuf>,
    pub services: Vec<ServiceSnapshot>,
    /// Only additions made by this transaction are recorded.
    pub membership_additions: Vec<MembershipAddition>,
    /// The complete ordered operation list is durable before effects begin.
    pub operations: Vec<OperationIntent>,
}

impl BeginRequest {
    /// Whether this request would make any persistent change. Empty plans do
    /// not create a lock, backup, journal, state file, or consume entropy.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
            && self.services.is_empty()
            && self.membership_additions.is_empty()
            && self.operations.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServiceSnapshot {
    pub unit: String,
    pub active: String,
    pub enabled: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MembershipAddition {
    pub user: String,
    pub group: String,
}

#[derive(Clone, Eq, PartialEq)]
pub struct OperationIntent {
    pub id: String,
    /// File operations are reconciled directly from disk. Package, service,
    /// and account operations use a caller-supplied fingerprint at recovery.
    pub target: Option<PathBuf>,
    pub before: String,
    pub after: String,
}

impl std::fmt::Debug for OperationIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationIntent")
            .field("id", &self.id)
            .field("target", &self.target)
            .field("before", &"[redacted]")
            .field("after", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Checkpoint {
    Started,
    Completed,
}

pub struct Transaction {
    pub stamp: String,
    pub plan_hash: String,
    writer_lock: WriterLock,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("stamp", &self.stamp)
            .field("plan_hash", &self.plan_hash)
            .finish()
    }
}

impl PartialEq for Transaction {
    fn eq(&self, other: &Self) -> bool {
        self.stamp == other.stamp && self.plan_hash == other.plan_hash
    }
}

impl Eq for Transaction {}

/// Opaque proof that the shared writer lock was acquired through `Host`.
pub struct WriterLock {
    guard: Box<dyn ApplyLock>,
}

impl std::fmt::Debug for WriterLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterLock")
            .field("path", &self.guard.path())
            .finish()
    }
}

/// Content returned from durable secret materialization deliberately has no
/// `Debug`, `Display`, or serialization implementation.
pub struct SecretMaterial {
    bytes: Vec<u8>,
}

impl SecretMaterial {
    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackupSummary {
    pub stamp: String,
    pub files: Vec<BackupFileSummary>,
    pub services: Vec<ServiceSnapshot>,
    pub membership_additions: Vec<MembershipAddition>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackupFileSummary {
    pub path: PathBuf,
    pub action: BackupAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupAction {
    Restore,
    Remove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconciledOperation {
    NotStarted,
    Before,
    After,
    NeedsObservation,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationRecovery {
    pub id: String,
    pub checkpoint: CheckpointState,
    pub observed: ReconciledOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointState {
    Planned,
    Started,
    Completed,
}

#[derive(Debug, Eq, PartialEq)]
pub struct RecoveryReport {
    pub transaction: Transaction,
    pub backup: BackupSummary,
    pub operations: Vec<OperationRecovery>,
    /// The state record already names this transaction. This is the crash
    /// window after state publication and before pending-journal removal.
    pub state_finalized: bool,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRecord {
    pub schema_version: u32,
    pub config_hash: String,
    pub profile_hash: String,
    pub outputs: BTreeMap<PathBuf, OutputRecord>,
    pub managed_identities: BTreeMap<String, String>,
    pub backup_reference: String,
    pub transaction_outcome: TransactionOutcome,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRecord {
    pub sha256: String,
    pub metadata: FileMetadataRecord,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileMetadataRecord {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

impl From<&FileMetadata> for FileMetadataRecord {
    fn from(value: &FileMetadata) -> Self {
        Self {
            uid: value.uid,
            gid: value.gid,
            mode: value.mode,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionOutcome {
    Applied,
    Recovered,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    stamp: String,
    plan_hash: String,
    files: Vec<ManifestFile>,
    services: Vec<ServiceSnapshot>,
    membership_additions: Vec<MembershipAddition>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum ManifestFile {
    Absent {
        path: PathBuf,
    },
    Present {
        path: PathBuf,
        blob: String,
        sha256: String,
        byte_len: usize,
        metadata: FileMetadataRecord,
    },
}

impl ManifestFile {
    fn path(&self) -> &Path {
        match self {
            Self::Absent { path } | Self::Present { path, .. } => path,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRecord {
    schema_version: u32,
    stamp: String,
    plan_hash: String,
    manifest_sha256: String,
    operations: Vec<OperationRecord>,
    state_finalization: StateFinalization,
    secrets: BTreeMap<SecretPurpose, SecretReference>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "status", rename_all = "snake_case")]
enum StateFinalization {
    Awaiting,
    Intent { state_sha256: String },
    Published { state_sha256: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationRecord {
    id: String,
    target: Option<PathBuf>,
    before: String,
    after: String,
    checkpoint: CheckpointState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretReference {
    file: String,
    sha256: String,
    byte_len: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretPurpose {
    Upsmon,
    Operator,
}

impl SecretPurpose {
    fn file_name(self) -> &'static str {
        match self {
            Self::Upsmon => "secret-upsmon.bin",
            Self::Operator => "secret-operator.bin",
        }
    }
}

/// Write every preimage and the complete pending journal before returning a
/// transaction that may be used for target effects. `Ok(None)` is the
/// strictly effect-free empty-plan path.
pub fn begin<H: Host>(
    host: &H,
    request: &BeginRequest,
) -> Result<Option<Transaction>, TransactionError> {
    if request.is_empty() {
        return Ok(None);
    }
    let writer_lock = acquire_writer_lock(host)?;
    begin_locked(host, writer_lock, request)
}

/// Acquire the shared process-scoped init/rollback writer lock. Callers that
/// need to re-observe and compare a reviewed plan under the lock should keep
/// this guard and pass it to [`begin_locked`].
pub fn acquire_writer_lock<H: Host>(host: &H) -> Result<WriterLock, TransactionError> {
    require_root(host)?;
    Ok(WriterLock {
        guard: host.lock_exclusive(Path::new(APPLY_LOCK_PATH))?,
    })
}

/// Begin a transaction while transferring an already-held shared writer lock
/// into its handle. This permits root callers to re-observe and hash-check
/// under the lock before entropy, backups, or target effects occur.
pub fn begin_locked<H: Host>(
    host: &H,
    writer_lock: WriterLock,
    request: &BeginRequest,
) -> Result<Option<Transaction>, TransactionError> {
    if request.is_empty() {
        return Ok(None);
    }
    require_root(host)?;
    require_writer_lock(&writer_lock)?;
    validate_begin_request(request)?;
    host.create_secure_dir(Path::new(STATE_DIR), &ROOT_PRIVATE_DIR)?;
    if host.read_protected(Path::new(PENDING_PATH))?.is_some() {
        return Err(TransactionError::AlreadyPending);
    }

    // Observe all targets before creating the backup directory. A failed
    // preflight therefore leaves no partial transaction behind.
    let mut sorted_targets = request.targets.clone();
    sorted_targets.sort();
    let mut snapshots = Vec::with_capacity(sorted_targets.len());
    for target in &sorted_targets {
        let bytes = host.read_protected(target)?;
        let metadata = host.metadata(target)?;
        let (snapshot, saved_bytes) = match (bytes, metadata) {
            (None, None) => (
                ManifestFile::Absent {
                    path: target.clone(),
                },
                None,
            ),
            (Some(bytes), Some(metadata)) if metadata.is_file && !metadata.is_dir => {
                if bytes.len() > MAX_FILE_BYTES {
                    return Err(TransactionError::Invalid(
                        "target exceeds the backup size limit",
                    ));
                }
                let verify = host
                    .read_protected(target)?
                    .ok_or(TransactionError::Invalid(
                        "target disappeared during backup preflight",
                    ))?;
                if verify != bytes {
                    return Err(TransactionError::Invalid(
                        "target changed during backup preflight",
                    ));
                }
                let verify_metadata = host.metadata(target)?.ok_or(TransactionError::Invalid(
                    "target disappeared during backup preflight",
                ))?;
                if verify_metadata != metadata {
                    return Err(TransactionError::Invalid(
                        "target metadata changed during backup preflight",
                    ));
                }
                (
                    ManifestFile::Present {
                        path: target.clone(),
                        blob: String::new(),
                        sha256: sha256(&bytes),
                        byte_len: bytes.len(),
                        metadata: FileMetadataRecord::from(&metadata),
                    },
                    Some(bytes),
                )
            }
            (None, Some(_)) => {
                return Err(TransactionError::Invalid(
                    "target metadata exists without a readable regular file",
                ));
            }
            (Some(_), None) => {
                return Err(TransactionError::Invalid(
                    "target contents exist without file metadata",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(TransactionError::Invalid("target is not a regular file"));
            }
        };
        snapshots.push((snapshot, saved_bytes));
    }
    let total_bytes = snapshots
        .iter()
        .filter_map(|(_, bytes)| bytes.as_ref())
        .map(Vec::len)
        .sum::<usize>();
    if total_bytes > MAX_TOTAL_BACKUP_BYTES {
        return Err(TransactionError::Invalid(
            "transaction backup exceeds the size limit",
        ));
    }
    validate_operation_preimages(&snapshots, &request.operations)?;

    let stamp = unique_stamp(host)?;
    let backup_dir = backup_dir(&stamp);
    host.create_secure_dir(Path::new(BACKUP_ROOT), &ROOT_PRIVATE_DIR)?;
    host.create_secure_dir(&backup_dir, &ROOT_PRIVATE_DIR)?;

    let mut files = Vec::with_capacity(snapshots.len());
    for (index, (mut snapshot, bytes)) in snapshots.into_iter().enumerate() {
        if let Some(bytes) = bytes {
            let blob = format!("file-{index:04}.bin");
            let path = backup_dir.join(&blob);
            host.replace_file_atomic(&path, &bytes, &ROOT_PRIVATE_FILE)?;
            if let ManifestFile::Present { blob: name, .. } = &mut snapshot {
                *name = blob;
            }
        }
        files.push(snapshot);
    }
    let manifest = Manifest {
        schema_version: 1,
        stamp: stamp.clone(),
        plan_hash: request.plan_hash.clone(),
        files,
        services: sorted_services(&request.services),
        membership_additions: sorted_memberships(&request.membership_additions),
    };
    let manifest_bytes = encode(&manifest)?;
    let manifest_path = backup_dir.join(MANIFEST_NAME);
    host.replace_file_atomic(&manifest_path, &manifest_bytes, &ROOT_PRIVATE_FILE)?;

    let operations = request
        .operations
        .iter()
        .map(|operation| OperationRecord {
            id: operation.id.clone(),
            target: operation.target.clone(),
            before: operation.before.clone(),
            after: operation.after.clone(),
            checkpoint: CheckpointState::Planned,
        })
        .collect();
    let pending = PendingRecord {
        schema_version: 1,
        stamp: stamp.clone(),
        plan_hash: request.plan_hash.clone(),
        manifest_sha256: sha256(&manifest_bytes),
        operations,
        state_finalization: StateFinalization::Awaiting,
        secrets: BTreeMap::new(),
    };
    host.replace_file_atomic(
        Path::new(PENDING_PATH),
        &encode(&pending)?,
        &ROOT_PRIVATE_FILE,
    )?;
    Ok(Some(Transaction {
        stamp,
        plan_hash: request.plan_hash.clone(),
        writer_lock,
    }))
}

/// Persist the intent/completion boundary around one planned operation.
pub fn checkpoint<H: Host>(
    host: &H,
    transaction: &Transaction,
    operation_id: &str,
    checkpoint: Checkpoint,
) -> Result<(), TransactionError> {
    require_root(host)?;
    require_transaction_lock(transaction)?;
    let mut pending = load_pending(host)?;
    ensure_transaction(&pending, transaction)?;
    let operation = pending
        .operations
        .iter_mut()
        .find(|operation| operation.id == operation_id)
        .ok_or(TransactionError::InvalidCheckpoint)?;
    match (operation.checkpoint, checkpoint) {
        (CheckpointState::Planned, Checkpoint::Started) => {
            operation.checkpoint = CheckpointState::Started;
        }
        (CheckpointState::Started, Checkpoint::Completed) => {
            operation.checkpoint = CheckpointState::Completed;
        }
        (CheckpointState::Started, Checkpoint::Started)
        | (CheckpointState::Completed, Checkpoint::Completed) => return Ok(()),
        _ => return Err(TransactionError::InvalidCheckpoint),
    }
    save_pending(host, &pending)
}

/// Inspect a journaled operation while retaining the writer lock. Resource
/// executors use this instead of reacquiring the process lock on retries.
pub(crate) fn operation_record<H: Host>(
    host: &H,
    transaction: &Transaction,
    id: &str,
) -> Result<(OperationIntent, CheckpointState), TransactionError> {
    require_root(host)?;
    require_transaction_lock(transaction)?;
    let pending = load_pending(host)?;
    ensure_transaction(&pending, transaction)?;
    read_and_validate_manifest(host, &pending)?;
    let op = pending
        .operations
        .iter()
        .find(|op| op.id == id)
        .ok_or(TransactionError::InvalidCheckpoint)?;
    Ok((
        OperationIntent {
            id: op.id.clone(),
            target: op.target.clone(),
            before: op.before.clone(),
            after: op.after.clone(),
        },
        op.checkpoint,
    ))
}

pub(crate) fn require_membership_addition<H: Host>(
    host: &H,
    transaction: &Transaction,
    user: &str,
    group: &str,
) -> Result<(), TransactionError> {
    let pending = load_pending(host)?;
    ensure_transaction(&pending, transaction)?;
    let (manifest, _) = read_and_validate_manifest(host, &pending)?;
    if !manifest
        .membership_additions
        .iter()
        .any(|a| a.user == user && a.group == group)
    {
        return Err(TransactionError::Invalid(
            "membership addition is missing from backup",
        ));
    }
    Ok(())
}

/// Read a validated original preimage. The returned bytes are deliberately
/// not Debug/Serialize; package policy restoration must retain exact bytes.
pub(crate) fn original_file<H: Host>(
    host: &H,
    transaction: &Transaction,
    path: &Path,
) -> Result<Option<(Vec<u8>, FileMetadata)>, TransactionError> {
    require_root(host)?;
    require_transaction_lock(transaction)?;
    let pending = load_pending(host)?;
    ensure_transaction(&pending, transaction)?;
    let (manifest, _) = read_and_validate_manifest(host, &pending)?;
    match manifest
        .files
        .iter()
        .find(|f| f.path() == path)
        .ok_or(TransactionError::Invalid("resource is missing its backup"))?
    {
        ManifestFile::Absent { .. } => Ok(None),
        ManifestFile::Present {
            blob,
            metadata,
            byte_len,
            sha256: expected,
            ..
        } => {
            let blob_path = backup_dir(&transaction.stamp).join(blob);
            verify_private_file(host, &blob_path)?;
            let bytes = host
                .read_protected(&blob_path)?
                .ok_or(TransactionError::Invalid("backup blob missing"))?;
            if bytes.len() != *byte_len || sha256(&bytes) != *expected {
                return Err(TransactionError::Invalid("backup blob changed"));
            }
            Ok(Some((
                bytes,
                FileMetadata::file(metadata.uid, metadata.gid, metadata.mode),
            )))
        }
    }
}

/// Bind a reserved resource once, after durable secret materialization or
/// package creation of a group. The original backup remains immutable.
/// Deferred resources are external operations: recovery must provide their
/// observed fingerprints, just like account and package operations.
pub(crate) fn bind_resource_operation<H: Host>(
    host: &H,
    transaction: &Transaction,
    reserved: &OperationIntent,
    bound: &OperationIntent,
    targets: &[PathBuf],
) -> Result<(), TransactionError> {
    require_root(host)?;
    require_transaction_lock(transaction)?;
    validate_operation_id(&bound.id)?;
    validate_fingerprint(&bound.before)?;
    validate_fingerprint(&bound.after)?;
    if bound.id != reserved.id || bound.target.is_some() || bound.before == bound.after {
        return Err(TransactionError::Invalid("invalid resource binding"));
    }
    let mut pending = load_pending(host)?;
    ensure_transaction(&pending, transaction)?;
    let (manifest, _) = read_and_validate_manifest(host, &pending)?;
    for target in targets {
        validate_target(target)?;
        if !manifest.files.iter().any(|f| f.path() == target) {
            return Err(TransactionError::Invalid("resource is missing its backup"));
        }
    }
    let op = pending
        .operations
        .iter_mut()
        .find(|op| op.id == bound.id)
        .ok_or(TransactionError::InvalidCheckpoint)?;
    if op.target == bound.target && op.before == bound.before && op.after == bound.after {
        return Ok(());
    }
    if op.checkpoint != CheckpointState::Planned
        || op.target != reserved.target
        || op.before != reserved.before
        || op.after != reserved.after
    {
        return Err(TransactionError::InvalidCheckpoint);
    }
    op.before.clone_from(&bound.before);
    op.after.clone_from(&bound.after);
    save_pending(host, &pending)
}

/// Generate a secret once, publish it privately, then bind its name, size,
/// and hash into the pending journal. A retry reads the existing blob and
/// does not draw more entropy.
pub fn materialize_secret<H: Host>(
    host: &H,
    transaction: &Transaction,
    purpose: SecretPurpose,
    entropy_bytes: usize,
) -> Result<SecretMaterial, TransactionError> {
    require_root(host)?;
    require_transaction_lock(transaction)?;
    if !(16..=64).contains(&entropy_bytes) {
        return Err(TransactionError::Invalid(
            "secret entropy size is out of range",
        ));
    }
    let mut pending = load_pending(host)?;
    ensure_transaction(&pending, transaction)?;
    let directory = backup_dir(&transaction.stamp);
    let path = directory.join(purpose.file_name());
    let expected_len = entropy_bytes * 2;

    if let Some(reference) = pending.secrets.get(&purpose) {
        if reference.file != purpose.file_name() || reference.byte_len != expected_len {
            return Err(TransactionError::Invalid("secret reference is malformed"));
        }
        let bytes = read_private_blob(host, &path, expected_len)?;
        if sha256(&bytes) != reference.sha256 {
            return Err(TransactionError::Invalid("materialized secret is corrupt"));
        }
        return Ok(SecretMaterial { bytes });
    }

    // A crash after atomic blob publication but before the journal update
    // leaves an adoptable complete secret. Atomic publication means a
    // truncated secret cannot be mistaken for one.
    let bytes = match host.read_protected(&path)? {
        Some(bytes) => {
            verify_private_file(host, &path)?;
            if bytes.len() != expected_len || !bytes.iter().all(u8::is_ascii_hexdigit) {
                return Err(TransactionError::Invalid("orphan secret blob is malformed"));
            }
            bytes
        }
        None => {
            let entropy = host.random_bytes(entropy_bytes)?;
            if entropy.len() != entropy_bytes {
                return Err(TransactionError::Invalid(
                    "host returned incomplete entropy",
                ));
            }
            let bytes = entropy
                .iter()
                .flat_map(|byte| [hex_digit(byte >> 4), hex_digit(byte & 0x0f)])
                .collect::<Vec<_>>();
            host.replace_file_atomic(&path, &bytes, &ROOT_PRIVATE_FILE)?;
            bytes
        }
    };
    pending.secrets.insert(
        purpose,
        SecretReference {
            file: purpose.file_name().to_owned(),
            sha256: sha256(&bytes),
            byte_len: bytes.len(),
        },
    );
    save_pending(host, &pending)?;
    Ok(SecretMaterial { bytes })
}

/// Validate the pending journal, the complete manifest, and all referenced
/// blobs. Missing external-operation observations stay unknown; they are
/// never treated as proof that an operation did not run.
pub fn recover<H: Host>(host: &H) -> Result<Option<RecoveryReport>, TransactionError> {
    recover_with_observations(host, &BTreeMap::new())
}

pub fn recover_with_observations<H: Host>(
    host: &H,
    observations: &BTreeMap<String, String>,
) -> Result<Option<RecoveryReport>, TransactionError> {
    require_root(host)?;
    if !host.path_exists(Path::new(PENDING_PATH))? {
        return Ok(None);
    }
    let writer_lock = acquire_writer_lock(host)?;
    recover_locked(host, writer_lock, observations)
}

/// Recover while transferring an already-held writer lock into the returned
/// transaction handle.
pub fn recover_locked<H: Host>(
    host: &H,
    writer_lock: WriterLock,
    observations: &BTreeMap<String, String>,
) -> Result<Option<RecoveryReport>, TransactionError> {
    require_root(host)?;
    require_writer_lock(&writer_lock)?;
    for (operation_id, fingerprint) in observations {
        validate_operation_id(operation_id)?;
        validate_fingerprint(fingerprint)?;
    }
    let Some(pending_bytes) = host.read_protected(Path::new(PENDING_PATH))? else {
        return Ok(None);
    };
    verify_private_file(host, Path::new(PENDING_PATH))?;
    if pending_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(TransactionError::Invalid(
            "pending journal exceeds the size limit",
        ));
    }
    let pending: PendingRecord = decode(&pending_bytes)?;
    validate_pending(&pending)?;
    let (manifest, backup) = read_and_validate_manifest(host, &pending)?;
    let _ = manifest;

    for (purpose, reference) in &pending.secrets {
        if reference.file != purpose.file_name() || !(32..=128).contains(&reference.byte_len) {
            return Err(TransactionError::Invalid(
                "pending secret reference is malformed",
            ));
        }
        if let Some(bytes) =
            host.read_protected(&backup_dir(&pending.stamp).join(&reference.file))?
        {
            verify_private_file(host, &backup_dir(&pending.stamp).join(&reference.file))?;
            if bytes.len() != reference.byte_len || sha256(&bytes) != reference.sha256 {
                return Err(TransactionError::Invalid("materialized secret is corrupt"));
            }
        } else {
            return Err(TransactionError::Invalid("materialized secret is missing"));
        }
    }

    let state_finalized = state_finalized_for_pending(host, &pending)?;
    let mut operations = Vec::with_capacity(pending.operations.len());
    for operation in pending.operations {
        let current = match operation.target.as_deref() {
            Some(path) => {
                Some(file_fingerprint(host, path)?.unwrap_or_else(|| "absent".to_owned()))
            }
            None => observations.get(&operation.id).cloned(),
        };
        let observed = match (operation.checkpoint, current) {
            (CheckpointState::Planned, Some(value)) if value == operation.before => {
                ReconciledOperation::NotStarted
            }
            (CheckpointState::Planned, Some(_)) => ReconciledOperation::Conflict,
            (CheckpointState::Planned, None) => ReconciledOperation::NeedsObservation,
            (CheckpointState::Started, Some(value)) if value == operation.before => {
                ReconciledOperation::Before
            }
            (CheckpointState::Started, Some(value)) if value == operation.after => {
                ReconciledOperation::After
            }
            (CheckpointState::Started, Some(_)) => ReconciledOperation::Conflict,
            (CheckpointState::Started, None) => ReconciledOperation::NeedsObservation,
            (CheckpointState::Completed, Some(value)) if value == operation.after => {
                ReconciledOperation::After
            }
            (CheckpointState::Completed, _) => ReconciledOperation::Conflict,
        };
        operations.push(OperationRecovery {
            id: operation.id,
            checkpoint: operation.checkpoint,
            observed,
        });
    }
    Ok(Some(RecoveryReport {
        transaction: Transaction {
            stamp: pending.stamp,
            plan_hash: pending.plan_hash,
            writer_lock,
        },
        backup,
        operations,
        state_finalized,
    }))
}

/// Publish the successful state record, then remove the pending journal. If
/// interrupted between those operations, a retry observes the same state
/// bytes and only removes the journal, preserving the original backup.
pub fn finalize<H: Host>(
    host: &H,
    transaction: &Transaction,
    state: &StateRecord,
) -> Result<(), TransactionError> {
    require_root(host)?;
    require_transaction_lock(transaction)?;
    let mut pending = load_pending(host)?;
    ensure_transaction(&pending, transaction)?;
    validate_state(state, &transaction.stamp)?;
    if pending
        .operations
        .iter()
        .any(|operation| operation.checkpoint != CheckpointState::Completed)
    {
        return Err(TransactionError::InvalidCheckpoint);
    }
    let (manifest, _) = read_and_validate_manifest(host, &pending)?;
    let state_sha256 = sha256(&encode(state)?);
    let state_finalized = state_finalized_for_pending(host, &pending)?;
    match &pending.state_finalization {
        StateFinalization::Awaiting => {
            pending.state_finalization = StateFinalization::Intent {
                state_sha256: state_sha256.clone(),
            };
            save_pending(host, &pending)?;
        }
        StateFinalization::Intent {
            state_sha256: expected,
        }
        | StateFinalization::Published {
            state_sha256: expected,
        } if expected != &state_sha256 => {
            return Err(TransactionError::Invalid(
                "state record differs from the durable finalization intent",
            ));
        }
        StateFinalization::Intent { .. } | StateFinalization::Published { .. } => {}
    }
    let bytes = encode(state)?;
    host.create_secure_dir(Path::new(STATE_DIR), &ROOT_PRIVATE_DIR)?;
    let existing = host.read_protected(Path::new(STATE_PATH))?;
    let existing_metadata = host.metadata(Path::new(STATE_PATH))?;
    let already_finalized = existing.as_deref() == Some(bytes.as_slice())
        && existing_metadata.as_ref() == Some(&ROOT_PRIVATE_FILE);
    if !already_finalized {
        if state_finalized
            || matches!(
                pending.state_finalization,
                StateFinalization::Published { .. }
            )
        {
            return Err(TransactionError::Invalid(
                "published state record changed during the transaction",
            ));
        }
        validate_state_preimage(
            host,
            &manifest,
            existing.as_deref(),
            existing_metadata.as_ref(),
        )?;
        host.replace_file_atomic(Path::new(STATE_PATH), &bytes, &ROOT_PRIVATE_FILE)?;
    }
    if !matches!(
        pending.state_finalization,
        StateFinalization::Published { .. }
    ) {
        pending.state_finalization = StateFinalization::Published { state_sha256 };
        save_pending(host, &pending)?;
    }
    host.remove_protected(Path::new(PENDING_PATH))?;
    Ok(())
}

/// Create a stable private fingerprint for a regular target file. `None`
/// means the target is absent; no content is returned or serialized.
pub fn file_fingerprint<H: Host>(
    host: &H,
    path: &Path,
) -> Result<Option<String>, TransactionError> {
    validate_target(path)?;
    match host.read_protected(path)? {
        None => Ok(None),
        Some(bytes) => {
            let metadata = host.metadata(path)?.ok_or(TransactionError::Invalid(
                "file disappeared during observation",
            ))?;
            if !metadata.is_file || metadata.is_dir {
                return Err(TransactionError::Invalid("target is not a regular file"));
            }
            if host.read_protected(path)?.as_deref() != Some(bytes.as_slice())
                || host.metadata(path)?.as_ref() != Some(&metadata)
            {
                return Err(TransactionError::Invalid(
                    "target changed during protected observation",
                ));
            }
            Ok(Some(fingerprint(&bytes, &metadata)))
        }
    }
}

/// Calculate a predicted file fingerprint using the metadata the atomic
/// publisher will install.
pub fn predicted_file_fingerprint(bytes: &[u8], metadata: &FileMetadata) -> String {
    fingerprint(bytes, metadata)
}

fn fingerprint(bytes: &[u8], metadata: &FileMetadata) -> String {
    let mut digest = Sha256::new();
    digest.update(b"nut-init-file-v1\0");
    digest.update(metadata.uid.to_be_bytes());
    digest.update(metadata.gid.to_be_bytes());
    digest.update(metadata.mode.to_be_bytes());
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn validate_begin_request(request: &BeginRequest) -> Result<(), TransactionError> {
    validate_hash(&request.plan_hash)?;
    if request.targets.len() > MAX_TARGETS || request.operations.len() > MAX_OPERATIONS {
        return Err(TransactionError::Invalid(
            "transaction exceeds the record size limit",
        ));
    }
    if request.services.len() > MAX_SERVICES || request.membership_additions.len() > MAX_MEMBERSHIPS
    {
        return Err(TransactionError::Invalid(
            "transaction metadata exceeds the size limit",
        ));
    }
    let mut targets = BTreeSet::<PathBuf>::new();
    for target in &request.targets {
        validate_target(target)?;
        if !targets.insert(target.clone()) {
            return Err(TransactionError::Invalid(
                "transaction contains duplicate targets",
            ));
        }
    }
    if !targets.contains(Path::new(STATE_PATH)) {
        return Err(TransactionError::Invalid(
            "transaction backup must include the state record preimage",
        ));
    }
    let mut service_names = BTreeSet::new();
    for service in &request.services {
        validate_unit(&service.unit)?;
        if !matches!(service.active.as_str(), "active" | "inactive" | "failed") {
            return Err(TransactionError::Invalid(
                "transaction service activity is not a stable state",
            ));
        }
        if !matches!(
            service.enabled.as_str(),
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
            return Err(TransactionError::Invalid(
                "transaction service enablement is not a recognized state",
            ));
        }
        if !service_names.insert(&service.unit) {
            return Err(TransactionError::Invalid(
                "transaction contains duplicate services",
            ));
        }
    }
    let mut memberships = BTreeSet::new();
    for addition in &request.membership_additions {
        validate_username(&addition.user)?;
        if !matches!(addition.group.as_str(), "nut-operators" | "systemd-journal") {
            return Err(TransactionError::Invalid(
                "transaction has an unknown membership group",
            ));
        }
        if !memberships.insert((&addition.user, &addition.group)) {
            return Err(TransactionError::Invalid(
                "transaction contains duplicate memberships",
            ));
        }
    }
    let mut operation_ids = BTreeSet::new();
    for operation in &request.operations {
        validate_operation_id(&operation.id)?;
        if !operation_ids.insert(&operation.id) || operation.before == operation.after {
            return Err(TransactionError::Invalid(
                "transaction operation is ambiguous",
            ));
        }
        validate_fingerprint(&operation.before)?;
        validate_fingerprint(&operation.after)?;
        if let Some(target) = operation.target.as_deref() {
            validate_target(target)?;
            if !targets.contains(target) {
                return Err(TransactionError::Invalid(
                    "file operation target is not included in the backup manifest",
                ));
            }
        }
    }
    Ok(())
}

fn validate_operation_preimages(
    snapshots: &[(ManifestFile, Option<Vec<u8>>)],
    operations: &[OperationIntent],
) -> Result<(), TransactionError> {
    for operation in operations {
        let Some(target) = operation.target.as_deref() else {
            continue;
        };
        let (snapshot, contents) = snapshots
            .iter()
            .find(|(snapshot, _)| snapshot.path() == target)
            .ok_or(TransactionError::Invalid(
                "file operation target is not included in the backup manifest",
            ))?;
        let actual = match (snapshot, contents) {
            (ManifestFile::Absent { .. }, None) => "absent".to_owned(),
            (ManifestFile::Present { metadata, .. }, Some(contents)) => fingerprint(
                contents,
                &FileMetadata {
                    uid: metadata.uid,
                    gid: metadata.gid,
                    mode: metadata.mode,
                    is_file: true,
                    is_dir: false,
                },
            ),
            _ => {
                return Err(TransactionError::Invalid(
                    "backup preimage does not match target state",
                ));
            }
        };
        if operation.before != actual {
            return Err(TransactionError::Invalid(
                "operation preimage changed before the transaction began",
            ));
        }
    }
    Ok(())
}

fn validate_pending(pending: &PendingRecord) -> Result<(), TransactionError> {
    if pending.schema_version != 1 {
        return Err(TransactionError::Invalid(
            "unsupported pending journal schema",
        ));
    }
    validate_stamp(&pending.stamp)?;
    validate_hash(&pending.plan_hash)?;
    validate_hash(&pending.manifest_sha256)?;
    if pending.operations.len() > MAX_OPERATIONS {
        return Err(TransactionError::Invalid(
            "pending journal exceeds the operation limit",
        ));
    }
    match &pending.state_finalization {
        StateFinalization::Awaiting => {}
        StateFinalization::Intent { state_sha256 }
        | StateFinalization::Published { state_sha256 } => validate_hash(state_sha256)?,
    }
    let mut ids = BTreeSet::new();
    for operation in &pending.operations {
        validate_operation_id(&operation.id)?;
        if !ids.insert(&operation.id) {
            return Err(TransactionError::Invalid(
                "pending journal has duplicate operation IDs",
            ));
        }
        validate_fingerprint(&operation.before)?;
        validate_fingerprint(&operation.after)?;
        if operation.before == operation.after {
            return Err(TransactionError::Invalid(
                "pending operation has no state change",
            ));
        }
        if let Some(path) = operation.target.as_deref() {
            validate_target(path)?;
        }
    }
    for (purpose, reference) in &pending.secrets {
        if reference.file != purpose.file_name()
            || !(32..=128).contains(&reference.byte_len)
            || !reference.byte_len.is_multiple_of(2)
        {
            return Err(TransactionError::Invalid(
                "pending secret reference is malformed",
            ));
        }
        validate_hash(&reference.sha256)?;
    }
    Ok(())
}

fn read_and_validate_manifest<H: Host>(
    host: &H,
    pending: &PendingRecord,
) -> Result<(Manifest, BackupSummary), TransactionError> {
    let directory = backup_dir(&pending.stamp);
    verify_private_dir(host, Path::new(BACKUP_ROOT))?;
    verify_private_dir(host, &directory)?;
    let manifest_path = directory.join(MANIFEST_NAME);
    let bytes = host
        .read_protected(&manifest_path)?
        .ok_or(TransactionError::Invalid("backup manifest is missing"))?;
    verify_private_file(host, &manifest_path)?;
    if bytes.len() > MAX_MANIFEST_BYTES || sha256(&bytes) != pending.manifest_sha256 {
        return Err(TransactionError::Invalid(
            "backup manifest hash or size is invalid",
        ));
    }
    let manifest: Manifest = decode(&bytes)?;
    validate_manifest(&manifest, pending)?;
    let mut total_bytes = 0usize;
    let mut expected_names = BTreeSet::from([MANIFEST_NAME.to_owned()]);
    let mut files = Vec::with_capacity(manifest.files.len());
    let mut preimage_fingerprints = BTreeMap::new();
    for (index, entry) in manifest.files.iter().enumerate() {
        let path = entry.path();
        match entry {
            ManifestFile::Absent { .. } => {
                preimage_fingerprints.insert(path.to_path_buf(), "absent".to_owned());
                files.push(BackupFileSummary {
                    path: path.to_path_buf(),
                    action: BackupAction::Remove,
                });
            }
            ManifestFile::Present {
                blob,
                sha256: expected_hash,
                byte_len,
                metadata,
                ..
            } => {
                let expected_name = format!("file-{index:04}.bin");
                if blob != &expected_name || !simple_name(blob) {
                    return Err(TransactionError::Invalid("backup blob name is invalid"));
                }
                expected_names.insert(blob.clone());
                let blob_path = directory.join(blob);
                let content = host
                    .read_protected(&blob_path)?
                    .ok_or(TransactionError::Invalid("backup file blob is missing"))?;
                verify_private_file(host, &blob_path)?;
                total_bytes = total_bytes.saturating_add(content.len());
                if total_bytes > MAX_TOTAL_BACKUP_BYTES
                    || content.len() != *byte_len
                    || content.len() > MAX_FILE_BYTES
                    || sha256(&content) != *expected_hash
                {
                    return Err(TransactionError::Invalid("backup file blob is corrupt"));
                }
                if metadata.mode > 0o7777 {
                    return Err(TransactionError::Invalid(
                        "backup target metadata is invalid",
                    ));
                }
                preimage_fingerprints.insert(
                    path.to_path_buf(),
                    fingerprint(
                        &content,
                        &FileMetadata {
                            uid: metadata.uid,
                            gid: metadata.gid,
                            mode: metadata.mode,
                            is_file: true,
                            is_dir: false,
                        },
                    ),
                );
                files.push(BackupFileSummary {
                    path: path.to_path_buf(),
                    action: BackupAction::Restore,
                });
            }
        }
    }
    for operation in &pending.operations {
        if let Some(target) = operation.target.as_ref()
            && preimage_fingerprints.get(target) != Some(&operation.before)
        {
            return Err(TransactionError::Invalid(
                "pending operation does not match its backup preimage",
            ));
        }
    }
    for (purpose, reference) in &pending.secrets {
        expected_names.insert(reference.file.clone());
        let secret_path = directory.join(reference.file.as_str());
        let content = host
            .read_protected(&secret_path)?
            .ok_or(TransactionError::Invalid("materialized secret is missing"))?;
        verify_private_file(host, &secret_path)?;
        if content.len() != reference.byte_len || sha256(&content) != reference.sha256 {
            return Err(TransactionError::Invalid("materialized secret is corrupt"));
        }
        let _ = purpose;
    }
    // An atomic secret publication can precede its journal reference by one
    // crash boundary. Permit only the two fixed orphan names for adoption.
    for orphan in
        [SecretPurpose::Upsmon, SecretPurpose::Operator].map(|purpose| purpose.file_name())
    {
        let orphan_path = directory.join(orphan);
        if let Some(bytes) = host.read_protected(&orphan_path)? {
            verify_private_file(host, &orphan_path)?;
            if !(32..=128).contains(&bytes.len())
                || !bytes.len().is_multiple_of(2)
                || !bytes.iter().all(u8::is_ascii_hexdigit)
            {
                return Err(TransactionError::Invalid("orphan secret blob is malformed"));
            }
            expected_names.insert(orphan.to_owned());
        }
    }
    let actual_names: BTreeSet<_> = host.list_dir(&directory)?.into_iter().collect();
    if actual_names != expected_names {
        return Err(TransactionError::Invalid(
            "backup directory contains unexpected entries",
        ));
    }
    let summary = BackupSummary {
        stamp: manifest.stamp.clone(),
        files,
        services: manifest.services.clone(),
        membership_additions: manifest.membership_additions.clone(),
    };
    Ok((manifest, summary))
}

fn validate_manifest(manifest: &Manifest, pending: &PendingRecord) -> Result<(), TransactionError> {
    if manifest.schema_version != 1 {
        return Err(TransactionError::Invalid(
            "unsupported backup manifest schema",
        ));
    }
    validate_stamp(&manifest.stamp)?;
    validate_hash(&manifest.plan_hash)?;
    if manifest.stamp != pending.stamp || manifest.plan_hash != pending.plan_hash {
        return Err(TransactionError::Invalid(
            "manifest does not match the pending journal",
        ));
    }
    if manifest.files.len() > MAX_TARGETS
        || manifest.services.len() > MAX_SERVICES
        || manifest.membership_additions.len() > MAX_MEMBERSHIPS
    {
        return Err(TransactionError::Invalid(
            "backup manifest exceeds the size limit",
        ));
    }
    let mut paths = BTreeSet::new();
    for entry in &manifest.files {
        validate_target(entry.path())?;
        if !paths.insert(entry.path()) {
            return Err(TransactionError::Invalid(
                "backup manifest contains duplicate targets",
            ));
        }
        if let ManifestFile::Present { blob, sha256, .. } = entry {
            if !simple_name(blob) {
                return Err(TransactionError::Invalid("backup blob name is invalid"));
            }
            validate_hash(sha256)?;
        }
    }
    let request = BeginRequest {
        plan_hash: manifest.plan_hash.clone(),
        targets: manifest
            .files
            .iter()
            .map(|entry| entry.path().to_path_buf())
            .collect(),
        services: manifest.services.clone(),
        membership_additions: manifest.membership_additions.clone(),
        operations: pending
            .operations
            .iter()
            .map(|operation| OperationIntent {
                id: operation.id.clone(),
                target: operation.target.clone(),
                before: operation.before.clone(),
                after: operation.after.clone(),
            })
            .collect(),
    };
    validate_begin_request(&request)?;
    Ok(())
}

fn validate_state(state: &StateRecord, stamp: &str) -> Result<(), TransactionError> {
    if state.schema_version != 1 {
        return Err(TransactionError::Invalid("unsupported state record schema"));
    }
    validate_hash(&state.config_hash)?;
    validate_hash(&state.profile_hash)?;
    validate_stamp(&state.backup_reference)?;
    if state.backup_reference != stamp {
        return Err(TransactionError::WrongTransaction);
    }
    if state.outputs.len() > MAX_TARGETS || state.managed_identities.len() > 128 {
        return Err(TransactionError::Invalid(
            "state record exceeds the size limit",
        ));
    }
    for (path, output) in &state.outputs {
        validate_target(path)?;
        if path == Path::new(STATE_PATH) {
            return Err(TransactionError::Invalid(
                "state record cannot hash its own bytes",
            ));
        }
        validate_hash(&output.sha256)?;
        if output.metadata.mode > 0o7777 {
            return Err(TransactionError::Invalid("state metadata is invalid"));
        }
    }
    for (key, value) in &state.managed_identities {
        validate_token(key, 64)?;
        validate_token(value, 128)?;
    }
    Ok(())
}

fn validate_state_preimage<H: Host>(
    host: &H,
    manifest: &Manifest,
    current_bytes: Option<&[u8]>,
    current_metadata: Option<&FileMetadata>,
) -> Result<(), TransactionError> {
    let snapshot = manifest
        .files
        .iter()
        .find(|entry| entry.path() == Path::new(STATE_PATH))
        .ok_or(TransactionError::Invalid(
            "backup manifest has no state record preimage",
        ))?;
    match (snapshot, current_bytes, current_metadata) {
        (ManifestFile::Absent { .. }, None, None) => Ok(()),
        (
            ManifestFile::Present {
                blob,
                metadata: before_metadata,
                ..
            },
            Some(current),
            Some(current_metadata),
        ) if current_metadata.is_file
            && !current_metadata.is_dir
            && current_metadata.uid == before_metadata.uid
            && current_metadata.gid == before_metadata.gid
            && current_metadata.mode == before_metadata.mode =>
        {
            let backup = backup_dir(&manifest.stamp).join(blob);
            let original = host
                .read_protected(&backup)?
                .ok_or(TransactionError::Invalid(
                    "state record preimage is missing",
                ))?;
            if original == current {
                Ok(())
            } else {
                Err(TransactionError::Invalid(
                    "state record changed during the transaction",
                ))
            }
        }
        _ => Err(TransactionError::Invalid(
            "state record changed during the transaction",
        )),
    }
}

fn state_finalized_for_pending<H: Host>(
    host: &H,
    pending: &PendingRecord,
) -> Result<bool, TransactionError> {
    let Some(bytes) = host.read_protected(Path::new(STATE_PATH))? else {
        return match &pending.state_finalization {
            StateFinalization::Published { .. } => Err(TransactionError::Invalid(
                "published state record is missing",
            )),
            _ => Ok(false),
        };
    };
    verify_private_file(host, Path::new(STATE_PATH))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(TransactionError::Invalid(
            "state record exceeds the size limit",
        ));
    }
    let state: StateRecord = decode(&bytes)?;
    let state_stamp = state.backup_reference.clone();
    validate_state(&state, &state_stamp)?;
    let belongs_to_transaction = state_stamp == pending.stamp;
    match &pending.state_finalization {
        StateFinalization::Awaiting if belongs_to_transaction => Err(TransactionError::Invalid(
            "state record was published without a journal intent",
        )),
        StateFinalization::Awaiting => Ok(false),
        StateFinalization::Intent { state_sha256 } => {
            if belongs_to_transaction && sha256(&bytes) == *state_sha256 {
                Ok(true)
            } else if belongs_to_transaction {
                Err(TransactionError::Invalid(
                    "state record does not match its journal intent",
                ))
            } else {
                Ok(false)
            }
        }
        StateFinalization::Published { state_sha256 } => {
            if belongs_to_transaction && sha256(&bytes) == *state_sha256 {
                Ok(true)
            } else {
                Err(TransactionError::Invalid(
                    "published state record does not match its journal",
                ))
            }
        }
    }
}

fn load_pending<H: Host>(host: &H) -> Result<PendingRecord, TransactionError> {
    let bytes = host
        .read_protected(Path::new(PENDING_PATH))?
        .ok_or(TransactionError::WrongTransaction)?;
    verify_private_file(host, Path::new(PENDING_PATH))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(TransactionError::Invalid(
            "pending journal exceeds the size limit",
        ));
    }
    let pending: PendingRecord = decode(&bytes)?;
    validate_pending(&pending)?;
    Ok(pending)
}

fn save_pending<H: Host>(host: &H, pending: &PendingRecord) -> Result<(), TransactionError> {
    validate_pending(pending)?;
    host.replace_file_atomic(
        Path::new(PENDING_PATH),
        &encode(pending)?,
        &ROOT_PRIVATE_FILE,
    )?;
    Ok(())
}

fn ensure_transaction(
    pending: &PendingRecord,
    transaction: &Transaction,
) -> Result<(), TransactionError> {
    if pending.stamp == transaction.stamp && pending.plan_hash == transaction.plan_hash {
        Ok(())
    } else {
        Err(TransactionError::WrongTransaction)
    }
}

fn unique_stamp<H: Host>(host: &H) -> Result<String, TransactionError> {
    let timestamp = DateTime::<Utc>::from(host.now()).format("%Y%m%dT%H%M%SZ");
    let entropy = host.random_bytes(8)?;
    if entropy.len() != 8 {
        return Err(TransactionError::Invalid(
            "host returned incomplete entropy",
        ));
    }
    let base = format!("{timestamp}-{}", encode_hex(&entropy));
    for collision in 0..1024usize {
        let stamp = if collision == 0 {
            base.clone()
        } else {
            format!("{base}-{collision}")
        };
        if !host.path_exists(&backup_dir(&stamp))? {
            return Ok(stamp);
        }
    }
    Err(TransactionError::Invalid(
        "could not allocate a unique backup stamp",
    ))
}

fn backup_dir(stamp: &str) -> PathBuf {
    Path::new(BACKUP_ROOT).join(stamp)
}

fn read_private_blob<H: Host>(
    host: &H,
    path: &Path,
    expected_len: usize,
) -> Result<Vec<u8>, TransactionError> {
    let bytes = host
        .read_protected(path)?
        .ok_or(TransactionError::Invalid("materialized secret is missing"))?;
    verify_private_file(host, path)?;
    if bytes.len() != expected_len || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(TransactionError::Invalid(
            "materialized secret is malformed",
        ));
    }
    Ok(bytes)
}

fn verify_private_dir<H: Host>(host: &H, path: &Path) -> Result<(), TransactionError> {
    let metadata = host.metadata(path)?.ok_or(TransactionError::Invalid(
        "private backup directory is missing",
    ))?;
    if !metadata.is_dir
        || metadata.is_file
        || metadata.uid != 0
        || metadata.gid != 0
        || metadata.mode != 0o700
    {
        return Err(TransactionError::Invalid(
            "private backup directory metadata is unsafe",
        ));
    }
    Ok(())
}

fn verify_private_file<H: Host>(host: &H, path: &Path) -> Result<(), TransactionError> {
    let metadata = host
        .metadata(path)?
        .ok_or(TransactionError::Invalid("private backup file is missing"))?;
    if !metadata.is_file
        || metadata.is_dir
        || metadata.uid != 0
        || metadata.gid != 0
        || metadata.mode != 0o600
    {
        return Err(TransactionError::Invalid(
            "private backup file metadata is unsafe",
        ));
    }
    Ok(())
}

fn validate_target(path: &Path) -> Result<(), TransactionError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        || !path
            .to_str()
            .is_some_and(|value| ALLOWED_TARGETS.contains(&value))
    {
        return Err(TransactionError::Invalid(
            "target path is not an allowlisted absolute file",
        ));
    }
    Ok(())
}

const ALLOWED_TARGETS: &[&str] = &[
    LIVE_NUT_CONF,
    LIVE_UPS_CONF,
    LIVE_UPSD_USERS,
    LIVE_UPSMON_CONF,
    LIVE_UPSSCHED_CONF,
    LIVE_HOOK,
    LEGACY_HOOK,
    "/etc/nut-cli/nut.toml",
    OPERATOR_SECRET_PATH,
    FSTAB_PATH,
    STATE_PATH,
    "/usr/sbin/policy-rc.d",
];

fn validate_unit(unit: &str) -> Result<(), TransactionError> {
    if matches!(
        unit,
        "nut.target"
            | "nut-server.service"
            | "nut-monitor.service"
            | "nut-driver-enumerator.service"
    ) || (unit.starts_with("nut-driver@")
        && unit.ends_with(".service")
        && unit.len() > 19
        && unit[11..unit.len() - 8]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
    {
        Ok(())
    } else {
        Err(TransactionError::Invalid(
            "transaction has an unknown service unit",
        ))
    }
}

fn validate_username(value: &str) -> Result<(), TransactionError> {
    if !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        Ok(())
    } else {
        Err(TransactionError::Invalid(
            "transaction has an invalid account name",
        ))
    }
}

fn validate_token(value: &str, max_len: usize) -> Result<(), TransactionError> {
    if !value.is_empty()
        && value.len() <= max_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@'))
    {
        Ok(())
    } else {
        Err(TransactionError::Invalid(
            "transaction contains an invalid identifier",
        ))
    }
}

fn validate_operation_id(value: &str) -> Result<(), TransactionError> {
    if !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@' | b':' | b'/')
        })
        && !value.split('/').any(|part| part == "..")
    {
        Ok(())
    } else {
        Err(TransactionError::Invalid(
            "transaction contains an invalid operation ID",
        ))
    }
}

fn validate_fingerprint(value: &str) -> Result<(), TransactionError> {
    if value == "absent" {
        return Ok(());
    }
    validate_hash(value)
}

fn validate_hash(value: &str) -> Result<(), TransactionError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(TransactionError::Invalid(
            "transaction contains an invalid SHA-256 value",
        ))
    }
}

fn validate_stamp(value: &str) -> Result<(), TransactionError> {
    let bytes = value.as_bytes();
    let prefix_valid = bytes.len() >= 33
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'Z'
        && bytes[16] == b'-'
        && bytes[17..33]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte));
    let collision_suffix = bytes.get(33..).unwrap_or_default();
    let suffix_valid = collision_suffix.is_empty()
        || (collision_suffix.first() == Some(&b'-')
            && collision_suffix.len() > 1
            && collision_suffix[1..].iter().all(u8::is_ascii_digit));
    if !prefix_valid || !suffix_valid || bytes.len() > 40 {
        return Err(TransactionError::Invalid("backup stamp is malformed"));
    }
    Ok(())
}

fn sorted_services(values: &[ServiceSnapshot]) -> Vec<ServiceSnapshot> {
    let mut values = values.to_vec();
    values.sort_by(|left, right| left.unit.cmp(&right.unit));
    values
}

fn sorted_memberships(values: &[MembershipAddition]) -> Vec<MembershipAddition> {
    let mut values = values.to_vec();
    values.sort_by(|left, right| (&left.user, &left.group).cmp(&(&right.user, &right.group)));
    values
}

fn simple_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && value != "."
        && value != ".."
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, TransactionError> {
    serde_json::to_vec_pretty(value)
        .map_err(|_| TransactionError::Invalid("cannot encode transaction record"))
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, TransactionError> {
    serde_json::from_slice(bytes)
        .map_err(|_| TransactionError::Invalid("cannot parse transaction record"))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_digit(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'a' + value - 10,
    }
}

fn require_root<H: Host>(host: &H) -> Result<(), TransactionError> {
    if host.euid() == 0 {
        Ok(())
    } else {
        Err(TransactionError::Invalid(
            "transaction storage requires root",
        ))
    }
}

fn require_writer_lock(lock: &WriterLock) -> Result<(), TransactionError> {
    if lock.guard.path() == Path::new(APPLY_LOCK_PATH) {
        Ok(())
    } else {
        Err(TransactionError::Invalid(
            "transaction writer lock is not held",
        ))
    }
}

fn require_transaction_lock(transaction: &Transaction) -> Result<(), TransactionError> {
    if transaction.writer_lock.guard.path() == Path::new(APPLY_LOCK_PATH) {
        Ok(())
    } else {
        Err(TransactionError::Invalid(
            "transaction writer lock is not held",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{FakeHost, Host};
    use std::time::{Duration, SystemTime};

    const CONFIG: &str = "/etc/nut/nut.conf";

    fn host() -> FakeHost {
        FakeHost::default()
            .with_euid(0)
            .at(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))
            .with_entropy((1u8..=64).collect::<Vec<_>>())
            .with_file(CONFIG, b"sentinel preimage password")
            .with_metadata(CONFIG, FileMetadata::file(0, 0, 0o640))
    }

    fn request(host: &FakeHost) -> BeginRequest {
        let before = file_fingerprint(host, Path::new(CONFIG)).unwrap().unwrap();
        let after =
            predicted_file_fingerprint(b"new managed config", &FileMetadata::file(0, 0, 0o640));
        BeginRequest {
            plan_hash: "a".repeat(64),
            targets: vec![
                PathBuf::from(CONFIG),
                PathBuf::from(LIVE_UPS_CONF),
                PathBuf::from(STATE_PATH),
            ],
            services: vec![ServiceSnapshot {
                unit: "nut-server.service".into(),
                active: "active".into(),
                enabled: "enabled".into(),
            }],
            membership_additions: vec![MembershipAddition {
                user: "alice".into(),
                group: "nut-operators".into(),
            }],
            operations: vec![
                OperationIntent {
                    id: "file:/etc/nut/nut.conf".into(),
                    target: Some(PathBuf::from(CONFIG)),
                    before,
                    after,
                },
                OperationIntent {
                    id: "file:/etc/nut/ups.conf".into(),
                    target: Some(PathBuf::from(LIVE_UPS_CONF)),
                    before: "absent".into(),
                    after: predicted_file_fingerprint(
                        b"new ups config",
                        &FileMetadata::file(0, 0, 0o640),
                    ),
                },
            ],
        }
    }

    fn state(transaction: &Transaction) -> StateRecord {
        StateRecord {
            schema_version: 1,
            config_hash: "b".repeat(64),
            profile_hash: "c".repeat(64),
            outputs: BTreeMap::new(),
            managed_identities: BTreeMap::from([("ups".into(), "cp1500".into())]),
            backup_reference: transaction.stamp.clone(),
            transaction_outcome: TransactionOutcome::Applied,
        }
    }

    #[test]
    fn empty_plan_does_not_create_state_backup_or_consume_entropy() {
        let host = FakeHost::default().with_euid(0).with_entropy(vec![9; 32]);
        let request = BeginRequest {
            plan_hash: "a".repeat(64),
            targets: Vec::new(),
            services: Vec::new(),
            membership_additions: Vec::new(),
            operations: Vec::new(),
        };
        assert_eq!(begin(&host, &request).unwrap(), None);
        assert!(host.effects().is_empty());
        assert!(!host.path_exists(Path::new(BACKUP_ROOT)).unwrap());
    }

    #[test]
    fn transaction_handle_holds_the_shared_writer_lock_until_drop() {
        let host = host();
        let transaction = begin(&host, &request(&host)).unwrap().unwrap();
        assert!(host.lock_exclusive(Path::new(APPLY_LOCK_PATH)).is_err());
        drop(transaction);
        assert_eq!(
            host.lock_exclusive(Path::new(APPLY_LOCK_PATH))
                .unwrap()
                .path(),
            Path::new(APPLY_LOCK_PATH)
        );
    }

    #[test]
    fn begin_publishes_private_manifest_and_absent_markers_before_effects() {
        let host = host();
        let request = request(&host);
        let transaction = begin(&host, &request).unwrap().unwrap();
        let stamp = transaction.stamp.clone();
        drop(transaction);
        assert!(
            host.read_protected(Path::new(PENDING_PATH))
                .unwrap()
                .is_some()
        );
        let report = recover(&host).unwrap().unwrap();
        assert_eq!(report.backup.files.len(), 3);
        assert_eq!(report.backup.files[0].action, BackupAction::Restore);
        assert_eq!(report.backup.files[1].action, BackupAction::Remove);
        assert_eq!(report.backup.files[2].action, BackupAction::Remove);
        assert_eq!(
            report.operations[0].observed,
            ReconciledOperation::NotStarted
        );
        assert!(!format!("{:?}", report.backup).contains("sentinel preimage password"));
        assert!(
            !serde_json::to_string(&report.backup)
                .unwrap()
                .contains("sentinel preimage password")
        );
        assert!(!format!("{:?}", request.operations[0]).contains(&request.operations[0].before));
        let dir = backup_dir(&stamp);
        let names = host.list_dir(&dir).unwrap();
        assert_eq!(names, ["file-0000.bin", "manifest.json"]);
        for name in names {
            let path = dir.join(name);
            assert_eq!(
                host.metadata(&path).unwrap(),
                Some(ROOT_PRIVATE_FILE.clone())
            );
        }
    }

    #[test]
    fn interrupted_manifest_publication_leaves_preimages_and_never_overwrites_them() {
        let host = host();
        host.fail_after(
            "replace_file",
            2,
            "simulated termination before journal publish",
        );
        let request = request(&host);
        assert!(begin(&host, &request).is_err());
        assert!(
            host.read_protected(Path::new(PENDING_PATH))
                .unwrap()
                .is_none()
        );
        assert!(recover(&host).unwrap().is_none());

        let stamps = host.list_dir(Path::new(BACKUP_ROOT)).unwrap();
        assert_eq!(stamps.len(), 1);
        let first_dir = Path::new(BACKUP_ROOT).join(&stamps[0]);
        let original_manifest = host
            .read_protected(&first_dir.join(MANIFEST_NAME))
            .unwrap()
            .unwrap();
        assert_eq!(
            host.read_protected(&first_dir.join("file-0000.bin"))
                .unwrap()
                .unwrap(),
            b"sentinel preimage password"
        );

        let next = begin(&host, &request).unwrap().unwrap();
        assert_ne!(next.stamp, stamps[0]);
        assert_eq!(
            host.read_protected(&first_dir.join(MANIFEST_NAME))
                .unwrap()
                .unwrap(),
            original_manifest
        );
    }

    #[test]
    fn started_operation_is_reconciled_from_disk_after_fake_process_restart() {
        let host = host();
        let request = request(&host);
        let transaction = begin(&host, &request).unwrap().unwrap();
        checkpoint(
            &host,
            &transaction,
            "file:/etc/nut/nut.conf",
            Checkpoint::Started,
        )
        .unwrap();
        host.replace_file_atomic(
            Path::new(CONFIG),
            b"new managed config",
            &FileMetadata::file(0, 0, 0o640),
        )
        .unwrap();
        let stamp = transaction.stamp.clone();
        drop(transaction);
        let restarted = host.snapshot_filesystem();
        let recovery = recover(&restarted).unwrap().unwrap();
        assert_eq!(recovery.operations[0].observed, ReconciledOperation::After);
        assert_eq!(
            recovery.operations[1].observed,
            ReconciledOperation::NotStarted
        );
        assert_eq!(
            restarted
                .read_protected(&backup_dir(&stamp).join("file-0000.bin"))
                .unwrap()
                .unwrap(),
            b"sentinel preimage password"
        );
        assert_eq!(recovery.backup.files[1].action, BackupAction::Remove);
        let resumed = recovery.transaction;
        checkpoint(
            &restarted,
            &resumed,
            "file:/etc/nut/nut.conf",
            Checkpoint::Completed,
        )
        .unwrap();
        checkpoint(
            &restarted,
            &resumed,
            "file:/etc/nut/ups.conf",
            Checkpoint::Started,
        )
        .unwrap();
        restarted
            .replace_file_atomic(
                Path::new(LIVE_UPS_CONF),
                b"new ups config",
                &FileMetadata::file(0, 0, 0o640),
            )
            .unwrap();
        checkpoint(
            &restarted,
            &resumed,
            "file:/etc/nut/ups.conf",
            Checkpoint::Completed,
        )
        .unwrap();
        drop(resumed);
        let restarted_again = restarted.snapshot_filesystem();
        let recovery = recover(&restarted_again).unwrap().unwrap();
        assert_eq!(recovery.operations[0].observed, ReconciledOperation::After);
        assert_eq!(recovery.operations[1].observed, ReconciledOperation::After);
    }

    #[test]
    fn secret_materialization_survives_restart_without_second_entropy_draw() {
        let host = host();
        let transaction = begin(&host, &request(&host)).unwrap().unwrap();
        let secret = materialize_secret(&host, &transaction, SecretPurpose::Upsmon, 16).unwrap();
        assert_eq!(secret.expose(), b"090a0b0c0d0e0f101112131415161718");
        assert_eq!(
            host.effects()
                .iter()
                .filter(|effect| matches!(effect, crate::host::HostEffect::Random { .. }))
                .count(),
            2
        );
        drop(transaction);
        let restarted = host.snapshot_filesystem();
        let report = recover(&restarted).unwrap().unwrap();
        let resumed =
            materialize_secret(&restarted, &report.transaction, SecretPurpose::Upsmon, 16).unwrap();
        assert_eq!(secret.expose(), resumed.expose());
        assert!(
            restarted
                .effects()
                .iter()
                .all(|effect| !matches!(effect, crate::host::HostEffect::Random { .. }))
        );
        assert!(!format!("{report:?}").contains("090a0b"));
    }

    #[test]
    fn orphan_secret_blob_is_adopted_after_publication_before_journal_update() {
        let host = host();
        let transaction = begin(&host, &request(&host)).unwrap().unwrap();
        let secret_path = backup_dir(&transaction.stamp).join(SecretPurpose::Upsmon.file_name());
        let material = b"0123456789abcdef0123456789abcdef";
        host.replace_file_atomic(&secret_path, material, &ROOT_PRIVATE_FILE)
            .unwrap();
        drop(transaction);

        let restarted = host.snapshot_filesystem();
        let report = recover(&restarted).unwrap().unwrap();
        let secret =
            materialize_secret(&restarted, &report.transaction, SecretPurpose::Upsmon, 16).unwrap();
        assert_eq!(secret.expose(), material);
        assert!(
            restarted
                .effects()
                .iter()
                .all(|effect| !matches!(effect, crate::host::HostEffect::Random { .. }))
        );
    }

    #[test]
    fn finalize_is_idempotent_across_state_publication_crash_window() {
        let host = FakeHost::default()
            .with_euid(0)
            .at(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))
            .fail_next("remove_protected", "simulated process termination")
            .with_entropy(vec![7; 8]);
        let request = BeginRequest {
            plan_hash: "a".repeat(64),
            targets: vec![PathBuf::from(STATE_PATH)],
            services: Vec::new(),
            membership_additions: Vec::new(),
            operations: vec![OperationIntent {
                id: "state-completion".into(),
                target: None,
                before: "absent".into(),
                after: "d".repeat(64),
            }],
        };
        let transaction = begin(&host, &request).unwrap().unwrap();
        checkpoint(&host, &transaction, "state-completion", Checkpoint::Started).unwrap();
        checkpoint(
            &host,
            &transaction,
            "state-completion",
            Checkpoint::Completed,
        )
        .unwrap();
        let state = state(&transaction);
        assert!(finalize(&host, &transaction, &state).is_err());
        drop(transaction);
        let restarted = host.snapshot_filesystem();
        let report = recover(&restarted).unwrap().unwrap();
        assert!(report.state_finalized);
        let transaction = report.transaction;
        let writes_before = restarted
            .effects()
            .iter()
            .filter(|effect| matches!(effect, crate::host::HostEffect::ReplaceFile { path, .. } if path == Path::new(STATE_PATH)))
            .count();
        finalize(&restarted, &transaction, &state).unwrap();
        let writes_after = restarted
            .effects()
            .iter()
            .filter(|effect| matches!(effect, crate::host::HostEffect::ReplaceFile { path, .. } if path == Path::new(STATE_PATH)))
            .count();
        assert_eq!(writes_before, writes_after);
        drop(transaction);
        assert!(recover(&restarted).unwrap().is_none());
        assert!(
            restarted
                .path_exists(&backup_dir(&state.backup_reference))
                .unwrap()
        );
    }

    #[test]
    fn state_write_is_reconciled_when_published_checkpoint_was_interrupted() {
        let host = FakeHost::default()
            .with_euid(0)
            .at(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))
            .with_entropy(vec![7; 8]);
        let request = BeginRequest {
            plan_hash: "a".repeat(64),
            targets: vec![PathBuf::from(STATE_PATH)],
            services: Vec::new(),
            membership_additions: Vec::new(),
            operations: Vec::new(),
        };
        let transaction = begin(&host, &request).unwrap().unwrap();
        let state = state(&transaction);
        // Begin publishes the manifest and pending journal. Finalization then
        // writes an intent, publishes state, and tries to record completion.
        host.fail_after(
            "replace_file",
            2,
            "simulated termination after state publish",
        );
        assert!(finalize(&host, &transaction, &state).is_err());
        drop(transaction);

        let restarted = host.snapshot_filesystem();
        let report = recover(&restarted).unwrap().unwrap();
        assert!(report.state_finalized);
        let transaction = report.transaction;
        let state_writes_before = restarted
            .effects()
            .iter()
            .filter(|effect| matches!(effect, crate::host::HostEffect::ReplaceFile { path, .. } if path == Path::new(STATE_PATH)))
            .count();
        finalize(&restarted, &transaction, &state).unwrap();
        let state_writes_after = restarted
            .effects()
            .iter()
            .filter(|effect| matches!(effect, crate::host::HostEffect::ReplaceFile { path, .. } if path == Path::new(STATE_PATH)))
            .count();
        assert_eq!(state_writes_before, state_writes_after);
        drop(transaction);
        assert!(recover(&restarted).unwrap().is_none());
    }

    #[test]
    fn external_started_operation_requires_observation_and_conflicts_fail_closed() {
        let host = FakeHost::default().with_euid(0).with_entropy(vec![1; 8]);
        let request = BeginRequest {
            plan_hash: "a".repeat(64),
            targets: vec![PathBuf::from(STATE_PATH)],
            services: Vec::new(),
            membership_additions: Vec::new(),
            operations: vec![OperationIntent {
                id: "install-nut".into(),
                target: None,
                before: "absent".into(),
                after: "e".repeat(64),
            }],
        };
        let transaction = begin(&host, &request).unwrap().unwrap();
        checkpoint(&host, &transaction, "install-nut", Checkpoint::Started).unwrap();
        drop(transaction);
        let report = recover(&host).unwrap().unwrap();
        assert_eq!(
            report.operations[0].observed,
            ReconciledOperation::NeedsObservation
        );
        drop(report);
        let observations = BTreeMap::from([("install-nut".into(), "e".repeat(64))]);
        let report = recover_with_observations(&host, &observations)
            .unwrap()
            .unwrap();
        assert_eq!(report.operations[0].observed, ReconciledOperation::After);
        drop(report);
        let conflict = BTreeMap::from([("install-nut".into(), "f".repeat(64))]);
        let report = recover_with_observations(&host, &conflict)
            .unwrap()
            .unwrap();
        assert_eq!(report.operations[0].observed, ReconciledOperation::Conflict);
    }

    #[test]
    fn corrupt_manifest_tampering_and_non_allowlisted_targets_fail_closed() {
        let host = host();
        let transaction = begin(&host, &request(&host)).unwrap().unwrap();
        let path = backup_dir(&transaction.stamp).join(MANIFEST_NAME);
        host.replace_file_atomic(&path, b"{}", &ROOT_PRIVATE_FILE)
            .unwrap();
        drop(transaction);
        assert!(
            recover(&host)
                .unwrap_err()
                .to_string()
                .contains("hash or size")
        );

        let unsafe_request = BeginRequest {
            plan_hash: "a".repeat(64),
            targets: vec![PathBuf::from("/etc/nut/../shadow")],
            services: Vec::new(),
            membership_additions: Vec::new(),
            operations: Vec::new(),
        };
        assert!(
            begin(&host, &unsafe_request)
                .unwrap_err()
                .to_string()
                .contains("allowlisted")
        );
    }

    #[test]
    fn traversal_manifest_target_is_rejected_even_with_a_matching_manifest_hash() {
        let host = host();
        let transaction = begin(&host, &request(&host)).unwrap().unwrap();
        let directory = backup_dir(&transaction.stamp);
        let manifest_path = directory.join(MANIFEST_NAME);
        let bytes = host.read_protected(&manifest_path).unwrap().unwrap();
        let mut manifest: Manifest = decode(&bytes).unwrap();
        let ManifestFile::Present { path, .. } = &mut manifest.files[0] else {
            panic!("expected a saved file");
        };
        *path = PathBuf::from("/etc/nut/../shadow");
        let manifest_bytes = encode(&manifest).unwrap();
        host.replace_file_atomic(&manifest_path, &manifest_bytes, &ROOT_PRIVATE_FILE)
            .unwrap();
        let mut pending = load_pending(&host).unwrap();
        pending.manifest_sha256 = sha256(&manifest_bytes);
        save_pending(&host, &pending).unwrap();
        drop(transaction);

        assert!(
            recover(&host)
                .unwrap_err()
                .to_string()
                .contains("allowlisted")
        );
    }

    #[test]
    fn same_clock_transactions_get_distinct_backup_directories() {
        let host = host();
        let first = begin(&host, &request(&host)).unwrap().unwrap();
        // Complete/remove the journal so a second reviewed transaction can begin.
        let first_pending = load_pending(&host).unwrap();
        host.remove_protected(Path::new(PENDING_PATH)).unwrap();
        drop(first);
        let second = begin(&host, &request(&host)).unwrap().unwrap();
        assert_ne!(first_pending.stamp, second.stamp);
        assert!(host.path_exists(&backup_dir(&first_pending.stamp)).unwrap());
        assert!(host.path_exists(&backup_dir(&second.stamp)).unwrap());
    }
}
