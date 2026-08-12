use std::fmt;
use std::io::BufRead;
use std::ops::Range;

use crate::limits::MessageLimits;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    raw: Vec<u8>,
    header: Range<usize>,
    body: Range<usize>,
}

impl Message {
    #[cfg(test)]
    pub(crate) fn from_bytes(raw: Vec<u8>) -> Self {
        let body_start = find_body_start(&raw).unwrap_or(raw.len());

        Self {
            header: 0..body_start,
            body: body_start..raw.len(),
            raw,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn header(&self) -> &[u8] {
        &self.raw[self.header.clone()]
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
        let mut raw = Vec::with_capacity(limits.message_size.min(64 * 1024));
        let mut field_size = 0usize;
        let body_start;

        loop {
            let Some(line) = read_header_line(reader, &limits, raw.len())? else {
                body_start = raw.len();
                break;
            };
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
                body_start = raw.len();
                break;
            }
        }

        read_body(reader, &limits, body_start, &mut raw)?;

        Ok(Self {
            header: 0..body_start,
            body: body_start..raw.len(),
            raw,
        })
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
) -> Result<(), MessageReadError> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        let body_size = raw.len() - body_start;
        check_size(
            body_size + available.len(),
            limits.body_size,
            MessageLimit::Body,
        )?;
        check_size(
            raw.len() + available.len(),
            limits.message_size,
            MessageLimit::Message,
        )?;

        let consumed = available.len();
        raw.extend_from_slice(available);
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
mod tests {
    use std::io::{self, BufReader, Cursor, Read};

    use super::{Message, MessageLimit, MessageReadError};
    use crate::limits::MessageLimits;

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
}
