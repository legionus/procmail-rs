// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

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
