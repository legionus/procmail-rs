// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn enforces_each_argument_length_at_the_boundary() {
    for (size, accepted) in [
        (MAX_POSITIONAL_ARGUMENT_LEN - 1, true),
        (MAX_POSITIONAL_ARGUMENT_LEN, true),
        (MAX_POSITIONAL_ARGUMENT_LEN + 1, false),
    ] {
        let mut arguments = PositionalArguments::default();
        assert_eq!(arguments.push("x".repeat(size)).is_ok(), accepted);
    }
}

#[test]
fn enforces_argument_count_at_the_boundary() {
    for count in [
        MAX_POSITIONAL_ARGUMENTS - 1,
        MAX_POSITIONAL_ARGUMENTS,
        MAX_POSITIONAL_ARGUMENTS + 1,
    ] {
        let mut arguments = PositionalArguments::default();
        let mut accepted = 0;
        for _ in 0..count {
            if arguments.push(String::new()).is_ok() {
                accepted += 1;
            }
        }
        assert_eq!(accepted, count.min(MAX_POSITIONAL_ARGUMENTS));
        assert_eq!(arguments.len(), count.min(MAX_POSITIONAL_ARGUMENTS));
    }
}

#[test]
fn enforces_aggregate_argument_bytes_at_the_boundary() {
    for (size, accepted) in [
        (MAX_POSITIONAL_ARGUMENT_BYTES - 1, true),
        (MAX_POSITIONAL_ARGUMENT_BYTES, true),
        (MAX_POSITIONAL_ARGUMENT_BYTES + 1, false),
    ] {
        let mut arguments = PositionalArguments::default();
        let chunks = size / MAX_POSITIONAL_ARGUMENT_LEN;
        let remainder = size % MAX_POSITIONAL_ARGUMENT_LEN;
        let mut result = Ok(());
        for _ in 0..chunks {
            result = arguments.push("x".repeat(MAX_POSITIONAL_ARGUMENT_LEN));
            if result.is_err() {
                break;
            }
        }
        if result.is_ok() && remainder != 0 {
            result = arguments.push("x".repeat(remainder));
        }
        assert_eq!(result.is_ok(), accepted, "aggregate size: {size}");
    }
}
