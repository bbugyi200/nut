use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use rustix::fs::{self as rfs, AtFlags, FileType, FlockOperation, Mode, OFlags, Stat};
use rustix::io::Errno;
use rustix::process::{Gid, Uid, getegid, geteuid};

use super::{ApplyLock, FileMetadata, HostError};

const MAX_PROTECTED_BYTES: usize = 16 * 1024 * 1024;
const TEMP_RANDOM_BYTES: usize = 8;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn fail_next_rename() {
    FAIL_NEXT_RENAME.with(|flag| flag.set(true));
}

pub(super) fn read_protected(path: &Path) -> Result<Option<Vec<u8>>, HostError> {
    let (dir, name) = open_parent(path)?;
    match inspect_target(&dir, &name, path, TargetKind::File)? {
        None => Ok(None),
        Some(_) => {
            let fd = rfs::openat(
                &dir,
                name.as_os_str(),
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| map_errno("cannot open protected file", path, error))?;
            let mut file = File::from(fd);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(|error| {
                HostError::from_io(&format!("cannot read {}", path.display()), error)
            })?;
            if bytes.len() > MAX_PROTECTED_BYTES {
                return Err(HostError::message(format!(
                    "{} exceeds {MAX_PROTECTED_BYTES} bytes",
                    path.display()
                )));
            }
            Ok(Some(bytes))
        }
    }
}

pub(super) fn replace_file_atomic(
    path: &Path,
    contents: &[u8],
    metadata: &FileMetadata,
) -> Result<(), HostError> {
    if contents.len() > MAX_PROTECTED_BYTES {
        return Err(HostError::message(format!(
            "{} exceeds {MAX_PROTECTED_BYTES} bytes",
            path.display()
        )));
    }
    let (dir, name) = open_parent(path)?;
    inspect_target(&dir, &name, path, TargetKind::File)?;
    let temp_name = unique_temp_name(&name)?;
    let temp_fd = rfs::openat(
        &dir,
        temp_name.as_os_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| map_errno("cannot create protected temporary file", path, error))?;
    let publish = (|| {
        let mut file = File::from(temp_fd);
        file.write_all(contents).map_err(|error| {
            HostError::from_io(
                &format!("cannot write temporary file for {}", path.display()),
                error,
            )
        })?;
        apply_fd_metadata(&file, metadata, path)?;
        file.sync_all().map_err(|error| {
            HostError::from_io(&format!("cannot fsync {}", path.display()), error)
        })?;
        drop(file);
        rename_atomic(&dir, &temp_name, &name, path)?;
        rfs::fsync(&dir)
            .map_err(|error| map_errno("cannot fsync parent directory", path, error))?;
        Ok(())
    })();
    if publish.is_err() {
        let _ = rfs::unlinkat(&dir, temp_name.as_os_str(), AtFlags::empty());
        let _ = rfs::fsync(&dir);
    }
    publish
}

pub(super) fn remove_protected(path: &Path) -> Result<(), HostError> {
    let (dir, name) = open_parent(path)?;
    match inspect_target(&dir, &name, path, TargetKind::File)? {
        None => Ok(()),
        Some(_) => {
            rfs::unlinkat(&dir, name.as_os_str(), AtFlags::empty())
                .map_err(|error| map_errno("cannot remove protected file", path, error))?;
            rfs::fsync(&dir)
                .map_err(|error| map_errno("cannot fsync parent directory", path, error))?;
            Ok(())
        }
    }
}

pub(super) fn set_metadata(path: &Path, metadata: &FileMetadata) -> Result<(), HostError> {
    let (dir, name) = open_parent(path)?;
    let kind = if metadata.is_dir {
        TargetKind::Dir
    } else {
        TargetKind::File
    };
    inspect_target(&dir, &name, path, kind)?.ok_or_else(|| {
        HostError::message(format!(
            "cannot change metadata of missing {}",
            path.display()
        ))
    })?;
    let flags = if metadata.is_dir {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    } else {
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    };
    let fd = rfs::openat(&dir, name.as_os_str(), flags, Mode::empty())
        .map_err(|error| map_errno("cannot open for metadata change", path, error))?;
    apply_fd_metadata(&fd, metadata, path)?;
    rfs::fsync(&fd)
        .map_err(|error| map_errno("cannot fsync after metadata change", path, error))?;
    rfs::fsync(&dir).map_err(|error| map_errno("cannot fsync parent directory", path, error))?;
    Ok(())
}

pub(super) fn create_secure_dir(path: &Path, metadata: &FileMetadata) -> Result<(), HostError> {
    let (dir, name) = open_parent(path)?;
    match inspect_target(&dir, &name, path, TargetKind::DirOrMissing)? {
        Some(_) => {
            let fd = rfs::openat(
                &dir,
                name.as_os_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| map_errno("cannot open secure directory", path, error))?;
            apply_fd_metadata(&fd, metadata, path)?;
            rfs::fsync(&fd)
                .map_err(|error| map_errno("cannot fsync secure directory", path, error))?;
        }
        None => {
            rfs::mkdirat(&dir, name.as_os_str(), Mode::from_raw_mode(metadata.mode))
                .map_err(|error| map_errno("cannot create secure directory", path, error))?;
            let fd = rfs::openat(
                &dir,
                name.as_os_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| map_errno("cannot open created directory", path, error))?;
            apply_fd_metadata(&fd, metadata, path)?;
            rfs::fsync(&fd)
                .map_err(|error| map_errno("cannot fsync created directory", path, error))?;
        }
    }
    rfs::fsync(&dir).map_err(|error| map_errno("cannot fsync parent directory", path, error))?;
    Ok(())
}

pub(super) fn lock_exclusive(path: &Path) -> Result<Box<dyn ApplyLock>, HostError> {
    let (dir, name) = open_parent(path)?;
    match inspect_target(&dir, &name, path, TargetKind::FileOrMissing)? {
        Some(_) | None => {}
    }
    let fd = rfs::openat(
        &dir,
        name.as_os_str(),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| map_errno("cannot open apply lock", path, error))?;
    let st = rfs::fstat(&fd).map_err(|error| map_errno("cannot stat apply lock", path, error))?;
    reject_special_or_link(&st, path, TargetKind::File)?;
    rfs::fchmod(&fd, Mode::from_raw_mode(0o600))
        .map_err(|error| map_errno("cannot chmod apply lock", path, error))?;
    match rfs::flock(&fd, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(error) if error == Errno::WOULDBLOCK || error == Errno::AGAIN => {
            return Err(HostError::message(format!(
                "apply lock is held: {}",
                path.display()
            )));
        }
        Err(error) => return Err(map_errno("cannot acquire apply lock", path, error)),
    }
    rfs::fsync(&dir).map_err(|error| map_errno("cannot fsync lock parent", path, error))?;
    Ok(Box::new(SystemApplyLock {
        path: path.to_path_buf(),
        fd,
    }))
}

struct SystemApplyLock {
    path: std::path::PathBuf,
    fd: rustix::fd::OwnedFd,
}

impl ApplyLock for SystemApplyLock {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SystemApplyLock {
    fn drop(&mut self) {
        let _ = rfs::flock(&self.fd, FlockOperation::Unlock);
    }
}

fn apply_fd_metadata<Fd: rustix::fd::AsFd>(
    fd: Fd,
    metadata: &FileMetadata,
    path: &Path,
) -> Result<(), HostError> {
    rfs::fchmod(&fd, Mode::from_raw_mode(metadata.mode))
        .map_err(|error| map_errno("cannot chmod protected path", path, error))?;
    let uid = Uid::from_raw(metadata.uid);
    let gid = Gid::from_raw(metadata.gid);
    match rfs::fchown(&fd, Some(uid), Some(gid)) {
        Ok(()) => Ok(()),
        Err(error)
            if (error == Errno::PERM || error == Errno::ACCESS)
                && metadata.uid == geteuid().as_raw()
                && metadata.gid == getegid().as_raw() =>
        {
            Ok(())
        }
        Err(error) => Err(map_errno("cannot chown protected path", path, error)),
    }
}

fn rename_atomic(
    dir: &rustix::fd::OwnedFd,
    from: &OsString,
    to: &OsString,
    path: &Path,
) -> Result<(), HostError> {
    #[cfg(test)]
    if FAIL_NEXT_RENAME.with(|flag| flag.replace(false)) {
        return Err(HostError::message(format!(
            "injected rename failure for {}",
            path.display()
        )));
    }
    rfs::renameat(dir, from.as_os_str(), dir, to.as_os_str())
        .map_err(|error| map_errno("cannot publish protected file", path, error))
}

fn unique_temp_name(basename: &OsString) -> Result<OsString, HostError> {
    let mut suffix = [0u8; TEMP_RANDOM_BYTES];
    fill_random(&mut suffix)?;
    let mut name = OsString::from(".");
    name.push(basename);
    name.push(".nut-");
    name.push(OsStr::from_bytes(hex_encode(&suffix).as_bytes()));
    Ok(name)
}

pub(super) fn fill_random(bytes: &mut [u8]) -> Result<(), HostError> {
    let mut written = 0;
    while written < bytes.len() {
        match rustix::rand::getrandom(&mut bytes[written..], rustix::rand::GetRandomFlags::empty())
        {
            Ok(0) => {
                return Err(HostError::message("getrandom returned no bytes"));
            }
            Ok(n) => written += n,
            Err(error) if error == Errno::INTR => {}
            Err(error) => {
                return Err(HostError::from_io(
                    "cannot read entropy",
                    io::Error::from(error),
                ));
            }
        }
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

#[derive(Clone, Copy)]
enum TargetKind {
    File,
    Dir,
    FileOrMissing,
    DirOrMissing,
}

fn open_parent(path: &Path) -> Result<(rustix::fd::OwnedFd, OsString), HostError> {
    if !path.is_absolute() {
        return Err(HostError::unsafe_path(path, "path is not absolute"));
    }
    let basename = path
        .file_name()
        .ok_or_else(|| HostError::unsafe_path(path, "path has no file name"))?;
    if basename == "." || basename == ".." {
        return Err(HostError::unsafe_path(path, "path traversal"));
    }
    let mut dir = rfs::openat(
        rfs::ABS,
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| map_errno("cannot open root directory", path, error))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("/"));
    for component in parent.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let st = rfs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|error| map_errno("cannot stat ancestor", path, error))?;
                reject_special_or_link(&st, path, TargetKind::Dir)?;
                reject_writable_mode(&st, path)?;
                dir = rfs::openat(
                    &dir,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| map_errno("cannot open ancestor", path, error))?;
                let held = rfs::fstat(&dir)
                    .map_err(|error| map_errno("cannot recheck ancestor", path, error))?;
                reject_special_or_link(&held, path, TargetKind::Dir)?;
                reject_writable_mode(&held, path)?;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(HostError::unsafe_path(path, "path traversal"));
            }
        }
    }
    Ok((dir, basename.to_os_string()))
}

fn inspect_target(
    dir: &rustix::fd::OwnedFd,
    name: &OsString,
    path: &Path,
    kind: TargetKind,
) -> Result<Option<Stat>, HostError> {
    match rfs::statat(dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => {
            reject_special_or_link(&st, path, kind)?;
            Ok(Some(st))
        }
        Err(error) if error == Errno::NOENT => Ok(None),
        Err(error) => Err(map_errno("cannot stat protected path", path, error)),
    }
}

fn reject_special_or_link(st: &Stat, path: &Path, kind: TargetKind) -> Result<(), HostError> {
    let file_type = FileType::from_raw_mode(st.st_mode);
    match (kind, file_type) {
        (_, FileType::Symlink) => Err(HostError::unsafe_path(path, "symlink")),
        (TargetKind::File | TargetKind::FileOrMissing, FileType::RegularFile) => {
            if st.st_nlink > 1 {
                Err(HostError::unsafe_path(path, "hardlink"))
            } else {
                Ok(())
            }
        }
        (TargetKind::Dir | TargetKind::DirOrMissing, FileType::Directory) => Ok(()),
        (TargetKind::File | TargetKind::FileOrMissing, FileType::Directory) => {
            Err(HostError::unsafe_path(path, "directory"))
        }
        (TargetKind::Dir | TargetKind::DirOrMissing, FileType::RegularFile) => {
            Err(HostError::unsafe_path(path, "not a directory"))
        }
        _ => Err(HostError::unsafe_path(path, "special file")),
    }
}

fn reject_writable_mode(st: &Stat, path: &Path) -> Result<(), HostError> {
    let mode = Mode::from_raw_mode(st.st_mode).as_raw_mode();
    let euid = geteuid().as_raw();
    if euid == 0 && st.st_uid != 0 {
        return Err(HostError::unsafe_path(path, "unowned ancestor"));
    }
    if mode & 0o002 != 0 {
        return Err(HostError::unsafe_path(path, "writable ancestor"));
    }
    if mode & 0o020 != 0 && (euid == 0 || st.st_uid != euid) {
        return Err(HostError::unsafe_path(path, "writable ancestor"));
    }
    Ok(())
}

fn map_errno(context: &str, path: &Path, error: Errno) -> HostError {
    HostError::from_io(
        &format!("{context} {}", path.display()),
        io::Error::from(error),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Host, SystemHost};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    fn scratch() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        fs::create_dir_all(&base).unwrap();
        tempfile::TempDir::new_in(&base).unwrap()
    }

    fn self_meta(mode: u32, dir: bool) -> FileMetadata {
        FileMetadata {
            uid: geteuid().as_raw(),
            gid: getegid().as_raw(),
            mode,
            is_file: !dir,
            is_dir: dir,
        }
    }

    #[test]
    fn atomic_replace_writes_mode_and_cleans_temp_files() {
        let root = scratch();
        let path = root.path().join("target");
        fs::write(&path, "original").unwrap();
        SystemHost
            .replace_file_atomic(&path, b"replacement", &self_meta(0o640, false))
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let leftovers: Vec<PathBuf> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(leftovers, [path]);
    }

    #[test]
    fn injected_rename_failure_keeps_original_and_removes_temp() {
        let root = scratch();
        let path = root.path().join("target");
        fs::write(&path, "original").unwrap();
        fail_next_rename();
        let error = SystemHost
            .replace_file_atomic(&path, b"replacement", &self_meta(0o600, false))
            .unwrap_err();
        assert!(error.to_string().contains("injected rename failure"));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        let leftovers: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, [std::ffi::OsString::from("target")]);
    }

    #[test]
    fn rejects_symlink_target_and_symlink_ancestor() {
        let root = scratch();
        let target = root.path().join("link");
        symlink("/etc/passwd", &target).unwrap();
        let error = SystemHost
            .replace_file_atomic(&target, b"x", &self_meta(0o600, false))
            .unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
        assert!(
            fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let linked = root.path().join("through");
        symlink(&real, &linked).unwrap();
        let nested = linked.join("file");
        let error = SystemHost
            .replace_file_atomic(&nested, b"x", &self_meta(0o600, false))
            .unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
        assert!(fs::read_dir(&real).unwrap().next().is_none());
    }

    #[test]
    fn rejects_writable_ancestor_special_file_and_hardlink() {
        let root = scratch();
        let open = root.path().join("open");
        fs::create_dir(&open).unwrap();
        let mut permissions = fs::metadata(&open).unwrap().permissions();
        permissions.set_mode(0o777);
        fs::set_permissions(&open, permissions).unwrap();
        let nested = open.join("file");
        let error = SystemHost
            .replace_file_atomic(&nested, b"x", &self_meta(0o600, false))
            .unwrap_err();
        assert!(error.to_string().contains("writable ancestor"), "{error}");

        let fifo = root.path().join("fifo");
        rfs::mkfifoat(rfs::CWD, fifo.as_path(), Mode::from_raw_mode(0o600)).unwrap();
        let error = SystemHost
            .replace_file_atomic(&fifo, b"x", &self_meta(0o600, false))
            .unwrap_err();
        assert!(error.to_string().contains("special file"), "{error}");

        let file = root.path().join("regular");
        fs::write(&file, "one").unwrap();
        let alias = root.path().join("alias");
        fs::hard_link(&file, &alias).unwrap();
        let error = SystemHost
            .replace_file_atomic(&file, b"two", &self_meta(0o600, false))
            .unwrap_err();
        assert!(error.to_string().contains("hardlink"), "{error}");
        assert_eq!(fs::read(&file).unwrap(), b"one");
    }

    #[test]
    fn protected_read_skips_missing_and_secure_dir_lock_round_trip() {
        let root = scratch();
        let missing = root.path().join("absent");
        assert_eq!(SystemHost.read_protected(&missing).unwrap(), None);
        let dir = root.path().join("state");
        SystemHost
            .create_secure_dir(&dir, &self_meta(0o700, true))
            .unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let lock_path = dir.join("apply.lock");
        let lock = SystemHost.lock_exclusive(&lock_path).unwrap();
        let contended = match SystemHost.lock_exclusive(&lock_path) {
            Err(error) => error,
            Ok(_) => panic!("expected lock contention"),
        };
        assert!(contended.to_string().contains("apply lock is held"));
        drop(lock);
        let _released = SystemHost.lock_exclusive(&lock_path).unwrap();
    }
}
