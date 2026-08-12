#![deny(unsafe_code)]

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("procmail-rs currently supports only 64-bit Linux targets");

pub mod config;
pub mod delivery;
pub mod eval;
pub mod limits;
pub mod message;

#[allow(unsafe_code)]
mod mapped_file;
