// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

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
    max_write_size: Option<usize>,
    fail_write_at: Option<usize>,
    fail_commit: bool,
    fail_after_publish: bool,
    fail_abort: bool,
    last_folder: PathBuf,
}

impl TestSink {
    fn boxed(state: Rc<RefCell<SinkState>>) -> Box<dyn PendingSink> {
        Box::new(Self {
            state,
            pending: Vec::new(),
            max_write_size: None,
            fail_write_at: None,
            fail_commit: false,
            fail_after_publish: false,
            fail_abort: false,
            last_folder: PathBuf::from("test-folder"),
        })
    }

    fn failing_write(state: Rc<RefCell<SinkState>>, at: usize) -> Box<dyn PendingSink> {
        Box::new(Self {
            state,
            pending: Vec::new(),
            max_write_size: None,
            fail_write_at: Some(at),
            fail_commit: false,
            fail_after_publish: false,
            fail_abort: false,
            last_folder: PathBuf::from("test-folder"),
        })
    }

    fn short_writing(state: Rc<RefCell<SinkState>>, size: usize) -> Box<dyn PendingSink> {
        Box::new(Self {
            state,
            pending: Vec::new(),
            max_write_size: Some(size),
            fail_write_at: None,
            fail_commit: false,
            fail_after_publish: false,
            fail_abort: false,
            last_folder: PathBuf::from("test-folder"),
        })
    }

    fn failing_commit(state: Rc<RefCell<SinkState>>) -> Box<dyn PendingSink> {
        Box::new(Self {
            state,
            pending: Vec::new(),
            max_write_size: None,
            fail_write_at: None,
            fail_commit: true,
            fail_after_publish: false,
            fail_abort: false,
            last_folder: PathBuf::from("test-folder"),
        })
    }

    fn failing_after_publish(state: Rc<RefCell<SinkState>>) -> Box<dyn PendingSink> {
        Box::new(Self {
            state,
            pending: Vec::new(),
            max_write_size: None,
            fail_write_at: None,
            fail_commit: false,
            fail_after_publish: true,
            fail_abort: false,
            last_folder: PathBuf::from("test-folder"),
        })
    }
}

impl Write for TestSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(limit) = self.fail_write_at {
            let remaining = limit.saturating_sub(self.pending.len());
            if remaining == 0 {
                return Err(io::Error::other("injected write failure"));
            }
            let length = bytes.len().min(remaining);
            self.pending.extend_from_slice(&bytes[..length]);
            return Ok(length);
        }
        let length = self
            .max_write_size
            .map_or(bytes.len(), |maximum| bytes.len().min(maximum));
        self.pending.extend_from_slice(&bytes[..length]);
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PendingSink for TestSink {
    fn commit(self: Box<Self>) -> Result<PublishedDelivery, SinkCommitError> {
        if self.fail_commit {
            return Err(SinkCommitError::before_publication(io::Error::other(
                "injected commit failure",
            )));
        }
        self.state.borrow_mut().visible = Some(self.pending.clone());
        let published = PublishedDelivery::new(self.last_folder.clone());
        if self.fail_after_publish {
            return Err(SinkCommitError::after_publication(
                io::Error::new(io::ErrorKind::StorageFull, "injected durability failure"),
                published,
            ));
        }
        Ok(published)
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
fn short_writes_preserve_the_complete_original_message() {
    let input = b"Subject: test\n\nbinary:\xff\x00body";
    let state = Rc::new(RefCell::new(SinkState::default()));
    let (head, mut reader) = read_head(input, MessageLimits::default());
    let pending = PendingFanout::new(vec![TestSink::short_writing(state.clone(), 3)]).unwrap();

    let (validated, message) = pending.stream(head, &mut reader).unwrap();
    assert_eq!(message.len(), input.len());
    validated.commit().unwrap();

    assert_eq!(state.borrow().visible.as_deref(), Some(input.as_slice()));
}

#[test]
fn failure_after_a_partial_write_never_publishes_a_message() {
    let input = b"Subject: test\n\nbody";
    let state = Rc::new(RefCell::new(SinkState::default()));
    let (head, mut reader) = read_head(input, MessageLimits::default());
    let pending = PendingFanout::new(vec![TestSink::failing_write(state.clone(), 5)]).unwrap();

    assert!(pending.stream(head, &mut reader).is_err());
    assert!(state.borrow().aborted);
    assert!(state.borrow().visible.is_none());
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
fn durability_failure_after_publication_is_not_reported_as_success() {
    let input = b"\nbody";
    let state = Rc::new(RefCell::new(SinkState::default()));
    let (head, mut reader) = read_head(input, MessageLimits::default());
    let pending = PendingFanout::new(vec![TestSink::failing_after_publish(state.clone())]).unwrap();
    let (validated, _) = pending.stream(head, &mut reader).unwrap();

    let error = validated.commit().unwrap_err();
    assert_eq!(error.committed(), 1);
    assert_eq!(error.last_folder(), Some(Path::new("test-folder")));
    assert_eq!(error.class(), DeliveryFailureClass::Retryable);
    assert_eq!(state.borrow().visible.as_deref(), Some(input.as_slice()));
}

#[test]
fn delivery_errors_preserve_retry_categories_through_fanout() {
    for (kind, expected) in [
        (io::ErrorKind::StorageFull, DeliveryFailureClass::Retryable),
        (
            io::ErrorKind::PermissionDenied,
            DeliveryFailureClass::Permanent,
        ),
        (io::ErrorKind::Other, DeliveryFailureClass::Internal),
    ] {
        let error = SinkCommitError::before_publication(io::Error::from(kind));
        assert_eq!(error.class(), expected);
    }
    assert_eq!(
        DeliveryFailureClass::from_io_error(&io::Error::from_raw_os_error(24)),
        DeliveryFailureClass::Retryable
    );

    let input = b"\nbody";
    let first = Rc::new(RefCell::new(SinkState::default()));
    let second = Rc::new(RefCell::new(SinkState::default()));
    let (head, mut reader) = read_head(input, MessageLimits::default());
    let pending = PendingFanout::new(vec![
        TestSink::boxed(first),
        TestSink::failing_commit(second),
    ])
    .unwrap();
    let (validated, _) = pending.stream(head, &mut reader).unwrap();

    assert_eq!(
        validated.commit().unwrap_err().class(),
        DeliveryFailureClass::Internal
    );
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
