#![forbid(unsafe_code)]

//! Typed observation and deterministic planning for `nut init`.
//!
//! `collect` gathers every input the planner needs without writing files,
//! consuming entropy, creating locks, or contacting anything beyond the
//! injected [`Host`] boundary. `plan` is pure: the same inputs always produce
//! the same ordered steps and the same authoritative hash.

pub mod converge;
mod hash;
mod observe;
mod plan;
pub mod resources;
pub mod rollback;
mod secrets;
mod transaction;

pub use converge::{ConvergeError, ConvergeOptions, ConvergeReport, converge};

pub use hash::plan_hash;
pub use observe::{FileState, Observation, ObservationError, collect};
pub use observe::{PENDING_PATH, STATE_PATH};
pub use plan::{Plan, PlanOptions, PlanStep, PlanStepKind, PlanView, plan};
pub use secrets::{
    escape_nut_secret, fingerprint_secret, parse_monitor_password, parse_nut_secret_value,
    parse_operator_section, parse_upsd_users,
};
pub use transaction::{
    APPLY_LOCK_PATH, BACKUP_ROOT, BackupAction, BackupFileSummary, BackupSummary, BeginRequest,
    Checkpoint, CheckpointState, FileMetadataRecord, MembershipAddition, OperationIntent,
    OperationRecovery, OutputRecord, ReconciledOperation, RecoveryReport, SecretMaterial,
    SecretPurpose, ServiceSnapshot, StateRecord, Transaction, TransactionError, TransactionOutcome,
    WriterLock, acquire_writer_lock, backup_summaries, backup_summary, begin, begin_locked,
    checkpoint, file_fingerprint, finalize, materialize_secret, peek_secret,
    predicted_file_fingerprint, read_state_record, recover, recover_locked,
    recover_with_observations,
};

pub use rollback::{RollbackError, RollbackOptions, RollbackPlan, RollbackReport};
