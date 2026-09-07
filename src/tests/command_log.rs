// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use procmail_rs::runtime::RuntimeVariables;

use super::CommandLog;

fn test_directory() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "procmail-rs-command-log-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    path
}

#[test]
fn diagnostics_follow_runtime_logfile_changes_and_append() {
    let directory = test_directory();
    let first = directory.join("first.log");
    let second = directory.join("second.log");
    let mut runtime = RuntimeVariables::default();

    runtime.set("LOGFILE", first.to_string_lossy().into_owned());
    CommandLog::new(&runtime)
        .write_diagnostic(b"one\n")
        .unwrap();
    CommandLog::new(&runtime)
        .write_diagnostic(b"two\n")
        .unwrap();
    runtime.set("LOGFILE", second.to_string_lossy().into_owned());
    CommandLog::new(&runtime)
        .write_diagnostic(b"three\n")
        .unwrap();

    assert_eq!(fs::read(&first).unwrap(), b"one\ntwo\n");
    assert_eq!(fs::read(&second).unwrap(), b"three\n");
    fs::remove_dir_all(directory).unwrap();
}
