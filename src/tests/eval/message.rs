// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

fn edited_header<'a>(
    header: impl Into<HeaderView<'a>>,
    body_len: usize,
    value: &str,
) -> EditedHeader {
    let action = crate::config::HeaderAction {
        operations: vec![crate::config::HeaderOperation::Set {
            line: 1,
            name: "X-State".into(),
            value: crate::config::HeaderValue {
                source: value.into(),
                expansion: None,
            },
        }],
    };
    crate::header_edit::apply_header_action(header, body_len, &action, MessageLimits::default())
        .unwrap()
        .into_parts()
        .0
}

#[test]
fn header_versions_share_body_without_retaining_preceding_versions() {
    let original = Message::from_bytes(b"Subject: original\n\nbinary:\0\xff".to_vec());
    let original_view = PreparedMatchingMessage::new(&original, false);

    for filtered in [false, true] {
        let mut current = CurrentMessage::default();

        if filtered {
            current.replace(Message::from_bytes(
                b"Subject: filtered\n\nbinary:\0\xff".to_vec(),
            ));
        }

        let view = current.view(original_view.complete(&original));
        let body_address = view.body().unwrap().as_ptr();
        let edit = edited_header(view.header_view(), view.body().unwrap().len(), "first");
        current
            .replace_header(edit, original.body(), MessageLimits::default())
            .unwrap();
        let branch = current.clone();
        let old_version = Arc::downgrade(current.replacement.as_ref().unwrap());
        let view = current.view(original_view.complete(&original));
        let _ = view.full();
        let edit = edited_header(view.header_view(), view.body().unwrap().len(), "second");
        current
            .replace_header(edit, original.body(), MessageLimits::default())
            .unwrap();
        let view = current.view(original_view.complete(&original));
        assert_eq!(view.body().unwrap().as_ptr(), body_address);
        assert!(view.raw_header().ends_with(b"X-State: second\n\n"));
        assert!(
            branch
                .view(original_view.complete(&original))
                .raw_header()
                .ends_with(b"X-State: first\n\n")
        );
        let mut output = Vec::new();
        FinalMessage::new(view)
            .unwrap()
            .write_to(&mut output)
            .unwrap();
        assert!(output.ends_with(b"\n\nbinary:\0\xff"));
        assert!(
            current
                .replacement
                .as_ref()
                .unwrap()
                .matching
                .get()
                .is_none()
        );
        drop(branch);
        assert!(old_version.upgrade().is_none());
    }

    assert_eq!(original.as_bytes(), b"Subject: original\n\nbinary:\0\xff");
}

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
        FinalMessage::new(branch.view(prepared.complete(&original)))
            .unwrap()
            .bytes()
            .parts()
            .concat(),
        replacement
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
