use std::fs;
use std::io::{Seek, Write};
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
fn invalid_configuration_does_not_consume_stdin() {
    for rules in [
        ":0\n| unsupported\n",
        "LIMIT_MSG_BODY=10KB\n:0\ninbox/\n",
        ":0 B\n* body\ninbox/\n",
        ":0\nmaildir:$UNDEFINED\n",
        ":0\nmbox:unsupported\n",
        ":0\nambiguous\n",
    ] {
        let config = config_file(rules);
        let input_path = config.parent().unwrap().join("message.eml");
        fs::write(&input_path, b"Subject: must remain unread\n\nbody").unwrap();
        let mut input = fs::File::open(&input_path).unwrap();

        // File::try_clone and the descriptor inherited by the child share a
        // file position on Linux. Observing offset zero after exit proves the
        // filter rejected configuration before issuing any stdin read.
        let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .stdin(Stdio::from(input.try_clone().unwrap()))
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert_eq!(input.stream_position().unwrap(), 0, "rules: {rules:?}");
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }
}

#[test]
fn check_rejects_unresolved_destination_types() {
    for (action, expected) in [
        (
            "mbox:unsupported",
            "line 2: mbox delivery is not implemented",
        ),
        (
            "ambiguous",
            "line 2: destination type is ambiguous; use an explicit maildir: or mbox: prefix, or a trailing '/' for Maildir",
        ),
    ] {
        let path = config_file(&format!(":0\n{action}\n"));
        let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["check", "--config"])
            .arg(&path)
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(String::from_utf8(output.stderr).unwrap().contains(expected));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
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
fn maildir_resolves_relative_delivery_paths() {
    let path = config_file("");
    let maildir = path.parent().unwrap().join("mail");
    let inbox = maildir.join("inbox");
    fs::create_dir(&maildir).unwrap();
    create_maildir(&inbox);
    fs::write(
        &path,
        format!("MAILDIR={}\n:0\nmaildir:inbox\n", maildir.display()),
    )
    .unwrap();
    let input = b"Subject: relative\n\nbody";
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
    assert_eq!(delivered_messages(&inbox), [input.to_vec()]);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn filter_expands_assignment_and_destination_variables() {
    let path = config_file("");
    let maildir = path.parent().unwrap().join("mail");
    let inbox = maildir.join("inbox");
    create_maildir(&maildir);
    create_maildir(&inbox);
    fs::write(
        &path,
        format!(
            "ROOT={}\nBOX=inbox\nMAILDIR=$ROOT\n:0\nmaildir:${{BOX}}\n",
            maildir.display()
        ),
    )
    .unwrap();
    let input = b"Subject: expanded\n\nbody";
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
    assert_eq!(delivered_messages(&inbox), [input.to_vec()]);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn filter_expands_explicit_command_line_variables() {
    let path = config_file("");
    let maildir = path.parent().unwrap().join("inbox");
    create_maildir(&maildir);
    fs::write(&path, ":0\nmaildir:$DESTINATION\n").unwrap();
    let input = b"Subject: supplied\n\nbody";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--set"])
        .arg(format!("DESTINATION={}", maildir.display()))
        .args(["--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&maildir), [input.to_vec()]);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn invalid_command_line_variable_does_not_consume_stdin() {
    let config = config_file(":0\nmaildir:unused\n");
    let input_path = config.parent().unwrap().join("message.eml");
    fs::write(&input_path, b"Subject: must remain unread\n\nbody").unwrap();
    let mut input = fs::File::open(&input_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .args(["--set", "LASTFOLDER=forged"])
        .stdin(Stdio::from(input.try_clone().unwrap()))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("variable LASTFOLDER cannot be supplied with --set")
    );
    assert_eq!(input.stream_position().unwrap(), 0);
    fs::remove_dir_all(config.parent().unwrap()).unwrap();
}

#[test]
fn filter_does_not_import_ambient_environment_variables() {
    let config = config_file(":0\nmaildir:$PROCMail_RS_AMBIENT_TEST\n");
    let input_path = config.parent().unwrap().join("message.eml");
    fs::write(&input_path, b"Subject: must remain unread\n\nbody").unwrap();
    let mut input = fs::File::open(&input_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .env("PROCMail_RS_AMBIENT_TEST", "attacker-controlled")
        .stdin(Stdio::from(input.try_clone().unwrap()))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("variable PROCMail_RS_AMBIENT_TEST is not defined")
    );
    assert_eq!(input.stream_position().unwrap(), 0);
    fs::remove_dir_all(config.parent().unwrap()).unwrap();
}

#[test]
fn check_rejects_too_many_command_line_variables() {
    let config = config_file(":0\nmaildir:unused\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_procmail-rs"));
    command.args(["check", "--config"]).arg(&config);
    for index in 0..=256 {
        command.args(["--set", &format!("V{index}=value")]);
    }

    let output = command.output().unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("too many --set values; hard limit is 256")
    );
    fs::remove_dir_all(config.parent().unwrap()).unwrap();
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
            "MAILDIR={}\n:0c\n* ^Subject: selected\nmaildir:{}\n:0B\n* needle\nmaildir:{}\n",
            path.parent().unwrap().display(),
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
            "MAILDIR={}\nLIMIT_MSG_BODY=3\n:0c\n* ^Subject: selected\nmaildir:{}\n:0B\n* body\nmaildir:{}\n",
            path.parent().unwrap().display(),
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

#[test]
fn size_only_rule_replays_staged_message_to_late_destination() {
    let path = config_file("");
    let final_maildir = path.parent().unwrap().join("final");
    create_maildir(&final_maildir);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\n* < 4096\nmaildir:{}\n",
            path.parent().unwrap().display(),
            final_maildir.display()
        ),
    )
    .unwrap();
    let input = b"Subject: size\n\nbinary:\xff\x00body";
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
    assert_eq!(delivered_messages(&final_maildir), [input.to_vec()]);
    assert_eq!(
        fs::read_dir(path.parent().unwrap().join(".procmail-rs-staging"))
            .unwrap()
            .count(),
        0
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
