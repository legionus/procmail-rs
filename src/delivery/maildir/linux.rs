// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io;

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, OFlags, RenameFlags, linkat, openat, renameat_with};
use rustix::rand::{GetRandomFlags, getrandom};

use super::{MAILDIR_FILE_MODE, MAX_NAME_ATTEMPTS, PlatformPublishError, io_error};

pub(super) struct PendingFile {
    file: OwnedFd,
}

impl PendingFile {
    pub(super) fn create(
        directory: &OwnedFd,
        mask: u32,
        _next_name: impl FnMut() -> io::Result<String>,
    ) -> io::Result<Self> {
        // Keeping the inode unnamed until commit makes abort a close-only
        // operation. No directory entry needs deletion when validation or a
        // write fails, so another process cannot substitute a cleanup target.
        let file = openat(
            directory,
            ".",
            OFlags::WRONLY | OFlags::TMPFILE | OFlags::CLOEXEC,
            super::super::creation_mode(MAILDIR_FILE_MODE, mask),
        )
        .map_err(io_error)?;
        Ok(Self { file })
    }

    pub(super) fn file(&self) -> &OwnedFd {
        &self.file
    }

    pub(super) fn publish(
        self,
        tmp_dir: &OwnedFd,
        new_dir: &OwnedFd,
        mut next_name: impl FnMut() -> io::Result<String>,
    ) -> Result<String, PlatformPublishError> {
        // Retry only collisions. Other failures cannot be repaired by another
        // name, and the fixed attempt count prevents a broken random source or
        // hostile directory from keeping delivery in this loop.
        let name = link_unique(&self.file, tmp_dir, &mut next_name)?;
        renameat_with(
            tmp_dir,
            name.as_str(),
            new_dir,
            name.as_str(),
            RenameFlags::NOREPLACE,
        )
        .map_err(io_error)
        .map_err(PlatformPublishError::before)?;
        Ok(name)
    }

    pub(super) fn abort(self, _tmp_dir: &OwnedFd) -> io::Result<()> {
        Ok(())
    }
}

fn link_unique(
    file: &OwnedFd,
    directory: &OwnedFd,
    next_name: &mut impl FnMut() -> io::Result<String>,
) -> Result<String, PlatformPublishError> {
    for _ in 0..MAX_NAME_ATTEMPTS {
        let name = next_name().map_err(PlatformPublishError::before)?;
        match linkat(file, "", directory, name.as_str(), AtFlags::EMPTY_PATH) {
            Ok(()) => return Ok(name),
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(PlatformPublishError::before(io_error(error))),
        }
    }
    Err(PlatformPublishError::before(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("cannot allocate a unique Maildir name after {MAX_NAME_ATTEMPTS} attempts"),
    )))
}

pub(super) fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    let mut filled = 0usize;
    while filled < bytes.len() {
        let count = getrandom(&mut bytes[filled..], GetRandomFlags::empty()).map_err(io_error)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Linux random source returned no data",
            ));
        }
        filled = filled
            .checked_add(count)
            .ok_or_else(|| io::Error::other("random byte count overflows"))?;
    }
    Ok(())
}

pub(super) fn validate_directories(
    _maildir: &OwnedFd,
    _tmp: &OwnedFd,
    _new: &OwnedFd,
    _cur: &OwnedFd,
) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/delivery/maildir/linux.rs"]
mod tests;
