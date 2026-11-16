use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Write};
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
    permission_denied: bool,
}

impl HostError {
    fn from_io(context: &str, error: io::Error) -> Self {
        Self {
            message: format!("{context}: {error}"),
            permission_denied: error.kind() == io::ErrorKind::PermissionDenied,
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permission_denied: false,
        }
    }

    pub fn permission_denied(path: &Path) -> Self {
        Self {
            message: format!("cannot read {}: permission denied", path.display()),
            permission_denied: true,
        }
    }

    pub fn is_permission_denied(&self) -> bool {
        self.permission_denied
    }
}

/// Operating-system capabilities used by the CLI.
///
/// Calls receive argv as separate values; implementations must never pass them through a shell.
pub trait Host {
    fn env(&self, key: &str) -> Option<OsString>;
    fn home_dir(&self) -> Option<PathBuf>;
    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError>;
    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), HostError>;
    fn create_dir_all(&self, path: &Path) -> Result<(), HostError>;
    fn list_dir(&self, path: &Path) -> Result<Vec<String>, HostError>;
    fn path_exists(&self, path: &Path) -> Result<bool, HostError>;
    fn run_program(&self, program: &OsStr, args: &[OsString]) -> Result<ProcessOutput, HostError>;
    fn now(&self) -> SystemTime;
    fn euid(&self) -> u32;
    fn hostname(&self) -> Result<String, HostError>;
    fn stdin_is_terminal(&self) -> bool;
    fn stdout_is_terminal(&self) -> bool;
    fn stderr_is_terminal(&self) -> bool;
    fn terminal_columns(&self) -> Option<usize>;
    fn prompt_line(&self, prompt: &str) -> Result<String, HostError>;
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
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                Err(HostError::permission_denied(path))
            }
            Err(error) => Err(HostError::from_io(
                &format!("cannot read {}", path.display()),
                error,
            )),
        }
    }

    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), HostError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            self.create_dir_all(parent)?;
        }
        fs::write(path, contents)
            .map_err(|error| HostError::from_io(&format!("cannot write {}", path.display()), error))
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), HostError> {
        fs::create_dir_all(path).map_err(|error| {
            HostError::from_io(
                &format!("cannot create directory {}", path.display()),
                error,
            )
        })
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<String>, HostError> {
        match fs::read_dir(path) {
            Ok(entries) => {
                let mut names = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|error| {
                        HostError::from_io(&format!("cannot list {}", path.display()), error)
                    })?;
                    names.push(entry.file_name().to_string_lossy().into_owned());
                }
                names.sort();
                Ok(names)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                Err(HostError::permission_denied(path))
            }
            Err(error) => Err(HostError::from_io(
                &format!("cannot list {}", path.display()),
                error,
            )),
        }
    }

    fn path_exists(&self, path: &Path) -> Result<bool, HostError> {
        Ok(path.exists())
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

    fn euid(&self) -> u32 {
        parse_status_euid(
            self.read_file(Path::new("/proc/self/status"))
                .ok()
                .flatten()
                .as_deref(),
        )
        .unwrap_or(u32::MAX)
    }

    fn hostname(&self) -> Result<String, HostError> {
        if let Some(bytes) = self.read_file(Path::new("/etc/hostname"))?
            && let Ok(text) = std::str::from_utf8(&bytes)
        {
            let name = text.trim();
            if !name.is_empty() {
                return Ok(name.to_owned());
            }
        }
        let output = self.run_program(OsStr::new("hostname"), &[])?;
        let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if name.is_empty() {
            Err(HostError::message("cannot determine hostname"))
        } else {
            Ok(name)
        }
    }

    fn stdin_is_terminal(&self) -> bool {
        io::stdin().is_terminal()
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

    fn prompt_line(&self, prompt: &str) -> Result<String, HostError> {
        eprint!("{prompt}");
        io::stderr()
            .flush()
            .map_err(|error| HostError::from_io("cannot write prompt", error))?;
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|error| HostError::from_io("cannot read prompt", error))?;
        Ok(line.trim_end_matches(['\n', '\r']).to_owned())
    }
}

fn parse_status_euid(bytes: Option<&[u8]>) -> Option<u32> {
    let text = std::str::from_utf8(bytes?).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            let mut fields = rest.split_whitespace();
            let _real = fields.next()?;
            return fields.next()?.parse().ok();
        }
    }
    None
}

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
#[derive(Default)]
pub struct FakeHost {
    env: BTreeMap<String, OsString>,
    files: RefCell<BTreeMap<PathBuf, Vec<u8>>>,
    dirs: RefCell<BTreeSet<PathBuf>>,
    unreadable: BTreeMap<PathBuf, ()>,
    programs: BTreeMap<OsString, ProcessOutput>,
    program_args: BTreeMap<(OsString, Vec<OsString>), ProcessOutput>,
    now: Option<SystemTime>,
    euid: Option<u32>,
    hostname: Option<String>,
    stdin_tty: bool,
    stdout_tty: bool,
    stderr_tty: bool,
    prompts: RefCell<VecDeque<String>>,
}

#[cfg(test)]
impl FakeHost {
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    pub fn with_file(self, path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        let path = path.into();
        insert_ancestors(&self.dirs, &path);
        self.files.borrow_mut().insert(path, contents.into());
        self
    }

    pub fn with_dir(self, path: impl Into<PathBuf>) -> Self {
        self.dirs.borrow_mut().insert(path.into());
        self
    }

    pub fn with_unreadable(mut self, path: impl Into<PathBuf>) -> Self {
        self.unreadable.insert(path.into(), ());
        self
    }

    pub fn with_program(mut self, program: impl Into<OsString>, output: ProcessOutput) -> Self {
        self.programs.insert(program.into(), output);
        self
    }

    pub fn with_program_args(
        mut self,
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        output: ProcessOutput,
    ) -> Self {
        let args = args.into_iter().map(Into::into).collect();
        self.program_args.insert((program.into(), args), output);
        self
    }

    pub fn at(mut self, now: SystemTime) -> Self {
        self.now = Some(now);
        self
    }

    pub fn with_euid(mut self, euid: u32) -> Self {
        self.euid = Some(euid);
        self
    }

    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    pub fn with_tty(mut self, stdout: bool, stderr: bool) -> Self {
        self.stdout_tty = stdout;
        self.stderr_tty = stderr;
        self
    }

    pub fn with_stdin_tty(mut self, stdin: bool) -> Self {
        self.stdin_tty = stdin;
        self
    }

    pub fn with_prompt_lines(self, lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.prompts
            .borrow_mut()
            .extend(lines.into_iter().map(Into::into));
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
        if self.unreadable.contains_key(path) {
            return Err(HostError::permission_denied(path));
        }
        Ok(self.files.borrow().get(path).cloned())
    }

    fn write_file(&self, path: &Path, contents: &[u8]) -> Result<(), HostError> {
        insert_ancestors(&self.dirs, path);
        self.files
            .borrow_mut()
            .insert(path.to_path_buf(), contents.to_vec());
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), HostError> {
        self.dirs.borrow_mut().insert(path.to_path_buf());
        Ok(())
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<String>, HostError> {
        if self.unreadable.contains_key(path) {
            return Err(HostError::permission_denied(path));
        }
        let files = self.files.borrow();
        let dirs = self.dirs.borrow();
        let mut names = BTreeSet::new();
        for file in files.keys() {
            if file.parent() == Some(path)
                && let Some(name) = file.file_name()
            {
                names.insert(name.to_string_lossy().into_owned());
            }
        }
        for dir in dirs.iter() {
            if dir.parent() == Some(path)
                && let Some(name) = dir.file_name()
            {
                names.insert(name.to_string_lossy().into_owned());
            }
        }
        if names.is_empty()
            && !dirs.contains(path)
            && !files.keys().any(|file| file.starts_with(path))
        {
            return Err(HostError::message(format!(
                "cannot list {}",
                path.display()
            )));
        }
        Ok(names.into_iter().collect())
    }

    fn path_exists(&self, path: &Path) -> Result<bool, HostError> {
        Ok(self.files.borrow().contains_key(path)
            || self.dirs.borrow().contains(path)
            || self
                .files
                .borrow()
                .keys()
                .any(|file| file.starts_with(path))
            || self.dirs.borrow().iter().any(|dir| dir.starts_with(path)))
    }

    fn run_program(&self, program: &OsStr, args: &[OsString]) -> Result<ProcessOutput, HostError> {
        let exact = (program.to_os_string(), args.to_vec());
        if let Some(output) = self.program_args.get(&exact) {
            return Ok(output.clone());
        }
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

    fn euid(&self) -> u32 {
        self.euid.unwrap_or(1000)
    }

    fn hostname(&self) -> Result<String, HostError> {
        self.hostname
            .clone()
            .ok_or_else(|| HostError::message("cannot determine hostname"))
    }

    fn stdin_is_terminal(&self) -> bool {
        self.stdin_tty
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

    fn prompt_line(&self, _prompt: &str) -> Result<String, HostError> {
        self.prompts
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| HostError::message("no scripted prompt answer"))
    }
}

#[cfg(test)]
fn insert_ancestors(dirs: &RefCell<BTreeSet<PathBuf>>, path: &Path) {
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir.as_os_str().is_empty() || dir == Path::new("/") {
            break;
        }
        dirs.borrow_mut().insert(dir.to_path_buf());
        parent = dir.parent();
    }
}
