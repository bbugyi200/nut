use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct HostError {
    message: String,
}

impl HostError {
    fn from_io(context: &str, error: io::Error) -> Self {
        Self {
            message: format!("{context}: {error}"),
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Operating-system capabilities used by the CLI.
///
/// Calls receive argv as separate values; implementations must never pass them through a shell.
pub trait Host {
    fn env(&self, key: &str) -> Option<OsString>;
    fn home_dir(&self) -> Option<PathBuf>;
    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError>;
    fn run_program(&self, program: &OsStr, args: &[OsString]) -> Result<ProcessOutput, HostError>;
    fn now(&self) -> SystemTime;
    fn stdout_is_terminal(&self) -> bool;
    fn stderr_is_terminal(&self) -> bool;
    fn terminal_columns(&self) -> Option<usize>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemHost;

impl Host for SystemHost {
    fn env(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }

    fn home_dir(&self) -> Option<PathBuf> {
        self.env("HOME")
            .or_else(|| self.env("USERPROFILE"))
            .map(PathBuf::from)
    }

    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(HostError::from_io(
                &format!("cannot read {}", path.display()),
                error,
            )),
        }
    }

    fn run_program(&self, program: &OsStr, args: &[OsString]) -> Result<ProcessOutput, HostError> {
        let output = Command::new(program).args(args).output().map_err(|error| {
            HostError::from_io(&format!("cannot run {}", program.to_string_lossy()), error)
        })?;
        Ok(ProcessOutput {
            status: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn stdout_is_terminal(&self) -> bool {
        io::stdout().is_terminal()
    }

    fn stderr_is_terminal(&self) -> bool {
        io::stderr().is_terminal()
    }

    fn terminal_columns(&self) -> Option<usize> {
        #[cfg(unix)]
        {
            if !io::stdout().is_terminal() {
                return None;
            }
            let output = Command::new("stty").arg("size").output().ok()?;
            let text = std::str::from_utf8(&output.stdout).ok()?;
            text.split_whitespace().last()?.parse().ok()
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
#[derive(Default)]
pub struct FakeHost {
    env: BTreeMap<String, OsString>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    programs: BTreeMap<OsString, ProcessOutput>,
    now: Option<SystemTime>,
    stdout_tty: bool,
    stderr_tty: bool,
}

#[cfg(test)]
impl FakeHost {
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    pub fn with_file(mut self, path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        self.files.insert(path.into(), contents.into());
        self
    }

    pub fn with_program(mut self, program: impl Into<OsString>, output: ProcessOutput) -> Self {
        self.programs.insert(program.into(), output);
        self
    }

    pub fn at(mut self, now: SystemTime) -> Self {
        self.now = Some(now);
        self
    }

    pub fn with_tty(mut self, stdout: bool, stderr: bool) -> Self {
        self.stdout_tty = stdout;
        self.stderr_tty = stderr;
        self
    }
}

#[cfg(test)]
impl Host for FakeHost {
    fn env(&self, key: &str) -> Option<OsString> {
        self.env.get(key).cloned()
    }

    fn home_dir(&self) -> Option<PathBuf> {
        self.env("HOME")
            .or_else(|| self.env("USERPROFILE"))
            .map(PathBuf::from)
    }

    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError> {
        Ok(self.files.get(path).cloned())
    }

    fn run_program(&self, program: &OsStr, _args: &[OsString]) -> Result<ProcessOutput, HostError> {
        Ok(self
            .programs
            .get(program)
            .cloned()
            .unwrap_or(ProcessOutput {
                status: Some(127),
                stdout: Vec::new(),
                stderr: b"fake: command not configured".to_vec(),
            }))
    }

    fn now(&self) -> SystemTime {
        self.now
            .unwrap_or(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
    }

    fn stdout_is_terminal(&self) -> bool {
        self.stdout_tty
    }

    fn stderr_is_terminal(&self) -> bool {
        self.stderr_tty
    }

    fn terminal_columns(&self) -> Option<usize> {
        None
    }
}
