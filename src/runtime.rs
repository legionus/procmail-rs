// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeMap;
use std::path::Path;

use crate::delivery::{CommitError, CommitReport, PublishedDelivery};
use crate::trace::{NoTrace, TraceEvent, TraceSink};

#[derive(Debug, Default)]
pub struct RuntimeVariables {
    values: BTreeMap<String, String>,
}

impl RuntimeVariables {
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.values.insert(name.into(), value.into());
    }

    pub fn last_folder(&self) -> Option<&str> {
        self.get("LASTFOLDER")
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn record_commit(&mut self, report: &CommitReport) -> Result<(), String> {
        self.record_commit_with_trace(report, &mut NoTrace)
    }

    pub fn record_commit_with_trace(
        &mut self,
        report: &CommitReport,
        trace: &mut impl TraceSink,
    ) -> Result<(), String> {
        self.record_last_folder(report.last_folder(), trace)
    }

    pub fn record_partial_commit(&mut self, error: &CommitError) -> Result<(), String> {
        self.record_partial_commit_with_trace(error, &mut NoTrace)
    }

    pub fn record_partial_commit_with_trace(
        &mut self,
        error: &CommitError,
        trace: &mut impl TraceSink,
    ) -> Result<(), String> {
        self.record_last_folder(error.last_folder(), trace)
    }

    pub fn record_delivery(&mut self, delivery: &PublishedDelivery) -> Result<(), String> {
        self.record_delivery_with_trace(delivery, &mut NoTrace)
    }

    pub fn record_delivery_with_trace(
        &mut self,
        delivery: &PublishedDelivery,
        trace: &mut impl TraceSink,
    ) -> Result<(), String> {
        self.record_last_folder(Some(delivery.last_folder()), trace)
    }

    fn record_last_folder(
        &mut self,
        path: Option<&Path>,
        trace: &mut impl TraceSink,
    ) -> Result<(), String> {
        let Some(path) = path else {
            return Ok(());
        };
        let value = path.to_str().ok_or_else(|| {
            format!(
                "published destination cannot be represented as UTF-8: {}",
                path.display()
            )
        })?;
        self.values
            .insert("LASTFOLDER".to_owned(), value.to_owned());
        trace.record(TraceEvent::LastFolderUpdated);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::{PendingFanout, PendingSink, PublishedDelivery};
    use std::io::{self, Write};
    use std::path::PathBuf;

    struct NamedSink {
        name: &'static str,
        fail: bool,
        fail_after_publish: bool,
    }

    impl Write for NamedSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl PendingSink for NamedSink {
        fn commit(self: Box<Self>) -> Result<PublishedDelivery, crate::delivery::SinkCommitError> {
            if self.fail {
                Err(crate::delivery::SinkCommitError::before_publication(
                    io::Error::other("injected commit failure"),
                ))
            } else {
                let published = PublishedDelivery::new(PathBuf::from(self.name));
                if self.fail_after_publish {
                    Err(crate::delivery::SinkCommitError::after_publication(
                        io::Error::other("injected durability failure"),
                        published,
                    ))
                } else {
                    Ok(published)
                }
            }
        }

        fn abort(self: Box<Self>) -> io::Result<()> {
            Ok(())
        }
    }

    fn validated(sinks: Vec<Box<dyn PendingSink>>) -> crate::delivery::ValidatedFanout {
        let pending = PendingFanout::new(sinks).unwrap();
        let head = crate::message::Message::read_headers(
            &mut io::Cursor::new(b"\n"),
            crate::limits::MessageLimits::default(),
        )
        .unwrap();
        pending
            .stream(head, &mut io::Cursor::new(&b""[..]))
            .unwrap()
            .0
    }

    #[test]
    fn records_last_successful_destination() {
        let report = validated(vec![
            Box::new(NamedSink {
                name: "first",
                fail: false,
                fail_after_publish: false,
            }),
            Box::new(NamedSink {
                name: "second",
                fail: false,
                fail_after_publish: false,
            }),
        ])
        .commit()
        .unwrap();
        let mut runtime = RuntimeVariables::default();

        runtime.record_commit(&report).unwrap();

        assert_eq!(runtime.last_folder(), Some("second"));
    }

    #[test]
    fn records_last_destination_before_partial_failure() {
        let error = validated(vec![
            Box::new(NamedSink {
                name: "first",
                fail: false,
                fail_after_publish: false,
            }),
            Box::new(NamedSink {
                name: "failed",
                fail: true,
                fail_after_publish: false,
            }),
        ])
        .commit()
        .unwrap_err();
        let mut runtime = RuntimeVariables::default();

        runtime.record_partial_commit(&error).unwrap();

        assert_eq!(runtime.last_folder(), Some("first"));
    }

    #[test]
    fn failure_before_publication_does_not_change_last_folder() {
        let error = validated(vec![Box::new(NamedSink {
            name: "never-visible",
            fail: true,
            fail_after_publish: false,
        })])
        .commit()
        .unwrap_err();
        let mut runtime = RuntimeVariables::default();
        runtime.set("LASTFOLDER", "previous");
        let mut trace = crate::trace::MemoryTrace::default();

        runtime
            .record_partial_commit_with_trace(&error, &mut trace)
            .unwrap();

        assert_eq!(runtime.last_folder(), Some("previous"));
        assert!(trace.events().is_empty());
    }

    #[test]
    fn failure_after_publication_records_the_visible_folder() {
        let error = validated(vec![Box::new(NamedSink {
            name: "visible-before-sync-failure",
            fail: false,
            fail_after_publish: true,
        })])
        .commit()
        .unwrap_err();
        let mut runtime = RuntimeVariables::default();

        runtime.record_partial_commit(&error).unwrap();

        assert_eq!(runtime.last_folder(), Some("visible-before-sync-failure"));
    }
}
