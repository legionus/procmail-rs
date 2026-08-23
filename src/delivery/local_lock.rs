// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Local recipe locking for the supported Linux target.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fd::OwnedFd;
use rustix::fs::{
    AtFlags, FileType, FlockOperation, Mode, OFlags, flock, fstat, openat, statat, unlinkat,
};

use super::maildir::open_directory_path;

pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(1024);
const FLOCK_RETRY: Duration = Duration::from_millis(10);
const DOTLOCK_RETRY: Duration = Duration::from_secs(8);
const LOCK_FILE_MODE: u32 = 0o600;
const DOTLOCK_FILE_MODE: u32 = 0o444;
const MAX_STALE_DOTLOCK_SIZE: u64 = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LockMethod {
    #[default]
    Flock,
    Dotlock,
}

impl LockMethod {
    pub fn parse(value: &str) -> Result<Self, String> {
        crate::config::validate_lock_method(value)?;
        match value {
            "flock" => Ok(Self::Flock),
            "dotlock" => Ok(Self::Dotlock),
            _ => unreachable!(),
        }
    }

    pub fn from_config(config: &crate::config::Config) -> Result<Self, String> {
        let mut method = Self::Flock;
        for statement in &config.statements {
            let crate::config::Statement::Assignment(assignment) = statement else {
                continue;
            };
            if assignment.target != crate::config::AssignmentTarget::LockMethod {
                continue;
            }
            method = Self::parse(&assignment.value)
                .map_err(|error| format!("line {}: {error}", assignment.line))?;
        }
        Ok(method)
    }
}

pub fn parse_lock_timeout(value: &str) -> Result<Duration, String> {
    crate::config::parse_lock_timeout_seconds(value).map(Duration::from_secs)
}

pub fn lock_timeout_from_config(config: &crate::config::Config) -> Result<Duration, String> {
    let mut timeout = DEFAULT_LOCK_TIMEOUT;
    for statement in &config.statements {
        let crate::config::Statement::Assignment(assignment) = statement else {
            continue;
        };
        if assignment.target != crate::config::AssignmentTarget::LockTimeout {
            continue;
        }
        timeout = parse_lock_timeout(&assignment.value)
            .map_err(|error| format!("line {}: {error}", assignment.line))?;
    }
    Ok(timeout)
}

#[derive(Debug)]
pub struct LocalLock {
    state: LockState,
}

#[derive(Debug)]
enum LockState {
    Flock(OwnedFd),
    Dotlock { parent: OwnedFd, name: Box<OsStr> },
}

impl LocalLock {
    pub fn acquire(
        path: &Path,
        method: LockMethod,
        expected_uid: u32,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::acquire_with_mask(path, method, expected_uid, timeout, 0)
    }

    pub fn acquire_with_mask(
        path: &Path,
        method: LockMethod,
        expected_uid: u32,
        timeout: Duration,
        mask: u32,
    ) -> io::Result<Self> {
        let retry = match method {
            LockMethod::Flock => FLOCK_RETRY,
            LockMethod::Dotlock => DOTLOCK_RETRY,
        };
        Self::acquire_with_policy_and_mask(path, method, expected_uid, timeout, retry, mask)
    }

    #[cfg(test)]
    fn acquire_with_policy(
        path: &Path,
        method: LockMethod,
        expected_uid: u32,
        timeout: Duration,
        retry: Duration,
    ) -> io::Result<Self> {
        Self::acquire_with_policy_and_mask(path, method, expected_uid, timeout, retry, 0)
    }

    fn acquire_with_policy_and_mask(
        path: &Path,
        method: LockMethod,
        expected_uid: u32,
        timeout: Duration,
        retry: Duration,
        mask: u32,
    ) -> io::Result<Self> {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "lockfile path has no file name",
            )
        })?;
        let parent_path = path.parent().unwrap_or_else(|| Path::new("."));
        let parent = open_directory_path(parent_path)?;
        match method {
            LockMethod::Flock => acquire_flock(parent, name, expected_uid, timeout, retry, mask),
            LockMethod::Dotlock => acquire_dotlock(parent, name, timeout, retry, mask),
        }
    }
}

fn acquire_flock(
    parent: OwnedFd,
    name: &OsStr,
    expected_uid: u32,
    timeout: Duration,
    retry: Duration,
    mask: u32,
) -> io::Result<LocalLock> {
    let file = openat(
        &parent,
        name.as_bytes(),
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(LOCK_FILE_MODE & !mask),
    )
    .map_err(io_error)?;
    let stat = fstat(&file).map_err(io_error)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "flock lockfile is not a regular file",
        ));
    }
    if stat.st_uid != expected_uid || stat.st_nlink != 1 || stat.st_mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "flock lockfile has unsafe ownership, links, or permissions",
        ));
    }

    acquire_flock_fd(
        &file,
        timeout,
        retry,
        "timed out waiting for local lockfile",
    )?;
    Ok(LocalLock {
        state: LockState::Flock(file),
    })
}

pub(super) fn acquire_flock_fd(
    file: &OwnedFd,
    timeout: Duration,
    retry: Duration,
    timeout_message: &'static str,
) -> io::Result<()> {
    let started = Instant::now();
    loop {
        match flock(file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => {
                wait_for_retry(started, timeout, retry, timeout_message)?
            }
            Err(error) => return Err(io_error(error)),
        }
    }
}

fn acquire_dotlock(
    parent: OwnedFd,
    name: &OsStr,
    timeout: Duration,
    retry: Duration,
    mask: u32,
) -> io::Result<LocalLock> {
    let started = Instant::now();
    loop {
        match openat(
            &parent,
            name.as_bytes(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(DOTLOCK_FILE_MODE & !mask),
        ) {
            Ok(file) => {
                drop(file);
                return Ok(LocalLock {
                    state: LockState::Dotlock {
                        parent,
                        name: name.into(),
                    },
                });
            }
            Err(rustix::io::Errno::EXIST) => {
                if stale_dotlock(&parent, name, timeout)? {
                    // Compatibility mode intentionally mirrors procmail's
                    // pathname-based stale removal. Another process can
                    // replace this entry between inspection and unlink.
                    unlinkat(&parent, name.as_bytes(), AtFlags::empty()).map_err(io_error)?;
                    continue;
                }
                wait_for_retry(
                    started,
                    timeout,
                    retry,
                    "timed out waiting for local lockfile",
                )?;
            }
            Err(error) => return Err(io_error(error)),
        }
    }
}

fn stale_dotlock(parent: &OwnedFd, name: &OsStr, timeout: Duration) -> io::Result<bool> {
    if timeout.is_zero() {
        return Ok(false);
    }
    let stat = statat(parent, name.as_bytes(), AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory
        || stat.st_size < 0
        || u64::try_from(stat.st_size).unwrap_or(u64::MAX) > MAX_STALE_DOTLOCK_SIZE
    {
        return Ok(false);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| io::Error::other("system time precedes the Unix epoch"))?
        .as_secs();
    let modified = u64::try_from(stat.st_mtime).unwrap_or(0);
    Ok(now.saturating_sub(modified) > timeout.as_secs())
}

fn wait_for_retry(
    started: Instant,
    timeout: Duration,
    retry: Duration,
    timeout_message: &'static str,
) -> io::Result<()> {
    let elapsed = started.elapsed();
    let Some(remaining) = timeout.checked_sub(elapsed) else {
        return Err(io::Error::new(io::ErrorKind::TimedOut, timeout_message));
    };
    if remaining.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, timeout_message));
    }
    thread::sleep(retry.min(remaining));
    Ok(())
}

impl Drop for LocalLock {
    fn drop(&mut self) {
        match &self.state {
            LockState::Flock(file) => {
                let _ = flock(file, FlockOperation::Unlock);
            }
            LockState::Dotlock { parent, name } => {
                // This is the same compatibility tradeoff as stale removal:
                // unlink cannot require that the path still names our inode.
                let _ = unlinkat(parent, name.as_bytes(), AtFlags::empty());
            }
        }
    }
}

fn io_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_directory(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "procmail-rs-local-lock-{}-{unique}-{name}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn parses_lock_method_and_defaults_to_flock() {
        let default = crate::config::parse(":0\nmaildir:inbox\n")
            .unwrap()
            .expand()
            .unwrap();
        assert_eq!(
            LockMethod::from_config(&default).unwrap(),
            LockMethod::Flock
        );

        for (value, expected) in [
            ("flock", LockMethod::Flock),
            ("dotlock", LockMethod::Dotlock),
        ] {
            let config = crate::config::parse(&format!("LOCKMETHOD={value}\n"))
                .unwrap()
                .expand()
                .unwrap();
            assert_eq!(LockMethod::from_config(&config).unwrap(), expected);
        }

        let invalid = crate::config::parse("LOCKMETHOD=other\n")
            .unwrap()
            .expand()
            .unwrap();
        assert_eq!(
            LockMethod::from_config(&invalid).unwrap_err(),
            "line 1: LOCKMETHOD must be 'flock' or 'dotlock'"
        );
    }

    #[test]
    fn parses_lock_timeout_at_supported_boundaries() {
        assert_eq!(parse_lock_timeout("1").unwrap(), Duration::from_secs(1));
        assert_eq!(
            parse_lock_timeout(&crate::config::MAX_LOCK_TIMEOUT_SECONDS.to_string()).unwrap(),
            Duration::from_secs(crate::config::MAX_LOCK_TIMEOUT_SECONDS)
        );
        for value in ["", "0", "1s", "86401", "18446744073709551616"] {
            assert!(parse_lock_timeout(value).is_err(), "accepted {value:?}");
        }

        let default = crate::config::parse("").unwrap().expand().unwrap();
        assert_eq!(
            lock_timeout_from_config(&default).unwrap(),
            DEFAULT_LOCK_TIMEOUT
        );
        let repeated = crate::config::parse("LOCKTIMEOUT=1\nLOCKTIMEOUT=2\n")
            .unwrap()
            .expand()
            .unwrap();
        assert_eq!(
            lock_timeout_from_config(&repeated).unwrap(),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn flock_file_persists_and_serializes_holders() {
        let directory = temporary_directory("flock");
        let path = directory.join("recipe.lock");
        let uid = fs::metadata(&directory).unwrap().uid();
        let first = LocalLock::acquire_with_policy(
            &path,
            LockMethod::Flock,
            uid,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
        let error = LocalLock::acquire_with_policy(
            &path,
            LockMethod::Flock,
            uid,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(first);
        LocalLock::acquire_with_policy(
            &path,
            LockMethod::Flock,
            uid,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
        assert!(path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dotlock_is_visible_only_while_held() {
        let directory = temporary_directory("dotlock");
        let path = directory.join("recipe.lock");
        let uid = fs::metadata(&directory).unwrap().uid();
        let lock = LocalLock::acquire_with_policy(
            &path,
            LockMethod::Dotlock,
            uid,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap();
        assert!(path.is_file());
        let error = LocalLock::acquire_with_policy(
            &path,
            LockMethod::Dotlock,
            uid,
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(lock);
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn flock_rejects_symlinks_and_broad_permissions() {
        let directory = temporary_directory("metadata");
        let uid = fs::metadata(&directory).unwrap().uid();
        let target = directory.join("target");
        fs::write(&target, b"").unwrap();
        let link = directory.join("link");
        symlink(&target, &link).unwrap();
        assert!(LocalLock::acquire(&link, LockMethod::Flock, uid, DEFAULT_LOCK_TIMEOUT).is_err());

        let broad = directory.join("broad");
        fs::write(&broad, b"").unwrap();
        fs::set_permissions(&broad, fs::Permissions::from_mode(0o666)).unwrap();
        let error =
            LocalLock::acquire(&broad, LockMethod::Flock, uid, DEFAULT_LOCK_TIMEOUT).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn creation_mask_can_only_remove_lockfile_permissions() {
        let directory = temporary_directory("creation-mask");
        let uid = fs::metadata(&directory).unwrap().uid();
        for (method, name) in [
            (LockMethod::Flock, "flock"),
            (LockMethod::Dotlock, "dotlock"),
        ] {
            let path = directory.join(name);
            let lock =
                LocalLock::acquire_with_mask(&path, method, uid, DEFAULT_LOCK_TIMEOUT, 0o777)
                    .unwrap();
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0);
            drop(lock);
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
