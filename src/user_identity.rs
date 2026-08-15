// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::mem::MaybeUninit;

const INITIAL_PASSWD_BUFFER_SIZE: usize = 1024;
const MAX_PASSWD_BUFFER_SIZE: usize = 64 * 1024;
const MAX_IDENTITY_FIELD_SIZE: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserIdentity {
    logname: String,
    home: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserIdentityError {
    message: String,
}

impl UserIdentity {
    pub fn current() -> Result<Self, UserIdentityError> {
        let uid = unsafe { libc::getuid() };
        let mut buffer_size = INITIAL_PASSWD_BUFFER_SIZE;

        loop {
            let mut passwd = MaybeUninit::<libc::passwd>::uninit();
            let mut result = std::ptr::null_mut();
            let mut buffer = vec![0_u8; buffer_size];

            // getpwuid_r writes the passwd record and all referenced strings
            // into caller-owned storage. Keep that storage alive until both
            // selected fields have been copied and validated below.
            let status = unsafe {
                libc::getpwuid_r(
                    uid,
                    passwd.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            };
            if status == libc::ERANGE {
                buffer_size = buffer_size
                    .checked_mul(2)
                    .ok_or_else(|| UserIdentityError::new("passwd lookup buffer size overflows"))?;
                if buffer_size > MAX_PASSWD_BUFFER_SIZE {
                    return Err(UserIdentityError::new(format!(
                        "passwd lookup requires more than {MAX_PASSWD_BUFFER_SIZE} bytes"
                    )));
                }
                continue;
            }
            if status != 0 {
                return Err(UserIdentityError::new(format!(
                    "cannot look up uid {uid}: {}",
                    std::io::Error::from_raw_os_error(status)
                )));
            }
            if result.is_null() {
                return Err(UserIdentityError::new(format!(
                    "no passwd entry exists for uid {uid}"
                )));
            }
            if result != passwd.as_mut_ptr() {
                return Err(UserIdentityError::new(
                    "passwd lookup returned an unexpected record pointer",
                ));
            }

            // A successful non-null result tells us that libc initialized the
            // record passed above. Validate that each selected pointer stays
            // inside our bounded buffer before inspecting its bytes.
            let passwd = unsafe { passwd.assume_init() };
            let logname = copy_field(&buffer, passwd.pw_name.cast(), "LOGNAME")?;
            let home = copy_field(&buffer, passwd.pw_dir.cast(), "HOME")?;
            return Ok(Self { logname, home });
        }
    }

    pub fn logname(&self) -> &str {
        &self.logname
    }

    pub fn home(&self) -> &str {
        &self.home
    }
}

fn copy_field(
    buffer: &[u8],
    pointer: *const u8,
    name: &'static str,
) -> Result<String, UserIdentityError> {
    let offset = (pointer as usize)
        .checked_sub(buffer.as_ptr() as usize)
        .filter(|offset| *offset < buffer.len())
        .ok_or_else(|| UserIdentityError::new(format!("passwd {name} pointer is out of bounds")))?;
    let bytes = &buffer[offset..];
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| UserIdentityError::new(format!("passwd {name} is not terminated")))?;
    if length == 0 {
        return Err(UserIdentityError::new(format!("passwd {name} is empty")));
    }
    if length > MAX_IDENTITY_FIELD_SIZE {
        return Err(UserIdentityError::new(format!(
            "passwd {name} exceeds the hard limit of {MAX_IDENTITY_FIELD_SIZE} bytes"
        )));
    }
    std::str::from_utf8(&bytes[..length])
        .map(str::to_owned)
        .map_err(|_| UserIdentityError::new(format!("passwd {name} is not valid UTF-8")))
}

impl UserIdentityError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for UserIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UserIdentityError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_current_process_identity() {
        let identity = UserIdentity::current().unwrap();

        assert!(!identity.logname().is_empty());
        assert!(!identity.home().is_empty());
    }

    #[test]
    fn bounds_fields_before_searching_outside_the_buffer() {
        let buffer = b"user\0/home/user\0";
        let unterminated = b"unterminated";

        assert_eq!(
            copy_field(buffer, buffer.as_ptr(), "LOGNAME").unwrap(),
            "user"
        );
        assert!(copy_field(buffer, std::ptr::null(), "LOGNAME").is_err());
        assert!(copy_field(unterminated, unterminated.as_ptr(), "HOME").is_err());
    }

    #[test]
    fn enforces_identity_field_size_limit() {
        let mut buffer = vec![b'x'; MAX_IDENTITY_FIELD_SIZE + 2];
        buffer[MAX_IDENTITY_FIELD_SIZE + 1] = 0;

        let error = copy_field(&buffer, buffer.as_ptr(), "HOME").unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("passwd HOME exceeds the hard limit of {MAX_IDENTITY_FIELD_SIZE} bytes")
        );
    }
}
