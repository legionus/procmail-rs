// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;
use crate::config;

#[test]
fn applies_binary_size_suffixes() {
    let config =
        config::parse("LIMIT_MSG_SIZE=25M\nLIMIT_MSG_BODY=10k\nLIMIT_HEADER_LINE=1024\n").unwrap();

    let limits = MessageLimits::from_config(&config).unwrap();

    assert_eq!(limits.message_size, 25 * MIB);
    assert_eq!(limits.body_size, 10 * KIB);
    assert_eq!(limits.header_line_size, 1024);
}

#[test]
fn later_assignment_wins() {
    let config = config::parse("LIMIT_MSG_BODY=20K\nLIMIT_MSG_BODY=10K\n").unwrap();

    assert_eq!(
        MessageLimits::from_config(&config).unwrap().body_size,
        10 * KIB
    );
}

#[test]
fn rejects_values_above_hard_ceiling() {
    let config = config::parse("LIMIT_MSG_HEADERS=17M\n").unwrap();

    let error = MessageLimits::from_config(&config).unwrap_err();

    assert_eq!(error.line, 1);
    assert!(error.reason.contains("hard ceiling"));
}

#[test]
fn caps_message_and_body_for_32_bit_address_space() {
    for name in ["LIMIT_MSG_SIZE", "LIMIT_MSG_BODY"] {
        let at_limit = config::parse(&format!("{name}=256M\n")).unwrap();
        assert!(MessageLimits::from_config(&at_limit).is_ok());

        let above_limit = config::parse(&format!("{name}=256M\n{name}=268435457\n")).unwrap();
        assert!(MessageLimits::from_config(&above_limit).is_err());
    }
}

#[test]
fn rejects_spaces_and_unknown_suffixes() {
    for value in ["10 K", "10KB", "-1", ""] {
        let config = config::parse(&format!("LIMIT_MSG_BODY={value}\n")).unwrap();
        assert!(MessageLimits::from_config(&config).is_err(), "{value}");
    }
}
