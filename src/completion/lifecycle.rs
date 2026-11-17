use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Shell;
use crate::cli::CompletionAction;
use crate::error::NutError;
use crate::host::Host;

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    adapters: BTreeMap<Shell, Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    sha256: String,
    protocol: u32,
}

/// Deadline for real-shell probes. It exists to bound hung shells, not to
/// measure speed.
const PROBE_DEADLINE: Duration = Duration::from_secs(10);

fn failure(error: impl std::fmt::Display) -> NutError {
    NutError::operational(format!("completion: {error}"))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn state_path<H: Host>(host: &H) -> Result<PathBuf, NutError> {
    let root = host
        .env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| host.home_dir().map(|home| home.join(".local/state")))
        .ok_or_else(|| failure("HOME is unset"))?;
    Ok(root.join("nut-cli/completion.json"))
}

fn load(path: &Path) -> Result<Manifest, NutError> {
    match fs::read(path) {
        Ok(bytes) => {
            let manifest: Manifest = serde_json::from_slice(&bytes).map_err(failure)?;
            if manifest.schema_version != 1 {
                return Err(failure("unsupported manifest version"));
            }
            for (shell, entry) in &manifest.adapters {
                if !entry.path.is_absolute()
                    || entry.path.file_name() != Some(shell.filename().as_ref())
                {
                    return Err(failure("invalid adapter path in manifest"));
                }
            }
            Ok(manifest)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Manifest {
            schema_version: 1,
            ..Default::default()
        }),
        Err(error) => Err(failure(error)),
    }
}

fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> Result<(), NutError> {
    let parent = path.parent().ok_or_else(|| failure("path has no parent"))?;
    fs::create_dir_all(parent).map_err(failure)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(failure)?;
    temp.write_all(bytes).map_err(failure)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(if private {
                0o600
            } else {
                0o644
            }))
            .map_err(failure)?;
    }
    temp.as_file().sync_all().map_err(failure)?;
    temp.persist(path).map_err(failure)?;
    fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(failure)
}

/// Bounded shell probes use fixed scripts and pass paths as argv. No eval or
/// shell interpolation of filenames; output and execution time are bounded.
fn probe(shell: Shell, flags: &str, script: &str, args: &[&Path]) -> Option<Vec<u8>> {
    let mut command = Command::new(shell.name());
    command
        .args([flags, script, "nut-completion-probe"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if flags != "-ic" {
        command.env_remove("BASH_ENV").env_remove("ENV");
    }
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(64 * 1024).read_to_end(&mut bytes).ok()?;
        let _ = sender.send(bytes);
        Some(())
    });
    let start = Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if start.elapsed() < PROBE_DEADLINE => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                #[cfg(unix)]
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    // A startup script may leave descendants holding stdout open; never wait
    // beyond the original deadline for its output.
    let result = if success {
        receiver
            .recv_timeout(PROBE_DEADLINE.saturating_sub(start.elapsed()))
            .ok()
    } else {
        None
    };
    #[cfg(unix)]
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    result
}

fn registration(shell: Shell, path: &Path) -> bool {
    let script = match shell {
        Shell::Zsh => {
            "fpath=(\"$1\" $fpath); autoload -Uz compinit; compinit -D -i; [[ ${_comps[nut]} == _nut ]] && print -r -- registered"
        }
        Shell::Bash => {
            "source \"$1\"; [[ $(complete -p nut) == 'complete -F _nut nut' ]] && printf registered"
        }
    };
    let target = if shell == Shell::Zsh {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    probe(
        shell,
        if shell == Shell::Zsh { "-fc" } else { "-c" },
        script,
        &[target],
    )
    .is_some_and(|output| output.starts_with(b"registered"))
}

fn target<H: Host>(
    shell: Shell,
    directory: Option<&Path>,
    manifest: &Manifest,
    host: &H,
) -> Result<PathBuf, NutError> {
    if let Some(directory) = directory {
        let directory = std::path::absolute(directory).map_err(failure)?;
        return Ok(directory.join(shell.filename()));
    }
    if let Some(entry) = manifest.adapters.get(&shell) {
        return Ok(entry.path.clone());
    }
    let home = host.home_dir().ok_or_else(|| failure("HOME is unset"))?;
    let directory = match shell {
        Shell::Zsh => {
            let output =
                probe(shell, "-ic", "printf '%s\\0' \"${fpath[@]}\"", &[]).unwrap_or_default();
            output
                .split(|byte| *byte == 0)
                .filter_map(|bytes| std::str::from_utf8(bytes).ok())
                .map(PathBuf::from)
                .find(|path| {
                    path.is_absolute()
                        && path.starts_with(&home)
                        && user_owned_directory(path, host.euid())
                })
                .unwrap_or_else(|| home.join(".zfunc"))
        }
        Shell::Bash => host
            .env("BASH_COMPLETION_USER_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| {
                host.env("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .unwrap_or_else(|| home.join(".local/share"))
                    .join("bash-completion")
            })
            .join("completions"),
    };
    Ok(directory.join(shell.filename()))
}

fn user_owned_directory(path: &Path, uid: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).is_ok_and(|metadata| metadata.is_dir() && metadata.uid() == uid)
    }
    #[cfg(not(unix))]
    {
        let _ = uid;
        path.is_dir()
    }
}

fn selected<H: Host>(shell: Option<Shell>, host: &H) -> Result<Shell, NutError> {
    shell
        .or_else(|| {
            let path = PathBuf::from(host.env("SHELL")?);
            match path.file_name()?.to_str()? {
                "zsh" => Some(Shell::Zsh),
                "bash" => Some(Shell::Bash),
                _ => None,
            }
        })
        .ok_or_else(|| NutError::usage("select a shell: nut completion install zsh|bash"))
}

fn owned(path: &Path, entry: &Entry) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        && fs::read(path).is_ok_and(|bytes| digest(&bytes) == entry.sha256)
}

pub fn run<H: Host, W: Write>(
    action: Option<CompletionAction>,
    json: bool,
    host: &H,
    output: &mut W,
) -> Result<(), NutError> {
    let action = action.unwrap_or(CompletionAction::Status);
    if matches!(action, CompletionAction::Zsh | CompletionAction::Bash) {
        let shell = if matches!(action, CompletionAction::Zsh) {
            Shell::Zsh
        } else {
            Shell::Bash
        };
        return write!(output, "{}", shell.adapter()).map_err(failure);
    }
    let path = state_path(host)?;
    let mut manifest = load(&path)?;
    match action {
        CompletionAction::Install {
            shell,
            dry_run,
            target: directory,
            force,
        } => {
            let shell = selected(shell, host)?;
            let adapter = target(shell, directory.as_deref(), &manifest, host)?;
            if let Some(entry) = manifest.adapters.get(&shell)
                && entry.path != adapter
            {
                return Err(failure(
                    "uninstall the recorded adapter before changing its target",
                ));
            }
            if fs::symlink_metadata(&adapter).is_ok()
                && !force
                && !manifest
                    .adapters
                    .get(&shell)
                    .is_some_and(|entry| owned(&adapter, entry))
            {
                return Err(failure(format!(
                    "refusing foreign or modified file {}",
                    adapter.display()
                ))
                .with_hint(format!(
                    "nut completion install {} --target {:?} --force",
                    shell.name(),
                    adapter.parent().unwrap()
                )));
            }
            if dry_run {
                return writeln!(
                    output,
                    "Would install {} completion at {}",
                    shell.name(),
                    adapter.display()
                )
                .map_err(failure);
            }
            let bytes = shell.adapter().as_bytes();
            let current = fs::read(&adapter).is_ok_and(|existing| existing == bytes)
                && manifest
                    .adapters
                    .get(&shell)
                    .is_some_and(|entry| owned(&adapter, entry));
            if !current {
                atomic_write(&adapter, bytes, false)?;
            }
            manifest.adapters.insert(
                shell,
                Entry {
                    path: adapter.clone(),
                    sha256: digest(bytes),
                    protocol: 1,
                },
            );
            // Retain ownership even when the subsequent shell probe fails, so an
            // unavailable shell doesn't leave an untracked adapter behind.
            let serialized = serde_json::to_vec_pretty(&manifest).map_err(failure)?;
            if !fs::read(&path).is_ok_and(|bytes| bytes == serialized) {
                atomic_write(&path, &serialized, true)?;
            }
            if !registration(shell, &adapter) {
                return Err(
                    failure("adapter written, but real-shell registration probe failed")
                        .with_hint("Install bash/zsh, then run: nut completion status"),
                );
            }
            writeln!(
                output,
                "{} {} completion at {} (isolated shell registration verified)",
                if current { "Current" } else { "Installed" },
                shell.name(),
                adapter.display()
            )
            .map_err(failure)?;
            match shell {
                Shell::Zsh => writeln!(output, "Add to ~/.zshrc before compinit: fpath=({} $fpath)\nThen restart zsh or run: autoload -Uz compinit; compinit", shell_quote(adapter.parent().unwrap())).map_err(failure),
                Shell::Bash => writeln!(output, "Enable bash-completion, or add to ~/.bashrc: source {}", shell_quote(&adapter)).map_err(failure),
            }
        }
        CompletionAction::Status => {
            let mut rows = Vec::new();
            for shell in [Shell::Zsh, Shell::Bash] {
                // Status inspects only tracked installs; it never launches the
                // user's startup scripts or creates a fallback target.
                let entry = manifest.adapters.get(&shell);
                let current = entry.is_some_and(|entry| {
                    owned(&entry.path, entry)
                        && entry.sha256 == digest(shell.adapter().as_bytes())
                        && entry.protocol == 1
                });
                let state = match entry {
                    None => "not-installed",
                    Some(entry) if fs::symlink_metadata(&entry.path).is_err() => "missing",
                    Some(entry) if !owned(&entry.path, entry) => "modified",
                    Some(_) if current => "current",
                    Some(_) => "outdated",
                };
                let registered =
                    current && entry.is_some_and(|entry| registration(shell, &entry.path));
                rows.push(serde_json::json!({"shell": shell, "path": entry.map(|entry| &entry.path), "state": state, "current": current, "registered": registered, "registration_scope": "isolated-shell"}));
                if !json {
                    writeln!(
                        output,
                        "{}: {state}; isolated shell registration: {}",
                        shell.name(),
                        if registered { "verified" } else { "unverified" }
                    )
                    .map_err(failure)?;
                }
            }
            if json {
                writeln!(
                    output,
                    "{}",
                    serde_json::json!({"schema_version": 1, "adapters": rows})
                )
                .map_err(failure)?;
            }
            Ok(())
        }
        CompletionAction::Uninstall { shell, dry_run } => {
            let shells: Vec<_> = shell
                .map(|shell| vec![shell])
                .unwrap_or_else(|| manifest.adapters.keys().copied().collect());
            // Preflight every file before removing any, preserving foreign edits.
            for shell in &shells {
                if let Some(entry) = manifest.adapters.get(shell)
                    && fs::symlink_metadata(&entry.path).is_ok()
                    && !owned(&entry.path, entry)
                {
                    return Err(failure(format!(
                        "refusing to remove modified file {}",
                        entry.path.display()
                    )));
                }
            }
            for shell in shells {
                if let Some(entry) = manifest.adapters.remove(&shell) {
                    writeln!(
                        output,
                        "{} {}",
                        if dry_run { "Would remove" } else { "Removing" },
                        entry.path.display()
                    )
                    .map_err(failure)?;
                    if !dry_run {
                        match fs::remove_file(&entry.path) {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(failure(error)),
                        }
                    }
                }
            }
            if !dry_run && path.exists() {
                atomic_write(
                    &path,
                    &serde_json::to_vec_pretty(&manifest).map_err(failure)?,
                    true,
                )?;
            }
            Ok(())
        }
        CompletionAction::Zsh | CompletionAction::Bash => unreachable!(),
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
