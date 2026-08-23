// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const WORKERS: usize = 12;
const MESSAGES_PER_WORKER: usize = 64;
const MESSAGE_COUNT: usize = WORKERS * MESSAGES_PER_WORKER;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn create(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "procmail-rs-stress-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn message(id: usize) -> Vec<u8> {
    format!("X-Stress-ID: {id:08x}\n\nstress-body-{id:08x}\n").into_bytes()
}

fn deliver_concurrently(config: &Path) {
    let barrier = Arc::new(Barrier::new(WORKERS));
    let config = Arc::new(config.to_owned());
    let mut workers = Vec::with_capacity(WORKERS);

    // Separate worker threads launch independent filter processes after one
    // common barrier. This keeps several deliveries in the filesystem backend
    // at once and exercises the kernel-visible publication and locking paths;
    // calling the library repeatedly in one process would miss those races.
    for worker in 0..WORKERS {
        let barrier = Arc::clone(&barrier);
        let config = Arc::clone(&config);
        workers.push(thread::spawn(move || {
            barrier.wait();
            for sequence in 0..MESSAGES_PER_WORKER {
                let id = worker * MESSAGES_PER_WORKER + sequence;
                let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
                    .args(["filter", "--config"])
                    .arg(config.as_path())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child.stdin.take().unwrap().write_all(&message(id)).unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "delivery {id} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }));
    }

    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
#[ignore = "long-running concurrent delivery stress test"]
fn concurrent_maildir_delivery_loses_or_duplicates_no_messages() {
    let directory = TestDirectory::create("maildir");
    let maildir = directory.path().join("maildir");
    for component in ["", "tmp", "new", "cur"] {
        fs::create_dir(maildir.join(component)).unwrap();
    }
    let config = directory.path().join("rules.rc");
    fs::write(&config, format!(":0\nmaildir:{}\n", maildir.display())).unwrap();

    deliver_concurrently(&config);

    assert_eq!(fs::read_dir(maildir.join("tmp")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(maildir.join("cur")).unwrap().count(), 0);
    let delivered: HashSet<Vec<u8>> = fs::read_dir(maildir.join("new"))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect();
    let expected: HashSet<Vec<u8>> = (0..MESSAGE_COUNT).map(message).collect();
    assert_eq!(delivered, expected);
}

#[test]
#[ignore = "long-running concurrent delivery stress test"]
fn concurrent_mbox_delivery_keeps_every_record_contiguous() {
    let directory = TestDirectory::create("mbox");
    let mailbox = directory.path().join("mailbox");
    let config = directory.path().join("rules.rc");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\n:0\nmbox:{}\n",
            directory.path().display(),
            mailbox.display()
        ),
    )
    .unwrap();

    deliver_concurrently(&config);

    let stored = fs::read(&mailbox).unwrap();
    let postmark = b"From MAILER-DAEMON ";
    assert_eq!(
        stored
            .windows(postmark.len())
            .filter(|window| *window == postmark)
            .count(),
        MESSAGE_COUNT
    );

    // Every expected payload must occur exactly once as one uninterrupted
    // mbox record body. Checking the complete payload plus its separator catches
    // interleaved or partial appends that marker-only counting would overlook.
    for id in 0..MESSAGE_COUNT {
        let mut record_body = message(id);
        record_body.push(b'\n');
        assert_eq!(
            stored
                .windows(record_body.len())
                .filter(|window| *window == record_body)
                .count(),
            1,
            "missing, duplicated, or mixed mbox record {id}"
        );
    }
}
