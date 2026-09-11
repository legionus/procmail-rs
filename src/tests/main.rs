// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{ExitStatus, OperationalError, load_root_config};

#[test]
fn operational_errors_have_distinct_stable_exit_statuses() {
    let cases = [
        (OperationalError::Configuration(String::new()), 78),
        (OperationalError::Input(String::new()), 65),
        (OperationalError::TemporaryDelivery(String::new()), 75),
        (OperationalError::PermanentDestination(String::new()), 73),
        (OperationalError::Undelivered(String::new()), 79),
        (OperationalError::Internal(String::new()), 70),
    ];

    for (error, value) in cases {
        assert_eq!(error.exit_code(), value);
    }
    assert_eq!(ExitStatus::Success as u8, 0);
}

#[test]
fn default_rc_search_prefers_xdg_style_path_then_procmail_path() {
    let home = temporary_home();
    let modern = home.join(".config/procmail-rs/config");
    let compatible = home.join(".procmailrc");
    fs::create_dir_all(modern.parent().unwrap()).unwrap();
    fs::write(&modern, "MODERN=yes\n").unwrap();
    fs::write(&compatible, "COMPATIBLE=yes\n").unwrap();

    let (path, _, loaded) = load_root_config(None, home.to_str().unwrap()).unwrap();
    assert_eq!(path, modern);
    assert_eq!(loaded.source(), "MODERN=yes\n");

    fs::remove_file(&modern).unwrap();
    let (path, _, loaded) = load_root_config(None, home.to_str().unwrap()).unwrap();
    assert_eq!(path, compatible);
    assert_eq!(loaded.source(), "COMPATIBLE=yes\n");
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn explicit_rc_path_bypasses_default_search() {
    let home = temporary_home();
    let default = home.join(".procmailrc");
    let explicit = home.join("selected.rc");
    fs::write(&default, "DEFAULT=yes\n").unwrap();
    fs::write(&explicit, "SELECTED=yes\n").unwrap();

    let (path, _, loaded) = load_root_config(Some(&explicit), home.to_str().unwrap()).unwrap();
    assert_eq!(path, explicit);
    assert_eq!(loaded.source(), "SELECTED=yes\n");
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn missing_default_rc_reports_every_candidate() {
    let home = temporary_home();
    let error = load_root_config(None, home.to_str().unwrap()).unwrap_err();
    let message = error.to_string();

    assert!(message.contains(".config/procmail-rs/config"), "{message}");
    assert!(message.contains(".procmailrc"), "{message}");
    assert!(message.contains("--config PATH"), "{message}");
    fs::remove_dir_all(home).unwrap();
}

fn temporary_home() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "procmail-rs-config-search-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    path
}
