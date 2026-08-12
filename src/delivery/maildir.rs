// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat};

use super::{PendingSink, PublishedDelivery};

const MAX_NAME_ATTEMPTS: u64 = 128;
static NEXT_NAME: AtomicU64 = AtomicU64::new(0);

pub struct MaildirSink {
    file: OwnedFd,
    tmp_dir: OwnedFd,
    new_dir: OwnedFd,
    name: String,
    maildir: PathBuf,
    pending: bool,
}

impl MaildirSink {
    pub fn create(path: &Path) -> io::Result<Self> {
        let maildir = open_directory_path(path)?;
        let tmp_dir = open_directory_at(&maildir, OsStr::new("tmp"))?;
        let new_dir = open_directory_at(&maildir, OsStr::new("new"))?;

        for attempt in 0..MAX_NAME_ATTEMPTS {
            let name = unique_name(attempt);
            match openat(
                &tmp_dir,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(fd) => {
                    return Ok(Self {
                        file: fd,
                        tmp_dir,
                        new_dir,
                        name,
                        maildir: path.to_owned(),
                        pending: true,
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

    fn cleanup(&mut self) -> io::Result<()> {
        if !self.pending {
            return Ok(());
        }
        match unlinkat(&self.tmp_dir, self.name.as_str(), AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {
                self.pending = false;
                Ok(())
            }
            Err(error) => Err(io_error(error)),
        }
    }
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
    fn commit(mut self: Box<Self>) -> io::Result<PublishedDelivery> {
        match renameat_with(
            &self.tmp_dir,
            self.name.as_str(),
            &self.new_dir,
            self.name.as_str(),
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {
                self.pending = false;
                Ok(PublishedDelivery::new(
                    self.maildir.join("new").join(&self.name),
                ))
            }
            Err(error) => {
                let rename_error = io_error(error);
                match self.cleanup() {
                    Ok(()) => Err(rename_error),
                    Err(cleanup_error) => Err(io::Error::new(
                        rename_error.kind(),
                        format!(
                            "{rename_error}; cannot remove pending Maildir file: {cleanup_error}"
                        ),
                    )),
                }
            }
        }
    }

    fn abort(mut self: Box<Self>) -> io::Result<()> {
        self.cleanup()
    }
}

impl Drop for MaildirSink {
    fn drop(&mut self) {
        let _ = self.cleanup();
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

fn unique_name(attempt: u64) -> String {
    let sequence = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!(
        "procmail-rs.{}.{}.{}.{}",
        process::id(),
        timestamp,
        sequence,
        attempt
    )
}

fn io_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::{Path, PathBuf};

    use super::*;

    struct TestMaildir {
        path: PathBuf,
    }

    impl TestMaildir {
        fn create() -> Self {
            let base = std::env::temp_dir();
            for attempt in 0..128u64 {
                let path = base.join(unique_name(attempt));
                match fs::create_dir(&path) {
                    Ok(()) => {
                        fs::create_dir(path.join("tmp")).unwrap();
                        fs::create_dir(path.join("new")).unwrap();
                        fs::create_dir(path.join("cur")).unwrap();
                        return Self { path };
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("cannot create test Maildir: {error}"),
                }
            }
            panic!("cannot allocate test Maildir name");
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestMaildir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).unwrap();
        }
    }

    #[test]
    fn keeps_message_private_until_commit() {
        let maildir = TestMaildir::create();
        let mut sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
        let name = sink.name.clone();
        sink.write_all(b"Subject: test\n\nbody").unwrap();

        assert!(maildir.path().join("tmp").join(&name).is_file());
        assert!(!maildir.path().join("new").join(&name).exists());

        let published = PendingSink::commit(sink).unwrap();
        assert_eq!(
            published.last_folder(),
            maildir.path().join("new").join(&name)
        );
        assert!(!maildir.path().join("tmp").join(&name).exists());
        assert_eq!(
            fs::read(maildir.path().join("new").join(name)).unwrap(),
            b"Subject: test\n\nbody"
        );
    }

    #[test]
    fn abort_removes_only_the_pending_file() {
        let maildir = TestMaildir::create();
        let mut sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
        let name = sink.name.clone();
        sink.write_all(b"partial").unwrap();

        PendingSink::abort(sink).unwrap();
        assert!(!maildir.path().join("tmp").join(name).exists());
        assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 0);
    }

    #[test]
    fn creates_private_file_mode() {
        let maildir = TestMaildir::create();
        let sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
        let metadata = fs::metadata(maildir.path().join("tmp").join(&sink.name)).unwrap();

        assert_eq!(metadata.mode() & 0o777, 0o600);
        PendingSink::abort(sink).unwrap();
    }

    #[test]
    fn rejects_symlinked_maildir_component() {
        let maildir = TestMaildir::create();
        let link = maildir.path().with_extension("link");
        symlink(maildir.path(), &link).unwrap();

        let error = MaildirSink::create(&link).err().unwrap();
        let code = error.raw_os_error();
        assert!(
            code == Some(rustix::io::Errno::LOOP.raw_os_error())
                || code == Some(rustix::io::Errno::NOTDIR.raw_os_error())
        );
        fs::remove_file(link).unwrap();
    }

    #[test]
    fn rejects_symlinked_tmp_directory() {
        let maildir = TestMaildir::create();
        fs::remove_dir(maildir.path().join("tmp")).unwrap();
        symlink(maildir.path().join("new"), maildir.path().join("tmp")).unwrap();

        assert!(MaildirSink::create(maildir.path()).is_err());
        assert_eq!(fs::read_dir(maildir.path().join("new")).unwrap().count(), 0);
    }

    #[test]
    fn commit_never_replaces_an_existing_new_file() {
        let maildir = TestMaildir::create();
        let mut sink = Box::new(MaildirSink::create(maildir.path()).unwrap());
        let name = sink.name.clone();
        sink.write_all(b"new message").unwrap();
        fs::write(maildir.path().join("new").join(&name), b"existing").unwrap();

        let error = PendingSink::commit(sink).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(maildir.path().join("new").join(&name)).unwrap(),
            b"existing"
        );
        assert!(!maildir.path().join("tmp").join(name).exists());
    }

    #[test]
    fn concurrent_deliveries_publish_unique_complete_messages() {
        const DELIVERIES: usize = 32;

        let maildir = TestMaildir::create();
        let mut threads = Vec::with_capacity(DELIVERIES);
        for index in 0..DELIVERIES {
            let path = maildir.path().to_owned();
            threads.push(std::thread::spawn(move || {
                let message = format!("Subject: {index}\n\nbody {index}").into_bytes();
                let mut sink = Box::new(MaildirSink::create(&path).unwrap());
                sink.write_all(&message).unwrap();
                PendingSink::commit(sink).unwrap();
                message
            }));
        }

        let mut expected: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        let mut delivered: Vec<_> = fs::read_dir(maildir.path().join("new"))
            .unwrap()
            .map(|entry| fs::read(entry.unwrap().path()).unwrap())
            .collect();
        expected.sort();
        delivered.sort();
        assert_eq!(delivered, expected);
        assert_eq!(fs::read_dir(maildir.path().join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn rejects_parent_components() {
        let error = MaildirSink::create(Path::new("mail/../dir")).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
