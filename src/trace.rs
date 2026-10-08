// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Typed, bounded records used to explain filtering decisions.

use crate::config::{HeaderAction, HeaderOperation};
use crate::source_location::SourceLocation;

mod config;
mod render;
mod schema;
mod writer;

pub use config::*;
pub use render::EscapedBytes;
pub use schema::*;
pub use writer::*;

pub trait TraceSink {
    fn detail(&self) -> TraceDetail {
        TraceDetail::Metadata
    }

    fn record(&mut self, event: impl Into<TraceRecord>);

    fn record_at(&mut self, location: &SourceLocation, event: TraceEvent) {
        let detail = self.detail();
        self.record(event.at(location, detail));
    }

    fn set_verbose(&mut self, _enabled: bool) {}

    fn set_log_abstract(&mut self, _mode: LogAbstractMode) {}

    fn finish(&mut self) {}
}

pub fn record_external_command(
    location: &SourceLocation,
    command: &str,
    trace: &mut impl TraceSink,
) {
    let command = trace
        .detail()
        .includes_variable_values()
        .then(|| TraceValue::new(command.as_bytes()));
    trace.record_at(
        location,
        TraceEvent::ExternalCommandExecuting {
            line: location.line(),
            command,
        },
    );
}

pub fn record_session_start(trace: &mut impl TraceSink) {
    let timestamp = format_session_timestamp(std::time::SystemTime::now());
    trace.record(TraceEvent::SessionStarted {
        pid: std::process::id(),
        timestamp,
    });
}

pub fn record_header_action(
    action: &HeaderAction,
    source: &SourceLocation,
    trace: &mut impl TraceSink,
) {
    for operation in &action.operations {
        let (line, kind, name, argument, extraction_mode) = match operation {
            HeaderOperation::Remove { line, name } => {
                (*line, HeaderOperationKind::Remove, name, None, None)
            }
            HeaderOperation::Set { line, name, .. } => {
                (*line, HeaderOperationKind::Set, name, None, None)
            }
            HeaderOperation::Add { line, name, .. } => {
                (*line, HeaderOperationKind::Add, name, None, None)
            }
            HeaderOperation::Prepend { line, name, .. } => {
                (*line, HeaderOperationKind::Prepend, name, None, None)
            }
            HeaderOperation::Rename { line, from, to } => (
                *line,
                HeaderOperationKind::Rename,
                from,
                Some(to.as_str()),
                None,
            ),
            HeaderOperation::Extract {
                line,
                name,
                target,
                mode,
            } => (
                *line,
                HeaderOperationKind::Extract,
                name,
                Some(target.as_str()),
                Some(*mode),
            ),
        };

        // The parser has already bounded and validated header and variable
        // names. Retain only those names here; header values must never enter
        // a trace event, including in high-detail mode.
        trace.record_at(
            &source.at_line(line),
            TraceEvent::HeaderOperation {
                line,
                kind,
                name: TraceName(name.clone()),
                argument: argument.map(|name| TraceName(name.to_owned())),
                extraction_mode,
            },
        );
    }
}
#[derive(Debug, Default)]
pub struct NoTrace;

impl TraceSink for NoTrace {
    fn record(&mut self, _: impl Into<TraceRecord>) {}
}
#[derive(Debug, Default)]
pub struct MemoryTrace {
    records: Vec<TraceRecord>,
    truncated: bool,
}

impl MemoryTrace {
    pub fn records(&self) -> &[TraceRecord] {
        &self.records
    }

    pub fn was_truncated(&self) -> bool {
        self.truncated
    }
}

impl TraceSink for MemoryTrace {
    fn record(&mut self, event: impl Into<TraceRecord>) {
        // Test traces still consume configuration-controlled events. Stop at
        // a fixed count instead of allowing a forgotten test sink to grow
        // without a limit during adversarial or fuzz-style execution.
        if self.records.len() < MAX_MEMORY_TRACE_EVENTS {
            let mut record = event.into();
            record.location = record
                .location
                .for_trace(self.detail().includes_variable_values());
            self.records.push(record);
        } else {
            self.truncated = true;
        }
    }
}
fn format_session_timestamp(time: std::time::SystemTime) -> String {
    let seconds = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_date_from_days(days as i64);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days % 7) as usize];
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][month as usize - 1];
    format!(
        "{weekday} {month_name} {day:2} {:02}:{:02}:{:02} {year}",
        day_seconds / 3600,
        (day_seconds / 60) % 60,
        day_seconds % 60
    )
}

// Convert days since 1970-01-01 to a Gregorian date without libc or locale
// state. Trace output must remain available in the executable's restricted
// environment, and a small deterministic formatter is sufficient here.
fn civil_date_from_days(days: i64) -> (i32, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}
#[cfg(test)]
#[path = "tests/trace.rs"]
mod tests;
