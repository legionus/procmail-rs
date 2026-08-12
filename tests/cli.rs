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

fn create_maildir(path: &std::path::Path) {
    fs::create_dir(path).unwrap();
    fs::create_dir(path.join("tmp")).unwrap();
    fs::create_dir(path.join("new")).unwrap();
    fs::create_dir(path.join("cur")).unwrap();
}

fn delivered_messages(path: &std::path::Path) -> Vec<Vec<u8>> {
    fs::read_dir(path.join("new"))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect()
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
    let path = config_file("");
    let maildir = path.parent().unwrap().join("inbox");
    create_maildir(&maildir);
    fs::write(
        &path,
        format!("LIMIT_MSG_BODY=3\n:0\nmaildir:{}\n", maildir.display()),
    )
    .unwrap();
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
    assert_eq!(fs::read_dir(maildir.join("tmp")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(maildir.join("new")).unwrap().count(), 0);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn filter_streams_header_decided_message_to_maildir() {
    let path = config_file("");
    let maildir = path.parent().unwrap().join("inbox");
    create_maildir(&maildir);
    fs::write(&path, format!(":0\nmaildir:{}\n", maildir.display())).unwrap();
    let input = b"Subject: selected\n\nbinary:\xff\x00body";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read_dir(maildir.join("tmp")).unwrap().count(), 0);
    let files: Vec<_> = fs::read_dir(maildir.join("new"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(fs::read(&files[0]).unwrap(), input);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn filter_streams_early_copy_while_buffering_for_body_rule() {
    let path = config_file("");
    let copy = path.parent().unwrap().join("copy");
    let final_maildir = path.parent().unwrap().join("final");
    create_maildir(&copy);
    create_maildir(&final_maildir);
    fs::write(
        &path,
        format!(
            ":0c\n* ^Subject: selected\nmaildir:{}\n:0B\n* needle\nmaildir:{}\n",
            copy.display(),
            final_maildir.display()
        ),
    )
    .unwrap();
    let input = b"Subject: selected\n\nbinary:\xffneedle\x00body";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&copy), [input.to_vec()]);
    assert_eq!(delivered_messages(&final_maildir), [input.to_vec()]);
    assert_eq!(fs::read_dir(copy.join("tmp")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(final_maildir.join("tmp")).unwrap().count(), 0);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn body_limit_aborts_early_copy_before_publication() {
    let path = config_file("");
    let copy = path.parent().unwrap().join("copy");
    let final_maildir = path.parent().unwrap().join("final");
    create_maildir(&copy);
    create_maildir(&final_maildir);
    fs::write(
        &path,
        format!(
            "LIMIT_MSG_BODY=3\n:0c\n* ^Subject: selected\nmaildir:{}\n:0B\n* body\nmaildir:{}\n",
            copy.display(),
            final_maildir.display()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Subject: selected\n\nbody")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(!output.status.success());
    assert!(delivered_messages(&copy).is_empty());
    assert!(delivered_messages(&final_maildir).is_empty());
    assert_eq!(fs::read_dir(copy.join("tmp")).unwrap().count(), 0);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
