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

#[test]
fn replacement_matching_is_lazy_reused_and_reset_by_replacement() {
    let original = Message::from_bytes(b"Subject: original\n\nbody".to_vec());
    let prepared = PreparedMatchingMessage::new(&original, true);
    let mut current = CurrentMessage::default();
    current.replace(Message::from_bytes(b"Subject: one\n two\n\nbody".to_vec()));
    let branch = current.clone();

    assert_eq!(
        current.view(prepared.complete(&original)).header_bytes(),
        b"Subject: one  two\n\n"
    );
    assert!(
        current
            .replacement
            .as_ref()
            .unwrap()
            .matching
            .get()
            .is_none()
    );

    let first = current.view(prepared.complete(&original)).full().unwrap();
    assert_eq!(first, b"Subject: one  two\n\nbody");
    let shared = branch.view(prepared.complete(&original)).full().unwrap();
    assert_eq!(first.as_ptr(), shared.as_ptr());

    current.replace(Message::from_bytes(
        b"Subject: changed\n again\n\nnew".to_vec(),
    ));
    assert!(
        current
            .replacement
            .as_ref()
            .unwrap()
            .matching
            .get()
            .is_none()
    );
    assert_eq!(
        current.view(prepared.complete(&original)).full().unwrap(),
        b"Subject: changed  again\n\nnew"
    );
    assert_eq!(
        branch.view(prepared.complete(&original)).full().unwrap(),
        b"Subject: one  two\n\nbody"
    );
}

#[test]
fn concurrent_copy_branches_share_one_normalized_full_view() {
    let original = Message::from_bytes(b"Subject: original\n\nbody".to_vec());
    let prepared = PreparedMatchingMessage::new(&original, true);
    let mut current = CurrentMessage::default();
    current.replace(Message::from_bytes(b"Subject: one\n two\n\nbody".to_vec()));
    let barrier = std::sync::Barrier::new(4);

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let branch = current.clone();
                let original = prepared.complete(&original);
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    branch.view(original).full().unwrap().as_ptr() as usize
                })
            })
            .collect();
        let addresses: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(addresses.iter().all(|address| *address == addresses[0]));
    });
}
