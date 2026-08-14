// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::ffi::OsString;
use std::fs;
use std::io::{Seek, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
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

fn assert_message_contents_absent(stderr: &[u8], secrets: &[&str]) {
    let diagnostic = String::from_utf8(stderr.to_vec()).unwrap();
    for secret in secrets {
        assert!(!diagnostic.contains(secret), "diagnostic leaked {secret:?}");
    }
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
fn filter_executes_header_only_include_and_returns_to_caller() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let selected = mailbase.join("selected");
    let fallback = mailbase.join("fallback");
    create_maildir(&mailbase);
    create_maildir(&selected);
    create_maildir(&fallback);
    let child_rc = mailbase.join("child.rc");
    fs::write(&child_rc, ":0\n* ^Subject: selected$\nmaildir:selected\n").unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nINCLUDERC=child.rc\n:0\nmaildir:fallback\n",
            mailbase.display()
        ),
    )
    .unwrap();

    for (subject, destination) in [
        ("selected", selected.as_path()),
        ("other", fallback.as_path()),
    ] {
        let input = format!("Subject: {subject}\n\nbody");
        let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().unwrap().write_all(input.as_bytes())?;
                child.wait_with_output()
            })
            .unwrap();

        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        assert_eq!(delivered_messages(destination), [input.into_bytes()]);
    }
    assert_eq!(delivered_messages(&selected).len(), 1);
    assert_eq!(delivered_messages(&fallback).len(), 1);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn filter_stages_only_after_selected_include_requires_body() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let selected = mailbase.join("selected");
    let fallback = mailbase.join("fallback");
    create_maildir(&mailbase);
    create_maildir(&selected);
    create_maildir(&fallback);
    let child_rc = mailbase.join("body.rc");
    fs::write(&child_rc, ":0 B\n* needle\nmaildir:selected\n").unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nINCLUDERC=body.rc\n:0\nmaildir:fallback\n",
            mailbase.display()
        ),
    )
    .unwrap();

    for (body, destination) in [
        ("contains needle", selected.as_path()),
        ("ordinary body", fallback.as_path()),
    ] {
        let input = format!("Subject: body include\n\n{body}");
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
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();

        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        assert_eq!(delivered_messages(destination), [input.into_bytes()]);
    }
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn filter_resolves_include_inside_selected_block_from_match() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let selected = mailbase.join("selected");
    let fallback = mailbase.join("fallback");
    create_maildir(&mailbase);
    create_maildir(&selected);
    create_maildir(&fallback);
    let child_rc = mailbase.join("selected.rc");
    fs::write(&child_rc, ":0\nmaildir:selected\n").unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\n* ^X-Rules: \\/(.*)$\n{{\nINCLUDERC=$MATCH\n}}\n:0\nmaildir:fallback\n",
            mailbase.display()
        ),
    )
    .unwrap();

    for (header, destination) in [
        ("X-Rules: selected.rc\n", selected.as_path()),
        ("", fallback.as_path()),
    ] {
        let input = format!("{header}Subject: runtime include\n\nbody");
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
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();

        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        assert_eq!(delivered_messages(destination), [input.into_bytes()]);
    }
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn included_ordered_rules_share_lastfolder_with_their_caller() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    create_maildir(&mailbase);
    let first = base.join("first.mbox");
    let second = base.join("first.mbox.second");
    let child_rc = mailbase.join("ordered.rc");
    fs::write(
        &child_rc,
        format!(
            ":0 c\nmbox:{}\n:0 a\nmbox:${{LASTFOLDER}}.second\n",
            first.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!("MAILDIR={}\nINCLUDERC=ordered.rc\n", mailbase.display()),
    )
    .unwrap();
    let input = b"Subject: ordered include\n\nbody";
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

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    for mailbox in [first, second] {
        let stored = fs::read(mailbox).unwrap();
        assert!(stored.ends_with(b"Subject: ordered include\n\nbody\n\n"));
    }
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn explain_reports_safe_plan_without_reading_or_delivery() {
    let path = config_file("");
    let destination = path.parent().unwrap().join("must-not-exist");
    fs::write(
        &path,
        format!(
            "PRIVATE_VALUE=assignment-secret\n:0 B\n* body-secret\nmaildir:{}\n",
            destination.display()
        ),
    )
    .unwrap();
    let input_path = path.parent().unwrap().join("message.eml");
    fs::write(&input_path, b"Subject: stdin-secret\n\nbody").unwrap();
    let mut input = fs::File::open(&input_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["explain", "--config"])
        .arg(&path)
        .stdin(Stdio::from(input.try_clone().unwrap()))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(input.stream_position().unwrap(), 0);
    assert!(!destination.exists());
    let explanation = String::from_utf8(output.stdout).unwrap();
    assert!(explanation.contains("input headers=yes body=yes end=yes"));
    assert!(explanation.contains("recipe line=2"));
    assert!(explanation.contains("condition kind=body-regex negated=no"));
    for secret in [
        "assignment-secret",
        "body-secret",
        "must-not-exist",
        "stdin-secret",
    ] {
        assert!(!explanation.contains(secret), "leaked {secret:?}");
    }
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn explain_reports_internal_stdout_failure() {
    let path = config_file(":0\nmaildir:unused\n");
    let failing_stdout = fs::File::options().write(true).open("/dev/full").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["explain", "--config"])
        .arg(&path)
        .stdout(Stdio::from(failing_stdout))
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(70));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("cannot write plan explanation")
    );
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

    assert_eq!(output.status.code(), Some(78));
    assert!(output.stdout.is_empty());
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
        ":0\nambiguous\n",
        ":0\nmaildir:../escape\n",
        ":0\nmaildir:one//two\n",
        "MAILDIR=\n:0\nmaildir:inbox\n",
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
fn non_utf8_command_line_value_does_not_consume_stdin() {
    let config = config_file(":0\nmaildir:$DESTINATION\n");
    let input_path = config.parent().unwrap().join("message.eml");
    fs::write(&input_path, b"Subject: must remain unread\n\nbody").unwrap();
    let mut input = fs::File::open(&input_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .arg("--set")
        .arg(OsString::from_vec(b"DESTINATION=bad\xffpath".to_vec()))
        .stdin(Stdio::from(input.try_clone().unwrap()))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("--set value is not valid UTF-8")
    );
    assert_eq!(input.stream_position().unwrap(), 0);
    fs::remove_dir_all(config.parent().unwrap()).unwrap();
}

#[test]
fn check_rejects_unresolved_destination_types() {
    let path = config_file(":0\nambiguous\n");
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains(
        "line 2: destination type is ambiguous; use an explicit maildir: or mbox: prefix, or a trailing '/' for Maildir"
    ));
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
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"X-Private: header-secret\n\nbody-secret")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stdout.is_empty());
    assert!(
        std::str::from_utf8(&output.stderr)
            .unwrap()
            .contains("message exceeds LIMIT_MSG_BODY (3 bytes)")
    );
    assert_message_contents_absent(&output.stderr, &["header-secret", "body-secret"]);
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
fn filter_delivers_mboxrd_record() {
    let path = config_file("");
    let mailbox = path.parent().unwrap().join("mailbox");
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\nmbox:{}\n",
            path.parent().unwrap().display(),
            mailbox.display()
        ),
    )
    .unwrap();
    let input = b"From hostile header\nX: value\n\nFrom body\n>From quoted\n";
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

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let stored = fs::read(&mailbox).unwrap();
    assert!(stored.starts_with(b"From MAILER-DAEMON "));
    assert!(stored.ends_with(b">From hostile header\nX: value\n\n>From body\n>>From quoted\n\n"));
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn error_recipe_recovers_from_a_real_delivery_failure() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let fallback = base.join("fallback");
    create_maildir(&fallback);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\nmaildir:missing\n:0 e\nmaildir:fallback\n",
            base.display()
        ),
    )
    .unwrap();
    let input = b"Subject: recover delivery\n\nbody";
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

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(delivered_messages(&fallback), [input.to_vec()]);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn error_recipe_recovers_from_a_failed_copy_inside_a_block() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let fallback = base.join("fallback");
    create_maildir(&fallback);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\n{{\n:0 c\nmaildir:missing\n}}\n:0 e\nmaildir:fallback\n",
            base.display()
        ),
    )
    .unwrap();
    let input = b"Subject: recover block delivery\n\nbody";
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

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(delivered_messages(&fallback), [input.to_vec()]);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn error_recipe_is_skipped_after_real_delivery_success() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let primary = base.join("primary");
    let fallback = base.join("fallback");
    create_maildir(&primary);
    create_maildir(&fallback);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\nmaildir:primary\n:0 e\nmaildir:fallback\n",
            base.display()
        ),
    )
    .unwrap();
    let input = b"Subject: primary delivery\n\nbody";
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

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&primary), [input.to_vec()]);
    assert!(delivered_messages(&fallback).is_empty());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn success_recipe_is_skipped_after_real_delivery_failure() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let dependent = base.join("dependent");
    create_maildir(&dependent);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0 c\nmaildir:missing\n:0 a\nmaildir:dependent\n",
            base.display()
        ),
    )
    .unwrap();
    let input = b"Subject: failed predecessor\n\nbody";
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

    assert!(!output.status.success());
    assert!(delivered_messages(&dependent).is_empty());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn mbox_delivery_updates_lastfolder_before_later_destination() {
    let path = config_file("");
    let first = path.parent().unwrap().join("first.mbox");
    let second = path.parent().unwrap().join("first.mbox.second");
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0 c\nmbox:{}\n:0\nmbox:${{LASTFOLDER}}.second\n",
            path.parent().unwrap().display(),
            first.display()
        ),
    )
    .unwrap();
    let input = b"Subject: ordered mbox\n\nbody";
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

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    for mailbox in [first, second] {
        let stored = fs::read(mailbox).unwrap();
        assert!(stored.ends_with(b"Subject: ordered mbox\n\nbody\n\n"));
    }
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn filter_reports_temporary_descriptor_exhaustion() {
    let path = config_file("");
    let maildir = path.parent().unwrap().join("inbox");
    create_maildir(&maildir);
    fs::write(&path, format!(":0\nmaildir:{}\n", maildir.display())).unwrap();

    // Limit only the child process. Maildir setup needs several directory
    // descriptors at once, which provides a deterministic retryable failure
    // without filling storage or changing global resource limits.
    let output = Command::new("/bin/sh")
        .args(["-c", "ulimit -n 5; exec \"$@\"", "procmail-rs"])
        .arg(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::from({
            let message = path.parent().unwrap().join("message.eml");
            fs::write(&message, b"Subject: retry later\n\nbody").unwrap();
            fs::File::open(message).unwrap()
        }))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(75));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Too many open files")
    );
    assert!(delivered_messages(&maildir).is_empty());
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn filter_fails_when_no_recipe_delivers_the_original() {
    let path = config_file(":0\n* ^Subject: wanted$\nmaildir:unused\n");
    let input = b"Subject: other\nX-Private: header-secret\n\nbody-secret";
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

    assert_eq!(output.status.code(), Some(79));
    assert!(output.stdout.is_empty());
    assert!(
        std::str::from_utf8(&output.stderr)
            .unwrap()
            .contains("original message was not delivered")
    );
    assert_message_contents_absent(&output.stderr, &["header-secret", "body-secret"]);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn filter_fails_when_only_a_copy_recipe_publishes() {
    let path = config_file("");
    let copy = path.parent().unwrap().join("copy");
    create_maildir(&copy);
    fs::write(&path, format!(":0c\nmaildir:{}\n", copy.display())).unwrap();
    let input = b"Subject: copied only\n\nbody";
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

    assert!(!output.status.success());
    assert_eq!(delivered_messages(&copy), [input.to_vec()]);
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("published 1 copy destination(s)")
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn successful_filter_is_quiet_and_does_not_create_disabled_logfile() {
    let path = config_file("");
    let maildir = path.parent().unwrap().join("inbox");
    let logfile = path.parent().unwrap().join("filter.log");
    create_maildir(&maildir);
    fs::write(
        &path,
        format!(
            "LOGFILE={}\n:0\nmaildir:{}\n",
            logfile.display(),
            maildir.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::from(
            fs::File::open({
                let message = path.parent().unwrap().join("message.eml");
                fs::write(&message, b"Subject: quiet\n\nbody").unwrap();
                message
            })
            .unwrap(),
        ))
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!logfile.exists());
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
fn filter_matches_folded_header_across_body_without_changing_delivery() {
    let path = config_file("");
    let maildir = path.parent().unwrap().join("mail");
    let inbox = maildir.join("inbox");
    create_maildir(&maildir);
    create_maildir(&inbox);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\n* HB ?? beta\\n\\nbody\nmaildir:inbox\n",
            maildir.display()
        ),
    )
    .unwrap();
    let input = b"Subject: alpha\n beta\n\nbody\n";
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

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
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
fn filter_expands_match_captures_in_destination() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let destination = base.join("alpha-beta-beta");
    create_maildir(&destination);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\n* ^Subject: ([a-z]+)-\\/([a-z]+)$\nmaildir:$MATCH1-$MATCH-$MATCH2\n",
            base.display()
        ),
    )
    .unwrap();
    let input = b"Subject: alpha-beta\n\nbody";
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

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(delivered_messages(&destination), [input.to_vec()]);
    fs::remove_dir_all(base).unwrap();
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
fn deferred_copy_keeps_variables_from_its_selection_point() {
    let path = config_file("");
    let maildir = path.parent().unwrap().join("mail");
    let first = maildir.join("first");
    let second = maildir.join("second");
    create_maildir(&maildir);
    create_maildir(&first);
    create_maildir(&second);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nBOX=first\n:0c\nmaildir:$BOX\nBOX=second\n:0B\n* body\nmaildir:$BOX\n",
            maildir.display()
        ),
    )
    .unwrap();
    let input = b"Subject: snapshots\n\nbody";
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
    assert_eq!(delivered_messages(&first), [input.to_vec()]);
    assert_eq!(delivered_messages(&second), [input.to_vec()]);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn runtime_destination_is_resolved_after_previous_copy_is_published() {
    let path = config_file("");
    let maildir = path.parent().unwrap().join("mail");
    let first = maildir.join("first");
    create_maildir(&maildir);
    create_maildir(&first);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0c\nfirst/\n:0\nmaildir:${{LASTFOLDER}}/child/\n",
            maildir.display()
        ),
    )
    .unwrap();
    let input = b"Subject: runtime destination\nX-Private: header-secret\n\nbody-secret";
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

    assert_eq!(output.status.code(), Some(73));
    assert_eq!(delivered_messages(&first), [input.to_vec()]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("runtime variable LASTFOLDER is not set"));
    assert!(stderr.contains("cannot open Maildir"), "{stderr}");
    for secret in ["header-secret", "body-secret"] {
        assert!(!stderr.contains(secret), "diagnostic leaked {secret:?}");
    }
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
