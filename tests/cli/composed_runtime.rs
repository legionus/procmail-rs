// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

fn filter(path: &std::path::Path, message: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--config"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(message).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn header_and_staged_paths_preserve_captures_edits_and_binary_body() {
    let input = b"Subject: one\n two\n\nbody\0\xff\n";
    let mut results = Vec::new();

    // A body condition on the first action forces replay before header edits.
    // Both executions must publish identical bytes, including arbitrary body
    // bytes and captures obtained from the normalized folded header.
    for staged in [false, true] {
        let path = config_file("");
        let base = path.parent().unwrap();
        create_maildir(&base.join("selected"));

        // An unusable staging directory proves the header-only case does not
        // accidentally buffer the body while executing an included rc file.
        if !staged {
            let staging = base.join(".procmail-rs-staging");
            fs::create_dir(&staging).unwrap();
            fs::set_permissions(&staging, fs::Permissions::from_mode(0o777)).unwrap();
        }

        let guard = if staged { "* B ?? body\n" } else { "" };
        fs::write(&path, format!(
            "MAILDIR={}\n:0\n{guard}headers {{\n extract unfolded Subject into SUBJECT\n}}\n:0\n* SUBJECT ?? ^one[ ]+\\/(two)$\nheaders {{\n set X-Capture $MATCH\n set X-Group $MATCH1\n}}\nINCLUDERC=child.rc\n",
            base.display()
        )).unwrap();
        let child = base.join("child.rc");
        fs::write(&child, ":0\n* ^X-Capture: two$\nmaildir:selected\n").unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
        let output = filter(&path, input);
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        results.push(delivered_messages(&base.join("selected")));
        fs::remove_dir_all(base).unwrap();
    }

    assert_eq!(results[0], results[1]);
    assert_eq!(
        results[0],
        [b"Subject: one\n two\nX-Capture: two\nX-Group: two\n\nbody\0\xff\n".to_vec()]
    );
}

#[test]
fn filter_include_copy_and_error_recovery_preserve_parent_message() {
    let path = config_file("");
    let base = path.parent().unwrap();

    for name in ["before", "copy", "parent", "unexpected"] {
        create_maildir(&base.join(name));
    }

    let child = base.join("child.rc");
    fs::write(&child, ":0\n* ^Subject: \\/(filtered)$\nheaders {\n set X-Include $MATCH1\n}\n:0 cw\n{\n:0\nheaders {\n set X-Branch copy\n}\n:0\nmaildir:copy\n}\n").unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, format!(
        "MAILDIR={}\n:0 fw\n| printf 'Subject: filtered\\n\\nbody\\n'\n:0 c\nmaildir:before\nINCLUDERC=child.rc\n:0 fw\n| printf 'Subject: rejected\\n\\nwrong\\n'; exit 7\n:0 e\nheaders {{\n set X-Status $?\n set X-Folder $LASTFOLDER\n}}\n:0\n* ^X-Status: 7$\n* ^X-Include: filtered$\nmaildir:parent\n:0\nmaildir:unexpected\n",
        base.display()
    )).unwrap();
    let output = filter(&path, b"Subject: original\n\noriginal body\n");
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(
        delivered_messages(&base.join("before")),
        [b"Subject: filtered\n\nbody\n".to_vec()]
    );
    assert_eq!(
        delivered_messages(&base.join("copy")),
        [b"Subject: filtered\nX-Include: filtered\nX-Branch: copy\n\nbody\n".to_vec()]
    );
    let published_before = fs::read_dir(base.join("before/new"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        delivered_messages(&base.join("parent")),
        [format!(
            "Subject: filtered\nX-Include: filtered\nX-Status: 7\nX-Folder: {}\n\nbody\n",
            published_before.display()
        )
        .into_bytes()]
    );
    assert!(delivered_messages(&base.join("unexpected")).is_empty());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn recovered_copy_does_not_mark_the_original_delivered() {
    let path = config_file("");
    let base = path.parent().unwrap();
    create_maildir(&base.join("copy"));
    let child = base.join("child.rc");
    fs::write(
        &child,
        ":0 fw\n| printf 'Subject: rejected\\n\\nwrong\\n'; exit 7\n:0 ec\nmaildir:copy\n",
    )
    .unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!(
            "MAILDIR={}\n:0\nheaders {{\n set X-State retained\n}}\nINCLUDERC=child.rc\n",
            base.display()
        ),
    )
    .unwrap();
    let output = filter(&path, b"Subject: original\n\nbody\0\xff\n");
    assert_eq!(output.status.code(), Some(79), "{:?}", output.stderr);
    assert!(String::from_utf8_lossy(&output.stderr).contains("published 1 copy destination"));
    assert_eq!(
        delivered_messages(&base.join("copy")),
        [b"Subject: original\nX-State: retained\n\nbody\0\xff\n".to_vec()]
    );
    fs::remove_dir_all(base).unwrap();
}
