// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, Mode, OFlags, fstat, mkdirat, openat, unlinkat};

use super::maildir::{open_directory_at, open_directory_path};
use crate::mapped_file::MappedFile;

const DIRECTORY_NAME: &str = ".procmail-rs-staging";
const MAX_NAME_ATTEMPTS: u64 = 128;
static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

pub struct StagingFile {
    file: Option<OwnedFd>,
    directory: OwnedFd,
    name: String,
    pending: bool,
}

impl StagingFile {
    pub fn create(maildir: &Path) -> io::Result<Self> {
        let base = open_directory_path(maildir)?;

        // Staging data may contain the complete message, so the service
        // directory must never be inherited from a permissive pre-existing
        // directory. Creation and opening stay relative to the verified
        // MAILDIR descriptor to avoid pathname replacement between steps.
        match mkdirat(&base, DIRECTORY_NAME, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(io_error(error)),
        }
        let directory = open_directory_at(&base, OsStr::new(DIRECTORY_NAME))?;
        let metadata = fstat(&directory).map_err(io_error)?;
        if metadata.st_mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{DIRECTORY_NAME} must not grant access to group or other users"),
            ));
        }

        for attempt in 0..MAX_NAME_ATTEMPTS {
            let name = unique_name(attempt);
            match openat(
                &directory,
                name.as_str(),
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        directory,
                        name,
                        pending: true,
                    });
                }
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(io_error(error)),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("cannot allocate a staging name after {MAX_NAME_ATTEMPTS} attempts"),
        ))
    }

    pub fn map(mut self, maximum_len: usize, header_len: usize) -> io::Result<StagedMessage> {
        let file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("staging file was already consumed before mapping"))?;

        // `MappedFile` removes the name before exposing bytes. Clear our
        // cleanup flag only after that succeeds, otherwise Drop must still
        // remove the private file left by a failed size check or mmap call.
        let mapping = MappedFile::unlink_and_map(file, &self.directory, &self.name, maximum_len)?;
        self.pending = false;
        if header_len > mapping.as_bytes().len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staging header length exceeds the message size",
            ));
        }
        Ok(StagedMessage {
            mapping,
            header_len,
        })
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if !self.pending {
            return Ok(());
        }
        match unlinkat(&self.directory, self.name.as_str(), AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {
                self.pending = false;
                Ok(())
            }
            Err(error) => Err(io_error(error)),
        }
    }
}

pub struct StagedMessage {
    mapping: MappedFile,
    header_len: usize,
}

impl StagedMessage {
    pub fn as_bytes(&self) -> &[u8] {
        self.mapping.as_bytes()
    }

    pub fn header_len(&self) -> usize {
        self.header_len
    }
}

impl Write for StagingFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::other("cannot write a consumed staging file"))?;
        rustix::io::write(file, bytes).map_err(io_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn unique_name(attempt: u64) -> String {
    let sequence = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!(
        "message.{}.{}.{}.{}",
        process::id(),
        timestamp,
        sequence,
        attempt
    )
}

fn io_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}
