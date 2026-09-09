// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags, openat};

use super::*;
use crate::delivery::PendingSink;
use crate::delivery::maildir::{Durability, MaildirSink, open_directory_path, unique_name};

struct TestMaildir {
    path: PathBuf,
}

impl TestMaildir {
    fn create() -> Self {
        let base = std::env::temp_dir();
        for attempt in 0..MAX_NAME_ATTEMPTS {
            let path = base.join(format!("{}linux-test.{attempt}", unique_name().unwrap()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    for component in ["tmp", "new", "cur"] {
                        fs::create_dir(path.join(component)).unwrap();
                    }
                    return Self { path };
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test Maildir: {error}"),
            }
        }
        panic!("cannot allocate test Maildir name");
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestMaildir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

#[test]
fn collision_retry_can_succeed_on_the_last_attempt() {
    let maildir = TestMaildir::create();
    let tmp_path = maildir.path().join("tmp");
    let occupied = "procmail-rs.occupied";
    let available = "procmail-rs.available";
    fs::write(tmp_path.join(occupied), b"existing").unwrap();
    let tmp_dir = open_directory_path(&tmp_path).unwrap();
    let pending = PendingFile::create(&tmp_dir, 0, unique_name).unwrap();
    let mut attempts = 0u64;

    let name = link_unique(pending.file(), &tmp_dir, &mut || {
        attempts += 1;
        Ok(if attempts < MAX_NAME_ATTEMPTS {
            occupied.to_owned()
        } else {
            available.to_owned()
        })
    })
    .unwrap();

    assert_eq!(attempts, MAX_NAME_ATTEMPTS);
    assert_eq!(name, available);
    fs::remove_file(tmp_path.join(available)).unwrap();
}

#[test]
fn collision_retry_stops_after_its_fixed_limit() {
    let maildir = TestMaildir::create();
    let tmp_path = maildir.path().join("tmp");
    let occupied = "procmail-rs.occupied";
    fs::write(tmp_path.join(occupied), b"existing").unwrap();
    let tmp_dir = open_directory_path(&tmp_path).unwrap();
    let pending = PendingFile::create(&tmp_dir, 0, unique_name).unwrap();
    let mut attempts = 0u64;

    let error = link_unique(pending.file(), &tmp_dir, &mut || {
        attempts += 1;
        Ok(occupied.to_owned())
    })
    .unwrap_err();

    assert_eq!(attempts, MAX_NAME_ATTEMPTS);
    assert_eq!(error.into_parts().0.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(tmp_path.join(occupied)).unwrap(), b"existing");
}

#[test]
fn publication_never_replaces_an_existing_new_file() {
    let maildir = TestMaildir::create();
    let tmp_dir = open_directory_path(&maildir.path().join("tmp")).unwrap();
    let new_dir = open_directory_path(&maildir.path().join("new")).unwrap();
    let pending = PendingFile::create(&tmp_dir, 0, unique_name).unwrap();
    let name = "procmail-rs.collision";
    fs::write(maildir.path().join("new").join(name), b"existing").unwrap();

    let error = pending
        .publish(&tmp_dir, &new_dir, || Ok(name.to_owned()))
        .unwrap_err();

    assert_eq!(error.into_parts().0.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        fs::read(maildir.path().join("new").join(name)).unwrap(),
        b"existing"
    );
    assert!(maildir.path().join("tmp").join(name).exists());
}

#[test]
fn write_failure_never_creates_a_maildir_entry() {
    let maildir = TestMaildir::create();
    let mut sink = Box::new(MaildirSink::create(maildir.path(), Durability::None, 0).unwrap());
    sink.pending.file = openat(
        CWD,
        "/dev/full",
        OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();

    let error = sink.write_all(b"message").unwrap_err();
    assert_eq!(
        error.raw_os_error(),
        Some(rustix::io::Errno::NOSPC.raw_os_error())
    );
    PendingSink::abort(sink).unwrap();
    assert_eq!(fs::read_dir(maildir.path().join("tmp")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 0);
}
