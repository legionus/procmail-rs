// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_directory(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "procmail-rs-log-{}-{unique}-{name}",
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    path
}

fn create_maildir(path: &std::path::Path) {
    fs::create_dir(path).unwrap();
    fs::create_dir(path.join("tmp")).unwrap();
    fs::create_dir(path.join("new")).unwrap();
    fs::create_dir(path.join("cur")).unwrap();
}

#[test]
fn verbose_filter_writes_text_trace_to_logfile() {
    let base = temporary_directory("text");
    let selected = base.join("selected");
    let logfile = base.join("filter.log");
    create_maildir(&selected);
    let config = base.join("rules.rc");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\nLOGFILE={}\nVERBOSE=yes\nLOGDETAIL=values\nBOX=selected\n:0\nheaders {{\n set Subject header-value-sentinel\n add X-Added added-value-sentinel\n prepend X-Prepended prepended-value-sentinel\n rename X-Added to X-Renamed\n extract unfolded Subject into EXTRACTED\n remove X-Prepended\n}}\n:0\n${{BOX}}/\n",
            base.display(),
            logfile.display()
        ),
    )
    .unwrap();

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
        .write_all(b"Subject: wanted\n\nbody\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let trace = fs::read_to_string(&logfile).unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    assert!(trace.contains("procmail-rs: Assigning at line 5 \"BOX=selected\""));
    assert!(trace.contains("procmail-rs: Setting header \"Subject\" at line 8"));
    assert!(trace.contains("procmail-rs: Adding header \"X-Added\" at line 9"));
    assert!(trace.contains("procmail-rs: Prepending header \"X-Prepended\" at line 10"));
    assert!(trace.contains("procmail-rs: Renaming header \"X-Added\" to \"X-Renamed\" at line 11"));
    assert!(trace.contains(
        "procmail-rs: Extracting header \"Subject\" into \"EXTRACTED\" (unfolded) at line 12"
    ));
    assert!(trace.contains("procmail-rs: Removing header \"X-Prepended\" at line 13"));
    assert!(trace.contains("Assigning at line 12 \"EXTRACTED\" (value hidden)"));
    assert!(trace.contains("completed Maildir delivery"));
    for value in [
        "header-value-sentinel",
        "added-value-sentinel",
        "prepended-value-sentinel",
    ] {
        assert!(
            !trace.contains(value),
            "header value leaked into trace: {value}"
        );
    }
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn json_format_emits_json_lines_with_requested_detail() {
    let base = temporary_directory("json");
    let config = base.join("rules.rc");
    fs::write(
        &config,
        "BOX=selected\n:0\nheaders {\n remove X-Test\n}\n:0\nmaildir:${BOX}\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args([
            "filter",
            "--dry-run",
            "--format=json",
            "--detail=values",
            "--config",
        ])
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
        .write_all(b"Subject: test\n\nbody\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let trace = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0), "{trace}");
    for line in trace.lines() {
        assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
    }
    assert!(trace.contains(
        "{\"event\":\"variable-assigned\",\"line\":1,\"name\":\"BOX\",\"source\":\"rc-file\",\"value\":\"selected\",\"value_truncated\":false}"
    ));
    assert!(trace.contains(
        "{\"event\":\"header-operation\",\"line\":4,\"operation\":\"remove\",\"name\":\"X-Test\"}"
    ));
    assert!(trace.contains("\"stage\":\"dry-run\",\"path\":"));
    fs::remove_dir_all(base).unwrap();
}
