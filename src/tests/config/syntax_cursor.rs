// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn reads_words_without_allocating_or_losing_byte_offsets() {
    let source = " \talpha  b\u{e9}ta\trest";
    let mut cursor = SyntaxCursor::new(source);

    assert_eq!(cursor.word(), Some("alpha"));
    assert_eq!(cursor.offset(), 7);
    assert_eq!(cursor.word(), Some("b\u{e9}ta"));
    assert_eq!(cursor.remainder(), "rest");
    assert!(cursor.is_end());
}

#[test]
fn keyword_failure_does_not_consume_input() {
    let mut cursor = SyntaxCursor::new("  into VALUE");

    assert!(!cursor.keyword("to"));
    assert!(cursor.keyword("into"));
    assert_eq!(cursor.word(), Some("VALUE"));
    assert!(cursor.is_end());
}

#[test]
fn remainder_preserves_internal_whitespace_and_punctuation() {
    let mut cursor = SyntaxCursor::new("set Subject  ${TEXT:-hello world}: tail");

    assert_eq!(cursor.word(), Some("set"));
    assert_eq!(cursor.word(), Some("Subject"));
    assert_eq!(cursor.remainder(), "${TEXT:-hello world}: tail");
}
