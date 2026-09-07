// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::config::{ActionInput, ConditionInput};
use crate::message::Message;
#[cfg(test)]
use crate::message::StreamedMessage;

#[derive(Debug, Clone, Copy)]
pub struct MatchingMessage<'a> {
    header: &'a [u8],
    full: Option<&'a [u8]>,
}

impl<'a> MatchingMessage<'a> {
    pub fn from_normalized_parts(header: &'a [u8], full: Option<&'a [u8]>) -> Self {
        Self { header, full }
    }

    pub(super) fn into_parts(self) -> (&'a [u8], Option<&'a [u8]>) {
        (self.header, self.full)
    }
}

#[derive(Debug)]
pub struct PreparedMatchingMessage {
    full: Option<Vec<u8>>,
}

impl PreparedMatchingMessage {
    pub fn new(message: &Message, needs_full: bool) -> Self {
        Self {
            full: needs_full.then(|| message.matching_message()).flatten(),
        }
    }

    pub fn views<'a>(&'a self, message: &'a Message) -> MatchingMessage<'a> {
        MatchingMessage::from_normalized_parts(message.matching_header(), self.full.as_deref())
    }

    #[cfg(test)]
    pub(super) fn complete<'a>(&'a self, message: &'a Message) -> CompleteMessage<'a> {
        CompleteMessage::Buffered {
            message,
            matching_full: self.full.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MappedMessageInput<'a> {
    pub(super) raw: &'a [u8],
    pub(super) header_len: usize,
    pub(super) matching: Option<MatchingMessage<'a>>,
}

impl<'a> MappedMessageInput<'a> {
    pub fn new(raw: &'a [u8], header_len: usize, matching: Option<MatchingMessage<'a>>) -> Self {
        Self {
            raw,
            header_len,
            matching,
        }
    }

    // Build borrowed views only after checking every related length. Later
    // matching code can then slice the raw message without repeating checks,
    // while a normalized header cannot be paired with an unrelated full view.
    pub(super) fn complete_message(self, needs_matching_raw: bool) -> Option<CompleteMessage<'a>> {
        if self.header_len > self.raw.len() {
            return None;
        }
        let (matching_header, matching_raw) = self
            .matching
            .map(|message| {
                let (header, full) = message.into_parts();
                (Some(header), full)
            })
            .unwrap_or((None, None));
        if !matching_views_are_valid(
            self.raw.len(),
            self.header_len,
            matching_header,
            matching_raw,
            needs_matching_raw,
        ) {
            return None;
        }
        Some(CompleteMessage::Mapped {
            raw: self.raw,
            header_len: self.header_len,
            matching_header,
            matching_raw,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ExternalActionInput<'a> {
    pub(super) selected: &'a [u8],
    pub(super) header: &'a [u8],
    pub(super) body: &'a [u8],
}

impl ExternalActionInput<'_> {
    pub fn selected(&self) -> &[u8] {
        self.selected
    }

    pub fn header(&self) -> &[u8] {
        self.header
    }

    pub fn body(&self) -> &[u8] {
        self.body
    }
}

#[derive(Debug)]
pub(super) struct OwnedCompleteMessage {
    pub(super) message: Message,
    pub(super) matching: PreparedMatchingMessage,
}

#[derive(Debug, Clone, Copy)]
pub struct FinalMessage<'a> {
    bytes: &'a [u8],
}

impl<'a> FinalMessage<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub fn as_bytes(self) -> &'a [u8] {
        self.bytes
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum CompleteMessage<'a> {
    Buffered {
        message: &'a Message,
        matching_full: Option<&'a [u8]>,
    },
    #[cfg(test)]
    Streamed(&'a StreamedMessage),
    Mapped {
        raw: &'a [u8],
        header_len: usize,
        matching_header: Option<&'a [u8]>,
        matching_raw: Option<&'a [u8]>,
    },
}

pub(super) fn current_ordered_message<'a>(
    original: CompleteMessage<'a>,
    replacement: Option<&'a OwnedCompleteMessage>,
) -> CompleteMessage<'a> {
    match replacement {
        Some(replacement) => CompleteMessage::Buffered {
            message: &replacement.message,
            matching_full: replacement.matching.full.as_deref(),
        },
        None => original,
    }
}

fn matching_views_are_valid(
    raw_len: usize,
    header_len: usize,
    matching_header: Option<&[u8]>,
    matching_raw: Option<&[u8]>,
    needs_matching_raw: bool,
) -> bool {
    // Normalizing CRLF folding can shorten the header, so validate the two
    // borrowed views by their independently known pieces rather than reusing
    // the raw header offset. A full HB view is mandatory whenever a changed
    // header could otherwise make matching fall back to delivery bytes.
    match (matching_header, matching_raw) {
        (None, None) => true,
        (Some(_), None) => !needs_matching_raw,
        (Some(header), Some(full)) => header
            .len()
            .checked_add(raw_len - header_len)
            .is_some_and(|expected| expected == full.len()),
        (None, Some(_)) => false,
    }
}

impl<'a> CompleteMessage<'a> {
    pub(super) fn raw(self) -> Option<&'a [u8]> {
        match self {
            Self::Buffered { message, .. } => Some(message.as_bytes()),
            #[cfg(test)]
            Self::Streamed(_) => None,
            Self::Mapped { raw, .. } => Some(raw),
        }
    }

    pub(super) fn raw_header(self) -> &'a [u8] {
        match self {
            Self::Buffered { message, .. } => message.header(),
            #[cfg(test)]
            Self::Streamed(message) => message.header(),
            Self::Mapped {
                raw, header_len, ..
            } => &raw[..header_len],
        }
    }

    pub(super) fn action_input(self, input: ActionInput) -> Option<&'a [u8]> {
        match input {
            ActionInput::Message => self.raw(),
            ActionInput::Headers => Some(self.raw_header()),
            ActionInput::Body => self.body(),
        }
    }

    pub(super) fn program_input(self, input: ConditionInput) -> Option<&'a [u8]> {
        match input {
            ConditionInput::Headers => Some(self.raw_header()),
            ConditionInput::Body => self.body(),
            ConditionInput::Message => self.raw(),
        }
    }

    pub(super) fn matching_input(self, input: ConditionInput) -> Option<&'a [u8]> {
        match input {
            ConditionInput::Headers => Some(self.header_bytes()),
            ConditionInput::Body => self.body(),
            ConditionInput::Message => self.full(),
        }
    }

    pub(super) fn header_bytes(self) -> &'a [u8] {
        match self {
            Self::Buffered { message, .. } => message.matching_header(),
            #[cfg(test)]
            Self::Streamed(message) => message.matching_header(),
            Self::Mapped {
                raw,
                header_len,
                matching_header,
                matching_raw: _,
            } => matching_header.unwrap_or(&raw[..header_len]),
        }
    }

    pub(super) fn body(self) -> Option<&'a [u8]> {
        match self {
            Self::Buffered { message, .. } => Some(message.body()),
            #[cfg(test)]
            Self::Streamed(_) => None,
            Self::Mapped {
                raw, header_len, ..
            } => Some(&raw[header_len..]),
        }
    }

    pub(super) fn full(self) -> Option<&'a [u8]> {
        match self {
            Self::Buffered {
                message,
                matching_full,
            } => Some(matching_full.unwrap_or_else(|| message.as_bytes())),
            #[cfg(test)]
            Self::Streamed(_) => None,
            Self::Mapped {
                raw, matching_raw, ..
            } => Some(matching_raw.unwrap_or(raw)),
        }
    }

    pub(super) fn len(self) -> usize {
        match self {
            Self::Buffered { message, .. } => message.len(),
            #[cfg(test)]
            Self::Streamed(message) => message.len(),
            Self::Mapped { raw, .. } => raw.len(),
        }
    }
}

#[cfg(test)]
#[path = "../tests/eval/message.rs"]
mod tests;
