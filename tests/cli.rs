use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

fn config_file(contents: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("procmail-rs-test-{}-{unique}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("rules.rc");
    fs::write(&path, contents).unwrap();
    path
}

#[test]
fn check_accepts_valid_config() {
    let path = config_file(":0\nmaildir:inbox\n");
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn check_reports_source_line() {
    let path = config_file(":0\n| command\n");
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("rules.rc:line 2: pipe actions are not supported")
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn check_rejects_invalid_message_limit() {
    let path = config_file("LIMIT_MSG_BODY=10KB\n:0\ninbox/\n");
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("rules.rc:line 1: invalid LIMIT_MSG_BODY")
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn filter_reports_body_limit() {
    let path = config_file("LIMIT_MSG_BODY=3\n:0\ninbox/\n");
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"\nbody").unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("message exceeds LIMIT_MSG_BODY (3 bytes)")
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
