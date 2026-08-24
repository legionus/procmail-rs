// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeMap;
use std::path::Path;

use crate::delivery::{CommitError, CommitReport, PublishedDelivery};
use crate::trace::{NoTrace, TraceEvent, TraceSink};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeVariables {
    values: BTreeMap<String, String>,
}

impl Default for RuntimeVariables {
    fn default() -> Self {
        let mut values = BTreeMap::new();
        values.insert(
            "LINEBUF".to_owned(),
            crate::config::DEFAULT_LINEBUF.to_string(),
        );
        values.insert("TIMEOUT".to_owned(), "960".to_owned());
        values.insert("UMASK".to_owned(), "077".to_owned());
        values.insert(
            "LOCKEXT".to_owned(),
            crate::config::DEFAULT_LOCK_EXT.to_owned(),
        );
        Self { values }
    }
}

impl RuntimeVariables {
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.values.insert(name.into(), value.into());
    }

    pub fn last_folder(&self) -> Option<&str> {
        self.get("LASTFOLDER")
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.values.remove(name);
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub(crate) fn clear_match_values(&mut self) {
        self.values.retain(|name, _| {
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
