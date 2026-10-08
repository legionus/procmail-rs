// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::sync::{Arc, OnceLock};

use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};
use crate::config::{
    HeaderAction, HeaderExtractionMode, HeaderOperation, MAX_ASSIGNMENT_VALUE_LEN,
};
use crate::header_value::{
    DecodedHeaderError, GeneratedHeaderError, RFC5322_HEADER_LINE_LIMIT,
    append_canonical_header_name, decode_rfc2047, serialize_generated_header,
};
use crate::limits::MessageLimits;
use crate::message::MessageLimit;

mod storage;
pub(crate) use storage::{EditedHeader, HeaderView};
use storage::{
    Field, HeaderStorage, HeaderStore, preferred_line_ending, serialized_size, split_fields,
    validate_aggregate_size,
};

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
    pub(crate) fn changed_from(&self, source: HeaderView<'_>) -> bool {
        match source {
            HeaderView::Indexed(header) => !Arc::ptr_eq(&header.storage, &self.header.storage),
            HeaderView::Raw(bytes) => !self
                .header
                .parts()
                .flatten()
                .copied()
                .eq(bytes.iter().copied()),
        }
    }
    pub(crate) fn into_parts(self) -> (EditedHeader, Vec<HeaderExtraction>) {
        (self.header, self.extractions)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeaderEditError {
    SizeOverflow,
    LimitExceeded { kind: MessageLimit, limit: usize },
    InvalidGeneratedValue,
    GeneratedLineTooLong { limit: usize },
    Decode(DecodedHeaderError),
    ExtractedValueTooLong { limit: usize },
}

impl fmt::Display for HeaderEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SizeOverflow => formatter.write_str("edited header size overflows usize"),
            Self::LimitExceeded { kind, limit } => {
                write!(formatter, "edited message exceeds {kind} ({limit} bytes)")
            }
            Self::InvalidGeneratedValue => formatter.write_str(
                "generated header value contains a control character that cannot be represented",
            ),
            Self::GeneratedLineTooLong { limit } => {
                write!(
                    formatter,
                    "generated header line exceeds RFC 5322 limit ({limit} bytes)"
                )
            }
            Self::Decode(error) => formatter.write_str(error.description()),
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

/// Apply already parsed operations to a bounded header section.
///
/// Operations run in source order. `remove` deletes every matching field,
/// including folded continuation lines. `set` replaces the first matching
/// field at its existing position and deletes later duplicates; when absent,
/// it appends the field. `add` appends and `prepend` inserts at the beginning.
pub(crate) fn apply_header_action<'a>(
    source: impl Into<HeaderView<'a>>,
    body_len: usize,
    action: &HeaderAction,
    limits: MessageLimits,
) -> Result<AppliedHeaderAction, HeaderEditError> {
    let source = source.into();
    let mut header = match source {
        HeaderView::Indexed(header) => header.clone(),
        HeaderView::Raw(bytes) => {
            let (fields, separator, line_ending) = split_fields(bytes);
            let store = HeaderStore::new(&fields, limits.headers_size.max(bytes.len()))?;
            EditedHeader {
                storage: Arc::new(HeaderStorage {
                    store,
                    separator: separator.to_vec(),
                    line_ending,
                    size: bytes.len(),
                    raw: OnceLock::new(),
                    matching: OnceLock::new(),
                }),
                limits,
            }
        }
    };
    header.limits = limits;
    let line_ending = header.storage.line_ending;
    let mut extractions = Vec::new();

    // A shared index is borrowed for extraction and no-op edits. Only a real
    // mutation creates private storage; validation and extraction publication
    // happen before accepting it, so failures cannot alter a copied branch.
    for operation in &action.operations {
        match operation {
            HeaderOperation::Remove { name, .. } => {
                if (0..header.storage.store.fields.len())
                    .any(|index| header.storage.store.matches(index, name.as_bytes()))
                {
                    header.store_mut().remove_matching(name.as_bytes());
                }
            }
            HeaderOperation::Set { name, value, .. } => {
                let replacement = make_field(name, &value.source, line_ending)?;
                let store = &header.storage.store;
                let mut matching =
                    (0..store.fields.len()).filter(|index| store.matches(*index, name.as_bytes()));
                let first = matching.next();
                let unchanged = first.is_some_and(|index| store.field(index).bytes == replacement)
                    && matching.next().is_none();

                if !unchanged {
                    let store = header.store_mut();
                    let position = first.unwrap_or(store.fields.len());
                    store.remove_matching(name.as_bytes());
                    store.insert(position, &replacement)?;
                }
            }
            HeaderOperation::Add { name, value, .. } => {
                let field = make_field(name, &value.source, line_ending)?;
                let store = header.store_mut();
                store.insert(store.fields.len(), &field)?;
            }
            HeaderOperation::Prepend { name, value, .. } => {
                let field = make_field(name, &value.source, line_ending)?;
                header.store_mut().insert(0, &field)?;
            }
            HeaderOperation::Rename { from, to, .. } => {
                for index in 0..header.storage.store.fields.len() {
                    let store = &header.storage.store;

                    if store.matches(index, from.as_bytes()) {
                        let replacement = rename_field(store.field(index), to)?;

                        if store.field(index).bytes != replacement {
                            let store = header.store_mut();
                            store.remove(index);
                            store.insert(index, &replacement)?;
                        }
                    }
                }
            }
            HeaderOperation::Extract {
                line,
                name,
                target,
                mode,
            } => {
                let store = &header.storage.store;
                let value = (0..store.fields.len())
                    .find(|index| store.matches(*index, name.as_bytes()))
                    .map_or_else(
                        || Ok(Vec::new()),
                        |index| extract_value(store.field(index), *mode),
                    )?;
                extractions.push(HeaderExtraction {
                    line: *line,
                    target: target.clone(),
                    value,
                });
            }
        }

        validate_aggregate_size(
            &header.storage.store,
            &header.storage.separator,
            line_ending,
            body_len,
            limits,
        )?;
    }

    let size = serialized_size(
        &header.storage.store,
        &header.storage.separator,
        line_ending,
    )?;

    if let Some(storage) = Arc::get_mut(&mut header.storage) {
        storage.size = size;
    }

    header.validate(body_len, limits)?;

    // Orphan continuations can join a prepended field, and a missing newline
    // needs an inserted delimiter before another field. Reindex these malformed
    // boundaries after validation so future preferences cannot change existing
    // delimiters or disagree with the checked serialized size.
    let store = &header.storage.store;
    let needs_reindex = (1..store.fields.len()).any(|index| {
        matches!(store.field(index).bytes.first(), Some(b' ' | b'\t'))
            || !store.field(index - 1).bytes.ends_with(b"\n")
    });

    if needs_reindex {
        let bytes = header.as_bytes();
        let (fields, separator, line_ending) = split_fields(bytes);
        let store = HeaderStore::new(&fields, limits.headers_size)?;
        header.storage = Arc::new(HeaderStorage {
            store,
            separator: separator.to_vec(),
            line_ending,
            size,
            raw: OnceLock::new(),
            matching: OnceLock::new(),
        });
    }

    // Removed fields or a newly inserted delimiter can change the first
    // physical newline. Later actions must choose exactly the ending a fresh
    // parse would observe, without serializing merely to make that choice.
    let line_ending = preferred_line_ending(header.parts());

    if let Some(storage) = Arc::get_mut(&mut header.storage) {
        storage.line_ending = line_ending;
    }

    Ok(AppliedHeaderAction {
        header,
        extractions,
    })
}

fn rename_field(field: Field<'_>, name: &str) -> Result<Vec<u8>, HeaderEditError> {
    let suffix = field
        .name
        .and_then(|old| field.bytes.get(old.len()..))
        .unwrap_or_default();
    let size = name
        .len()
        .checked_add(suffix.len())
        .ok_or(HeaderEditError::SizeOverflow)?;
    let mut renamed = Vec::with_capacity(size);
    append_canonical_header_name(&mut renamed, name);
    renamed.extend_from_slice(suffix);
    Ok(renamed)
}

fn extract_value(field: Field<'_>, mode: HeaderExtractionMode) -> Result<Vec<u8>, HeaderEditError> {
    let value = field.value().unwrap_or_default();
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
        HeaderExtractionMode::Decoded => {
            let unfolded = unfold_value(value)?;
            decode_rfc2047(&unfolded, MAX_ASSIGNMENT_VALUE_LEN).map_err(HeaderEditError::Decode)
        }
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

fn make_field(name: &str, value: &str, line_ending: &[u8]) -> Result<Vec<u8>, HeaderEditError> {
    serialize_generated_header(name, value, line_ending).map_err(|error| match error {
        GeneratedHeaderError::InvalidValue => HeaderEditError::InvalidGeneratedValue,
        GeneratedHeaderError::LineTooLong => HeaderEditError::GeneratedLineTooLong {
            limit: RFC5322_HEADER_LINE_LIMIT,
        },
        GeneratedHeaderError::SizeOverflow => HeaderEditError::SizeOverflow,
    })
}

#[cfg(test)]
#[path = "tests/header_edit.rs"]
mod tests;
