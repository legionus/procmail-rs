// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn accepts_growth_at_the_limit() {
    let mut bytes = BoundedBytes::with_capacity(3, 1);

    bytes.try_extend(b"ab").unwrap();
    assert_eq!(bytes.remaining(), Ok(1));
    bytes.try_extend(b"c").unwrap();

    assert_eq!(bytes.into_vec(), b"abc");
}

#[test]
fn rejects_growth_beyond_the_limit_without_partial_output() {
    let mut bytes = BoundedBytes::with_capacity(2, 0);
    bytes.try_extend(b"a").unwrap();

    assert_eq!(
        bytes.try_extend(b"bc"),
        Err(BoundedBytesError::LimitExceeded { attempted: 3 })
    );
    assert_eq!(bytes.into_vec(), b"a");
}
