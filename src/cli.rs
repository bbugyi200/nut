use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

/// Manage and verify NUT UPS protection.
#[derive(Debug, Parser)]
#[command(
    name = "nut",
    version,
    about = "Manage and verify NUT UPS protection",
    long_about = "A read-first management CLI for Network UPS Tools. NUT's daemons remain the only part of the outage shutdown path.",
    subcommand_required = false,
    disable_help_subcommand = true,
    next_line_help = true
)]
pub struct Cli {
    /// Color output policy.
    #[arg(long, value_enum, value_name = "WHEN", default_value_t = ColorChoice::Auto, global = true)]
    pub color: ColorChoice,

    /// Use ASCII symbols and bars.
    #[arg(long, global = true)]
    pub ascii: bool,

    /// Emit machine-readable JSON where supported.
    #[arg(short = 'j', long, global = true)]
    pub json: bool,

    /// Include additional diagnostic detail.
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

    /// Read host settings from this file.
    #[arg(short = 'c', long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "Show UPS status")]
    Status(StatusArgs),

    #[command(about = "Show battery charge and runtime")]
    Battery(BatteryArgs),

    #[command(about = "Show UPS event history")]
    Events(EventsArgs),

    #[command(about = "Show or change the UPS beeper")]
    Beeper(BeeperArgs),

    #[command(about = "Run a guarded battery self-test")]
    Test(TestArgs),

    #[command(about = "Send or inspect UPS notifications")]
    Notify(NotifyArgs),

    #[command(about = "Plan or apply NUT configuration")]
    Init(InitArgs),

    #[command(about = "List or restore a configuration backup")]
    Rollback(RollbackArgs),

    #[command(about = "Inspect, scaffold, render, or diff host settings")]
    Config(ConfigArgs),

    #[command(about = "Guide and verify BIOS recovery settings")]
    Bios(BiosArgs),

    #[command(about = "Check UPS protection and recovery readiness")]
    Doctor(DoctorArgs),

    #[command(about = "Guide and record a physical outage drill")]
    Drill(DrillArgs),

    #[command(about = "Install, inspect, or remove shell completion")]
    Completion(CompletionArgs),
}

#[derive(Args, Debug, Default)]
pub struct StatusArgs {
    /// Continuously refresh the status view.
    #[arg(long)]
    pub watch: bool,

    /// Include raw NUT variables.
    #[arg(long)]
    pub all: bool,

    /// UPS name (defaults to the selected host configuration, or `primary`).
    #[arg(long, value_name = "UPS")]
    pub ups: Option<String>,

    /// Loopback upsd address (defaults to host configuration, or 127.0.0.1).
    #[arg(long, value_name = "ADDRESS")]
    pub upsd_address: Option<String>,

    /// Loopback upsd port (defaults to host configuration, or 3493).
    #[arg(long, value_name = "PORT")]
    pub upsd_port: Option<u16>,
}

#[derive(Args, Debug, Default)]
pub struct BatteryArgs {
    /// UPS name (defaults to the selected host configuration, or `primary`).
    #[arg(long, value_name = "UPS")]
    pub ups: Option<String>,

    /// Loopback upsd address (defaults to host configuration, or 127.0.0.1).
    #[arg(long, value_name = "ADDRESS")]
    pub upsd_address: Option<String>,

    /// Loopback upsd port (defaults to host configuration, or 3493).
    #[arg(long, value_name = "PORT")]
    pub upsd_port: Option<u16>,
}

#[derive(Args, Debug, Default)]
#[command(
    after_help = "System journal access may require sudo nut events. Optional systemd-journal membership exposes all system logs; nut never changes it automatically."
)]
pub struct EventsArgs {
    /// Include entries since a journalctl time (for example, 'today' or '-2h').
    #[arg(short = 's', long, value_name = "TIME", allow_hyphen_values = true)]
    pub since: Option<String>,

    /// Select a boot offset (0 is current, -1 is previous).
    #[arg(short = 'b', long, value_name = "N", allow_hyphen_values = true)]
    pub boot: Option<i32>,

    /// Stream new entries and updated stories until interrupted.
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Show only power outages, blips, and shutdowns.
    #[arg(short = 'o', long, conflicts_with = "raw")]
    pub outages: bool,

    /// Show source journal messages instead of stories.
    #[arg(short = 'r', long, conflicts_with = "last_outage")]
    pub raw: bool,

    /// Maximum stories or raw entries to show (1-2000; default 20).
    #[arg(short = 'n', long, value_name = "COUNT", value_parser = clap::value_parser!(u32).range(1..=2000))]
    pub limit: Option<u32>,

    /// Show only the most recent outage (alias for --outages --limit 1).
    #[arg(long)]
    pub last_outage: bool,
}

#[derive(Args, Debug, Default)]
pub struct BeeperArgs {
    #[command(subcommand)]
    pub action: Option<BeeperAction>,
}

#[derive(Debug, Subcommand)]
pub enum BeeperAction {
    Mute,
    Enable,
    Disable,
}

#[derive(Args, Debug, Default)]
pub struct TestArgs {
    #[command(subcommand)]
    pub action: Option<TestAction>,
}

#[derive(Debug, Subcommand)]
pub enum TestAction {
    Quick,
    Stop,
}

#[derive(Args, Debug, Default)]
pub struct NotifyArgs {
    #[command(subcommand)]
    pub action: Option<NotifyAction>,
}

#[derive(Debug, Subcommand)]
pub enum NotifyAction {
    Test,
}

#[derive(Args, Debug, Default)]
pub struct InitArgs {
    /// Show the plan without applying changes.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Rotate both managed credentials instead of adopting existing ones.
    #[arg(long)]
    pub rotate_secrets: bool,

    /// Apply only when the recomputed plan matches this hash.
    #[arg(long, value_name = "HASH")]
    pub expect_plan: Option<String>,

    /// Apply the reviewed plan without an interactive prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// UPS name (defaults to the selected host configuration).
    #[arg(long, value_name = "UPS")]
    pub ups: Option<String>,
}

#[derive(Args, Debug, Default)]
pub struct RollbackArgs {
    /// List available backups.
    #[arg(short = 'l', long)]
    pub list: bool,

    /// Show the restore plan without changing anything.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Restore without an interactive prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Backup stamp to restore (defaults to the newest complete backup).
    pub stamp: Option<String>,
}

#[derive(Args, Debug, Default)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: Option<ConfigAction>,
}

#[derive(Debug, Subcommand, Clone)]
pub enum ConfigAction {
    /// Print the selected host configuration path and why it was chosen.
    Path,
    /// Parse and display the selected host configuration.
    Show,
    /// Scaffold a commented host config from live USB and DMI discovery.
    New {
        /// Write the scaffold to this path instead of stdout.
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
    /// Render NUT files and the event hook with secret placeholders.
    Render {
        /// Write each rendered file into this directory.
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,
    },
    /// Compare rendered files with live NUT configuration. Secrets are masked.
    Diff,
}

#[derive(Args, Debug, Default)]
pub struct BiosArgs {
    #[command(subcommand)]
    pub action: Option<BiosAction>,
}

#[derive(Debug, Subcommand)]
pub enum BiosAction {
    /// Print a numbered keyboard walkthrough for this board.
    Guide,
    /// Reboot into firmware setup after a hostname interlock.
    Setup {
        /// Refused. BIOS setup cannot be unattended.
        #[arg(long)]
        yes: bool,
    },
    /// Record a BIOS-version-pinned attestation receipt.
    Attest,
    /// List, show, or scaffold a board profile.
    Profile {
        #[command(subcommand)]
        action: Option<ProfileAction>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProfileAction {
    List,
    Show { id: String },
    New,
}

#[derive(Args, Debug, Default)]
pub struct DoctorArgs {
    /// Run additional stability observations for this many seconds.
    #[arg(short = 'o', long, value_name = "SECS")]
    pub observe: Option<u64>,

    /// Treat warnings as failures.
    #[arg(short = 'S', long)]
    pub strict: bool,

    /// List the check catalog without running checks.
    #[arg(short = 'l', long)]
    pub list: bool,

    /// UPS name (defaults to the selected host configuration, or `cp1500`).
    #[arg(long, value_name = "UPS")]
    pub ups: Option<String>,

    /// Loopback upsd address (defaults to 127.0.0.1).
    #[arg(long, value_name = "ADDRESS")]
    pub upsd_address: Option<String>,

    /// Loopback upsd port (defaults to 3493).
    #[arg(long, value_name = "PORT")]
    pub upsd_port: Option<u16>,

    /// Check IDs or section prefixes.
    #[arg(value_name = "CHECK")]
    pub checks: Vec<String>,
}

#[derive(Args, Debug, Default)]
pub struct DrillArgs {
    #[command(subcommand)]
    pub action: Option<DrillAction>,

    /// Refused. Drills need a person at the keyboard.
    #[arg(long)]
    pub yes: bool,

    /// UPS name (defaults to the selected host configuration, or `cp1500`).
    #[arg(long, value_name = "UPS")]
    pub ups: Option<String>,

    /// Loopback upsd address (defaults to 127.0.0.1).
    #[arg(long, value_name = "ADDRESS")]
    pub upsd_address: Option<String>,

    /// Loopback upsd port (defaults to 3493).
    #[arg(long, value_name = "PORT")]
    pub upsd_port: Option<u16>,
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub enum DrillAction {
    /// Test a 60-second mains interruption without shutting down.
    Blip,
    /// Test automatic power-on after a true AC loss.
    AcLoss,
    /// Test UPS outlet cycling after a clean shutdown.
    Cycle,
    /// Test shutdown and recovery during a sustained outage.
    Outage,
    /// Record observations after a drill and review the prior boot journal.
    Report,
}

#[derive(Args, Debug, Default)]
pub struct CompletionArgs {
    #[command(subcommand)]
    pub action: Option<CompletionAction>,
}

#[derive(Debug, Subcommand)]
pub enum CompletionAction {
    /// Install an adapter and verify registration in an isolated real shell.
    Install {
        #[arg(value_enum)]
        shell: Option<crate::completion::Shell>,
        /// Preview without creating files.
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Directory in which to install the adapter.
        #[arg(short = 't', long, value_hint = clap::ValueHint::DirPath)]
        target: Option<PathBuf>,
        /// Replace a foreign or modified adapter.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Inspect installed adapters and probe their registration.
    Status,
    /// Remove adapters whose bytes still match the recorded manifest.
    Uninstall {
        #[arg(value_enum)]
        shell: Option<crate::completion::Shell>,
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    Zsh,
    Bash,
}

/// Workflow groups shared by help and completion.
pub const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("Monitor", &["status", "battery", "events"]),
    ("Act", &["beeper", "test", "notify"]),
    ("Set up", &["init", "rollback", "config"]),
    ("Prove", &["doctor", "bios", "drill"]),
    ("Shell", &["completion"]),
];

/// Render the root help from Clap's command tree, keeping workflow groups explicit.
pub fn write_root_help<W: std::io::Write>(output: &mut W) -> std::io::Result<()> {
    let mut command = Cli::command();
    command.build();
    writeln!(
        output,
        "{}",
        command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_else(|| "Manage and verify NUT UPS protection".to_owned())
    )?;
    writeln!(output)?;
    writeln!(output, "Usage: nut [OPTIONS] [COMMAND]")?;
    writeln!(output)?;
    writeln!(output, "Commands:")?;

    let subcommands = command.get_subcommands().collect::<Vec<_>>();
    for &(heading, names) in COMMAND_GROUPS {
        writeln!(output, "{heading}:")?;
        for name in names {
            if let Some(subcommand) = subcommands.iter().find(|item| item.get_name() == *name) {
                let description = subcommand
                    .get_about()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                writeln!(output, "  {name:<12} {description}")?;
            }
        }
    }

    writeln!(output)?;
    writeln!(output, "Options:")?;
    writeln!(output, "  -h, --help              Print help")?;
    writeln!(output, "  -V, --version           Print version")?;
    writeln!(
        output,
        "      --ascii              Use ASCII symbols and bars"
    )?;
    writeln!(
        output,
        "      --color <WHEN>       Color output: auto, always, or never [default: auto]"
    )?;
    writeln!(
        output,
        "  -c, --config <PATH>      Read host settings from this file"
    )?;
    writeln!(
        output,
        "  -j, --json              Emit machine-readable JSON where supported"
    )?;
    writeln!(
        output,
        "  -v, --verbose            Include additional diagnostic detail"
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_visible_command_appears_in_exactly_one_help_group() {
        let mut command = Cli::command();
        command.build();
        let visible = command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .map(|subcommand| subcommand.get_name().to_owned())
            .collect::<BTreeSet<_>>();
        let mut grouped = BTreeSet::new();
        for &(_, names) in COMMAND_GROUPS {
            for name in names {
                assert!(
                    grouped.insert((*name).to_owned()),
                    "{name} appears in more than one help group"
                );
                assert!(
                    visible.contains(*name),
                    "{name} is grouped but not a visible command"
                );
            }
        }
        assert_eq!(grouped, visible);
    }
}
