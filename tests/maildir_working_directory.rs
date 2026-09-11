// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn create() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        for _ in 0..128 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "procmail-rs-maildir-cwd-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("cannot create temporary directory: {error}"),
            }
        }
        panic!("cannot allocate a temporary directory");
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn create_maildir(path: &Path) {
    fs::create_dir(path).unwrap();
    for child in ["tmp", "new", "cur"] {
        fs::create_dir(path.join(child)).unwrap();
    }
}

#[test]
fn external_condition_uses_the_active_maildir_as_its_working_directory() {
    let temporary = TempDirectory::create();
    let mail_root = temporary.0.join("mail-root");
    fs::create_dir(&mail_root).unwrap();
    fs::write(mail_root.join("relative-marker"), b"").unwrap();
    create_maildir(&mail_root.join("selected"));
    create_maildir(&mail_root.join("fallback"));

    let config = temporary.0.join("rc");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\n:0 W\n* ? test -e relative-marker\nselected/\n:0\nfallback/\n",
            mail_root.display()
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .current_dir(&temporary.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        child.stdin.as_mut().unwrap(),
        b"Subject: MAILDIR cwd\n\nbody\n",
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(
        fs::read_dir(mail_root.join("selected/new"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        fs::read_dir(mail_root.join("fallback/new"))
            .unwrap()
            .count(),
        0
    );
}
