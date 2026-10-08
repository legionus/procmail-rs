// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::io::{BufRead, Read, Write};
use std::ops::Range;

use crate::config::ActionInput;
use crate::header_edit::{EditedHeader, HeaderEditError, HeaderView};
use crate::limits::MessageLimits;

/// Borrowed input pieces; writing or reading them does not join the body.
#[derive(Debug, Clone, Copy)]
pub struct MessageBytes<'a> {
    parts: [&'a [u8]; 2],
}

impl<'a> MessageBytes<'a> {
    pub fn new(first: &'a [u8], second: &'a [u8]) -> Self {
        Self {
            parts: [first, second],
        }
    }

    pub fn reader(self) -> std::io::Chain<std::io::Cursor<&'a [u8]>, std::io::Cursor<&'a [u8]>> {
        std::io::Cursor::new(self.parts[0]).chain(std::io::Cursor::new(self.parts[1]))
    }

    pub fn parts(self) -> [&'a [u8]; 2] {
        self.parts
    }

    pub fn write_to(self, writer: &mut impl Write) -> std::io::Result<()> {
        for part in self.parts {
            writer.write_all(part)?;
        }

        Ok(())
    }

    pub fn ends_with(self, suffix: &[u8]) -> bool {
        self.parts
            .into_iter()
            .rev()
            .flat_map(|part| part.iter().rev())
            .take(suffix.len())
            .eq(suffix.iter().rev())
    }

    pub fn is_empty(self) -> bool {
        self.parts.iter().all(|part| part.is_empty())
    }
}

impl<'a> From<&'a [u8]> for MessageBytes<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        Self::new(bytes, &[])
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for MessageBytes<'a> {
    fn from(bytes: &'a [u8; N]) -> Self {
        Self::from(bytes.as_slice())
    }
}

impl<'a> From<&'a Vec<u8>> for MessageBytes<'a> {
    fn from(bytes: &'a Vec<u8>) -> Self {
        Self::from(bytes.as_slice())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    raw: Vec<u8>,
    header: Range<usize>,
    body: Range<usize>,
    matching_header: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageHead {
    backing: HeaderBacking,
    matching_header: Option<Vec<u8>>,
    limits: MessageLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HeaderBacking {
    Input(Vec<u8>),
    Indexed(EditedHeader),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamedMessage {
    header: Vec<u8>,
    matching_header: Option<Vec<u8>>,
    len: usize,
}

impl Message {
    pub fn from_filter_output(
        header: &[u8],
        body: &[u8],
        output: Message,
        area: ActionInput,
        limits: MessageLimits,
    ) -> Result<Self, MessageReadError> {
        // The child reader already owns the full output. Recheck the active
        // limits through the same bounded streaming path, then retain that
        // allocation. Partial filters still require a newly validated join
        // because the preserved area was not part of the child output.
        if area == ActionInput::Message {
            let mut reader = std::io::Cursor::new(output.as_bytes());
            Self::read_headers(&mut reader, limits)?
                .stream_to(&mut reader, &mut std::io::sink())?;
            return Ok(output);
        }

        let output = if area == ActionInput::Body {
            output.as_bytes().get(1..).ok_or_else(|| {
                MessageReadError::Io(std::io::Error::other(
                    "body filter output lost its private separator",
                ))
            })?
        } else {
            output.as_bytes()
        };

        // A partial filter replaces only the bytes sent to the command. Feed
        // the joined slices back through bounded ingestion so retained input
        // plus child output cannot exceed any message or header limit.
        if area == ActionInput::Headers {
            let reader = std::io::Cursor::new(output).chain(std::io::Cursor::new(body));
            Self::read_from(&mut std::io::BufReader::new(reader), limits)
        } else {
            let reader = std::io::Cursor::new(header).chain(std::io::Cursor::new(output));
            Self::read_from(&mut std::io::BufReader::new(reader), limits)
        }
    }

    #[cfg(test)]
    pub(crate) fn from_bytes(raw: Vec<u8>) -> Self {
        let body_start = find_body_start(&raw).unwrap_or(raw.len());
        let matching_header = normalize_folded_header(&raw[..body_start]);

        Self {
            header: 0..body_start,
            body: body_start..raw.len(),
            matching_header,
            raw,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn header(&self) -> &[u8] {
        &self.raw[self.header.clone()]
    }

    pub fn matching_header(&self) -> &[u8] {
        self.matching_header
            .as_deref()
            .unwrap_or_else(|| self.header())
    }

    pub fn matching_message(&self) -> Option<Vec<u8>> {
        let header = self.matching_header.as_ref()?;
        let mut matching = Vec::with_capacity(self.raw.len());
        matching.extend_from_slice(header);
        matching.extend_from_slice(self.body());
        Some(matching)
    }

    pub fn body(&self) -> &[u8] {
        &self.raw[self.body.clone()]
    }

    pub fn len(&self) -> usize {
        self.raw.len()
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    pub fn read_from(
        reader: &mut impl BufRead,
        limits: MessageLimits,
    ) -> Result<Self, MessageReadError> {
        Self::read_headers(reader, limits)?.read_body(reader)
    }

    pub fn read_headers(
        reader: &mut impl BufRead,
        limits: MessageLimits,
    ) -> Result<MessageHead, MessageReadError> {
        let mut raw = Vec::with_capacity(limits.headers_size.min(64 * 1024));
        let mut field_size = 0usize;

        while let Some(line) = read_header_line(reader, &limits, raw.len())? {
            let is_separator = line == b"\n" || line == b"\r\n";

            if !is_separator {
                if matches!(line.first(), Some(b' ' | b'\t')) {
                    field_size = field_size.checked_add(line.len()).ok_or_else(|| {
                        MessageReadError::limit(MessageLimit::HeaderField, limits.header_field_size)
                    })?;
                } else {
                    field_size = line.len();
                }
                if field_size > limits.header_field_size {
                    return Err(MessageReadError::limit(
                        MessageLimit::HeaderField,
                        limits.header_field_size,
                    ));
                }
            }

            raw.extend_from_slice(&line);
            if is_separator {
                break;
            }
        }

        let matching_header = normalize_folded_header(&raw);
        Ok(MessageHead {
            backing: HeaderBacking::Input(raw),
            matching_header,
            limits,
        })
    }
}

impl MessageHead {
    pub(crate) fn from_edited_header(
        edited: EditedHeader,
        body_len: usize,
        limits: MessageLimits,
    ) -> Result<Self, HeaderEditError> {
        edited.validate(body_len, limits)?;
        Ok(Self {
            backing: HeaderBacking::Indexed(edited),
            matching_header: None,
            limits,
        })
    }

    pub(crate) fn replace_edited_header(&mut self, edited: EditedHeader) {
        self.backing = HeaderBacking::Indexed(edited);
        self.matching_header = None;
    }

    pub(crate) fn limits(&self) -> MessageLimits {
        self.limits
    }

    pub fn as_bytes(&self) -> &[u8] {
        match &self.backing {
            HeaderBacking::Input(raw) => raw,
            HeaderBacking::Indexed(header) => header.as_bytes(),
        }
    }

    pub(crate) fn header_view(&self) -> HeaderView<'_> {
        match &self.backing {
            HeaderBacking::Input(raw) => HeaderView::Raw(raw),
            HeaderBacking::Indexed(header) => HeaderView::Indexed(header),
        }
    }

    pub fn matching_header(&self) -> &[u8] {
        match &self.backing {
            HeaderBacking::Input(raw) => self.matching_header.as_deref().unwrap_or(raw),
            HeaderBacking::Indexed(header) => header.matching_header(),
        }
    }

    pub fn take_matching_header(&mut self) -> Option<Vec<u8>> {
        match &self.backing {
            HeaderBacking::Input(_) => self.matching_header.take(),
            HeaderBacking::Indexed(header) => header.normalized().map(<[u8]>::to_vec),
        }
    }

    pub fn len(&self) -> usize {
        match &self.backing {
            HeaderBacking::Input(raw) => raw.len(),
            HeaderBacking::Indexed(header) => header.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn into_serialized_parts(self) -> (Vec<u8>, Option<Vec<u8>>, MessageLimits) {
        let (raw, matching) = match self.backing {
            HeaderBacking::Input(raw) => (raw, self.matching_header),
            HeaderBacking::Indexed(header) => {
                let matching = header.normalized().map(<[u8]>::to_vec);
                (header.into_streaming_bytes(), matching)
            }
        };
        (raw, matching, self.limits)
    }

    pub fn read_body(self, reader: &mut impl BufRead) -> Result<Message, MessageReadError> {
        let (mut raw, matching_header, limits) = self.into_serialized_parts();
        let body_start = raw.len();
        read_body(reader, &limits, body_start, &mut raw, true, None)?;
        let body_end = raw.len();

        Ok(Message {
            header: 0..body_start,
            body: body_start..body_end,
            matching_header,
            raw,
        })
    }

    pub fn read_body_to(
        self,
        reader: &mut impl BufRead,
        writer: &mut impl Write,
    ) -> Result<Message, MessageReadError> {
        let (mut raw, matching_header, limits) = self.into_serialized_parts();
        writer.write_all(&raw)?;
        let body_start = raw.len();
        let mut total = body_start;
        read_body(
            reader,
            &limits,
            body_start,
            &mut raw,
            true,
            Some((&mut total, writer)),
        )?;
        let body_end = raw.len();

        Ok(Message {
            header: 0..body_start,
            body: body_start..body_end,
            matching_header,
            raw,
        })
    }

    pub fn stream_to(
        self,
        reader: &mut impl BufRead,
        writer: &mut impl Write,
    ) -> Result<StreamedMessage, MessageReadError> {
        let (raw, matching_header, limits) = self.into_serialized_parts();
        writer.write_all(&raw)?;
        let mut total = raw.len();
        read_body(
            reader,
            &limits,
            raw.len(),
            &mut Vec::new(),
            false,
            Some((&mut total, writer)),
        )?;

        Ok(StreamedMessage {
            header: raw,
            matching_header,
            len: total,
        })
    }
}

// Matching treats a continued field as one logical line, while delivery must
// retain the exact input. Allocate a second header only after finding folding;
// replacing CRLF or LF with one space cannot make it larger than the bounded
// raw header. The continuation's own leading whitespace remains untouched to
// match procmail's `concon(' ')` representation.
pub(crate) fn normalize_folded_header(raw: &[u8]) -> Option<Vec<u8>> {
    raw.iter().enumerate().find_map(|(index, byte)| {
        (*byte == b'\n' && matches!(raw.get(index + 1), Some(b' ' | b'\t'))).then_some(index)
    })?;
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
    Some(normalized)
}

impl StreamedMessage {
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    #[cfg(test)]
    pub(crate) fn matching_header(&self) -> &[u8] {
        self.matching_header.as_deref().unwrap_or(&self.header)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLimit {
    Message,
    Headers,
    Body,
    HeaderLine,
    HeaderField,
}

impl fmt::Display for MessageLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Message => "LIMIT_MSG_SIZE",
            Self::Headers => "LIMIT_MSG_HEADERS",
            Self::Body => "LIMIT_MSG_BODY",
            Self::HeaderLine => "LIMIT_HEADER_LINE",
            Self::HeaderField => "LIMIT_HEADER_FIELD",
        };
        formatter.write_str(name)
    }
}

#[derive(Debug)]
pub enum MessageReadError {
    Io(std::io::Error),
    LimitExceeded { kind: MessageLimit, limit: usize },
}

impl MessageReadError {
    fn limit(kind: MessageLimit, limit: usize) -> Self {
        Self::LimitExceeded { kind, limit }
    }
}

impl fmt::Display for MessageReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "input error: {error}"),
            Self::LimitExceeded { kind, limit } => {
                write!(formatter, "message exceeds {kind} ({limit} bytes)")
            }
        }
    }
}

impl std::error::Error for MessageReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::LimitExceeded { .. } => None,
        }
    }
}

impl From<std::io::Error> for MessageReadError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn read_header_line(
    reader: &mut impl BufRead,
    limits: &MessageLimits,
    headers_read: usize,
) -> Result<Option<Vec<u8>>, MessageReadError> {
    let mut line = Vec::new();

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let new_line_size = line.len().checked_add(take).ok_or_else(|| {
            MessageReadError::limit(MessageLimit::HeaderLine, limits.header_line_size)
        })?;

        check_size(
            new_line_size,
            limits.header_line_size,
            MessageLimit::HeaderLine,
        )?;
        check_size(
            headers_read + new_line_size,
            limits.headers_size,
            MessageLimit::Headers,
        )?;
        check_size(
            headers_read + new_line_size,
            limits.message_size,
            MessageLimit::Message,
        )?;

        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
    }
}

fn read_body(
    reader: &mut impl BufRead,
    limits: &MessageLimits,
    body_start: usize,
    raw: &mut Vec<u8>,
    retain: bool,
    mut stream: Option<(&mut usize, &mut dyn Write)>,
) -> Result<(), MessageReadError> {
    let mut body_size = 0usize;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        let total_size = stream.as_ref().map_or(raw.len(), |(total, _)| **total);
        let next_body_size = body_size
            .checked_add(available.len())
            .ok_or_else(|| MessageReadError::limit(MessageLimit::Body, limits.body_size))?;
        let next_total_size = total_size
            .checked_add(available.len())
            .ok_or_else(|| MessageReadError::limit(MessageLimit::Message, limits.message_size))?;
        check_size(next_body_size, limits.body_size, MessageLimit::Body)?;
        check_size(next_total_size, limits.message_size, MessageLimit::Message)?;

        let consumed = available.len();
        if retain {
            debug_assert_eq!(raw.len().saturating_sub(body_start), body_size);
            raw.extend_from_slice(available);
        }
        if let Some((total, writer)) = stream.as_mut() {
            writer.write_all(available)?;
            **total = next_total_size;
        }
        body_size = next_body_size;
        reader.consume(consumed);
    }
}

fn check_size(size: usize, limit: usize, kind: MessageLimit) -> Result<(), MessageReadError> {
    if size > limit {
        Err(MessageReadError::limit(kind, limit))
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn find_body_start(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .or_else(|| {
            raw.windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| index + 2)
        })
}

#[cfg(test)]
#[path = "tests/message.rs"]
mod tests;
