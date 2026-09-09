// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn run_case(
    name: &str,
    rules: impl FnOnce(&std::path::Path) -> String,
) -> (PathBuf, std::process::Output) {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "procmail-rs-async-{}-{unique}-{name}",
        std::process::id()
    ));
    fs::create_dir(&base).unwrap();
    let config = base.join("rules.rc");
    fs::write(&config, rules(&base)).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
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
        .write_all(b"Subject: async\n\nbody\n")
        .unwrap();
    (base, child.wait_with_output().unwrap())
}

#[test]
fn unwaited_pipes_overlap_and_are_reaped_before_completion() {
    let (base, output) = run_case("overlap", |base| {
        let marker = base.join("parent-ran");
        let log = base.join("commands.log");
        format!(
            "MAILDIR={}\nTIMEOUT=3\nLOGFILE={}\n:0 c\n| while test ! -e {}; do sleep 0.01; done; printf first >&2\n:0\n| : > {}; printf second >&2\n",
            base.display(),
            log.display(),
            marker.display(),
            marker.display()
        )
    });

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let log = fs::read(base.join("commands.log")).unwrap();
    assert!(log.windows(5).any(|bytes| bytes == b"first"));
    assert!(log.windows(6).any(|bytes| bytes == b"second"));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn unwaited_pipe_holds_its_local_lock_until_the_child_exits() {
    let (base, output) = run_case("lock", |base| {
        let marker = base.join("second-ran");
        let acquired = base.join("lock-was-free");
        let lock = base.join("recipe.lock");
        format!(
            "MAILDIR={}\nTIMEOUT=3\n:0 c : {}\n| while test ! -e {}; do sleep 0.01; done\n:0\n| if flock -n {} -c true; then : > {}; fi; : > {}\n",
            base.display(),
            lock.display(),
            marker.display(),
            lock.display(),
            acquired.display(),
            marker.display()
        )
    });

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!base.join("lock-was-free").exists());
    fs::remove_dir_all(base).unwrap();
}
