// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io;

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, fstat, linkat, openat, statat, unlinkat};

use super::{MAILDIR_FILE_MODE, MAX_NAME_ATTEMPTS, PlatformPublishError, io_error};

pub(super) struct PendingFile {
    file: OwnedFd,
    directory: OwnedFd,
    name: Option<String>,
}

impl PendingFile {
    pub(super) fn create(
        directory: &OwnedFd,
        mask: u32,
        mut next_name: impl FnMut() -> io::Result<String>,
    ) -> io::Result<Self> {
        let owned_directory = rustix::io::fcntl_dupfd_cloexec(directory, 0).map_err(io_error)?;
        for _ in 0..MAX_NAME_ATTEMPTS {
            let name = next_name()?;
            match openat(
                directory,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                super::super::creation_mode(MAILDIR_FILE_MODE, mask),
            ) {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        directory: owned_directory,
                        name: Some(name),
                    });
                }
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(io_error(error)),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("cannot allocate a unique Maildir name after {MAX_NAME_ATTEMPTS} attempts"),
        ))
    }

    pub(super) fn file(&self) -> &OwnedFd {
        &self.file
    }

    pub(super) fn publish(
        mut self,
        _tmp_dir: &OwnedFd,
        new_dir: &OwnedFd,
        _next_name: impl FnMut() -> io::Result<String>,
    ) -> Result<String, PlatformPublishError> {
        let name = self.name.as_deref().ok_or_else(|| {
            PlatformPublishError::before(io::Error::other(
                "Maildir temporary name was already consumed",
            ))
        })?;
        verify_named_file(&self.file, &self.directory, name)
            .map_err(PlatformPublishError::before)?;

        // linkat is the FreeBSD no-replace publication operation. Unlike the
        // Linux path, its source is a pathname, so checks on both sides detect
        // substitutions except for the documented race during each syscall.
        linkat(&self.directory, name, new_dir, name, AtFlags::empty())
            .map_err(io_error)
            .map_err(PlatformPublishError::before)?;
        let published_name = name.to_owned();
        verify_named_file(&self.file, new_dir, name)
            .map_err(|error| PlatformPublishError::after(error, published_name.clone()))?;
        self.cleanup()
            .map_err(|error| PlatformPublishError::after(error, published_name.clone()))?;
        Ok(published_name)
    }

    pub(super) fn abort(mut self, _tmp_dir: &OwnedFd) -> io::Result<()> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> io::Result<()> {
        let Some(name) = self.name.as_deref() else {
            return Ok(());
        };
        verify_named_file(&self.file, &self.directory, name)?;
        unlinkat(&self.directory, name, AtFlags::empty()).map_err(io_error)?;
        self.name = None;
        Ok(())
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn verify_named_file(file: &OwnedFd, directory: &OwnedFd, name: &str) -> io::Result<()> {
    let opened = fstat(file).map_err(io_error)?;
    let named = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io_error)?;
    if FileType::from_raw_mode(named.st_mode) != FileType::RegularFile
        || named.st_dev != opened.st_dev
        || named.st_ino != opened.st_ino
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Maildir temporary file was replaced concurrently",
        ));
    }
    Ok(())
}

pub(super) fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    let random = openat(
        CWD,
        "/dev/random",
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io_error)?;
    let metadata = fstat(&random).map_err(io_error)?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::CharacterDevice {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FreeBSD random source is not a character device",
        ));
    }

    let mut filled = 0usize;
    while filled < bytes.len() {
        match rustix::io::read(&random, &mut bytes[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "FreeBSD random source returned end of file",
                ));
            }
            Ok(count) => {
                filled = filled
                    .checked_add(count)
                    .ok_or_else(|| io::Error::other("random byte count overflows"))?;
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}

pub(super) fn validate_directories(
    maildir: &OwnedFd,
    tmp: &OwnedFd,
    new: &OwnedFd,
    cur: &OwnedFd,
) -> io::Result<()> {
    for (directory, name) in [
        (maildir, "Maildir"),
        (tmp, "Maildir/tmp"),
        (new, "Maildir/new"),
        (cur, "Maildir/cur"),
    ] {
        let metadata = fstat(directory).map_err(io_error)?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} is not a directory"),
            ));
        }
        if metadata.st_uid != rustix::process::getuid().as_raw() || metadata.st_mode & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{name} must be owned by the current uid and not writable by other users"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/delivery/maildir/freebsd.rs"]
mod tests;
