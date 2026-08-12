// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::io;
use std::ptr::{self, NonNull};
use std::slice;

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, fstat, unlinkat};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};

pub(crate) struct MappedFile {
    _file: OwnedFd,
    address: Option<NonNull<u8>>,
    len: usize,
}

impl MappedFile {
    pub(crate) fn unlink_and_map(
        file: OwnedFd,
        directory: &OwnedFd,
        name: &str,
        maximum_len: usize,
    ) -> io::Result<Self> {
        let metadata = fstat(&file).map_err(io_error)?;
        let len = usize::try_from(metadata.st_size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "staging file size does not fit in usize",
            )
        })?;
        if len > maximum_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("staging file has {len} bytes, limit is {maximum_len}"),
            ));
        }

        // Removing the only pathname before mapping prevents ordinary code
        // from reopening and changing the staging file while regex holds a
        // shared byte slice. The owned fd keeps the data alive until munmap.
        unlinkat(directory, name, AtFlags::empty()).map_err(io_error)?;
        if len == 0 {
            return Ok(Self {
                _file: file,
                address: None,
                len,
            });
        }

        // The requested address is null, so the kernel chooses a valid page
        // aligned range. MAP_PRIVATE and read-only protection prevent this
        // process from modifying the bytes exposed through `as_bytes`.
        let address = unsafe {
            mmap(
                ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::PRIVATE,
                &file,
                0,
            )
        }
        .map_err(io_error)?;
        let address = NonNull::new(address.cast())
            .ok_or_else(|| io::Error::other("mmap returned a null address for a non-empty file"))?;

        Ok(Self {
            _file: file,
            address: Some(address),
            len,
        })
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        let Some(address) = self.address else {
            return &[];
        };

        // The mapping covers exactly `len` initialized file bytes, remains
        // read-only, and cannot be unmapped while this borrow of `self` lives.
        unsafe { slice::from_raw_parts(address.as_ptr(), self.len) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        let Some(address) = self.address else {
            return;
        };

        // Drop has exclusive access to the object, so no slice returned from
        // `as_bytes` can still be used when the mapping is released.
        let _ = unsafe { munmap(address.as_ptr().cast(), self.len) };
    }
}

fn io_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rustix::fs::{CWD, Mode, OFlags, openat};

    use super::*;

    static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

    fn mapped(bytes: &[u8], maximum_len: usize) -> io::Result<(MappedFile, PathBuf)> {
        let name = format!(
            "procmail-rs-map-test-{}-{}",
            process::id(),
            NEXT_NAME.fetch_add(1, Ordering::Relaxed)
        );
        let directory = openat(
            CWD,
            "/tmp",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(io_error)?;
        let fd = openat(
            &directory,
            name.as_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .map_err(io_error)?;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let written = rustix::io::write(&fd, remaining).map_err(io_error)?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "cannot populate mapping test file",
                ));
            }
            remaining = &remaining[written..];
        }
        let path = PathBuf::from("/tmp").join(&name);

        match MappedFile::unlink_and_map(fd, &directory, &name, maximum_len) {
            Ok(mapping) => Ok((mapping, path)),
            Err(error) => {
                let _ = unlinkat(&directory, name.as_str(), AtFlags::empty());
                Err(error)
            }
        }
    }

    #[test]
    fn maps_bytes_after_removing_the_staging_name() {
        let (mapping, path) = mapped(b"binary:\xff\x00body", 64).unwrap();

        assert!(!path.exists());
        assert_eq!(mapping.as_bytes(), b"binary:\xff\x00body");
    }

    #[test]
    fn represents_an_empty_file_without_mapping_pages() {
        let (mapping, path) = mapped(b"", 0).unwrap();

        assert!(!path.exists());
        assert!(mapping.as_bytes().is_empty());
    }

    #[test]
    fn rejects_file_above_mapping_limit_without_removing_it() {
        let error = mapped(b"1234", 3).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
