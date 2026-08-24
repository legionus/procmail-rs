// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn builds_only_defaults_and_explicit_runtime_values() {
    let mut runtime = RuntimeVariables::default();
    runtime.set("HOME", "/mail/user");
    runtime.set("PATH", "/approved/bin");

    let environment = ProcessEnvironment::from_runtime(&runtime).unwrap();
    assert_eq!(environment.get("HOME"), Some("/mail/user"));
    assert_eq!(environment.get("PATH"), Some("/approved/bin"));
    assert_eq!(environment.get("SHELL"), Some(DEFAULT_SHELL));
    assert_eq!(environment.get("SHELLFLAGS"), Some(DEFAULT_SHELL_FLAGS));
    assert_eq!(environment.get("LINEBUF"), Some("2048"));
    assert_eq!(environment.get("TIMEOUT"), Some("960"));
    assert_eq!(environment.get("UMASK"), Some("077"));
    assert_eq!(
        environment.get("LOCKEXT"),
        Some(crate::config::DEFAULT_LOCK_EXT)
    );
    assert_eq!(environment.values().count(), 8);
}

#[test]
fn rejects_nul_and_oversized_aggregate_environment() {
    let mut runtime = RuntimeVariables::default();
    runtime.set("VALUE", "contains\0nul");
    assert!(ProcessEnvironment::from_runtime(&runtime).is_err());

    let mut runtime = RuntimeVariables::default();
    for index in 0..MAX_CHILD_ENVIRONMENT_VARIABLES {
        runtime.set(format!("V{index}"), "x");
    }
    assert!(ProcessEnvironment::from_runtime(&runtime).is_err());
}

#[test]
fn shell_policy_requires_an_exact_operator_approved_path() {
    let environment = ProcessEnvironment::from_runtime(&RuntimeVariables::default()).unwrap();
    assert!(ShellPolicy::disabled().authorize(&environment).is_err());

    let invocation = ShellPolicy::approve(DEFAULT_SHELL)
        .unwrap()
        .authorize(&environment)
        .unwrap();
    assert_eq!(invocation.path(), DEFAULT_SHELL);
    assert_eq!(invocation.flags(), DEFAULT_SHELL_FLAGS);

    let mut runtime = RuntimeVariables::default();
    runtime.set("SHELL", "/usr/bin/sh");
    let environment = ProcessEnvironment::from_runtime(&runtime).unwrap();
    assert!(
        ShellPolicy::approve(DEFAULT_SHELL)
            .unwrap()
            .authorize(&environment)
            .is_err()
    );
}

#[test]
fn shell_policy_accepts_only_bounded_absolute_normal_paths() {
    for path in [
        "",
        "bin/sh",
        "/",
        "//bin/sh",
        "/bin//sh",
        "/bin/sh/",
        "/bin/./sh",
        "/bin/../bin/sh",
        "/bin/s\0h",
    ] {
        assert!(ShellPolicy::approve(path).is_err(), "{path:?}");
    }
    assert!(ShellPolicy::approve(&format!("/{}", "x".repeat(MAX_SHELL_SETTING_LEN))).is_err());
    assert!(ShellPolicy::approve("/bin/sh").is_ok());
}
