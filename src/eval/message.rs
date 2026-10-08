// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::sync::{Arc, OnceLock};

use crate::config::{ActionInput, ConditionInput};
use crate::header_edit::{EditedHeader, HeaderEditError, HeaderView};
use crate::limits::MessageLimits;
#[cfg(test)]
use crate::message::StreamedMessage;
use crate::message::{Message, MessageBytes, MessageHead};

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
    pub(super) selected: MessageBytes<'a>,
    pub(super) header: &'a [u8],
    pub(super) body: &'a [u8],
}

impl ExternalActionInput<'_> {
    pub fn selected(&self) -> MessageBytes<'_> {
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
pub(super) struct OwnedCompleteMessage<'input> {
    version: MessageVersion<'input>,
    matching: OnceLock<Vec<u8>>,
}

#[derive(Debug, Clone)]
enum BodyBacking<'input> {
    Original(&'input [u8]),
    Filter(Arc<Message>),
}

impl BodyBacking<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Original(bytes) => bytes,
            Self::Filter(message) => message.body(),
        }
    }
}

#[derive(Debug)]
enum MessageVersion<'input> {
    Whole(Arc<Message>),
    Headers {
        head: MessageHead,
        body: BodyBacking<'input>,
        len: usize,
    },
}

impl OwnedCompleteMessage<'_> {
    fn header_view(&self) -> HeaderView<'_> {
        match &self.version {
            MessageVersion::Whole(message) => HeaderView::Raw(message.header()),
            MessageVersion::Headers { head, .. } => head.header_view(),
        }
    }
    fn header(&self) -> &[u8] {
        match &self.version {
            MessageVersion::Whole(message) => message.header(),
            MessageVersion::Headers { head, .. } => head.as_bytes(),
        }
    }

    fn matching_header(&self) -> &[u8] {
        match &self.version {
            MessageVersion::Whole(message) => message.matching_header(),
            MessageVersion::Headers { head, .. } => head.matching_header(),
        }
    }

    fn body(&self) -> &[u8] {
        match &self.version {
            MessageVersion::Whole(message) => message.body(),
            MessageVersion::Headers { body, .. } => body.bytes(),
        }
    }

    fn len(&self) -> usize {
        match &self.version {
            MessageVersion::Whole(message) => message.len(),
            MessageVersion::Headers { len, .. } => *len,
        }
    }

    fn full(&self) -> &[u8] {
        // A regex may cross the header/body boundary, so it needs one range.
        // Delivery and ordinary header edits borrow the pieces instead. This
        // cache belongs to an immutable version and is shared by copy branches.
        if let MessageVersion::Whole(message) = &self.version {
            if self.header() == self.matching_header() {
                return message.as_bytes();
            }
        }

        self.matching.get_or_init(|| {
            let mut bytes = Vec::with_capacity(self.len());
            bytes.extend_from_slice(self.matching_header());
            bytes.extend_from_slice(self.body());
            bytes
        })
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct CurrentMessage<'input> {
    replacement: Option<Arc<OwnedCompleteMessage<'input>>>,
}

impl<'input> CurrentMessage<'input> {
    pub(super) fn replace(&mut self, message: Message) {
        // A new version owns a fresh cache. Copy branches can share this
        // immutable version and its eventual full regex view without copying
        // the body; replacing one branch cannot invalidate another branch.
        self.replacement = Some(Arc::new(OwnedCompleteMessage {
            version: MessageVersion::Whole(Arc::new(message)),
            matching: OnceLock::new(),
        }));
    }

    pub(super) fn replace_header(
        &mut self,
        edited: EditedHeader,
        original_body: &'input [u8],
        limits: MessageLimits,
    ) -> Result<(), HeaderEditError> {
        // Retain the body owner directly rather than the preceding header
        // version. Repeated edits must not form a chain that keeps old headers
        // and full-message caches alive. The mapped original outlives execution;
        // filter output has an Arc owner shared with any still-running branch.
        let body = match self.replacement.as_deref().map(|owned| &owned.version) {
            Some(MessageVersion::Whole(message)) => BodyBacking::Filter(message.clone()),
            Some(MessageVersion::Headers { body, .. }) => body.clone(),
            None => BodyBacking::Original(original_body),
        };
        let head = MessageHead::from_edited_header(edited, body.bytes().len(), limits)?;
        let len = head
            .len()
            .checked_add(body.bytes().len())
            .ok_or(HeaderEditError::SizeOverflow)?;
        self.replacement = Some(Arc::new(OwnedCompleteMessage {
            version: MessageVersion::Headers { head, body, len },
            matching: OnceLock::new(),
        }));
        Ok(())
    }

    pub(super) fn view<'a>(&'a self, original: CompleteMessage<'a>) -> CompleteMessage<'a> {
        match self.replacement.as_deref() {
            Some(replacement) => CompleteMessage::Owned(replacement),
            None => original,
        }
    }

    #[cfg(test)]
    pub(super) fn shares_replacement_with(&self, other: &Self) -> bool {
        match (&self.replacement, &other.replacement) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FinalMessage<'a> {
    storage: FinalStorage<'a>,
}

#[derive(Debug, Clone, Copy)]
enum FinalStorage<'a> {
    Bytes(&'a [u8]),
    Version(&'a OwnedCompleteMessage<'a>),
}

impl<'a> FinalMessage<'a> {
    pub(super) fn new(view: CompleteMessage<'a>) -> Option<Self> {
        let storage = match view {
            CompleteMessage::Owned(owned) => FinalStorage::Version(owned),
            CompleteMessage::Mapped { raw, .. } => FinalStorage::Bytes(raw),
            #[cfg(test)]
            CompleteMessage::Buffered { message, .. } => FinalStorage::Bytes(message.as_bytes()),
            #[cfg(test)]
            CompleteMessage::Streamed(_) => return None,
        };
        Some(Self { storage })
    }

    pub fn len(self) -> usize {
        match self.storage {
            FinalStorage::Bytes(bytes) => bytes.len(),
            FinalStorage::Version(owned) => owned.len(),
        }
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn write_to(self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        self.bytes().write_to(writer)
    }

    pub fn bytes(self) -> MessageBytes<'a> {
        match self.storage {
            FinalStorage::Bytes(bytes) => bytes.into(),
            FinalStorage::Version(owned) => MessageBytes::new(owned.header(), owned.body()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum CompleteMessage<'a> {
    Owned(&'a OwnedCompleteMessage<'a>),
    #[cfg(test)]
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
    pub(super) fn header_view(self) -> HeaderView<'a> {
        match self {
            Self::Owned(owned) => owned.header_view(),
            _ => HeaderView::Raw(self.raw_header()),
        }
    }
    pub(super) fn raw_header(self) -> &'a [u8] {
        match self {
            Self::Owned(owned) => owned.header(),
            #[cfg(test)]
            Self::Buffered { message, .. } => message.header(),
            #[cfg(test)]
            Self::Streamed(message) => message.header(),
            Self::Mapped {
                raw, header_len, ..
            } => &raw[..header_len],
        }
    }

    pub(super) fn action_input(self, input: ActionInput) -> Option<MessageBytes<'a>> {
        match input {
            ActionInput::Message => Some(FinalMessage::new(self)?.bytes()),
            ActionInput::Headers => Some(self.raw_header().into()),
            ActionInput::Body => self.body().map(Into::into),
        }
    }

    pub(super) fn program_input(self, input: ConditionInput) -> Option<MessageBytes<'a>> {
        match input {
            ConditionInput::Headers => Some(self.raw_header().into()),
            ConditionInput::Body => self.body().map(Into::into),
            ConditionInput::Message => Some(FinalMessage::new(self)?.bytes()),
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
            Self::Owned(owned) => owned.matching_header(),
            #[cfg(test)]
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
            Self::Owned(owned) => Some(owned.body()),
            #[cfg(test)]
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
            Self::Owned(owned) => Some(owned.full()),
            #[cfg(test)]
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
            Self::Owned(owned) => owned.len(),
            #[cfg(test)]
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
