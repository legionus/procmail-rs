// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn later_maildir_assignment_selects_staging_base() {
    let config = parse("MAILDIR=old\nMAILDIR=/srv/mail\n").unwrap();

    assert_eq!(config.maildir(), Some("/srv/mail"));
}
