// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Delivery into an existing Maildir directory structure.
//!
//! This backend never creates, changes ownership of, or changes permissions
//! on the Maildir directories. A pending message is created with the process's
//! filesystem identity and requests mode `0600`; the process umask may remove
//! owner permissions but cannot grant access to group or other users.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, RenameFlags, fsync, linkat, openat, renameat_with};
use rustix::rand::{GetRandomFlags, getrandom};

use super::{PendingSink, PublishedDelivery, SinkCommitError};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Durability {
    #[default]
    None,
    File,
    Full,
}

impl Durability {
    pub fn from_config(config: &crate::config::Config) -> Result<Self, String> {
        let mut policy = Self::None;
        for statement in &config.statements {
            let crate::config::Statement::Assignment(assignment) = statement else {
                continue;
            };
            if assignment.target != crate::config::AssignmentTarget::Durability {
                continue;
            }
            policy = match assignment.value.as_str() {
                "none" => Self::None,
                "file" => Self::File,
                "full" => Self::Full,
                _ => {
                    return Err(format!(
                        "line {}: invalid DURABILITY: expected 'none', 'file', or 'full'",
                        assignment.line
                    ));
                }
            };
        }
        Ok(policy)
    }
}

const MAX_NAME_ATTEMPTS: u64 = 128;
const MAILDIR_NAME_PREFIX: &str = "procmail-rs.";
const MAILDIR_RANDOM_BYTES: usize = 16;
const MAILDIR_NAME_LEN: usize = MAILDIR_NAME_PREFIX.len() + MAILDIR_RANDOM_BYTES * 2;
const MAILDIR_FILE_MODE: u32 = 0o600;

/// A pending delivery into an existing Maildir.
///
/// The destination and its `tmp`, `new`, and `cur` directories must already
/// exist. Delivery never creates or repairs Maildir directory structures.
pub struct MaildirSink {
    file: OwnedFd,
    tmp_dir: OwnedFd,
    new_dir: OwnedFd,
    maildir: PathBuf,
    durability: Durability,
}

impl MaildirSink {
    pub fn create(path: &Path) -> io::Result<Self> {
        Self::create_with_durability(path, Durability::None)
    }

    pub fn create_with_durability(path: &Path, durability: Durability) -> io::Result<Self> {
        Self::create_with_durability_and_mask(path, durability, 0)
    }

    pub fn create_with_durability_and_mask(
        path: &Path,
        durability: Durability,
        mask: u32,
    ) -> io::Result<Self> {
        let maildir = open_directory_path(path)?;

        // Validate all three standard components before creating a pending
        // file. Although delivery does not access `cur`, accepting an
        // incomplete directory here would hide a configuration error until a
        // mail reader tries to use the destination. Descriptor-relative opens
        // also reject a component replaced with a symlink during this step.
        let tmp_dir = open_directory_at(&maildir, OsStr::new("tmp"))?;
        let new_dir = open_directory_at(&maildir, OsStr::new("new"))?;
        let _cur_dir = open_directory_at(&maildir, OsStr::new("cur"))?;

        let file = create_unnamed_pending_file(&tmp_dir, mask)?;
        Ok(Self {
            file,
            tmp_dir,
            new_dir,
            maildir: path.to_owned(),
            durability,
        })
    }
}

fn create_unnamed_pending_file(dir: &OwnedFd, mask: u32) -> io::Result<OwnedFd> {
    // Keeping the inode unnamed until commit makes abort a close-only
    // operation. No directory entry needs deletion when validation or a write
    // fails, so another process cannot substitute a victim for cleanup.
    openat(
        dir,
        ".",
        OFlags::WRONLY | OFlags::TMPFILE | OFlags::CLOEXEC,
        Mode::from_raw_mode(MAILDIR_FILE_MODE & !mask),
    )
    .map_err(io_error)
}

fn link_unique_pending_file(
    file: &OwnedFd,
    dir: &OwnedFd,
    mut next_name: impl FnMut() -> io::Result<String>,
) -> io::Result<String> {
    // Retry only collisions: another error describes a condition that choosing
    // another name cannot repair. The fixed attempt count also prevents a bad
    // random source or a hostile directory from keeping delivery in this loop.
    for _ in 0..MAX_NAME_ATTEMPTS {
        let name = next_name()?;
        match linkat(file, "", dir, name.as_str(), AtFlags::EMPTY_PATH) {
            Ok(()) => return Ok(name),
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(io_error(error)),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("cannot allocate a unique Maildir name after {MAX_NAME_ATTEMPTS} attempts"),
    ))
}

fn publish_linked_file(tmp_dir: &OwnedFd, new_dir: &OwnedFd, name: &str) -> io::Result<()> {
    renameat_with(tmp_dir, name, new_dir, name, RenameFlags::NOREPLACE).map_err(io_error)
}

impl Write for MaildirSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        rustix::io::write(&self.file, bytes).map_err(io_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PendingSink for MaildirSink {
    fn commit(self: Box<Self>) -> Result<PublishedDelivery, SinkCommitError> {
        (*self).commit_with(|file| fsync(file).map_err(io_error), publish_linked_file)
    }

    fn abort(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }
}

impl MaildirSink {
    // Keep publication ordering in one path while allowing tests to fail the
    // otherwise hard-to-reproduce sync and rename syscalls. The supplied
    // operations must have the same success effects as fsync and the
    // descriptor-relative tmp-to-new rename used by production.
    fn commit_with(
        self,
        mut sync: impl FnMut(&OwnedFd) -> io::Result<()>,
        rename: impl FnOnce(&OwnedFd, &OwnedFd, &str) -> io::Result<()>,
    ) -> Result<PublishedDelivery, SinkCommitError> {
        if self.durability != Durability::None {
            sync(&self.file).map_err(SinkCommitError::before_publication)?;
        }

        let name = link_unique_pending_file(&self.file, &self.tmp_dir, unique_name)
            .map_err(SinkCommitError::before_publication)?;

        // Publish with one descriptor-relative rename so readers observe
        // either no entry or the complete file. A cross-mount layout fails
        // with EXDEV; falling back to copy-and-remove would expose partial
        // contents and is deliberately not attempted.
        match rename(&self.tmp_dir, &self.new_dir, &name) {
            Ok(()) => {
                let published = PublishedDelivery::new(self.maildir.join("new").join(&name));
                if self.durability == Durability::Full {
                    for directory in [&self.tmp_dir, &self.new_dir] {
                        if let Err(error) = sync(directory) {
                            return Err(SinkCommitError::after_publication(error, published));
                        }
                    }
                }
                Ok(published)
            }
            Err(error) => Err(SinkCommitError::before_publication(error)),
        }
    }
}

pub(crate) fn open_directory_path(path: &Path) -> io::Result<OwnedFd> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Maildir path is empty",
        ));
    }

    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal_component = true,
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Maildir path must not contain '..'",
                ));
            }
            Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Maildir path has an unsupported prefix",
                ));
            }
            Component::RootDir | Component::CurDir => {}
        }
    }
    if !path.is_absolute() && !has_normal_component {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Maildir path does not name a directory",
        ));
    }

    let mut directory = if path.is_absolute() {
        open_directory_at(CWD, OsStr::new("/"))?
    } else {
        open_directory_at(CWD, OsStr::new("."))?
    };
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                directory = open_directory_at(&directory, name)?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Maildir path changed during validation",
                ));
            }
        }
    }
    Ok(directory)
}

pub(crate) fn open_directory_at(dir: impl rustix::fd::AsFd, name: &OsStr) -> io::Result<OwnedFd> {
    openat(
        dir,
        name.as_encoded_bytes(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io_error)
}

fn unique_name() -> io::Result<String> {
    let mut random = [0u8; MAILDIR_RANDOM_BYTES];
    getrandom(&mut random, GetRandomFlags::empty()).map_err(io_error)?;

    // Encode the fixed-size random value directly so the name cannot contain
    // path separators or metadata about the process. Exclusive creation below
    // remains the final collision check even when the random source repeats.
    let mut name = String::with_capacity(MAILDIR_NAME_LEN);
    name.push_str(MAILDIR_NAME_PREFIX);
    for byte in random {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").map_err(|_| io::Error::other("cannot format Maildir name"))?;
    }
    debug_assert_eq!(name.len(), MAILDIR_NAME_LEN);
    Ok(name)
}

fn io_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(test)]
mod tests;
