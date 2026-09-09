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
use rustix::fs::{CWD, Mode, OFlags, fsync, openat};

use super::{PendingSink, PublishedDelivery, SinkCommitError};

#[cfg(target_os = "freebsd")]
#[path = "maildir/freebsd.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "maildir/linux.rs"]
mod platform;

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
    pending: platform::PendingFile,
    tmp_dir: OwnedFd,
    new_dir: OwnedFd,
    maildir: PathBuf,
    durability: Durability,
}

impl MaildirSink {
    pub fn create(path: &Path, durability: Durability, mask: u32) -> io::Result<Self> {
        let maildir = open_directory_path(path)?;

        // Validate all three standard components before creating a pending
        // file. Although delivery does not access `cur`, accepting an
        // incomplete directory here would hide a configuration error until a
        // mail reader tries to use the destination. Descriptor-relative opens
        // also reject a component replaced with a symlink during this step.
        let tmp_dir = open_directory_at(&maildir, OsStr::new("tmp"))?;
        let new_dir = open_directory_at(&maildir, OsStr::new("new"))?;
        let cur_dir = open_directory_at(&maildir, OsStr::new("cur"))?;
        platform::validate_directories(&maildir, &tmp_dir, &new_dir, &cur_dir)?;
        let pending = platform::PendingFile::create(&tmp_dir, mask, unique_name)?;
        Ok(Self {
            pending,
            tmp_dir,
            new_dir,
            maildir: path.to_owned(),
            durability,
        })
    }
}

impl Write for MaildirSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        rustix::io::write(self.pending.file(), bytes).map_err(io_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PendingSink for MaildirSink {
    fn commit(self: Box<Self>) -> Result<PublishedDelivery, SinkCommitError> {
        (*self).commit_with(|file| fsync(file).map_err(io_error))
    }

    fn abort(self: Box<Self>) -> io::Result<()> {
        let MaildirSink {
            pending, tmp_dir, ..
        } = *self;
        pending.abort(&tmp_dir)
    }
}

impl MaildirSink {
    // Keep durability ordering above the platform publication mechanism so
    // every backend reaches the same visible success point. Tests replace only
    // fsync here; platform-specific collision and pathname behavior stays in
    // the selected implementation module.
    fn commit_with(
        self,
        mut sync: impl FnMut(&OwnedFd) -> io::Result<()>,
    ) -> Result<PublishedDelivery, SinkCommitError> {
        if self.durability != Durability::None {
            sync(self.pending.file()).map_err(SinkCommitError::before_publication)?;
        }

        let name = self
            .pending
            .publish(&self.tmp_dir, &self.new_dir, unique_name)
            .map_err(|error| match error.into_parts() {
                (source, Some(name)) => SinkCommitError::after_publication(
                    source,
                    PublishedDelivery::new(self.maildir.join("new").join(name)),
                ),
                (source, None) => SinkCommitError::before_publication(source),
            })?;
        let published = PublishedDelivery::new(self.maildir.join("new").join(name));

        if self.durability == Durability::Full {
            for directory in [&self.tmp_dir, &self.new_dir] {
                if let Err(error) = sync(directory) {
                    return Err(SinkCommitError::after_publication(error, published));
                }
            }
        }
        Ok(published)
    }
}

#[derive(Debug)]
struct PlatformPublishError {
    source: io::Error,
    published_name: Option<String>,
}

impl PlatformPublishError {
    fn before(source: io::Error) -> Self {
        Self {
            source,
            published_name: None,
        }
    }

    #[cfg(target_os = "freebsd")]
    fn after(source: io::Error, published_name: String) -> Self {
        Self {
            source,
            published_name: Some(published_name),
        }
    }

    fn into_parts(self) -> (io::Error, Option<String>) {
        (self.source, self.published_name)
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
    platform::fill_random(&mut random)?;

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
#[path = "../tests/delivery/maildir.rs"]
mod tests;
