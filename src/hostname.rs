// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;

pub const MAX_HOSTNAME_LEN: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostnameError {
    message: String,
}

pub fn current_hostname() -> Result<String, HostnameError> {
    let system = rustix::system::uname();
    hostname_from_bytes(system.nodename().to_bytes())
}

fn hostname_from_bytes(name: &[u8]) -> Result<String, HostnameError> {
    if name.is_empty() {
        return Err(error("current hostname is empty"));
    }
    if name.len() > MAX_HOSTNAME_LEN {
        return Err(error(format!(
            "current hostname exceeds the hard limit of {MAX_HOSTNAME_LEN} bytes"
        )));
    }
    let name =
        std::str::from_utf8(name).map_err(|_| error("current hostname is not valid UTF-8"))?;
    Ok(name.to_owned())
}

fn error(message: impl Into<String>) -> HostnameError {
    HostnameError {
        message: message.into(),
    }
}

impl fmt::Display for HostnameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HostnameError {}

#[cfg(test)]
mod tests;
