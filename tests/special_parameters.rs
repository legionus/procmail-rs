// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temporary_directory() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "procmail-rs-special-{}-{unique}-{}",
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path
}

fn run_filter(config: &Path, message: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(message).unwrap();
    child.wait_with_output().unwrap()
}

fn create_maildir(path: &Path) {
    fs::create_dir(path).unwrap();
    for component in ["tmp", "new", "cur"] {
        fs::create_dir(path.join(component)).unwrap();
    }
}

#[test]
fn current_rc_name_tracks_include_and_switch_boundaries() {
    let directory = temporary_directory();
    let root = directory.join("root.rc");
    let include = directory.join("include.rc");
    let switched = directory.join("switched.rc");
    let staging = directory.join("staging");
    create_maildir(&staging);
    fs::write(
        &root,
        format!(
            "MAILDIR={}\nROOT=$_\nINCLUDERC={}\nAFTER=$_\nSWITCHRC={}\n",
            staging.display(),
            include.display(),
            switched.display()
        ),
    )
    .unwrap();
    fs::write(&include, "INCLUDED=$_\n").unwrap();
    fs::write(
        &switched,
        format!(
            "CURRENT=$_\n:0\n* ? test \"$ROOT\" = '{}'\n* ? test \"$INCLUDED\" = '{}'\n* ? test \"$AFTER\" = '{}'\n* ? test \"$CURRENT\" = '{}'\n/dev/null\n",
            root.display(),
            include.display(),
            root.display(),
            switched.display()
        ),
    )
    .unwrap();

    let output = run_filter(&root, b"Subject: rc name\n\nbody\n");

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
}

#[test]
fn command_status_pid_and_lastfolder_are_available_in_statement_order() {
    let directory = temporary_directory();
    let root = directory.join("root.rc");
    let first = directory.join("first.mbox");
    let staging = directory.join("staging");
    create_maildir(&staging);
    fs::write(
        &root,
        format!(
            "MAILDIR={}\nINITIAL=$?\nPID=$$\nCAPTURE=`exit 9`\nCAPTURE_STATUS=$?\n:0 c\nmbox:{}\nFOLDER=$-\n:0\n* ? test \"$INITIAL\" = 0\n* ? test \"$PID\" = \"$PPID\"\n* ? test \"$CAPTURE_STATUS\" = 9\n* ? test \"$FOLDER\" = '{}'\n/dev/null\n",
            staging.display(),
            first.display(),
            first.display()
        ),
    )
    .unwrap();

    let output = run_filter(&root, b"Subject: special values\n\nbody\n");

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(first.exists());
}

#[test]
fn command_status_tracks_conditions_captures_and_pipe_actions() {
    let directory = temporary_directory();
    let root = directory.join("root.rc");
    let staging = directory.join("staging");
    create_maildir(&staging);
    fs::write(
        &root,
        format!(
            "MAILDIR={}\n:0\n* ? exit 7\n/dev/null\nCONDITION_STATUS=$?\nCAPTURE=`exit 9`\nCAPTURE_STATUS=$?\n:0 w\n| exit 6\nPIPE_STATUS=$?\n:0 e\n* CONDITION_STATUS ?? ^7$\n* CAPTURE_STATUS ?? ^9$\n* PIPE_STATUS ?? ^6$\n/dev/null\n",
            staging.display()
        ),
    )
    .unwrap();

    let output = run_filter(&root, b"Subject: command status\n\nbody\n");

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
}

#[test]
fn command_status_tracks_a_failed_waited_filter() {
    let directory = temporary_directory();
    let root = directory.join("root.rc");
    let staging = directory.join("staging");
    create_maildir(&staging);
    fs::write(
        &root,
        format!(
            "MAILDIR={}\n:0 fw\n| cat; exit 5\nFILTER_STATUS=$?\n:0 e\n* FILTER_STATUS ?? ^5$\n/dev/null\n",
            staging.display()
        ),
    )
    .unwrap();

    let output = run_filter(&root, b"Subject: filter status\n\nbody\n");

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
}
