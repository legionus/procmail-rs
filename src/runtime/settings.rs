// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fmt;
use std::time::Duration;

use super::RuntimeVariables;
use crate::delivery::local_lock::LockMethod;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSettingError {
    line: Option<usize>,
    name: &'static str,
    message: String,
}

impl RuntimeSettingError {
    fn new(line: Option<usize>, name: &'static str, message: String) -> Self {
        Self {
            line,
            name,
            message,
        }
    }

    pub fn line(&self) -> Option<usize> {
        self.line
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RuntimeSettingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(formatter, "line {line}: {}", self.message)
        } else {
            formatter.write_str(&self.message)
        }
    }
}

impl std::error::Error for RuntimeSettingError {}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeSettings<'a> {
    variables: &'a RuntimeVariables,
    line: Option<usize>,
}

impl<'a> RuntimeSettings<'a> {
    pub fn new(variables: &'a RuntimeVariables) -> Self {
        Self {
            variables,
            line: None,
        }
    }

    pub fn at_line(variables: &'a RuntimeVariables, line: usize) -> Self {
        Self {
            variables,
            line: Some(line),
        }
    }

    pub fn process_timeout(self) -> Result<Duration, RuntimeSettingError> {
        // Read on every operation rather than caching a parsed snapshot: rc
        // assignments must affect only work reached after that assignment.
        match self.variables.get("TIMEOUT") {
            Some(value) => crate::config::parse_process_timeout_seconds(value)
                .map(Duration::from_secs)
                .map_err(|message| self.error("TIMEOUT", message)),
            None => Ok(crate::external_process::DEFAULT_PROCESS_TIMEOUT),
        }
    }

    pub fn lock_timeout(self) -> Result<Duration, RuntimeSettingError> {
        match self.variables.get("LOCKTIMEOUT") {
            Some(value) => crate::config::parse_lock_timeout_seconds(value)
                .map(Duration::from_secs)
                .map_err(|message| self.error("LOCKTIMEOUT", message)),
            None => Ok(crate::delivery::local_lock::DEFAULT_LOCK_TIMEOUT),
        }
    }

    pub fn lock_method(self) -> Result<LockMethod, RuntimeSettingError> {
        match self.variables.get("LOCKMETHOD") {
            Some(value) => {
                LockMethod::parse(value).map_err(|message| self.error("LOCKMETHOD", message))
            }
            None => Ok(LockMethod::default()),
        }
    }

    pub fn umask(self) -> Result<u32, RuntimeSettingError> {
        match self.variables.get("UMASK") {
            Some(value) => {
                crate::config::parse_umask(value).map_err(|message| self.error("UMASK", message))
            }
            None => Ok(crate::config::DEFAULT_UMASK),
        }
    }

    pub fn linebuf(self) -> Result<usize, RuntimeSettingError> {
        let Some(value) = self.variables.get("LINEBUF") else {
            return Ok(crate::config::DEFAULT_LINEBUF);
        };
        let value = value.parse::<usize>().map_err(|_| {
            self.error(
                "LINEBUF",
                "LINEBUF must be an unsigned decimal integer".to_owned(),
            )
        })?;
        if !(crate::config::MIN_LINEBUF..=crate::config::MAX_LINEBUF).contains(&value) {
            return Err(self.error(
                "LINEBUF",
                format!(
                    "LINEBUF must be from {} through {} bytes",
                    crate::config::MIN_LINEBUF,
                    crate::config::MAX_LINEBUF
                ),
            ));
        }
        Ok(value)
    }

    pub fn logfile(self) -> Option<&'a str> {
        self.variables
            .get("LOGFILE")
            .filter(|value| !value.is_empty())
    }

    pub fn maildir(self) -> Option<&'a str> {
        self.variables
            .get("MAILDIR")
            .filter(|value| !value.is_empty())
    }

    fn error(self, name: &'static str, message: String) -> RuntimeSettingError {
        RuntimeSettingError::new(self.line, name, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_current_values_instead_of_taking_a_snapshot() {
        let mut variables = RuntimeVariables::default();
        assert_eq!(
            RuntimeSettings::new(&variables).process_timeout().unwrap(),
            Duration::from_secs(960)
        );

        variables.set("TIMEOUT", "7");
        assert_eq!(
            RuntimeSettings::new(&variables).process_timeout().unwrap(),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn reports_the_setting_name_and_source_line() {
        let mut variables = RuntimeVariables::default();
        variables.set("LINEBUF", "invalid");
        let error = RuntimeSettings::at_line(&variables, 17)
            .linebuf()
            .unwrap_err();

        assert_eq!(error.line(), Some(17));
        assert_eq!(error.name(), "LINEBUF");
        assert_eq!(
            error.to_string(),
            "line 17: LINEBUF must be an unsigned decimal integer"
        );
    }

    #[test]
    fn supplies_existing_defaults_and_optional_paths() {
        let variables = RuntimeVariables::default();
        let settings = RuntimeSettings::new(&variables);

        assert_eq!(settings.lock_method().unwrap(), LockMethod::Flock);
        assert_eq!(settings.lock_timeout().unwrap(), Duration::from_secs(1024));
        assert_eq!(settings.umask().unwrap(), 0o077);
        assert_eq!(settings.linebuf().unwrap(), crate::config::DEFAULT_LINEBUF);
        assert_eq!(settings.logfile(), None);
        assert_eq!(settings.maildir(), None);
    }

    #[test]
    fn validates_each_typed_setting_at_runtime() {
        let cases = [
            ("TIMEOUT", "0", "TIMEOUT"),
            ("LOCKTIMEOUT", "86401", "LOCKTIMEOUT"),
            ("LOCKMETHOD", "unknown", "LOCKMETHOD"),
            ("UMASK", "1000", "UMASK"),
            ("LINEBUF", "127", "LINEBUF"),
        ];

        for (name, value, expected_name) in cases {
            let mut variables = RuntimeVariables::default();
            variables.set(name, value);
            let settings = RuntimeSettings::new(&variables);
            let error = match name {
                "TIMEOUT" => settings.process_timeout().unwrap_err(),
                "LOCKTIMEOUT" => settings.lock_timeout().unwrap_err(),
                "LOCKMETHOD" => settings.lock_method().unwrap_err(),
                "UMASK" => settings.umask().unwrap_err(),
                "LINEBUF" => settings.linebuf().unwrap_err(),
                _ => unreachable!(),
            };
            assert_eq!(error.name(), expected_name);
        }
    }

    #[test]
    fn treats_empty_optional_paths_as_unset() {
        let mut variables = RuntimeVariables::default();
        variables.set("LOGFILE", "");
        variables.set("MAILDIR", "");

        let settings = RuntimeSettings::new(&variables);
        assert_eq!(settings.logfile(), None);
        assert_eq!(settings.maildir(), None);
    }
}
