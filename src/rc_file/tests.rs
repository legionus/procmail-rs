// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "procmail-rs-rc-loader-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn reads_trusted_regular_files_and_accounts_bytes() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    let child = directory.path("child.rc");
    fs::write(&root, "ROOT=yes\n").unwrap();
    fs::write(&child, "CHILD=yes\n").unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();

    let (mut loader, loaded_root) = RcFileLoader::for_root(&root).unwrap();
    let loaded_child = loader.load(&child, 1).unwrap();

    assert_eq!(loaded_root.source(), "ROOT=yes\n");
    assert_eq!(loaded_child.source(), "CHILD=yes\n");
    assert_eq!(loader.files_read(), 2);
    assert_eq!(loader.bytes_read(), 19);
}

#[test]
fn rejects_non_regular_and_broadly_writable_runtime_files() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    let child = directory.path("child.rc");
    let link = directory.path("link.rc");
    let subdirectory = directory.path("directory.rc");
    fs::write(&root, "ROOT=yes\n").unwrap();
    fs::write(&child, "CHILD=yes\n").unwrap();
    symlink(&child, &link).unwrap();
    fs::create_dir(&subdirectory).unwrap();

    let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
    assert!(loader.load(&link, 1).is_err());
    let error = loader.load(&subdirectory, 1).unwrap_err();
    assert!(error.safe_message().contains("not a regular file"));

    fs::set_permissions(&child, fs::Permissions::from_mode(0o620)).unwrap();
    let error = loader.load(&child, 1).unwrap_err();
    assert!(error.to_string().contains("writable by group"));

    fs::set_permissions(&child, fs::Permissions::from_mode(0o602)).unwrap();
    let error = loader.load(&child, 1).unwrap_err();
    assert!(error.to_string().contains("other users"));
}

#[test]
fn rejects_a_runtime_file_owned_by_a_different_uid() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    let child = directory.path("child.rc");
    fs::write(&root, "ROOT=yes\n").unwrap();
    fs::write(&child, "CHILD=yes\n").unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
    let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
    loader.trusted_uid ^= 1;

    let error = loader.load(&child, 1).unwrap_err();

    assert!(error.safe_message().contains("differs from trusted owner"));
}

#[test]
fn enforces_depth_before_opening_the_file() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    fs::write(&root, "ROOT=yes\n").unwrap();
    let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

    let error = loader
        .load(&directory.path("missing.rc"), MAX_RC_INCLUDE_DEPTH + 1)
        .unwrap_err();

    assert!(error.to_string().contains("nesting exceeds"));
    assert!(error.is_resource_limit());
    assert_eq!(loader.files_read(), 1);
}

#[test]
fn distinguishes_recoverable_open_errors_from_resource_limits() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    fs::write(&root, "ROOT=yes\n").unwrap();
    let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

    let error = loader.load(&directory.path("missing.rc"), 1).unwrap_err();

    assert!(!error.is_resource_limit());
    assert!(!error.safe_message().is_empty());
}

#[test]
fn failed_utf8_validation_still_consumes_file_and_byte_budget() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    let child = directory.path("binary.rc");
    fs::write(&root, "ROOT=yes\n").unwrap();
    fs::write(&child, [0xff]).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
    let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

    let error = loader.load(&child, 1).unwrap_err();

    assert!(error.to_string().contains("not valid UTF-8"));
    assert_eq!(loader.files_read(), 2);
    assert_eq!(loader.bytes_read(), 10);
}

#[test]
fn failed_open_attempts_reach_the_file_count_limit() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    fs::write(&root, "ROOT=yes\n").unwrap();
    let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

    for attempt in 1..MAX_RC_FILE_COUNT {
        let error = loader
            .load(&directory.path(&format!("missing-{attempt}.rc")), 1)
            .unwrap_err();
        assert!(!error.is_resource_limit());
    }
    let error = loader
        .load(&directory.path("one-too-many.rc"), 1)
        .unwrap_err();

    assert!(error.is_resource_limit());
    assert!(error.safe_message().contains("file count exceeds"));
}

#[test]
fn repeated_files_reach_the_aggregate_byte_limit() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    let child = directory.path("child.rc");
    fs::write(&root, "#\n").unwrap();
    fs::write(&child, vec![b'#'; MAX_RC_SIZE]).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
    let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();

    for _ in 0..3 {
        loader.load(&child, 1).unwrap();
    }
    let error = loader.load(&child, 1).unwrap_err();

    assert!(error.is_resource_limit());
    assert!(error.safe_message().contains("aggregate rc size exceeds"));
}

#[test]
fn parsed_runtime_files_share_root_syntax_budgets() {
    let mut condition_source = String::new();
    for _ in 0..(MAX_RC_CONDITIONS / config::MAX_CONDITIONS_PER_RECIPE) {
        condition_source.push_str(":0 c\n");
        condition_source.push_str(&"* < 1\n".repeat(config::MAX_CONDITIONS_PER_RECIPE));
        condition_source.push_str("maildir:x\n");
    }
    let cases = [
        (
            "statement",
            "A=\n".repeat(MAX_RC_STATEMENTS),
            "B=\n".to_owned(),
        ),
        (
            "recipe",
            ":0 c\nmaildir:x\n".repeat(MAX_RC_RECIPES),
            ":0\nmaildir:x\n".to_owned(),
        ),
        (
            "condition",
            condition_source,
            ":0\n* < 1\nmaildir:x\n".to_owned(),
        ),
        (
            "regex",
            format!(":0\n{}maildir:x\n", "* pattern\n".repeat(MAX_RC_REGEXES)),
            ":0\n* pattern\nmaildir:x\n".to_owned(),
        ),
    ];

    for (name, root_source, child_source) in cases {
        let directory = TestDirectory::new();
        let root = directory.path("root.rc");
        let child = directory.path("child.rc");
        fs::write(&root, &root_source).unwrap();
        fs::write(&child, child_source).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
        let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
        let root_config = config::parse(&root_source).unwrap();
        loader.account_root_config(&root_config).unwrap();
        let expression_config = config::parse(&format!("INCLUDERC={}\n", child.display())).unwrap();
        let Statement::Include(expression) = &expression_config.statements[0] else {
            panic!("expected include");
        };

        let error = loader
            .load_config(expression, &RuntimeVariables::default(), 1)
            .unwrap_err();

        assert!(error.is_resource_limit(), "{name}: {error}");
        assert!(
            error
                .safe_message()
                .contains(&format!("rc {name} count exceeds the active limit")),
            "{name}: {error}"
        );
    }
}

#[test]
fn runtime_file_uses_limits_reached_before_its_include() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    let child = directory.path("child.rc");
    let child_source = format!(
        ":0 c\n{}maildir:x\n:0\n* extra\nmaildir:y\n",
        "* pattern\n".repeat(MAX_RC_REGEXES)
    );
    fs::write(&root, "LIMIT_RC_REGEXES=257\n").unwrap();
    fs::write(&child, child_source).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
    let expression_config = config::parse(&format!("INCLUDERC={}\n", child.display())).unwrap();
    let Statement::Include(expression) = &expression_config.statements[0] else {
        panic!("expected include");
    };

    let (mut raised_loader, _) = RcFileLoader::for_root(&root).unwrap();
    let root_config = config::parse("LIMIT_RC_REGEXES=257\n").unwrap();
    raised_loader.account_root_config(&root_config).unwrap();
    let mut raised_runtime = RuntimeVariables::default();
    raised_runtime.set("LIMIT_RC_REGEXES".to_owned(), "257".to_owned());
    assert!(
        raised_loader
            .load_config(expression, &raised_runtime, 1)
            .is_ok()
    );

    let (mut default_loader, _) = RcFileLoader::for_root(&root).unwrap();
    default_loader.account_root_config(&root_config).unwrap();
    let error = default_loader
        .load_config(expression, &RuntimeVariables::default(), 1)
        .unwrap_err();
    assert!(error.is_resource_limit());
    assert!(
        error
            .safe_message()
            .contains("rc regex count exceeds the active limit of 256")
    );
}

#[test]
fn resolves_runtime_include_path_and_expands_child_with_current_values() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    let child = directory.path("child.rc");
    fs::write(&root, "ROOT=yes\n").unwrap();
    fs::write(&child, "CHILD=$PARENT\n").unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
    let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
    let parsed = config::parse("INCLUDERC=$SELECTED\n").unwrap();
    let config::Statement::Include(expression) = &parsed.statements[0] else {
        panic!("expected include");
    };
    let mut runtime = RuntimeVariables::default();
    runtime.set("MAILDIR", directory.0.to_string_lossy());
    runtime.set("SELECTED", "child.rc");
    runtime.set("PARENT", "visible");

    let loaded = loader
        .load_config(expression, &runtime, 1)
        .unwrap()
        .unwrap();
    let config::Statement::Assignment(assignment) = &loaded.config().statements[0] else {
        panic!("expected assignment");
    };

    assert_eq!(loaded.path(), child);
    assert_eq!(assignment.value, "visible");
}

#[test]
fn rejects_runtime_include_that_changes_pre_input_settings() {
    let directory = TestDirectory::new();
    let root = directory.path("root.rc");
    let child = directory.path("child.rc");
    fs::write(&root, "ROOT=yes\n").unwrap();
    fs::write(&child, "LIMIT_MSG_BODY=1k\n").unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o600)).unwrap();
    let (mut loader, _) = RcFileLoader::for_root(&root).unwrap();
    let parsed = config::parse(&format!("INCLUDERC={}\n", child.display())).unwrap();
    let config::Statement::Include(expression) = &parsed.statements[0] else {
        panic!("expected include");
    };

    let error = loader
        .load_config(expression, &RuntimeVariables::default(), 1)
        .unwrap_err();

    assert!(error.to_string().contains("must be set before message"));
}
