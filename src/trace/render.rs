// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Complete bounded records, including source annotations and delimiters.

use super::*;
use crate::config::HeaderExtractionMode;
use std::fmt;
use std::fmt::Write as _;

mod json;
mod text;
use json::{render_json_event, render_json_string};
use text::render_human_event;

pub struct EscapedBytes<'a>(&'a [u8]);

impl<'a> EscapedBytes<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for EscapedBytes<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Render one byte at a time so malformed UTF-8 can never enter the
        // output unchecked. Keeping only a conservative printable ASCII set
        // literal also makes every record boundary visible to line-oriented
        // log consumers.
        for byte in self.0 {
            match byte {
                b'\n' => formatter.write_str("\\n")?,
                b'\r' => formatter.write_str("\\r")?,
                b'\t' => formatter.write_str("\\t")?,
                b'\\' => formatter.write_str("\\\\")?,
                b'\'' => formatter.write_str("\\'")?,
                b'"' => formatter.write_str("\\\"")?,
                b' '..=b'~' => formatter.write_str(char::from(*byte).encode_utf8(&mut [0; 4]))?,
                _ => write!(formatter, "\\x{byte:02x}")?,
            }
        }
        Ok(())
    }
}

pub(super) struct BoundedText {
    bytes: String,
    limit: usize,
}

impl BoundedText {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            bytes: String::with_capacity(limit),
            limit,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }
}

impl fmt::Write for BoundedText {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let new_len = self
            .bytes
            .len()
            .checked_add(value.len())
            .ok_or(fmt::Error)?;
        if new_len > self.limit {
            return Err(fmt::Error);
        }
        self.bytes.push_str(value);
        Ok(())
    }
}

fn render_source_location(
    output: &mut BoundedText,
    format: TraceFormat,
    location: &SourceLocation,
    event: &TraceEvent,
) -> fmt::Result {
    let Some(file) = location.file() else {
        return Ok(());
    };

    match format {
        TraceFormat::Json => {
            // Every renderer finishes a JSON object. Extend that same bounded
            // buffer so source and event cannot become separate log records.
            if output.bytes.pop() != Some('}') {
                return Err(fmt::Error);
            }

            output.write_str(",\"rc_file\":")?;
            render_json_string(output, file.as_bytes())?;

            if location.is_truncated() {
                output.write_str(",\"rc_file_truncated\":true")?;
            }

            output.write_char('}')
        }
        TraceFormat::Text if !matches!(event, TraceEvent::Log { .. }) => {
            write!(output, " [rc \"{}\"", EscapedBytes::new(file.as_bytes()))?;

            if location.is_truncated() {
                output.write_str(" [truncated]")?;
            }

            write!(output, ":{}]", location.line())
        }
        TraceFormat::Text => Ok(()),
    }
}

fn header_extraction_mode_name(mode: HeaderExtractionMode) -> &'static str {
    match mode {
        HeaderExtractionMode::Raw => "raw",
        HeaderExtractionMode::Unfolded => "unfolded",
        HeaderExtractionMode::Decoded => "decoded",
    }
}

fn failure_class_name(class: FailureClass) -> &'static str {
    match class {
        FailureClass::InputLimit => "input-limit",
        FailureClass::Transient => "transient",
        FailureClass::Permanent => "permanent",
        FailureClass::Internal => "internal",
    }
}

fn delivery_failure_class_name(class: crate::delivery::DeliveryFailureClass) -> &'static str {
    match class {
        crate::delivery::DeliveryFailureClass::Retryable => "transient",
        crate::delivery::DeliveryFailureClass::Permanent => "permanent",
        crate::delivery::DeliveryFailureClass::Internal => "internal",
    }
}

impl TraceFormat {
    pub(super) fn includes(self, event: &TraceEvent) -> bool {
        self != Self::Text
            || !matches!(
                event,
                TraceEvent::RecipeEvaluated { .. } | TraceEvent::LastFolderUpdated
            )
    }
}

pub(super) fn render_record(
    record: &TraceRecord,
    format: TraceFormat,
) -> Result<BoundedText, fmt::Error> {
    let event = &record.event;

    let mut output = BoundedText::new(MAX_TRACE_EVENT_SIZE);

    match format {
        TraceFormat::Json => render_json_event(&mut output, event)?,
        TraceFormat::Text => render_human_event(&mut output, event)?,
    }

    render_source_location(&mut output, format, &record.location, event)?;

    if format == TraceFormat::Json || !matches!(event, TraceEvent::Log { .. }) {
        output.write_char('\n')?;
    }

    Ok(output)
}
