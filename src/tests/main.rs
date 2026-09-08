// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::{ExitStatus, OperationalError};

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
