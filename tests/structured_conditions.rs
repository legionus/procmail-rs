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
        "procmail-rs-structured-condition-{}-{unique}",
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

fn delivered_messages(path: &Path) -> Vec<Vec<u8>> {
    fs::read_dir(path.join("new"))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect()
}

#[test]
fn address_condition_observes_headers_added_by_an_earlier_recipe() {
    let base = temporary_directory();
    let destinations = base.join("dest");
    fs::create_dir(&destinations).unwrap();
    let bar = destinations.join("bar");
    let baz = destinations.join("baz");
    create_maildir(&bar);
    create_maildir(&baz);

    let config = base.join("rules.rc");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\n\
:0\n\
* address Cc ?? bar@.*\n\
dest/bar/\n\
\n\
:0\n\
headers {{\n\
    add Cc bar@example.com\n\
}}\n\
\n\
:0\n\
* address Cc ?? bar@.*\n\
dest/baz/\n",
            base.display()
        ),
    )
    .unwrap();

    let message = b"From: foo@example.com\n\
To: me@example.com\n\
Cc: copy@example.com\n\
Subject: test\n\
\n\
Text\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(message).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(delivered_messages(&bar).is_empty());
    let delivered = delivered_messages(&baz);
    assert_eq!(delivered.len(), 1);
    assert!(
        delivered[0]
            .windows(b"Cc: bar@example.com\n".len())
            .any(|window| window == b"Cc: bar@example.com\n")
    );

    fs::remove_dir_all(base).unwrap();
}

#[test]
fn thread_condition_routes_all_replies_to_same_discussion_folder() {
    let base = temporary_directory();
    let thread_dir = base.join("thread");
    let other_dir = base.join("other");
    create_maildir(&thread_dir);
    create_maildir(&other_dir);

    let config = base.join("rules.rc");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\n\
:0\n\
* identifier %thread ?? ^root-discuss-42@example\\.org$\n\
thread/\n\
\n\
:0\n\
other/\n",
            base.display()
        ),
    )
    .unwrap();

    let root_msg = b"Message-ID: <root-discuss-42@example.org>\n\
From: author@example.org\n\
Subject: Start of discussion\n\nHello\n";

    let reply_msg = b"Message-ID: <reply-101@example.org>\n\
From: peer@example.org\n\
In-Reply-To: <root-discuss-42@example.org>\n\
References: <root-discuss-42@example.org>\n\
Subject: Re: Start of discussion\n\nReply\n";

    let subreply_msg = b"Message-ID: <subreply-202@example.org>\n\
From: third@example.org\n\
In-Reply-To: <reply-101@example.org>\n\
References: <root-discuss-42@example.org> <reply-101@example.org>\n\
Subject: Re: Start of discussion\n\nSubreply\n";

    let unrelated_msg = b"Message-ID: <unrelated-303@example.org>\n\
From: stranger@example.org\n\
Subject: Unrelated\n\nOther\n";

    for msg in [
        root_msg.as_slice(),
        reply_msg.as_slice(),
        subreply_msg.as_slice(),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(msg).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    }

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
        .write_all(unrelated_msg)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);

    assert_eq!(delivered_messages(&thread_dir).len(), 3);
    assert_eq!(delivered_messages(&other_dir).len(), 1);

    fs::remove_dir_all(base).unwrap();
}

#[test]
fn recipient_and_delivered_aliases_match_subaddresses_and_envelope_headers() {
    let base = temporary_directory();
    let alerts_dir = base.join("alerts");
    let cron_dir = base.join("cron");
    let fallback_dir = base.join("fallback");
    create_maildir(&alerts_dir);
    create_maildir(&cron_dir);
    create_maildir(&fallback_dir);

    let config = base.join("rules.rc");
    fs::write(
        &config,
        format!(
            "MAILDIR={}\n\
:0\n\
* address %delivered ?? ^user\\+cron@example\\.org$\n\
cron/\n\
\n\
:0\n\
* address %recipient ?? ^user\\+alerts@example\\.org$\n\
alerts/\n\
\n\
:0\n\
fallback/\n",
            base.display()
        ),
    )
    .unwrap();

    let cron_msg = b"From: system@example.org\n\
Delivered-To: <user+cron@example.org>\n\
To: undisclosed-recipients:;\n\
Subject: Cron job\n\nDaily report\n";

    let alias_msg = b"From: monitoring@example.org\n\
X-Original-To: \"Oncall\" <user+alerts@example.org>\n\
Delivered-To: user@example.org\n\
To: dev-team@example.org\n\
Subject: Alert\n\nHigh load\n";

    let direct_msg = b"From: friend@example.org\n\
To: user+alerts@example.org\n\
Subject: Heads up\n\nCheck this\n";

    let other_msg = b"From: stranger@example.org\n\
To: someone-else@example.org\n\
Subject: Hello\n\nOther\n";

    for msg in [
        cron_msg.as_slice(),
        alias_msg.as_slice(),
        direct_msg.as_slice(),
        other_msg.as_slice(),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(&config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(msg).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    }

    assert_eq!(delivered_messages(&cron_dir).len(), 1);
    assert_eq!(delivered_messages(&alerts_dir).len(), 2);
    assert_eq!(delivered_messages(&fallback_dir).len(), 1);

    fs::remove_dir_all(base).unwrap();
}
