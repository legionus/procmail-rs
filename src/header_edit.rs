// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use crate::config::{HeaderAction, HeaderOperation};
use crate::limits::MessageLimits;
use crate::message::MessageLimit;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditedHeader {
    bytes: Vec<u8>,
    limits: MessageLimits,
}

impl EditedHeader {
    #[cfg(test)]
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn into_bytes_for_body(self, body_len: usize) -> Result<Vec<u8>, HeaderEditError> {
        validate_edited_header(&self.bytes, body_len, self.limits)?;
        Ok(self.bytes)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn into_streaming_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeaderEditError {
    SizeOverflow,
    LimitExceeded { kind: MessageLimit, limit: usize },
}

impl fmt::Display for HeaderEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SizeOverflow => formatter.write_str("edited header size overflows usize"),
            Self::LimitExceeded { kind, limit } => {
                write!(formatter, "edited message exceeds {kind} ({limit} bytes)")
            }
        }
    }
}

impl std::error::Error for HeaderEditError {}

#[derive(Clone)]
struct Field<'a> {
    bytes: &'a [u8],
    name: Option<&'a [u8]>,
}

enum EditedField<'a> {
    Borrowed(Field<'a>),
    Added(Vec<u8>),
}

impl EditedField<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Borrowed(field) => field.bytes,
            Self::Added(bytes) => bytes,
        }
    }

    fn matches(&self, name: &[u8]) -> bool {
        match self {
            Self::Borrowed(field) => field
                .name
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name)),
            Self::Added(bytes) => bytes
                .iter()
                .position(|byte| *byte == b':')
                .is_some_and(|colon| bytes[..colon].eq_ignore_ascii_case(name)),
        }
    }
}

/// Apply already parsed operations to a bounded header section.
///
/// Operations run in source order. `remove` deletes every matching field,
/// including folded continuation lines. `set` replaces the first matching
/// field at its existing position and deletes later duplicates; when absent,
/// it appends the field. `add` appends and `prepend` inserts at the beginning.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn apply_header_action(
    header: &[u8],
    body_len: usize,
    action: &HeaderAction,
    limits: MessageLimits,
) -> Result<EditedHeader, HeaderEditError> {
    let (fields, separator, line_ending) = split_fields(header);
    let mut edited: Vec<EditedField<'_>> = fields.into_iter().map(EditedField::Borrowed).collect();

    // Keeping edits as whole physical byte ranges prevents a folded field
    // from being separated from its continuation lines. Operations are
    // applied sequentially so a later operation sees every earlier change.
    for operation in &action.operations {
        match operation {
            HeaderOperation::Remove { name, .. } => {
                edited.retain(|field| !field.matches(name.as_bytes()));
            }
            HeaderOperation::Set { name, value, .. } => {
                let replacement = make_field(name, &value.source, line_ending)?;
                if let Some(first) = edited
                    .iter()
                    .position(|field| field.matches(name.as_bytes()))
                {
                    edited[first] = EditedField::Added(replacement);
                    let mut seen = false;
                    edited.retain(|field| {
                        if !field.matches(name.as_bytes()) {
                            return true;
                        }
                        if !seen {
                            seen = true;
                            true
                        } else {
                            false
                        }
                    });
                } else {
                    edited.push(EditedField::Added(replacement));
                }
            }
            HeaderOperation::Add { name, value, .. } => {
                edited.push(EditedField::Added(make_field(
                    name,
                    &value.source,
                    line_ending,
                )?));
            }
            HeaderOperation::Prepend { name, value, .. } => {
                edited.insert(
                    0,
                    EditedField::Added(make_field(name, &value.source, line_ending)?),
                );
            }
        }
        validate_aggregate_size(&edited, separator, line_ending, body_len, limits)?;
    }

    let size = serialized_size(&edited, separator, line_ending)?;
    let mut result = Vec::with_capacity(size);
    for field in edited {
        if !result.is_empty() && !result.ends_with(b"\n") {
            result.extend_from_slice(line_ending);
        }
        result.extend_from_slice(field.bytes());
    }
    result.extend_from_slice(separator);
    validate_edited_header(&result, body_len, limits)?;
    Ok(EditedHeader {
        bytes: result,
        limits,
    })
}

fn serialized_size(
    fields: &[EditedField<'_>],
    separator: &[u8],
    line_ending: &[u8],
) -> Result<usize, HeaderEditError> {
    let mut size = 0usize;
    let mut previous_terminated = true;
    for field in fields {
        if size != 0 && !previous_terminated {
            size = size
                .checked_add(line_ending.len())
                .ok_or(HeaderEditError::SizeOverflow)?;
        }
        size = size
            .checked_add(field.bytes().len())
            .ok_or(HeaderEditError::SizeOverflow)?;
        previous_terminated = field.bytes().ends_with(b"\n");
    }
    size.checked_add(separator.len())
        .ok_or(HeaderEditError::SizeOverflow)
}

fn validate_aggregate_size(
    fields: &[EditedField<'_>],
    separator: &[u8],
    line_ending: &[u8],
    body_len: usize,
    limits: MessageLimits,
) -> Result<(), HeaderEditError> {
    let headers = serialized_size(fields, separator, line_ending)?;
    check_limit(headers, limits.headers_size, MessageLimit::Headers)?;
    let message = headers
        .checked_add(body_len)
        .ok_or(HeaderEditError::SizeOverflow)?;
    check_limit(message, limits.message_size, MessageLimit::Message)
}

fn validate_edited_header(
    header: &[u8],
    body_len: usize,
    limits: MessageLimits,
) -> Result<(), HeaderEditError> {
    check_limit(header.len(), limits.headers_size, MessageLimit::Headers)?;
    let message = header
        .len()
        .checked_add(body_len)
        .ok_or(HeaderEditError::SizeOverflow)?;
    check_limit(message, limits.message_size, MessageLimit::Message)?;

    let mut field_size = 0usize;
    let mut cursor = 0usize;
    while cursor < header.len() {
        let end = header[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(header.len(), |offset| cursor + offset + 1);
        let line = &header[cursor..end];
        check_limit(
            line.len(),
            limits.header_line_size,
            MessageLimit::HeaderLine,
        )?;
        if line == b"\n" || line == b"\r\n" {
            break;
        }
        field_size = if matches!(line.first(), Some(b' ' | b'\t')) {
            field_size
                .checked_add(line.len())
                .ok_or(HeaderEditError::SizeOverflow)?
        } else {
            line.len()
        };
        check_limit(
            field_size,
            limits.header_field_size,
            MessageLimit::HeaderField,
        )?;
        cursor = end;
    }
    Ok(())
}

fn check_limit(size: usize, limit: usize, kind: MessageLimit) -> Result<(), HeaderEditError> {
    if size > limit {
        Err(HeaderEditError::LimitExceeded { kind, limit })
    } else {
        Ok(())
    }
}

fn make_field(name: &str, value: &str, line_ending: &[u8]) -> Result<Vec<u8>, HeaderEditError> {
    let size = name
        .len()
        .checked_add(2)
        .and_then(|size| size.checked_add(value.len()))
        .and_then(|size| size.checked_add(line_ending.len()))
        .ok_or(HeaderEditError::SizeOverflow)?;
    let mut field = Vec::with_capacity(size);
    field.extend_from_slice(name.as_bytes());
    field.extend_from_slice(b": ");
    field.extend_from_slice(value.as_bytes());
    field.extend_from_slice(line_ending);
    Ok(field)
}

fn split_fields(header: &[u8]) -> (Vec<Field<'_>>, &[u8], &[u8]) {
    let separator_start = header_separator_start(header).unwrap_or(header.len());
    let content = &header[..separator_start];
    let separator = &header[separator_start..];
    let line_ending = content.iter().position(|byte| *byte == b'\n').map_or_else(
        || {
            if separator.starts_with(b"\r\n") {
                b"\r\n".as_slice()
            } else {
                b"\n".as_slice()
            }
        },
        |newline| {
            if newline > 0 && content[newline - 1] == b'\r' {
                b"\r\n".as_slice()
            } else {
                b"\n".as_slice()
            }
        },
    );
    let mut fields = Vec::new();
    let mut start = 0usize;
    let mut cursor = 0usize;

    while cursor < content.len() {
        let end = content[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(content.len(), |offset| cursor + offset + 1);
        let continuation = matches!(content.get(cursor), Some(b' ' | b'\t'));
        if cursor != start && !continuation {
            fields.push(field_from_bytes(&content[start..cursor]));
            start = cursor;
        }
        cursor = end;
    }
    if start < content.len() {
        fields.push(field_from_bytes(&content[start..]));
    }
    (fields, separator, line_ending)
}

fn field_from_bytes(bytes: &[u8]) -> Field<'_> {
    let first_line_end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(bytes.len());
    let name = bytes[..first_line_end]
        .iter()
        .position(|byte| *byte == b':')
        .map(|colon| &bytes[..colon]);
    Field { bytes, name }
}

fn header_separator_start(header: &[u8]) -> Option<usize> {
    let mut line_start = 0usize;
    while line_start < header.len() {
        let newline = header[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')?;
        let line_end = line_start.checked_add(newline)?.checked_add(1)?;
        if &header[line_start..line_end] == b"\n" || &header[line_start..line_end] == b"\r\n" {
            return Some(line_start);
        }
        line_start = line_end;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor, Seek};

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
        apply_header_action(header, body_len, action, MessageLimits::default()).unwrap()
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
        let head = head.with_edited_header(edited);
        assert_eq!(reader.stream_position().unwrap(), body_position);

        let message = head.read_body(&mut reader).unwrap();
        assert_eq!(message.header(), b"A: new\n\n");
        assert_eq!(message.body(), b"body remains unread");
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
        result: Result<EditedHeader, HeaderEditError>,
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
}
