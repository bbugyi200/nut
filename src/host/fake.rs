use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use super::{
    ApplyLock, FileMetadata, Host, HostError, ProcessOutput, ProcessRequest, UpsObservation,
};

type CommandQueue = VecDeque<(
    Option<(OsString, Vec<OsString>)>,
    Result<ProcessOutput, String>,
)>;
type FailureQueue = VecDeque<(String, usize, String)>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostEffect {
    WriteFile {
        path: PathBuf,
        len: usize,
    },
    ReplaceFile {
        path: PathBuf,
        len: usize,
    },
    Remove {
        path: PathBuf,
    },
    CreateDir {
        path: PathBuf,
    },
    SetMetadata {
        path: PathBuf,
    },
    Lock {
        path: PathBuf,
    },
    Random {
        len: usize,
    },
    Run {
        program: OsString,
        args: Vec<OsString>,
    },
    ObserveUps {
        address: String,
        port: u16,
        ups: String,
    },
}

#[derive(Default)]
pub struct FakeHost {
    env: BTreeMap<String, OsString>,
    files: RefCell<BTreeMap<PathBuf, Vec<u8>>>,
    dirs: RefCell<BTreeSet<PathBuf>>,
    unreadable: BTreeMap<PathBuf, ()>,
    metadata: RefCell<BTreeMap<PathBuf, FileMetadata>>,
    symlinks: RefCell<BTreeSet<PathBuf>>,
    hardlinks: RefCell<BTreeSet<PathBuf>>,
    programs: BTreeMap<OsString, ProcessOutput>,
    program_args: BTreeMap<(OsString, Vec<OsString>), ProcessOutput>,
    command_queue: RefCell<CommandQueue>,
    ups_queue: RefCell<VecDeque<UpsObservation>>,
    ups_list: RefCell<Vec<(String, String)>>,
    entropy: RefCell<Vec<u8>>,
    failures: RefCell<FailureQueue>,
    effects: RefCell<Vec<HostEffect>>,
    lock: Rc<RefCell<Option<PathBuf>>>,
    now: Option<SystemTime>,
    euid: Option<u32>,
    hostname: Option<String>,
    current_exe: Option<PathBuf>,
    stdin_tty: bool,
    stdout_tty: bool,
    stderr_tty: bool,
    prompts: RefCell<VecDeque<String>>,
    slept: RefCell<Duration>,
}

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

    pub fn with_metadata(self, path: impl Into<PathBuf>, metadata: FileMetadata) -> Self {
        self.metadata.borrow_mut().insert(path.into(), metadata);
        self
    }

    pub fn with_symlink(self, path: impl Into<PathBuf>) -> Self {
        self.symlinks.borrow_mut().insert(path.into());
        self
    }

    pub fn with_hardlink(self, path: impl Into<PathBuf>) -> Self {
        self.hardlinks.borrow_mut().insert(path.into());
        self
    }

    pub fn with_current_exe(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_exe = Some(path.into());
        self
    }

    pub fn slept(&self) -> Duration {
        *self.slept.borrow()
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

    pub fn queue_program(
        self,
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        output: ProcessOutput,
    ) -> Self {
        let args = args.into_iter().map(Into::into).collect();
        self.command_queue
            .borrow_mut()
            .push_back((Some((program.into(), args)), Ok(output)));
        self
    }

    pub fn queue_program_error(self, message: impl Into<String>) -> Self {
        self.command_queue
            .borrow_mut()
            .push_back((None, Err(message.into())));
        self
    }

    pub fn with_ups_observation(self, observation: UpsObservation) -> Self {
        self.ups_queue.borrow_mut().push_back(observation);
        self
    }

    pub fn with_ups_list(self, rows: impl IntoIterator<Item = (String, String)>) -> Self {
        self.ups_list.borrow_mut().extend(rows);
        self
    }

    pub fn with_entropy(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.entropy.borrow_mut().extend(bytes.into());
        self
    }

    pub fn fail_next(self, operation: impl Into<String>, message: impl Into<String>) -> Self {
        self.failures
            .borrow_mut()
            .push_back((operation.into(), 0, message.into()));
        self
    }

    /// Fail an operation after the requested number of matching successes.
    /// This makes durable transaction crash boundaries reproducible.
    pub fn fail_after(
        &self,
        operation: impl Into<String>,
        successful_calls: usize,
        message: impl Into<String>,
    ) {
        self.failures
            .borrow_mut()
            .push_back((operation.into(), successful_calls, message.into()));
    }

    pub fn effects(&self) -> Vec<HostEffect> {
        self.effects.borrow().clone()
    }

    pub fn snapshot_filesystem(&self) -> Self {
        Self {
            env: self.env.clone(),
            files: RefCell::new(self.files.borrow().clone()),
            dirs: RefCell::new(self.dirs.borrow().clone()),
            unreadable: self.unreadable.clone(),
            metadata: RefCell::new(self.metadata.borrow().clone()),
            symlinks: RefCell::new(self.symlinks.borrow().clone()),
            hardlinks: RefCell::new(self.hardlinks.borrow().clone()),
            programs: self.programs.clone(),
            program_args: self.program_args.clone(),
            command_queue: RefCell::new(VecDeque::new()),
            ups_queue: RefCell::new(VecDeque::new()),
            ups_list: RefCell::new(self.ups_list.borrow().clone()),
            entropy: RefCell::new(self.entropy.borrow().clone()),
            failures: RefCell::new(VecDeque::new()),
            effects: RefCell::new(Vec::new()),
            lock: Rc::new(RefCell::new(None)),
            now: self.now,
            euid: self.euid,
            hostname: self.hostname.clone(),
            current_exe: self.current_exe.clone(),
            stdin_tty: self.stdin_tty,
            stdout_tty: self.stdout_tty,
            stderr_tty: self.stderr_tty,
            prompts: RefCell::new(VecDeque::new()),
            slept: RefCell::new(Duration::ZERO),
        }
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

    fn check_failure(&self, operation: &str) -> Result<(), HostError> {
        let mut failures = self.failures.borrow_mut();
        if failures
            .front()
            .is_some_and(|(name, _, _)| name == operation)
        {
            let (name, remaining, message) = failures.pop_front().expect("front was present");
            if remaining == 0 {
                return Err(HostError::message(message));
            }
            failures.push_front((name, remaining - 1, message));
        }
        Ok(())
    }

    fn record(&self, effect: HostEffect) {
        self.effects.borrow_mut().push(effect);
    }

    fn reject_protected(&self, path: &Path) -> Result<(), HostError> {
        if !path.is_absolute() {
            return Err(HostError::unsafe_path(path, "path is not absolute"));
        }
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        {
            return Err(HostError::unsafe_path(path, "path traversal"));
        }
        if self.symlinks.borrow().contains(path) {
            return Err(HostError::unsafe_path(path, "symlink"));
        }
        if self.hardlinks.borrow().contains(path) {
            return Err(HostError::unsafe_path(path, "hardlink"));
        }
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if dir.as_os_str().is_empty() {
                break;
            }
            if self.symlinks.borrow().contains(dir) {
                return Err(HostError::unsafe_path(path, "symlink"));
            }
            if let Some(metadata) = self.metadata.borrow().get(dir)
                && metadata.mode & 0o022 != 0
            {
                return Err(HostError::unsafe_path(path, "writable ancestor"));
            }
            if dir == Path::new("/") {
                break;
            }
            parent = dir.parent();
        }
        Ok(())
    }
}

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
        self.record(HostEffect::WriteFile {
            path: path.to_path_buf(),
            len: contents.len(),
        });
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
            || self.metadata.borrow().contains_key(path)
            || self
                .files
                .borrow()
                .keys()
                .any(|file| file.starts_with(path))
            || self.dirs.borrow().iter().any(|dir| dir.starts_with(path)))
    }

    fn metadata(&self, path: &Path) -> Result<Option<FileMetadata>, HostError> {
        if self.unreadable.contains_key(path) {
            return Err(HostError::permission_denied(path));
        }
        if let Some(metadata) = self.metadata.borrow().get(path).cloned() {
            return Ok(Some(metadata));
        }
        if self.files.borrow().contains_key(path) {
            return Ok(Some(FileMetadata {
                uid: 0,
                gid: 0,
                mode: 0o644,
                is_file: true,
                is_dir: false,
            }));
        }
        if self.dirs.borrow().contains(path) {
            return Ok(Some(FileMetadata {
                uid: 0,
                gid: 0,
                mode: 0o755,
                is_file: false,
                is_dir: true,
            }));
        }
        Ok(None)
    }

    fn current_exe(&self) -> Result<PathBuf, HostError> {
        self.current_exe
            .clone()
            .ok_or_else(|| HostError::message("cannot resolve current executable"))
    }

    fn sleep(&self, duration: Duration) {
        let mut slept = self.slept.borrow_mut();
        *slept = slept.saturating_add(duration);
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

    fn read_protected(&self, path: &Path) -> Result<Option<Vec<u8>>, HostError> {
        self.check_failure("read_protected")?;
        self.reject_protected(path)?;
        self.read_file(path)
    }

    fn replace_file_atomic(
        &self,
        path: &Path,
        contents: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), HostError> {
        self.check_failure("replace_file")?;
        self.reject_protected(path)?;
        insert_ancestors(&self.dirs, path);
        self.files
            .borrow_mut()
            .insert(path.to_path_buf(), contents.to_vec());
        self.metadata
            .borrow_mut()
            .insert(path.to_path_buf(), metadata.clone());
        self.symlinks.borrow_mut().remove(path);
        self.record(HostEffect::ReplaceFile {
            path: path.to_path_buf(),
            len: contents.len(),
        });
        Ok(())
    }

    fn remove_protected(&self, path: &Path) -> Result<(), HostError> {
        self.check_failure("remove_protected")?;
        self.reject_protected(path)?;
        self.files.borrow_mut().remove(path);
        self.metadata.borrow_mut().remove(path);
        self.record(HostEffect::Remove {
            path: path.to_path_buf(),
        });
        Ok(())
    }

    fn set_metadata(&self, path: &Path, metadata: &FileMetadata) -> Result<(), HostError> {
        self.check_failure("set_metadata")?;
        self.reject_protected(path)?;
        self.metadata
            .borrow_mut()
            .insert(path.to_path_buf(), metadata.clone());
        self.record(HostEffect::SetMetadata {
            path: path.to_path_buf(),
        });
        Ok(())
    }

    fn create_secure_dir(&self, path: &Path, metadata: &FileMetadata) -> Result<(), HostError> {
        self.check_failure("create_secure_dir")?;
        self.reject_protected(path)?;
        insert_ancestors(&self.dirs, path);
        self.dirs.borrow_mut().insert(path.to_path_buf());
        self.metadata
            .borrow_mut()
            .insert(path.to_path_buf(), metadata.clone());
        self.record(HostEffect::CreateDir {
            path: path.to_path_buf(),
        });
        Ok(())
    }

    fn lock_exclusive(&self, path: &Path) -> Result<Box<dyn ApplyLock>, HostError> {
        self.check_failure("lock")?;
        self.reject_protected(path)?;
        let mut held = self.lock.borrow_mut();
        if held.is_some() {
            return Err(HostError::message(format!(
                "apply lock is held: {}",
                path.display()
            )));
        }
        *held = Some(path.to_path_buf());
        drop(held);
        self.record(HostEffect::Lock {
            path: path.to_path_buf(),
        });
        Ok(Box::new(FakeApplyLock {
            path: path.to_path_buf(),
            held: Rc::clone(&self.lock),
        }))
    }

    fn random_bytes(&self, len: usize) -> Result<Vec<u8>, HostError> {
        self.check_failure("random_bytes")?;
        let mut entropy = self.entropy.borrow_mut();
        if entropy.len() < len {
            let fill = len - entropy.len();
            entropy.extend(std::iter::repeat_n(0x11, fill));
        }
        let bytes = entropy.drain(..len).collect();
        self.record(HostEffect::Random { len });
        Ok(bytes)
    }

    fn run_bounded(&self, request: &ProcessRequest<'_>) -> Result<ProcessOutput, HostError> {
        self.check_failure("run_bounded")?;
        self.record(HostEffect::Run {
            program: request.program.to_os_string(),
            args: request.args.to_vec(),
        });
        let mut queue = self.command_queue.borrow_mut();
        if let Some((wanted, result)) = queue.pop_front() {
            if let Some((program, args)) = wanted
                && (program != request.program || args != request.args)
            {
                queue.push_front((Some((program, args)), result));
            } else {
                return result.map_err(HostError::message);
            }
        }
        drop(queue);
        self.run_program(request.program, request.args)
    }

    fn observe_ups(&self, address: &str, port: u16, ups: &str) -> UpsObservation {
        self.record(HostEffect::ObserveUps {
            address: address.to_owned(),
            port,
            ups: ups.to_owned(),
        });
        self.ups_queue.borrow_mut().pop_front().unwrap_or_else(|| {
            UpsObservation::unavailable(format!(
                "cannot connect to upsd at {address}:{port}: Connection refused (os error 111)"
            ))
        })
    }

    fn list_observed_ups(
        &self,
        _address: &str,
        _port: u16,
    ) -> Result<Vec<(String, String)>, HostError> {
        Ok(self.ups_list.borrow().clone())
    }
}

struct FakeApplyLock {
    path: PathBuf,
    held: Rc<RefCell<Option<PathBuf>>>,
}

impl ApplyLock for FakeApplyLock {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FakeApplyLock {
    fn drop(&mut self) {
        *self.held.borrow_mut() = None;
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::time::Duration;

    fn file_meta() -> FileMetadata {
        FileMetadata {
            uid: 0,
            gid: 0,
            mode: 0o640,
            is_file: true,
            is_dir: false,
        }
    }

    #[test]
    fn fake_timeout_and_failure_never_touch_os_or_upsd() {
        let host = FakeHost::default()
            .queue_program_error("sleep timed out")
            .fail_next("replace_file", "injected failure")
            .with_file("/etc/nut/nut.conf", b"original");
        let request = ProcessRequest {
            program: OsStr::new("sleep"),
            args: &[OsString::from("30")],
            timeout: Duration::from_secs(30),
            secret: true,
            inherit_tty: false,
        };
        let error = host.run_bounded(&request).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let error = host
            .replace_file_atomic(Path::new("/etc/nut/nut.conf"), b"new", &file_meta())
            .unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(
            host.read_file(Path::new("/etc/nut/nut.conf")).unwrap(),
            Some(b"original".to_vec())
        );
        let observation = host.observe_ups("127.0.0.1", 3493, "cp1500");
        assert!(
            observation
                .vars_error
                .as_deref()
                .is_some_and(|error| error.contains("Connection refused"))
        );
    }

    #[test]
    fn snapshot_filesystem_resumes_after_injected_failure() {
        let host = FakeHost::default()
            .with_file("/etc/nut/nut.conf", b"original")
            .fail_next("replace_file", "crash after plan");
        host.replace_file_atomic(Path::new("/etc/nut/nut.conf"), b"new", &file_meta())
            .unwrap_err();
        let resumed = host.snapshot_filesystem();
        assert_eq!(
            resumed
                .read_protected(Path::new("/etc/nut/nut.conf"))
                .unwrap(),
            Some(b"original".to_vec())
        );
        resumed
            .replace_file_atomic(Path::new("/etc/nut/nut.conf"), b"new", &file_meta())
            .unwrap();
        assert_eq!(
            resumed
                .read_file(Path::new("/etc/nut/nut.conf"))
                .unwrap()
                .as_deref(),
            Some(b"new".as_slice())
        );
        assert!(
            resumed
                .effects()
                .iter()
                .any(|effect| matches!(effect, HostEffect::ReplaceFile { .. }))
        );
    }

    #[test]
    fn fake_rejects_symlink_and_scripted_entropy() {
        let host = FakeHost::default()
            .with_symlink(PathBuf::from("/etc/nut"))
            .with_entropy(vec![1, 2, 3, 4]);
        let error = host
            .replace_file_atomic(Path::new("/etc/nut/nut.conf"), b"x", &file_meta())
            .unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(host.random_bytes(4).unwrap(), vec![1, 2, 3, 4]);
    }
}
