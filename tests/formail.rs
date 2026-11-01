// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io::Write;
use std::process::{Command, Stdio};

fn formail(args: &[&str], stdin: &[u8]) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_formail-rs"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn formail-rs");

    if !stdin.is_empty() {
        if let Some(mut child_stdin) = child.stdin.take() {
            let _ = child_stdin.write_all(stdin);
        }
    } else {
        drop(child.stdin.take());
    }

    let output = child.wait_with_output().expect("failed to wait on child");
    (output.status, output.stdout, output.stderr)
}

#[test]
fn test_version_and_help() {
    let (status, stdout, stderr) = formail(&["-v"], b"");
    assert!(status.success());
    let err_str = String::from_utf8_lossy(&stderr);
    assert!(err_str.contains("formail-rs"));
    assert!(stdout.is_empty());

    let (status, stdout, _) = formail(&["--help"], b"");
    assert!(status.success());
    let out_str = String::from_utf8_lossy(&stdout);
    assert!(out_str.contains("Usage: formail-rs"));
}

#[test]
fn test_unsupported_option_fails() {
    let (status, _, stderr) = formail(&["-s"], b"");
    assert_eq!(status.code(), Some(64));
    let err_str = String::from_utf8_lossy(&stderr);
    assert!(err_str.contains("unsupported option '-s'"));
}

#[test]
fn test_passthrough_and_postmark() {
    let input = b"Subject: Hello\n\nBody line\n";

    // Without -f: generates postmark line
    let (status, stdout, _) = formail(&[], input);
    assert!(status.success());
    assert!(stdout.starts_with(b"From MAILER-DAEMON "));
    assert!(stdout.ends_with(b"Subject: Hello\n\nBody line\n"));

    // With -f: passes through without postmark
    let (status, stdout, _) = formail(&["-f"], input);
    assert!(status.success());
    assert_eq!(stdout, input);

    // Existing From postmark is preserved
    let mbox_input = b"From user@example.org Sat Jan 1 00:00:00 2000\nSubject: Hello\n\nBody\n";
    let (status, stdout, _) = formail(&[], mbox_input);
    assert!(status.success());
    assert_eq!(stdout, mbox_input);
}

#[test]
fn test_delete_and_replace_with_big_i() {
    let input = b"Subject: Old Subject\nX-Keep: 1\n\nBody\n";

    // -I with replacement
    let (status, stdout, _) = formail(&["-f", "-I", "Subject: New Subject"], input);
    assert!(status.success());
    assert_eq!(stdout, b"X-Keep: 1\nSubject: New Subject\n\nBody\n");

    // -I deletion with colon
    let (status, stdout, _) = formail(&["-f", "-I", "Subject:"], input);
    assert!(status.success());
    assert_eq!(stdout, b"X-Keep: 1\n\nBody\n");

    // -I deletion prefix
    let multi_input = b"X-Spam-1: a\nX-Spam-2: b\nSubject: Test\n\nBody\n";
    let (status, stdout, _) = formail(&["-f", "-I", "X-Spam"], multi_input);
    assert!(status.success());
    assert_eq!(stdout, b"Subject: Test\n\nBody\n");
}

#[test]
fn test_rename_and_replace_with_small_i() {
    let input = b"Subject: Old Subject\nX-Keep: 1\n\nBody\n";

    // -i with replacement: old becomes Old-Subject, new is added
    let (status, stdout, _) = formail(&["-f", "-i", "Subject: New Subject"], input);
    assert!(status.success());
    assert_eq!(
        stdout,
        b"Old-Subject: Old Subject\nX-Keep: 1\nSubject: New Subject\n\nBody\n"
    );

    // -i without replacement: renames to Old-Subject only
    let (status, stdout, _) = formail(&["-f", "-i", "Subject:"], input);
    assert!(status.success());
    assert_eq!(stdout, b"Old-Subject: Old Subject\nX-Keep: 1\n\nBody\n");
}

#[test]
fn test_append_always_with_big_a() {
    let input = b"Subject: Test\n\nBody\n";

    let (status, stdout, _) = formail(&["-f", "-A", "X-Added: yes"], input);
    assert!(status.success());
    assert_eq!(stdout, b"Subject: Test\nX-Added: yes\n\nBody\n");

    // Multiple -A appends
    let (status, stdout, _) = formail(&["-f", "-A", "X-First: 1", "-A", "X-Second: 2"], input);
    assert!(status.success());
    assert_eq!(stdout, b"Subject: Test\nX-First: 1\nX-Second: 2\n\nBody\n");

    // Missing colon is rejected
    let (status, _, stderr) = formail(&["-f", "-A", "NoColon"], input);
    assert_eq!(status.code(), Some(64));
    assert!(String::from_utf8_lossy(&stderr).contains("missing colon"));
}

#[test]
fn test_append_if_not_with_small_a() {
    let input = b"Subject: Test\n\nBody\n";

    // Adds if not present
    let (status, stdout, _) = formail(&["-f", "-a", "X-Header: val"], input);
    assert!(status.success());
    assert_eq!(stdout, b"Subject: Test\nX-Header: val\n\nBody\n");

    // Does not add if already present
    let existing = b"Subject: Test\nX-Header: old\n\nBody\n";
    let (status, stdout, _) = formail(&["-f", "-a", "X-Header: new"], existing);
    assert!(status.success());
    assert_eq!(stdout, existing);

    // Message-ID generation
    let (status, stdout, _) = formail(&["-f", "-a", "Message-ID:"], input);
    assert!(status.success());
    let text = String::from_utf8_lossy(&stdout);
    assert!(text.contains("Message-ID: <"));

    // Message-ID intact if already present
    let with_id = b"Subject: Test\nMessage-ID: <id@host>\n\nBody\n";
    let (status, stdout, _) = formail(&["-f", "-a", "Message-ID:"], with_id);
    assert!(status.success());
    assert_eq!(stdout, with_id);
}

#[test]
fn test_uniqueness_with_u_and_big_u() {
    let input = b"X-Dup: first\nX-Other: mid\nX-Dup: second\nX-Dup: third\n\nBody\n";

    // -u keeps first occurrence
    let (status, stdout, _) = formail(&["-f", "-u", "X-Dup:"], input);
    assert!(status.success());
    assert_eq!(stdout, b"X-Dup: first\nX-Other: mid\n\nBody\n");

    // -U keeps last occurrence
    let (status, stdout, _) = formail(&["-f", "-U", "X-Dup:"], input);
    assert!(status.success());
    assert_eq!(stdout, b"X-Other: mid\nX-Dup: third\n\nBody\n");
}

#[test]
fn test_rename_with_big_r() {
    let input = b"X-Old-Field: value\nSubject: Test\n\nBody\n";

    // Exact name rename
    let (status, stdout, _) = formail(&["-f", "-R", "X-Old-Field:", "X-New-Field:"], input);
    assert!(status.success());
    assert_eq!(stdout, b"X-New-Field: value\nSubject: Test\n\nBody\n");

    // Prefix rename
    let (status, stdout, _) = formail(&["-f", "-R", "X-Old-", "X-Renamed-"], input);
    assert!(status.success());
    assert_eq!(stdout, b"X-Renamed-Field: value\nSubject: Test\n\nBody\n");
}

#[test]
fn test_extraction_with_x_and_big_x() {
    let input = b"Subject: Test Subject\nX-Target: extracted value\n\nBody content\n";

    // -x extracts value only, without body
    let (status, stdout, _) = formail(&["-x", "Subject:"], input);
    assert!(status.success());
    assert_eq!(stdout, b" Test Subject\n");

    // -X extracts field name and value, without body
    let (status, stdout, _) = formail(&["-X", "Subject:"], input);
    assert!(status.success());
    assert_eq!(stdout, b"Subject: Test Subject\n");

    // -k keeps body
    let (status, stdout, _) = formail(&["-k", "-X", "Subject:"], input);
    assert!(status.success());
    assert_eq!(stdout, b"Subject: Test Subject\n\nBody content\n");
}

#[test]
fn test_concatenation_with_c() {
    let folded = b"Subject: line 1\n line 2\n\tline 3\nX-Keep: val\n\nBody\n";

    let (status, stdout, _) = formail(&["-f", "-c"], folded);
    assert!(status.success());
    assert_eq!(
        stdout,
        b"Subject: line 1  line 2 \tline 3\nX-Keep: val\n\nBody\n"
    );

    // With extraction -x -c
    let (status, stdout, _) = formail(&["-c", "-x", "Subject:"], folded);
    assert!(status.success());
    assert_eq!(stdout, b" line 1  line 2 \tline 3\n");
}

#[test]
fn test_zap_whitespace_with_z() {
    // Missing whitespace after colon is fixed
    let input = b"Subject:NoSpace\nX-Single: \n\nBody\n";
    let (status, stdout, _) = formail(&["-f", "-z"], input);
    assert!(status.success());
    assert_eq!(stdout, b"Subject: NoSpace\n\nBody\n");

    // -z with -x trims leading and trailing whitespace
    let ws_input = b"Subject:   padded value   \n\nBody\n";
    let (status, stdout, _) = formail(&["-z", "-x", "Subject:"], ws_input);
    assert!(status.success());
    assert_eq!(stdout, b"padded value\n");
}

#[test]
fn test_bogus_from_escaping_and_b() {
    let input = b"Subject: Test\n\nFrom start of line\n>From quoted line\nsafe line\n";

    // Default: escapes bogus From lines with '>'
    let (status, stdout, _) = formail(&["-f"], input);
    assert!(status.success());
    assert_eq!(
        stdout,
        b"Subject: Test\n\n>From start of line\n>>From quoted line\nsafe line\n"
    );

    // -b: disables bogus From escaping
    let (status, stdout, _) = formail(&["-f", "-b"], input);
    assert!(status.success());
    assert_eq!(stdout, input);

    // -p: custom quote prefix
    let (status, stdout, _) = formail(&["-f", "-p", "# "], input);
    assert!(status.success());
    assert_eq!(
        stdout,
        b"Subject: Test\n\n# From start of line\n# >From quoted line\nsafe line\n"
    );
}

#[test]
fn test_composed_flags_and_bundled_options() {
    let input = b"Subject: line 1\n line 2\nX-Old: val\n\nFrom body\n";

    // -fbc: pass-through, no bogus escaping, unfold
    let (status, stdout, _) = formail(&["-fbc"], input);
    assert!(status.success());
    assert_eq!(
        stdout,
        b"Subject: line 1  line 2\nX-Old: val\n\nFrom body\n"
    );

    // Attached argument: -IX-Old:
    let (status, stdout, _) = formail(&["-f", "-IX-Old:"], input);
    assert!(status.success());
    assert_eq!(stdout, b"Subject: line 1\n line 2\n\n>From body\n");
}

#[test]
fn test_formail_streams_body_larger_than_address_space_limit() {
    use std::io::Read;

    const BODY_LEN: usize = 48 * 1024 * 1024;

    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            "ulimit -v 49152; exec \"$@\"",
            "formail-test-shell",
            env!("CARGO_BIN_EXE_formail-rs"),
            "-f",
            "-b",
            "-I",
            "Subject: Streaming",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn formail-rs under ulimit");

    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        if stdin
            .write_all(b"From: sender@example.com\nSubject: Original\n\n")
            .is_err()
        {
            return;
        }
        let chunk = [b'x'; 8192];
        for _ in 0..(BODY_LEN / chunk.len()) {
            if stdin.write_all(&chunk).is_err() {
                break;
            }
        }
    });

    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut total_body = 0usize;
        let mut buf = [0u8; 8192];
        let mut header_buf = Vec::new();
        let mut in_body = false;
        loop {
            let n = stdout.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            if !in_body {
                header_buf.extend_from_slice(&buf[..n]);
                if let Some(pos) = header_buf.windows(2).position(|w| w == b"\n\n") {
                    total_body += header_buf.len() - (pos + 2);
                    in_body = true;
                }
            } else {
                total_body += n;
            }
        }
        (total_body, header_buf)
    });

    writer.join().unwrap();
    let (body_received, header_bytes) = reader.join().unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(body_received, BODY_LEN);
    assert!(header_bytes.starts_with(b"From: sender@example.com\nSubject: Streaming\n\n"));
}
