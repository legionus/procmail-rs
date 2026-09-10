// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::fs;
use std::path::{Path, PathBuf};

use super::*;

const MAX_REGRESSION_INPUT: usize = 1024 * 1024;

#[test]
fn replay_saved_fuzz_regressions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fuzz-regressions");
    let mut targets = directory_entries(&root);
    targets.sort();

    for target in targets {
        assert!(
            target.is_dir(),
            "unexpected fuzz regression entry: {}",
            target.display()
        );
        let target_name = target
            .file_name()
            .and_then(|name| name.to_str())
            .expect("fuzz target directory must have a UTF-8 name");
        let mut fixtures = directory_entries(&target);
        fixtures.sort();
        for fixture in fixtures {
            let data = decode_fixture(&fixture);
            replay(target_name, &data);
        }
    }
}

fn directory_entries(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
        .map(|entry| {
            entry
                .expect("cannot read fuzz regression directory entry")
                .path()
        })
        .collect()
}

fn decode_fixture(path: &Path) -> Vec<u8> {
    let encoded_limit = MAX_REGRESSION_INPUT * 2 + 1;
    let metadata = fs::metadata(path)
        .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", path.display()));
    assert!(
        metadata.len() <= encoded_limit as u64,
        "fuzz regression fixture exceeds {MAX_REGRESSION_INPUT} decoded bytes: {}",
        path.display()
    );
    let encoded =
        fs::read(path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let encoded = encoded.strip_suffix(b"\n").unwrap_or(&encoded);
    assert_eq!(
        encoded.len() % 2,
        0,
        "fuzz regression fixture has an incomplete hex byte: {}",
        path.display()
    );

    encoded
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0]).unwrap_or_else(|| invalid_hex(path));
            let low = hex_digit(pair[1]).unwrap_or_else(|| invalid_hex(path));
            (high << 4) | low
        })
        .collect()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn invalid_hex(path: &Path) -> ! {
    panic!(
        "fuzz regression fixture is not hexadecimal: {}",
        path.display()
    )
}

fn replay(target: &str, data: &[u8]) {
    match target {
        "destination-path" => destination_path(data),
        "header-edit" => header_edit(data),
        "message" => message(data),
        "ordered-evaluation" => ordered_evaluation(data),
        "rc" => rc_configuration(data),
        "regex" => regex(data),
        "shell-condition" => check_shell_result(shell_condition(data)),
        "shell-expression" => check_shell_result(shell_expression(data)),
        "shell-pattern" => shell_pattern(data),
        _ => panic!("unknown fuzz regression target: {target}"),
    }
}

fn check_shell_result(result: Option<ShellEvaluationSummary>) {
    if let Some(result) = result {
        assert!(result.output_len <= result.limit);
        assert!(
            result
                .assignment_lengths
                .iter()
                .all(|length| *length <= result.limit)
        );
    }
}
