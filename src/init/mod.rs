#![forbid(unsafe_code)]

//! Typed observation and deterministic planning for `nut init`.
//!
//! `collect` gathers every input the planner needs without writing files,
//! consuming entropy, creating locks, or contacting anything beyond the
//! injected [`Host`] boundary. `plan` is pure: the same inputs always produce
//! the same ordered steps and the same authoritative hash.

mod hash;
mod observe;
mod plan;
mod secrets;

pub use hash::plan_hash;
pub use observe::{FileState, Observation, ObservationError, collect};
pub use plan::{Plan, PlanOptions, PlanStep, PlanStepKind, PlanView, plan};
pub use secrets::{
    escape_nut_secret, fingerprint_secret, parse_monitor_password, parse_nut_secret_value,
    parse_operator_section, parse_upsd_users,
};
