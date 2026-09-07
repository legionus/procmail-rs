// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn resolves_current_process_identity() {
    let identity = UserIdentity::current().unwrap();

    assert!(!identity.logname().is_empty());
    assert!(!identity.home().is_empty());
}

#[test]
fn bounds_fields_before_searching_outside_the_buffer() {
    let buffer = b"user\0/home/user\0";
    let unterminated = b"unterminated";

    assert_eq!(
        copy_field(buffer, buffer.as_ptr(), "LOGNAME").unwrap(),
        "user"
    );
    assert!(copy_field(buffer, std::ptr::null(), "LOGNAME").is_err());
    assert!(copy_field(unterminated, unterminated.as_ptr(), "HOME").is_err());
}

#[test]
fn enforces_identity_field_size_limit() {
    let mut buffer = vec![b'x'; MAX_IDENTITY_FIELD_SIZE + 2];
    buffer[MAX_IDENTITY_FIELD_SIZE + 1] = 0;

    let error = copy_field(&buffer, buffer.as_ptr(), "HOME").unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("passwd HOME exceeds the hard limit of {MAX_IDENTITY_FIELD_SIZE} bytes")
    );
}
