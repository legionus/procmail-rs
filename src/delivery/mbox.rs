// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Byte-oriented mboxrd record formatting.

use std::fmt;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fd::OwnedFd;
use rustix::fs::{FileType, FlockOperation, Mode, OFlags, flock, fstat, openat};

use super::maildir::open_directory_path;

pub const MAX_POSTMARK_LEN: usize = 512;
const MBOX_FILE_MODE: u32 = 0o600;
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

pub struct MboxFile {
    file: OwnedFd,
    path: PathBuf,
}

pub struct LockedMbox {
    file: OwnedFd,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Postmark(Vec<u8>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostmarkError {
    TooLong,
    InvalidShape,
    NonAscii,
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
}

impl fmt::Display for PostmarkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooLong => "mbox postmark exceeds its hard limit",
            Self::InvalidShape => "mbox postmark must be one LF-terminated 'From ' line",
            Self::NonAscii => "mbox postmark must contain only printable ASCII",
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
            path: path.to_owned(),
        })
    }

    pub fn lock(self) -> io::Result<LockedMbox> {
        self.lock_with_policy(LOCK_TIMEOUT, LOCK_RETRY_INTERVAL)
    }

    fn lock_with_policy(self, timeout: Duration, retry: Duration) -> io::Result<LockedMbox> {
        let started = Instant::now();
        loop {
            match flock(&self.file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {
                    return Ok(LockedMbox {
                        file: self.file,
                        path: self.path,
                    });
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::AGAIN) => {
                    let elapsed = started.elapsed();
                    let Some(remaining) = timeout.checked_sub(elapsed) else {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out waiting for mbox lock",
                        ));
                    };
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out waiting for mbox lock",
                        ));
                    }
                    thread::sleep(retry.min(remaining));
                }
                Err(error) => return Err(io_error(error)),
            }
        }
    }
}

impl LockedMbox {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn unlock(self) -> io::Result<()> {
        flock(&self.file, FlockOperation::Unlock).map_err(io_error)
    }
}

pub fn write_record(
    writer: &mut impl Write,
    postmark: &Postmark,
    message: &[u8],
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

    if !message.ends_with(b"\n") {
        writer.write_all(b"\n")?;
    }
    writer.write_all(b"\n")
}

fn io_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{MAX_POSTMARK_LEN, MboxFile, Postmark, PostmarkError, write_record};

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
}
