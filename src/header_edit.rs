// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use crate::config::{HeaderAction, HeaderOperation};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeaderEditError {
    SizeOverflow,
}

impl fmt::Display for HeaderEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SizeOverflow => formatter.write_str("edited header size overflows usize"),
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
    action: &HeaderAction,
) -> Result<Vec<u8>, HeaderEditError> {
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
    }

    let size = edited.iter().try_fold(separator.len(), |size, field| {
        size.checked_add(field.bytes().len())
    });
    let mut result = Vec::with_capacity(size.ok_or(HeaderEditError::SizeOverflow)?);
    for field in edited {
        result.extend_from_slice(field.bytes());
    }
    result.extend_from_slice(separator);
    Ok(result)
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
    use super::*;
    use crate::config::HeaderValue;

    fn value(source: &str) -> HeaderValue {
        HeaderValue {
            source: source.into(),
            expansion: None,
        }
    }

    fn action(operations: Vec<HeaderOperation>) -> HeaderAction {
        HeaderAction { operations }
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
            apply_header_action(header, &action).unwrap(),
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
            apply_header_action(header, &action).unwrap(),
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

        assert_eq!(
            apply_header_action(header, &action).unwrap(),
            b"A: one\nB: two\n\n"
        );
    }
}
