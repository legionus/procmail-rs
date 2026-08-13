// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeSet;
use std::fs;
use std::io::Cursor;
use std::path::Path;

use procmail_rs::config::{self, Destination};
use procmail_rs::eval::{Delivery, Outcome, evaluate};
use procmail_rs::limits::MessageLimits;
use procmail_rs::message::Message;

const FIXTURES: &str = "tests/fixtures/differential_eval";

#[derive(Default)]
struct Recorder {
    selected: Vec<String>,
    failures: BTreeSet<String>,
}

impl Delivery for Recorder {
    fn deliver(&mut self, destination: &Destination, _: &Message) -> Result<(), String> {
        let name = Path::new(destination.path())
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "fixture destination has no UTF-8 basename".to_owned())?;
        if self.failures.contains(name) {
            return Err("injected reference action failure".to_owned());
        }
        self.selected.push(name.to_owned());
        Ok(())
    }
}

#[test]
fn supported_milestone_7_behavior_matches_reference_procmail() {
    for case in fixture_cases() {
        let directory = Path::new(FIXTURES).join(&case);
        let source = fs::read_to_string(directory.join("procmail-rs.rc")).unwrap();
        let message = fs::read(directory.join("message.eml")).unwrap();
        let expected = fs::read_to_string(directory.join("expected.destinations"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let config = config::parse(&source).unwrap().expand().unwrap();
        let failures = fs::read_to_string(directory.join("fail.destinations"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        let mut recorder = Recorder {
            failures,
            ..Recorder::default()
        };

        let message =
            Message::read_from(&mut Cursor::new(message), MessageLimits::default()).unwrap();
        let outcome = evaluate(&config, &message, &mut recorder).unwrap();

        assert_eq!(recorder.selected, expected, "fixture: {case}");
        assert!(
            matches!(outcome, Outcome::Delivered { .. }),
            "fixture did not deliver the original: {case}"
        );
    }
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
