// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use crate::message::{Message, MessageHead, MessageReadError, StreamedMessage};

#[cfg(target_os = "linux")]
pub mod local_lock;
#[cfg(target_os = "linux")]
pub mod maildir;
#[cfg(target_os = "linux")]
pub mod mbox;
#[cfg(target_os = "linux")]
pub mod staging;

pub const MAX_PENDING_SINKS: usize = 256;

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

impl DeliveryFailureClass {
    pub fn from_io_error(error: &io::Error) -> Self {
        // Rust 1.93 does not expose EMFILE as a distinct ErrorKind. On the
        // currently supported Linux target, descriptor exhaustion is a
        // temporary process resource failure and can succeed after retry.
        const LINUX_EMFILE: i32 = 24;

        if error.raw_os_error() == Some(LINUX_EMFILE) {
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
}

impl SinkCommitError {
    pub fn before_publication(source: io::Error) -> Self {
        Self {
            source,
            published: None,
        }
    }

    pub fn after_publication(source: io::Error, published: PublishedDelivery) -> Self {
        Self {
            source,
            published: Some(published),
        }
    }

    pub fn published(&self) -> Option<&PublishedDelivery> {
        self.published.as_ref()
    }

    pub fn kind(&self) -> io::ErrorKind {
        self.source.kind()
    }

    pub fn class(&self) -> DeliveryFailureClass {
        DeliveryFailureClass::from_io_error(&self.source)
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
    source: SinkCommitError,
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
mod tests;
