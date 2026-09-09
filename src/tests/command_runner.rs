// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn background_command_limit_checks_each_boundary() {
    for (count, accepted) in [
        (MAX_BACKGROUND_COMMANDS - 1, true),
        (MAX_BACKGROUND_COMMANDS, true),
        (MAX_BACKGROUND_COMMANDS + 1, false),
    ] {
        assert_eq!(
            check_background_command_count(count).is_ok(),
            accepted,
            "count {count}"
        );
    }
}
