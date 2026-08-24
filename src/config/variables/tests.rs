// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn assigns_explicit_sources_to_variable_classes() {
    assert_eq!(
        variable_policy("MAILDIR").assignment_target(VariableSource::RcFile),
        Some(AssignmentTarget::Maildir)
    );
    assert!(!variable_policy("MAILDIR").allows(VariableSource::CommandLine));
    assert!(variable_policy("LASTFOLDER").allows(VariableSource::Runtime));
    assert!(!variable_policy("LASTFOLDER").allows(VariableSource::RcFile));
    assert!(variable_policy("MATCH").allows(VariableSource::Runtime));
    assert!(variable_policy("MATCH1").allows(VariableSource::Runtime));
    assert!(!variable_policy("MATCH1").allows(VariableSource::RcFile));
    assert_eq!(
        variable_policy("VERBOSE").assignment_target(VariableSource::RcFile),
        Some(AssignmentTarget::Verbose)
    );
    assert_eq!(
        variable_policy("LOGFILE").assignment_target(VariableSource::RcFile),
        Some(AssignmentTarget::LogFile)
    );
    assert_eq!(
        variable_policy("LOGDETAIL").assignment_target(VariableSource::RcFile),
        Some(AssignmentTarget::LogDetail)
    );
    assert!(!variable_policy("VERBOSE").allows(VariableSource::CommandLine));
    assert!(!variable_policy("LOGFILE").allows(VariableSource::CommandLine));
    assert!(!variable_policy("LOGDETAIL").allows(VariableSource::CommandLine));
    assert_eq!(
        variable_policy("LOGABSTRACT").assignment_target(VariableSource::RcFile),
        Some(AssignmentTarget::LogAbstract)
    );
    assert!(!variable_policy("LOGABSTRACT").allows(VariableSource::CommandLine));
    assert_eq!(
        variable_policy("USER_VALUE").assignment_target(VariableSource::CommandLine),
        Some(AssignmentTarget::User)
    );
    assert_eq!(
        variable_policy("SHELL").assignment_target(VariableSource::RcFile),
        Some(AssignmentTarget::Shell)
    );
    assert_eq!(
        variable_policy("SHELLFLAGS").assignment_target(VariableSource::CommandLine),
        Some(AssignmentTarget::ShellFlags)
    );
    assert_eq!(
        variable_policy("PATH").assignment_target(VariableSource::RcFile),
        Some(AssignmentTarget::Path)
    );
    assert_eq!(
        variable_policy("LOCKEXT").assignment_target(VariableSource::RcFile),
        Some(AssignmentTarget::LockExt)
    );
    assert!(!variable_policy("LOCKEXT").allows(VariableSource::CommandLine));
    assert_eq!(variable_policy("DEFAULT"), VariablePolicy::Unsupported);
    assert_eq!(
        variable_policy("USER_VALUE"),
        VariablePolicy::RcOrCommandLine(AssignmentTarget::User)
    );
}

#[test]
fn rejects_unsupported_procmail_variables_from_command_line() {
    for name in UNSUPPORTED_PROCMAIL_VARIABLES {
        let error = SuppliedVariable::parse(format!("{name}=value")).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("procmail variable {name} is not supported")
        );
    }
}

#[test]
fn registry_classifies_every_unsupported_procmail_variable() {
    for name in UNSUPPORTED_PROCMAIL_VARIABLES {
        assert_eq!(variable_policy(name), VariablePolicy::Unsupported, "{name}");
    }
}

#[test]
fn special_procmail_names_never_fall_through_to_user_policy() {
    for name in [
        "LOCKEXT",
        "LOGABSTRACT",
        "LOG",
        "DELIVERED",
        "SHELLMETAS",
        "PROCMAIL_VERSION",
        "PROCMAIL_OVERFLOW",
    ] {
        assert_ne!(
            variable_policy(name),
            VariablePolicy::RcOrCommandLine(AssignmentTarget::User),
            "{name}"
        );
    }
}

#[test]
fn validates_lock_extension_after_expansion() {
    for value in ["", ".lock", "-user"] {
        assert!(validate_lock_ext(value).is_ok(), "{value:?}");
    }
    for value in ["dir/lock", "\0"] {
        assert!(validate_lock_ext(value).is_err(), "{value:?}");
    }
}

#[test]
fn accepts_only_the_privacy_preserving_log_abstract_mode() {
    assert!(validate_log_abstract("no").is_ok());
    for value in ["", "No", "off", "yes", "all"] {
        assert_eq!(
            validate_log_abstract(value).unwrap_err(),
            "LOGABSTRACT supports only 'no'; other values could log sensitive header values"
        );
    }
}

#[test]
fn parses_bounded_command_line_variables() {
    let variable = SuppliedVariable::parse("BOX=one=two".into()).unwrap();
    assert_eq!(variable.name(), "BOX");
    assert_eq!(variable.value(), "one=two");

    for input in ["BOX", "=value", "9BOX=value", "BOX-NAME=value"] {
        assert!(SuppliedVariable::parse(input.into()).is_err(), "{input:?}");
    }
}

#[test]
fn admits_only_passwd_backed_initial_names() {
    for name in ["HOME", "LOGNAME"] {
        let variable = SuppliedVariable::from_environment(name, "value".into()).unwrap();
        assert_eq!(variable.name(), name);
        assert_eq!(variable.source(), VariableSource::Environment);
    }
    assert!(SuppliedVariable::from_environment("PATH", "value".into()).is_err());
}

#[test]
fn admits_a_bounded_system_hostname_without_allowing_host_on_the_command_line() {
    let hostname = SuppliedVariable::from_system_hostname("mail.example".to_owned()).unwrap();
    assert_eq!(hostname.name(), "HOST");
    assert_eq!(hostname.value(), "mail.example");
    assert_eq!(hostname.source(), VariableSource::System);

    assert!(SuppliedVariable::from_system_hostname(String::new()).is_err());
    assert!(
        SuppliedVariable::from_system_hostname("h".repeat(crate::hostname::MAX_HOSTNAME_LEN + 1))
            .is_err()
    );
    assert!(SuppliedVariable::parse("HOST=forged".to_owned()).is_err());
}

#[test]
fn exposes_a_read_only_program_version() {
    let version = SuppliedVariable::from_program_version().unwrap();

    assert_eq!(version.name(), "PROCMAIL_VERSION");
    assert_eq!(version.value(), env!("CARGO_PKG_VERSION"));
    assert_eq!(version.source(), VariableSource::System);
    assert_eq!(
        variable_policy("PROCMAIL_VERSION"),
        VariablePolicy::ReadOnly
    );
    assert!(SuppliedVariable::parse("PROCMAIL_VERSION=3.22".to_owned()).is_err());
}

#[test]
fn rejects_command_line_sources_not_allowed_by_policy() {
    for name in ["MAILDIR", "LASTFOLDER", "LIMIT_MSG_BODY", "LOGABSTRACT"] {
        let error = SuppliedVariable::parse(format!("{name}=value")).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("variable {name} cannot be supplied with --set")
        );
    }
}

#[test]
fn bounds_command_line_name_and_value() {
    let name_at_limit = "A".repeat(MAX_ASSIGNMENT_NAME_LEN);
    assert!(SuppliedVariable::parse(format!("{name_at_limit}=value")).is_ok());
    assert!(
        SuppliedVariable::parse(format!("{}=value", "A".repeat(MAX_ASSIGNMENT_NAME_LEN + 1)))
            .is_err()
    );

    let value_at_limit = "v".repeat(MAX_ASSIGNMENT_VALUE_LEN);
    assert!(SuppliedVariable::parse(format!("A={value_at_limit}")).is_ok());
    assert!(
        SuppliedVariable::parse(format!("A={}", "v".repeat(MAX_ASSIGNMENT_VALUE_LEN + 1))).is_err()
    );
}

#[test]
fn applies_shell_setting_limit_to_command_line_values() {
    for name in ["SHELL", "SHELLFLAGS", "PATH"] {
        assert!(
            SuppliedVariable::parse(format!("{name}={}", "x".repeat(MAX_SHELL_SETTING_LEN)))
                .is_ok()
        );
        let error =
            SuppliedVariable::parse(format!("{name}={}", "x".repeat(MAX_SHELL_SETTING_LEN + 1)))
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("--set {name} value exceeds the hard limit of {MAX_SHELL_SETTING_LEN} bytes")
        );
    }
}

#[test]
fn parses_umask_at_accepted_boundaries() {
    for (value, expected) in [
        ("0", 0),
        ("0000", 0),
        ("077", 0o077),
        ("0776", 0o776),
        ("0777", 0o777),
    ] {
        assert_eq!(parse_umask(value).unwrap(), expected);
    }
    for value in [
        "",
        "00000",
        "8",
        "078",
        "1000",
        "777777777777777777777777",
        "-1",
        "0o77",
        " 077",
    ] {
        assert!(parse_umask(value).is_err(), "{value:?}");
    }
}
