// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn removes_shortest_and_longest_prefixes() {
    assert_eq!(
        remove(b"a/b/c", b"*/", Edge::Prefix, Selection::Shortest).unwrap(),
        b"b/c"
    );
    assert_eq!(
        remove(b"a/b/c", b"*/", Edge::Prefix, Selection::Longest).unwrap(),
        b"c"
    );
}

#[test]
fn removes_shortest_and_longest_suffixes() {
    assert_eq!(
        remove(b"name.tar.gz", b".*", Edge::Suffix, Selection::Shortest).unwrap(),
        b"name.tar"
    );
    assert_eq!(
        remove(b"name.tar.gz", b".*", Edge::Suffix, Selection::Longest).unwrap(),
        b"name"
    );
}

#[test]
fn supports_question_classes_ranges_negation_and_escaping() {
    for pattern in [b"?[ab][0-9]".as_slice(), b"?[!z][0-9]"] {
        assert_eq!(
            remove(b"xa7tail", pattern, Edge::Prefix, Selection::Shortest).unwrap(),
            b"tail",
            "{pattern:?}"
        );
    }
    assert_eq!(
        remove(b"*tail", b"\\*", Edge::Prefix, Selection::Shortest).unwrap(),
        b"tail"
    );
}

#[test]
fn leaves_the_value_unchanged_without_a_match() {
    assert_eq!(
        remove(b"value", b"x*", Edge::Prefix, Selection::Longest).unwrap(),
        b"value"
    );
}

#[test]
fn shortest_star_selects_the_empty_prefix() {
    assert_eq!(
        remove(b"value", b"*", Edge::Prefix, Selection::Shortest).unwrap(),
        b"value"
    );
    assert_eq!(
        remove(b"value", b"*", Edge::Prefix, Selection::Longest).unwrap(),
        b""
    );
}

#[test]
fn escaped_hyphen_is_literal_inside_a_class() {
    assert_eq!(
        remove(b"-tail", b"[a\\-c]", Edge::Prefix, Selection::Shortest).unwrap(),
        b"tail"
    );
    assert_eq!(
        remove(b"btail", b"[a\\-c]", Edge::Prefix, Selection::Shortest).unwrap(),
        b"btail"
    );
}

#[test]
fn rejects_work_above_the_fixed_step_ceiling() {
    let value = vec![b'x'; MAX_PATTERN_STEPS / 2];
    assert!(matches!(
        remove(&value, b"??", Edge::Prefix, Selection::Longest),
        Err(PatternError::TooComplex { .. })
    ));
}

#[test]
fn accepts_work_through_the_fixed_step_ceiling() {
    let tokens = [Token::Literal(b'z')];
    for steps in [MAX_PATTERN_STEPS - 1, MAX_PATTERN_STEPS] {
        assert_eq!(
            matching_endpoint(
                std::iter::repeat_n(b'x', steps - 1),
                steps - 1,
                &tokens,
                Selection::Longest,
            )
            .unwrap(),
            None,
            "steps {steps}",
        );
    }
    assert!(matches!(
        matching_endpoint(
            std::iter::repeat_n(b'x', MAX_PATTERN_STEPS),
            MAX_PATTERN_STEPS,
            &tokens,
            Selection::Longest,
        ),
        Err(PatternError::TooComplex { .. })
    ));
}

#[test]
fn bounds_aggregate_case_transformation_work() {
    let pattern = vec![b'z'; 1024];
    let limit = MAX_PATTERN_STEPS / (2 * pattern.len());

    assert!(transform_matching_bytes(&vec![b'a'; limit - 1], &pattern, true, |byte| byte).is_ok());
    assert!(transform_matching_bytes(&vec![b'a'; limit], &pattern, true, |byte| byte).is_ok());
    assert!(matches!(
        transform_matching_bytes(&vec![b'a'; limit + 1], &pattern, true, |byte| byte),
        Err(PatternError::TooComplex { .. })
    ));
}
