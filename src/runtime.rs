// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeMap;
use std::path::Path;

use crate::delivery::{CommitError, CommitReport, PublishedDelivery};
use crate::trace::{
    NoTrace, TraceEvent, TraceName, TraceSink, TraceValue, VariableSource as TraceVariableSource,
};

mod settings;

pub use settings::{RuntimeSettingError, RuntimeSettings};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeVariables {
    values: BTreeMap<String, String>,
    byte_values: BTreeMap<String, Vec<u8>>,
    system_hostname: Option<String>,
}

impl Default for RuntimeVariables {
    fn default() -> Self {
        let mut values = BTreeMap::new();
        values.insert(
            "LINEBUF".to_owned(),
            crate::config::DEFAULT_LINEBUF.to_string(),
        );
        values.insert(
            "TIMEOUT".to_owned(),
            crate::external_process::DEFAULT_PROCESS_TIMEOUT
                .as_secs()
                .to_string(),
        );
        values.insert(
            "UMASK".to_owned(),
            format!("{:03o}", crate::config::DEFAULT_UMASK),
        );
        values.insert(
            "LOCKEXT".to_owned(),
            crate::config::DEFAULT_LOCK_EXT.to_owned(),
        );
        Self {
            values,
            byte_values: BTreeMap::new(),
            system_hostname: None,
        }
    }
}

impl RuntimeVariables {
    pub fn set_system_hostname(&mut self, hostname: String) {
        self.system_hostname = Some(hostname);
    }

    pub(crate) fn system_hostname(&self) -> Option<&str> {
        self.system_hostname.as_deref()
    }
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        self.byte_values.remove(&name);
        self.values.insert(name, value.into());
    }

    pub fn set_bytes(&mut self, name: impl Into<String>, value: Vec<u8>) {
        let name = name.into();
        match String::from_utf8(value) {
            Ok(value) => {
                self.byte_values.remove(&name);
                self.values.insert(name, value);
            }
            Err(error) => {
                self.values.remove(&name);
                self.byte_values.insert(name, error.into_bytes());
            }
        }
    }

    pub(crate) fn set_bytes_with_trace(
        &mut self,
        name: String,
        value: Vec<u8>,
        line: Option<usize>,
        source: TraceVariableSource,
        trace: &mut impl TraceSink,
    ) {
        // Construct the bounded trace fields before moving the complete value
        // into runtime storage. The trace receives no value in metadata mode,
        // while high-detail mode copies only its independently limited prefix.
        let event = TraceName::new(&name).ok().map(|name| {
            let value = trace
                .detail()
                .includes_variable_values()
                .then(|| TraceValue::new(&value));
            TraceEvent::VariableAssigned {
                line,
                name,
                source,
                value,
            }
        });
        self.set_bytes(name, value);
        if let Some(event) = event {
            trace.record(event);
        }
    }

    pub fn last_folder(&self) -> Option<&str> {
        self.get("LASTFOLDER")
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn get_bytes(&self, name: &str) -> Option<&[u8]> {
        self.byte_values
            .get(name)
            .map(Vec::as_slice)
            .or_else(|| self.values.get(name).map(String::as_bytes))
    }

    pub fn expand_bytes(
        &self,
        source: &str,
        line: usize,
        limit: usize,
    ) -> Result<Vec<u8>, crate::config::ExpansionError> {
        crate::config::expand::expand_runtime_bytes(source, line, limit, |name| {
            self.get_bytes(name)
        })
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.values.remove(name);
        self.byte_values.remove(name);
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub(crate) fn byte_values(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_bytes()))
            .chain(
                self.byte_values
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_slice())),
            )
    }

    pub(crate) fn clear_match_values(&mut self) {
        self.values.retain(|name, _| {
            name != "MATCH"
                && !name.strip_prefix("MATCH").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
        });
        self.byte_values.retain(|name, _| {
            name != "MATCH"
                && !name.strip_prefix("MATCH").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
        });
    }

    pub(crate) fn set_match_value(&mut self, name: String, value: String) {
        self.values.insert(name, value);
    }

    pub fn record_commit(&mut self, report: &CommitReport) -> Result<(), String> {
        self.record_commit_with_trace(report, &mut NoTrace)
    }

    pub fn record_commit_with_trace(
        &mut self,
        report: &CommitReport,
        trace: &mut impl TraceSink,
    ) -> Result<(), String> {
        self.record_last_folder(report.last_folder(), trace)
    }

    pub fn record_partial_commit(&mut self, error: &CommitError) -> Result<(), String> {
        self.record_partial_commit_with_trace(error, &mut NoTrace)
    }

    pub fn record_partial_commit_with_trace(
        &mut self,
        error: &CommitError,
        trace: &mut impl TraceSink,
    ) -> Result<(), String> {
        self.record_last_folder(error.last_folder(), trace)
    }

    pub fn record_delivery(&mut self, delivery: &PublishedDelivery) -> Result<(), String> {
        self.record_delivery_with_trace(delivery, &mut NoTrace)
    }

    pub fn record_delivery_with_trace(
        &mut self,
        delivery: &PublishedDelivery,
        trace: &mut impl TraceSink,
    ) -> Result<(), String> {
        self.record_last_folder(Some(delivery.last_folder()), trace)
    }

    fn record_last_folder(
        &mut self,
        path: Option<&Path>,
        trace: &mut impl TraceSink,
    ) -> Result<(), String> {
        let Some(path) = path else {
            return Ok(());
        };
        let value = path.to_str().ok_or_else(|| {
            format!(
                "published destination cannot be represented as UTF-8: {}",
                path.display()
            )
        })?;
        self.values
            .insert("LASTFOLDER".to_owned(), value.to_owned());
        trace.record(TraceEvent::LastFolderUpdated);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
