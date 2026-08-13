// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Byte-oriented mboxrd record formatting.

use std::fmt;
use std::io::{self, Write};

pub const MAX_POSTMARK_LEN: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Postmark(Vec<u8>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostmarkError {
    TooLong,
    InvalidShape,
    NonAscii,
}

impl Postmark {
    pub fn new(bytes: &[u8]) -> Result<Self, PostmarkError> {
        if bytes.len() > MAX_POSTMARK_LEN {
            return Err(PostmarkError::TooLong);
        }
        if !bytes.starts_with(b"From ")
            || !bytes.ends_with(b"\n")
            || bytes[..bytes.len().saturating_sub(1)].contains(&b'\n')
        {
            return Err(PostmarkError::InvalidShape);
        }
        if !bytes[..bytes.len() - 1]
            .iter()
            .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
        {
            return Err(PostmarkError::NonAscii);
        }
        Ok(Self(bytes.to_vec()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for PostmarkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooLong => "mbox postmark exceeds its hard limit",
            Self::InvalidShape => "mbox postmark must be one LF-terminated 'From ' line",
            Self::NonAscii => "mbox postmark must contain only printable ASCII",
        })
    }
}

impl std::error::Error for PostmarkError {}

pub fn write_record(
    writer: &mut impl Write,
    postmark: &Postmark,
    message: &[u8],
) -> io::Result<()> {
    writer.write_all(postmark.as_bytes())?;

    // Quote directly from the input slices so one hostile line cannot cause a
    // second message-sized allocation. mboxrd adds one '>' to every physical
    // line whose first non-'>' bytes are exactly "From ".
    let mut offset = 0usize;
    while offset < message.len() {
        let relative_end = message[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(message.len() - offset, |index| index + 1);
        let end = offset + relative_end;
        let line = &message[offset..end];
        let prefix = line.iter().take_while(|byte| **byte == b'>').count();
        if line[prefix..].starts_with(b"From ") {
            writer.write_all(b">")?;
        }
        writer.write_all(line)?;
        offset = end;
    }

    if !message.ends_with(b"\n") {
        writer.write_all(b"\n")?;
    }
    writer.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::{MAX_POSTMARK_LEN, Postmark, PostmarkError, write_record};

    fn postmark() -> Postmark {
        Postmark::new(b"From MAILER-DAEMON Thu Jan  1 00:00:00 1970\n").unwrap()
    }

    #[test]
    fn quotes_every_mboxrd_line_reversibly() {
        let message = b"From hostile header\nX: value\n\nFrom body\n>From quoted\n>>From twice\nnot From safe\n";
        let mut output = Vec::new();

        write_record(&mut output, &postmark(), message).unwrap();

        assert_eq!(
            output,
            b"From MAILER-DAEMON Thu Jan  1 00:00:00 1970\n>From hostile header\nX: value\n\n>From body\n>>From quoted\n>>>From twice\nnot From safe\n\n"
        );
    }

    #[test]
    fn adds_record_separator_without_changing_input() {
        for (message, suffix) in [
            (&b"body"[..], &b"body\n\n"[..]),
            (&b"body\n"[..], &b"body\n\n"[..]),
            (&b"body\n\n"[..], &b"body\n\n\n"[..]),
            (&b""[..], &b"\n"[..]),
        ] {
            let original = message.to_vec();
            let mut output = Vec::new();
            write_record(&mut output, &postmark(), message).unwrap();
            assert!(output.ends_with(suffix));
            assert_eq!(message, original);
        }
    }

    #[test]
    fn validates_postmark_shape_and_limit() {
        assert_eq!(
            Postmark::new(b"not From\n"),
            Err(PostmarkError::InvalidShape)
        );
        assert_eq!(
            Postmark::new(b"From first\nsecond\n"),
            Err(PostmarkError::InvalidShape)
        );
        assert_eq!(
            Postmark::new(b"From non-ascii \xff\n"),
            Err(PostmarkError::NonAscii)
        );

        let mut boundary = b"From ".to_vec();
        boundary.resize(MAX_POSTMARK_LEN - 1, b'x');
        boundary.push(b'\n');
        assert!(Postmark::new(&boundary).is_ok());
        boundary.insert(MAX_POSTMARK_LEN - 1, b'x');
        assert_eq!(Postmark::new(&boundary), Err(PostmarkError::TooLong));
    }
}
