// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

use crate::config::{AddressField, IdentifierField};

pub(crate) const MAX_STRUCTURED_HEADER_VALUES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructuredHeaderError {
    message: String,
}

#[derive(Debug)]
pub(crate) enum StructuredVisitError<E> {
    Header(StructuredHeaderError),
    Visitor(E),
}

impl fmt::Display for StructuredHeaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub(crate) fn any_address<E>(
    header: &[u8],
    wanted: &[AddressField],
    mut matches: impl FnMut(&[u8]) -> Result<bool, E>,
) -> Result<bool, StructuredVisitError<E>> {
    let mut count = 0usize;
    visit_fields(header, |name, value| {
        let Some(_) = wanted
            .iter()
            .find(|field| name.eq_ignore_ascii_case(field.name().as_bytes()))
        else {
            return Ok(false);
        };
        let Some(value) = remove_comments_and_unfold(value) else {
            return Ok(false);
        };
        for element in mailbox_elements(&value) {
            let Some(address) = normalize_mailbox(element) else {
                continue;
            };
            count = count
                .checked_add(1)
                .ok_or_else(|| StructuredVisitError::Header(work_limit_error()))?;
            if count > MAX_STRUCTURED_HEADER_VALUES {
                return Err(StructuredVisitError::Header(work_limit_error()));
            }
            if matches(&address).map_err(StructuredVisitError::Visitor)? {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

pub(crate) fn any_identifier<E>(
    header: &[u8],
    field: IdentifierField,
    mut matches: impl FnMut(&[u8]) -> Result<bool, E>,
) -> Result<bool, StructuredVisitError<E>> {
    let mut count = 0usize;
    visit_fields(header, |name, value| {
        if !name.eq_ignore_ascii_case(field.name().as_bytes()) {
            return Ok(false);
        }
        let Some(value) = remove_comments_and_unfold(value) else {
            return Ok(false);
        };
        let Some(identifier) = normalize_list_id(&value) else {
            return Ok(false);
        };
        count = count
            .checked_add(1)
            .ok_or_else(|| StructuredVisitError::Header(work_limit_error()))?;
        if count > MAX_STRUCTURED_HEADER_VALUES {
            return Err(StructuredVisitError::Header(work_limit_error()));
        }
        matches(&identifier).map_err(StructuredVisitError::Visitor)
    })
}

fn visit_fields<E>(
    header: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<bool, E>,
) -> Result<bool, E> {
    let mut offset = 0usize;
    let mut field_start = None;
    while offset < header.len() {
        let line_end = header[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(header.len(), |end| offset + end + 1);
        let line = &header[offset..line_end];
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content.is_empty() {
            if let Some(start) = field_start {
                if visit_field(&header[start..offset], &mut visit)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        if !matches!(line.first(), Some(b' ' | b'\t')) {
            if let Some(start) = field_start.replace(offset) {
                if visit_field(&header[start..offset], &mut visit)? {
                    return Ok(true);
                }
            }
        }
        offset = line_end;
    }
    if let Some(start) = field_start {
        return visit_field(&header[start..], &mut visit);
    }
    Ok(false)
}

fn visit_field<E>(
    field: &[u8],
    visit: &mut impl FnMut(&[u8], &[u8]) -> Result<bool, E>,
) -> Result<bool, E> {
    let first_end = field
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(field.len());
    let first = &field[..first_end];
    let Some(colon) = first.iter().position(|byte| *byte == b':') else {
        return Ok(false);
    };
    visit(&first[..colon], &field[colon + 1..])
}

fn remove_comments_and_unfold(input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len());
    let mut comment_depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut pending_space = false;
    // Comments and folding can contain delimiters which must not split a
    // mailbox. Remove them in one bounded pass while retaining quoted-string
    // escapes; treating all punctuation alike would let a display-name comma
    // or an address-looking comment become a separate candidate.
    for &byte in input {
        if escaped {
            if comment_depth == 0 {
                output.push(byte);
            }
            escaped = false;
            continue;
        }
        if byte == b'\\' && (quoted || comment_depth > 0) {
            if comment_depth == 0 {
                output.push(byte);
            }
            escaped = true;
            continue;
        }
        if comment_depth > 0 {
            match byte {
                b'(' => comment_depth = comment_depth.checked_add(1)?,
                b')' => comment_depth -= 1,
                _ => {}
            }
            continue;
        }
        if !quoted && byte == b'(' {
            comment_depth = 1;
            continue;
        }
        if byte == b'"' {
            if pending_space && !output.is_empty() && !quoted {
                output.push(b' ');
            }
            pending_space = false;
            quoted = !quoted;
            output.push(byte);
            continue;
        }
        if !quoted && byte.is_ascii_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !output.is_empty() && !quoted {
            output.push(b' ');
        }
        pending_space = false;
        output.push(byte);
    }
    (comment_depth == 0 && !quoted && !escaped).then_some(output)
}

fn mailbox_elements(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut quoted = false;
    let mut angle = 0usize;
    let mut escaped = false;
    // A field may mix groups, quoted display names, and angle-addresses. Split
    // only at top-level list separators so every later parse sees one bounded
    // candidate and cannot mistake quoted punctuation for address structure.
    input.split(move |byte| {
        if escaped {
            escaped = false;
            return false;
        }
        if quoted && *byte == b'\\' {
            escaped = true;
            return false;
        }
        if *byte == b'"' {
            quoted = !quoted;
            return false;
        }
        if !quoted {
            match byte {
                b'<' => angle += 1,
                b'>' => angle = angle.saturating_sub(1),
                b',' | b';' if angle == 0 => return true,
                _ => {}
            }
        }
        false
    })
}

fn normalize_mailbox(element: &[u8]) -> Option<Vec<u8>> {
    let element = trim_ascii(element);
    if element.is_empty() {
        return None;
    }
    let source = if let Some(open) = find_unquoted(element, b'<') {
        let close = find_unquoted(&element[open + 1..], b'>')? + open + 1;
        if !trim_ascii(&element[close + 1..]).is_empty() {
            return None;
        }
        &element[open + 1..close]
    } else {
        let start = find_unquoted(element, b':').map_or(0, |colon| colon + 1);
        &element[start..]
    };
    normalize_addr_spec(trim_ascii(source))
}

fn normalize_addr_spec(input: &[u8]) -> Option<Vec<u8>> {
    let mut at = None;
    let mut quoted = false;
    let mut bracketed = false;
    let mut escaped = false;
    // Locate the one structural at-sign without decoding or converting the
    // local part. This prevents display text and quoted at-signs from changing
    // the selected domain while allowing domain normalization to stay ASCII
    // only and byte preserving everywhere else.
    for (index, &byte) in input.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && byte == b'\\' {
            escaped = true;
            continue;
        }
        match byte {
            b'"' if !bracketed => quoted = !quoted,
            b'[' if !quoted => bracketed = true,
            b']' if !quoted => bracketed = false,
            b'@' if !quoted && !bracketed => match at.replace(index) {
                None => {}
                Some(_) => return None,
            },
            _ => {}
        }
    }
    if quoted || bracketed || escaped {
        return None;
    }
    let at = at?;
    let local = trim_ascii(&input[..at]);
    let domain = trim_ascii(&input[at + 1..]);
    if !valid_local(local) || !valid_domain(domain) {
        return None;
    }
    let mut output = Vec::with_capacity(input.len());
    output.extend_from_slice(local);
    output.push(b'@');
    output.extend(domain.iter().map(|byte| byte.to_ascii_lowercase()));
    Some(output)
}

fn valid_local(local: &[u8]) -> bool {
    if local.len() >= 2 && local.first() == Some(&b'"') && local.last() == Some(&b'"') {
        let mut escaped = false;
        return local[1..local.len() - 1].iter().all(|byte| {
            if escaped {
                escaped = false;
                return byte.is_ascii() && !matches!(byte, b'\r' | b'\n');
            }
            if *byte == b'\\' {
                escaped = true;
                return true;
            }
            byte.is_ascii() && !matches!(byte, b'"' | b'\r' | b'\n' | 0)
        }) && !escaped;
    }
    valid_dot_atom(local)
}

fn valid_domain(domain: &[u8]) -> bool {
    if domain.len() >= 2 && domain.first() == Some(&b'[') && domain.last() == Some(&b']') {
        return domain[1..domain.len() - 1]
            .iter()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'[' | b']' | b'\\'));
    }
    valid_dot_atom(domain)
}

fn valid_dot_atom(value: &[u8]) -> bool {
    !value.is_empty()
        && value.first() != Some(&b'.')
        && value.last() != Some(&b'.')
        && !value.windows(2).any(|pair| pair == b"..")
        && value.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'/'
                        | b'='
                        | b'?'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'{'
                        | b'|'
                        | b'}'
                        | b'~'
                        | b'.'
                )
        })
}

fn normalize_list_id(input: &[u8]) -> Option<Vec<u8>> {
    let open = find_unquoted(input, b'<')?;
    let close = find_unquoted(&input[open + 1..], b'>')? + open + 1;
    if !trim_ascii(&input[close + 1..]).is_empty() {
        return None;
    }
    let identifier = trim_ascii(&input[open + 1..close]);
    if !valid_dot_atom(identifier) || !identifier.contains(&b'.') {
        return None;
    }
    Some(
        identifier
            .iter()
            .map(|byte| byte.to_ascii_lowercase())
            .collect(),
    )
}

fn find_unquoted(input: &[u8], wanted: u8) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, &byte) in input.iter().enumerate() {
        if escaped {
            escaped = false;
        } else if quoted && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && byte == wanted {
            return Some(index);
        }
    }
    None
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
        value = &value[1..];
    }
    while value.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
        value = &value[..value.len() - 1];
    }
    value
}

fn work_limit_error() -> StructuredHeaderError {
    StructuredHeaderError {
        message: format!(
            "structured header value count exceeds the hard limit of {MAX_STRUCTURED_HEADER_VALUES}"
        ),
    }
}

#[cfg(test)]
#[path = "tests/structured_header.rs"]
mod tests;
