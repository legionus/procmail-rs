// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

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
fn injected_flock_error_is_returned_without_retrying() {
    let directory = temporary_directory("injected-flock");
    let parent = open_directory_path(&directory).unwrap();
    let file = openat(
        &parent,
        "lock",
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC,
        Mode::from_raw_mode(LOCK_FILE_MODE),
    )
    .unwrap();
    let mut attempts = 0usize;

    let error = acquire_flock_fd_with(
        &file,
        Duration::ZERO,
        Duration::ZERO,
        "injected timeout",
        |_| {
            attempts += 1;
            Err(rustix::io::Errno::IO)
        },
    )
    .unwrap_err();

    assert_eq!(attempts, 1);
    assert_eq!(
        error.raw_os_error(),
        Some(rustix::io::Errno::IO.raw_os_error())
    );
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
            LocalLock::acquire_with_mask(&path, method, uid, DEFAULT_LOCK_TIMEOUT, 0o777).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0);
        drop(lock);
    }
    fs::remove_dir_all(directory).unwrap();
}
