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
        "COUNT=$#\nACCOUNT=$1\nMISSING=$10\n:0\n* COUNT ?? ^2$\n* ACCOUNT ?? ^account\\$name$\n* MISSING ?? ^$\nmaildir:${2}\n",
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

#[test]
fn shift_applies_in_order_across_a_selected_block_and_runtime_include() {
    let directory = temporary_directory();
    let destination = directory.join("selected");
    let root = directory.join("root.rc");
    let child = directory.join("child.rc");
    create_maildir(&destination);
    fs::write(
        &root,
        format!(
            "SHIFT=1\n:0\n{{\nSHIFT=1\n}}\nINCLUDERC={}\n",
            child.display()
        ),
    )
    .unwrap();
    fs::write(&child, "COUNT=$#\n:0\n* COUNT ?? ^1$\nmaildir:$1\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&root)
        .args(["-a", "ignored", "-a", "also-ignored", "-a"])
        .arg(&destination)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read_dir(destination.join("new")).unwrap().count(), 1);
}

#[test]
fn skipped_shift_block_does_not_change_the_parent_arguments() {
    let directory = temporary_directory();
    let destination = directory.join("selected");
    let root = directory.join("root.rc");
    create_maildir(&destination);
    fs::write(
        &root,
        format!(
            "MAILDIR={}\n:0\n* ^X-Never: present$\n{{\nSHIFT=1\n}}\n:0\nmaildir:$1\n",
            directory.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&root)
        .arg("-a")
        .arg("selected")
        .args(["-a", "ignored"])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read_dir(destination.join("new")).unwrap().count(), 1);
}

#[test]
fn shift_in_a_copy_branch_does_not_change_the_parent_window() {
    let directory = temporary_directory();
    let parent = directory.join("parent");
    let branch = directory.join("branch");
    let root = directory.join("root.rc");
    create_maildir(&parent);
    create_maildir(&branch);
    fs::write(
        &root,
        format!(
            "MAILDIR={}\n:0 c\n{{\nSHIFT=1\n:0\nmaildir:$1\n}}\n:0\nmaildir:$1\n",
            directory.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&root)
        .args(["-a", "parent", "-a", "branch"])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read_dir(parent.join("new")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(branch.join("new")).unwrap().count(), 1);
}

#[test]
fn excessive_shift_empties_the_window_and_is_traced() {
    let directory = temporary_directory();
    let root = directory.join("root.rc");
    fs::write(
        &root,
        "VERBOSE=yes\nLOGDETAIL=values\nSHIFT=999\nCOUNT=$#\nVALUE=$1\n:0\n* COUNT ?? ^0$\n* VALUE ?? ^$\n/dev/null\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&root)
        .args(["-a", "first", "-a", "second"])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Assigning at line 3 \"SHIFT=999\""),
        "{:?}",
        output.stderr
    );
}

#[test]
fn shift_from_runtime_include_changes_following_parent_statements() {
    let directory = temporary_directory();
    let destination = directory.join("selected");
    let root = directory.join("root.rc");
    let child = directory.join("child.rc");
    create_maildir(&destination);
    fs::write(&child, "SHIFT=1\n").unwrap();
    fs::write(
        &root,
        format!(
            "MAILDIR={}\nINCLUDERC={}\nCOUNT=$#\n:0\n* COUNT ?? ^1$\nmaildir:$1\n",
            directory.display(),
            child.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&root)
        .args(["-a", "ignored", "-a", "selected"])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read_dir(destination.join("new")).unwrap().count(), 1);
}

#[test]
fn command_output_can_select_the_shift_amount() {
    let directory = temporary_directory();
    let destination = directory.join("selected");
    let root = directory.join("root.rc");
    create_maildir(&destination);
    fs::write(
        &root,
        format!(
            "MAILDIR={}\nSHIFT=`printf 1`\n:0\nmaildir:$1\n",
            directory.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&root)
        .args(["-a", "ignored", "-a", "selected"])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read_dir(destination.join("new")).unwrap().count(), 1);
}
