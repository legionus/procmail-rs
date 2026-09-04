// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::process::Stdio;

use procmail_rs::runtime::{RuntimeSettings, RuntimeVariables};

pub(super) struct CommandLog<'a> {
    runtime: &'a RuntimeVariables,
}

impl<'a> CommandLog<'a> {
    pub(super) fn new(runtime: &'a RuntimeVariables) -> Self {
        Self { runtime }
    }

    pub(super) fn stderr(&self) -> io::Result<Stdio> {
        Ok(match self.open()? {
            Some(file) => Stdio::from(file),
            None => Stdio::inherit(),
        })
    }

    pub(super) fn write_diagnostic(&self, record: &[u8]) -> Result<(), DiagnosticWriteError> {
        match self.open().map_err(DiagnosticWriteError::Open)? {
            Some(mut file) => file.write_all(record).map_err(DiagnosticWriteError::Write),
            None => io::stderr()
                .lock()
                .write_all(record)
                .map_err(DiagnosticWriteError::Write),
        }
    }

    pub(super) fn trap_output(&self) -> Result<(Stdio, Stdio), TrapOutputError> {
        let Some(file) = self.open().map_err(TrapOutputError::Open)? else {
            return Ok(Self::inherited_trap_output());
        };
        let stdout = file.try_clone().map_err(TrapOutputError::Duplicate)?;
        Ok((Stdio::from(stdout), Stdio::from(file)))
    }

    pub(super) fn inherited_trap_output() -> (Stdio, Stdio) {
        // TRAP combines stdout with stderr in original procmail. Duplicate the
        // inherited descriptor instead of routing stdout to normal command
        // output, where diagnostics could corrupt a protocol-facing response.
        (Stdio::from(io::stderr()), Stdio::from(io::stderr()))
    }

    fn open(&self) -> io::Result<Option<File>> {
        let settings = RuntimeSettings::new(self.runtime);
        let Some(path) = settings.logfile() else {
            return Ok(None);
        };
        let mask = settings
            .umask()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let nofollow = i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())
            .map_err(|_| io::Error::other("Linux O_NOFOLLOW does not fit custom-flags type"))?;

        // Open and validate the append target for every command so assignments
        // to LOGFILE and UMASK take effect in statement order. O_NOFOLLOW and
        // the descriptor metadata check prevent a symlink or non-regular
        // target from silently receiving command output.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600 & !mask)
            .custom_flags(nofollow)
            .open(path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "LOGFILE is not a regular file",
            ));
        }
        Ok(Some(file))
    }
}

pub(super) enum TrapOutputError {
    Open(io::Error),
    Duplicate(io::Error),
}

#[derive(Debug)]
pub(super) enum DiagnosticWriteError {
    Open(io::Error),
    Write(io::Error),
}

impl DiagnosticWriteError {
    pub(super) fn into_io_error(self) -> io::Error {
        match self {
            Self::Open(error) | Self::Write(error) => error,
        }
    }
}

#[cfg(test)]
#[path = "command_log_tests.rs"]
mod tests;
