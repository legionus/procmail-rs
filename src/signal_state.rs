// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

//! Minimal process-signal state for the supported Linux targets.

use std::io;
use std::sync::atomic::{AtomicI32, Ordering};

static RECEIVED_SIGNAL: AtomicI32 = AtomicI32::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivedSignal(i32);

impl ReceivedSignal {
    pub fn number(self) -> i32 {
        self.0
    }

    pub fn exit_code(self) -> u8 {
        u8::try_from(128_i32.saturating_add(self.0)).unwrap_or(255)
    }

    pub fn name(self) -> &'static str {
        match self.0 {
            libc::SIGHUP => "SIGHUP",
            libc::SIGINT => "SIGINT",
            libc::SIGQUIT => "SIGQUIT",
            libc::SIGTERM => "SIGTERM",
            _ => "signal",
        }
    }
}

pub fn install() -> io::Result<()> {
    // Linux represents a simple handler in sa_sigaction and accepts a zeroed
    // mask and flag word. Initialize the complete C value before exposing it
    // to libc, then install only the handlers reviewed for this program. The
    // handler itself must remain limited to lock-free atomic operations.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = record_signal as *const () as usize;
        action.sa_flags = 0;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(io::Error::last_os_error());
        }
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

pub fn received() -> Option<ReceivedSignal> {
    let signal = RECEIVED_SIGNAL.load(Ordering::SeqCst);
    (signal != 0).then_some(ReceivedSignal(signal))
}

pub fn check_io() -> io::Result<()> {
    match received() {
        Some(signal) => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!("interrupted by {}", signal.name()),
        )),
        None => Ok(()),
    }
}

extern "C" fn record_signal(signal: libc::c_int) {
    let _ = RECEIVED_SIGNAL.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
}

pub struct InterruptibleReader<R> {
    inner: R,
}

impl<R> InterruptibleReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R: io::Read> io::Read for InterruptibleReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        check_io()?;
        let result = self.inner.read(buffer);
        check_io()?;
        result
    }
}

impl<R: io::BufRead> io::BufRead for InterruptibleReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        check_io()?;
        let result = self.inner.fill_buf();

        // A handled signal can interrupt the underlying syscall before the
        // caller gets another chance to inspect the recorded signal. Check
        // again after fill_buf so the filtering result reports signal
        // termination instead of misclassifying the resulting EINTR as
        // malformed message input.
        check_io()?;
        result
    }

    fn consume(&mut self, amount: usize) {
        self.inner.consume(amount);
    }
}

#[cfg(test)]
#[path = "tests/signal_state.rs"]
mod tests;
