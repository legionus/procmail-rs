// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::*;

struct TestMaildir {
    path: PathBuf,
}

impl TestMaildir {
    fn create() -> Self {
        let base = std::env::temp_dir();
        for attempt in 0..128u64 {
            let path = base.join(format!("{}test.{attempt}", unique_name().unwrap()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::create_dir(path.join("tmp")).unwrap();
                    fs::create_dir(path.join("new")).unwrap();
                    fs::create_dir(path.join("cur")).unwrap();
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
fn generated_names_have_a_fixed_safe_shape() {
    let name = unique_name().unwrap();

    assert_eq!(name.len(), MAILDIR_NAME_LEN);
    assert!(name.starts_with(MAILDIR_NAME_PREFIX));
    assert!(
        name[MAILDIR_NAME_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
}

#[test]
fn random_source_distinguishes_generated_names() {
    let first = unique_name().unwrap();
    let second = unique_name().unwrap();

    assert_ne!(first, second);
    assert_eq!(first.len(), MAILDIR_NAME_LEN);
    assert_eq!(second.len(), MAILDIR_NAME_LEN);
}

#[test]
fn reads_explicit_durability_policy_in_statement_order() {
    for (value, expected) in [
        ("none", Durability::None),
        ("file", Durability::File),
        ("full", Durability::Full),
    ] {
        let config = crate::config::parse(&format!("DURABILITY={value}\n:0\nmaildir:box\n"))
            .unwrap()
            .expand()
            .unwrap();
        assert_eq!(Durability::from_config(&config).unwrap(), expected);
    }

    let config = crate::config::parse("DURABILITY=file\nDURABILITY=none\n:0\nmaildir:box\n")
        .unwrap()
        .expand()
        .unwrap();
    assert_eq!(Durability::from_config(&config).unwrap(), Durability::None);
}

#[test]
fn rejects_unknown_durability_before_delivery() {
    let config = crate::config::parse("DURABILITY=strong\n:0\nmaildir:box\n")
        .unwrap()
        .expand()
        .unwrap();

    assert!(Durability::from_config(&config).is_err());
}

#[test]
fn every_durability_mode_can_publish_a_complete_message() {
    for durability in [Durability::None, Durability::File, Durability::Full] {
        let maildir = TestMaildir::create();
        let mut sink =
            Box::new(MaildirSink::create_with_durability(maildir.path(), durability).unwrap());
        sink.write_all(b"Subject: sync\n\nbody").unwrap();

        let published = PendingSink::commit(sink).unwrap();
        assert_eq!(
            fs::read(published.last_folder()).unwrap(),
            b"Subject: sync\n\nbody"
        );
    }
}

#[test]
fn exclusive_creation_never_opens_an_existing_tmp_file() {
    let maildir = TestMaildir::create();
    let name = unique_name().unwrap();
    let path = maildir.path().join("tmp").join(&name);
    fs::write(&path, b"owned by another delivery").unwrap();
    let tmp_dir = open_directory_path(&maildir.path().join("tmp")).unwrap();

    let file = create_unnamed_pending_file(&tmp_dir, 0).unwrap();
    let error = link_unique_pending_file(&file, &tmp_dir, || Ok(name.clone())).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(path).unwrap(), b"owned by another delivery");
}

#[test]
fn collision_retry_can_succeed_on_the_last_attempt() {
    let maildir = TestMaildir::create();
    let tmp_path = maildir.path().join("tmp");
    let occupied = "procmail-rs.occupied";
    let available = "procmail-rs.available";
    fs::write(tmp_path.join(occupied), b"existing").unwrap();
    let tmp_dir = open_directory_path(&tmp_path).unwrap();
    let mut attempts = 0u64;

    let file = create_unnamed_pending_file(&tmp_dir, 0).unwrap();
    let name = link_unique_pending_file(&file, &tmp_dir, || {
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
    let mut attempts = 0u64;

    let file = create_unnamed_pending_file(&tmp_dir, 0).unwrap();
    let error = link_unique_pending_file(&file, &tmp_dir, || {
        attempts += 1;
        Ok(occupied.to_owned())
    })
    .unwrap_err();

    assert_eq!(attempts, MAX_NAME_ATTEMPTS);
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(tmp_path.join(occupied)).unwrap(), b"existing");
}

#[test]
fn commit_atomically_moves_the_complete_file_from_tmp_to_new() {
    let maildir = TestMaildir::create();
    let mut sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
    sink.write_all(b"Subject: test\n\nbody").unwrap();

    assert_eq!(fs::read_dir(maildir.path().join("tmp")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 0);

    let published = PendingSink::commit(sink).unwrap();
    assert!(
        published
            .last_folder()
            .starts_with(maildir.path().join("new"))
    );
    assert_eq!(fs::read_dir(maildir.path().join("tmp")).unwrap().count(), 0);
    assert_eq!(
        fs::read(published.last_folder()).unwrap(),
        b"Subject: test\n\nbody"
    );
}

#[test]
fn abort_closes_the_unnamed_file_without_removing_a_path() {
    let maildir = TestMaildir::create();
    let mut sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
    sink.write_all(b"partial").unwrap();

    PendingSink::abort(sink).unwrap();
    assert_eq!(fs::read_dir(maildir.path().join("tmp")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 0);
}

#[test]
fn injected_write_failure_never_creates_a_maildir_entry() {
    let maildir = TestMaildir::create();
    let mut sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
    sink.file = openat(
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

#[test]
fn injected_file_sync_failure_happens_before_maildir_publication() {
    let maildir = TestMaildir::create();
    let mut sink =
        Box::new(MaildirSink::create_with_durability(maildir.path(), Durability::File).unwrap());
    sink.write_all(b"complete message").unwrap();

    let error = (*sink)
        .commit_with(
            |_| Err(io::Error::other("injected file sync failure")),
            publish_linked_file,
        )
        .unwrap_err();

    assert!(error.published().is_none());
    assert_eq!(fs::read_dir(maildir.path().join("tmp")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 0);
}

#[test]
fn injected_rename_failure_does_not_publish_a_maildir_message() {
    let maildir = TestMaildir::create();
    let mut sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
    sink.write_all(b"complete message").unwrap();

    let error = (*sink)
        .commit_with(
            |_| Ok(()),
            |_, _, _| Err(io::Error::other("injected rename failure")),
        )
        .unwrap_err();

    assert!(error.published().is_none());
    assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(maildir.path().join("tmp")).unwrap().count(), 1);
}

#[test]
fn injected_directory_sync_failure_reports_visible_maildir_message() {
    let maildir = TestMaildir::create();
    let mut sink =
        Box::new(MaildirSink::create_with_durability(maildir.path(), Durability::Full).unwrap());
    sink.write_all(b"complete message").unwrap();
    let mut sync_calls = 0usize;

    let error = (*sink)
        .commit_with(
            |_| {
                sync_calls += 1;
                if sync_calls == 2 {
                    Err(io::Error::other("injected directory sync failure"))
                } else {
                    Ok(())
                }
            },
            publish_linked_file,
        )
        .unwrap_err();

    let published = error.published().unwrap();
    assert_eq!(
        fs::read(published.last_folder()).unwrap(),
        b"complete message"
    );
    assert_eq!(fs::read_dir(maildir.path().join("tmp")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 1);
}

#[test]
fn creates_a_file_without_group_or_other_access() {
    let maildir = TestMaildir::create();
    let sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
    let metadata = rustix::fs::fstat(&sink.file).unwrap();

    assert_eq!(metadata.st_mode & 0o777 & !MAILDIR_FILE_MODE, 0);
    PendingSink::abort(sink).unwrap();
}

#[test]
fn rejects_symlinked_maildir_component() {
    let maildir = TestMaildir::create();
    let link = maildir.path().with_extension("link");
    symlink(maildir.path(), &link).unwrap();

    let error = MaildirSink::create(&link).err().unwrap();
    let code = error.raw_os_error();
    assert!(
        code == Some(rustix::io::Errno::LOOP.raw_os_error())
            || code == Some(rustix::io::Errno::NOTDIR.raw_os_error())
    );
    fs::remove_file(link).unwrap();
}

#[test]
fn rejects_symlinked_tmp_directory() {
    let maildir = TestMaildir::create();
    fs::remove_dir(maildir.path().join("tmp")).unwrap();
    symlink(maildir.path().join("new"), maildir.path().join("tmp")).unwrap();

    assert!(MaildirSink::create(maildir.path()).is_err());
    assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 0);
}

#[test]
fn directory_replacement_cannot_redirect_an_open_delivery() {
    let maildir = TestMaildir::create();
    let moved = maildir.path().with_extension("opened");
    let mut sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
    sink.write_all(b"Subject: original directories\n\nbody")
        .unwrap();

    // Replace the configured pathname after the sink has opened every
    // directory. Commit must keep using those descriptors instead of
    // resolving the hostile path again and writing into the replacement.
    fs::rename(maildir.path(), &moved).unwrap();
    fs::create_dir(maildir.path()).unwrap();
    for component in ["tmp", "new", "cur"] {
        fs::create_dir(maildir.path().join(component)).unwrap();
    }

    PendingSink::commit(sink).unwrap();

    assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 0);
    let delivered = fs::read_dir(moved.join("new"))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        delivered,
        [b"Subject: original directories\n\nbody".to_vec()]
    );
    fs::remove_dir_all(moved).unwrap();
}

#[test]
fn requires_an_existing_complete_maildir() {
    for component in ["tmp", "new", "cur"] {
        let maildir = TestMaildir::create();
        fs::remove_dir(maildir.path().join(component)).unwrap();

        let error = MaildirSink::create(maildir.path()).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{component}");
        assert!(!maildir.path().join(component).exists());
    }
}

#[test]
fn commit_never_replaces_an_existing_new_file() {
    let maildir = TestMaildir::create();
    let tmp_dir = open_directory_path(&maildir.path().join("tmp")).unwrap();
    let new_dir = open_directory_path(&maildir.path().join("new")).unwrap();
    let file = create_unnamed_pending_file(&tmp_dir, 0).unwrap();
    let name = "procmail-rs.collision";
    linkat(&file, "", &tmp_dir, name, AtFlags::EMPTY_PATH).unwrap();
    fs::write(maildir.path().join("new").join(name), b"existing").unwrap();

    let error = publish_linked_file(&tmp_dir, &new_dir, name).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        fs::read(maildir.path().join("new").join(name)).unwrap(),
        b"existing"
    );
    assert!(maildir.path().join("tmp").join(name).exists());
}

#[test]
fn concurrent_deliveries_publish_unique_complete_messages() {
    const DELIVERIES: usize = 32;

    let maildir = TestMaildir::create();
    let mut threads = Vec::with_capacity(DELIVERIES);
    for index in 0..DELIVERIES {
        let path = maildir.path().to_owned();
        threads.push(std::thread::spawn(move || {
            let message = format!("Subject: {index}\n\nbody {index}").into_bytes();
            let mut sink = Box::new(MaildirSink::create(&path).unwrap());
            sink.write_all(&message).unwrap();
            PendingSink::commit(sink).unwrap();
            message
        }));
    }

    let mut expected: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    let mut delivered: Vec<_> = fs::read_dir(maildir.path().join("new"))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect();
    expected.sort();
    delivered.sort();
    assert_eq!(delivered, expected);
    assert_eq!(fs::read_dir(maildir.path().join("tmp")).unwrap().count(), 0);
}

#[test]
fn rejects_parent_components() {
    let error = MaildirSink::create(Path::new("mail/../dir")).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}
