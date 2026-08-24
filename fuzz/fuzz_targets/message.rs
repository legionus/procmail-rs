// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![no_main]

use std::io::{BufReader, Cursor};

use libfuzzer_sys::fuzz_target;
use procmail_rs::limits::MessageLimits;
use procmail_rs::message::Message;

fuzz_target!(|data: &[u8]| {
    let mut reader = BufReader::new(Cursor::new(data));
    let _ = Message::read_from(&mut reader, MessageLimits::default());
});
