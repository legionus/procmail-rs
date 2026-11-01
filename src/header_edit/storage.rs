// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::ops::Range;
use std::sync::{Arc, OnceLock};

use super::HeaderEditError;
use crate::config::MessageStatus;
use crate::limits::MessageLimits;
use crate::message::MessageLimit;

#[derive(Debug, Clone)]
pub struct EditedHeader {
    pub(super) storage: Arc<HeaderStorage>,
    pub(super) limits: MessageLimits,
}

#[derive(Debug)]
pub(super) struct HeaderStorage {
    pub(super) store: HeaderStore,
    pub(super) separator: Vec<u8>,
    pub(super) line_ending: &'static [u8],
    pub(super) size: usize,
    pub(super) status: MessageStatus,
    pub(super) raw: OnceLock<Vec<u8>>,
    pub(super) matching: OnceLock<Option<Vec<u8>>>,
}

impl Clone for HeaderStorage {
    fn clone(&self) -> Self {
        // A private editing transaction copies only the arena and index, not
        // cached views or earlier versions. Branches retain the old immutable
        // storage; a failed transaction cannot alter their fields or caches.
        Self {
            store: self.store.clone(),
            separator: self.separator.clone(),
            line_ending: self.line_ending,
            size: self.size,
            status: self.status,
            raw: OnceLock::new(),
            matching: OnceLock::new(),
        }
    }
}

impl PartialEq for EditedHeader {
    fn eq(&self, other: &Self) -> bool {
        self.limits == other.limits
            && self.status() == other.status()
            && self.as_bytes() == other.as_bytes()
    }
}

impl Eq for EditedHeader {}

#[derive(Debug, Clone, Copy)]
pub enum HeaderView<'a> {
    Raw(&'a [u8]),
    Indexed(&'a EditedHeader),
}

impl<'a> From<&'a [u8]> for HeaderView<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        Self::Raw(bytes)
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for HeaderView<'a> {
    fn from(bytes: &'a [u8; N]) -> Self {
        Self::Raw(bytes)
    }
}

impl<'a> From<&'a Vec<u8>> for HeaderView<'a> {
    fn from(bytes: &'a Vec<u8>) -> Self {
        Self::Raw(bytes)
    }
}

impl<'a> HeaderView<'a> {
    pub(crate) fn fields(self) -> HeaderFields<'a> {
        HeaderFields {
            view: self,
            offset: 0,
        }
    }
}

pub(crate) struct HeaderFields<'a> {
    pub(super) view: HeaderView<'a>,
    pub(super) offset: usize,
}

impl<'a> Iterator for HeaderFields<'a> {
    type Item = Field<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.view {
            HeaderView::Indexed(header) => {
                if self.offset >= header.storage.store.fields.len() {
                    return None;
                }

                let field = header.storage.store.field(self.offset);
                self.offset += 1;
                Some(field)
            }
            HeaderView::Raw(header) => {
                let start = self.offset;
                let line = header
                    .get(start..)?
                    .split_inclusive(|byte| *byte == b'\n')
                    .next()?;

                if line == b"\n" || line == b"\r\n" {
                    return None;
                }

                self.offset = start.checked_add(line.len())?;

                // Preserve malformed and leading continuation bytes as fields
                // too. Editing and structured lookup must agree on boundaries;
                // only the latter ignores entries without a usable field name.
                while matches!(header.get(self.offset), Some(b' ' | b'\t')) {
                    let line = header[self.offset..]
                        .split_inclusive(|byte| *byte == b'\n')
                        .next()?;
                    self.offset = self.offset.checked_add(line.len())?;
                }

                Some(field_from_bytes(&header[start..self.offset]))
            }
        }
    }
}

impl EditedHeader {
    pub fn new<'a>(
        source: impl Into<HeaderView<'a>>,
        limits: MessageLimits,
    ) -> Result<Self, HeaderEditError> {
        let source = source.into();
        let header = match source {
            HeaderView::Indexed(header) => {
                let mut cloned = header.clone();
                cloned.limits = limits;
                cloned
            }
            HeaderView::Raw(bytes) => {
                let (fields, separator, line_ending) = split_fields(bytes);
                let store = HeaderStore::new(&fields, limits.headers_size.max(bytes.len()))?;
                EditedHeader {
                    storage: Arc::new(HeaderStorage {
                        store,
                        separator: separator.to_vec(),
                        line_ending,
                        size: bytes.len(),
                        status: MessageStatus::NONE,
                        raw: OnceLock::new(),
                        matching: OnceLock::new(),
                    }),
                    limits,
                }
            }
        };
        Ok(header)
    }

    pub fn field_count(&self) -> usize {
        self.storage.store.fields.len()
    }

    pub fn field(&self, index: usize) -> Field<'_> {
        self.storage.store.field(index)
    }

    pub fn has_matching_pattern(&self, pattern: &[u8]) -> bool {
        self.storage.store.has_matching_pattern(pattern)
    }

    pub fn remove_matching(&mut self, name: &str) {
        if (0..self.storage.store.fields.len())
            .any(|index| self.storage.store.matches(index, name.as_bytes()))
        {
            self.store_mut().remove_matching(name.as_bytes());
        }
    }

    pub fn remove_matching_pattern(&mut self, pattern: &[u8]) {
        self.store_mut().remove_matching_pattern(pattern);
    }

    pub fn rename_matching_pattern(
        &mut self,
        from_pattern: &[u8],
        to_pattern: &[u8],
    ) -> Result<(), HeaderEditError> {
        self.store_mut()
            .rename_matching_pattern(from_pattern, to_pattern)
    }

    pub fn prepend_prefix_to_matching(
        &mut self,
        pattern: &[u8],
        prefix: &[u8],
    ) -> Result<(), HeaderEditError> {
        self.store_mut().prepend_prefix_to_matching(pattern, prefix)
    }

    pub fn make_unique_first(&mut self, pattern: &[u8]) {
        self.store_mut().make_unique_first(pattern);
    }

    pub fn make_unique_last(&mut self, pattern: &[u8]) {
        self.store_mut().make_unique_last(pattern);
    }

    pub fn add_raw_field(&mut self, raw: &[u8]) -> Result<(), HeaderEditError> {
        let line_ending = self.storage.line_ending;
        let mut bytes = raw.to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.extend_from_slice(line_ending);
        }
        let pos = self.storage.store.fields.len();
        self.store_mut().insert(pos, &bytes)
    }

    pub fn concatenate_continued_fields(&mut self) -> Result<(), HeaderEditError> {
        self.store_mut().concatenate_continued()
    }

    pub fn zap_whitespace(&mut self) -> Result<(), HeaderEditError> {
        self.store_mut().zap_whitespace()
    }

    pub fn extract_fields(
        &self,
        pattern: &[u8],
        include_name: bool,
        concatenate: bool,
        zap: bool,
    ) -> Vec<Vec<u8>> {
        let store = &self.storage.store;
        let mut results = Vec::new();

        for i in 0..store.fields.len() {
            let field = store.field(i);
            if field_matches_pattern(field, pattern) {
                let mut bytes = if include_name {
                    if concatenate {
                        unfold_field_bytes(field.bytes)
                    } else {
                        field.bytes.to_vec()
                    }
                } else {
                    let val = field.value().unwrap_or_default();
                    if concatenate {
                        unfold_field_bytes(val)
                    } else {
                        val.to_vec()
                    }
                };

                if zap && !include_name {
                    let stripped = strip_line_ending(&bytes);
                    let start = stripped
                        .iter()
                        .position(|b| *b != b' ' && *b != b'\t')
                        .unwrap_or(stripped.len());
                    let end = stripped
                        .iter()
                        .rposition(|b| *b != b' ' && *b != b'\t')
                        .map_or(0, |idx| idx + 1);
                    if start < end {
                        let mut trimmed = stripped[start..end].to_vec();
                        trimmed.push(b'\n');
                        bytes = trimmed;
                    } else {
                        bytes = vec![b'\n'];
                    }
                } else if !bytes.ends_with(b"\n") {
                    bytes.push(b'\n');
                }
                results.push(bytes);
            }
        }
        results
    }

    pub fn finish(&mut self, body_len: usize) -> Result<(), HeaderEditError> {
        let line_ending = self.storage.line_ending;
        let size = serialized_size(&self.storage.store, &self.storage.separator, line_ending)?;

        if let Some(storage) = Arc::get_mut(&mut self.storage) {
            storage.size = size;
        }

        self.validate(body_len, self.limits)?;

        let store = &self.storage.store;
        let needs_reindex = (1..store.fields.len()).any(|index| {
            matches!(store.field(index).bytes.first(), Some(b' ' | b'\t'))
                || !store.field(index - 1).bytes.ends_with(b"\n")
        });

        if needs_reindex {
            let status = self.status();
            let bytes = self.as_bytes();
            let (fields, separator, line_ending) = split_fields(bytes);
            let store = HeaderStore::new(&fields, self.limits.headers_size)?;
            self.storage = Arc::new(HeaderStorage {
                store,
                separator: separator.to_vec(),
                line_ending,
                size,
                status,
                raw: OnceLock::new(),
                matching: OnceLock::new(),
            });
        }

        let line_ending = preferred_line_ending(self.parts());

        if let Some(storage) = Arc::get_mut(&mut self.storage) {
            storage.line_ending = line_ending;
        }

        Ok(())
    }

    pub(super) fn store_mut(&mut self) -> &mut HeaderStore {
        // Lowering an active limit may leave accepted input above it. Removal
        // may repair that input, but subsequent growth never gets a larger
        // allowance merely because the arena contains previously deleted data.
        let ceiling = self.len().max(self.limits.headers_size);
        let storage = Arc::make_mut(&mut self.storage);
        storage.store.ceiling = ceiling;
        &mut storage.store
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.storage.raw.get_or_init(|| {
            let mut bytes = Vec::with_capacity(self.len());

            for part in self.parts() {
                bytes.extend_from_slice(part);
            }

            bytes
        })
    }

    pub fn len(&self) -> usize {
        self.storage.size
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn status(&self) -> MessageStatus {
        self.storage.status
    }

    pub(super) fn set_status(&mut self, status: MessageStatus) {
        if self.storage.status != status {
            Arc::make_mut(&mut self.storage).status = status;
        }
    }

    pub(super) fn parts(&self) -> impl Iterator<Item = &[u8]> {
        header_parts(
            &self.storage.store,
            &self.storage.separator,
            self.storage.line_ending,
        )
    }

    pub fn matching_header(&self) -> &[u8] {
        self.normalized().unwrap_or_else(|| self.as_bytes())
    }

    pub fn normalized(&self) -> Option<&[u8]> {
        self.storage
            .matching
            .get_or_init(|| crate::message::normalize_folded_header(self.as_bytes()))
            .as_deref()
    }

    pub fn validate(&self, body_len: usize, limits: MessageLimits) -> Result<(), HeaderEditError> {
        validate_header_parts(self.parts(), self.len(), body_len, limits)
    }

    pub fn into_streaming_bytes(self) -> Vec<u8> {
        match Arc::try_unwrap(self.storage) {
            Ok(storage) => storage.raw.into_inner().unwrap_or_else(|| {
                header_parts(&storage.store, &storage.separator, storage.line_ending)
                    .flatten()
                    .copied()
                    .collect()
            }),
            Err(storage) => Self {
                storage,
                limits: self.limits,
            }
            .as_bytes()
            .to_vec(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Field<'a> {
    pub bytes: &'a [u8],
    pub name: Option<&'a [u8]>,
}

impl<'a> Field<'a> {
    pub fn value(&self) -> Option<&'a [u8]> {
        let start = self.name?.len().checked_add(1)?;
        self.bytes.get(start..)
    }
}

#[derive(Debug, Clone)]
pub(super) struct HeaderField {
    pub(super) raw: Range<usize>,
    pub(super) name: Option<Range<usize>>,
}

#[derive(Debug, Clone)]
pub(crate) struct HeaderStore {
    pub(super) bytes: Vec<u8>,
    pub(super) fields: Vec<HeaderField>,
    pub(super) free: Vec<Range<usize>>,
    pub(super) ceiling: usize,
}

impl HeaderStore {
    pub(super) fn new(fields: &[Field<'_>], ceiling: usize) -> Result<Self, HeaderEditError> {
        let size = fields.iter().try_fold(0usize, |size, field| {
            size.checked_add(field.bytes.len())
                .ok_or(HeaderEditError::SizeOverflow)
        })?;
        check_limit(size, ceiling, MessageLimit::Headers)?;
        let mut store = Self {
            bytes: Vec::with_capacity(size),
            fields: Vec::with_capacity(fields.len()),
            free: Vec::new(),
            ceiling,
        };

        for field in fields {
            store.insert(store.fields.len(), field.bytes)?;
        }

        Ok(store)
    }

    pub(super) fn field(&self, index: usize) -> Field<'_> {
        let field = &self.fields[index];
        Field {
            bytes: &self.bytes[field.raw.clone()],
            name: field.name.as_ref().map(|range| &self.bytes[range.clone()]),
        }
    }

    pub(super) fn matches(&self, index: usize, name: &[u8]) -> bool {
        self.field(index)
            .name
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
    }

    pub(super) fn remove(&mut self, index: usize) {
        self.free.push(self.fields.remove(index).raw);
        self.coalesce_free();
    }

    pub(super) fn remove_matching(&mut self, name: &[u8]) {
        let bytes = &self.bytes;
        let free = &mut self.free;

        // Removing a large run one index at a time repeatedly shifts the same
        // tail. Retain survivors once, then merge physical holes in one pass.
        self.fields.retain(|field| {
            let matches = field
                .name
                .as_ref()
                .is_some_and(|range| bytes[range.clone()].eq_ignore_ascii_case(name));

            if matches {
                free.push(field.raw.clone());
            }

            !matches
        });
        self.coalesce_free();
    }

    pub(super) fn has_matching_pattern(&self, pattern: &[u8]) -> bool {
        (0..self.fields.len()).any(|index| field_matches_pattern(self.field(index), pattern))
    }

    pub(super) fn remove_matching_pattern(&mut self, pattern: &[u8]) {
        let bytes = &self.bytes;
        let free = &mut self.free;

        self.fields.retain(|field| {
            let f = Field {
                bytes: &bytes[field.raw.clone()],
                name: field.name.as_ref().map(|range| &bytes[range.clone()]),
            };
            let matches = field_matches_pattern(f, pattern);
            if matches {
                free.push(field.raw.clone());
            }
            !matches
        });
        self.coalesce_free();
    }

    pub(super) fn make_unique_first(&mut self, pattern: &[u8]) {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let bytes = &self.bytes;
        let free = &mut self.free;

        self.fields.retain(|field| {
            let f = Field {
                bytes: &bytes[field.raw.clone()],
                name: field.name.as_ref().map(|range| &bytes[range.clone()]),
            };
            if field_matches_pattern(f, pattern) {
                let name_key = if let Some(name) = f.name {
                    name.to_ascii_lowercase()
                } else {
                    b"from ".to_vec()
                };
                if seen.iter().any(|s| s == &name_key) {
                    free.push(field.raw.clone());
                    return false;
                }
                seen.push(name_key);
            }
            true
        });
        self.coalesce_free();
    }

    pub(super) fn make_unique_last(&mut self, pattern: &[u8]) {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut retain_indices = Vec::with_capacity(self.fields.len());
        for (idx, field) in self.fields.iter().enumerate().rev() {
            let f = Field {
                bytes: &self.bytes[field.raw.clone()],
                name: field.name.as_ref().map(|range| &self.bytes[range.clone()]),
            };
            if field_matches_pattern(f, pattern) {
                let name_key = if let Some(name) = f.name {
                    name.to_ascii_lowercase()
                } else {
                    b"from ".to_vec()
                };
                if seen.iter().any(|s| s == &name_key) {
                    continue;
                }
                seen.push(name_key);
            }
            retain_indices.push(idx);
        }
        retain_indices.sort_unstable();

        let mut current_idx = 0;
        let free = &mut self.free;
        self.fields.retain(|field| {
            let keep = retain_indices.binary_search(&current_idx).is_ok();
            if !keep {
                free.push(field.raw.clone());
            }
            current_idx += 1;
            keep
        });
        self.coalesce_free();
    }

    pub(super) fn rename_matching_pattern(
        &mut self,
        from_pattern: &[u8],
        to_pattern: &[u8],
    ) -> Result<(), HeaderEditError> {
        let mut i = 0;
        while i < self.fields.len() {
            let field = self.field(i);
            if field_matches_pattern(field, from_pattern) {
                if let Some(replacement) = rename_field_pattern(field, from_pattern, to_pattern)? {
                    if field.bytes != replacement.as_slice() {
                        self.remove(i);
                        self.insert(i, &replacement)?;
                    }
                }
            }
            i += 1;
        }
        Ok(())
    }

    pub(super) fn prepend_prefix_to_matching(
        &mut self,
        pattern: &[u8],
        prefix: &[u8],
    ) -> Result<(), HeaderEditError> {
        let mut i = 0;
        while i < self.fields.len() {
            let field = self.field(i);
            if field_matches_pattern(field, pattern) && field.name.is_some() {
                let mut replacement = Vec::with_capacity(prefix.len() + field.bytes.len());
                replacement.extend_from_slice(prefix);
                replacement.extend_from_slice(field.bytes);
                self.remove(i);
                self.insert(i, &replacement)?;
            }
            i += 1;
        }
        Ok(())
    }

    pub(super) fn concatenate_continued(&mut self) -> Result<(), HeaderEditError> {
        let mut i = 0;
        while i < self.fields.len() {
            let field = self.field(i);
            if !field.bytes.starts_with(b"From ") {
                let unfolded = unfold_field_bytes(field.bytes);
                if unfolded != field.bytes {
                    self.remove(i);
                    self.insert(i, &unfolded)?;
                }
            }
            i += 1;
        }
        Ok(())
    }

    pub(super) fn zap_whitespace(&mut self) -> Result<(), HeaderEditError> {
        let mut i = 0;
        while i < self.fields.len() {
            let field = self.field(i);
            if let Some(name) = field.name {
                let after_colon = &field.bytes[name.len() + 1..];
                let trimmed_ending = strip_line_ending(after_colon);
                if trimmed_ending.len() == 1
                    && (trimmed_ending[0] == b' ' || trimmed_ending[0] == b'\t')
                {
                    self.remove(i);
                    continue;
                }
                if !after_colon.is_empty()
                    && after_colon[0] != b' '
                    && after_colon[0] != b'\t'
                    && after_colon[0] != b'\r'
                    && after_colon[0] != b'\n'
                {
                    let mut replacement = Vec::with_capacity(field.bytes.len() + 1);
                    replacement.extend_from_slice(name);
                    replacement.push(b':');
                    replacement.push(b' ');
                    replacement.extend_from_slice(after_colon);
                    self.remove(i);
                    self.insert(i, &replacement)?;
                }
            }
            i += 1;
        }
        Ok(())
    }

    pub(super) fn coalesce_free(&mut self) {
        // Logical field order differs from physical order after prepend or
        // reuse. Both single and bulk removal merge only physical neighbors.
        self.free.sort_unstable_by_key(|range| range.start);
        let mut retained = 0usize;

        for index in 0..self.free.len() {
            let range = self.free[index].clone();

            if retained > 0 && self.free[retained - 1].end == range.start {
                self.free[retained - 1].end = range.end;
            } else {
                self.free[retained] = range;
                retained += 1;
            }
        }

        self.free.truncate(retained);

        if self
            .free
            .last()
            .is_some_and(|range| range.end == self.bytes.len())
        {
            if let Some(range) = self.free.pop() {
                self.bytes.truncate(range.start);
            }
        }
    }

    pub(super) fn compact(&mut self) -> Result<(), HeaderEditError> {
        // Sorting active ranges lets copy_within move bytes only toward the
        // beginning, without overwriting an unread field or allocating a second
        // arena. Keep fields in output order and update their offsets in place.
        let mut physical: Vec<usize> = (0..self.fields.len()).collect();
        physical.sort_unstable_by_key(|index| self.fields[*index].raw.start);
        let mut cursor = 0usize;

        for index in physical {
            let field = &mut self.fields[index];
            let len = field.raw.len();
            self.bytes.copy_within(field.raw.clone(), cursor);

            if let Some(name) = &mut field.name {
                let relative_start = name.start - field.raw.start;
                let relative_end = name.end - field.raw.start;
                let start = cursor
                    .checked_add(relative_start)
                    .ok_or(HeaderEditError::SizeOverflow)?;
                let end = cursor
                    .checked_add(relative_end)
                    .ok_or(HeaderEditError::SizeOverflow)?;
                *name = start..end;
            }

            let end = cursor
                .checked_add(len)
                .ok_or(HeaderEditError::SizeOverflow)?;
            field.raw = cursor..end;
            cursor = end;
        }

        self.bytes.truncate(cursor);
        self.free.clear();
        Ok(())
    }

    pub(super) fn insert(&mut self, index: usize, bytes: &[u8]) -> Result<(), HeaderEditError> {
        if bytes.is_empty() {
            return Err(HeaderEditError::InvalidGeneratedValue);
        }

        let mut hole = self
            .free
            .iter()
            .position(|range| range.len() >= bytes.len());
        let end = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(HeaderEditError::SizeOverflow)?;

        // Live fields are bounded by their byte size (each owns at least one
        // byte), and holes by live fields plus one. Compact fragmentation before
        // rejecting growth; deleted data must not consume the message allowance.
        if hole.is_none() && end > self.ceiling {
            self.compact()?;
            hole = self
                .free
                .iter()
                .position(|range| range.len() >= bytes.len());
        }

        let start = if let Some(position) = hole {
            let start = self.free[position].start;
            let end = start
                .checked_add(bytes.len())
                .ok_or(HeaderEditError::SizeOverflow)?;
            self.bytes[start..end].copy_from_slice(bytes);
            self.free[position].start = end;

            if self.free[position].is_empty() {
                self.free.remove(position);
            }

            start
        } else {
            let start = self.bytes.len();
            let end = start
                .checked_add(bytes.len())
                .ok_or(HeaderEditError::SizeOverflow)?;
            check_limit(end, self.ceiling, MessageLimit::Headers)?;
            self.bytes.extend_from_slice(bytes);
            start
        };
        let end = start
            .checked_add(bytes.len())
            .ok_or(HeaderEditError::SizeOverflow)?;
        let name = field_from_bytes(bytes)
            .name
            .map(|name| {
                start
                    .checked_add(name.len())
                    .map(|end| start..end)
                    .ok_or(HeaderEditError::SizeOverflow)
            })
            .transpose()?;
        self.fields.insert(
            index,
            HeaderField {
                raw: start..end,
                name,
            },
        );
        Ok(())
    }
}

pub(super) fn header_parts<'a>(
    fields: &'a HeaderStore,
    separator: &'a [u8],
    line_ending: &'a [u8],
) -> impl Iterator<Item = &'a [u8]> {
    (0..fields.fields.len())
        .flat_map(move |index| {
            let join = if index > 0 && !fields.field(index - 1).bytes.ends_with(b"\n") {
                joining_line_ending(fields.field(index - 1).bytes, line_ending)
            } else {
                &[]
            };
            [join, fields.field(index).bytes]
        })
        .chain(std::iter::once(separator))
}

pub(super) fn serialized_size(
    fields: &HeaderStore,
    separator: &[u8],
    line_ending: &[u8],
) -> Result<usize, HeaderEditError> {
    header_parts(fields, separator, line_ending).try_fold(0usize, |size, part| {
        size.checked_add(part.len())
            .ok_or(HeaderEditError::SizeOverflow)
    })
}

pub(super) fn joining_line_ending<'a>(previous: &[u8], preferred: &'a [u8]) -> &'a [u8] {
    // A lone CR at the end of hostile input is header data, but appending LF
    // would turn it into an empty CRLF line and move following fields into the
    // body. Insert a complete CRLF delimiter so the old CR remains non-empty
    // header data and editing cannot change the message boundary.
    if previous.ends_with(b"\r") && preferred == b"\n" {
        b"\r\n"
    } else {
        preferred
    }
}

pub(super) fn validate_aggregate_size(
    fields: &HeaderStore,
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

pub(super) fn validate_header_parts<'a>(
    parts: impl Iterator<Item = &'a [u8]>,
    size: usize,
    body_len: usize,
    limits: MessageLimits,
) -> Result<(), HeaderEditError> {
    check_limit(size, limits.headers_size, MessageLimit::Headers)?;
    let message = size
        .checked_add(body_len)
        .ok_or(HeaderEditError::SizeOverflow)?;
    check_limit(message, limits.message_size, MessageLimit::Message)?;

    // Validate the exact output byte sequence, including inserted delimiters,
    // without preparing a serialized cache. A line or folded field may span
    // several arena ranges; counting each range separately would miss limits.
    let mut line_size = 0usize;
    let mut first = None;
    let mut field_size = 0usize;

    for &byte in parts.flatten() {
        if line_size == 0 {
            first = Some(byte);
        }

        line_size = line_size
            .checked_add(1)
            .ok_or(HeaderEditError::SizeOverflow)?;
        check_limit(line_size, limits.header_line_size, MessageLimit::HeaderLine)?;

        if byte == b'\n' {
            if line_size == 1 || (line_size == 2 && first == Some(b'\r')) {
                return Ok(());
            }

            validate_field_line(first, line_size, &mut field_size, limits)?;
            line_size = 0;
        }
    }

    if line_size != 0 {
        validate_field_line(first, line_size, &mut field_size, limits)?;
    }

    Ok(())
}

pub(super) fn validate_field_line(
    first: Option<u8>,
    line_size: usize,
    field_size: &mut usize,
    limits: MessageLimits,
) -> Result<(), HeaderEditError> {
    *field_size = if matches!(first, Some(b' ' | b'\t')) {
        field_size
            .checked_add(line_size)
            .ok_or(HeaderEditError::SizeOverflow)?
    } else {
        line_size
    };
    check_limit(
        *field_size,
        limits.header_field_size,
        MessageLimit::HeaderField,
    )
}

pub(super) fn check_limit(
    size: usize,
    limit: usize,
    kind: MessageLimit,
) -> Result<(), HeaderEditError> {
    if size > limit {
        Err(HeaderEditError::LimitExceeded { kind, limit })
    } else {
        Ok(())
    }
}

pub(super) fn preferred_line_ending<'a>(parts: impl Iterator<Item = &'a [u8]>) -> &'static [u8] {
    let mut previous = None;

    for &byte in parts.flatten() {
        if byte == b'\n' {
            return if previous == Some(b'\r') {
                b"\r\n"
            } else {
                b"\n"
            };
        }

        previous = Some(byte);
    }

    b"\n"
}

pub(crate) fn split_fields(header: &[u8]) -> (Vec<Field<'_>>, &[u8], &'static [u8]) {
    let separator_start = header_separator_start(header).unwrap_or(header.len());
    let content = &header[..separator_start];
    let separator = &header[separator_start..];
    let line_ending = preferred_line_ending(std::iter::once(header));
    let fields = HeaderView::Raw(content).fields().collect();
    (fields, separator, line_ending)
}

pub(super) fn field_from_bytes(bytes: &[u8]) -> Field<'_> {
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

pub(super) fn header_separator_start(header: &[u8]) -> Option<usize> {
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

pub fn strip_line_ending(value: &[u8]) -> &[u8] {
    value
        .strip_suffix(b"\r\n")
        .or_else(|| value.strip_suffix(b"\n"))
        .unwrap_or(value)
}

pub fn field_matches_pattern(field: Field<'_>, pattern: &[u8]) -> bool {
    if let Some(name) = field.name {
        if let Some(target) = pattern.strip_suffix(b":") {
            name.eq_ignore_ascii_case(target)
        } else {
            name.len() >= pattern.len() && name[..pattern.len()].eq_ignore_ascii_case(pattern)
        }
    } else if field.bytes.starts_with(b"From ") {
        pattern == b"From " || pattern == b"From" || pattern == b"From:"
    } else {
        false
    }
}

fn rename_field_pattern(
    field: Field<'_>,
    from_pattern: &[u8],
    to_pattern: &[u8],
) -> Result<Option<Vec<u8>>, HeaderEditError> {
    let Some(name) = field.name else {
        return Ok(None);
    };
    let to_clean = to_pattern.strip_suffix(b":").unwrap_or(to_pattern);
    let mut renamed_name = Vec::new();

    if let Some(from_clean) = from_pattern.strip_suffix(b":") {
        if name.eq_ignore_ascii_case(from_clean) {
            renamed_name.extend_from_slice(to_clean);
        } else {
            return Ok(None);
        }
    } else if name.len() >= from_pattern.len()
        && name[..from_pattern.len()].eq_ignore_ascii_case(from_pattern)
    {
        renamed_name.extend_from_slice(to_clean);
        renamed_name.extend_from_slice(&name[from_pattern.len()..]);
    } else {
        return Ok(None);
    }

    let suffix = &field.bytes[name.len()..];
    let mut result = Vec::with_capacity(renamed_name.len() + suffix.len());
    result.extend_from_slice(&renamed_name);
    result.extend_from_slice(suffix);
    Ok(Some(result))
}

pub fn unfold_field_bytes(raw: &[u8]) -> Vec<u8> {
    if !raw
        .windows(2)
        .any(|w| w[0] == b'\n' && (w[1] == b' ' || w[1] == b'\t'))
    {
        return raw.to_vec();
    }
    let mut normalized = Vec::with_capacity(raw.len());
    let mut copied = 0usize;
    let mut scanned = 0usize;

    while let Some(offset) = raw[scanned..].iter().position(|byte| *byte == b'\n') {
        let newline = scanned + offset;
        scanned = newline + 1;
        if matches!(raw.get(scanned), Some(b' ' | b'\t')) {
            let line_ending_start = if newline > copied && raw.get(newline - 1) == Some(&b'\r') {
                newline - 1
            } else {
                newline
            };
            normalized.extend_from_slice(&raw[copied..line_ending_start]);
            normalized.push(b' ');
            copied = scanned;
        }
    }
    normalized.extend_from_slice(&raw[copied..]);
    normalized
}
