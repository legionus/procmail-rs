// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_directory(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "procmail-rs-dry-run-{}-{unique}-{name}",
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    path
}

fn run_filter(config: &Path, message: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--dry-run", "--config"])
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(message).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn reports_runtime_reason_without_publishing_maildir() {
    let base = temporary_directory("reason");
    let config = base.join("rules.rc");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOGDETAIL=values\nLISTDIR=missing\n:0\n* ^List-Id:\n{{\n  :0 W\n  * ? test ! -e $LISTDIR\n  {{\n    LISTDIR=unknown\n  }}\n  :0\n  ${{LISTDIR}}/\n}}\n",
            base.display()
        ),
    )
    .unwrap();

    let output = run_filter(&config, b"List-Id: <example.test>\n\nbody\n");
    let trace = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0), "{trace}");
    assert!(
        trace.contains("procmail-rs: Executing at line 8 \"test ! -e $LISTDIR\""),
        "{trace}"
    );
    assert!(trace.contains("procmail-rs: Match on line 8 on \"test ! -e $LISTDIR\""));
    assert!(trace.contains("procmail-rs: Assigning at line 10 \"LISTDIR=unknown\""));
    assert!(trace.contains("procmail-rs: Would deliver to Maildir"));
    assert!(!base.join("unknown").exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn runs_filters_but_suppresses_delivery_commands_locks_and_trap() {
    let base = temporary_directory("effects");
    let config = base.join("rules.rc");
    let pipe_marker = base.join("pipe-ran");
    let trap_marker = base.join("trap-ran");
    let lock = base.join("global.lock");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKFILE={}\nTRAP='touch {}'\n:0 fw\n| sed 's/^Subject: old/Subject: new/'\n:0 c\n| touch {}\n:0\n* ^Subject: new\nselected/\n",
            base.display(),
            lock.display(),
            trap_marker.display(),
            pipe_marker.display()
        ),
    )
    .unwrap();

    let output = run_filter(&config, b"Subject: old\n\nbody\n");
    let trace = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0), "{trace}");
    assert!(trace.contains("procmail-rs: Match on line 9"), "{trace}");
    assert!(trace.contains("procmail-rs: Would deliver to Maildir"));
    assert!(!base.join("selected").exists());
    assert!(!pipe_marker.exists());
    assert!(!trap_marker.exists());
    assert!(!lock.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn rejects_dry_run_for_non_filter_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--dry-run"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(78));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("--dry-run may only be used with filter")
    );
}
