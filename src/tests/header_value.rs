// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn base64_encoder_covers_complete_and_partial_groups() {
    for (input, expected) in [
        (&b"f"[..], &b"Zg=="[..]),
        (&b"fo"[..], &b"Zm8="[..]),
        (&b"foo"[..], &b"Zm9v"[..]),
        (&b"foobar"[..], &b"Zm9vYmFy"[..]),
    ] {
        let mut output = Vec::new();
        append_base64(&mut output, input);
        assert_eq!(output, expected);
    }
}

#[test]
fn leaves_ascii_values_unencoded_and_canonicalizes_the_name() {
    assert_eq!(
        serialize_generated_header("x-SPAM-status", "yes", b"\r\n").unwrap(),
        b"X-Spam-Status: yes\r\n"
    );
}

#[test]
fn encodes_utf8_as_an_rfc2047_b_word() {
    assert_eq!(
        serialize_generated_header(
            "subject",
            "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}",
            b"\r\n"
        )
        .unwrap(),
        b"Subject: =?UTF-8?B?0J/RgNC40LLQtdGC?=\r\n"
    );
}

#[test]
fn splits_long_values_at_utf8_boundaries_and_folds_each_word() {
    let value = "\u{e9}".repeat(50);
    let field = serialize_generated_header("Subject", &value, b"\r\n").unwrap();
    assert!(field.is_ascii());
    for line in field.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        assert!(line.len() <= RFC2047_HEADER_LINE_LIMIT, "{}", line.len());
    }
    for word in field.split(|byte| matches!(byte, b' ' | b'\r' | b'\n')) {
        if word.starts_with(b"=?UTF-8?B?") {
            assert!(word.len() <= 75, "{}", word.len());
            assert!(word.ends_with(b"?="));
        }
    }
}

#[test]
fn rejects_ascii_and_unicode_control_characters() {
    for value in ["nul\0byte", "line\nfeed", "delete\u{7f}", "control\u{85}"] {
        assert_eq!(
            validate_generated_header_value("Subject", value),
            Err(GeneratedHeaderError::InvalidValue),
            "{value:?}"
        );
    }
}

#[test]
fn decodes_supported_charsets_and_both_encodings() {
    for (input, expected) in [
        (
            &b"=?UTF-8?B?0J/RgNC40LLQtdGC?="[..],
            "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}".as_bytes(),
        ),
        (&b"=?utf-8?Q?caf=C3=A9?="[..], "caf\u{e9}".as_bytes()),
        (&b"=?US-ASCII?Q?hello_world?="[..], &b"hello world"[..]),
        (&b"=?ISO-8859-1?Q?caf=E9?="[..], "caf\u{e9}".as_bytes()),
    ] {
        assert_eq!(decode_rfc2047(input, 64).unwrap(), expected);
    }
}

#[test]
fn joins_adjacent_words_and_preserves_other_text() {
    assert_eq!(
        decode_rfc2047(b"prefix =?UTF-8?Q?one?= \t =?US-ASCII?B?dHdv?= suffix", 64,).unwrap(),
        b"prefix onetwo suffix"
    );
}

#[test]
fn rejects_unknown_or_malformed_encoded_words() {
    for (input, expected) in [
        (
            &b"=?KOI8-R?B?8NLJ18XU?="[..],
            DecodedHeaderError::UnsupportedCharset,
        ),
        (
            &b"=?UTF-8?X?text?="[..],
            DecodedHeaderError::UnsupportedEncoding,
        ),
        (&b"=?UTF-8?B?abc?="[..], DecodedHeaderError::MalformedWord),
        (&b"=?UTF-8?B?Zh==?="[..], DecodedHeaderError::MalformedWord),
        (
            &b"=?UTF-8?Q?bad=XX?="[..],
            DecodedHeaderError::MalformedWord,
        ),
        (&b"=?UTF-8?B?/w==?="[..], DecodedHeaderError::InvalidText),
        (&b"=?US-ASCII?Q?=80?="[..], DecodedHeaderError::InvalidText),
        (
            &b"=?UTF-8?Q?line=0Afeed?="[..],
            DecodedHeaderError::InvalidText,
        ),
        (&b"prefix =?broken"[..], DecodedHeaderError::MalformedWord),
    ] {
        assert_eq!(decode_rfc2047(input, 64), Err(expected), "{input:?}");
    }
}

#[test]
fn bounds_decoded_output_during_append() {
    for size in [7usize, 8, 9] {
        let result = decode_rfc2047(&vec![b'x'; size], 8);
        if size <= 8 {
            assert_eq!(result.unwrap().len(), size);
        } else {
            assert_eq!(result, Err(DecodedHeaderError::TooLong));
        }
    }
}
