// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::delivery::{CommitError, CommitReport, PublishedDelivery};
use crate::trace::{
    TraceEvent, TraceName, TraceSink, TraceValue, VariableSource as TraceVariableSource,
};

mod settings;

pub use settings::{RuntimeSettingError, RuntimeSettings};

#[derive(Debug, Clone, Copy)]
pub enum PublicationResult<'a> {
    Delivery(&'a PublishedDelivery),
    Fanout(&'a CommitReport),
    PartialFanout(&'a CommitError),
}

impl<'a> PublicationResult<'a> {
    pub fn len(self) -> usize {
        match self {
            Self::Delivery(_) => 1,
            Self::Fanout(report) => report.published().len(),
            Self::PartialFanout(error) => error.published().len(),
        }
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    fn last_folder(self) -> Option<&'a Path> {
        match self {
            Self::Delivery(delivery) => Some(delivery.last_folder()),
            Self::Fanout(report) => report.last_folder(),
            Self::PartialFanout(error) => error.last_folder(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeVariables {
    parent: Option<Arc<RuntimeLayer>>,
    values: BTreeMap<String, RuntimeValue>,
    system_hostname: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct RuntimeLayer {
    parent: Option<Arc<RuntimeLayer>>,
    values: BTreeMap<String, RuntimeValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeValue {
    Text(String),
    Bytes(Vec<u8>),
    Removed,
}

impl Default for RuntimeVariables {
    fn default() -> Self {
        let mut values = BTreeMap::new();
        values.insert(
            "LINEBUF".to_owned(),
            RuntimeValue::Text(crate::config::DEFAULT_LINEBUF.to_string()),
        );
        values.insert(
            "TIMEOUT".to_owned(),
            RuntimeValue::Text(
                crate::external_process::DEFAULT_PROCESS_TIMEOUT
                    .as_secs()
                    .to_string(),
            ),
        );
        values.insert(
            "UMASK".to_owned(),
            RuntimeValue::Text(format!("{:03o}", crate::config::DEFAULT_UMASK)),
        );
        values.insert(
            "LOCKEXT".to_owned(),
            RuntimeValue::Text(crate::config::DEFAULT_LOCK_EXT.to_owned()),
        );
        Self {
            parent: None,
            values,
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

    pub fn fork(&mut self) -> Self {
        // Freeze the current delta once and let both execution branches point
        // at it. Later assignments stay in branch-local maps, avoiding an
        // eager copy of every bounded value when a copy block is selected.
        let shared = Arc::new(RuntimeLayer {
            parent: self.parent.take(),
            values: std::mem::take(&mut self.values),
        });
        self.parent = Some(Arc::clone(&shared));
        Self {
            parent: Some(shared),
            values: BTreeMap::new(),
            system_hostname: self.system_hostname.clone(),
        }
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.values
            .insert(name.into(), RuntimeValue::Text(value.into()));
    }

    pub fn set_bytes(&mut self, name: impl Into<String>, value: Vec<u8>) {
        let name = name.into();
        let value = match String::from_utf8(value) {
            Ok(value) => RuntimeValue::Text(value),
            Err(error) => RuntimeValue::Bytes(error.into_bytes()),
        };
        self.values.insert(name, value);
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

    pub(crate) fn apply_header_extractions(
        &mut self,
        extractions: Vec<crate::header_edit::HeaderExtraction>,
        trace: &mut impl TraceSink,
    ) {
        for extraction in extractions {
            let name = TraceName::new(&extraction.target).ok();
            self.set_bytes(extraction.target, extraction.value);

            // Extracted bytes are header values even after assignment to an rc
            // variable. Record the assignment itself, but never copy those
            // bytes into diagnostics in either trace detail mode.
            if let Some(name) = name {
                trace.record(TraceEvent::VariableAssigned {
                    line: Some(extraction.line),
                    name,
                    source: TraceVariableSource::RcFile,
                    value: None,
                });
            }
        }
    }

    pub fn last_folder(&self) -> Option<&str> {
        self.get("LASTFOLDER")
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        let Some(value) = self.find(name) else {
            return match name {
                "#" => Some("0"),
                _ if crate::config::is_positional_parameter_name(name) => Some(""),
                _ => None,
            };
        };
        match value {
            RuntimeValue::Text(value) => Some(value),
            RuntimeValue::Bytes(_) | RuntimeValue::Removed => None,
        }
    }

    pub fn get_bytes(&self, name: &str) -> Option<&[u8]> {
        let Some(value) = self.find(name) else {
            return match name {
                "#" => Some(b"0"),
                _ if crate::config::is_positional_parameter_name(name) => Some(b""),
                _ => None,
            };
        };
        match value {
            RuntimeValue::Text(value) => Some(value.as_bytes()),
            RuntimeValue::Bytes(value) => Some(value),
            RuntimeValue::Removed => None,
        }
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
        self.values.insert(name.to_owned(), RuntimeValue::Removed);
    }

    pub(crate) fn shift_positionals(&mut self, requested: usize) {
        let count = self
            .get("#")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let amount = requested.min(count);
        let remaining = count - amount;

        // Collect the bounded source window before overwriting numeric names.
        // This preserves overlapping shifts and keeps a forked runtime from
        // consulting values changed earlier in the same operation.
        let shifted = (1..=remaining)
            .map(|index| {
                self.get(&(index + amount).to_string())
                    .unwrap_or("")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        for index in 1..=crate::config::MAX_POSITIONAL_ARGUMENTS {
            if let Some(value) = shifted.get(index - 1) {
                self.set(index.to_string(), value.clone());
            } else {
                self.remove(&index.to_string());
            }
        }
        self.set("#", remaining.to_string());
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.visible_values()
            .into_iter()
            .filter_map(|(name, value)| {
                if let RuntimeValue::Text(value) = value {
                    Some((name, value.as_str()))
                } else {
                    None
                }
            })
    }

    pub(crate) fn byte_values(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.visible_values()
            .into_iter()
            .filter_map(|(name, value)| {
                let value = match value {
                    RuntimeValue::Text(value) => value.as_bytes(),
                    RuntimeValue::Bytes(value) => value.as_slice(),
                    RuntimeValue::Removed => return None,
                };
                Some((name, value))
            })
    }

    pub(crate) fn clear_match_values(&mut self) {
        self.remove("MATCH");
        self.clear_numbered_match_values();
    }

    pub(crate) fn clear_numbered_match_values(&mut self) {
        let names = self
            .visible_values()
            .into_keys()
            .filter(|name| {
                name.strip_prefix("MATCH").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for name in names {
            self.remove(&name);
        }
    }

    pub(crate) fn set_match_value(&mut self, name: String, value: String) {
        self.values.insert(name, RuntimeValue::Text(value));
    }

    pub fn record_publication(
        &mut self,
        result: PublicationResult<'_>,
        trace: &mut impl TraceSink,
    ) -> Result<(), String> {
        let Some(path) = result.last_folder() else {
            return Ok(());
        };
        let value = path.to_str().ok_or_else(|| {
            format!(
                "published destination cannot be represented as UTF-8: {}",
                path.display()
            )
        })?;
        self.values.insert(
            "LASTFOLDER".to_owned(),
            RuntimeValue::Text(value.to_owned()),
        );
        trace.record(TraceEvent::LastFolderUpdated);
        Ok(())
    }

    fn find(&self, name: &str) -> Option<&RuntimeValue> {
        if let Some(value) = self.values.get(name) {
            return Some(value);
        }
        let mut layer = self.parent.as_deref();
        while let Some(current) = layer {
            if let Some(value) = current.values.get(name) {
                return Some(value);
            }
            layer = current.parent.as_deref();
        }
        None
    }

    fn visible_values(&self) -> BTreeMap<&str, &RuntimeValue> {
        let mut visible = BTreeMap::new();
        for (name, value) in &self.values {
            visible.insert(name.as_str(), value);
        }
        let mut layer = self.parent.as_deref();
        while let Some(current) = layer {
            for (name, value) in &current.values {
                visible.entry(name.as_str()).or_insert(value);
            }
            layer = current.parent.as_deref();
        }
        visible
    }
}

#[cfg(test)]
#[path = "tests/runtime.rs"]
mod tests;
