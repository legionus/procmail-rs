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
        "procmail-rs-arguments-{}-{unique}",
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

#[test]
fn positional_arguments_reach_runtime_includes_and_select_a_destination() {
    let directory = temporary_directory();
    let destination = directory.join("selected");
    let root = directory.join("root.rc");
    let child = directory.join("child.rc");
    create_maildir(&destination);
    fs::write(&root, format!("INCLUDERC={}\n", child.display())).unwrap();
    fs::write(
        &child,
        "COUNT=$#\nACCOUNT=$1\n:0\n* COUNT ?? ^2$\n* ACCOUNT ?? ^account\\$name$\nmaildir:${2}\n",
    )
    .unwrap();

    let mut process = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&root)
        .args(["-a", "account$name", "--argument"])
        .arg(&destination)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(b"Subject: positional arguments\n\nbody\n")
        .unwrap();
    let output = process.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read_dir(destination.join("new")).unwrap().count(), 1);
}

#[test]
fn detailed_trace_does_not_reveal_positional_argument_values() {
    let directory = temporary_directory();
    let destination = directory.join("selected");
    let config = directory.join("rules.rc");
    create_maildir(&destination);
    fs::write(
        &config,
        format!(
            "VERBOSE=yes\nLOGDETAIL=values\n:0\nmaildir:{}\n",
            destination.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .args(["-a", "argument-secret-sentinel"])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("argument-secret-sentinel"));
}
