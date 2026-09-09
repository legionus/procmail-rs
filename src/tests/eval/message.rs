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

#[test]
fn current_message_clones_share_the_complete_replacement() {
    let original = Message::from_bytes(b"Subject: original\n\nbody".to_vec());
    let prepared = PreparedMatchingMessage::new(&original, true);
    let mut current = CurrentMessage::default();
    let replacement = b"X-State: replaced\n\nlarge body".repeat(1024);
    current.replace(Message::from_bytes(replacement.clone()));

    let branch = current.clone();

    assert!(current.shares_replacement_with(&branch));
    assert_eq!(
        branch.view(prepared.complete(&original)).raw(),
        Some(replacement.as_slice())
    );
}
