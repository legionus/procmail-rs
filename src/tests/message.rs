// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io::{self, BufReader, Cursor, Read, Write};

use super::{Message, MessageLimit, MessageReadError};
use crate::config::ActionInput;
use crate::limits::MessageLimits;

#[test]
fn segmented_writes_propagate_failures_after_short_writes() {
    struct FailsAfter {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for FailsAfter {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            if self.bytes.len() == self.limit {
                return Err(io::Error::other("injected write failure"));
            }

            let count = input.len().min(self.limit - self.bytes.len());
            self.bytes.extend_from_slice(&input[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    for limit in [3, 4, 5] {
        let mut writer = FailsAfter {
            bytes: Vec::new(),
            limit,
        };
        let error = super::MessageBytes::new(b"head", b"body")
            .write_to(&mut writer)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(writer.bytes, b"headbody"[..limit]);
    }
}

#[test]
fn segmented_bytes_preserve_every_split_and_suffix() {
    let bytes = b"Subject: test\n\nFrom body\0\xff\n\n";

    for split in 0..=bytes.len() {
        let input = super::MessageBytes::new(&bytes[..split], &bytes[split..]);
        let mut output = Vec::new();
        input.write_to(&mut output).unwrap();
        assert_eq!(output, bytes);
        let mut read = Vec::new();
        std::io::Read::read_to_end(&mut input.reader(), &mut read).unwrap();
        assert_eq!(read, bytes);

        for suffix in [
            &b""[..],
            &b"\n"[..],
            &b"\n\n"[..],
            &b"\xff\n\n"[..],
            &b"wrong"[..],
            bytes,
        ] {
            assert_eq!(
                input.ends_with(suffix),
                bytes.ends_with(suffix),
                "split {split}"
            );
        }
    }
}

#[test]
fn full_filter_output_retains_the_owned_allocation_and_validates_new_limits() {
    let raw = b"Subject: one\n two\n\nbody";
    let output = Message::read_from(&mut Cursor::new(raw), MessageLimits::default()).unwrap();
    let address = output.as_bytes().as_ptr();
    let replacement = Message::from_filter_output(
        b"old\n\n",
        b"old body",
        output,
        ActionInput::Message,
        MessageLimits::default(),
    )
    .unwrap();
    assert_eq!(replacement.as_bytes().as_ptr(), address);
    assert_eq!(replacement.as_bytes(), raw);
    assert_eq!(replacement.matching_header(), b"Subject: one  two\n\n");

    for limit in [raw.len() - 1, raw.len(), raw.len() + 1] {
        let limits = MessageLimits {
            message_size: limit,
            ..MessageLimits::default()
        };
        let result = Message::from_filter_output(
            b"old\n\n",
            b"old body",
            replacement.clone(),
            ActionInput::Message,
            limits,
        );
        assert_eq!(result.is_ok(), limit >= raw.len());
    }
}

#[test]
fn owned_full_filter_output_enforces_every_ingestion_limit() {
    let raw = b"Subject: one\n two\nX: y\n\nbody";
    let output = Message::read_from(&mut Cursor::new(raw), MessageLimits::default()).unwrap();

    // The owned fast path must reject the same bytes as bounded ingestion,
    // including physical lines and folded fields, even when a caller lowers
    // its limits after the output was initially accepted.
    for limit in 0..=raw.len() + 1 {
        let defaults = MessageLimits::default();

        for limits in [
            MessageLimits {
                message_size: limit,
                ..defaults
            },
            MessageLimits {
                headers_size: limit,
                ..defaults
            },
            MessageLimits {
                body_size: limit,
                ..defaults
            },
            MessageLimits {
                header_line_size: limit,
                ..defaults
            },
            MessageLimits {
                header_field_size: limit,
                ..defaults
            },
        ] {
            let expected = Message::read_from(&mut Cursor::new(raw), limits);
            let actual = Message::from_filter_output(
                b"old\n\n",
                b"old",
                output.clone(),
                ActionInput::Message,
                limits,
            );
            assert_eq!(actual.is_ok(), expected.is_ok(), "{limits:?}");

            if let (Err(actual), Err(expected)) = (actual, expected) {
                assert_eq!(actual.to_string(), expected.to_string());
            }
        }
    }
}

#[test]
fn partial_filter_output_preserves_the_unselected_area() {
    let header_output = Message::from_bytes(b"Subject: new\n\n".to_vec());
    let body_output = Message::from_bytes(b"\nnew body".to_vec());

    let replaced_header = Message::from_filter_output(
        b"Subject: old\n\n",
        b"old body",
        header_output,
        ActionInput::Headers,
        MessageLimits::default(),
    )
    .unwrap();
    let replaced_body = Message::from_filter_output(
        b"Subject: old\n\n",
        b"old body",
        body_output,
        ActionInput::Body,
        MessageLimits::default(),
    )
    .unwrap();

    assert_eq!(replaced_header.as_bytes(), b"Subject: new\n\nold body");
    assert_eq!(replaced_body.as_bytes(), b"Subject: old\n\nnew body");
}

#[test]
fn partial_filter_output_rechecks_the_combined_message_limit() {
    let output = Message::from_bytes(b"\n1234".to_vec());
    let limits = MessageLimits {
        message_size: 8,
        body_size: 4,
        ..MessageLimits::default()
    };

    assert!(
        Message::from_filter_output(b"X:\n\n", b"old", output.clone(), ActionInput::Body, limits)
            .is_ok()
    );

    let limits = MessageLimits {
        message_size: 7,
        ..limits
    };
    let error = Message::from_filter_output(b"X:\n\n", b"old", output, ActionInput::Body, limits)
        .unwrap_err();
    assert!(matches!(
        error,
        MessageReadError::LimitExceeded {
            kind: MessageLimit::Message,
            limit: 7
        }
    ));
}

#[test]
fn splits_lf_message() {
    let message = Message::from_bytes(b"Subject: test\n\nbody\n".to_vec());

    assert_eq!(message.header(), b"Subject: test\n\n");
    assert_eq!(message.body(), b"body\n");
}

#[test]
fn splits_crlf_message() {
    let message = Message::from_bytes(b"Subject: test\r\n\r\nbody\r\n".to_vec());

    assert_eq!(message.header(), b"Subject: test\r\n\r\n");
    assert_eq!(message.body(), b"body\r\n");
}

#[test]
fn preserves_binary_input() {
    let raw = b"X-Binary: yes\n\n\0\xff\n".to_vec();
    let message = Message::from_bytes(raw.clone());

    assert_eq!(message.as_bytes(), raw);
    assert_eq!(message.body(), b"\0\xff\n");
}

#[test]
fn treats_message_without_separator_as_headers() {
    let message = Message::from_bytes(b"Subject: test\n".to_vec());

    assert_eq!(message.header(), b"Subject: test\n");
    assert!(message.body().is_empty());
}

#[test]
fn normalizes_lf_and_crlf_continuations_only_for_matching() {
    for (raw, matching) in [
        (
            &b"Subject: alpha\n beta\n\tmore\nX-Test: value\n\nbody"[..],
            &b"Subject: alpha  beta \tmore\nX-Test: value\n\n"[..],
        ),
        (
            &b"Subject: alpha\r\n beta\r\nX-Test: value\r\n\r\nbody"[..],
            &b"Subject: alpha  beta\r\nX-Test: value\r\n\r\n"[..],
        ),
    ] {
        let mut reader = Cursor::new(raw);
        let head = Message::read_headers(&mut reader, MessageLimits::default()).unwrap();

        assert_eq!(head.matching_header(), matching);
        assert_eq!(head.as_bytes(), &raw[..head.len()]);
    }
}

#[test]
fn does_not_allocate_a_matching_header_without_folding() {
    let raw = b"Subject: alpha\nX-Test: value\n\nbody";
    let mut reader = Cursor::new(raw);
    let head = Message::read_headers(&mut reader, MessageLimits::default()).unwrap();

    assert!(head.matching_header.is_none());
    assert_eq!(head.matching_header(), b"Subject: alpha\nX-Test: value\n\n");
}

fn read_with(raw: &[u8], limits: MessageLimits) -> Result<Message, MessageReadError> {
    Message::read_from(&mut Cursor::new(raw), limits)
}

fn limit_kind(error: MessageReadError) -> MessageLimit {
    let MessageReadError::LimitExceeded { kind, .. } = error else {
        panic!("expected limit error: {error}");
    };
    kind
}

#[test]
fn accepts_values_exactly_at_limits() {
    let raw = b"X: a\n\nbody";
    let limits = MessageLimits {
        message_size: raw.len(),
        headers_size: 6,
        body_size: 4,
        header_line_size: 5,
        header_field_size: 5,
    };

    assert_eq!(read_with(raw, limits).unwrap().as_bytes(), raw);
}

#[test]
fn rejects_oversized_header_section_before_body() {
    let limits = MessageLimits {
        headers_size: 5,
        ..MessageLimits::default()
    };

    assert_eq!(
        limit_kind(read_with(b"X: a\n\n", limits).unwrap_err()),
        MessageLimit::Headers
    );
}

#[test]
fn rejects_header_line_without_waiting_for_newline() {
    let limits = MessageLimits {
        header_line_size: 8,
        ..MessageLimits::default()
    };

    assert_eq!(
        limit_kind(read_with(b"X-Long: never ends", limits).unwrap_err()),
        MessageLimit::HeaderLine
    );
}

#[test]
fn counts_folded_lines_as_one_header_field() {
    let limits = MessageLimits {
        header_field_size: 10,
        ..MessageLimits::default()
    };

    assert_eq!(
        limit_kind(read_with(b"X: a\n b\n c\n\n", limits).unwrap_err()),
        MessageLimit::HeaderField
    );
}

#[test]
fn rejects_oversized_body() {
    let limits = MessageLimits {
        body_size: 3,
        ..MessageLimits::default()
    };

    assert_eq!(
        limit_kind(read_with(b"\nbody", limits).unwrap_err()),
        MessageLimit::Body
    );
}

#[test]
fn stops_reading_an_infinite_body_at_total_limit() {
    let input = Cursor::new(b"\n".to_vec()).chain(io::repeat(b'x'));
    let mut reader = BufReader::with_capacity(8, input);
    let limits = MessageLimits {
        message_size: 32,
        headers_size: 8,
        body_size: 64,
        header_line_size: 8,
        header_field_size: 8,
    };

    assert_eq!(
        limit_kind(Message::read_from(&mut reader, limits).unwrap_err()),
        MessageLimit::Message
    );
}

#[test]
fn stops_reading_an_infinite_header_without_newline() {
    let mut reader = BufReader::with_capacity(8, io::repeat(b'x'));
    let limits = MessageLimits {
        message_size: 64,
        headers_size: 64,
        body_size: 64,
        header_line_size: 16,
        header_field_size: 64,
    };

    assert_eq!(
        limit_kind(Message::read_from(&mut reader, limits).unwrap_err()),
        MessageLimit::HeaderLine
    );
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(input.len())
            .ok_or_else(|| io::Error::other("counter overflow"))?;
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn streams_body_without_retaining_it() {
    let raw = b"Subject: test\n\nbody";
    let mut reader = Cursor::new(raw);
    let head = Message::read_headers(&mut reader, MessageLimits::default()).unwrap();
    let header_len = head.len();
    let mut writer = CountingWriter::default();

    let streamed = head.stream_to(&mut reader, &mut writer).unwrap();

    assert_eq!(streamed.header(), &raw[..header_len]);
    assert_eq!(streamed.len(), raw.len());
    assert_eq!(writer.bytes, raw.len());
}

#[test]
fn buffers_body_while_streaming_an_identical_message() {
    let input = b"Subject: test\n\nbinary:\xff\x00body";
    let mut reader = Cursor::new(input);
    let head = Message::read_headers(&mut reader, MessageLimits::default()).unwrap();
    let mut writer = Vec::new();

    let message = head.read_body_to(&mut reader, &mut writer).unwrap();
    assert_eq!(message.as_bytes(), input);
    assert_eq!(message.body(), b"binary:\xff\x00body");
    assert_eq!(writer, input);
}

#[test]
fn header_phase_leaves_body_unconsumed() {
    let raw = b"Subject: test\n\nbody";
    let mut reader = Cursor::new(raw);

    let head = Message::read_headers(&mut reader, MessageLimits::default()).unwrap();

    assert_eq!(head.as_bytes(), b"Subject: test\n\n");
    assert_eq!(reader.position(), head.len() as u64);
}

#[test]
fn bounded_streaming_stops_an_infinite_body() {
    let input = Cursor::new(b"\n".to_vec()).chain(io::repeat(b'x'));
    let mut reader = BufReader::with_capacity(8, input);
    let limits = MessageLimits {
        message_size: 32,
        headers_size: 8,
        body_size: 64,
        header_line_size: 8,
        header_field_size: 8,
    };
    let head = Message::read_headers(&mut reader, limits).unwrap();
    let mut writer = CountingWriter::default();

    assert_eq!(
        limit_kind(head.stream_to(&mut reader, &mut writer).unwrap_err()),
        MessageLimit::Message
    );
    assert!(writer.bytes <= 32);
}
