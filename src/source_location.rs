// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::path::Path;
use std::sync::Arc;

/// An rc position whose filename is shared by all positions in that file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceLocation {
    line: usize,
    file: Option<Arc<str>>,
    truncated: bool,
}

#[cfg(test)]
#[path = "tests/source_location.rs"]
mod tests;

impl fmt::Display for SourceLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = self.file() {
            write!(
                formatter,
                "{}:",
                crate::trace::EscapedBytes::new(file.as_bytes())
            )?;
        }

        write!(formatter, "line {}", self.line)
    }
}

impl SourceLocation {
    pub fn unknown(line: usize) -> Self {
        Self {
            line,
            file: None,
            truncated: false,
        }
    }

    pub fn for_file(path: &Path, line: usize) -> Result<Self, String> {
        let file = path.to_str().ok_or("rc path is not valid UTF-8")?;

        if file.is_empty()
            || file.len() > crate::config::MAX_PATH_EXPRESSION_LEN
            || file.contains('\0')
        {
            return Err("rc path is empty, contains NUL, or exceeds the path limit".to_owned());
        }

        Ok(Self {
            line,
            file: Some(Arc::from(file)),
            truncated: false,
        })
    }

    pub fn at_line(&self, line: usize) -> Self {
        Self {
            line,
            file: self.file.clone(),
            truncated: self.truncated,
        }
    }

    pub fn line(&self) -> usize {
        self.line
    }

    pub fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    pub(crate) fn for_trace(&self, values: bool) -> Self {
        let Some(file) = self.file().filter(|_| values) else {
            return Self::unknown(self.line);
        };

        if file.len() <= crate::trace::MAX_TRACE_VALUE_SIZE {
            return self.clone();
        }

        let mut length = file.len().min(crate::trace::MAX_TRACE_VALUE_SIZE);

        // A log retains only a bounded filename prefix. Keep the cut on a
        // character boundary; escaping still happens at the byte renderer.
        while !file.is_char_boundary(length) {
            length -= 1;
        }

        Self {
            line: self.line,
            file: Some(Arc::from(&file[..length])),
            truncated: self.truncated || length < file.len(),
        }
    }

    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }
}
