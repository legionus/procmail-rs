// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::ReceivedSignal;

#[test]
fn handled_signals_have_names_and_shell_exit_codes() {
    for (number, name, exit_code) in [
        (libc::SIGHUP, "SIGHUP", 129),
        (libc::SIGINT, "SIGINT", 130),
        (libc::SIGQUIT, "SIGQUIT", 131),
        (libc::SIGTERM, "SIGTERM", 143),
    ] {
        let signal = ReceivedSignal(number);
        assert_eq!(signal.number(), number);
        assert_eq!(signal.name(), name);
        assert_eq!(signal.exit_code(), exit_code);
    }
}
