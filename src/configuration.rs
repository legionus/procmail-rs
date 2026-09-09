// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Filesystem-independent validation required before message input.

use crate::config::{self, Config};
use crate::delivery::local_lock::{LockMethod, lock_sleep_from_config, lock_timeout_from_config};
use crate::delivery::maildir::Durability;
use crate::external_process::process_timeout_from_config;
use crate::limits::MessageLimits;
use crate::trace::TraceConfig;

#[derive(Debug, Clone, Copy)]
pub struct ConfigurationSettings {
    pub message_limits: MessageLimits,
    pub durability: Durability,
}

pub fn validate(config: &Config) -> Result<ConfigurationSettings, String> {
    let message_limits = MessageLimits::from_config(config).map_err(|error| error.to_string())?;
    let durability = Durability::from_config(config)?;

    // These settings are read again at their statement-order execution
    // points, but validating every reachable literal here ensures malformed
    // configuration is rejected before stdin is consumed.
    LockMethod::from_config(config)?;
    lock_timeout_from_config(config)?;
    lock_sleep_from_config(config)?;
    process_timeout_from_config(config)?;
    config::umask_from_config(config)?;
    TraceConfig::from_config(config).map_err(|error| error.to_string())?;

    Ok(ConfigurationSettings {
        message_limits,
        durability,
    })
}
