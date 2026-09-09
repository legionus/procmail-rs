// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn temporary_case(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "procmail-rs-copy-block-{}-{unique}-{name}",
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

fn delivered_count(path: &Path) -> usize {
    fs::read_dir(path.join("new")).unwrap().count()
}

fn write_private(path: &Path, contents: impl AsRef<[u8]>) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn run_filter(config: &Path) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Subject: copy branch\n\nbody\n")
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn host_stops_only_the_waited_copy_branch() {
    let base = temporary_case("host");
    let selected = base.join("selected");
    let unreachable = base.join("unreachable");
    create_maildir(&selected);
    create_maildir(&unreachable);
    let config = base.join("rules.rc");
    write_private(
        &config,
        format!(
            "MAILDIR={}\n:0 cw\n{{\nHOST=${{HOST}}-copy\n:0 c\nmaildir:unreachable\n}}\n:0\nmaildir:selected\n",
            base.display()
        ),
    );

    let output = run_filter(&config);

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_count(&selected), 1);
    assert_eq!(delivered_count(&unreachable), 0);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn runtime_include_changes_only_the_waited_copy_branch() {
    let base = temporary_case("include");
    let parent = base.join("parent");
    let copy = base.join("copy");
    create_maildir(&parent);
    create_maildir(&copy);
    let included = base.join("included.rc");
    write_private(&included, "TARGET=copy\n");
    let config = base.join("rules.rc");
    write_private(
        &config,
        format!(
            "MAILDIR={}\nTARGET=parent\n:0 cw\n{{\nINCLUDERC=included.rc\n}}\n:0\nmaildir:$TARGET\n",
            base.display()
        ),
    );

    let output = run_filter(&config);

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_count(&copy), 1);
    assert_eq!(delivered_count(&parent), 1);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn runtime_switch_replaces_only_the_waited_copy_branch() {
    let base = temporary_case("switch");
    let parent = base.join("parent");
    let switched = base.join("switched");
    create_maildir(&parent);
    create_maildir(&switched);
    let switched_rc = base.join("switched.rc");
    write_private(&switched_rc, ":0\nmaildir:switched\n");
    let config = base.join("rules.rc");
    write_private(
        &config,
        format!(
            "MAILDIR={}\n:0 cw\n{{\nSWITCHRC=switched.rc\n}}\n:0\nmaildir:parent\n",
            base.display()
        ),
    );

    let output = run_filter(&config);

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_count(&switched), 1);
    assert_eq!(delivered_count(&parent), 1);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn external_filter_changes_only_the_waited_copy_branch_message() {
    let base = temporary_case("filter");
    let parent = base.join("parent");
    let copy = base.join("copy");
    create_maildir(&parent);
    create_maildir(&copy);
    let config = base.join("rules.rc");
    write_private(
        &config,
        format!(
            "MAILDIR={}\n:0 cw\n{{\n:0 fw\n| sed 's/^Subject: copy branch$/Subject: filtered copy/'\n}}\n:0 c\n* ^Subject: filtered copy$\nmaildir:copy\n:0 c\n* ^Subject: copy branch$\nmaildir:parent\n:0\n/dev/null\n",
            base.display()
        ),
    );

    let output = run_filter(&config);

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_count(&copy), 1);
    assert_eq!(delivered_count(&parent), 1);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn waited_copy_branch_keeps_parent_and_branch_global_locks_held() {
    let base = temporary_case("locks");
    let parent_lock = base.join("parent.lock");
    let branch_lock = base.join("branch.lock");
    let marker = base.join("branch-running");
    let config = base.join("rules.rc");
    write_private(
        &config,
        format!(
            "MAILDIR={}\nLOCKFILE={}\n:0 cw\n{{\nLOCKFILE={}\n:0 w\n| : > {}; sleep 1\n}}\n:0\n/dev/null\n",
            base.display(),
            parent_lock.display(),
            branch_lock.display(),
            marker.display()
        ),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Subject: locks\n\nbody\n")
        .unwrap();

    let started = Instant::now();
    while !marker.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(10));
    }
    for path in [&parent_lock, &branch_lock] {
        let file = rustix::fs::open(
            path.as_os_str().as_bytes(),
            rustix::fs::OFlags::RDWR,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        assert_eq!(
            rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive),
            Err(rustix::io::Errno::AGAIN),
            "lock was not held: {}",
            path.display()
        );
    }

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn unwaited_copy_branch_overlaps_the_parent_and_continues_after_the_block() {
    let base = temporary_case("unwaited");
    let parent = base.join("parent");
    let branch = base.join("branch");
    let marker = base.join("parent-running");
    create_maildir(&parent);
    create_maildir(&branch);
    let config = base.join("rules.rc");
    write_private(
        &config,
        format!(
            "MAILDIR={}\nTIMEOUT=2\nTARGET=parent\nMARKER={}\n:0 c\n{{\n:0 cw\n| while test ! -e \"$MARKER\"; do sleep 0.01; done\nTARGET=branch\n}}\n:0 c\nmaildir:$TARGET\n:0 w\n| : > \"$MARKER\"\n",
            base.display(),
            marker.display()
        ),
    );

    let output = run_filter(&config);

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_count(&parent), 1);
    assert_eq!(delivered_count(&branch), 1);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn unwaited_copy_branch_keeps_filter_changes_local() {
    let base = temporary_case("unwaited-filter");
    let original = base.join("original");
    let changed = base.join("changed");
    create_maildir(&original);
    create_maildir(&changed);
    let config = base.join("rules.rc");
    write_private(
        &config,
        format!(
            "MAILDIR={}\n:0 c\n{{\n:0 fw\n| sed 's/^Subject: copy branch$/Subject: changed/'\n}}\n:0 c\n* ^Subject: changed$\nmaildir:changed\n:0 c\n* ^Subject: copy branch$\nmaildir:original\n:0\n/dev/null\n",
            base.display()
        ),
    );

    let output = run_filter(&config);

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_count(&original), 1);
    assert_eq!(delivered_count(&changed), 1);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn unwaited_copy_branches_load_runtime_rc_paths_from_local_variables() {
    let base = temporary_case("unwaited-include");
    let parent = base.join("parent");
    let branch = base.join("branch");
    create_maildir(&parent);
    create_maildir(&branch);
    write_private(&base.join("parent.rc"), "TARGET=parent\n");
    write_private(&base.join("branch.rc"), "TARGET=branch\n");
    let config = base.join("rules.rc");
    write_private(
        &config,
        format!(
            "MAILDIR={}\nTARGET=parent\nINCLUDE=parent.rc\n:0 c\n{{\nINCLUDE=branch.rc\n}}\nINCLUDERC=$INCLUDE\n:0 c\nmaildir:$TARGET\n:0\n/dev/null\n",
            base.display()
        ),
    );

    let output = run_filter(&config);

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_count(&parent), 1);
    assert_eq!(delivered_count(&branch), 1);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn a_local_lock_makes_a_plain_copy_block_waited() {
    let base = temporary_case("implicit-wait");
    let branch_done = base.join("branch-done");
    let parent_observed = base.join("parent-observed");
    let lock = base.join("block.lock");
    let config = base.join("rules.rc");
    write_private(
        &config,
        format!(
            "MAILDIR={}\n:0 c : {}\n{{\n:0 w\n| sleep 0.1; : > {}\n}}\n:0 cw\n| test -e {} && : > {}\n:0\n/dev/null\n",
            base.display(),
            lock.display(),
            branch_done.display(),
            branch_done.display(),
            parent_observed.display()
        ),
    );

    let output = run_filter(&config);

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(parent_observed.exists());
    fs::remove_dir_all(base).unwrap();
}
