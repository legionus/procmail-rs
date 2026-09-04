// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn later_maildir_assignment_selects_staging_base() {
    let config = parse("MAILDIR=old\nMAILDIR=/srv/mail\n").unwrap();

    assert_eq!(config.maildir(), Some("/srv/mail"));
}

#[test]
fn destination_kind_defines_delivery_behavior() {
    let cases = [
        (
            Destination::Maildir("maildir".into()),
            DestinationKind::Maildir,
            false,
            true,
        ),
        (
            Destination::Mbox("mbox".into()),
            DestinationKind::Mbox,
            true,
            false,
        ),
        (
            Destination::File("file".into()),
            DestinationKind::File,
            true,
            false,
        ),
        (
            Destination::Discard("/dev/null".into()),
            DestinationKind::Discard,
            false,
            true,
        ),
    ];

    for (destination, kind, ordered, fanout) in cases {
        assert_eq!(destination.kind(), kind);
        assert_eq!(destination.requires_ordered_delivery(), ordered);
        assert_eq!(destination.supports_fanout_delivery(), fanout);
    }
}
