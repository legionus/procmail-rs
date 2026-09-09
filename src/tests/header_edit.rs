// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>
use std::io::{self, BufReader, Cursor, Read, Seek};

use super::*;
use crate::config::HeaderValue;
use crate::limits::MessageLimits;
use crate::message::Message;

fn value(source: &str) -> HeaderValue {
    HeaderValue {
        source: source.into(),
        expansion: None,
    }
}

fn action(operations: Vec<HeaderOperation>) -> HeaderAction {
    HeaderAction { operations }
}

fn apply(header: &[u8], body_len: usize, action: &HeaderAction) -> EditedHeader {
    apply_header_action(header, body_len, action, MessageLimits::default())
        .unwrap()
        .into_parts()
        .0
}

#[test]
fn operations_run_in_order_and_match_names_without_ascii_case() {
    let header = b"A: one\nX-Test: old\nX-Test: second\nZ: last\n\n";
    let action = action(vec![
        HeaderOperation::Set {
            line: 1,
            name: "x-test".into(),
            value: value("new"),
        },
        HeaderOperation::Add {
            line: 2,
            name: "X-Test".into(),
            value: value("added"),
        },
        HeaderOperation::Prepend {
            line: 3,
            name: "First".into(),
            value: value("yes"),
        },
    ]);

    assert_eq!(
        apply(header, 0, &action).as_bytes(),
        b"First: yes\nA: one\nx-test: new\nZ: last\nX-Test: added\n\n"
    );
}

#[test]
fn rename_preserves_values_folding_order_and_endings() {
    let header = b"Legacy: one\r\n\tcontinued\r\nKeep: exact\r\nlegacy: two\r\n\r\n";
    let action = action(vec![HeaderOperation::Rename {
        line: 1,
        from: "LEGACY".into(),
        to: "Current".into(),
    }]);

    assert_eq!(
        apply(header, 0, &action).as_bytes(),
        b"Current: one\r\n\tcontinued\r\nKeep: exact\r\nCurrent: two\r\n\r\n"
    );
}

#[test]
fn extraction_reads_the_first_field_after_preceding_edits() {
    let header = b"Subject:  first\r\n\tcontinued\r\nSubject: second\r\n\r\n";
    let action = action(vec![
        HeaderOperation::Rename {
            line: 1,
            from: "Subject".into(),
            to: "X-Subject".into(),
        },
        HeaderOperation::Extract {
            line: 2,
            name: "X-Subject".into(),
            target: "RAW".into(),
            mode: HeaderExtractionMode::Raw,
        },
        HeaderOperation::Extract {
            line: 3,
            name: "X-Subject".into(),
            target: "UNFOLDED".into(),
            mode: HeaderExtractionMode::Unfolded,
        },
        HeaderOperation::Extract {
            line: 4,
            name: "Missing".into(),
            target: "MISSING".into(),
            mode: HeaderExtractionMode::Raw,
        },
    ]);

    let (edited, extracted) = apply_header_action(header, 0, &action, MessageLimits::default())
        .unwrap()
        .into_parts();
    assert_eq!(
        edited.as_bytes(),
        b"X-Subject:  first\r\n\tcontinued\r\nX-Subject: second\r\n\r\n"
    );
    assert_eq!(
        extracted,
        [
            HeaderExtraction {
                line: 2,
                target: "RAW".into(),
                value: b"  first\r\n\tcontinued".to_vec(),
            },
            HeaderExtraction {
                line: 3,
                target: "UNFOLDED".into(),
                value: b"first continued".to_vec(),
            },
            HeaderExtraction {
                line: 4,
                target: "MISSING".into(),
                value: Vec::new(),
            },
        ]
    );
}

#[test]
fn extraction_preserves_arbitrary_value_bytes() {
    let action = action(vec![HeaderOperation::Extract {
        line: 1,
        name: "X-Binary".into(),
        target: "VALUE".into(),
        mode: HeaderExtractionMode::Unfolded,
    }]);
    let (_, extracted) = apply_header_action(
        b"X-Binary: \xff\0value\n\n",
        0,
        &action,
        MessageLimits::default(),
    )
    .unwrap()
    .into_parts();

    assert_eq!(extracted[0].value, b"\xff\0value");
}

#[test]
fn extraction_enforces_its_value_limit_at_the_boundary() {
    for mode in [HeaderExtractionMode::Raw, HeaderExtractionMode::Unfolded] {
        for length in [
            MAX_ASSIGNMENT_VALUE_LEN - 1,
            MAX_ASSIGNMENT_VALUE_LEN,
            MAX_ASSIGNMENT_VALUE_LEN + 1,
        ] {
            let mut header = b"X:".to_vec();
            header.extend(std::iter::repeat_n(b'x', length));
            header.extend_from_slice(b"\n\n");
            let action = action(vec![HeaderOperation::Extract {
                line: 1,
                name: "X".into(),
                target: "VALUE".into(),
                mode,
            }]);
            let input_limit = MAX_ASSIGNMENT_VALUE_LEN + 1024;
            let limits = MessageLimits {
                message_size: input_limit,
                headers_size: input_limit,
                header_line_size: input_limit,
                header_field_size: input_limit,
                ..MessageLimits::default()
            };
            let result = apply_header_action(&header, 0, &action, limits);
            if length <= MAX_ASSIGNMENT_VALUE_LEN {
                assert_eq!(result.unwrap().into_parts().1[0].value.len(), length);
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    HeaderEditError::ExtractedValueTooLong {
                        limit: MAX_ASSIGNMENT_VALUE_LEN,
                    }
                );
            }
        }
    }
}

#[test]
fn remove_deletes_a_complete_folded_field() {
    let header = b"Keep: one\r\nFolded: first\r\n\tsecond\r\nkeep: two\r\n\r\n";
    let action = action(vec![HeaderOperation::Remove {
        line: 1,
        name: "FOLDED".into(),
    }]);

    assert_eq!(
        apply(header, 0, &action).as_bytes(),
        b"Keep: one\r\nkeep: two\r\n\r\n"
    );
}

#[test]
fn set_appends_when_the_field_is_absent() {
    let header = b"A: one\n\n";
    let action = action(vec![HeaderOperation::Set {
        line: 1,
        name: "B".into(),
        value: value("two"),
    }]);

    assert_eq!(apply(header, 0, &action).as_bytes(), b"A: one\nB: two\n\n");
}

#[test]
fn buffered_edit_preserves_body_and_preceding_message() {
    let original = Message::from_bytes(b"A: old\nKeep: exact\n\nbinary:\xff\0body".to_vec());
    let action = action(vec![HeaderOperation::Set {
        line: 1,
        name: "A".into(),
        value: value("new"),
    }]);
    let edited = apply(original.header(), original.body().len(), &action);
    let replacement = original.with_edited_header(edited).unwrap();

    assert_eq!(replacement.header(), b"A: new\nKeep: exact\n\n");
    assert_eq!(replacement.body(), b"binary:\xff\0body");
    assert_eq!(
        original.as_bytes(),
        b"A: old\nKeep: exact\n\nbinary:\xff\0body"
    );
}

#[test]
fn edit_preserves_unrecognized_binary_header_fields() {
    let header = b"\xffBroken\0field\n continuation\nGood: old\n\n";
    let action = action(vec![HeaderOperation::Set {
        line: 1,
        name: "Good".into(),
        value: value("new"),
    }]);

    assert_eq!(
        apply(header, 0, &action).as_bytes(),
        b"\xffBroken\0field\n continuation\nGood: new\n\n"
    );
}

#[test]
fn header_phase_edit_does_not_consume_body() {
    let input = b"A: old\n\nbody remains unread";
    let mut reader = BufReader::with_capacity(1, Cursor::new(input));
    let head = Message::read_headers(&mut reader, MessageLimits::default()).unwrap();
    let body_position = reader.stream_position().unwrap();
    let action = action(vec![HeaderOperation::Set {
        line: 1,
        name: "A".into(),
        value: value("new"),
    }]);

    let edited = apply(head.as_bytes(), 0, &action);
    let mut head = head;
    head.replace_edited_header(edited);
    assert_eq!(reader.stream_position().unwrap(), body_position);

    let message = head.read_body(&mut reader).unwrap();
    assert_eq!(message.header(), b"A: new\n\n");
    assert_eq!(message.body(), b"body remains unread");
}

#[test]
fn edited_header_streams_a_large_synthetic_body_without_retaining_it() {
    const BODY_LEN: usize = 64 * 1024 * 1024;

    let limits = MessageLimits {
        message_size: BODY_LEN + 64,
        body_size: BODY_LEN,
        ..MessageLimits::default()
    };
    let mut header_reader = Cursor::new(b"A: old\n\n");
    let mut head = Message::read_headers(&mut header_reader, limits).unwrap();
    let action = action(vec![HeaderOperation::Set {
        line: 1,
        name: "A".into(),
        value: value("new"),
    }]);
    let edited = apply_header_action(head.as_bytes(), 0, &action, limits)
        .unwrap()
        .into_parts()
        .0;
    head.replace_edited_header(edited);

    // Generate the body on demand so the fixture itself does not allocate
    // in proportion to the input and the returned streaming summary has
    // no place to retain body bytes.
    let mut body = BufReader::with_capacity(8192, io::repeat(b'x').take(BODY_LEN as u64));
    let streamed = head.stream_to(&mut body, &mut io::sink()).unwrap();

    assert_eq!(streamed.header(), b"A: new\n\n");
    assert_eq!(streamed.len(), BODY_LEN + b"A: new\n\n".len());
}

#[test]
fn rechecks_each_edited_message_limit_at_its_boundary() {
    let empty = action(Vec::new());

    for size in [7usize, 8, 9] {
        let limits = MessageLimits {
            headers_size: 8,
            ..MessageLimits::default()
        };
        let header = vec![b'x'; size];
        let result = apply_header_action(&header, 0, &empty, limits);
        assert_limit_result(result, size, 8, MessageLimit::Headers);
    }

    for body_len in [7usize, 8, 9] {
        let limits = MessageLimits {
            message_size: 12,
            ..MessageLimits::default()
        };
        let result = apply_header_action(b"A:\n\n", body_len, &empty, limits);
        assert_limit_result(result, body_len + 4, 12, MessageLimit::Message);
    }

    for line_len in [7usize, 8, 9] {
        let limits = MessageLimits {
            header_line_size: 8,
            ..MessageLimits::default()
        };
        let mut header = b"A:".to_vec();
        header.extend(std::iter::repeat_n(b'x', line_len - 3));
        header.extend_from_slice(b"\n\n");
        let result = apply_header_action(&header, 0, &empty, limits);
        assert_limit_result(result, line_len, 8, MessageLimit::HeaderLine);
    }

    for field_len in [7usize, 8, 9] {
        let limits = MessageLimits {
            header_field_size: 8,
            ..MessageLimits::default()
        };
        let mut header = b"A:\n ".to_vec();
        header.extend(std::iter::repeat_n(b'x', field_len - 5));
        header.extend_from_slice(b"\n\n");
        let result = apply_header_action(&header, 0, &empty, limits);
        assert_limit_result(result, field_len, 8, MessageLimit::HeaderField);
    }
}

#[test]
fn growth_limit_failure_keeps_the_input_header_unchanged() {
    let header = b"A: one\n\n".to_vec();
    let action = action(vec![HeaderOperation::Add {
        line: 1,
        name: "B".into(),
        value: value("two"),
    }]);
    let limits = MessageLimits {
        headers_size: header.len(),
        ..MessageLimits::default()
    };

    let error = apply_header_action(&header, 0, &action, limits).unwrap_err();
    assert_eq!(
        error,
        HeaderEditError::LimitExceeded {
            kind: MessageLimit::Headers,
            limit: header.len(),
        }
    );
    assert_eq!(header, b"A: one\n\n");
}

fn assert_limit_result(
    result: Result<AppliedHeaderAction, HeaderEditError>,
    size: usize,
    limit: usize,
    kind: MessageLimit,
) {
    if size <= limit {
        assert!(result.is_ok());
    } else {
        assert_eq!(
            result.unwrap_err(),
            HeaderEditError::LimitExceeded { kind, limit }
        );
    }
}
