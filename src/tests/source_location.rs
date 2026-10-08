// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn rc_path_limit_is_checked_before_retaining_the_filename() {
    for length in [
        crate::config::MAX_PATH_EXPRESSION_LEN - 1,
        crate::config::MAX_PATH_EXPRESSION_LEN,
        crate::config::MAX_PATH_EXPRESSION_LEN + 1,
    ] {
        let path = "x".repeat(length);
        assert_eq!(
            SourceLocation::for_file(Path::new(&path), 1).is_ok(),
            length <= crate::config::MAX_PATH_EXPRESSION_LEN
        );
    }

    for path in ["", "bad\0path"] {
        assert!(SourceLocation::for_file(Path::new(path), 1).is_err());
    }
}

#[test]
fn lines_share_one_filename_without_copying_it() {
    let source = SourceLocation::for_file(Path::new("rules.rc"), 1).unwrap();
    let next = source.at_line(2);
    assert_eq!(source.line(), 1);
    assert_eq!(next.line(), 2);
    assert_eq!(next.file(), Some("rules.rc"));
    assert_eq!(
        source.file().unwrap().as_ptr(),
        next.file().unwrap().as_ptr()
    );
}
