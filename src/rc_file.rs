// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{CWD, FileType, Mode, OFlags, fstat, openat};

use crate::config::{
    self, AssignmentTarget, Config, MAX_RC_SIZE, RcFileExpression, RecipeAction, Statement,
};
use crate::runtime::RuntimeVariables;

pub const MAX_RC_FILE_COUNT: usize = 32;
pub const MAX_RC_AGGREGATE_SIZE: usize = 4 * 1024 * 1024;
pub const MAX_RC_INCLUDE_DEPTH: usize = 16;
pub const MAX_RC_TRANSITIONS: usize = 256;

#[derive(Debug)]
pub struct RcFileLoader {
    trusted_uid: u32,
    files_read: usize,
    bytes_read: usize,
}

#[derive(Debug)]
pub struct LoadedRcFile {
    path: PathBuf,
    source: String,
}

#[derive(Debug)]
pub struct LoadedRcConfig {
    path: PathBuf,
    config: Config,
}

#[derive(Debug)]
pub struct RcFileError {
    path: PathBuf,
    message: String,
}

impl RcFileLoader {
    pub fn for_root(path: &Path) -> Result<(Self, LoadedRcFile), RcFileError> {
        let mut file = File::open(path).map_err(|error| RcFileError::io(path, error))?;
        let metadata = file
            .metadata()
            .map_err(|error| RcFileError::io(path, error))?;
        if !metadata.file_type().is_file() {
            return Err(RcFileError::new(path, "rc path is not a regular file"));
        }
        let source = read_bounded(path, &mut file, MAX_RC_SIZE, MAX_RC_AGGREGATE_SIZE)?;
        let source_len = source.len();
        let source = String::from_utf8(source)
            .map_err(|_| RcFileError::new(path, "rc file is not valid UTF-8"))?;
        let loaded = LoadedRcFile {
            path: path.to_owned(),
            source,
        };
        Ok((
            Self {
                trusted_uid: metadata.uid(),
                files_read: 1,
                bytes_read: source_len,
            },
            loaded,
        ))
    }

    pub fn load(&mut self, path: &Path, depth: usize) -> Result<LoadedRcFile, RcFileError> {
        if depth > MAX_RC_INCLUDE_DEPTH {
            return Err(RcFileError::new(
                path,
                format!("rc nesting exceeds the hard limit of {MAX_RC_INCLUDE_DEPTH}"),
            ));
        }
        if self.files_read >= MAX_RC_FILE_COUNT {
            return Err(RcFileError::new(
                path,
                format!("rc file count exceeds the hard limit of {MAX_RC_FILE_COUNT}"),
            ));
        }
        let remaining = MAX_RC_AGGREGATE_SIZE
            .checked_sub(self.bytes_read)
            .ok_or_else(|| RcFileError::new(path, "aggregate rc byte count overflows"))?;
        self.files_read = self
            .files_read
            .checked_add(1)
            .ok_or_else(|| RcFileError::new(path, "rc file count overflows"))?;

        // Open the selected file and validate the descriptor itself. A
        // separate metadata lookup could approve one file and then let a
        // concurrent rename substitute a symlink or an untrusted file before
        // the actual read.
        let fd = openat(
            CWD,
            path.as_os_str().as_bytes(),
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| RcFileError::io(path, io_error(error)))?;
        let stat = fstat(&fd).map_err(|error| RcFileError::io(path, io_error(error)))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(RcFileError::new(path, "rc path is not a regular file"));
        }
        if stat.st_uid != self.trusted_uid {
            return Err(RcFileError::new(
                path,
                format!(
                    "rc file owner {} differs from trusted owner {}",
                    stat.st_uid, self.trusted_uid
                ),
            ));
        }
        if stat.st_mode & 0o022 != 0 {
            return Err(RcFileError::new(
                path,
                "rc file is writable by group or other users",
            ));
        }

        let source = read_bounded(path, &mut FdReader(&fd), MAX_RC_SIZE, remaining)?;
        self.bytes_read = self
            .bytes_read
            .checked_add(source.len())
            .ok_or_else(|| RcFileError::new(path, "aggregate rc byte count overflows"))?;
        let source = String::from_utf8(source)
            .map_err(|_| RcFileError::new(path, "rc file is not valid UTF-8"))?;
        Ok(LoadedRcFile {
            path: path.to_owned(),
            source,
        })
    }

    pub fn load_config(
        &mut self,
        expression: &RcFileExpression,
        runtime: &RuntimeVariables,
        depth: usize,
    ) -> Result<Option<LoadedRcConfig>, RcFileError> {
        let path = expression
            .resolve_with(|name| runtime.get(name).map(str::to_owned))
            .map_err(|error| RcFileError::new(Path::new("<runtime rc path>"), error.to_string()))?;
        if path.is_empty() {
            return Ok(None);
        }
        let loaded = self.load(Path::new(&path), depth)?;
        let config = config::parse(loaded.source()).map_err(|error| {
            RcFileError::new(loaded.path(), format!("invalid rc syntax: {error}"))
        })?;
        let config = config
            .expand_with_runtime_values(runtime.values())
            .map_err(|error| {
                RcFileError::new(loaded.path(), format!("cannot expand rc file: {error}"))
            })?;
        validate_runtime_settings(&config.statements).map_err(|(line, name)| {
            RcFileError::new(
                loaded.path(),
                format!("line {line}: {name} must be set before message processing begins"),
            )
        })?;
        Ok(Some(LoadedRcConfig {
            path: loaded.path,
            config,
        }))
    }

    pub fn files_read(&self) -> usize {
        self.files_read
    }

    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }
}

fn validate_runtime_settings(statements: &[Statement]) -> Result<(), (usize, &str)> {
    for statement in statements {
        match statement {
            Statement::Assignment(assignment)
                if !matches!(
                    assignment.target,
                    AssignmentTarget::User | AssignmentTarget::Maildir
                ) =>
            {
                return Err((assignment.line, assignment.name.as_str()));
            }
            Statement::Recipe(recipe) => {
                if let RecipeAction::Block(children) = &recipe.action {
                    validate_runtime_settings(children)?;
                }
            }
            Statement::Assignment(_) | Statement::Include(_) | Statement::Switch(_) => {}
        }
    }
    Ok(())
}

impl LoadedRcFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

impl LoadedRcConfig {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn into_config(self) -> Config {
        self.config
    }
}

impl RcFileError {
    fn new(path: &Path, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            message: message.into(),
        }
    }

    fn io(path: &Path, error: io::Error) -> Self {
        Self::new(path, error.to_string())
    }
}

impl fmt::Display for RcFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot read {}: {}",
            self.path.display(),
            self.message
        )
    }
}

impl std::error::Error for RcFileError {}

fn read_bounded(
    path: &Path,
    file: &mut impl Read,
    file_limit: usize,
    aggregate_remaining: usize,
) -> Result<Vec<u8>, RcFileError> {
    let limit = file_limit.min(aggregate_remaining);
    let read_limit = limit
        .checked_add(1)
        .ok_or_else(|| RcFileError::new(path, "rc read limit overflows"))?;
    let mut source = Vec::with_capacity(limit.min(64 * 1024));
    file.take(read_limit as u64)
        .read_to_end(&mut source)
        .map_err(|error| RcFileError::io(path, error))?;
    if source.len() > file_limit {
        return Err(RcFileError::new(
            path,
            format!("rc file exceeds the hard limit of {file_limit} bytes"),
        ));
    }
    if source.len() > aggregate_remaining {
        return Err(RcFileError::new(
            path,
            format!("aggregate rc size exceeds the hard limit of {MAX_RC_AGGREGATE_SIZE} bytes"),
        ));
    }
    Ok(source)
}

struct FdReader<'a>(&'a OwnedFd);

impl Read for FdReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        rustix::io::read(self.0, buffer).map_err(io_error)
    }
}

fn io_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "procmail-rs-rc-loader-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_trusted_regular_files_and_accounts_bytes() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        let child = directory.path("child.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        fs::write(&child, "CHILD=yes\n").unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();

        let (mut loader, loaded_root) = RcFileLoader::for_root(&root).unwrap();
        let loaded_child = loader.load(&child, 1).unwrap();

        assert_eq!(loaded_root.source(), "ROOT=yes\n");
        assert_eq!(loaded_child.source(), "CHILD=yes\n");
        assert_eq!(loader.files_read(), 2);
        assert_eq!(loader.bytes_read(), 19);
    }

    #[test]
    fn rejects_symlink_and_group_writable_include() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        let child = directory.path("child.rc");
        let link = directory.path("link.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        fs::write(&child, "CHILD=yes\n").unwrap();
        symlink(&child, &link).unwrap();

        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
        assert!(loader.load(&link, 1).is_err());

        fs::set_permissions(&child, fs::Permissions::from_mode(0o620)).unwrap();
        let error = loader.load(&child, 1).unwrap_err();
        assert!(error.to_string().contains("writable by group"));
    }

    #[test]
    fn enforces_depth_before_opening_the_file() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

        let error = loader
            .load(&directory.path("missing.rc"), MAX_RC_INCLUDE_DEPTH + 1)
            .unwrap_err();

        assert!(error.to_string().contains("nesting exceeds"));
        assert_eq!(loader.files_read(), 1);
    }

    #[test]
    fn failed_utf8_validation_still_consumes_file_and_byte_budget() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        let child = directory.path("binary.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        fs::write(&child, [0xff]).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

        let error = loader.load(&child, 1).unwrap_err();

        assert!(error.to_string().contains("not valid UTF-8"));
        assert_eq!(loader.files_read(), 2);
        assert_eq!(loader.bytes_read(), 10);
    }

    #[test]
    fn resolves_runtime_include_path_and_expands_child_with_current_values() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        let child = directory.path("child.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        fs::write(&child, "CHILD=$PARENT\n").unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
        let parsed = config::parse("INCLUDERC=$SELECTED\n").unwrap();
        let config::Statement::Include(expression) = &parsed.statements[0] else {
            panic!("expected include");
        };
        let mut runtime = RuntimeVariables::default();
        runtime.set("MAILDIR", directory.0.to_string_lossy());
        runtime.set("SELECTED", "child.rc");
        runtime.set("PARENT", "visible");

        let loaded = loader
            .load_config(expression, &runtime, 1)
            .unwrap()
            .unwrap();
        let config::Statement::Assignment(assignment) = &loaded.config().statements[0] else {
            panic!("expected assignment");
        };

        assert_eq!(loaded.path(), child);
        assert_eq!(assignment.value, "visible");
    }

    #[test]
    fn rejects_runtime_include_that_changes_pre_input_settings() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        let child = directory.path("child.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        fs::write(&child, "LIMIT_MSG_BODY=1k\n").unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
        let parsed = config::parse(&format!("INCLUDERC={}\n", child.display())).unwrap();
        let config::Statement::Include(expression) = &parsed.statements[0] else {
            panic!("expected include");
        };

        let error = loader
            .load_config(expression, &RuntimeVariables::default(), 1)
            .unwrap_err();

        assert!(error.to_string().contains("must be set before message"));
    }
}
