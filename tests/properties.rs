// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io::Cursor;

use procmail_rs::config;
use procmail_rs::limits::MessageLimits;
use procmail_rs::message::Message;

#[test]
fn bounded_rc_corpus_always_finishes_parsing_and_expansion() {
    const ALPHABET: &[u8] = b":0*!?{}=$\n ";

    // Exhaust all short strings because punctuation at token boundaries finds
    // parser-state combinations that a collection of hand-written valid rc
    // files tends to miss. The fixed maximum keeps runtime and allocation
    // independent of external input.
    for length in 0..=4 {
        let combinations = ALPHABET.len().pow(length);
        for ordinal in 0..combinations {
            let source = generated_ascii_word(ALPHABET, length, ordinal);
            if let Ok(parsed) = config::parse(&source) {
                let _ = parsed.expand();
            }
        }
    }

    // Exercise deeper valid and nearly valid paths separately so the small
    // exhaustive alphabet is not responsible for reaching every AST shape.
    for prefix in ["", "VALUE=text\n", "LINEBUF=128\n"] {
        for condition in ["", "* ^Subject:", "* ! ^TO_user", "* < 64", "* ? true"] {
            for action in [
                "maildir:box",
                "mbox:box",
                "| true",
                "! user@example.test",
                "{\n:0\nmaildir:nested\n}",
            ] {
                let separator = if condition.is_empty() { "" } else { "\n" };
                let source = format!("{prefix}:0\n{condition}{separator}{action}\n");
                if let Ok(parsed) = config::parse(&source) {
                    let _ = parsed.expand();
                }
            }
        }
    }
}

#[test]
fn generated_binary_messages_preserve_framing_and_bytes() {
    let headers: [&[u8]; 4] = [
        b"",
        b"Subject: value\n",
        b"Subject: value\r\n",
        b"X-Binary: \0\xff\n folded\r\n",
    ];
    let separators: [&[u8]; 2] = [b"\n", b"\r\n"];
    let bodies: [&[u8]; 5] = [b"", b"body", b"body\n", b"\0\xff\r\n", b"\n\r\n\n"];

    for header in headers {
        for separator in separators {
            for body in bodies {
                let mut input = Vec::new();
                input.extend_from_slice(header);
                input.extend_from_slice(separator);
                input.extend_from_slice(body);

                let message =
                    Message::read_from(&mut Cursor::new(&input), MessageLimits::default()).unwrap();
                assert_eq!(message.as_bytes(), input);
                assert_eq!(message.header(), [header, separator].concat());
                assert_eq!(message.body(), body);
            }
        }

        let message =
            Message::read_from(&mut Cursor::new(header), MessageLimits::default()).unwrap();
        assert_eq!(message.as_bytes(), header);
        assert_eq!(message.header(), header);
        assert!(message.body().is_empty());
    }
}

#[test]
fn generated_message_lengths_obey_both_total_and_body_limits() {
    let config = config::parse(
        "LIMIT_MSG_SIZE=8\nLIMIT_MSG_HEADERS=2\nLIMIT_MSG_BODY=6\nLIMIT_HEADER_LINE=2\nLIMIT_HEADER_FIELD=2\n",
    )
    .unwrap();
    let limits = MessageLimits::from_config(&config).unwrap();

    for body_length in 0..=10 {
        let mut input = vec![b'\n'];
        input.extend(std::iter::repeat_n(b'x', body_length));
        let result = Message::read_from(&mut Cursor::new(&input), limits);
        if body_length <= 6 && input.len() <= 8 {
            assert_eq!(
                result.unwrap().as_bytes(),
                input,
                "body length {body_length}"
            );
        } else {
            assert!(result.is_err(), "accepted body length {body_length}");
        }
    }
}

fn generated_ascii_word(alphabet: &[u8], length: u32, mut ordinal: usize) -> String {
    let mut word = vec![alphabet[0]; length as usize];
    for byte in &mut word {
        *byte = alphabet[ordinal % alphabet.len()];
        ordinal /= alphabet.len();
    }
    String::from_utf8(word).unwrap()
}
