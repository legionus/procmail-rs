// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::derive_implicit_lockfile_path;
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
