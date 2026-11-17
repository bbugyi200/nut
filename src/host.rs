use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

#[cfg(test)]
mod fake;
mod observe;
mod process;
mod protect;
mod stream;

#[cfg(test)]
pub use fake::{FakeHost, HostEffect};
pub use observe::UpsObservation;

use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub is_file: bool,
    pub is_dir: bool,
}

impl FileMetadata {
    pub fn executable(&self) -> bool {
        self.mode & 0o111 != 0
    }

    pub fn file(uid: u32, gid: u32, mode: u32) -> Self {
        Self {
            uid,
            gid,
            mode,
            is_file: true,
            is_dir: false,
        }
    }

    pub fn dir(uid: u32, gid: u32, mode: u32) -> Self {
        Self {
            uid,
            gid,
            mode,
            is_file: false,
            is_dir: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRequest<'a> {
    pub program: &'a OsStr,
    pub args: &'a [OsString],
    pub timeout: Duration,
    pub secret: bool,
    pub inherit_tty: bool,
}

/// Process-scoped exclusive apply lock. Dropping it releases the lock.
pub trait ApplyLock {
    fn path(&self) -> &Path;
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

    pub fn unsafe_path(path: &Path, reason: &str) -> Self {
        Self {
            message: format!("unsafe path {}: {reason}", path.display()),
            permission_denied: false,
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
    fn metadata(&self, path: &Path) -> Result<Option<FileMetadata>, HostError>;
    fn current_exe(&self) -> Result<PathBuf, HostError>;
    fn sleep(&self, duration: Duration);
    fn run_program(&self, program: &OsStr, args: &[OsString]) -> Result<ProcessOutput, HostError>;
    /// Read bounded process lines incrementally, with an optional total timeout.
    fn stream_program(
        &self,
        program: &OsStr,
        args: &[OsString],
        timeout: Option<Duration>,
        line: &mut dyn FnMut(&[u8]) -> Result<(), HostError>,
    ) -> Result<ProcessOutput, HostError> {
        let _ = timeout;
        let output = self.run_program(program, args)?;
        for bytes in output.stdout.split(|byte| *byte == b'\n') {
            if bytes.len() > stream::MAX_LINE_BYTES {
                return Err(HostError::message("process line exceeds 64 KiB"));
            }
            if !bytes.is_empty() {
                line(bytes)?;
            }
        }
        Ok(ProcessOutput {
            stdout: Vec::new(),
            ..output
        })
    }
    fn now(&self) -> SystemTime;
    fn euid(&self) -> u32;
    fn hostname(&self) -> Result<String, HostError>;
    fn stdin_is_terminal(&self) -> bool;
    fn stdout_is_terminal(&self) -> bool;
    fn stderr_is_terminal(&self) -> bool;
    fn terminal_columns(&self) -> Option<usize>;
    fn prompt_line(&self, prompt: &str) -> Result<String, HostError>;
    fn read_protected(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError>;
    fn replace_file_atomic(
        &self,
        path: &Path,
        contents: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), HostError>;
    fn remove_protected(&self, path: &Path) -> Result<(), HostError>;
    fn set_metadata(&self, path: &Path, metadata: &FileMetadata) -> Result<(), HostError>;
    fn create_secure_dir(&self, path: &Path, metadata: &FileMetadata) -> Result<(), HostError>;
    fn lock_exclusive(&self, path: &Path) -> Result<Box<dyn ApplyLock>, HostError>;
    fn random_bytes(&self, len: usize) -> Result<Vec<u8>, HostError>;
    fn run_bounded(&self, request: &ProcessRequest<'_>) -> Result<ProcessOutput, HostError>;
    fn observe_ups(&self, address: &str, port: u16, ups: &str) -> UpsObservation;
    fn list_observed_ups(
        &self,
        address: &str,
        port: u16,
    ) -> Result<Vec<(String, String)>, HostError>;
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

    fn metadata(&self, path: &Path) -> Result<Option<FileMetadata>, HostError> {
        match fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(unix_metadata(&meta))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                Err(HostError::permission_denied(path))
            }
            Err(error) => Err(HostError::from_io(
                &format!("cannot stat {}", path.display()),
                error,
            )),
        }
    }

    fn current_exe(&self) -> Result<PathBuf, HostError> {
        std::env::current_exe()
            .map_err(|error| HostError::from_io("cannot resolve current executable", error))
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }

    fn run_program(&self, program: &OsStr, args: &[OsString]) -> Result<ProcessOutput, HostError> {
        let output = process::command(program)
            .args(args)
            .output()
            .map_err(|error| {
                HostError::from_io(&format!("cannot run {}", program.to_string_lossy()), error)
            })?;
        Ok(ProcessOutput {
            status: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn stream_program(
        &self,
        program: &OsStr,
        args: &[OsString],
        timeout: Option<Duration>,
        line: &mut dyn FnMut(&[u8]) -> Result<(), HostError>,
    ) -> Result<ProcessOutput, HostError> {
        stream::run(program, args, timeout, line)
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
            let output = process::command(OsStr::new("stty"))
                .arg("size")
                .output()
                .ok()?;
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

    fn read_protected(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError> {
        protect::read_protected(path)
    }

    fn replace_file_atomic(
        &self,
        path: &Path,
        contents: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), HostError> {
        protect::replace_file_atomic(path, contents, metadata)
    }

    fn remove_protected(&self, path: &Path) -> Result<(), HostError> {
        protect::remove_protected(path)
    }

    fn set_metadata(&self, path: &Path, metadata: &FileMetadata) -> Result<(), HostError> {
        protect::set_metadata(path, metadata)
    }

    fn create_secure_dir(&self, path: &Path, metadata: &FileMetadata) -> Result<(), HostError> {
        protect::create_secure_dir(path, metadata)
    }

    fn lock_exclusive(&self, path: &Path) -> Result<Box<dyn ApplyLock>, HostError> {
        protect::lock_exclusive(path)
    }

    fn random_bytes(&self, len: usize) -> Result<Vec<u8>, HostError> {
        let mut bytes = vec![0u8; len];
        protect::fill_random(&mut bytes)?;
        Ok(bytes)
    }

    fn run_bounded(&self, request: &ProcessRequest<'_>) -> Result<ProcessOutput, HostError> {
        process::run_bounded(request)
    }

    fn observe_ups(&self, address: &str, port: u16, ups: &str) -> UpsObservation {
        observe::collect_native(address, port, ups)
    }

    fn list_observed_ups(
        &self,
        address: &str,
        port: u16,
    ) -> Result<Vec<(String, String)>, HostError> {
        observe::list_native(address, port)
    }
}

fn unix_metadata(meta: &fs::Metadata) -> FileMetadata {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileMetadata {
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.mode() & 0o7777,
            is_file: meta.file_type().is_file(),
            is_dir: meta.file_type().is_dir(),
        }
    }
    #[cfg(not(unix))]
    {
        FileMetadata {
            uid: 0,
            gid: 0,
            mode: if meta.is_dir() { 0o755 } else { 0o644 },
            is_file: meta.is_file(),
            is_dir: meta.is_dir(),
        }
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
