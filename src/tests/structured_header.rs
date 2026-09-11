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
fn extracts_and_normalizes_list_id() {
    let mut found = Vec::new();
    any_identifier(
        b"List-Id: Project discussion <Project.Users.Example.ORG>\n\n",
        IdentifierField::ListId,
        |value| {
            found.push(value.to_vec());
            Ok::<bool, ()>(false)
        },
    )
    .unwrap();
    assert_eq!(found, [b"project.users.example.org".to_vec()]);
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
