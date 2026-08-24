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
    self, AssignmentTarget, Config, MAX_RC_SIZE, RcFileExpression, RcLimitVariable, RcLimits,
    RcParseState, RecipeAction, Statement,
};
use crate::runtime::RuntimeVariables;

#[cfg(test)]
use crate::config::{MAX_RC_CONDITIONS, MAX_RC_RECIPES, MAX_RC_REGEXES, MAX_RC_STATEMENTS};

pub const MAX_RC_FILE_COUNT: usize = 32;
pub const MAX_RC_AGGREGATE_SIZE: usize = 4 * 1024 * 1024;
pub const MAX_RC_INCLUDE_DEPTH: usize = 16;
pub const MAX_RC_TRANSITIONS: usize = 256;
pub const MAX_RC_CHECK_WARNINGS: usize = 128;

#[derive(Debug)]
pub struct RcFileLoader {
    trusted_uid: u32,
    files_read: usize,
    bytes_read: usize,
    parse_state: RcParseState,
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
                parse_state: RcParseState::default(),
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
        self.activate_runtime_limits(runtime, loaded.path())?;
        let mut next_parse_state = self.parse_state;
        let config = config::parse_with_state(loaded.source(), &mut next_parse_state)
            .map_err(|error| parse_file_error(loaded.path(), error))?;
        self.parse_state = next_parse_state;
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

    fn load_check_config(
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
        self.activate_runtime_limits(runtime, loaded.path())?;
        let mut next_parse_state = self.parse_state;
        let config = config::parse_with_state(loaded.source(), &mut next_parse_state)
            .map_err(|error| parse_file_error(loaded.path(), error))?;
        self.parse_state = next_parse_state;
        let config = config
            .prepare_for_check(runtime.values())
            .map_err(|error| {
                RcFileError::new(loaded.path(), format!("cannot validate rc file: {error}"))
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
        self.parse_state.counts = config.parse_counts();
        self.parse_state.limits = RcLimits::default();
        Ok(())
    }

    fn activate_runtime_limits(
        &mut self,
        runtime: &RuntimeVariables,
        path: &Path,
    ) -> Result<(), RcFileError> {
        const LIMITS: [(&str, RcLimitVariable); 7] = [
            ("LIMIT_MAX_ASSIGNMENTS", RcLimitVariable::Assignments),
            ("LIMIT_RC_STATEMENTS", RcLimitVariable::Statements),
            ("LIMIT_RC_RECIPES", RcLimitVariable::Recipes),
            ("LIMIT_RC_CONDITIONS", RcLimitVariable::Conditions),
            ("LIMIT_RC_REGEXES", RcLimitVariable::Regexes),
            (
                "LIMIT_RECIPE_CONDITIONS",
                RcLimitVariable::ConditionsPerRecipe,
            ),
            ("LIMIT_RECIPE_NESTING", RcLimitVariable::NestingDepth),
        ];
        // Rebuild the active limits from assignments that have really run.
        // Keeping the parser's final values would let an assignment after an
        // include change how that earlier include is parsed.
        let mut limits = RcLimits::default();
        for (name, kind) in LIMITS {
            let Some(value) = runtime.get(name) else {
                continue;
            };
            let value = value.parse::<usize>().map_err(|_| {
                RcFileError::new(
                    path,
                    format!("runtime {name} is not an unsigned decimal integer"),
                )
            })?;
            if let Err(hard_limit) = limits.set(kind, value) {
                return Err(RcFileError::limit(
                    path,
                    format!("runtime {name} exceeds the hard limit of {hard_limit}"),
                ));
            }
        }
        self.parse_state.limits = limits;
        Ok(())
    }

    pub fn check_resolvable_files(&mut self, config: &Config) -> Result<Vec<String>, RcFileError> {
        let mut runtime = RuntimeVariables::default();
        for (name, value, _) in config.initial_variables() {
            runtime.set(name.clone(), value.clone());
        }
        let mut warnings = RcCheckWarnings::default();
        self.check_statements(&config.statements, &mut runtime, 0, &mut warnings)?;
        Ok(warnings.finish())
    }

    fn check_statements(
        &mut self,
        statements: &[Statement],
        runtime: &mut RuntimeVariables,
        depth: usize,
        warnings: &mut RcCheckWarnings,
    ) -> Result<(), RcFileError> {
        for statement in statements {
            match statement {
                Statement::Assignment(assignment) => {
                    match assignment.resolve_with(|name| runtime.get(name).map(str::to_owned)) {
                        Ok(value) => runtime.set(assignment.name.clone(), value),
                        Err(_) => runtime.remove(&assignment.name),
                    }
                }
                Statement::Include(expression) | Statement::Switch(expression) => {
                    let statement_name = if matches!(statement, Statement::Include(_)) {
                        "INCLUDERC"
                    } else {
                        "SWITCHRC"
                    };
                    if expression
                        .resolve_with(|name| runtime.get(name).map(str::to_owned))
                        .is_err()
                    {
                        warnings.dynamic_path(depth, expression.line, statement_name)?;
                        continue;
                    }
                    let child_depth = depth.checked_add(1).ok_or_else(|| {
                        RcFileError::limit(Path::new("<check>"), "rc check nesting depth overflows")
                    })?;
                    let Some(loaded) = self.load_check_config(expression, runtime, child_depth)?
                    else {
                        continue;
                    };
                    let mut warning_error = None;
                    loaded
                        .config()
                        .for_each_compatibility_warning(|line, flag| {
                            if warning_error.is_none() {
                                warning_error =
                                    warnings.compatibility_flag(loaded.path(), line, flag).err();
                            }
                        });
                    if let Some(error) = warning_error {
                        return Err(error);
                    }

                    // INCLUDERC assignments affect statements that follow in
                    // the same selected path. SWITCHRC never reaches those
                    // statements, so validate its replacement with a private
                    // value table and do not leak its assignments forward.
                    if matches!(statement, Statement::Include(_)) {
                        self.check_statements(
                            &loaded.config().statements,
                            runtime,
                            child_depth,
                            warnings,
                        )?;
                    } else {
                        let mut switched_runtime = runtime.clone();
                        self.check_statements(
                            &loaded.config().statements,
                            &mut switched_runtime,
                            child_depth,
                            warnings,
                        )?;
                        break;
                    }
                }
                Statement::Recipe(recipe) => {
                    if let RecipeAction::Block(children) = &recipe.action {
                        // A block may not be selected for a particular
                        // message. Validate its statically known files with a
                        // cloned table, but keep conditional assignments from
                        // changing the sibling path examined afterwards.
                        let mut child_runtime = runtime.clone();
                        self.check_statements(children, &mut child_runtime, depth, warnings)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn files_read(&self) -> usize {
        self.files_read
    }

    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }
}

#[derive(Default)]
struct RcCheckWarnings {
    messages: Vec<String>,
    omitted: usize,
}

fn parse_file_error(path: &Path, error: config::ParseError) -> RcFileError {
    let message = format!("invalid rc syntax: {error}");
    if error.is_resource_limit() {
        RcFileError::limit(path, message)
    } else {
        RcFileError::new(path, message)
    }
}

impl RcCheckWarnings {
    fn push(&mut self, message: String) -> Result<(), RcFileError> {
        if self.messages.len() < MAX_RC_CHECK_WARNINGS {
            self.messages.push(message);
        } else {
            self.omitted = self.omitted.checked_add(1).ok_or_else(|| {
                RcFileError::limit(Path::new("<check>"), "rc check warning count overflows")
            })?;
        }
        Ok(())
    }

    fn dynamic_path(
        &mut self,
        depth: usize,
        line: usize,
        statement: &str,
    ) -> Result<(), RcFileError> {
        self.push(format!(
            "rc depth {depth}, line {line}: dynamic {statement} path was not validated"
        ))
    }

    fn compatibility_flag(
        &mut self,
        path: &Path,
        line: usize,
        flag: char,
    ) -> Result<(), RcFileError> {
        self.push(format!(
            "{}:{line}: recipe flag '{flag}' has no effect on a block",
            path.display()
        ))
    }

    fn finish(mut self) -> Vec<String> {
        if self.omitted != 0 {
            self.messages.push(format!(
                "{} additional rc warnings were omitted",
                self.omitted
            ));
        }
        self.messages
    }
}

fn validate_runtime_settings(statements: &[Statement]) -> Result<(), (usize, &str)> {
    for statement in statements {
        match statement {
            Statement::Assignment(assignment)
                if !matches!(
                    assignment.target,
                    AssignmentTarget::User
                        | AssignmentTarget::Maildir
                        | AssignmentTarget::Shell
                        | AssignmentTarget::ShellFlags
                        | AssignmentTarget::Path
                        | AssignmentTarget::LockMethod
                        | AssignmentTarget::LockFile
                        | AssignmentTarget::LockTimeout
                        | AssignmentTarget::LineBuf
                        | AssignmentTarget::ProcessTimeout
                        | AssignmentTarget::Umask
                        | AssignmentTarget::Trap
                        | AssignmentTarget::RcLimit(_)
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
mod tests;
