// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const FIXTURES: &str = "tests/fixtures/differential_rc";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn create() -> Self {
        let base = std::env::temp_dir();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        // Parallel test processes can share a timestamp and pid. Add a local
        // sequence and rely on atomic directory creation to reject the rare
        // collision without ever reusing another test's output directory.
        for _ in 0..128 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "procmail-rs-differential-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("cannot create temporary output directory: {error}"),
            }
        }
        panic!("cannot allocate a unique temporary output directory");
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn runtime_rc_behavior_matches_reference_procmail() {
    for case in fixture_cases() {
        let directory = Path::new(FIXTURES).join(&case).canonicalize().unwrap();
        let output_directory = TempDirectory::create();
        let backend = fs::read_to_string(directory.join("expected.backend")).unwrap();
        let backend = backend.trim_end();
        let expected = destination_names(&directory.join("expected.destinations"));
        if backend == "maildir" {
            for destination in &expected {
                let path = output_directory.0.join(destination);
                fs::create_dir(&path).unwrap();
                for subdirectory in ["tmp", "new", "cur"] {
                    fs::create_dir(path.join(subdirectory)).unwrap();
                }
            }
        } else {
            assert_eq!(backend, "mbox", "fixture: {case}");
        }
        let message = fs::File::open(directory.join("message.eml")).unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_procmail-rs"))
            .args(["filter", "--config"])
            .arg(directory.join("procmail-rs.rc"))
            .args(["--set", &format!("OUT={}", output_directory.0.display())])
            .args(["--set", &format!("CASE={}", directory.display())])
            .args(["--set", "DIALECT=procmail-rs"])
            .current_dir(&directory)
            .stdin(Stdio::from(message))
            .output()
            .unwrap();

        let expected_status = fs::read_to_string(directory.join("expected.status"))
            .unwrap()
            .trim_end()
            .parse::<i32>()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected_status),
            "fixture {case}: {:?}",
            output.stderr
        );
        let mut actual = fs::read_dir(&output_directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(actual, expected, "fixture: {case}");

        let expected_delivery = fs::read(directory.join("expected.delivery")).unwrap();
        for destination in actual {
            let path = output_directory.0.join(&destination);
            if backend == "mbox" {
                let delivered = fs::read(path).unwrap();
                assert_eq!(
                    mbox_payload(&delivered),
                    expected_delivery,
                    "fixture: {case}, destination: {destination}"
                );
            } else {
                assert_eq!(fs::read_dir(path.join("tmp")).unwrap().count(), 0);
                assert_eq!(fs::read_dir(path.join("cur")).unwrap().count(), 0);
                let mut published = fs::read_dir(path.join("new")).unwrap();
                let delivered = fs::read(published.next().unwrap().unwrap().path()).unwrap();
                assert!(published.next().is_none(), "fixture: {case}");
                assert_eq!(
                    delivered, expected_delivery,
                    "fixture: {case}, destination: {destination}"
                );
            }
        }
    }
}

fn mbox_payload(delivery: &[u8]) -> &[u8] {
    let postmark_end = delivery
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("fixture mbox delivery must contain an LF-terminated postmark");
    assert!(
        delivery[..postmark_end].starts_with(b"From MAILER-DAEMON "),
        "fixture mbox delivery must start with the generated postmark"
    );
    &delivery[postmark_end + 1..]
}

fn destination_names(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn fixture_cases() -> Vec<String> {
    let mut cases = fs::read_dir(FIXTURES)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry.file_type().unwrap().is_dir().then(|| {
                entry
                    .file_name()
                    .into_string()
                    .expect("fixture directory name must be UTF-8")
            })
        })
        .collect::<Vec<_>>();
    cases.sort();
    assert!(!cases.is_empty());
    cases
}
