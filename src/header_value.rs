// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::bounded_bytes::{BoundedBytes, BoundedBytesError};

pub(crate) const RFC5322_HEADER_LINE_LIMIT: usize = 998;
const RFC2047_HEADER_LINE_LIMIT: usize = 76;
const RFC2047_INPUT_CHUNK: usize = 45;
const ENCODED_WORD_PREFIX: &[u8] = b"=?UTF-8?B?";
const ENCODED_WORD_SUFFIX: &[u8] = b"?=";
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneratedHeaderError {
    InvalidValue,
    LineTooLong,
    SizeOverflow,
}

impl GeneratedHeaderError {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::InvalidValue => {
                "contains a control character that cannot be represented in a header field"
            }
            Self::LineTooLong => "would exceed the RFC 5322 limit of 998 bytes per line",
            Self::SizeOverflow => "length overflows the supported range",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneratedHeaderEncoding {
    Ascii,
    Rfc2047,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodedHeaderError {
    MalformedWord,
    UnsupportedCharset,
    UnsupportedEncoding,
    InvalidText,
    TooLong,
}

impl DecodedHeaderError {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::MalformedWord => "malformed RFC 2047 encoded-word",
            Self::UnsupportedCharset => "unsupported RFC 2047 charset",
            Self::UnsupportedEncoding => "unsupported RFC 2047 encoding",
            Self::InvalidText => "decoded header value is not valid text",
            Self::TooLong => "decoded header value exceeds MAX_ASSIGNMENT_VALUE_LEN",
        }
    }
}

pub(crate) fn decode_rfc2047(input: &[u8], limit: usize) -> Result<Vec<u8>, DecodedHeaderError> {
    let mut output = BoundedBytes::with_capacity(limit, input.len());
    let mut cursor = 0usize;
    let mut previous_encoded = false;

    // Whitespace between adjacent encoded-words is presentation folding and
    // disappears after decoding. Other text is copied once and validated as
    // UTF-8 so a byte sequence cannot acquire a locale-dependent meaning.
    while cursor < input.len() {
        if input[cursor..].starts_with(b"=?") {
            let (end, decoded) = decode_word(&input[cursor..])?;
            append_decoded_text(&mut output, &decoded)?;
            cursor = cursor.checked_add(end).ok_or(DecodedHeaderError::TooLong)?;
            previous_encoded = true;
            continue;
        }

        let next =
            find_encoded_word(&input[cursor..]).map_or(input.len(), |offset| cursor + offset);
        let text = &input[cursor..next];
        if !(previous_encoded
            && next < input.len()
            && text.iter().all(|byte| matches!(byte, b' ' | b'\t')))
        {
            append_decoded_text(&mut output, text)?;
            previous_encoded = false;
        }
        cursor = next;
    }
    Ok(output.into_vec())
}

fn find_encoded_word(input: &[u8]) -> Option<usize> {
    input.windows(2).position(|bytes| bytes == b"=?")
}

fn decode_word(input: &[u8]) -> Result<(usize, Vec<u8>), DecodedHeaderError> {
    let first = input[2..]
        .iter()
        .position(|byte| *byte == b'?')
        .and_then(|offset| offset.checked_add(2))
        .ok_or(DecodedHeaderError::MalformedWord)?;
    let second = input[first + 1..]
        .iter()
        .position(|byte| *byte == b'?')
        .map(|offset| first + 1 + offset)
        .ok_or(DecodedHeaderError::MalformedWord)?;
    let encoded_start = second
        .checked_add(1)
        .ok_or(DecodedHeaderError::MalformedWord)?;
    let end = input[encoded_start..]
        .windows(2)
        .position(|bytes| bytes == b"?=")
        .and_then(|offset| encoded_start.checked_add(offset))
        .and_then(|offset| offset.checked_add(2))
        .ok_or(DecodedHeaderError::MalformedWord)?;
    if end > 75 {
        return Err(DecodedHeaderError::MalformedWord);
    }
    let charset = &input[2..first];
    let encoding = &input[first + 1..second];
    let encoded = &input[encoded_start..end - 2];
    if charset.is_empty() || encoding.len() != 1 || encoded.is_empty() || encoded.contains(&b'?') {
        return Err(DecodedHeaderError::MalformedWord);
    }
    let bytes = match encoding[0].to_ascii_uppercase() {
        b'B' => decode_base64(encoded)?,
        b'Q' => decode_q(encoded)?,
        _ => return Err(DecodedHeaderError::UnsupportedEncoding),
    };
    let text = transcode(charset, &bytes)?;
    Ok((end, text))
}

fn decode_base64(input: &[u8]) -> Result<Vec<u8>, DecodedHeaderError> {
    if input.len() % 4 != 0 {
        return Err(DecodedHeaderError::MalformedWord);
    }
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    for (index, chunk) in input.chunks_exact(4).enumerate() {
        let last = index + 1 == input.len() / 4;
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c = if chunk[2] == b'=' {
            if !last || chunk[3] != b'=' {
                return Err(DecodedHeaderError::MalformedWord);
            }
            0
        } else {
            base64_value(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            if !last {
                return Err(DecodedHeaderError::MalformedWord);
            }
            0
        } else {
            base64_value(chunk[3])?
        };
        if (chunk[2] == b'=' && b & 0x0f != 0) || (chunk[3] == b'=' && c & 0x03 != 0) {
            return Err(DecodedHeaderError::MalformedWord);
        }
        output.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            output.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            output.push((c << 6) | d);
        }
    }
    Ok(output)
}

fn base64_value(byte: u8) -> Result<u8, DecodedHeaderError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(DecodedHeaderError::MalformedWord),
    }
}

fn decode_q(input: &[u8]) -> Result<Vec<u8>, DecodedHeaderError> {
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    while cursor < input.len() {
        match input[cursor] {
            b'_' => {
                output.push(b' ');
                cursor += 1;
            }
            b'=' => {
                let high = input
                    .get(cursor + 1)
                    .copied()
                    .and_then(hex_value)
                    .ok_or(DecodedHeaderError::MalformedWord)?;
                let low = input
                    .get(cursor + 2)
                    .copied()
                    .and_then(hex_value)
                    .ok_or(DecodedHeaderError::MalformedWord)?;
                output.push((high << 4) | low);
                cursor += 3;
            }
            byte if matches!(byte, b'!'..=b'~') => {
                output.push(byte);
                cursor += 1;
            }
            _ => return Err(DecodedHeaderError::MalformedWord),
        }
    }
    Ok(output)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn transcode(charset: &[u8], input: &[u8]) -> Result<Vec<u8>, DecodedHeaderError> {
    if charset.eq_ignore_ascii_case(b"UTF-8") {
        std::str::from_utf8(input).map_err(|_| DecodedHeaderError::InvalidText)?;
        return Ok(input.to_vec());
    }
    if charset.eq_ignore_ascii_case(b"US-ASCII") {
        if input.iter().any(|byte| *byte > 0x7f) {
            return Err(DecodedHeaderError::InvalidText);
        }
        return Ok(input.to_vec());
    }
    if charset.eq_ignore_ascii_case(b"ISO-8859-1") {
        let mut output = Vec::with_capacity(input.len().saturating_mul(2));
        for byte in input {
            let mut encoded = [0u8; 2];
            output.extend_from_slice(char::from(*byte).encode_utf8(&mut encoded).as_bytes());
        }
        return Ok(output);
    }
    Err(DecodedHeaderError::UnsupportedCharset)
}

fn append_decoded_text(output: &mut BoundedBytes, text: &[u8]) -> Result<(), DecodedHeaderError> {
    let text = std::str::from_utf8(text).map_err(|_| DecodedHeaderError::InvalidText)?;
    if text
        .chars()
        .any(|character| character != '\t' && character.is_control())
    {
        return Err(DecodedHeaderError::InvalidText);
    }
    output
        .try_extend(text.as_bytes())
        .map_err(|_: BoundedBytesError| DecodedHeaderError::TooLong)
        .map(|_| ())
}

pub(crate) fn validate_generated_header_value(
    name: &str,
    value: &str,
) -> Result<GeneratedHeaderEncoding, GeneratedHeaderError> {
    let mut non_ascii = false;
    for character in value.chars() {
        if character == '\t' || character.is_ascii_graphic() || character == ' ' {
            continue;
        }
        if character.is_control() {
            return Err(GeneratedHeaderError::InvalidValue);
        }
        non_ascii = true;
    }

    if non_ascii {
        return Ok(GeneratedHeaderEncoding::Rfc2047);
    }
    let line_size = name
        .len()
        .checked_add(2)
        .and_then(|size| size.checked_add(value.len()))
        .ok_or(GeneratedHeaderError::SizeOverflow)?;
    if line_size > RFC5322_HEADER_LINE_LIMIT {
        return Err(GeneratedHeaderError::LineTooLong);
    }
    Ok(GeneratedHeaderEncoding::Ascii)
}

pub(crate) fn serialize_generated_header(
    name: &str,
    value: &str,
    line_ending: &[u8],
) -> Result<Vec<u8>, GeneratedHeaderError> {
    match validate_generated_header_value(name, value)? {
        GeneratedHeaderEncoding::Ascii => serialize_ascii(name, value, line_ending),
        GeneratedHeaderEncoding::Rfc2047 => serialize_rfc2047(name, value, line_ending),
    }
}

fn serialize_ascii(
    name: &str,
    value: &str,
    line_ending: &[u8],
) -> Result<Vec<u8>, GeneratedHeaderError> {
    let size = name
        .len()
        .checked_add(2)
        .and_then(|size| size.checked_add(value.len()))
        .and_then(|size| size.checked_add(line_ending.len()))
        .ok_or(GeneratedHeaderError::SizeOverflow)?;
    let mut field = Vec::with_capacity(size);
    append_canonical_header_name(&mut field, name);
    field.extend_from_slice(b": ");
    field.extend_from_slice(value.as_bytes());
    field.extend_from_slice(line_ending);
    Ok(field)
}

fn serialize_rfc2047(
    name: &str,
    value: &str,
    line_ending: &[u8],
) -> Result<Vec<u8>, GeneratedHeaderError> {
    let mut field = Vec::new();
    append_canonical_header_name(&mut field, name);
    field.push(b':');

    // Each chunk fits in one encoded-word and ends at a UTF-8 character
    // boundary. Folding every later word prevents an otherwise valid value
    // from exceeding RFC 2047's stricter limit for lines containing words.
    let mut start = 0usize;
    let mut first = true;
    while start < value.len() {
        let mut end = start
            .checked_add(RFC2047_INPUT_CHUNK)
            .map_or(value.len(), |end| end.min(value.len()));
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let encoded_len = base64_len(end - start)?;
        let word_len = ENCODED_WORD_PREFIX
            .len()
            .checked_add(encoded_len)
            .and_then(|size| size.checked_add(ENCODED_WORD_SUFFIX.len()))
            .ok_or(GeneratedHeaderError::SizeOverflow)?;
        let fits_first_line = first
            && field
                .len()
                .checked_add(1)
                .and_then(|size| size.checked_add(word_len))
                .is_some_and(|size| size <= RFC2047_HEADER_LINE_LIMIT);
        if fits_first_line {
            field.push(b' ');
        } else {
            field.extend_from_slice(line_ending);
            field.push(b' ');
        }
        field.extend_from_slice(ENCODED_WORD_PREFIX);
        append_base64(&mut field, &value.as_bytes()[start..end]);
        field.extend_from_slice(ENCODED_WORD_SUFFIX);
        start = end;
        first = false;
    }
    field.extend_from_slice(line_ending);
    Ok(field)
}

fn base64_len(input: usize) -> Result<usize, GeneratedHeaderError> {
    input
        .checked_add(2)
        .map(|size| size / 3)
        .and_then(|size| size.checked_mul(4))
        .ok_or(GeneratedHeaderError::SizeOverflow)
}

fn append_base64(output: &mut Vec<u8>, input: &[u8]) {
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(BASE64[usize::from(first >> 2)]);
        output.push(BASE64[usize::from(((first & 0x03) << 4) | (second >> 4))]);
        output.push(if chunk.len() > 1 {
            BASE64[usize::from(((second & 0x0f) << 2) | (third >> 6))]
        } else {
            b'='
        });
        output.push(if chunk.len() > 2 {
            BASE64[usize::from(third & 0x3f)]
        } else {
            b'='
        });
    }
}

pub(crate) fn append_canonical_header_name(output: &mut Vec<u8>, name: &str) {
    let mut beginning = true;
    for byte in name.bytes() {
        output.push(if beginning {
            byte.to_ascii_uppercase()
        } else {
            byte.to_ascii_lowercase()
        });
        beginning = byte == b'-';
    }
}

#[cfg(test)]
#[path = "tests/header_value.rs"]
mod tests;
