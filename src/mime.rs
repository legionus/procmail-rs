// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use crate::header_edit::HeaderView;
use crate::structured_header::{
    StructuredVisitError, remove_comments_and_unfold, work_limit_error,
};

pub(crate) const MAX_MIME_DEPTH: usize = 8;
pub(crate) const MAX_MIME_PARTS: usize = 256;
const MAX_BOUNDARY_LEN: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MimePart {
    pub(crate) mimetype: Option<Vec<u8>>,
    pub(crate) disposition: Option<Vec<u8>>,
    pub(crate) filename: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedContentType {
    pub(crate) mimetype: Option<Vec<u8>>,
    pub(crate) boundary: Option<Vec<u8>>,
    pub(crate) name: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedContentDisposition {
    pub(crate) disposition: Option<Vec<u8>>,
    pub(crate) filename: Option<Vec<u8>>,
}

pub(crate) fn parse_content_type(value: &[u8]) -> ParsedContentType {
    let Some(cleaned) = remove_comments_and_unfold(value) else {
        return ParsedContentType::default();
    };

    let mut parts = split_mime_parameters(&cleaned);
    let Some(first) = parts.next() else {
        return ParsedContentType::default();
    };

    let mimetype = parse_media_type(first);
    let mut boundary = None;
    let mut name = None;

    for param in parts {
        let Some((param_name, param_value)) = parse_parameter(param) else {
            continue;
        };
        if param_name == b"boundary" && boundary.is_none() {
            if !param_value.is_empty() && param_value.len() <= MAX_BOUNDARY_LEN {
                boundary = Some(param_value);
            }
        } else if param_name == b"name" && name.is_none() && !param_value.is_empty() {
            name = Some(param_value);
        }
    }

    ParsedContentType {
        mimetype,
        boundary,
        name,
    }
}

pub(crate) fn parse_content_disposition(value: &[u8]) -> ParsedContentDisposition {
    let Some(cleaned) = remove_comments_and_unfold(value) else {
        return ParsedContentDisposition::default();
    };

    let mut parts = split_mime_parameters(&cleaned);
    let Some(first) = parts.next() else {
        return ParsedContentDisposition::default();
    };

    let disposition = parse_disposition_type(first);
    let mut filename = None;

    for param in parts {
        let Some((param_name, param_value)) = parse_parameter(param) else {
            continue;
        };
        if param_name == b"filename" && filename.is_none() && !param_value.is_empty() {
            filename = Some(param_value);
        }
    }

    ParsedContentDisposition {
        disposition,
        filename,
    }
}

pub(crate) fn part_from_headers(header: HeaderView<'_>) -> (MimePart, Option<Vec<u8>>) {
    let mut content_type = None;
    let mut content_disposition = None;

    for field in header.fields() {
        let (Some(name), Some(value)) = (field.name, field.value()) else {
            continue;
        };
        if name.eq_ignore_ascii_case(b"Content-Type") && content_type.is_none() {
            content_type = Some(parse_content_type(value));
        } else if name.eq_ignore_ascii_case(b"Content-Disposition") && content_disposition.is_none()
        {
            content_disposition = Some(parse_content_disposition(value));
        }
    }

    let ct = content_type.unwrap_or_default();
    let cd = content_disposition.unwrap_or_default();
    let filename = cd.filename.or(ct.name);
    let boundary = ct.boundary;

    (
        MimePart {
            mimetype: ct.mimetype,
            disposition: cd.disposition,
            filename,
        },
        boundary,
    )
}

pub(crate) fn any_mime_part<'a, E>(
    root_header: impl Into<HeaderView<'a>>,
    body: &[u8],
    mut visit: impl FnMut(&MimePart) -> Result<bool, E>,
) -> Result<bool, StructuredVisitError<E>> {
    let (root_part, root_boundary) = part_from_headers(root_header.into());
    let mut count = 0usize;

    count = count
        .checked_add(1)
        .ok_or_else(|| StructuredVisitError::Header(work_limit_error()))?;
    if count > MAX_MIME_PARTS {
        return Err(StructuredVisitError::Header(work_limit_error()));
    }

    if visit(&root_part).map_err(StructuredVisitError::Visitor)? {
        return Ok(true);
    }

    let is_multipart = root_part
        .mimetype
        .as_deref()
        .is_some_and(|m| m.starts_with(b"multipart/"));

    if is_multipart {
        if let Some(boundary) = root_boundary {
            return walk_multipart(body, &boundary, 1, &mut count, &mut visit);
        }
    }

    Ok(false)
}

fn walk_multipart<E>(
    body: &[u8],
    boundary: &[u8],
    depth: usize,
    count: &mut usize,
    visit: &mut impl FnMut(&MimePart) -> Result<bool, E>,
) -> Result<bool, StructuredVisitError<E>> {
    if depth > MAX_MIME_DEPTH || boundary.is_empty() {
        return Ok(false);
    }

    let delimiter = [b"--", boundary].concat();

    let Some(first_boundary_start) = find_boundary(body, 0, &delimiter) else {
        return Ok(false);
    };

    let after_first = first_boundary_start + delimiter.len();
    if body
        .get(after_first..)
        .is_some_and(|s| s.starts_with(b"--"))
    {
        return Ok(false);
    }

    let part_content_start = find_line_end(body, after_first);
    let mut current_part_start = part_content_start;
    let mut search_from = current_part_start;

    while let Some(next_boundary_start) = find_boundary(body, search_from, &delimiter) {
        let mut part_content_end = next_boundary_start;
        if part_content_end > current_part_start && body[part_content_end - 1] == b'\n' {
            part_content_end -= 1;
            if part_content_end > current_part_start && body[part_content_end - 1] == b'\r' {
                part_content_end -= 1;
            }
        }

        let part_bytes = &body[current_part_start..part_content_end];
        let (part_headers, part_body) = split_part_headers_and_body(part_bytes);

        *count = count
            .checked_add(1)
            .ok_or_else(|| StructuredVisitError::Header(work_limit_error()))?;
        if *count > MAX_MIME_PARTS {
            return Err(StructuredVisitError::Header(work_limit_error()));
        }

        let (part, sub_boundary) = part_from_headers(HeaderView::Raw(part_headers));

        if visit(&part).map_err(StructuredVisitError::Visitor)? {
            return Ok(true);
        }

        let is_sub_multipart = part
            .mimetype
            .as_deref()
            .is_some_and(|m| m.starts_with(b"multipart/"));

        if is_sub_multipart {
            if let Some(inner_boundary) = sub_boundary {
                if walk_multipart(part_body, &inner_boundary, depth + 1, count, visit)? {
                    return Ok(true);
                }
            }
        }

        let after_delim = next_boundary_start + delimiter.len();
        if body
            .get(after_delim..)
            .is_some_and(|s| s.starts_with(b"--"))
        {
            break;
        }

        current_part_start = find_line_end(body, after_delim);
        search_from = current_part_start;
    }

    Ok(false)
}

fn find_boundary(body: &[u8], start: usize, delimiter: &[u8]) -> Option<usize> {
    if start >= body.len() || delimiter.is_empty() {
        return None;
    }

    let mut index = start;
    while index + delimiter.len() <= body.len() {
        let at_line_start = index == 0 || body[index - 1] == b'\n';
        if at_line_start && body[index..].starts_with(delimiter) {
            return Some(index);
        }
        index += 1;
    }

    None
}

fn find_line_end(body: &[u8], offset: usize) -> usize {
    let mut index = offset;
    while index < body.len() {
        if body[index] == b'\n' {
            return index + 1;
        }
        index += 1;
    }
    body.len()
}

fn split_part_headers_and_body(part_bytes: &[u8]) -> (&[u8], &[u8]) {
    let mut i = 0;
    while i < part_bytes.len() {
        if part_bytes[i] == b'\n' {
            if i + 1 < part_bytes.len() && part_bytes[i + 1] == b'\n' {
                return (&part_bytes[..i + 2], &part_bytes[i + 2..]);
            }
            if i + 2 < part_bytes.len() && part_bytes[i + 1] == b'\r' && part_bytes[i + 2] == b'\n'
            {
                return (&part_bytes[..i + 3], &part_bytes[i + 3..]);
            }
        }
        i += 1;
    }
    (part_bytes, &[])
}

fn split_mime_parameters(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut quoted = false;
    let mut escaped = false;
    input.split(move |&byte| {
        if escaped {
            escaped = false;
            return false;
        }
        if quoted && byte == b'\\' {
            escaped = true;
            return false;
        }
        if byte == b'"' {
            quoted = !quoted;
            return false;
        }
        !quoted && byte == b';'
    })
}

fn parse_parameter(param: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let param = trim_ascii(param);
    let eq = find_unquoted_char(param, b'=')?;
    let name = trim_ascii(&param[..eq]);
    let value = trim_ascii(&param[eq + 1..]);
    if name.is_empty() {
        return None;
    }

    let is_extended = name.ends_with(b"*");
    let clean_name = if is_extended {
        name[..name.len() - 1].to_ascii_lowercase()
    } else {
        name.to_ascii_lowercase()
    };

    let unquoted = unquote_mime_value(value);
    let final_value = if is_extended {
        decode_rfc2231(&unquoted)
    } else if unquoted.windows(2).any(|w| w == b"=?") {
        crate::header_value::decode_rfc2047(&unquoted, crate::config::MAX_ASSIGNMENT_VALUE_LEN)
            .unwrap_or(unquoted)
    } else {
        unquoted
    };

    Some((clean_name, final_value))
}

fn unquote_mime_value(value: &[u8]) -> Vec<u8> {
    let value = trim_ascii(value);
    if value.len() >= 2 && value.first() == Some(&b'"') && value.last() == Some(&b'"') {
        let inner = &value[1..value.len() - 1];
        let mut out = Vec::with_capacity(inner.len());
        let mut escaped = false;
        for &byte in inner {
            if escaped {
                out.push(byte);
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else {
                out.push(byte);
            }
        }
        out
    } else {
        value.to_vec()
    }
}

fn decode_rfc2231(value: &[u8]) -> Vec<u8> {
    let payload = if let Some(first_quote) = value.iter().position(|&b| b == b'\'') {
        if let Some(second_quote) = value[first_quote + 1..].iter().position(|&b| b == b'\'') {
            &value[first_quote + 1 + second_quote + 1..]
        } else {
            value
        }
    } else {
        value
    };
    percent_decode(payload)
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(h1), Some(h2)) =
                (from_hex_digit(input[i + 1]), from_hex_digit(input[i + 2]))
            {
                out.push((h1 << 4) | h2);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn from_hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_media_type(input: &[u8]) -> Option<Vec<u8>> {
    let input = trim_ascii(input);
    let slash = input.iter().position(|&b| b == b'/')?;
    let main_type = &input[..slash];
    let subtype = &input[slash + 1..];
    if main_type.is_empty() || subtype.is_empty() {
        return None;
    }
    if !is_mime_token(main_type) || !is_mime_token(subtype) {
        return None;
    }
    let mut out = Vec::with_capacity(main_type.len() + 1 + subtype.len());
    out.extend(main_type.iter().map(|b| b.to_ascii_lowercase()));
    out.push(b'/');
    out.extend(subtype.iter().map(|b| b.to_ascii_lowercase()));
    Some(out)
}

fn parse_disposition_type(input: &[u8]) -> Option<Vec<u8>> {
    let input = trim_ascii(input);
    if !is_mime_token(input) {
        return None;
    }
    Some(input.iter().map(|b| b.to_ascii_lowercase()).collect())
}

fn is_mime_token(token: &[u8]) -> bool {
    !token.is_empty()
        && token.iter().all(|&b| {
            b.is_ascii_graphic()
                && !matches!(
                    b,
                    b'(' | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b':'
                        | b'\\'
                        | b'"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                )
        })
}

fn find_unquoted_char(input: &[u8], target: u8) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, &byte) in input.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && byte == b'\\' {
            escaped = true;
            continue;
        }
        if byte == b'"' {
            quoted = !quoted;
            continue;
        }
        if !quoted && byte == target {
            return Some(index);
        }
    }
    None
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(0, |idx| idx + 1);
    if start >= end {
        &[]
    } else {
        &bytes[start..end]
    }
}

#[cfg(test)]
#[path = "tests/mime.rs"]
mod tests;
