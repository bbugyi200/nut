//! Management-plane CLI for Network UPS Tools.
//!
//! NUT daemons and their shell hook remain responsible for outage protection.
#![forbid(unsafe_code)]

/// Build the bootstrap command without accessing hardware or changing host state.
pub fn command() -> clap::Command {
    clap::Command::new("nut")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Manage, check, and prove NUT UPS protection")
        .after_help("Bootstrap only: monitoring and management commands are not implemented yet.\nNUT daemons remain responsible for outage protection.")
}
