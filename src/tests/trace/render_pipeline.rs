// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn suppressed_text_events_do_not_consume_a_record_budget() {
    let mut writer =
        BoundedTraceWriter::formatted(Vec::new(), TraceDetail::Metadata, TraceFormat::Text);

    for _ in 0..=MAX_TRACE_EVENTS {
        writer.record(TraceEvent::LastFolderUpdated);
    }

    assert_eq!(writer.event_count(), 0);
    assert_eq!(writer.byte_count(), 0);
    assert_eq!(writer.stop_reason(), None);
    writer.record(variable_event("VISIBLE"));
    assert_eq!(writer.event_count(), 1);
    assert!(!writer.into_inner().is_empty());
}

#[test]
fn event_budget_stops_before_attempting_to_render_another_record() {
    let mut writer = BoundedTraceWriter::new(io::sink());

    for _ in 0..MAX_TRACE_EVENTS {
        writer.record(TraceEvent::LastFolderUpdated);
    }

    writer.record(TraceEvent::SessionStarted {
        pid: 1,
        timestamp: "x".repeat(MAX_TRACE_EVENT_SIZE + 1),
    });

    assert_eq!(writer.stop_reason(), Some(TraceStopReason::EventLimit));
    assert_eq!(writer.event_count(), MAX_TRACE_EVENTS);
}
