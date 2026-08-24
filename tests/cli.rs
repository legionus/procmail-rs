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
fn bare_host_stops_processing_as_a_successful_fake_delivery() {
    let path = config_file("HOST\n:0\nmaildir:unreachable\n");
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
        .write_all(b"Subject: test\n\nbody\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!path.parent().unwrap().join("unreachable").exists());
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn failure_handler_can_override_exit_status_and_stop_with_host() {
    let path = config_file(":0 w\n| exit 7\n:0 e\n{\nEXITCODE=75\nHOST\n}\n");
    let rules = format!(
        "MAILDIR={}\n:0 w\n| exit 7\n:0 e\n{{\nEXITCODE=75\nHOST\n}}\n",
        path.parent().unwrap().display()
    );
    fs::write(&path, rules).unwrap();
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
        .write_all(b"Subject: test\n\nbody\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(75), "{:?}", output.stderr);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn invalid_exitcode_is_reported_without_echoing_message_data() {
    let path = config_file("EXITCODE=999\nHOST\n");
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(73));
    assert!(String::from_utf8_lossy(&output.stderr).contains("EXITCODE must be"));
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
fn program_condition_can_select_a_block_and_update_a_quoted_variable() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let selected = base.join("unknown");
    create_maildir(&selected);
    let rules = format!(
        "MAILDIR={}\nUNKNOWN_FOLDER=unknown\nLISTDIR=missing\n:0 W\n* ? test ! -e $LISTDIR\n{{\n    LISTDIR=\"$UNKNOWN_FOLDER\"\n}}\n:0\n$LISTDIR/\n",
        base.display()
    );
    fs::write(&path, rules).unwrap();
    let message = b"Subject: program condition\n\nbody\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(message).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&selected), [message]);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn check_and_explain_accept_pipe_actions_without_executing_them() {
    let path = config_file(":0 fw\n| private-command --secret=value\n");

    let check = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(check.status.code(), Some(0), "{:?}", check.stderr);
    let check_stderr = String::from_utf8(check.stderr).unwrap();
    assert!(
        check_stderr.contains("external shell actions"),
        "{check_stderr}"
    );
    assert!(!check_stderr.contains("private-command"), "{check_stderr}");
    assert!(!check_stderr.contains("secret"), "{check_stderr}");

    let explain = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["explain", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(explain.status.code(), Some(0), "{:?}", explain.stderr);
    let stdout = String::from_utf8(explain.stdout).unwrap();
    assert!(stdout.contains("destination=external-program"), "{stdout}");
    assert!(!stdout.contains("private-command"), "{stdout}");
    assert!(!stdout.contains("secret"), "{stdout}");
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn allowed_filter_replaces_message_before_later_delivery() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let selected = base.join("selected");
    create_maildir(&selected);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0 fw\n| sed 's/^Subject: old$/Subject: new/'\n:0\n* ^Subject: new$\nselected/\n",
            base.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Subject: old\n\nbody")?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(
        delivered_messages(&selected),
        [b"Subject: new\n\nbody\n".to_vec()]
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn failed_waited_filter_keeps_original_for_error_recipe() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let fallback = base.join("fallback");
    create_maildir(&fallback);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0 fw\n| cat; exit 7\n:0 e\nfallback/\n",
            base.display()
        ),
    )
    .unwrap();

    let original = b"Subject: original\n\nbody";
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(original)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("external command exited unsuccessfully")
    );
    assert_eq!(delivered_messages(&fallback), [original.to_vec()]);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn external_filter_stderr_is_appended_to_logfile() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let selected = base.join("selected");
    let logfile = base.join("filter.log");
    create_maildir(&selected);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nLOGFILE={}\n:0 fw\n| printf 'filter diagnostic' >&2; cat\n:0\nselected/\n",
            base.display(),
            logfile.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Subject: original\n\nbody")?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read(&logfile).unwrap(), b"filter diagnostic");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn trap_receives_the_final_filtered_message_and_logs_both_outputs() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let selected = base.join("selected");
    let captured = base.join("trap-message.eml");
    let logfile = base.join("trap.log");
    create_maildir(&selected);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nCAPTURE={}\nLOGFILE={}\nTRAP=\"cat > $CAPTURE; printf trap-out; printf trap-err >&2\"\n:0 fw\n| sed 's/original/replaced/'\n:0\nselected/\n",
            base.display(),
            captured.display(),
            logfile.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Subject: original\n\nbody\n")?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(
        fs::read(&captured).unwrap(),
        b"Subject: replaced\n\nbody\n\n"
    );
    let log = fs::read(&logfile).unwrap();
    assert!(
        log.windows(b"trap-out".len())
            .any(|part| part == b"trap-out")
    );
    assert!(
        log.windows(b"trap-err".len())
            .any(|part| part == b"trap-err")
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn trap_exit_status_follows_exitcode_selection() {
    for (assignment, trap_status, expected) in [
        (None, 6, 0),
        (Some(""), 0, 0),
        (Some(""), 5, 5),
        (Some("7"), 8, 7),
    ] {
        let path = config_file("");
        let base = path.parent().unwrap();
        let selected = base.join("selected");
        create_maildir(&selected);
        let exitcode = assignment
            .map(|value| format!("EXITCODE=\"{value}\"\n"))
            .unwrap_or_default();
        fs::write(
            &path,
            format!(
                "MAILDIR={}\n{exitcode}TRAP=\"exit {trap_status};\"\n:0\nselected/\n",
                base.display()
            ),
        )
        .unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"Subject: status\n\nbody\n")?;
                child.wait_with_output()
            })
            .unwrap();

        assert_eq!(
            output.status.code(),
            Some(expected),
            "assignment={assignment:?}, trap_status={trap_status}, stderr={:?}",
            output.stderr
        );
        fs::remove_dir_all(base).unwrap();
    }
}

#[test]
fn empty_trap_does_not_require_staging() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let selected = base.join("selected");
    create_maildir(&selected);
    fs::write(
        &path,
        format!("TRAP=\"\"\n:0\nmaildir:{}/\n", selected.display()),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Subject: no trap\n\nbody\n")?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!base.join(".procmail-rs-staging").exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn successful_trap_does_not_hide_a_delivery_failure() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let marker = base.join("trap-ran");
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nMARKER={}\nEXITCODE=\"\"\nTRAP=\"touch $MARKER\"\n:0\nmissing/\n",
            base.display(),
            marker.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Subject: failure\n\nbody\n")?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(73), "{:?}", output.stderr);
    assert!(marker.is_file());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn timed_out_trap_uses_a_temporary_status_when_exitcode_is_empty() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let selected = base.join("selected");
    let logfile = base.join("trap.log");
    create_maildir(&selected);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nLOGFILE={}\nTIMEOUT=1\nEXITCODE=\"\"\nTRAP=\"trap '' TERM; sleep 30\"\n:0\nselected/\n",
            base.display(),
            logfile.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Subject: timeout\n\nbody\n")?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(75), "{:?}", output.stderr);
    assert!(
        fs::read_to_string(&logfile)
            .unwrap()
            .contains("TRAP exceeded TIMEOUT")
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn selected_runtime_include_can_install_trap_before_body_staging() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let selected = base.join("selected");
    let child_rc = base.join("trap.rc");
    let captured = base.join("runtime-trap.eml");
    create_maildir(&selected);
    fs::write(
        &child_rc,
        format!("CAPTURE={}\nTRAP=\"cat > $CAPTURE\"\n", captured.display()),
    )
    .unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nCHILD={}\n:0\n* ^Subject: runtime trap\n{{\nINCLUDERC=$CHILD\n}}\n:0\nselected/\n",
            base.display(),
            child_rc.display()
        ),
    )
    .unwrap();
    let message = b"Subject: runtime trap\n\nbody\n";

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(message)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let mut expected = message.to_vec();
    expected.push(b'\n');
    assert_eq!(fs::read(&captured).unwrap(), expected);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn regular_pipe_delivers_to_program_and_discards_stdout() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let captured = base.join("captured.eml");
    let unreachable = base.join("unreachable");
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nCAPTURE={}\n:0 w\n| cat > \"$CAPTURE\"; printf 'not a message'\n:0\nunreachable/\n",
            base.display(),
            captured.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Subject: original\n\nbody")?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(captured).unwrap(), b"Subject: original\n\nbody\n");
    assert!(!unreachable.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn failed_regular_pipe_allows_error_recipe_to_deliver_original() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let fallback = base.join("fallback");
    create_maildir(&fallback);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0 w\n| exit 9\n:0 e\nfallback/\n",
            base.display()
        ),
    )
    .unwrap();

    let original = b"Subject: original\n\nbody";
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(original)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&fallback), [original.to_vec()]);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn copy_pipe_continues_to_final_delivery() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let captured = base.join("captured.eml");
    let selected = base.join("selected");
    create_maildir(&selected);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nCAPTURE={}\n:0 cw\n| cat > \"$CAPTURE\"\n:0\nselected/\n",
            base.display(),
            captured.display()
        ),
    )
    .unwrap();

    let original = b"Subject: original\n\nbody";
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(original)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read(captured).unwrap(), b"Subject: original\n\nbody\n");
    assert_eq!(delivered_messages(&selected), [original.to_vec()]);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn check_recursively_validates_statically_resolved_includes() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let first = base.join("first.rc");
    let malformed = base.join("malformed.rc");
    fs::write(&first, "SWITCHRC=malformed.rc\n").unwrap();
    fs::write(&malformed, ":0\n* unterminated[\nmaildir:selected\n").unwrap();
    fs::set_permissions(&first, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&malformed, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, "RULES=first.rc\nINCLUDERC=$RULES\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .current_dir(base)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(78), "{:?}", output.stderr);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invalid rc syntax"), "{stderr}");
    assert!(stderr.contains("invalid regular expression"), "{stderr}");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn check_stops_following_the_replaced_rc_file_after_switch() {
    let path = config_file("SWITCHRC=selected.rc\nINCLUDERC=unreachable.rc\n");
    let base = path.parent().unwrap();
    let selected = base.join("selected.rc");
    fs::write(&selected, "SELECTED=yes\n").unwrap();
    fs::set_permissions(&selected, fs::Permissions::from_mode(0o600)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .current_dir(base)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn check_warns_for_message_derived_include_without_reading_stdin() {
    let path = config_file(":0\n* ^X-Rules: \\/(.*)$\n{\nINCLUDERC=$MATCH\n}\n");
    let input_path = path.parent().unwrap().join("input.eml");
    fs::write(&input_path, b"X-Rules: private-name.rc\n\nbody").unwrap();
    let mut input = fs::File::open(&input_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .stdin(Stdio::from(input.try_clone().unwrap()))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("warning: rc depth 0, line 4: dynamic INCLUDERC path was not validated"),
        "{stderr}"
    );
    assert!(!stderr.contains("private-name.rc"), "{stderr}");
    assert_eq!(input.stream_position().unwrap(), 0);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn check_preserves_message_derived_assignments_in_a_static_include() {
    let path = config_file("INCLUDERC=child.rc\n");
    let base = path.parent().unwrap();
    let child_rc = base.join("child.rc");
    fs::write(&child_rc, "SELECTED=$MATCH\nINCLUDERC=$SELECTED\n").unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .current_dir(base)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("warning: rc depth 1, line 2: dynamic INCLUDERC path was not validated"),
        "{stderr}"
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn check_rejects_undefined_ordinary_variables_in_a_static_include() {
    let path = config_file("INCLUDERC=child.rc\n");
    let base = path.parent().unwrap();
    let child_rc = base.join("child.rc");
    fs::write(&child_rc, "SELECTED=$UNKNOWN\n").unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .current_dir(base)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(78), "{:?}", output.stderr);
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("variable UNKNOWN is not defined")
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn check_limits_dynamic_rc_path_warnings() {
    let limit = procmail_rs::rc_file::MAX_RC_CHECK_WARNINGS;
    let source = "INCLUDERC=$MATCH\n".repeat(limit + 1);
    let path = config_file(&source);

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.matches("path was not validated").count(), limit);
    assert!(
        stderr.contains("1 additional rc warnings were omitted"),
        "{stderr}"
    );
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
fn header_only_runtime_include_does_not_touch_staging() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let selected = mailbase.join("selected");
    let staging = mailbase.join(".procmail-rs-staging");
    create_maildir(&mailbase);
    create_maildir(&selected);
    fs::create_dir(&staging).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o777)).unwrap();
    let child_rc = mailbase.join("header.rc");
    fs::write(&child_rc, ":0\n* ^Subject: selected$\nmaildir:selected\n").unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!("MAILDIR={}\nINCLUDERC=header.rc\n", mailbase.display()),
    )
    .unwrap();
    let input = b"Subject: selected\n\nbody";
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
    assert_eq!(delivered_messages(&selected), [input.to_vec()]);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn body_runtime_include_touches_staging_only_when_selected() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let selected = mailbase.join("selected");
    let fallback = mailbase.join("fallback");
    let staging = mailbase.join(".procmail-rs-staging");
    create_maildir(&mailbase);
    create_maildir(&selected);
    create_maildir(&fallback);
    fs::create_dir(&staging).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o777)).unwrap();
    let child_rc = mailbase.join("body.rc");
    fs::write(&child_rc, ":0 B\n* needle\nmaildir:selected\n").unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\n* ^X-Use-Body: yes$\n{{\nINCLUDERC=body.rc\n}}\n:0\nmaildir:fallback\n",
            mailbase.display()
        ),
    )
    .unwrap();

    let ordinary = b"Subject: ordinary\n\nneedle";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(ordinary).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&fallback), [ordinary.to_vec()]);

    let selected_input = b"X-Use-Body: yes\n\nneedle";
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
        .write_all(selected_input)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(75), "{:?}", output.stderr);
    assert!(delivered_messages(&selected).is_empty());
    assert_eq!(delivered_messages(&fallback), [ordinary.to_vec()]);
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("must not grant access to group or other users")
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn runtime_include_copy_waits_for_later_body_validation() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let copy = base.join("copy");
    let final_maildir = base.join("final");
    create_maildir(&base.join("mailbase"));
    create_maildir(&copy);
    create_maildir(&final_maildir);
    let child_rc = base.join("mailbase/copy.rc");
    fs::write(
        &child_rc,
        format!(":0 c\n* ^Subject: selected$\nmaildir:{}\n", copy.display()),
    )
    .unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nLIMIT_MSG_BODY=3\nINCLUDERC=copy.rc\n:0 B\n* body\nmaildir:{}\n",
            base.join("mailbase").display(),
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

    assert_eq!(output.status.code(), Some(65), "{:?}", output.stderr);
    assert!(delivered_messages(&copy).is_empty());
    assert!(delivered_messages(&final_maildir).is_empty());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn relative_include_and_switch_use_the_process_working_directory() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let working = base.join("working");
    let selected = base.join("selected");
    let unreachable = base.join("unreachable");
    fs::create_dir(&working).unwrap();
    create_maildir(&selected);
    create_maildir(&unreachable);
    let include_rc = working.join("include.rc");
    let switch_rc = working.join("switch.rc");
    fs::write(&include_rc, "SWITCHRC=switch.rc\n").unwrap();
    fs::write(&switch_rc, format!(":0\nmaildir:{}\n", selected.display())).unwrap();
    fs::set_permissions(&include_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&switch_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "INCLUDERC=include.rc\n:0\nmaildir:{}\n",
            unreachable.display()
        ),
    )
    .unwrap();
    let input = b"Subject: process cwd\n\nbody";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&path)
        .current_dir(&working)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&selected), [input.to_vec()]);
    assert!(delivered_messages(&unreachable).is_empty());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn runtime_maildir_changes_the_base_for_include_and_switch() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let initial = base.join("initial");
    let runtime_base = base.join("runtime");
    let fallback = initial.join("fallback");
    create_maildir(&initial);
    create_maildir(&runtime_base);
    create_maildir(&fallback);

    for (statement, rc_name, destination_name) in [
        ("INCLUDERC", "included.rc", "included"),
        ("SWITCHRC", "switched.rc", "switched"),
    ] {
        let destination = runtime_base.join(destination_name);
        create_maildir(&destination);
        let selected_rc = runtime_base.join(rc_name);
        fs::write(&selected_rc, format!(":0\nmaildir:{destination_name}\n")).unwrap();
        fs::set_permissions(&selected_rc, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            &path,
            format!(
                "MAILDIR={}\nNEXT={}\n:0\n* ^X-Select: yes$\n{{\nMAILDIR=$NEXT\n{statement}={rc_name}\n}}\n:0\nmaildir:fallback\n",
                initial.display(),
                runtime_base.display()
            ),
        )
        .unwrap();
        let input = format!("X-Select: yes\nSubject: runtime {statement}\n\nbody");
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
        assert_eq!(delivered_messages(&destination), [input.into_bytes()]);
    }
    assert!(delivered_messages(&fallback).is_empty());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn include_and_switch_cycles_stop_at_the_rc_depth_limit() {
    for statement in ["INCLUDERC", "SWITCHRC"] {
        let path = config_file("");
        let base = path.parent().unwrap();
        let selected_rc = base.join("private-cycle.rc");
        fs::write(&path, format!("{statement}=private-cycle.rc\n")).unwrap();
        fs::write(&selected_rc, format!("{statement}=private-cycle.rc\n")).unwrap();
        fs::set_permissions(&selected_rc, fs::Permissions::from_mode(0o600)).unwrap();
        let input = format!("Subject: {statement} cycle\n\nbody");
        let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&path)
            .current_dir(base)
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

        assert_eq!(output.status.code(), Some(73), "{:?}", output.stderr);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("rc nesting exceeds the hard limit"),
            "{stderr}"
        );
        assert!(!stderr.contains("private-cycle.rc"), "{stderr}");
        fs::remove_dir_all(base).unwrap();
    }
}

#[test]
fn executed_empty_includes_stop_at_the_transition_limit() {
    let limit = procmail_rs::rc_file::MAX_RC_TRANSITIONS;
    let mut source = "INCLUDERC=\n".repeat(limit);
    source.push_str("INCLUDERC=\n:0\nmaildir:unreachable\n");
    let path = config_file(&source);
    let input = b"Subject: transition limit\n\nbody";
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

    assert_eq!(output.status.code(), Some(73), "{:?}", output.stderr);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!("rc transitions exceed the hard limit of {limit}")),
        "{stderr}"
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn filter_reports_failed_include_without_exposing_its_path_and_continues() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let fallback = mailbase.join("fallback");
    create_maildir(&mailbase);
    create_maildir(&fallback);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nINCLUDERC=private-selected-name.rc\n:0\nmaildir:fallback\n",
            mailbase.display()
        ),
    )
    .unwrap();
    let input = b"Subject: failed include\n\nbody";
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
    assert_eq!(delivered_messages(&fallback), [input.to_vec()]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("line 2: INCLUDERC failed:"), "{stderr}");
    assert!(stderr.contains("No such file or directory"), "{stderr}");
    assert!(!stderr.contains("private-selected-name.rc"), "{stderr}");
    assert!(
        !stderr.contains(&mailbase.display().to_string()),
        "{stderr}"
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn filter_reports_malformed_include_and_continues() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let fallback = mailbase.join("fallback");
    create_maildir(&mailbase);
    create_maildir(&fallback);
    let child_rc = mailbase.join("malformed-private-name.rc");
    fs::write(&child_rc, ":0\n* unterminated[\nmaildir:selected\n").unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nINCLUDERC=malformed-private-name.rc\n:0\nmaildir:fallback\n",
            mailbase.display()
        ),
    )
    .unwrap();
    let input = b"Subject: malformed include\n\nbody";
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
    assert_eq!(delivered_messages(&fallback), [input.to_vec()]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("line 2: INCLUDERC failed:"), "{stderr}");
    assert!(!stderr.contains("malformed-private-name.rc"), "{stderr}");
    assert!(
        !stderr.contains(&mailbase.display().to_string()),
        "{stderr}"
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn filter_switches_to_selected_rc_file_and_abandons_the_current_file() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let selected = mailbase.join("selected");
    let unreachable = mailbase.join("unreachable");
    create_maildir(&mailbase);
    create_maildir(&selected);
    create_maildir(&unreachable);
    let switched_rc = mailbase.join("switched.rc");
    fs::write(&switched_rc, ":0\nmaildir:selected\n").unwrap();
    fs::set_permissions(&switched_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\n* ^X-Switch: yes$\n{{\nSWITCHRC=switched.rc\n}}\n:0\nmaildir:unreachable\n",
            mailbase.display()
        ),
    )
    .unwrap();
    let input = b"X-Switch: yes\nSubject: switch root\n\nbody";
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
    assert_eq!(delivered_messages(&selected), [input.to_vec()]);
    assert!(delivered_messages(&unreachable).is_empty());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn switched_include_returns_to_its_caller_after_the_replacement_ends() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let selected = mailbase.join("selected");
    create_maildir(&mailbase);
    create_maildir(&selected);
    let child_rc = mailbase.join("child.rc");
    let switched_rc = mailbase.join("switched.rc");
    fs::write(&child_rc, "SWITCHRC=switched.rc\nTARGET=unreachable\n").unwrap();
    fs::write(&switched_rc, "TARGET=selected\n").unwrap();
    fs::set_permissions(&child_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&switched_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nTARGET=unreachable\nINCLUDERC=child.rc\n:0\nmaildir:$TARGET\n",
            mailbase.display()
        ),
    )
    .unwrap();
    let input = b"Subject: nested switch\n\nbody";
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
    assert_eq!(delivered_messages(&selected), [input.to_vec()]);
    assert!(!mailbase.join("unreachable").exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn empty_switch_ends_the_current_rc_file() {
    let path = config_file("SWITCHRC=\n:0\nmaildir:unreachable\n");
    let input = b"Subject: empty switch\n\nbody";
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

    assert_eq!(output.status.code(), Some(79), "{:?}", output.stderr);
    assert!(!path.parent().unwrap().join("unreachable").exists());
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn failed_switch_reports_the_error_and_continues_the_current_rc_file() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let fallback = mailbase.join("fallback");
    create_maildir(&mailbase);
    create_maildir(&fallback);
    fs::write(
        &path,
        format!(
            "MAILDIR={}\nSWITCHRC=private-missing-switch.rc\n:0\nmaildir:fallback\n",
            mailbase.display()
        ),
    )
    .unwrap();
    let input = b"Subject: failed switch\n\nbody";
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
    assert_eq!(delivered_messages(&fallback), [input.to_vec()]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("line 2: SWITCHRC failed:"), "{stderr}");
    assert!(!stderr.contains("private-missing-switch.rc"), "{stderr}");
    assert!(
        !stderr.contains(&mailbase.display().to_string()),
        "{stderr}"
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn filter_stages_after_switched_rc_file_selects_a_body_rule() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let mailbase = base.join("mailbase");
    let selected = mailbase.join("selected");
    let fallback = mailbase.join("fallback");
    create_maildir(&mailbase);
    create_maildir(&selected);
    create_maildir(&fallback);
    let switched_rc = mailbase.join("body.rc");
    fs::write(
        &switched_rc,
        ":0 B\n* needle\nmaildir:selected\n:0\nmaildir:fallback\n",
    )
    .unwrap();
    fs::set_permissions(&switched_rc, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!("MAILDIR={}\nSWITCHRC=body.rc\n", mailbase.display()),
    )
    .unwrap();

    for (body, destination) in [
        ("contains needle", selected.as_path()),
        ("ordinary body", fallback.as_path()),
    ] {
        let input = format!("Subject: body switch\n\n{body}");
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
    let path = config_file(":0\n! user@example.test\n");
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
            .contains("rules.rc:line 2: forward actions are not supported")
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
fn umask_is_bound_to_each_selected_delivery_in_statement_order() {
    let config = config_file("");
    let directory = config.parent().unwrap();
    let maildir = directory.join("selected");
    let mailbox = directory.join("copy.mbox");
    create_maildir(&maildir);
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nUMASK=0777\n:0 c\nmbox:{}\nUMASK=0000\n:0\nmaildir:{}/\n",
            directory.display(),
            mailbox.display(),
            maildir.display()
        ),
    )
    .unwrap();
    let message = b"Subject: permissions\n\nbody\n";

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(message)?;
            child.wait_with_output()
        })
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::metadata(&mailbox).unwrap().permissions().mode() & 0o777,
        0
    );
    let entries = fs::read_dir(maildir.join("new"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(entries[0].path()).unwrap(), message);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn invalid_configuration_does_not_consume_stdin() {
    for rules in [
        ":0\n| unsupported\n",
        ":0\n! user@example.test\n",
        "DEFAULT=mailbox\n:0\nmaildir:inbox\n",
        "ORGMAIL=mailbox\n:0\nmaildir:inbox\n",
        "COMSAT=yes\n:0\nmaildir:inbox\n",
        "HOST=other-host\n:0\nmaildir:inbox\n",
        "LIMIT_MSG_BODY=10KB\n:0\ninbox/\n",
        ":0 B\n* body\ninbox/\n",
        ":0\nmaildir:$UNDEFINED\n",
        ":0\nambiguous\n",
        ":0\nmaildir:../escape\n",
        ":0\nmaildir:one//two\n",
        "MAILDIR=\n:0\nmaildir:inbox\n",
        "LOCKMETHOD=other\n:0\nmaildir:inbox\n",
        "LOCKTIMEOUT=0\n:0\nmaildir:inbox\n",
        "LOCKTIMEOUT=86401\n:0\nmaildir:inbox\n",
        "LOCKTIMEOUT=invalid\n:0\nmaildir:inbox\n",
        "LINEBUF=127\n:0\nmaildir:inbox\n",
        "LINEBUF=1k\n:0\nmaildir:inbox\n",
        "TIMEOUT=0\n:0\nmaildir:inbox\n",
        "TIMEOUT=86401\n:0\nmaildir:inbox\n",
        "TIMEOUT=invalid\n:0\nmaildir:inbox\n",
        ":0\n{\nTIMEOUT=0\n:0\nmaildir:inbox\n}\n",
        "UMASK=\n:0\nmaildir:inbox\n",
        "UMASK=078\n:0\nmaildir:inbox\n",
        "UMASK=1000\n:0\nmaildir:inbox\n",
        ":0\n{\nUMASK=invalid\n:0\nmaildir:inbox\n}\n",
        "TRAP=:\n:0\nmaildir:inbox\n",
        "TRAP=bad\0command\n:0\nmaildir:inbox\n",
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
fn known_unsupported_constructs_fail_check_before_message_input() {
    let mut cases = procmail_rs::config::UNSUPPORTED_PROCMAIL_VARIABLES
        .iter()
        .map(|name| {
            (
                format!("{name}=value\n:0\nmaildir:unused\n"),
                format!("rules.rc:line 1: procmail variable {name} is not supported"),
            )
        })
        .collect::<Vec<_>>();
    cases.extend([
        (
            ":0\n! user@example.test\n".to_owned(),
            "rules.rc:line 2: forward actions are not supported".to_owned(),
        ),
        (
            "HOST=other-host\n".to_owned(),
            "rules.rc:line 1: non-empty HOST assignments are not supported yet".to_owned(),
        ),
        (
            ":0\nambiguous-path\n".to_owned(),
            "rules.rc:line 2: destination type is ambiguous".to_owned(),
        ),
    ]);

    for (rules, expected) in cases {
        let config = config_file(&rules);
        let input_path = config.parent().unwrap().join("message.eml");
        fs::write(&input_path, b"Subject: must remain unread\n\nbody").unwrap();

        // Check the source-located user diagnostic first, then use a shared
        // file offset with filter because check never opens message input and
        // therefore cannot by itself prove pre-input rejection.
        let checked = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["check", "--config"])
            .arg(&config)
            .output()
            .unwrap();
        assert_eq!(checked.status.code(), Some(78), "{rules:?}");
        assert!(
            String::from_utf8_lossy(&checked.stderr).contains(&expected),
            "rules: {rules:?}, stderr: {:?}",
            String::from_utf8_lossy(&checked.stderr)
        );

        let mut input = fs::File::open(&input_path).unwrap();
        let filtered = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .stdin(Stdio::from(input.try_clone().unwrap()))
            .output()
            .unwrap();
        assert_eq!(filtered.status.code(), Some(78), "{rules:?}");
        assert_eq!(input.stream_position().unwrap(), 0, "{rules:?}");

        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }
}

#[test]
fn logabstract_rejects_header_logging_modes_before_message_input() {
    for rules in [
        "LOGABSTRACT=\n:0\nmaildir:unused\n",
        "LOGABSTRACT=No\n:0\nmaildir:unused\n",
        "LOGABSTRACT=off\n:0\nmaildir:unused\n",
        "LOGABSTRACT=yes\n:0\nmaildir:unused\n",
        "LOGABSTRACT=all\n:0\nmaildir:unused\n",
        "MODE=all\nLOGABSTRACT=$MODE\n:0\nmaildir:unused\n",
    ] {
        let config = config_file(rules);
        let input_path = config.parent().unwrap().join("message.eml");
        fs::write(
            &input_path,
            b"From: private-from-sentinel\nSubject: private-subject-sentinel\n\nbody",
        )
        .unwrap();
        let mut input = fs::File::open(&input_path).unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .stdin(Stdio::from(input.try_clone().unwrap()))
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(78), "{rules:?}");
        assert_eq!(input.stream_position().unwrap(), 0, "{rules:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("LOGABSTRACT supports only 'no'"),
            "{stderr}"
        );
        assert!(!stderr.contains("private-from-sentinel"), "{stderr}");
        assert!(!stderr.contains("private-subject-sentinel"), "{stderr}");
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }
}

#[test]
fn logabstract_no_does_not_create_an_abstract_log() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let selected = base.join("selected");
    let logfile = base.join("filter.log");
    create_maildir(&selected);
    fs::write(
        &config,
        format!(
            "MODE=no\nLOGABSTRACT=$MODE\nLOGFILE={}\nMAILDIR={}\n:0\nmaildir:selected\n",
            logfile.display(),
            base.display()
        ),
    )
    .unwrap();
    let input = b"From: private-from-sentinel\nSubject: private-subject-sentinel\n\nprivate-body";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&selected), [input.to_vec()]);
    assert!(!logfile.exists());
    assert_message_contents_absent(
        &output.stderr,
        &[
            "private-from-sentinel",
            "private-subject-sentinel",
            "private-body",
        ],
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn compatibility_document_lists_every_unsupported_variable() {
    let documentation = include_str!("../Documentation/Compatibility.md");
    for name in procmail_rs::config::UNSUPPORTED_PROCMAIL_VARIABLES {
        assert!(documentation.contains(&format!("`{name}`")), "{name}");
    }
}

#[test]
fn linebuf_does_not_limit_mail_header_lines() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let selected = base.join("selected");
    create_maildir(&selected);
    fs::write(
        &config,
        format!(
            "LINEBUF=128\nLIMIT_HEADER_LINE=256\nMAILDIR={}\n:0\nselected/\n",
            base.display()
        ),
    )
    .unwrap();
    let input = format!("X-Long: {}\n\nbody", "x".repeat(200));

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(input.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&selected), [input.into_bytes()]);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn timed_out_waited_program_runs_its_error_recipe() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let selected = base.join("selected");
    let log = base.join("trace.log");
    create_maildir(&selected);
    fs::write(
        &config,
        format!(
            "TIMEOUT=1\nLOGFILE={}\nMAILDIR={}\n:0 w\n| sleep 30\n:0 e\nselected/\n",
            log.display(),
            base.display()
        ),
    )
    .unwrap();
    let input = b"Subject: timeout\n\nbody";
    let started = std::time::Instant::now();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(input)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    assert_eq!(delivered_messages(&selected), [input.to_vec()]);
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .contains("external command exceeded TIMEOUT")
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn implicit_pipe_lockfile_is_rejected_before_stdin() {
    {
        let rules = ":0 :\n| command\n";
        let config = config_file(rules);
        let input_path = config.parent().unwrap().join("message.eml");
        fs::write(&input_path, b"Subject: must remain unread\n\nbody").unwrap();
        let mut input = fs::File::open(&input_path).unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .stdin(Stdio::from(input.try_clone().unwrap()))
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(78));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains(
                "rules.rc:line 1: an implicit local lockfile requires a filesystem destination"
            ),
            "{stderr}"
        );
        assert_eq!(input.stream_position().unwrap(), 0);
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }
}

#[test]
fn lockmethod_applies_in_statement_order_to_filesystem_recipes() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let first = base.join("first");
    let second = base.join("second");
    create_maildir(&first);
    create_maildir(&second);
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKMETHOD=dotlock\n:0 c:first.lock\nfirst/\nLOCKMETHOD=flock\n:0 :second.lock\nsecond/\n",
            base.display()
        ),
    )
    .unwrap();
    let input = b"Subject: locked\n\nbody";
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(input)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&first), [input.to_vec()]);
    assert_eq!(delivered_messages(&second), [input.to_vec()]);
    assert!(!base.join("first.lock").exists());
    assert!(base.join("second.lock").is_file());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn implicit_flock_name_is_derived_from_the_destination() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let selected = base.join("selected");
    create_maildir(&selected);
    fs::write(
        &config,
        format!("MAILDIR={}\n:0 :\nselected/\n", base.display()),
    )
    .unwrap();
    let input = b"Subject: implicit lock\n\nbody";
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(input)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&selected), [input.to_vec()]);
    assert!(selected.join(".lock").is_file());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn lockext_is_applied_when_each_implicit_lock_is_acquired() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let first = base.join("first");
    let second = base.join("second");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKEXT=.first\n:0 c:\nmbox:{}\n:0\n{{\nLOCKEXT=.second\n:0 :\nmbox:{}\n}}\n",
            base.display(),
            first.display(),
            second.display()
        ),
    )
    .unwrap();
    let input = b"Subject: ordered LOCKEXT\n\nbody";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(base.join("first.first").is_file());
    assert!(base.join("second.second").is_file());
    assert!(
        fs::read(&first)
            .unwrap()
            .ends_with(b"Subject: ordered LOCKEXT\n\nbody\n\n")
    );
    assert!(
        fs::read(&second)
            .unwrap()
            .ends_with(b"Subject: ordered LOCKEXT\n\nbody\n\n")
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn named_dotlock_is_held_while_a_pipe_action_runs() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let lock = base.join("pipe.lock");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKMETHOD=dotlock\nLOCKPATH={}\n:0 w:$LOCKPATH\n| test -f \"$LOCKPATH\"\n",
            base.display(),
            lock.display()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Subject: pipe lock\n\nbody")?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!lock.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn named_dotlock_is_held_across_a_selected_recipe_block() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let lock = base.join("block.lock");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKMETHOD=dotlock\nLOCKPATH={}\n:0 : $LOCKPATH\n{{\n:0 w\n| test -f \"$LOCKPATH\"\n}}\n",
            base.display(),
            lock.display()
        ),
    )
    .unwrap();
    let input = b"Subject: block lock\n\nbody";
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(input)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!lock.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn recipe_block_resolves_its_lockfile_from_match_at_selection_time() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let lock = base.join("selected.lock");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKMETHOD=dotlock\n:0 : $MATCH\n* ^X-Lock: \\/(selected\\.lock)$\n{{\n:0 w\n| test -f \"$MAILDIR/selected.lock\"\n}}\n",
            base.display()
        ),
    )
    .unwrap();
    let input = b"X-Lock: selected.lock\nSubject: dynamic block lock\n\nbody";
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(input)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!lock.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn recipe_block_releases_its_dotlock_before_an_error_handler_runs() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let lock = base.join("block.lock");
    let fallback = base.join("fallback");
    create_maildir(&fallback);
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKMETHOD=dotlock\nLOCKPATH={}\n:0 : $LOCKPATH\n{{\n:0 w\n| false\n}}\n:0 e\nmaildir:fallback\n",
            base.display(),
            lock.display()
        ),
    )
    .unwrap();
    let input = b"Subject: block failure\n\nbody";
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(input)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&fallback), [input.to_vec()]);
    assert!(!lock.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn block_lock_timeout_is_recoverable_by_an_error_handler() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let lock = base.join("busy.lock");
    let marker = base.join("entered");
    let fallback = base.join("fallback");
    create_maildir(&fallback);
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKMETHOD=flock\nLOCKTIMEOUT=1\nLOCKPATH={}\nMARKER={}\n:0 : $LOCKPATH\n{{\n:0 w\n| : > \"$MARKER\"; sleep 2\n}}\n:0 e\nmaildir:fallback\n",
            base.display(),
            lock.display(),
            marker.display()
        ),
    )
    .unwrap();
    let first_input = b"Subject: first block lock\n\nbody";
    let mut first = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    first.stdin.take().unwrap().write_all(first_input).unwrap();
    for _ in 0..200 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(marker.exists());

    let second_input = b"Subject: second block lock\n\nbody";
    let started = std::time::Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(second_input)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(delivered_messages(&fallback), [second_input.to_vec()]);
    assert!(first.wait().unwrap().success());
    assert!(lock.is_file());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn flock_serializes_external_actions_across_processes() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let lock = base.join("shared.lock");
    let events = base.join("events");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKPATH={}\nEVENTS={}\n:0 w:$LOCKPATH\n| printf 'start\\n' >> \"$EVENTS\"; sleep 0.2; printf 'end\\n' >> \"$EVENTS\"\n",
            base.display(),
            lock.display(),
            events.display()
        ),
    )
    .unwrap();

    let spawn = || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"Subject: concurrent\n\nbody")
            .unwrap();
        child
    };
    let mut first = spawn();
    for _ in 0..100 {
        if fs::read(&events).is_ok_and(|bytes| bytes == b"start\n") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(fs::read(&events).unwrap(), b"start\n");
    let mut second = spawn();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());

    assert_eq!(fs::read(&events).unwrap(), b"start\nend\nstart\nend\n");
    assert!(lock.is_file());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn global_lockfile_replacement_follows_statement_order() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let first = base.join("first.lock");
    let second = base.join("second.lock");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKMETHOD=dotlock\nFIRST={}\nSECOND={}\nLOCKFILE=$FIRST\n:0 cw\n| test -f \"$FIRST\"\nLOCKFILE=$SECOND\n:0 w\n| test ! -e \"$FIRST\" && test -f \"$SECOND\"\n",
            base.display(),
            first.display(),
            second.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Subject: global lock\n\nbody")?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(!first.exists());
    assert!(!second.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn locktimeout_bounds_global_flock_wait() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let lock = base.join("global.lock");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOCKTIMEOUT=1\nLOCKFILE={}\n:0 w\n| sleep 2\n",
            base.display(),
            lock.display()
        ),
    )
    .unwrap();

    let spawn = || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"Subject: timeout\n\nbody")
            .unwrap();
        child
    };
    let mut first = spawn();
    for _ in 0..100 {
        if lock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(lock.exists());
    let started = std::time::Instant::now();
    let second = spawn().wait_with_output().unwrap();
    let elapsed = started.elapsed();

    assert_eq!(second.status.code(), Some(75), "{:?}", second.stderr);
    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "{elapsed:?}"
    );
    assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
    assert!(first.wait().unwrap().success());
    assert!(lock.is_file());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn filesystem_ignore_write_error_is_rejected_before_stdin() {
    for destination in ["mbox:target", "maildir:target"] {
        let rules = format!(":0 i\n{destination}\n");
        let config = config_file(&rules);
        let input_path = config.parent().unwrap().join("message.eml");
        fs::write(&input_path, b"Subject: must remain unread\n\nbody").unwrap();
        let mut input = fs::File::open(&input_path).unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .stdin(Stdio::from(input.try_clone().unwrap()))
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(78));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("may publish an incomplete message"),
            "{stderr}"
        );
        assert_eq!(input.stream_position().unwrap(), 0);
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }
}

#[test]
fn block_pipe_flags_are_ignored_with_warnings_and_the_block_still_runs() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let selected = base.join("selected");
    create_maildir(&selected);
    fs::write(
        &config,
        format!(
            "MAILDIR={}\n:0 ir\n{{\n:0\nmaildir:selected\n}}\n",
            base.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&selected), [Vec::new()]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(":2: recipe flag 'i' has no effect on a block"),
        "{stderr}"
    );
    assert!(
        stderr.contains(":2: recipe flag 'r' has no effect on a block"),
        "{stderr}"
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn block_flag_warnings_include_the_runtime_rc_path() {
    let config = config_file("");
    let base = config.parent().unwrap();
    let selected = base.join("selected");
    let child = base.join("child.rc");
    create_maildir(&selected);
    fs::write(
        &config,
        format!("MAILDIR={}\nINCLUDERC=child.rc\n", base.display()),
    )
    .unwrap();
    fs::write(&child, ":0 ir\n{\n:0\nmaildir:selected\n}\n").unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();

    let checked = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["check", "--config"])
        .arg(&config)
        .current_dir(base)
        .output()
        .unwrap();
    assert_eq!(checked.status.code(), Some(0), "{:?}", checked.stderr);
    let check_stderr = String::from_utf8(checked.stderr).unwrap();
    assert!(
        check_stderr.contains(&format!(
            "{}:1: recipe flag 'i' has no effect on a block",
            child.display()
        )),
        "{check_stderr}"
    );

    let filtered = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .current_dir(base)
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(filtered.status.code(), Some(0), "{:?}", filtered.stderr);
    assert_eq!(delivered_messages(&selected), [Vec::new()]);
    let filter_stderr = String::from_utf8(filtered.stderr).unwrap();
    assert!(
        filter_stderr.contains(&format!(
            "warning: {}:1: recipe flag 'r' has no effect on a block",
            child.display()
        )),
        "{filter_stderr}"
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn runtime_block_flag_warnings_stop_at_the_diagnostic_limit() {
    let limit = procmail_rs::eval::MAX_RUNTIME_RC_WARNINGS;
    for warning_count in [limit - 1, limit, limit + 1] {
        let config = config_file("");
        let base = config.parent().unwrap();
        let selected = base.join("selected");
        let child = base.join("child.rc");
        create_maildir(&selected);
        fs::write(
            &config,
            format!("MAILDIR={}\nINCLUDERC=child.rc\n", base.display()),
        )
        .unwrap();
        let mut child_source = ":0 i\n{\n:0\nmaildir:selected\n}\n".repeat(warning_count);
        child_source.push_str(":0\nmaildir:selected\n");
        fs::write(&child, child_source).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .current_dir(base)
            .stdin(Stdio::piped())
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            stderr.matches("has no effect on a block").count(),
            warning_count.min(limit),
            "{stderr}"
        );
        assert_eq!(
            stderr.contains("additional runtime rc warnings were omitted"),
            warning_count > limit,
            "{stderr}"
        );
        fs::remove_dir_all(base).unwrap();
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
fn raw_filesystem_delivery_preserves_maildir_and_minimizes_mbox_framing() {
    let path = config_file("");
    let base = path.parent().unwrap();
    let maildir = base.join("raw-maildir");
    let mailbox = base.join("raw-mbox");
    create_maildir(&maildir);
    let input = b"Subject: raw\n\nFrom body without newline";

    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0 cr\nraw-maildir/\n:0 r\nmbox:{}\n",
            base.display(),
            mailbox.display()
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
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&maildir), [input.to_vec()]);
    let stored = fs::read(&mailbox).unwrap();
    assert!(stored.starts_with(b"From MAILER-DAEMON "));
    assert!(stored.ends_with(b"Subject: raw\n\n>From body without newline\n"));
    assert!(!stored.ends_with(b"\n\n"));
    fs::remove_dir_all(base).unwrap();
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
fn filter_gets_home_and_logname_from_the_passwd_database() {
    let identity = procmail_rs::user_identity::UserIdentity::current().unwrap();
    let config = config_file("");
    let base = config.parent().unwrap();
    let selected = base.join("selected");
    create_maildir(&selected);
    fs::write(
        &config,
        format!(
            "MAILDIR={}\n:0 W\n* ? test \"$HOME\" = \"$EXPECTED_HOME\" && test \"$LOGNAME\" = \"$EXPECTED_LOGNAME\"\n{{\n:0\nselected/\n}}\n",
            base.display()
        ),
    )
    .unwrap();
    let message = b"Subject: passwd identity\n\nbody\n";
    let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .args(["--set", &format!("EXPECTED_HOME={}", identity.home())])
        .args(["--set", &format!("EXPECTED_LOGNAME={}", identity.logname())])
        .env("HOME", "/ambient/home/must-not-win")
        .env("LOGNAME", "ambient-logname-must-not-win")
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(message)?;
            child.wait_with_output()
        })
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(delivered_messages(&selected), [message]);
    fs::remove_dir_all(base).unwrap();
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
