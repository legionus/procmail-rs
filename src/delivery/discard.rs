// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io::{self, Write};
use std::path::PathBuf;

use super::{PendingSink, PublishedDelivery, SinkCommitError};

pub struct DiscardSink {
    path: PathBuf,
}

impl DiscardSink {
    pub fn null() -> Self {
        Self {
            path: PathBuf::from("/dev/null"),
        }
    }
}

impl Write for DiscardSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PendingSink for DiscardSink {
    fn commit(self: Box<Self>) -> Result<PublishedDelivery, SinkCommitError> {
        Ok(PublishedDelivery::new(self.path))
    }

    fn abort(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }
}
