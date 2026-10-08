// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn fuzz_regression_lone_cr_changes_the_next_actions_preferred_ending() {
    // Exact message bytes from crash-00f0ecf13354884c55da3a0c8c0fc36dcbe666bf.
    let operations = action(vec![HeaderOperation::Add {
        line: 1,
        name: "(".into(),
        value: value(""),
    }]);
    let first = apply(b"I\xff\xff\xfb!\r", 0, &operations);
    assert_eq!(first.as_bytes(), b"I\xff\xff\xfb!\r\r\n(: \n");
    let second = apply_header_action(
        HeaderView::Indexed(&first),
        0,
        &operations,
        MessageLimits::default(),
    )
    .unwrap()
    .into_parts()
    .0;
    assert_eq!(second.as_bytes(), b"I\xff\xff\xfb!\r\r\n(: \n(: \r\n");
}

#[test]
fn removing_first_field_updates_the_preferred_ending_for_later_actions() {
    let first = apply(
        b"A: one\nB: two\r\n\r\n",
        0,
        &action(vec![HeaderOperation::Remove {
            line: 1,
            name: "A".into(),
        }]),
    );
    let second = update(
        &first,
        vec![HeaderOperation::Add {
            line: 1,
            name: "C".into(),
            value: value("three"),
        }],
    );
    assert_eq!(second.as_bytes(), b"B: two\r\nC: three\r\n\r\n");
}

#[test]
fn refreshed_preference_does_not_change_a_previously_inserted_delimiter() {
    let first = apply(
        b"A: first\nB: two\r\nC: unterminated",
        0,
        &action(vec![
            HeaderOperation::Remove {
                line: 1,
                name: "A".into(),
            },
            HeaderOperation::Add {
                line: 2,
                name: "D".into(),
                value: value("new"),
            },
        ]),
    );
    let expected = b"B: two\r\nC: unterminated\nD: new\n";
    assert_eq!(first.len(), expected.len());
    assert_eq!(first.as_bytes(), expected);
    first
        .validate(
            0,
            MessageLimits {
                headers_size: expected.len(),
                ..MessageLimits::default()
            },
        )
        .unwrap();
    let second = update(
        &first,
        vec![HeaderOperation::Add {
            line: 1,
            name: "E".into(),
            value: value("next"),
        }],
    );
    assert_eq!(
        second.as_bytes(),
        b"B: two\r\nC: unterminated\nD: new\nE: next\r\n"
    );
}

fn update(header: &EditedHeader, operations: Vec<HeaderOperation>) -> EditedHeader {
    apply_header_action(
        HeaderView::Indexed(header),
        0,
        &action(operations),
        MessageLimits::default(),
    )
    .unwrap()
    .into_parts()
    .0
}

#[test]
fn separate_actions_reuse_holes_without_serializing_or_reparsing() {
    let first = apply(
        b"From: a@example.org\nCc: old@example.org\nTo: b@example.org\n\n",
        0,
        &action(Vec::new()),
    );
    let removed = update(
        &first,
        vec![HeaderOperation::Remove {
            line: 1,
            name: "Cc".into(),
        }],
    );
    let offset = removed.storage.store.free[0].start;
    let added = update(
        &removed,
        vec![HeaderOperation::Add {
            line: 1,
            name: "Cc".into(),
            value: value("new@example.org"),
        }],
    );
    assert_eq!(added.storage.store.fields[2].raw.start, offset);
    assert_eq!(
        added.storage.store.bytes.len(),
        first.storage.store.bytes.len()
    );
    assert!(added.storage.store.free.is_empty());
    let matched = crate::structured_header::any_address(
        HeaderView::Indexed(&added),
        &[crate::config::AddressField::Cc],
        |address| Ok::<_, ()>(address == b"new@example.org"),
    )
    .unwrap();
    assert!(matched);
    assert!(added.storage.raw.get().is_none());
    assert!(added.storage.matching.get().is_none());
    assert!(first.storage.raw.get().is_none());
}

#[test]
fn no_op_and_extraction_share_storage_and_prepared_views() {
    let first = apply(b"X: same\nSubject: one\n two\n\n", 0, &action(Vec::new()));
    let matching = first.matching_header().as_ptr();
    let applied = apply_header_action(
        HeaderView::Indexed(&first),
        0,
        &action(vec![
            HeaderOperation::Remove {
                line: 1,
                name: "Missing".into(),
            },
            HeaderOperation::Set {
                line: 2,
                name: "X".into(),
                value: value("same"),
            },
            HeaderOperation::Rename {
                line: 3,
                from: "X".into(),
                to: "x".into(),
            },
            HeaderOperation::Extract {
                line: 4,
                name: "Subject".into(),
                target: "S".into(),
                mode: HeaderExtractionMode::Unfolded,
            },
        ]),
        MessageLimits::default(),
    )
    .unwrap();
    assert!(!applied.changed_from(HeaderView::Indexed(&first)));
    let (second, extracted) = applied.into_parts();
    assert!(Arc::ptr_eq(&first.storage, &second.storage));
    assert_eq!(matching, second.matching_header().as_ptr());
    assert_eq!(extracted[0].value, b"one two");
}

#[test]
fn changed_branch_has_private_arena_and_empty_caches() {
    let first = apply(b"Subject: one\n two\n\n", 0, &action(Vec::new()));
    let _ = first.matching_header();
    let branch = first.clone();
    let changed = update(
        &first,
        vec![HeaderOperation::Set {
            line: 1,
            name: "Subject".into(),
            value: value("changed"),
        }],
    );
    assert!(!Arc::ptr_eq(&first.storage, &changed.storage));
    assert!(Arc::ptr_eq(&first.storage, &branch.storage));
    assert!(changed.storage.raw.get().is_none());
    assert!(changed.storage.matching.get().is_none());
    assert_eq!(branch.as_bytes(), b"Subject: one\n two\n\n");
    assert_eq!(changed.matching_header(), b"Subject: changed\n\n");
}

#[test]
fn failed_transaction_preserves_fields_holes_and_caches() {
    let first = apply(b"X: old\n\n", 0, &action(Vec::new()));
    let bytes = first.as_bytes().as_ptr();
    let limits = MessageLimits {
        headers_size: first.len(),
        ..MessageLimits::default()
    };
    let failed = apply_header_action(
        HeaderView::Indexed(&first),
        0,
        &action(vec![
            HeaderOperation::Extract {
                line: 1,
                name: "X".into(),
                target: "V".into(),
                mode: HeaderExtractionMode::Raw,
            },
            HeaderOperation::Add {
                line: 2,
                name: "Long".into(),
                value: value("too large"),
            },
        ]),
        limits,
    );
    assert!(matches!(
        failed,
        Err(HeaderEditError::LimitExceeded {
            kind: MessageLimit::Headers,
            ..
        })
    ));
    assert_eq!(first.as_bytes(), b"X: old\n\n");
    assert_eq!(first.as_bytes().as_ptr(), bytes);
    assert!(first.storage.store.free.is_empty());
    assert_eq!(first.storage.store.fields.len(), 1);
}

#[test]
fn prepending_before_orphan_continuation_reindexes_the_joined_field() {
    let first = apply(
        b" continuation\nTo: reader@example.org\n\n",
        0,
        &action(Vec::new()),
    );
    let next = update(
        &first,
        vec![HeaderOperation::Prepend {
            line: 1,
            name: "Subject".into(),
            value: value("new"),
        }],
    );
    let fields: Vec<_> = HeaderView::Indexed(&next).fields().collect();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].bytes, b"Subject: new\n continuation\n");
    let (_, extraction) = apply_header_action(
        HeaderView::Indexed(&next),
        0,
        &action(vec![HeaderOperation::Extract {
            line: 1,
            name: "Subject".into(),
            target: "S".into(),
            mode: HeaderExtractionMode::Unfolded,
        }]),
        MessageLimits::default(),
    )
    .unwrap()
    .into_parts();
    assert_eq!(extraction[0].value, b"new continuation");
    assert_eq!(
        next.as_bytes(),
        b"Subject: new\n continuation\nTo: reader@example.org\n\n"
    );
}

#[test]
fn joined_orphan_continuation_checks_logical_field_limit_before_publication() {
    let first = apply(b" continuation\n\n", 0, &action(Vec::new()));
    let operations = action(vec![HeaderOperation::Prepend {
        line: 1,
        name: "Subject".into(),
        value: value("new"),
    }]);
    let size = b"Subject: new\n continuation\n".len();

    for limit in [size - 1, size, size + 1] {
        let limits = MessageLimits {
            header_field_size: limit,
            ..MessageLimits::default()
        };
        let result = apply_header_action(HeaderView::Indexed(&first), 0, &operations, limits);
        assert_eq!(result.is_ok(), limit >= size);
    }

    assert_eq!(first.as_bytes(), b" continuation\n\n");
}

#[test]
fn raw_and_indexed_field_iteration_agree_on_binary_and_folded_input() {
    for bytes in [
        b"From: a@example.org\r\n\tmore\r\nFrom: b@example.org\r\n\r\n".as_slice(),
        b"\xffbroken\0\n continuation\nCc: c@example.org\n\n",
        b" orphan\r\nSubject: unterminated\r",
        b"\n",
    ] {
        let indexed = apply(bytes, 0, &action(Vec::new()));
        let raw: Vec<_> = HeaderView::Raw(bytes)
            .fields()
            .map(|field| (field.name, field.bytes, field.value()))
            .collect();
        let stored: Vec<_> = HeaderView::Indexed(&indexed)
            .fields()
            .map(|field| (field.name, field.bytes, field.value()))
            .collect();
        assert_eq!(raw, stored);
        assert!(indexed.storage.raw.get().is_none());
    }
}
