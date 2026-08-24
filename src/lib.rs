// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

#![deny(unsafe_code)]

#[cfg(not(all(
    target_os = "linux",
    any(target_pointer_width = "32", target_pointer_width = "64")
)))]
compile_error!("procmail-rs currently supports only 32-bit and 64-bit Linux targets");

pub mod config;
pub mod delivery;
pub mod environment;
pub mod eval;
pub mod external_filter;
pub mod external_process;
pub mod hostname;
pub mod limits;
pub mod message;
pub mod rc_file;
pub mod runtime;
pub mod trace;

#[allow(unsafe_code)]
mod mapped_file;

#[allow(unsafe_code)]
pub mod user_identity;
