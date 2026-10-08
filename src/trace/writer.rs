// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Output budgets, stop state, and VERBOSE/LOGABSTRACT event selection.

use super::render::render_record;
use super::*;
use std::io::{self, Write};

#[derive(Debug)]
pub struct BoundedTraceWriter<W> {
    writer: W,
    events: usize,
    bytes: usize,
    stopped: Option<TraceStopReason>,
    detail: TraceDetail,
    format: TraceFormat,
    verbose: bool,
    abstract_mode: LogAbstractMode,
    last_published: Option<TraceRecord>,
}

impl<W> BoundedTraceWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            events: 0,
            bytes: 0,
            stopped: None,
            detail: TraceDetail::Metadata,
            format: TraceFormat::Json,
            verbose: true,
            abstract_mode: LogAbstractMode::No,
            last_published: None,
        }
    }

    pub fn with_detail(writer: W, detail: TraceDetail) -> Self {
        Self {
            writer,
            events: 0,
            bytes: 0,
            stopped: None,
            detail,
            format: TraceFormat::Json,
            verbose: true,
            abstract_mode: LogAbstractMode::No,
            last_published: None,
        }
    }

    pub fn formatted(writer: W, detail: TraceDetail, format: TraceFormat) -> Self {
        Self {
            writer,
            events: 0,
            bytes: 0,
            stopped: None,
            detail,
            format,
            verbose: true,
            abstract_mode: LogAbstractMode::No,
            last_published: None,
        }
    }

    pub fn runtime_formatted(
        writer: W,
        detail: TraceDetail,
        format: TraceFormat,
        verbose: bool,
        abstract_mode: LogAbstractMode,
    ) -> Self {
        Self {
            writer,
            events: 0,
            bytes: 0,
            stopped: None,
            detail,
            format,
            verbose,
            abstract_mode,
            last_published: None,
        }
    }

    pub fn event_count(&self) -> usize {
        self.events
    }

    pub fn byte_count(&self) -> usize {
        self.bytes
    }

    pub fn stop_reason(&self) -> Option<TraceStopReason> {
        self.stopped
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: Write> TraceSink for BoundedTraceWriter<W> {
    fn detail(&self) -> TraceDetail {
        self.detail
    }

    fn record(&mut self, record: impl Into<TraceRecord>) {
        let mut record = record.into();
        record.location = record
            .location
            .for_trace(self.detail.includes_variable_values());
        let event = &record.event;
        if let TraceEvent::Delivery {
            recipe_line,
            destination,
            stage: DeliveryStage::Published,
            path,
        } = event
        {
            match self.abstract_mode {
                LogAbstractMode::No => {}
                LogAbstractMode::Yes => {
                    self.last_published = Some(TraceRecord {
                        event: TraceEvent::DeliveryAbstract {
                            recipe_line: *recipe_line,
                            destination: *destination,
                            path: path.clone(),
                        },
                        location: record.location.clone(),
                    });
                }
                LogAbstractMode::All => {
                    let abstract_event = TraceEvent::DeliveryAbstract {
                        recipe_line: *recipe_line,
                        destination: *destination,
                        path: path.clone(),
                    };
                    self.write_event(&TraceRecord {
                        event: abstract_event,
                        location: record.location.clone(),
                    });
                }
            }
        }
        if !self.verbose && !matches!(event, TraceEvent::Log { .. }) {
            return;
        }
        self.write_event(&record);
    }

    fn set_verbose(&mut self, enabled: bool) {
        self.verbose = enabled;
    }

    fn set_log_abstract(&mut self, mode: LogAbstractMode) {
        self.abstract_mode = mode;
        self.last_published = None;
    }

    fn finish(&mut self) {
        if self.abstract_mode != LogAbstractMode::Yes {
            return;
        }
        if let Some(record) = self.last_published.take() {
            self.write_event(&record);
        }
    }
}

impl<W: Write> BoundedTraceWriter<W> {
    fn write_event(&mut self, record: &TraceRecord) {
        if self.stopped.is_some() {
            return;
        }

        if !self.format.includes(&record.event) {
            return;
        }

        if self.events >= MAX_TRACE_EVENTS {
            self.stopped = Some(TraceStopReason::EventLimit);
            return;
        }

        // Rendering completes under the per-record bound before the writer
        // sees any bytes. All formats therefore share the same accounting
        // and stop before an oversized record can reach the output.
        let rendered = match render_record(record, self.format) {
            Ok(rendered) => rendered,
            Err(_) => {
                self.stopped = Some(TraceStopReason::EventSizeLimit);
                return;
            }
        };
        let Some(total) = self.bytes.checked_add(rendered.len()) else {
            self.stopped = Some(TraceStopReason::ByteLimit);
            return;
        };
        if total > MAX_TRACE_BYTES {
            self.stopped = Some(TraceStopReason::ByteLimit);
            return;
        }
        if let Err(error) = self.writer.write_all(rendered.as_bytes()) {
            self.stopped = Some(TraceStopReason::Io(error.kind()));
            return;
        }
        self.events += 1;
        self.bytes = total;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceStopReason {
    EventSizeLimit,
    EventLimit,
    ByteLimit,
    Io(io::ErrorKind),
}
