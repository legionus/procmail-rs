// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{sync::Arc, sync::Barrier, thread};

use crate::config::OutputEnding;

use super::{
    MAX_POSTMARK_LEN, MboxFile, Postmark, PostmarkError, write_record, write_record_with_ending,
};
use crate::delivery::maildir::Durability;

fn temporary_path(name: &str) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "procmail-rs-mbox-{}-{unique}-{name}",
        std::process::id()
    ))
}

fn postmark() -> Postmark {
    Postmark::new(b"From MAILER-DAEMON Thu Jan  1 00:00:00 1970\n").unwrap()
}

#[test]
fn quotes_every_mboxrd_line_reversibly() {
    let message =
        b"From hostile header\nX: value\n\nFrom body\n>From quoted\n>>From twice\nnot From safe\n";
    let mut output = Vec::new();

    write_record(&mut output, &postmark(), message).unwrap();

    assert_eq!(
            output,
            b"From MAILER-DAEMON Thu Jan  1 00:00:00 1970\n>From hostile header\nX: value\n\n>From body\n>>From quoted\n>>>From twice\nnot From safe\n\n"
        );
}

#[test]
fn adds_record_separator_without_changing_input() {
    for (message, suffix) in [
        (&b"body"[..], &b"body\n\n"[..]),
        (&b"body\n"[..], &b"body\n\n"[..]),
        (&b"body\n\n"[..], &b"body\n\n\n"[..]),
        (&b""[..], &b"\n"[..]),
    ] {
        let original = message.to_vec();
        let mut output = Vec::new();
        write_record(&mut output, &postmark(), message).unwrap();
        assert!(output.ends_with(suffix));
        assert_eq!(message, original);
    }
}

#[test]
fn raw_mode_adds_only_the_line_ending_required_for_mbox_framing() {
    for (message, suffix) in [
        (&b"body"[..], &b"body\n"[..]),
        (&b"body\n"[..], &b"body\n"[..]),
        (&b"body\n\n"[..], &b"body\n\n"[..]),
        (&b""[..], &b"1970\n"[..]),
    ] {
        let mut output = Vec::new();
        write_record_with_ending(&mut output, &postmark(), message, OutputEnding::Preserve)
            .unwrap();
        assert!(output.ends_with(suffix), "{output:?}");
    }
}

#[test]
fn validates_postmark_shape_and_limit() {
    assert_eq!(
        Postmark::new(b"not From\n"),
        Err(PostmarkError::InvalidShape)
    );
    assert_eq!(
        Postmark::new(b"From first\nsecond\n"),
        Err(PostmarkError::InvalidShape)
    );
    assert_eq!(
        Postmark::new(b"From non-ascii \xff\n"),
        Err(PostmarkError::NonAscii)
    );

    let mut boundary = b"From ".to_vec();
    boundary.resize(MAX_POSTMARK_LEN - 1, b'x');
    boundary.push(b'\n');
    assert!(Postmark::new(&boundary).is_ok());
    boundary.insert(MAX_POSTMARK_LEN - 1, b'x');
    assert_eq!(Postmark::new(&boundary), Err(PostmarkError::TooLong));
}

#[test]
fn generates_stable_utc_postmark() {
    assert_eq!(
        Postmark::generated(UNIX_EPOCH).unwrap().as_bytes(),
        b"From MAILER-DAEMON Thu Jan  1 00:00:00 1970\n"
    );
    assert_eq!(
        Postmark::generated(UNIX_EPOCH + Duration::from_secs(951_827_696))
            .unwrap()
            .as_bytes(),
        b"From MAILER-DAEMON Tue Feb 29 12:34:56 2000\n"
    );
}

#[test]
fn opens_regular_mailbox_without_following_symlinks_or_hard_links() {
    let directory = temporary_path("open");
    fs::create_dir(&directory).unwrap();
    let mailbox = directory.join("mailbox");
    let _opened = MboxFile::open(&mailbox).unwrap();
    assert_eq!(
        fs::metadata(&mailbox).unwrap().permissions().mode() & 0o077,
        0
    );

    let symlink = directory.join("symlink");
    std::os::unix::fs::symlink(&mailbox, &symlink).unwrap();
    assert!(MboxFile::open(&symlink).is_err());

    let hardlink = directory.join("hardlink");
    fs::hard_link(&mailbox, &hardlink).unwrap();
    assert!(MboxFile::open(&mailbox).is_err());

    let inaccessible = directory.join("inaccessible");
    fs::write(&inaccessible, b"").unwrap();
    fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(
        MboxFile::open(&inaccessible).err().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    drop(_opened);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn lock_timeout_is_finite() {
    let directory = temporary_path("lock");
    fs::create_dir(&directory).unwrap();
    let mailbox = directory.join("mailbox");
    let first = MboxFile::open(&mailbox).unwrap().lock().unwrap();
    let second = MboxFile::open(&mailbox).unwrap();

    let error = second
        .lock_with_policy(Duration::ZERO, Duration::ZERO)
        .err()
        .unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);

    first.unlock().unwrap();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn append_publishes_complete_record_and_reports_path() {
    let directory = temporary_path("append");
    fs::create_dir(&directory).unwrap();
    let mailbox = directory.join("mailbox");
    let published = MboxFile::open(&mailbox)
        .unwrap()
        .lock()
        .unwrap()
        .append(
            b"Subject: test\n\nFrom body",
            OutputEnding::Normalize,
            Durability::File,
        )
        .unwrap();

    assert_eq!(published.last_folder(), mailbox);
    let bytes = fs::read(&mailbox).unwrap();
    assert!(bytes.starts_with(b"From MAILER-DAEMON "));
    assert!(bytes.ends_with(b"Subject: test\n\n>From body\n\n"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn append_failure_restores_original_length() {
    let directory = temporary_path("rollback");
    fs::create_dir(&directory).unwrap();
    let mailbox = directory.join("mailbox");
    fs::write(&mailbox, b"existing").unwrap();
    let locked = MboxFile::open(&mailbox).unwrap().lock().unwrap();

    let error = locked
        .append_with(Durability::None, |file| {
            rustix::io::write(file, b"partial").map_err(super::io_error)?;
            Err(io::Error::from_raw_os_error(28))
        })
        .unwrap_err();

    assert!(!error.published());
    assert!(!error.rollback_failed());
    let mut bytes = Vec::new();
    fs::File::open(&mailbox)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"existing");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn durability_failure_is_rolled_back_and_not_published() {
    let directory = temporary_path("sync-failure");
    fs::create_dir(&directory).unwrap();
    let mailbox = directory.join("mailbox");
    fs::write(&mailbox, b"existing").unwrap();
    let locked = MboxFile::open(&mailbox).unwrap().lock().unwrap();

    let mut sync_calls = 0usize;
    let error = locked
        .append_with_sync(
            Durability::File,
            |file| {
                rustix::io::write(file, b"complete-record").map_err(super::io_error)?;
                Ok(())
            },
            |_| {
                sync_calls += 1;
                if sync_calls == 1 {
                    Err(io::Error::other("injected sync failure"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

    assert!(!error.published());
    assert!(!error.rollback_failed());
    assert_eq!(fs::read(&mailbox).unwrap(), b"existing");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn truncate_failure_reports_failed_rollback_and_preserves_partial_bytes() {
    let directory = temporary_path("truncate-failure");
    fs::create_dir(&directory).unwrap();
    let mailbox = directory.join("mailbox");
    fs::write(&mailbox, b"existing").unwrap();
    let locked = MboxFile::open(&mailbox).unwrap().lock().unwrap();

    let error = locked
        .append_with_operations(
            Durability::None,
            |file| {
                rustix::io::write(file, b"partial").map_err(super::io_error)?;
                Err(io::Error::other("injected write failure"))
            },
            |_| Ok(()),
            |_, _| Err(io::Error::other("injected truncate failure")),
        )
        .unwrap_err();

    assert!(!error.published());
    assert!(error.rollback_failed());
    assert_eq!(fs::read(&mailbox).unwrap(), b"existingpartial");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn concurrent_writers_append_intact_records() {
    const WRITERS: usize = 16;

    let directory = temporary_path("concurrent");
    fs::create_dir(&directory).unwrap();
    let mailbox = directory.join("mailbox");
    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut workers = Vec::new();
    for index in 0..WRITERS {
        let mailbox = mailbox.clone();
        let barrier = barrier.clone();
        workers.push(thread::spawn(move || {
            let message = format!("Subject: writer-{index:02}\n\nbody-{index:02}\n");
            barrier.wait();
            MboxFile::open(&mailbox)
                .unwrap()
                .lock()
                .unwrap()
                .append(
                    message.as_bytes(),
                    OutputEnding::Normalize,
                    Durability::None,
                )
                .unwrap();
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }

    let bytes = fs::read(&mailbox).unwrap();
    assert_eq!(
        bytes
            .windows(b"From MAILER-DAEMON ".len())
            .filter(|window| *window == b"From MAILER-DAEMON ")
            .count(),
        WRITERS
    );
    for index in 0..WRITERS {
        let marker = format!("Subject: writer-{index:02}\n\nbody-{index:02}\n\n");
        assert_eq!(
            bytes
                .windows(marker.len())
                .filter(|window| *window == marker.as_bytes())
                .count(),
            1
        );
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn malformed_existing_bytes_are_not_parsed_or_rewritten() {
    let directory = temporary_path("malformed");
    fs::create_dir(&directory).unwrap();
    let mailbox = directory.join("mailbox");
    let original = b"not an mbox\x00\xffwithout newline";
    fs::write(&mailbox, original).unwrap();

    MboxFile::open(&mailbox)
        .unwrap()
        .lock()
        .unwrap()
        .append(
            b"Subject: appended\n\nbody",
            OutputEnding::Normalize,
            Durability::None,
        )
        .unwrap();

    let bytes = fs::read(&mailbox).unwrap();
    assert!(bytes.starts_with(original));
    assert!(bytes.ends_with(b"Subject: appended\n\nbody\n\n"));
    fs::remove_dir_all(directory).unwrap();
}
