// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};
use crate::config::{
    HeaderAction, HeaderExtractionMode, HeaderOperation, MAX_ASSIGNMENT_VALUE_LEN,
};
use crate::limits::MessageLimits;
use crate::message::MessageLimit;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditedHeader {
    bytes: Vec<u8>,
    limits: MessageLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeaderExtraction {
    pub(crate) line: usize,
    pub(crate) target: String,
    pub(crate) value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppliedHeaderAction {
    header: EditedHeader,
    extractions: Vec<HeaderExtraction>,
}

impl AppliedHeaderAction {
    pub(crate) fn into_parts(self) -> (EditedHeader, Vec<HeaderExtraction>) {
        (self.header, self.extractions)
    }
}

impl EditedHeader {
    #[cfg(test)]
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_bytes_for_body(self, body_len: usize) -> Result<Vec<u8>, HeaderEditError> {
        validate_edited_header(&self.bytes, body_len, self.limits)?;
        Ok(self.bytes)
    }

    pub(crate) fn into_streaming_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeaderEditError {
    SizeOverflow,
    LimitExceeded { kind: MessageLimit, limit: usize },
    ExtractedValueTooLong { limit: usize },
}

impl fmt::Display for HeaderEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SizeOverflow => formatter.write_str("edited header size overflows usize"),
            Self::LimitExceeded { kind, limit } => {
                write!(formatter, "edited message exceeds {kind} ({limit} bytes)")
            }
            Self::ExtractedValueTooLong { limit } => {
                write!(
                    formatter,
                    "extracted header value exceeds MAX_ASSIGNMENT_VALUE_LEN ({limit} bytes)"
                )
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
pub(crate) fn apply_header_action(
    header: &[u8],
    body_len: usize,
    action: &HeaderAction,
    limits: MessageLimits,
) -> Result<AppliedHeaderAction, HeaderEditError> {
    let (fields, separator, line_ending) = split_fields(header);
    let mut edited: Vec<EditedField<'_>> = fields.into_iter().map(EditedField::Borrowed).collect();
    let mut extractions = Vec::new();

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
            HeaderOperation::Rename { from, to, .. } => {
                for field in &mut edited {
                    if field.matches(from.as_bytes()) {
                        *field = EditedField::Added(rename_field(field.bytes(), to)?);
                    }
                }
            }
            HeaderOperation::Extract {
                line,
                name,
                target,
                mode,
            } => {
                let value = edited
                    .iter()
                    .find(|field| field.matches(name.as_bytes()))
                    .map_or_else(
                        || Ok(Vec::new()),
                        |field| extract_value(field.bytes(), *mode),
                    )?;
                extractions.push(HeaderExtraction {
                    line: *line,
                    target: target.clone(),
                    value,
                });
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
    Ok(AppliedHeaderAction {
        header: EditedHeader {
            bytes: result,
            limits,
        },
        extractions,
    })
}

fn rename_field(field: &[u8], name: &str) -> Result<Vec<u8>, HeaderEditError> {
    let colon = field
        .iter()
        .position(|byte| *byte == b':')
        .unwrap_or(field.len());
    let suffix = field.get(colon..).unwrap_or_default();
    let size = name
        .len()
        .checked_add(suffix.len())
        .ok_or(HeaderEditError::SizeOverflow)?;
    let mut renamed = Vec::with_capacity(size);
    renamed.extend_from_slice(name.as_bytes());
    renamed.extend_from_slice(suffix);
    Ok(renamed)
}

fn extract_value(field: &[u8], mode: HeaderExtractionMode) -> Result<Vec<u8>, HeaderEditError> {
    let first_line_end = field
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(field.len(), |offset| offset + 1);
    let colon = field[..first_line_end]
        .iter()
        .position(|byte| *byte == b':')
        .unwrap_or(first_line_end);
    let value = colon
        .checked_add(1)
        .and_then(|start| field.get(start..))
        .unwrap_or_default();
    match mode {
        HeaderExtractionMode::Raw => {
            let value = strip_line_ending(value);
            if value.len() > MAX_ASSIGNMENT_VALUE_LEN {
                return Err(HeaderEditError::ExtractedValueTooLong {
                    limit: MAX_ASSIGNMENT_VALUE_LEN,
                });
            }
            Ok(value.to_vec())
        }
        HeaderExtractionMode::Unfolded => unfold_value(value),
    }
}

fn unfold_value(value: &[u8]) -> Result<Vec<u8>, HeaderEditError> {
    let mut output = BoundedBytes::with_capacity(MAX_ASSIGNMENT_VALUE_LEN, value.len());
    let mut cursor = 0usize;
    let mut first = true;
    while cursor < value.len() {
        let end = value[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(value.len(), |offset| cursor + offset + 1);
        let line = strip_line_ending(&value[cursor..end]);
        let content = line
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t'))
            .map_or(&[][..], |start| &line[start..]);
        if !first {
            output.try_extend(b" ").map_err(extraction_length_error)?;
        }
        output
            .try_extend(content)
            .map_err(extraction_length_error)?;
        first = false;
        cursor = end;
    }
    Ok(output.into_vec())
}

fn strip_line_ending(value: &[u8]) -> &[u8] {
    value
        .strip_suffix(b"\r\n")
        .or_else(|| value.strip_suffix(b"\n"))
        .unwrap_or(value)
}

fn extraction_length_error(_: BoundedBytesError) -> HeaderEditError {
    HeaderEditError::ExtractedValueTooLong {
        limit: MAX_ASSIGNMENT_VALUE_LEN,
    }
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
#[path = "tests/header_edit.rs"]
mod tests;
