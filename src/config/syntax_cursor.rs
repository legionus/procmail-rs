// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#[derive(Debug, Clone, Copy)]
pub(super) struct SyntaxCursor<'a> {
    source: &'a str,
    offset: usize,
}

impl<'a> SyntaxCursor<'a> {
    pub(super) fn new(source: &'a str) -> Self {
        Self { source, offset: 0 }
    }

    pub(super) fn word(&mut self) -> Option<&'a str> {
        self.skip_whitespace();
        let start = self.offset;
        while self
            .source
            .as_bytes()
            .get(self.offset)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
        {
            self.offset += 1;
        }
        (start != self.offset).then(|| &self.source[start..self.offset])
    }

    pub(super) fn keyword(&mut self, expected: &str) -> bool {
        let saved = self.offset;
        if self.word() == Some(expected) {
            true
        } else {
            self.offset = saved;
            false
        }
    }

    pub(super) fn remainder(&mut self) -> &'a str {
        self.skip_whitespace();
        let start = self.offset;
        self.offset = self.source.len();
        &self.source[start..]
    }

    pub(super) fn is_end(&mut self) -> bool {
        self.skip_whitespace();
        self.offset == self.source.len()
    }

    #[cfg(test)]
    pub(super) fn offset(&self) -> usize {
        self.offset
    }

    fn skip_whitespace(&mut self) {
        while self
            .source
            .as_bytes()
            .get(self.offset)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.offset += 1;
        }
    }
}

#[cfg(test)]
#[path = "../tests/config/syntax_cursor.rs"]
mod tests;
