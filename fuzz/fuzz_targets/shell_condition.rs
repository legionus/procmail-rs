// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(result) = procmail_rs::fuzzing::shell_condition(data) {
        assert!(result.output_len <= result.limit);
        assert!(
            result
                .assignment_lengths
                .iter()
                .all(|length| *length <= result.limit)
        );
    }
});
