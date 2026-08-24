// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::{ExitStatus, OperationalError, config, derive_implicit_lockfile_path};

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
fn operational_errors_have_distinct_stable_exit_statuses() {
    let cases = [
        (
            OperationalError::Configuration(String::new()),
            ExitStatus::Configuration,
            78,
        ),
        (
            OperationalError::Input(String::new()),
            ExitStatus::Input,
            65,
        ),
        (
            OperationalError::TemporaryDelivery(String::new()),
            ExitStatus::TemporaryDelivery,
            75,
        ),
        (
            OperationalError::PermanentDestination(String::new()),
            ExitStatus::PermanentDestination,
            73,
        ),
        (
            OperationalError::Undelivered(String::new()),
            ExitStatus::Undelivered,
            79,
        ),
        (
            OperationalError::Internal(String::new()),
            ExitStatus::Internal,
            70,
        ),
    ];

    for (error, status, value) in cases {
        assert_eq!(error.exit_status(), status);
        assert_eq!(status as u8, value);
    }
    assert_eq!(ExitStatus::Success as u8, 0);
}
