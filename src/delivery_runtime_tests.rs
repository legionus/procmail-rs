// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::{PublicationTracker, derive_implicit_lockfile_path};
use procmail_rs::config;

#[test]
fn implicit_lockfile_path_enforces_the_complete_path_limit() {
    let at_limit = "x".repeat(config::MAX_PATH_EXPRESSION_LEN - 1);
    assert_eq!(
        derive_implicit_lockfile_path("d", &at_limit).unwrap().len(),
        config::MAX_PATH_EXPRESSION_LEN
    );

    let above_limit = "x".repeat(config::MAX_PATH_EXPRESSION_LEN);
    let error = derive_implicit_lockfile_path("d", &above_limit).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("implicit lockfile path exceeds the hard limit")
    );
    assert_eq!(
        derive_implicit_lockfile_path("mailbox", "").unwrap(),
        "mailbox"
    );
}

#[test]
fn publication_tracker_reports_copies_and_resets_after_completion() {
    let mut tracker = PublicationTracker::default();
    tracker.record(2, false).unwrap();
    let error = tracker.finish().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("published 2 copy destination(s)")
    );

    tracker.record(1, true).unwrap();
    assert!(tracker.finish().is_ok());
    assert_eq!(tracker.published, 0);
    assert!(!tracker.original_delivered);
}

#[test]
fn publication_tracker_rejects_count_overflow() {
    let mut tracker = PublicationTracker::default();
    tracker.record(usize::MAX, false).unwrap();
    let error = tracker.record(1, false).unwrap_err();
    assert!(error.to_string().contains("count overflows"));
}
