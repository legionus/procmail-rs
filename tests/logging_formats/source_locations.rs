// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

fn run(config: &std::path::Path, format: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
        .args(["filter", "--format", format, "--config"])
        .arg(config)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Subject: test private-header-sentinel\n\nbody private-body-sentinel\n")
        .unwrap();
    child.wait_with_output().unwrap()
}

fn check_source(record: &str, file: &std::path::Path, line: usize, format: &str) {
    if format == "json" {
        assert!(
            record.contains(&format!("\"rc_file\":\"{}\"", file.display())),
            "{record}"
        );
        assert!(
            record.contains(&format!("\"line\":{line},"))
                || record.contains(&format!("\"recipe_line\":{line},"))
                || record.contains(&format!("\"condition_line\":{line},")),
            "{record}"
        );
    } else {
        assert!(
            record.contains(&format!("[rc \"{}\":{line}]", file.display())),
            "{record}"
        );
    }
}

#[test]
fn include_switch_and_deferred_delivery_keep_their_own_sources() {
    for format in ["text", "json"] {
        for detail in ["metadata", "values"] {
            for area in ["", " B"] {
                let base = temporary_directory("rc-sources");
                let root = base.join("private-root-source.rc");
                let child = base.join("private-child-source.rc");
                let switched = base.join("private-switched-source.rc");
                let logfile = base.join("filter.log");
                let copied = base.join("copied");
                create_maildir(&copied);
                fs::write(&root, format!(
                    "MAILDIR={}\nLOGFILE={}\nVERBOSE=yes\nLOGDETAIL={detail}\nINCLUDERC=private-child-source.rc\nROOT_AFTER=done\n:0{area}\n* {}\n/dev/null\n",
                    base.display(), logfile.display(),
                    if area.is_empty() { "^Subject: changed" } else { "^body" },
                )).unwrap();
                fs::write(&child, "CHILD_BEFORE=child\nSWITCHRC=private-switched-source.rc\nCHILD_UNREACHABLE=bad\n").unwrap();
                fs::write(&switched, ":0 c\n* ^Subject: test\ncopied/\n:0\nheaders {\n set Subject changed-private-header\n extract raw Subject into EXTRACTED\n}\nSWITCH_TAIL=done\n").unwrap();
                let output = run(&root, format);
                assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
                let trace = fs::read_to_string(&logfile).unwrap();
                assert!(!trace.contains("CHILD_UNREACHABLE"), "{trace}");
                assert_eq!(fs::read_dir(copied.join("new")).unwrap().count(), 1);

                for secret in [
                    "private-header-sentinel",
                    "private-body-sentinel",
                    "changed-private-header",
                ] {
                    assert!(!trace.contains(secret), "leaked {secret}: {trace}");
                }

                if detail == "metadata" {
                    for secret in [
                        "private-root-source",
                        "private-child-source",
                        "private-switched-source",
                        "copied",
                        "^Subject: test",
                    ] {
                        assert!(!trace.contains(secret), "leaked {secret}: {trace}");
                    }

                    assert!(!trace.contains("rc_file"));
                } else {
                    let find =
                        |text: &str| trace.lines().find(|record| record.contains(text)).unwrap();
                    check_source(find("INCLUDERC"), &root, 5, format);
                    check_source(find("CHILD_BEFORE"), &child, 1, format);
                    check_source(find("SWITCHRC"), &child, 2, format);
                    check_source(find("SWITCH_TAIL"), &switched, 9, format);
                    check_source(find("ROOT_AFTER"), &root, 6, format);
                    let publication = trace
                        .lines()
                        .find(|record| {
                            if format == "json" {
                                record.contains("\"stage\":\"published\"")
                                    && record.contains("\"destination\":\"maildir\"")
                            } else {
                                record.contains("Delivered to Maildir")
                            }
                        })
                        .unwrap();
                    check_source(publication, &switched, 3, format);
                    check_source(find("EXTRACTED"), &switched, 7, format);
                }

                fs::remove_dir_all(base).unwrap();
            }
        }
    }
}

#[test]
fn parallel_copy_branch_does_not_change_parent_source() {
    let base = temporary_directory("parallel-rc-sources");
    let root = base.join("parent.rc");
    let child = base.join("branch.rc");
    let logfile = base.join("filter.log");
    create_maildir(&base.join("copied"));
    fs::write(&root, format!(
        "MAILDIR={}\nLOGFILE={}\nVERBOSE=yes\nLOGDETAIL=values\n:0 c\n{{\n INCLUDERC=branch.rc\n}}\nPARENT_AFTER=done\n:0\n/dev/null\n",
        base.display(), logfile.display(),
    )).unwrap();
    fs::write(
        &child,
        "BRANCH_VALUE=child\n:0\n* ^Subject: test\ncopied/\n",
    )
    .unwrap();
    let output = run(&root, "json");
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let trace = fs::read_to_string(logfile).unwrap();
    check_source(
        trace
            .lines()
            .find(|record| record.contains("PARENT_AFTER"))
            .unwrap(),
        &root,
        9,
        "json",
    );
    check_source(
        trace
            .lines()
            .find(|record| record.contains("BRANCH_VALUE"))
            .unwrap(),
        &child,
        1,
        "json",
    );
    check_source(
        trace
            .lines()
            .find(|record| {
                record.contains("\"stage\":\"published\"")
                    && record.contains("\"destination\":\"maildir\"")
            })
            .unwrap(),
        &child,
        4,
        "json",
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn commands_filters_unsets_and_reparsed_conditions_keep_the_include_source() {
    let base = temporary_directory("command-rc-sources");
    let root = base.join("parent.rc");
    let child = base.join("commands.rc");
    let logfile = base.join("filter.log");
    fs::write(&root, format!(
        "MAILDIR={}\nLOGFILE={}\nVERBOSE=yes\nLOGDETAIL=values\nINCLUDERC=commands.rc\nROOT_AFTER=done\n:0\n/dev/null\n",
        base.display(), logfile.display(),
    )).unwrap();
    fs::write(&child, "CAPTURED=`printf test`\nREMOVED=value\nREMOVED\n:0 fw\n| cat\n:0\n* $ ^Subject: ${CAPTURED}\nheaders {\n extract raw Subject into EXTRACTED\n}\nLOG=tail\n").unwrap();
    let output = run(&root, "json");
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let trace = fs::read_to_string(logfile).unwrap();

    for (event, line) in [
        ("\"event\":\"external-command-executing\",\"line\":1", 1),
        ("\"event\":\"variable-unset\"", 3),
        ("\"event\":\"external-filter-replaced\"", 4),
        ("\"event\":\"condition\"", 7),
        ("\"event\":\"header-operation\"", 9),
        ("\"event\":\"log\"", 11),
    ] {
        check_source(
            trace.lines().find(|record| record.contains(event)).unwrap(),
            &child,
            line,
            "json",
        );
    }

    for secret in ["private-header-sentinel", "private-body-sentinel"] {
        assert!(!trace.contains(secret), "{trace}");
    }

    check_source(
        trace
            .lines()
            .find(|record| record.contains("ROOT_AFTER"))
            .unwrap(),
        &root,
        6,
        "json",
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn empty_and_failed_include_events_identify_the_calling_statement() {
    for target in ["", "missing.rc"] {
        for detail in ["metadata", "values"] {
            let base = temporary_directory("include-status-source");
            let root = base.join("private-root-source.rc");
            let logfile = base.join("filter.log");
            fs::write(&root, format!(
                "MAILDIR={}\nLOGFILE={}\nVERBOSE=yes\nLOGDETAIL={detail}\nINCLUDERC={target}\n:0\n/dev/null\n",
                base.display(), logfile.display(),
            )).unwrap();
            let output = run(&root, "json");
            assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
            let trace = fs::read_to_string(logfile).unwrap();
            let record = trace
                .lines()
                .find(|record| record.contains("\"event\":\"rc-file\""))
                .unwrap();
            let stage = if target.is_empty() { "empty" } else { "failed" };
            assert!(
                record.contains(&format!("\"stage\":\"{stage}\"")),
                "{record}"
            );

            if detail == "values" {
                check_source(record, &root, 5, "json");
            } else {
                assert!(!record.contains("rc_file"));
                assert!(!record.contains("private-root-source"));
                assert!(!record.contains("missing.rc"));
            }

            fs::remove_dir_all(base).unwrap();
        }
    }
}
