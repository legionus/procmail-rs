// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use crate::message::{Message, MessageHead, MessageReadError, StreamedMessage};

pub mod discard;
pub mod local_lock;
pub mod maildir;
pub mod mbox;
pub mod staging;

pub const MAX_PENDING_SINKS: usize = 256;

pub(crate) fn creation_mode(requested: u32, mask: u32) -> rustix::fs::Mode {
    let allowed = requested & !mask;
    let mut mode = rustix::fs::Mode::empty();
    for (bit, permission) in [
        (0o400, rustix::fs::Mode::RUSR),
        (0o200, rustix::fs::Mode::WUSR),
        (0o100, rustix::fs::Mode::XUSR),
        (0o040, rustix::fs::Mode::RGRP),
        (0o020, rustix::fs::Mode::WGRP),
        (0o010, rustix::fs::Mode::XGRP),
        (0o004, rustix::fs::Mode::ROTH),
        (0o002, rustix::fs::Mode::WOTH),
        (0o001, rustix::fs::Mode::XOTH),
    ] {
        if allowed & bit != 0 {
            mode.insert(permission);
        }
    }
    mode
}

/// A destination which keeps written bytes private until `commit` succeeds.
///
/// Implementations must arrange for dropping the sink, or calling `abort`, to
/// leave no visible delivery behind. `commit` can fail after publication when
/// a requested durability operation fails; that error must carry the exact
/// visible destination.
pub trait PendingSink: Write {
    /// Publishes the pending bytes and reports the exact visible destination.
    fn commit(self: Box<Self>) -> Result<PublishedDelivery, SinkCommitError>;
    fn abort(self: Box<Self>) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryFailureClass {
    Retryable,
    Permanent,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOperation {
    Open,
    Lock,
    Write,
    Publish,
    SyncFile,
    SyncDirectory,
    Unlock,
}

impl DeliveryOperation {
    pub fn description(self) -> &'static str {
        match self {
            Self::Open => "opening destination",
            Self::Lock => "acquiring lock",
            Self::Write => "writing message",
            Self::Publish => "publishing message",
            Self::SyncFile => "syncing file",
            Self::SyncDirectory => "syncing directory",
            Self::Unlock => "releasing lock",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Lock => "lock",
            Self::Write => "write",
            Self::Publish => "publish",
            Self::SyncFile => "sync-file",
            Self::SyncDirectory => "sync-directory",
            Self::Unlock => "unlock",
        }
    }
}

// Keep arbitrary OS error text outside traces. This bounded summary is shared
// by backends, exit-status selection, and both renderers, and records visibility
// independently of success because durability can fail after publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryFailure {
    pub class: DeliveryFailureClass,
    pub operation: DeliveryOperation,
    pub kind: io::ErrorKind,
    pub published: bool,
}

impl DeliveryFailure {
    pub fn from_io(operation: DeliveryOperation, error: &io::Error, published: bool) -> Self {
        Self {
            class: DeliveryFailureClass::from_io_error(error),
            operation,
            kind: error.kind(),
            published,
        }
    }
}

impl DeliveryFailureClass {
    pub fn from_io_error(error: &io::Error) -> Self {
        // Rust 1.93 does not expose descriptor exhaustion as a distinct
        // ErrorKind. Ask rustix for the target ABI value instead of embedding
        // a Linux errno number now that delivery also runs on FreeBSD.
        if error.raw_os_error() == Some(rustix::io::Errno::MFILE.raw_os_error()) {
            return Self::Retryable;
        }
        Self::from_io_kind(error.kind())
    }

    pub fn from_io_kind(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::AlreadyExists
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::Interrupted
            | io::ErrorKind::OutOfMemory
            | io::ErrorKind::ResourceBusy
            | io::ErrorKind::StorageFull
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::WriteZero => Self::Retryable,
            io::ErrorKind::InvalidData
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::IsADirectory
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::NotFound
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::ReadOnlyFilesystem
            | io::ErrorKind::Unsupported => Self::Permanent,
            _ => Self::Internal,
        }
    }
}

#[derive(Debug)]
pub struct SinkCommitError {
    source: io::Error,
    published: Option<PublishedDelivery>,
    operation: DeliveryOperation,
}

impl SinkCommitError {
    pub fn before_publication(source: io::Error) -> Self {
        Self {
            source,
            published: None,
            operation: DeliveryOperation::Publish,
        }
    }

    pub fn after_publication(source: io::Error, published: PublishedDelivery) -> Self {
        Self {
            source,
            published: Some(published),
            operation: DeliveryOperation::Publish,
        }
    }

    pub fn published(&self) -> Option<&PublishedDelivery> {
        self.published.as_ref()
    }

    pub fn class(&self) -> DeliveryFailureClass {
        self.failure().class
    }

    pub fn with_operation(mut self, operation: DeliveryOperation) -> Self {
        self.operation = operation;
        self
    }

    pub fn failure(&self) -> DeliveryFailure {
        DeliveryFailure::from_io(self.operation, &self.source, self.published.is_some())
    }
}

impl fmt::Display for SinkCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for SinkCommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
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
    write_failure: Option<(usize, DeliveryFailure)>,
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
    delivery_failure: Option<(usize, DeliveryFailure)>,
    staging_failure: Option<DeliveryFailure>,
}

#[derive(Debug)]
pub struct CommitError {
    source: SinkCommitError,
    published: Vec<PublishedDelivery>,
    abort_failures: usize,
    failed_index: usize,
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
    delivery_failure: Option<(usize, DeliveryFailure)>,
}

impl PendingFanout {
    pub fn new(sinks: Vec<Box<dyn PendingSink>>) -> Result<Self, FanoutLimitError> {
        if sinks.len() > MAX_PENDING_SINKS {
            return Err(FanoutLimitError::TooManySinks {
                count: sinks.len(),
                limit: MAX_PENDING_SINKS,
            });
        }
        Ok(Self {
            sinks,
            write_failure: None,
        })
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
                    delivery_failure: self.write_failure,
                    staging_failure: None,
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
                    delivery_failure: self.write_failure,
                    staging_failure: None,
                })
            }
        }
    }

    pub fn stage(
        mut self,
        head: MessageHead,
        reader: &mut impl BufRead,
        staging: &mut impl Write,
    ) -> Result<(ValidatedFanout, StreamedMessage), StreamDeliveryError> {
        let mut writer = TeeWriter {
            fanout: &mut self,
            staging,
            failure: None,
        };
        match head.stream_to(reader, &mut writer) {
            Ok(message) => Ok((
                ValidatedFanout {
                    sinks: std::mem::take(&mut self.sinks),
                },
                message,
            )),
            Err(source) => {
                let staging_failure = writer.failure;
                let abort_failures = self.abort_all();
                Err(StreamDeliveryError {
                    source,
                    abort_failures,
                    delivery_failure: self.write_failure,
                    staging_failure,
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

struct TeeWriter<'a, W: Write> {
    fanout: &'a mut PendingFanout,
    staging: &'a mut W,
    failure: Option<DeliveryFailure>,
}

impl<W: Write> Write for TeeWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.fanout.write_all(bytes)?;
        self.staging.write_all(bytes).inspect_err(|error| {
            self.failure = Some(DeliveryFailure::from_io(
                DeliveryOperation::Write,
                error,
                false,
            ));
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.fanout.flush()?;
        self.staging.flush().inspect_err(|error| {
            self.failure = Some(DeliveryFailure::from_io(
                DeliveryOperation::Write,
                error,
                false,
            ));
        })
    }
}

impl Write for PendingFanout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for (index, sink) in self.sinks.iter_mut().enumerate() {
            if let Err(error) = sink.write_all(bytes) {
                self.write_failure = Some((
                    index,
                    DeliveryFailure::from_io(DeliveryOperation::Write, &error, false),
                ));
                return Err(error);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        for (index, sink) in self.sinks.iter_mut().enumerate() {
            if let Err(error) = sink.flush() {
                self.write_failure = Some((
                    index,
                    DeliveryFailure::from_io(DeliveryOperation::Write, &error, false),
                ));
                return Err(error);
            }
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
                delivery_failure: None,
            });
        }

        if let Err(source) = pending.write_all(message) {
            let abort_failures = pending.abort_all().saturating_add(self.abort_all());
            return Err(AppendError {
                source,
                abort_failures,
                delivery_failure: pending.write_failure,
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
                    let failed_index = published.len();
                    if let Some(delivery) = source.published().cloned() {
                        published.push(delivery);
                    }
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
                        failed_index,
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
    pub fn staging_failure(&self) -> Option<DeliveryFailure> {
        self.staging_failure
    }

    pub fn delivery_failure(&self) -> Option<(usize, DeliveryFailure)> {
        self.delivery_failure
    }

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
    pub fn failed_index(&self) -> usize {
        self.failed_index
    }

    pub fn failure(&self) -> DeliveryFailure {
        self.source.failure()
    }

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

    pub fn class(&self) -> DeliveryFailureClass {
        self.source.class()
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
    pub fn delivery_failure(&self) -> Option<(usize, DeliveryFailure)> {
        self.delivery_failure
    }

    pub fn abort_failures(&self) -> usize {
        self.abort_failures
    }

    pub fn class(&self) -> DeliveryFailureClass {
        DeliveryFailureClass::from_io_error(&self.source)
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
#[path = "tests/delivery/mod.rs"]
mod tests;
