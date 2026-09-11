// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_directory() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "procmail-rs-structured-condition-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    path
}

fn create_maildir(path: &Path) {
    fs::create_dir(path).unwrap();
    for component in ["tmp", "new", "cur"] {
        fs::create_dir(path.join(component)).unwrap();
    }
}

fn delivered_messages(path: &Path) -> Vec<Vec<u8>> {
    fs::read_dir(path.join("new"))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect()
}

#[test]
fn address_condition_observes_headers_added_by_an_earlier_recipe() {
    let base = temporary_directory();
    let destinations = base.join("dest");
    fs::create_dir(&destinations).unwrap();
    let bar = destinations.join("bar");
    let baz = destinations.join("baz");
    create_maildir(&bar);
    create_maildir(&baz);

    let config = base.join("rules.rc");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\n\
:0\n\
* address Cc ?? bar@.*\n\
dest/bar/\n\
\n\
:0\n\
headers {{\n\
    add Cc bar@example.com\n\
}}\n\
\n\
:0\n\
* address Cc ?? bar@.*\n\
dest/baz/\n",
            base.display()
        ),
    )
    .unwrap();

    let message = b"From: foo@example.com\n\
To: me@example.com\n\
Cc: copy@example.com\n\
Subject: test\n\
\n\
Text\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(message).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(delivered_messages(&bar).is_empty());
    let delivered = delivered_messages(&baz);
    assert_eq!(delivered.len(), 1);
    assert!(
        delivered[0]
            .windows(b"Cc: bar@example.com\n".len())
            .any(|window| window == b"Cc: bar@example.com\n")
    );

    fs::remove_dir_all(base).unwrap();
}
