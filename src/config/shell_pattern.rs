// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

#[cfg(test)]
#[path = "../tests/config/shell_pattern.rs"]
mod tests;

pub(crate) const MAX_PATTERN_STEPS: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edge {
    Prefix,
    Suffix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Selection {
    Shortest,
    Longest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatternError {
    TooComplex { attempted: usize },
}

impl fmt::Display for PatternError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooComplex { attempted } => write!(
                formatter,
                "shell pattern requires {attempted} steps, exceeding the hard limit of {MAX_PATTERN_STEPS}"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(u8),
    AnyByte,
    AnyBytes,
    Class(ByteClass),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ByteClass {
    negated: bool,
    ranges: Vec<(u8, u8)>,
}

impl ByteClass {
    fn matches(&self, byte: u8) -> bool {
        let included = self
            .ranges
            .iter()
            .any(|&(start, end)| (start..=end).contains(&byte));
        included != self.negated
    }
}

pub(crate) fn remove(
    value: &[u8],
    pattern: &[u8],
    edge: Edge,
    selection: Selection,
) -> Result<Vec<u8>, PatternError> {
    let mut tokens = tokenize(pattern);
    let reversed = edge == Edge::Suffix;
    if reversed {
        tokens.reverse();
    }
    let input: Box<dyn Iterator<Item = u8> + '_> = if reversed {
        Box::new(value.iter().rev().copied())
    } else {
        Box::new(value.iter().copied())
    };
    let removed = matching_endpoint(input, value.len(), &tokens, selection)?;
    Ok(match (edge, removed) {
        (_, None) => value.to_vec(),
        (Edge::Prefix, Some(length)) => value[length..].to_vec(),
        (Edge::Suffix, Some(length)) => value[..value.len() - length].to_vec(),
    })
}

pub(crate) fn transform_matching_bytes(
    value: &[u8],
    pattern: &[u8],
    all: bool,
    mut transform: impl FnMut(u8) -> u8,
) -> Result<Vec<u8>, PatternError> {
    let tokens = tokenize(if pattern.is_empty() { b"?" } else { pattern });
    let candidates = if all {
        value.len()
    } else {
        usize::from(!value.is_empty())
    };
    check_steps(candidates, 1, &tokens)?;

    // Tokenize and account for the complete scan once. Checking each byte in
    // isolation would let a long value repeat a near-limit pattern match and
    // exceed the intended CPU ceiling without any individual call failing.
    let mut result = value.to_vec();
    let bytes = if all {
        result.as_mut_slice()
    } else {
        result.get_mut(..1).unwrap_or_default()
    };
    for byte in bytes {
        if matches!(
            matching_endpoint_unchecked(std::iter::once(*byte), &tokens, Selection::Longest),
            Some(1)
        ) {
            *byte = transform(*byte);
        }
    }
    Ok(result)
}

fn matching_endpoint(
    input: impl Iterator<Item = u8>,
    input_len: usize,
    tokens: &[Token],
    selection: Selection,
) -> Result<Option<usize>, PatternError> {
    check_steps(1, input_len, tokens)?;
    Ok(matching_endpoint_unchecked(input, tokens, selection))
}

fn check_steps(candidates: usize, input_len: usize, tokens: &[Token]) -> Result<(), PatternError> {
    let transition_cost = tokens.iter().fold(0usize, |total, token| {
        total.saturating_add(match token {
            Token::Class(class) => class.ranges.len().max(1),
            _ => 1,
        })
    });
    let attempted = candidates
        .checked_mul(input_len.saturating_add(1))
        .and_then(|total| total.checked_mul(transition_cost.max(1)));
    if attempted.is_none_or(|steps| steps > MAX_PATTERN_STEPS) {
        return Err(PatternError::TooComplex {
            attempted: attempted.unwrap_or(usize::MAX),
        });
    }
    Ok(())
}

fn matching_endpoint_unchecked(
    input: impl Iterator<Item = u8>,
    tokens: &[Token],
    selection: Selection,
) -> Option<usize> {
    let width = tokens.len().saturating_add(1);

    // Keep one NFA row per pattern token, rather than one entry per message
    // byte. This bounds auxiliary memory by the rc-controlled pattern size;
    // the separate step ceiling prevents a small hostile pattern and a large
    // variable value from consuming excessive CPU time.
    let mut states = vec![false; width];
    states[0] = true;
    close_stars(&mut states, tokens);
    let mut matched = states[tokens.len()].then_some(0);
    if matched.is_some() && selection == Selection::Shortest {
        return matched;
    }
    let mut next = vec![false; width];
    for (offset, byte) in input.enumerate() {
        next.fill(false);
        for (index, token) in tokens.iter().enumerate() {
            if !states[index] {
                continue;
            }
            match token {
                Token::AnyBytes => next[index] = true,
                Token::AnyByte => next[index + 1] = true,
                Token::Literal(expected) if *expected == byte => next[index + 1] = true,
                Token::Class(class) if class.matches(byte) => next[index + 1] = true,
                Token::Literal(_) | Token::Class(_) => {}
            }
        }
        close_stars(&mut next, tokens);
        std::mem::swap(&mut states, &mut next);
        if states[tokens.len()] {
            let endpoint = offset + 1;
            if selection == Selection::Shortest {
                return Some(endpoint);
            }
            matched = Some(endpoint);
        }
    }
    matched
}

fn close_stars(states: &mut [bool], tokens: &[Token]) {
    for (index, token) in tokens.iter().enumerate() {
        if states[index] && *token == Token::AnyBytes {
            states[index + 1] = true;
        }
    }
}

fn tokenize(pattern: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::with_capacity(pattern.len());
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            b'\\' if index + 1 < pattern.len() => {
                tokens.push(Token::Literal(pattern[index + 1]));
                index += 2;
            }
            b'\\' => {
                tokens.push(Token::Literal(b'\\'));
                index += 1;
            }
            b'*' => {
                if !matches!(tokens.last(), Some(Token::AnyBytes)) {
                    tokens.push(Token::AnyBytes);
                }
                index += 1;
            }
            b'?' => {
                tokens.push(Token::AnyByte);
                index += 1;
            }
            b'[' => {
                if let Some((class, next)) = parse_class(pattern, index + 1) {
                    tokens.push(Token::Class(class));
                    index = next;
                } else {
                    tokens.push(Token::Literal(b'['));
                    index += 1;
                }
            }
            byte => {
                tokens.push(Token::Literal(byte));
                index += 1;
            }
        }
    }
    tokens
}

fn parse_class(pattern: &[u8], mut index: usize) -> Option<(ByteClass, usize)> {
    let mut negated = false;
    if matches!(pattern.get(index), Some(b'!' | b'^')) {
        negated = true;
        index += 1;
    }
    let mut bytes = Vec::new();
    if pattern.get(index) == Some(&b']') {
        bytes.push((b']', false));
        index += 1;
    }
    while index < pattern.len() && pattern[index] != b']' {
        let (byte, quoted) = if pattern[index] == b'\\' && index + 1 < pattern.len() {
            index += 1;
            (pattern[index], true)
        } else {
            (pattern[index], false)
        };
        bytes.push((byte, quoted));
        index += 1;
    }
    if index == pattern.len() || bytes.is_empty() {
        return None;
    }

    let mut ranges = Vec::new();
    let mut item = 0;
    while item < bytes.len() {
        if item + 2 < bytes.len() && bytes[item + 1] == (b'-', false) {
            ranges.push((bytes[item].0, bytes[item + 2].0));
            item += 3;
        } else {
            ranges.push((bytes[item].0, bytes[item].0));
            item += 1;
        }
    }
    Some((ByteClass { negated, ranges }, index + 1))
}
