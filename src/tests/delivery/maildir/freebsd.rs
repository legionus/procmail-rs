// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::delivery::maildir::{MaildirSink, open_directory_path, unique_name};

struct TestMaildir {
    path: PathBuf,
}

impl TestMaildir {
    fn create() -> Self {
        let base = std::env::temp_dir();
        for attempt in 0..MAX_NAME_ATTEMPTS {
            let path = base.join(format!("{}freebsd-test.{attempt}", unique_name().unwrap()));
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
fn named_pending_file_retries_collisions_and_abort_removes_it() {
    let maildir = TestMaildir::create();
    let tmp_path = maildir.path().join("tmp");
    let occupied = "procmail-rs.occupied";
    let available = "procmail-rs.available";
    fs::write(tmp_path.join(occupied), b"existing").unwrap();
    let tmp_dir = open_directory_path(&tmp_path).unwrap();
    let mut attempts = 0u64;

    let pending = PendingFile::create(&tmp_dir, 0, || {
        attempts += 1;
        Ok(if attempts == 1 {
            occupied.to_owned()
        } else {
            available.to_owned()
        })
    })
    .unwrap();

    assert_eq!(attempts, 2);
    assert!(tmp_path.join(available).is_file());
    pending.abort(&tmp_dir).unwrap();
    assert!(!tmp_path.join(available).exists());
    assert_eq!(fs::read(tmp_path.join(occupied)).unwrap(), b"existing");
}

#[test]
fn replacement_is_detected_without_deleting_the_new_entry() {
    let maildir = TestMaildir::create();
    let tmp_path = maildir.path().join("tmp");
    let tmp_dir = open_directory_path(&tmp_path).unwrap();
    let name = "procmail-rs.replaced";
    let pending = PendingFile::create(&tmp_dir, 0, || Ok(name.to_owned())).unwrap();
    fs::remove_file(tmp_path.join(name)).unwrap();
    fs::write(tmp_path.join(name), b"replacement").unwrap();

    let error = pending.abort(&tmp_dir).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(fs::read(tmp_path.join(name)).unwrap(), b"replacement");
}

#[test]
fn publication_does_not_replace_an_existing_new_file() {
    let maildir = TestMaildir::create();
    let tmp_dir = open_directory_path(&maildir.path().join("tmp")).unwrap();
    let new_dir = open_directory_path(&maildir.path().join("new")).unwrap();
    let name = "procmail-rs.collision";
    let pending = PendingFile::create(&tmp_dir, 0, || Ok(name.to_owned())).unwrap();
    fs::write(maildir.path().join("new").join(name), b"existing").unwrap();

    let error = pending
        .publish(&tmp_dir, &new_dir, unique_name)
        .unwrap_err();

    assert_eq!(error.into_parts().0.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        fs::read(maildir.path().join("new").join(name)).unwrap(),
        b"existing"
    );
}

#[test]
fn writable_maildir_component_is_rejected() {
    let maildir = TestMaildir::create();
    fs::set_permissions(
        maildir.path().join("tmp"),
        fs::Permissions::from_mode(0o770),
    )
    .unwrap();

    let error = MaildirSink::create(maildir.path(), super::super::Durability::None, 0)
        .err()
        .unwrap();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}
