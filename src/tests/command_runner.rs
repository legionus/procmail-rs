// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn background_command_limit_checks_each_boundary() {
    let budget = BackgroundCommandBudget::default();

    for _ in 0..MAX_BACKGROUND_COMMANDS - 1 {
        budget.reserve().unwrap();
    }
    assert_eq!(
        budget.started.load(Ordering::Relaxed),
        MAX_BACKGROUND_COMMANDS - 1
    );
    budget.reserve().unwrap();
    assert_eq!(
        budget.started.load(Ordering::Relaxed),
        MAX_BACKGROUND_COMMANDS
    );
    assert!(budget.reserve().is_err());
}
