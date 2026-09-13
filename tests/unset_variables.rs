// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_directory() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("procmail-rs-unset-{}-{unique}", std::process::id()));
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
fn unset_and_empty_values_remain_distinct_across_blocks_and_runtime_includes() {
    let directory = temporary_directory();
    let destination = directory.join("selected");
    let root = directory.join("root.rc");
    let included = directory.join("included.rc");
    create_maildir(&destination);
    fs::write(
        &included,
        "VALUE\nINCLUDED=${VALUE-fallback}\nEMPTY_DASH=${EMPTY-fallback}\nEMPTY_COLON=${EMPTY:-fallback}\n",
    )
    .unwrap();
    fs::write(
        &root,
        format!(
            "MAILDIR={}\nVERBOSE=yes\nVALUE=present\nEMPTY=\nINCLUDERC={}\n:0\n{{\nBLOCKED=present\nBLOCKED\n}}\nAFTER_BLOCK=${{BLOCKED-fallback}}\n:0\n* INCLUDED ?? ^fallback$\n* EMPTY_DASH ?? ^$\n* EMPTY_COLON ?? ^fallback$\n* AFTER_BLOCK ?? ^fallback$\nmaildir:{}/\n",
            directory.display(),
            included.display(),
            destination.display(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&root)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read_dir(destination.join("new")).unwrap().count(), 1);
    let trace = String::from_utf8_lossy(&output.stderr);
    assert!(trace.contains("Unsetting at line 1 \"VALUE\""), "{trace}");
    assert!(trace.contains("Unsetting at line 9 \"BLOCKED\""), "{trace}");
}

#[test]
fn unsetting_global_lockfile_releases_it_before_following_recipe() {
    let directory = temporary_directory();
    let config = directory.join("rules.rc");
    let lock = directory.join("global.lock");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKMETHOD=dotlock\nLOCKFILE={}\nLOCKFILE\n:0\n* ? test ! -e {}\n/dev/null\n",
            directory.display(),
            lock.display(),
            lock.display(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!lock.exists());
}
