// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Byte-oriented mboxrd record formatting.

use std::fmt;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fd::OwnedFd;
use rustix::fs::{
    FileType, FlockOperation, Mode, OFlags, SeekFrom, flock, fstat, fsync, ftruncate, openat, seek,
};

use crate::config::OutputEnding;

use super::local_lock::acquire_flock_fd;
use super::maildir::Durability;
use super::maildir::open_directory_path;
use super::{DeliveryFailureClass, PublishedDelivery};

pub const MAX_POSTMARK_LEN: usize = 512;
const MBOX_FILE_MODE: u32 = 0o600;
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

pub struct MboxFile {
    file: OwnedFd,
    parent: OwnedFd,
    path: PathBuf,
}

pub struct LockedMbox {
    file: OwnedFd,
    parent: OwnedFd,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Postmark(Vec<u8>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostmarkError {
    TooLong,
    InvalidShape,
    NonAscii,
    TimeBeforeEpoch,
    TimeOutOfRange,
}

#[derive(Debug)]
pub struct MboxAppendError {
    source: io::Error,
    rollback: Option<io::Error>,
    published: bool,
}

impl Postmark {
    pub fn new(bytes: &[u8]) -> Result<Self, PostmarkError> {
        if bytes.len() > MAX_POSTMARK_LEN {
            return Err(PostmarkError::TooLong);
        }
        if !bytes.starts_with(b"From ")
            || !bytes.ends_with(b"\n")
            || bytes[..bytes.len().saturating_sub(1)].contains(&b'\n')
        {
            return Err(PostmarkError::InvalidShape);
        }
        if !bytes[..bytes.len() - 1]
            .iter()
            .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
        {
            return Err(PostmarkError::NonAscii);
        }
        Ok(Self(bytes.to_vec()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn generated(time: SystemTime) -> Result<Self, PostmarkError> {
        let seconds = time
            .duration_since(UNIX_EPOCH)
            .map_err(|_| PostmarkError::TimeBeforeEpoch)?
            .as_secs();
        let days = i64::try_from(seconds / 86_400).map_err(|_| PostmarkError::TimeOutOfRange)?;
        let seconds_in_day = seconds % 86_400;
        let hour = seconds_in_day / 3_600;
        let minute = seconds_in_day % 3_600 / 60;
        let second = seconds_in_day % 60;
        let (year, month, day) = civil_from_days(days).ok_or(PostmarkError::TimeOutOfRange)?;
        let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]
            [usize::try_from(days.rem_euclid(7)).map_err(|_| PostmarkError::TimeOutOfRange)?];
        let month = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ]
        .get(month.checked_sub(1).ok_or(PostmarkError::TimeOutOfRange)? as usize)
        .ok_or(PostmarkError::TimeOutOfRange)?;
        let line = format!(
            "From MAILER-DAEMON {weekday} {month} {day:>2} {hour:02}:{minute:02}:{second:02} {year:04}\n"
        );
        Self::new(line.as_bytes())
    }
}

impl fmt::Display for PostmarkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooLong => "mbox postmark exceeds its hard limit",
            Self::InvalidShape => "mbox postmark must be one LF-terminated 'From ' line",
            Self::NonAscii => "mbox postmark must contain only printable ASCII",
            Self::TimeBeforeEpoch => "cannot generate mbox postmark before the Unix epoch",
            Self::TimeOutOfRange => "cannot represent timestamp in mbox postmark",
        })
    }
}

impl std::error::Error for PostmarkError {}

impl MboxFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "mbox path has no file name")
        })?;
        let parent_path = path.parent().unwrap_or_else(|| Path::new("."));
        let parent = open_directory_path(parent_path)?;
        let fd = openat(
            &parent,
            name.as_bytes(),
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(MBOX_FILE_MODE),
        )
        .map_err(io_error)?;
        let stat = fstat(&fd).map_err(io_error)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mbox destination is not a regular file",
            ));
        }
        if stat.st_nlink != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mbox destination must have exactly one hard link",
            ));
        }
        Ok(Self {
            file: fd,
            parent,
            path: path.to_owned(),
        })
    }

    pub fn lock(self) -> io::Result<LockedMbox> {
        self.lock_with_policy(LOCK_TIMEOUT, LOCK_RETRY_INTERVAL)
    }

    pub fn lock_with_timeout(self, timeout: Duration) -> io::Result<LockedMbox> {
        self.lock_with_policy(timeout, LOCK_RETRY_INTERVAL)
    }

    fn lock_with_policy(self, timeout: Duration, retry: Duration) -> io::Result<LockedMbox> {
        acquire_flock_fd(
            &self.file,
            timeout,
            retry,
            "timed out waiting for mbox lock",
        )?;
        Ok(LockedMbox {
            file: self.file,
            parent: self.parent,
            path: self.path,
        })
    }
}

impl LockedMbox {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn unlock(self) -> io::Result<()> {
        flock(&self.file, FlockOperation::Unlock).map_err(io_error)
    }

    pub fn append(
        self,
        message: &[u8],
        output_ending: OutputEnding,
        durability: Durability,
    ) -> Result<PublishedDelivery, MboxAppendError> {
        self.append_with(durability, |file| {
            let postmark = Postmark::generated(SystemTime::now()).map_err(io::Error::other)?;
            write_record_with_ending(&mut FdWriter(file), &postmark, message, output_ending)
        })
    }

    fn append_with(
        self,
        durability: Durability,
        write: impl FnOnce(&OwnedFd) -> io::Result<()>,
    ) -> Result<PublishedDelivery, MboxAppendError> {
        self.append_with_sync(durability, write, |file| fsync(file).map_err(io_error))
    }

    fn append_with_sync(
        self,
        durability: Durability,
        write: impl FnOnce(&OwnedFd) -> io::Result<()>,
        sync: impl Fn(&OwnedFd) -> io::Result<()>,
    ) -> Result<PublishedDelivery, MboxAppendError> {
        let original_len = seek(&self.file, SeekFrom::End(0))
            .map_err(|error| MboxAppendError::before_publication(io_error(error), None))?;

        // Every failure before unlock attempts to restore the exact original
        // length while this writer still owns the lock. Preserve both errors
        // when recovery fails so callers can escalate possible corruption.
        let operation = write(&self.file).and_then(|()| match durability {
            Durability::None => Ok(()),
            Durability::File | Durability::Full => sync(&self.file),
        });
        if let Err(source) = operation {
            let rollback = rollback(&self.file, original_len, durability).err();
            let _unlock = flock(&self.file, FlockOperation::Unlock);
            return Err(MboxAppendError::before_publication(source, rollback));
        }

        if durability == Durability::Full
            && let Err(source) = sync(&self.parent)
        {
            let rollback = rollback(&self.file, original_len, durability).err();
            let _unlock = flock(&self.file, FlockOperation::Unlock);
            return Err(MboxAppendError::before_publication(source, rollback));
        }

        let published = PublishedDelivery::new(self.path.clone());
        flock(&self.file, FlockOperation::Unlock)
            .map_err(io_error)
            .map_err(MboxAppendError::after_publication)?;
        Ok(published)
    }
}

impl MboxAppendError {
    fn before_publication(source: io::Error, rollback: Option<io::Error>) -> Self {
        Self {
            source,
            rollback,
            published: false,
        }
    }

    fn after_publication(source: io::Error) -> Self {
        Self {
            source,
            rollback: None,
            published: true,
        }
    }

    pub fn class(&self) -> DeliveryFailureClass {
        if self.rollback.is_some() {
            DeliveryFailureClass::Internal
        } else {
            DeliveryFailureClass::from_io_error(&self.source)
        }
    }

    pub fn published(&self) -> bool {
        self.published
    }

    pub fn rollback_failed(&self) -> bool {
        self.rollback.is_some()
    }
}

impl fmt::Display for MboxAppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "cannot append mbox record: {}", self.source)?;
        if let Some(error) = &self.rollback {
            write!(formatter, "; cannot complete mailbox rollback: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for MboxAppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub fn write_record(
    writer: &mut impl Write,
    postmark: &Postmark,
    message: &[u8],
) -> io::Result<()> {
    write_record_with_ending(writer, postmark, message, OutputEnding::Normalize)
}

pub fn write_record_with_ending(
    writer: &mut impl Write,
    postmark: &Postmark,
    message: &[u8],
    output_ending: OutputEnding,
) -> io::Result<()> {
    writer.write_all(postmark.as_bytes())?;

    // Quote directly from the input slices so one hostile line cannot cause a
    // second message-sized allocation. mboxrd adds one '>' to every physical
    // line whose first non-'>' bytes are exactly "From ".
    let mut offset = 0usize;
    while offset < message.len() {
        let relative_end = message[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(message.len() - offset, |index| index + 1);
        let end = offset + relative_end;
        let line = &message[offset..end];
        let prefix = line.iter().take_while(|byte| **byte == b'>').count();
        if line[prefix..].starts_with(b"From ") {
            writer.write_all(b">")?;
        }
        writer.write_all(line)?;
        offset = end;
    }

    // Even raw-mode records need the next postmark to begin on a fresh line.
    // Add only that structural LF in preserve mode; the normal mode adds one
    // more LF so adjacent records retain the project's documented separator.
    if !message.ends_with(b"\n")
        && (output_ending == OutputEnding::Normalize || !message.is_empty())
    {
        writer.write_all(b"\n")?;
    }
    if output_ending == OutputEnding::Normalize {
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn io_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

struct FdWriter<'a>(&'a OwnedFd);

impl Write for FdWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        rustix::io::write(self.0, bytes).map_err(io_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn rollback(file: &OwnedFd, original_len: u64, durability: Durability) -> io::Result<()> {
    ftruncate(file, original_len).map_err(io_error)?;
    seek(file, SeekFrom::End(0)).map_err(io_error)?;
    if durability != Durability::None {
        fsync(file).map_err(io_error)?;
    }
    Ok(())
}

fn civil_from_days(days_since_epoch: i64) -> Option<(i64, u32, u32)> {
    // Convert a non-negative Unix day number to the proleptic Gregorian date
    // using only checked integer arithmetic, avoiding locale and libc state in
    // the security-sensitive postmark path.
    let shifted = days_since_epoch.checked_add(719_468)?;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era.checked_add(era.checked_mul(400)?)?;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    Some((year, u32::try_from(month).ok()?, u32::try_from(day).ok()?))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use std::{sync::Arc, sync::Barrier, thread};

    use crate::config::OutputEnding;

    use super::{
        MAX_POSTMARK_LEN, MboxFile, Postmark, PostmarkError, write_record, write_record_with_ending,
    };
    use crate::delivery::maildir::Durability;

    fn temporary_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "procmail-rs-mbox-{}-{unique}-{name}",
            std::process::id()
        ))
    }

    fn postmark() -> Postmark {
        Postmark::new(b"From MAILER-DAEMON Thu Jan  1 00:00:00 1970\n").unwrap()
    }

    #[test]
    fn quotes_every_mboxrd_line_reversibly() {
        let message = b"From hostile header\nX: value\n\nFrom body\n>From quoted\n>>From twice\nnot From safe\n";
        let mut output = Vec::new();

        write_record(&mut output, &postmark(), message).unwrap();

        assert_eq!(
            output,
            b"From MAILER-DAEMON Thu Jan  1 00:00:00 1970\n>From hostile header\nX: value\n\n>From body\n>>From quoted\n>>>From twice\nnot From safe\n\n"
        );
    }

    #[test]
    fn adds_record_separator_without_changing_input() {
        for (message, suffix) in [
            (&b"body"[..], &b"body\n\n"[..]),
            (&b"body\n"[..], &b"body\n\n"[..]),
            (&b"body\n\n"[..], &b"body\n\n\n"[..]),
            (&b""[..], &b"\n"[..]),
        ] {
            let original = message.to_vec();
            let mut output = Vec::new();
            write_record(&mut output, &postmark(), message).unwrap();
            assert!(output.ends_with(suffix));
            assert_eq!(message, original);
        }
    }

    #[test]
    fn raw_mode_adds_only_the_line_ending_required_for_mbox_framing() {
        for (message, suffix) in [
            (&b"body"[..], &b"body\n"[..]),
            (&b"body\n"[..], &b"body\n"[..]),
            (&b"body\n\n"[..], &b"body\n\n"[..]),
            (&b""[..], &b"1970\n"[..]),
        ] {
            let mut output = Vec::new();
            write_record_with_ending(&mut output, &postmark(), message, OutputEnding::Preserve)
                .unwrap();
            assert!(output.ends_with(suffix), "{output:?}");
        }
    }

    #[test]
    fn validates_postmark_shape_and_limit() {
        assert_eq!(
            Postmark::new(b"not From\n"),
            Err(PostmarkError::InvalidShape)
        );
        assert_eq!(
            Postmark::new(b"From first\nsecond\n"),
            Err(PostmarkError::InvalidShape)
        );
        assert_eq!(
            Postmark::new(b"From non-ascii \xff\n"),
            Err(PostmarkError::NonAscii)
        );

        let mut boundary = b"From ".to_vec();
        boundary.resize(MAX_POSTMARK_LEN - 1, b'x');
        boundary.push(b'\n');
        assert!(Postmark::new(&boundary).is_ok());
        boundary.insert(MAX_POSTMARK_LEN - 1, b'x');
        assert_eq!(Postmark::new(&boundary), Err(PostmarkError::TooLong));
    }

    #[test]
    fn generates_stable_utc_postmark() {
        assert_eq!(
            Postmark::generated(UNIX_EPOCH).unwrap().as_bytes(),
            b"From MAILER-DAEMON Thu Jan  1 00:00:00 1970\n"
        );
        assert_eq!(
            Postmark::generated(UNIX_EPOCH + Duration::from_secs(951_827_696))
                .unwrap()
                .as_bytes(),
            b"From MAILER-DAEMON Tue Feb 29 12:34:56 2000\n"
        );
    }

    #[test]
    fn opens_regular_mailbox_without_following_symlinks_or_hard_links() {
        let directory = temporary_path("open");
        fs::create_dir(&directory).unwrap();
        let mailbox = directory.join("mailbox");
        let _opened = MboxFile::open(&mailbox).unwrap();
        assert_eq!(
            fs::metadata(&mailbox).unwrap().permissions().mode() & 0o077,
            0
        );

        let symlink = directory.join("symlink");
        std::os::unix::fs::symlink(&mailbox, &symlink).unwrap();
        assert!(MboxFile::open(&symlink).is_err());

        let hardlink = directory.join("hardlink");
        fs::hard_link(&mailbox, &hardlink).unwrap();
        assert!(MboxFile::open(&mailbox).is_err());

        let inaccessible = directory.join("inaccessible");
        fs::write(&inaccessible, b"").unwrap();
        fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(
            MboxFile::open(&inaccessible).err().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        drop(_opened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lock_timeout_is_finite() {
        let directory = temporary_path("lock");
        fs::create_dir(&directory).unwrap();
        let mailbox = directory.join("mailbox");
        let first = MboxFile::open(&mailbox).unwrap().lock().unwrap();
        let second = MboxFile::open(&mailbox).unwrap();

        let error = second
            .lock_with_policy(Duration::ZERO, Duration::ZERO)
            .err()
            .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);

        first.unlock().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn append_publishes_complete_record_and_reports_path() {
        let directory = temporary_path("append");
        fs::create_dir(&directory).unwrap();
        let mailbox = directory.join("mailbox");
        let published = MboxFile::open(&mailbox)
            .unwrap()
            .lock()
            .unwrap()
            .append(
                b"Subject: test\n\nFrom body",
                OutputEnding::Normalize,
                Durability::File,
            )
            .unwrap();

        assert_eq!(published.last_folder(), mailbox);
        let bytes = fs::read(&mailbox).unwrap();
        assert!(bytes.starts_with(b"From MAILER-DAEMON "));
        assert!(bytes.ends_with(b"Subject: test\n\n>From body\n\n"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn append_failure_restores_original_length() {
        let directory = temporary_path("rollback");
        fs::create_dir(&directory).unwrap();
        let mailbox = directory.join("mailbox");
        fs::write(&mailbox, b"existing").unwrap();
        let locked = MboxFile::open(&mailbox).unwrap().lock().unwrap();

        let error = locked
            .append_with(Durability::None, |file| {
                rustix::io::write(file, b"partial").map_err(super::io_error)?;
                Err(io::Error::from_raw_os_error(28))
            })
            .unwrap_err();

        assert!(!error.published());
        assert!(!error.rollback_failed());
        let mut bytes = Vec::new();
        fs::File::open(&mailbox)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"existing");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn durability_failure_is_rolled_back_and_not_published() {
        let directory = temporary_path("sync-failure");
        fs::create_dir(&directory).unwrap();
        let mailbox = directory.join("mailbox");
        fs::write(&mailbox, b"existing").unwrap();
        let locked = MboxFile::open(&mailbox).unwrap().lock().unwrap();

        let error = locked
            .append_with_sync(
                Durability::File,
                |file| {
                    rustix::io::write(file, b"complete-record").map_err(super::io_error)?;
                    Ok(())
                },
                |_| Err(io::Error::other("injected sync failure")),
            )
            .unwrap_err();

        assert!(!error.published());
        assert!(!error.rollback_failed());
        assert_eq!(fs::read(&mailbox).unwrap(), b"existing");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_writers_append_intact_records() {
        const WRITERS: usize = 16;

        let directory = temporary_path("concurrent");
        fs::create_dir(&directory).unwrap();
        let mailbox = directory.join("mailbox");
        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut workers = Vec::new();
        for index in 0..WRITERS {
            let mailbox = mailbox.clone();
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                let message = format!("Subject: writer-{index:02}\n\nbody-{index:02}\n");
                barrier.wait();
                MboxFile::open(&mailbox)
                    .unwrap()
                    .lock()
                    .unwrap()
                    .append(
                        message.as_bytes(),
                        OutputEnding::Normalize,
                        Durability::None,
                    )
                    .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let bytes = fs::read(&mailbox).unwrap();
        assert_eq!(
            bytes
                .windows(b"From MAILER-DAEMON ".len())
                .filter(|window| *window == b"From MAILER-DAEMON ")
                .count(),
            WRITERS
        );
        for index in 0..WRITERS {
            let marker = format!("Subject: writer-{index:02}\n\nbody-{index:02}\n\n");
            assert_eq!(
                bytes
                    .windows(marker.len())
                    .filter(|window| *window == marker.as_bytes())
                    .count(),
                1
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_existing_bytes_are_not_parsed_or_rewritten() {
        let directory = temporary_path("malformed");
        fs::create_dir(&directory).unwrap();
        let mailbox = directory.join("mailbox");
        let original = b"not an mbox\x00\xffwithout newline";
        fs::write(&mailbox, original).unwrap();

        MboxFile::open(&mailbox)
            .unwrap()
            .lock()
            .unwrap()
            .append(
                b"Subject: appended\n\nbody",
                OutputEnding::Normalize,
                Durability::None,
            )
            .unwrap();

        let bytes = fs::read(&mailbox).unwrap();
        assert!(bytes.starts_with(original));
        assert!(bytes.ends_with(b"Subject: appended\n\nbody\n\n"));
        fs::remove_dir_all(directory).unwrap();
    }
}
