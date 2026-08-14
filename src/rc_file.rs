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
    self, AssignmentTarget, Config, MAX_RC_CONDITIONS, MAX_RC_RECIPES, MAX_RC_REGEXES, MAX_RC_SIZE,
    MAX_RC_STATEMENTS, RcFileExpression, RcParseCounts, RecipeAction, Statement,
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
    parse_counts: RcParseCounts,
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
    resource_limit: bool,
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
                parse_counts: RcParseCounts::default(),
            },
            loaded,
        ))
    }

    pub fn load(&mut self, path: &Path, depth: usize) -> Result<LoadedRcFile, RcFileError> {
        if depth > MAX_RC_INCLUDE_DEPTH {
            return Err(RcFileError::limit(
                path,
                format!("rc nesting exceeds the hard limit of {MAX_RC_INCLUDE_DEPTH}"),
            ));
        }
        if self.files_read >= MAX_RC_FILE_COUNT {
            return Err(RcFileError::limit(
                path,
                format!("rc file count exceeds the hard limit of {MAX_RC_FILE_COUNT}"),
            ));
        }
        let remaining = MAX_RC_AGGREGATE_SIZE
            .checked_sub(self.bytes_read)
            .ok_or_else(|| RcFileError::limit(path, "aggregate rc byte count overflows"))?;
        self.files_read = self
            .files_read
            .checked_add(1)
            .ok_or_else(|| RcFileError::limit(path, "rc file count overflows"))?;

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
            .ok_or_else(|| RcFileError::limit(path, "aggregate rc byte count overflows"))?;
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
        self.account_config(loaded.path(), &config)?;
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

    pub fn account_root_config(&mut self, config: &Config) -> Result<(), RcFileError> {
        self.account_config(Path::new("<root rc>"), config)
    }

    fn account_config(&mut self, path: &Path, config: &Config) -> Result<(), RcFileError> {
        let added = config.parse_counts();

        // Calculate every total before updating the loader. A rejected file
        // still consumes its file and byte budgets, but must not leave only a
        // subset of the syntax counters changed.
        let statements = add_parse_count(
            path,
            "statement",
            self.parse_counts.statements,
            added.statements,
            MAX_RC_STATEMENTS,
        )?;
        let recipes = add_parse_count(
            path,
            "recipe",
            self.parse_counts.recipes,
            added.recipes,
            MAX_RC_RECIPES,
        )?;
        let conditions = add_parse_count(
            path,
            "condition",
            self.parse_counts.conditions,
            added.conditions,
            MAX_RC_CONDITIONS,
        )?;
        let regexes = add_parse_count(
            path,
            "regex",
            self.parse_counts.regexes,
            added.regexes,
            MAX_RC_REGEXES,
        )?;
        self.parse_counts = RcParseCounts {
            statements,
            recipes,
            conditions,
            regexes,
        };
        Ok(())
    }

    pub fn files_read(&self) -> usize {
        self.files_read
    }

    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }
}

fn add_parse_count(
    path: &Path,
    name: &str,
    current: usize,
    added: usize,
    limit: usize,
) -> Result<usize, RcFileError> {
    let total = current
        .checked_add(added)
        .ok_or_else(|| RcFileError::limit(path, format!("aggregate rc {name} count overflows")))?;
    if total > limit {
        return Err(RcFileError::limit(
            path,
            format!("aggregate rc {name} count exceeds the hard limit of {limit}"),
        ));
    }
    Ok(total)
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
            resource_limit: false,
        }
    }

    fn limit(path: &Path, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            message: message.into(),
            resource_limit: true,
        }
    }

    fn io(path: &Path, error: io::Error) -> Self {
        Self::new(path, error.to_string())
    }

    pub fn is_resource_limit(&self) -> bool {
        self.resource_limit
    }

    pub fn safe_message(&self) -> &str {
        &self.message
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
        .ok_or_else(|| RcFileError::limit(path, "rc read limit overflows"))?;
    let mut source = Vec::with_capacity(limit.min(64 * 1024));
    file.take(read_limit as u64)
        .read_to_end(&mut source)
        .map_err(|error| RcFileError::io(path, error))?;
    if source.len() > file_limit {
        return Err(RcFileError::limit(
            path,
            format!("rc file exceeds the hard limit of {file_limit} bytes"),
        ));
    }
    if source.len() > aggregate_remaining {
        return Err(RcFileError::limit(
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
    fn rejects_non_regular_and_broadly_writable_runtime_files() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        let child = directory.path("child.rc");
        let link = directory.path("link.rc");
        let subdirectory = directory.path("directory.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        fs::write(&child, "CHILD=yes\n").unwrap();
        symlink(&child, &link).unwrap();
        fs::create_dir(&subdirectory).unwrap();

        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
        assert!(loader.load(&link, 1).is_err());
        let error = loader.load(&subdirectory, 1).unwrap_err();
        assert!(error.safe_message().contains("not a regular file"));

        fs::set_permissions(&child, fs::Permissions::from_mode(0o620)).unwrap();
        let error = loader.load(&child, 1).unwrap_err();
        assert!(error.to_string().contains("writable by group"));

        fs::set_permissions(&child, fs::Permissions::from_mode(0o602)).unwrap();
        let error = loader.load(&child, 1).unwrap_err();
        assert!(error.to_string().contains("other users"));
    }

    #[test]
    fn rejects_a_runtime_file_owned_by_a_different_uid() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        let child = directory.path("child.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        fs::write(&child, "CHILD=yes\n").unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
        loader.trusted_uid ^= 1;

        let error = loader.load(&child, 1).unwrap_err();

        assert!(error.safe_message().contains("differs from trusted owner"));
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
        assert!(error.is_resource_limit());
        assert_eq!(loader.files_read(), 1);
    }

    #[test]
    fn distinguishes_recoverable_open_errors_from_resource_limits() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

        let error = loader.load(&directory.path("missing.rc"), 1).unwrap_err();

        assert!(!error.is_resource_limit());
        assert!(!error.safe_message().is_empty());
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
    fn failed_open_attempts_reach_the_file_count_limit() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        fs::write(&root, "ROOT=yes\n").unwrap();
        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

        for attempt in 1..MAX_RC_FILE_COUNT {
            let error = loader
                .load(&directory.path(&format!("missing-{attempt}.rc")), 1)
                .unwrap_err();
            assert!(!error.is_resource_limit());
        }
        let error = loader
            .load(&directory.path("one-too-many.rc"), 1)
            .unwrap_err();

        assert!(error.is_resource_limit());
        assert!(error.safe_message().contains("file count exceeds"));
    }

    #[test]
    fn repeated_files_reach_the_aggregate_byte_limit() {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        let child = directory.path("child.rc");
        fs::write(&root, "#\n").unwrap();
        fs::write(&child, vec![b'#'; MAX_RC_SIZE]).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

        for _ in 0..3 {
            loader.load(&child, 1).unwrap();
        }
        let error = loader.load(&child, 1).unwrap_err();

        assert!(error.is_resource_limit());
        assert!(error.safe_message().contains("aggregate rc size exceeds"));
    }

    #[test]
    fn parsed_runtime_files_share_root_syntax_budgets() {
        let mut condition_source = String::new();
        for _ in 0..(MAX_RC_CONDITIONS / config::MAX_CONDITIONS_PER_RECIPE) {
            condition_source.push_str(":0 c\n");
            condition_source.push_str(&"* < 1\n".repeat(config::MAX_CONDITIONS_PER_RECIPE));
            condition_source.push_str("maildir:x\n");
        }
        let cases = [
            (
                "statement",
                "A=\n".repeat(MAX_RC_STATEMENTS),
                "B=\n".to_owned(),
            ),
            (
                "recipe",
                ":0 c\nmaildir:x\n".repeat(MAX_RC_RECIPES),
                ":0\nmaildir:x\n".to_owned(),
            ),
            (
                "condition",
                condition_source,
                ":0\n* < 1\nmaildir:x\n".to_owned(),
            ),
            (
                "regex",
                format!(":0\n{}maildir:x\n", "* pattern\n".repeat(MAX_RC_REGEXES)),
                ":0\n* pattern\nmaildir:x\n".to_owned(),
            ),
        ];

        for (name, root_source, child_source) in cases {
            let directory = TestDirectory::new();
            let root = directory.path("root.rc");
            let child = directory.path("child.rc");
            fs::write(&root, &root_source).unwrap();
            fs::write(&child, child_source).unwrap();
            fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
            let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
            let root_config = config::parse(&root_source).unwrap();
            loader.account_root_config(&root_config).unwrap();
            let expression_config =
                config::parse(&format!("INCLUDERC={}\n", child.display())).unwrap();
            let Statement::Include(expression) = &expression_config.statements[0] else {
                panic!("expected include");
            };

            let error = loader
                .load_config(expression, &RuntimeVariables::default(), 1)
                .unwrap_err();

            assert!(error.is_resource_limit(), "{name}: {error}");
            assert!(
                error
                    .safe_message()
                    .contains(&format!("aggregate rc {name} count exceeds")),
                "{name}: {error}"
            );
        }
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
