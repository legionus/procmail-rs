// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

fn addresses(header: &[u8], fields: &[AddressField]) -> Vec<Vec<u8>> {
    let mut values = Vec::new();
    any_address(header, fields, |value| {
        values.push(value.to_vec());
        Ok::<bool, ()>(false)
    })
    .unwrap();
    values
}

#[test]
fn extracts_mailboxes_without_display_names_or_comments() {
    let header = b"From: \"fake@example.net\" <Real(User)@Example.ORG>\r\n\
To: Group: first@example.org, \"two words\"@Example.NET;\r\n\
Cc: folded@example.com,\r\n\tother@example.net\r\n\r\n";
    assert_eq!(
        addresses(
            header,
            &[AddressField::From, AddressField::To, AddressField::Cc]
        ),
        [
            b"Real@example.org".to_vec(),
            b"first@example.org".to_vec(),
            b"\"two words\"@example.net".to_vec(),
            b"folded@example.com".to_vec(),
            b"other@example.net".to_vec(),
        ]
    );
}

#[test]
fn ignores_malformed_mailboxes_and_unbalanced_comments() {
    let header = b"From: fake@example.net <real@example.org\nTo: value(comment@example.org\n\n";
    assert!(addresses(header, &[AddressField::From, AddressField::To]).is_empty());
}

#[test]
fn extracts_mailboxes_from_delivery_and_resent_headers() {
    let header = b"Delivered-To: <user+tag@Example.ORG>\r\n\
X-Original-To: \"Sub Address\" <user+orig@Example.NET>\r\n\
Envelope-To: env@example.com\r\n\
Resent-From: forwarder@Example.org\r\n\
Resent-Sender: sender@example.com\r\n\
Resent-To: \"Resent Recipient\" <resent-to@example.org>\r\n\
Resent-Cc: resent-cc@example.net\r\n\r\n";

    assert_eq!(
        addresses(header, &AddressField::DELIVERED),
        [
            b"user+tag@example.org".to_vec(),
            b"user+orig@example.net".to_vec(),
            b"env@example.com".to_vec(),
        ]
    );

    assert_eq!(
        addresses(header, &AddressField::SENDER),
        [
            b"forwarder@example.org".to_vec(),
            b"sender@example.com".to_vec(),
        ]
    );

    assert_eq!(
        addresses(header, &AddressField::RECIPIENT),
        [
            b"user+tag@example.org".to_vec(),
            b"user+orig@example.net".to_vec(),
            b"env@example.com".to_vec(),
            b"resent-to@example.org".to_vec(),
            b"resent-cc@example.net".to_vec(),
        ]
    );
}

#[test]
fn extracts_and_normalizes_list_id() {
    let mut found = Vec::new();
    any_identifier(
        b"List-Id: Project discussion <Project.Users.Example.ORG>\n\n",
        &[IdentifierField::ListId],
        |value| {
            found.push(value.to_vec());
            Ok::<bool, ()>(false)
        },
    )
    .unwrap();
    assert_eq!(found, [b"project.users.example.org".to_vec()]);
}

#[test]
fn extracts_message_identifiers_and_deduplicates_across_thread_fields() {
    let mut found = Vec::new();
    let header = b"Message-ID: <Msg.123@Example.ORG>\n\
In-Reply-To: (parent) <Parent.456@Example.ORG>\n\
References: <Root.000@Example.ORG> <Parent.456@Example.ORG>\n\n";

    any_identifier(header, &IdentifierField::THREAD, |value| {
        found.push(value.to_vec());
        Ok::<bool, ()>(false)
    })
    .unwrap();

    assert_eq!(
        found,
        [
            b"Msg.123@Example.ORG".to_vec(),
            b"Parent.456@Example.ORG".to_vec(),
            b"Root.000@Example.ORG".to_vec(),
        ]
    );
}

#[test]
fn extracts_references_with_folding_commas_and_ignores_malformed_tokens() {
    let mut found = Vec::new();
    let header = b"References: <first@example.org>,\n <second@example.org> malformed <>\n <third@example.org>\n\n";

    any_identifier(header, &[IdentifierField::References], |value| {
        found.push(value.to_vec());
        Ok::<bool, ()>(false)
    })
    .unwrap();

    assert_eq!(
        found,
        [
            b"first@example.org".to_vec(),
            b"second@example.org".to_vec(),
            b"third@example.org".to_vec(),
        ]
    );
}

#[test]
fn bounds_the_number_of_values_presented_to_the_regex() {
    for (count, accepted) in [
        (MAX_STRUCTURED_HEADER_VALUES - 1, true),
        (MAX_STRUCTURED_HEADER_VALUES, true),
        (MAX_STRUCTURED_HEADER_VALUES + 1, false),
    ] {
        let mut header = Vec::new();
        for _ in 0..count {
            header.extend_from_slice(b"From: user@example.org\n");
        }
        header.push(b'\n');
        let result = any_address(&header, &[AddressField::From], |_| Ok::<bool, ()>(false));
        assert_eq!(result.is_ok(), accepted, "value count: {count}");
        if let Err(error) = result {
            assert!(matches!(error, StructuredVisitError::Header(_)));
        }
    }
}

#[test]
fn ignores_non_ascii_and_control_bytes_in_address_syntax() {
    let header = b"From: user\xff@example.org, valid@example.org\n\
To: control\0@example.org\n\n";
    assert_eq!(
        addresses(header, &[AddressField::From, AddressField::To]),
        [b"valid@example.org".to_vec()]
    );
}

#[test]
fn collects_keywords_preserving_case_order_and_imap_atoms() {
    let header = b"Keywords: Work, work,\r\n\t$Forwarded, \xffinvalid\r\n\
x-label: Work, Urgent, bad\0tag\r\n\
X-Keywords: $Junk $NotJunk Urgent\r\n\r\n";
    assert_eq!(
        collect_keywords(header, &KeywordField::ALL).unwrap(),
        ["Work", "work", "$Forwarded", "Urgent", "$Junk", "$NotJunk"]
    );
    assert_eq!(
        collect_keywords(header, &[KeywordField::XLabel]).unwrap(),
        ["Work", "Urgent"]
    );
}

#[test]
fn bounds_collected_and_configured_keywords_at_the_ceiling() {
    for (count, accepted) in [
        (MAX_STRUCTURED_HEADER_VALUES - 1, true),
        (MAX_STRUCTURED_HEADER_VALUES, true),
        (MAX_STRUCTURED_HEADER_VALUES + 1, false),
    ] {
        let mut header = b"Keywords: ".to_vec();
        for index in 0..count {
            if index != 0 {
                header.extend_from_slice(b", ");
            }
            header.extend_from_slice(b"Work");
        }
        header.extend_from_slice(b"\n\n");
        assert_eq!(
            collect_keywords(&header, &KeywordField::ALL).is_ok(),
            accepted,
            "collected count: {count}"
        );

        let configured = std::iter::repeat_n("Work", count)
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            parse_configured_keywords(&configured).is_ok(),
            accepted,
            "configured count: {count}"
        );
    }
}

#[test]
fn validates_configured_keyword_lists() {
    assert_eq!(
        parse_configured_keywords("Work, Urgent\t$Forwarded  work, Work").unwrap(),
        ["Work", "Urgent", "$Forwarded", "work"]
    );
    for empty in ["", "   ", " , \t , "] {
        assert_eq!(
            parse_configured_keywords(empty),
            Err(ConfiguredKeywordError::Empty)
        );
    }
    for invalid in ["Work, \u{e9}", "bad\x07tag", "line\nbreak"] {
        assert_eq!(
            parse_configured_keywords(invalid),
            Err(ConfiguredKeywordError::InvalidToken)
        );
    }
}
