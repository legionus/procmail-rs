// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn reads_a_bounded_utf8_hostname() {
    let hostname = current_hostname().unwrap();

    assert!(!hostname.is_empty());
    assert!(hostname.len() <= MAX_HOSTNAME_LEN);
    assert!(!hostname.as_bytes().contains(&0));
}

#[test]
fn validates_hostname_bytes_at_every_length_boundary() {
    for length in [MAX_HOSTNAME_LEN - 1, MAX_HOSTNAME_LEN] {
        let hostname = hostname_from_bytes(&vec![b'h'; length]).unwrap();
        assert_eq!(hostname.len(), length);
    }
    let error = hostname_from_bytes(&vec![b'h'; MAX_HOSTNAME_LEN + 1]).unwrap_err();
    assert!(error.to_string().contains("exceeds the hard limit"));

    assert!(hostname_from_bytes(b"").is_err());
    assert!(hostname_from_bytes(b"host\xff").is_err());
}
