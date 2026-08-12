// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use crate::message::{Message, MessageHead, MessageReadError, StreamedMessage};

#[cfg(target_os = "linux")]
pub mod maildir;
#[cfg(target_os = "linux")]
pub mod staging;

pub const MAX_PENDING_SINKS: usize = 256;

/// A destination which keeps written bytes private until `commit` succeeds.
///
/// Implementations must arrange for dropping the sink, or calling `abort`, to
/// leave no visible delivery behind. `commit` may make the delivery visible;
/// if it returns an error, it must not have published the delivery and must
/// clean up its private state.
pub trait PendingSink: Write {
    /// Publishes the pending bytes and reports the exact visible destination.
    fn commit(self: Box<Self>) -> io::Result<PublishedDelivery>;
    fn abort(self: Box<Self>) -> io::Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDelivery {
    last_folder: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReport {
    published: Vec<PublishedDelivery>,
}

pub struct PendingFanout {
    sinks: Vec<Box<dyn PendingSink>>,
}

pub struct ValidatedFanout {
    sinks: Vec<Box<dyn PendingSink>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanoutLimitError {
    TooManySinks { count: usize, limit: usize },
}

#[derive(Debug)]
pub struct StreamDeliveryError {
    source: MessageReadError,
    abort_failures: usize,
}

#[derive(Debug)]
pub struct CommitError {
    source: io::Error,
    published: Vec<PublishedDelivery>,
    abort_failures: usize,
}

impl PublishedDelivery {
    pub fn new(last_folder: PathBuf) -> Self {
        Self { last_folder }
    }

    pub fn last_folder(&self) -> &std::path::Path {
        &self.last_folder
    }
}

impl CommitReport {
    pub fn published(&self) -> &[PublishedDelivery] {
        &self.published
    }

    pub fn last_folder(&self) -> Option<&std::path::Path> {
        self.published.last().map(PublishedDelivery::last_folder)
    }
}

#[derive(Debug)]
pub struct AppendError {
    source: io::Error,
    abort_failures: usize,
}

impl PendingFanout {
    pub fn new(sinks: Vec<Box<dyn PendingSink>>) -> Result<Self, FanoutLimitError> {
        if sinks.len() > MAX_PENDING_SINKS {
            return Err(FanoutLimitError::TooManySinks {
                count: sinks.len(),
                limit: MAX_PENDING_SINKS,
            });
        }
        Ok(Self { sinks })
    }

    pub fn len(&self) -> usize {
        self.sinks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }

    pub fn stream(
        mut self,
        head: MessageHead,
        reader: &mut impl BufRead,
    ) -> Result<(ValidatedFanout, StreamedMessage), StreamDeliveryError> {
        match head.stream_to(reader, &mut self) {
            Ok(message) => Ok((
                ValidatedFanout {
                    sinks: std::mem::take(&mut self.sinks),
                },
                message,
            )),
            Err(source) => {
                let abort_failures = self.abort_all();
                Err(StreamDeliveryError {
                    source,
                    abort_failures,
                })
            }
        }
    }

    pub fn buffer(
        mut self,
        head: MessageHead,
        reader: &mut impl BufRead,
    ) -> Result<(ValidatedFanout, Message), StreamDeliveryError> {
        match head.read_body_to(reader, &mut self) {
            Ok(message) => Ok((
                ValidatedFanout {
                    sinks: std::mem::take(&mut self.sinks),
                },
                message,
            )),
            Err(source) => {
                let abort_failures = self.abort_all();
                Err(StreamDeliveryError {
                    source,
                    abort_failures,
                })
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub fn stage(
        mut self,
        head: MessageHead,
        reader: &mut impl BufRead,
        staging: &mut staging::StagingFile,
    ) -> Result<(ValidatedFanout, StreamedMessage), StreamDeliveryError> {
        let mut writer = TeeWriter {
            fanout: &mut self,
            staging,
        };
        match head.stream_to(reader, &mut writer) {
            Ok(message) => Ok((
                ValidatedFanout {
                    sinks: std::mem::take(&mut self.sinks),
                },
                message,
            )),
            Err(source) => {
                let abort_failures = self.abort_all();
                Err(StreamDeliveryError {
                    source,
                    abort_failures,
                })
            }
        }
    }

    fn abort_all(&mut self) -> usize {
        let mut failures = 0usize;
        while let Some(sink) = self.sinks.pop() {
            if sink.abort().is_err() {
                failures = failures.saturating_add(1);
            }
        }
        failures
    }
}

#[cfg(target_os = "linux")]
struct TeeWriter<'a> {
    fanout: &'a mut PendingFanout,
    staging: &'a mut staging::StagingFile,
}

#[cfg(target_os = "linux")]
impl Write for TeeWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.fanout.write_all(bytes)?;
        self.staging.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.fanout.flush()?;
        self.staging.flush()
    }
}

impl Write for PendingFanout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for sink in &mut self.sinks {
            sink.write_all(bytes)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        for sink in &mut self.sinks {
            sink.flush()?;
        }
        Ok(())
    }
}

impl Drop for PendingFanout {
    fn drop(&mut self) {
        self.abort_all();
    }
}

impl ValidatedFanout {
    pub fn append_buffered(
        self,
        pending: PendingFanout,
        message: &Message,
    ) -> Result<Self, AppendError> {
        self.append_bytes(pending, message.as_bytes())
    }

    pub fn append_bytes(
        mut self,
        mut pending: PendingFanout,
        message: &[u8],
    ) -> Result<Self, AppendError> {
        let count = self.sinks.len().saturating_add(pending.sinks.len());
        if count > MAX_PENDING_SINKS {
            let source = io::Error::new(
                io::ErrorKind::InvalidInput,
                FanoutLimitError::TooManySinks {
                    count,
                    limit: MAX_PENDING_SINKS,
                },
            );
            let abort_failures = pending.abort_all().saturating_add(self.abort_all());
            return Err(AppendError {
                source,
                abort_failures,
            });
        }

        if let Err(source) = pending.write_all(message) {
            let abort_failures = pending.abort_all().saturating_add(self.abort_all());
            return Err(AppendError {
                source,
                abort_failures,
            });
        }
        self.sinks.append(&mut pending.sinks);
        Ok(self)
    }

    pub fn commit(mut self) -> Result<CommitReport, CommitError> {
        let mut published = Vec::with_capacity(self.sinks.len());
        let sinks = std::mem::take(&mut self.sinks);
        let mut remaining = sinks.into_iter();
        while let Some(sink) = remaining.next() {
            match sink.commit() {
                Ok(delivery) => published.push(delivery),
                Err(source) => {
                    // A fan-out cannot roll back sinks already made visible.
                    // Preserve their exact names for LASTFOLDER, while aborting
                    // every sink that has not yet reached publication.
                    let abort_failures = remaining
                        .map(|sink| usize::from(sink.abort().is_err()))
                        .fold(0usize, usize::saturating_add);
                    return Err(CommitError {
                        source,
                        published,
                        abort_failures,
                    });
                }
            }
        }
        Ok(CommitReport { published })
    }

    fn abort_all(&mut self) -> usize {
        let mut failures = 0usize;
        while let Some(sink) = self.sinks.pop() {
            if sink.abort().is_err() {
                failures = failures.saturating_add(1);
            }
        }
        failures
    }
}

impl Drop for ValidatedFanout {
    fn drop(&mut self) {
        self.abort_all();
    }
}

impl FanoutLimitError {
    pub fn count(self) -> usize {
        match self {
            Self::TooManySinks { count, .. } => count,
        }
    }

    pub fn limit(self) -> usize {
        match self {
            Self::TooManySinks { limit, .. } => limit,
        }
    }
}

impl fmt::Display for FanoutLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManySinks { count, limit } => {
                write!(
                    formatter,
                    "delivery fan-out has {count} sinks, limit is {limit}"
                )
            }
        }
    }
}

impl std::error::Error for FanoutLimitError {}

impl StreamDeliveryError {
    pub fn abort_failures(&self) -> usize {
        self.abort_failures
    }
}

impl fmt::Display for StreamDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "cannot validate message: {}", self.source)?;
        if self.abort_failures != 0 {
            write!(
                formatter,
                "; failed to abort {} pending sink(s)",
                self.abort_failures
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for StreamDeliveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl CommitError {
    pub fn committed(&self) -> usize {
        self.published.len()
    }

    pub fn published(&self) -> &[PublishedDelivery] {
        &self.published
    }

    pub fn last_folder(&self) -> Option<&std::path::Path> {
        self.published.last().map(PublishedDelivery::last_folder)
    }

    pub fn abort_failures(&self) -> usize {
        self.abort_failures
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot commit delivery after publishing {} sink(s): {}",
            self.published.len(),
            self.source
        )?;
        if self.abort_failures != 0 {
            write!(
                formatter,
                "; failed to abort {} remaining sink(s)",
                self.abort_failures
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for CommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl AppendError {
    pub fn abort_failures(&self) -> usize {
        self.abort_failures
    }
}

impl fmt::Display for AppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot append buffered delivery: {}",
            self.source
        )?;
        if self.abort_failures != 0 {
            write!(
                formatter,
                "; failed to abort {} pending sink(s)",
                self.abort_failures
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for AppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{self, Cursor, Write};
    use std::path::Path;
    use std::rc::Rc;

    use super::*;
    use crate::limits::MessageLimits;
    use crate::message::{Message, MessageLimit};

    #[derive(Default)]
    struct SinkState {
        visible: Option<Vec<u8>>,
        aborted: bool,
    }

    struct TestSink {
        state: Rc<RefCell<SinkState>>,
        pending: Vec<u8>,
        fail_write_at: Option<usize>,
        fail_commit: bool,
        fail_abort: bool,
        last_folder: PathBuf,
    }

    impl TestSink {
        fn boxed(state: Rc<RefCell<SinkState>>) -> Box<dyn PendingSink> {
            Box::new(Self {
                state,
                pending: Vec::new(),
                fail_write_at: None,
                fail_commit: false,
                fail_abort: false,
                last_folder: PathBuf::from("test-folder"),
            })
        }

        fn failing_write(state: Rc<RefCell<SinkState>>, at: usize) -> Box<dyn PendingSink> {
            Box::new(Self {
                state,
                pending: Vec::new(),
                fail_write_at: Some(at),
                fail_commit: false,
                fail_abort: false,
                last_folder: PathBuf::from("test-folder"),
            })
        }

        fn failing_commit(state: Rc<RefCell<SinkState>>) -> Box<dyn PendingSink> {
            Box::new(Self {
                state,
                pending: Vec::new(),
                fail_write_at: None,
                fail_commit: true,
                fail_abort: false,
                last_folder: PathBuf::from("test-folder"),
            })
        }
    }

    impl Write for TestSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self
                .fail_write_at
                .is_some_and(|limit| self.pending.len().saturating_add(bytes.len()) > limit)
            {
                return Err(io::Error::other("injected write failure"));
            }
            self.pending.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl PendingSink for TestSink {
        fn commit(self: Box<Self>) -> io::Result<PublishedDelivery> {
            if self.fail_commit {
                return Err(io::Error::other("injected commit failure"));
            }
            self.state.borrow_mut().visible = Some(self.pending.clone());
            Ok(PublishedDelivery::new(self.last_folder.clone()))
        }

        fn abort(self: Box<Self>) -> io::Result<()> {
            self.state.borrow_mut().aborted = true;
            if self.fail_abort {
                Err(io::Error::other("injected abort failure"))
            } else {
                Ok(())
            }
        }
    }

    fn read_head(input: &[u8], limits: MessageLimits) -> (MessageHead, Cursor<&[u8]>) {
        let mut reader = Cursor::new(input);
        let head = Message::read_headers(&mut reader, limits).unwrap();
        (head, reader)
    }

    #[test]
    fn publishes_only_after_validation_and_commit() {
        let input = b"Subject: test\n\nbody";
        let first = Rc::new(RefCell::new(SinkState::default()));
        let second = Rc::new(RefCell::new(SinkState::default()));
        let (head, mut reader) = read_head(input, MessageLimits::default());
        let pending = PendingFanout::new(vec![
            TestSink::boxed(first.clone()),
            TestSink::boxed(second.clone()),
        ])
        .unwrap();

        let (validated, message) = pending.stream(head, &mut reader).unwrap();
        assert_eq!(message.len(), input.len());
        assert!(first.borrow().visible.is_none());
        assert!(second.borrow().visible.is_none());

        let report = validated.commit().unwrap();
        assert_eq!(report.last_folder(), Some(Path::new("test-folder")));
        assert_eq!(first.borrow().visible.as_deref(), Some(input.as_slice()));
        assert_eq!(second.borrow().visible.as_deref(), Some(input.as_slice()));
    }

    #[test]
    fn limit_failure_aborts_every_sink_without_publishing() {
        let input = b"Subject: test\n\ntoo long";
        let state = Rc::new(RefCell::new(SinkState::default()));
        let limits = MessageLimits {
            body_size: 3,
            ..MessageLimits::default()
        };
        let (head, mut reader) = read_head(input, limits);
        let pending = PendingFanout::new(vec![TestSink::boxed(state.clone())]).unwrap();

        let error = match pending.stream(head, &mut reader) {
            Ok(_) => panic!("message exceeding the body limit was accepted"),
            Err(error) => error,
        };
        assert!(matches!(
            error.source,
            MessageReadError::LimitExceeded {
                kind: MessageLimit::Body,
                limit: 3
            }
        ));
        assert!(state.borrow().visible.is_none());
        assert!(state.borrow().aborted);
    }

    #[test]
    fn sink_write_failure_aborts_all_sinks() {
        let input = b"Subject: test\n\nbody";
        let first = Rc::new(RefCell::new(SinkState::default()));
        let second = Rc::new(RefCell::new(SinkState::default()));
        let (head, mut reader) = read_head(input, MessageLimits::default());
        let pending = PendingFanout::new(vec![
            TestSink::boxed(first.clone()),
            TestSink::failing_write(second.clone(), 0),
        ])
        .unwrap();

        if pending.stream(head, &mut reader).is_ok() {
            panic!("injected sink write failure was ignored");
        }
        assert!(first.borrow().aborted);
        assert!(second.borrow().aborted);
        assert!(first.borrow().visible.is_none());
        assert!(second.borrow().visible.is_none());
    }

    #[test]
    fn dropping_validated_fanout_aborts_without_publishing() {
        let input = b"\nbody";
        let state = Rc::new(RefCell::new(SinkState::default()));
        let (head, mut reader) = read_head(input, MessageLimits::default());
        let pending = PendingFanout::new(vec![TestSink::boxed(state.clone())]).unwrap();

        drop(pending.stream(head, &mut reader).unwrap());
        assert!(state.borrow().aborted);
        assert!(state.borrow().visible.is_none());
    }

    #[test]
    fn commit_error_reports_partial_publication_and_aborts_rest() {
        let input = b"\nbody";
        let first = Rc::new(RefCell::new(SinkState::default()));
        let second = Rc::new(RefCell::new(SinkState::default()));
        let third = Rc::new(RefCell::new(SinkState::default()));
        let (head, mut reader) = read_head(input, MessageLimits::default());
        let pending = PendingFanout::new(vec![
            TestSink::boxed(first.clone()),
            TestSink::failing_commit(second.clone()),
            TestSink::boxed(third.clone()),
        ])
        .unwrap();
        let (validated, _) = pending.stream(head, &mut reader).unwrap();

        let error = validated.commit().unwrap_err();
        assert_eq!(error.committed(), 1);
        assert_eq!(first.borrow().visible.as_deref(), Some(input.as_slice()));
        assert!(second.borrow().visible.is_none());
        assert!(third.borrow().aborted);
        assert!(third.borrow().visible.is_none());
    }

    #[test]
    fn fanout_limit_accepts_boundary_and_rejects_one_more() {
        for count in [MAX_PENDING_SINKS - 1, MAX_PENDING_SINKS] {
            let sinks = (0..count)
                .map(|_| TestSink::boxed(Rc::new(RefCell::new(SinkState::default()))))
                .collect();
            let pending = PendingFanout::new(sinks).unwrap();
            assert_eq!(pending.len(), count);
        }

        let sinks = (0..MAX_PENDING_SINKS + 1)
            .map(|_| TestSink::boxed(Rc::new(RefCell::new(SinkState::default()))))
            .collect();
        let error = PendingFanout::new(sinks).err().unwrap();
        assert_eq!(error.count(), MAX_PENDING_SINKS + 1);
        assert_eq!(error.limit(), MAX_PENDING_SINKS);
    }
}
