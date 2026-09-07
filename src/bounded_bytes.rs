// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundedBytesError {
    LengthOverflow,
    LimitExceeded { attempted: usize },
}

pub(crate) struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedBytes {
    pub(crate) fn with_capacity(limit: usize, capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity.min(limit)),
            limit,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn remaining(&self) -> Result<usize, BoundedBytesError> {
        self.limit
            .checked_sub(self.bytes.len())
            .ok_or(BoundedBytesError::LengthOverflow)
    }

    pub(crate) fn try_extend(&mut self, value: &[u8]) -> Result<(), BoundedBytesError> {
        Self::try_extend_vec(&mut self.bytes, self.limit, value)
    }

    pub(crate) fn try_extend_vec(
        output: &mut Vec<u8>,
        limit: usize,
        value: &[u8],
    ) -> Result<(), BoundedBytesError> {
        // Compute the complete new length before modifying the buffer. This
        // keeps callers from observing a prefix when arithmetic overflows or
        // the selected subsystem limit would be exceeded.
        let new_len = output
            .len()
            .checked_add(value.len())
            .ok_or(BoundedBytesError::LengthOverflow)?;
        if new_len > limit {
            return Err(BoundedBytesError::LimitExceeded { attempted: new_len });
        }
        output.extend_from_slice(value);
        Ok(())
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
#[path = "tests/bounded_bytes.rs"]
mod tests;
