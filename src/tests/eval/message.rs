// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn prepared_matching_message_normalizes_only_matching_views() {
    let message = Message::from_bytes(b"Subject: one\n two\n\nbody".to_vec());
    let prepared = PreparedMatchingMessage::new(&message, true);
    let (header, full) = prepared.views(&message).into_parts();

    assert_eq!(header, b"Subject: one  two\n\n");
    assert_eq!(full, Some(&b"Subject: one  two\n\nbody"[..]));
    assert_eq!(message.as_bytes(), b"Subject: one\n two\n\nbody");
}

#[test]
fn prepared_matching_message_skips_unused_complete_view() {
    let message = Message::from_bytes(b"Subject: one\n two\n\nbody".to_vec());
    let prepared = PreparedMatchingMessage::new(&message, false);
    let (_, full) = prepared.views(&message).into_parts();

    assert_eq!(full, None);
}
