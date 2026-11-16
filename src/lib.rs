#![forbid(unsafe_code)]

pub mod bios;
pub mod cli;
pub mod completion;
pub mod config;
pub mod duration;
pub mod error;
pub mod events;
pub mod fake_upsd;
pub mod firmware;
pub mod host;
pub mod journal;
pub mod profiles;
pub mod render;
pub mod status;
pub mod ui;
pub mod upsd;

use std::ffi::OsString;
use std::io::{self, Write};

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::error::NutError;
use crate::host::{Host, SystemHost};

pub fn run() -> i32 {
    let mut args = std::env::args_os().collect::<Vec<_>>();
    if args.len() == 1 {
        args.push(OsString::from("status"));
    }
    if args.get(1).is_some_and(|arg| arg == "__complete") {
        // The hidden endpoint must never leak an internal panic into the prompt.
        std::panic::set_hook(Box::new(|_| {}));
    }
    let host = SystemHost;
    run_with_host(args, &host, &mut io::stdout(), &mut io::stderr())
}

pub fn run_with_host<I, T, H, O, E>(args: I, host: &H, stdout: &mut O, stderr: &mut E) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
    H: Host,
    O: Write,
    E: Write,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<OsString>>();
    if args.get(1).is_some_and(|arg| arg == "__complete") {
        return completion::complete(&args[2..], host, stdout, stderr);
    }
    if is_root_help(&args) {
        return match cli::write_root_help(stdout) {
            Ok(()) => 0,
            Err(error) => {
                let _ = writeln!(stderr, "nut: cannot write help: {error}");
                1
            }
        };
    }

    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            let rendered = error.render();
            if error.use_stderr() {
                let _ = write!(stderr, "{rendered}");
            } else {
                let _ = write!(stdout, "{rendered}");
            }
            return code;
        }
    };

    match execute(cli, host, stdout) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "nut: {error}");
            if let Some(hint) = error.hint() {
                let _ = writeln!(stderr, "→ {hint}");
            }
            error.exit_code()
        }
    }
}

fn is_root_help(args: &[OsString]) -> bool {
    let mut help_requested = false;
    let mut index = 1;
    while index < args.len() {
        let Some(argument) = args[index].to_str() else {
            return false;
        };
        match argument {
            "--help" | "-h" => help_requested = true,
            "--ascii" | "--json" | "-j" | "--verbose" | "-v" => {}
            "-c" | "--color" | "--config" => {
                index += 1;
                if index >= args.len() {
                    return false;
                }
            }
            value if value.starts_with("--color=") || value.starts_with("--config=") => {}
            value if value.starts_with("-c") && !value.starts_with("--") => {}
            _ => return false,
        }
        index += 1;
    }
    help_requested
}

fn execute<H: Host, O: Write>(cli: Cli, host: &H, stdout: &mut O) -> Result<(), NutError> {
    match cli.command.unwrap_or(Command::Status(Default::default())) {
        Command::Config(args) => {
            config::run(args.action, cli.config.as_deref(), cli.json, host, stdout)
        }
        Command::Bios(args) => bios::run(
            bios::BiosRun {
                action: args.action,
                json: cli.json,
                ascii: cli.ascii,
                color: cli.color,
                verbose: cli.verbose,
                config: cli.config.as_deref(),
            },
            host,
            stdout,
        ),
        Command::Status(args) => {
            let _ = bios::write_pending_reminder(host, cli.ascii, stdout)?;
            status::run_status(
                args,
                status::OutputOptions {
                    config_path: cli.config.as_deref(),
                    json: cli.json,
                    verbose: cli.verbose,
                    color: cli.color,
                    ascii: cli.ascii,
                },
                host,
                stdout,
            )
        }
        Command::Battery(args) => status::run_battery(
            args,
            status::OutputOptions {
                config_path: cli.config.as_deref(),
                json: cli.json,
                verbose: cli.verbose,
                color: cli.color,
                ascii: cli.ascii,
            },
            host,
            stdout,
        ),
        Command::Completion(args) => completion::run(args.action, cli.json, host, stdout),
        Command::Events(args) => events::run(
            args,
            status::OutputOptions {
                config_path: cli.config.as_deref(),
                json: cli.json,
                verbose: cli.verbose,
                color: cli.color,
                ascii: cli.ascii,
            },
            host,
            stdout,
        ),
        Command::Beeper(_)
        | Command::Test(_)
        | Command::Notify(_)
        | Command::Init(_)
        | Command::Rollback(_)
        | Command::Doctor(_)
        | Command::Drill(_) => Err(NutError::operational(
            "this command is provided by a later nut phase",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;

    #[test]
    fn config_path_command_uses_fake_host_and_never_reads_config() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(
            ["nut", "config", "path"],
            &FakeHost::default().with_env("HOME", "/home/bryan"),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("/etc/nut-cli/nut.toml"));
        assert!(output.contains("source: default"));
        assert!(err.is_empty());
    }

    #[test]
    fn bad_arguments_use_usage_exit_code() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(
            ["nut", "no-such-command"],
            &FakeHost::default(),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 2);
        assert!(out.is_empty());
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("unrecognized subcommand")
        );
    }

    #[test]
    fn root_help_is_grouped_even_with_global_options() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(
            ["nut", "--color", "always", "--help"],
            &FakeHost::default(),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Monitor:"));
        assert!(output.contains("Set up:"));
        assert!(err.is_empty());
    }
}
