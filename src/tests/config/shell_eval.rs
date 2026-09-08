// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeMap;

use super::*;

#[derive(Debug, PartialEq, Eq)]
enum TestError {
    Missing(String),
    Required(String),
    Unsupported(UnsupportedPart),
    Depth,
    Overflow,
    Length(BoundedBytesError),
    Pattern(PatternError),
}

struct TestContext {
    values: BTreeMap<String, Vec<u8>>,
}

impl EvaluationContext for TestContext {
    type Error = TestError;

    fn depth_mode(&self) -> EvaluationDepth {
        EvaluationDepth::None
    }

    fn variable(&mut self, name: &str) -> Result<Option<VariableValue>, Self::Error> {
        Ok(self.values.get(name).map(|bytes| VariableValue {
            bytes: bytes.clone(),
            depth: 0,
        }))
    }

    fn command(&mut self, _: &str, _: usize) -> Result<Vec<u8>, Self::Error> {
        Err(self.unsupported_part(UnsupportedPart::Command))
    }

    fn regex_quoted(&mut self, _: &str, _: usize) -> Result<Vec<u8>, Self::Error> {
        Err(self.unsupported_part(UnsupportedPart::RegexQuotedVariable))
    }

    fn missing_variable(&self, name: &str) -> Self::Error {
        TestError::Missing(name.to_owned())
    }

    fn required_parameter(&self, name: &str) -> Self::Error {
        TestError::Required(name.to_owned())
    }

    fn pattern_error(&self, error: PatternError) -> Self::Error {
        TestError::Pattern(error)
    }

    fn unsupported_part(&self, part: UnsupportedPart) -> Self::Error {
        TestError::Unsupported(part)
    }

    fn depth_exceeded(&self) -> Self::Error {
        TestError::Depth
    }

    fn depth_overflow(&self) -> Self::Error {
        TestError::Overflow
    }

    fn length_error(&self, error: BoundedBytesError, _: usize, _: usize) -> Self::Error {
        TestError::Length(error)
    }
}

fn literal(text: &str) -> ShellExpression {
    ShellExpression {
        parts: vec![ShellPart::Literal(text.to_owned())],
    }
}

#[test]
fn selects_defaults_only_for_missing_or_empty_values() {
    let expression = ShellExpression {
        parts: vec![
            ShellPart::Variable {
                name: "SET".to_owned(),
                operation: ParameterOperation::DefaultIfUnsetOrEmpty(literal("wrong")),
            },
            ShellPart::Variable {
                name: "EMPTY".to_owned(),
                operation: ParameterOperation::DefaultIfUnsetOrEmpty(literal("empty-default")),
            },
            ShellPart::Variable {
                name: "MISSING".to_owned(),
                operation: ParameterOperation::DefaultIfUnsetOrEmpty(literal("missing-default")),
            },
        ],
    };
    let mut context = TestContext {
        values: BTreeMap::from([
            ("SET".to_owned(), b"set".to_vec()),
            ("EMPTY".to_owned(), Vec::new()),
        ]),
    };

    let result = evaluate(&expression, 40, &mut context).unwrap();

    assert_eq!(result.bytes, b"setempty-defaultmissing-default");
}

#[test]
fn accounts_for_literal_prefix_before_evaluating_a_default() {
    let expression = ShellExpression {
        parts: vec![
            ShellPart::Literal("ab".to_owned()),
            ShellPart::Variable {
                name: "MISSING".to_owned(),
                operation: ParameterOperation::DefaultIfUnsetOrEmpty(literal("cde")),
            },
        ],
    };
    let mut context = TestContext {
        values: BTreeMap::new(),
    };

    assert_eq!(
        evaluate(&expression, 5, &mut context).unwrap().bytes,
        b"abcde"
    );
    assert_eq!(
        evaluate(&expression, 4, &mut context),
        Err(TestError::Length(BoundedBytesError::LimitExceeded {
            attempted: 3,
        }))
    );
}

#[test]
fn rejects_parts_disabled_by_the_context() {
    let mut context = TestContext {
        values: BTreeMap::new(),
    };
    let command = ShellExpression {
        parts: vec![ShellPart::Command("printf value".to_owned())],
    };
    let quoted = ShellExpression {
        parts: vec![ShellPart::RegexQuotedVariable("VALUE".to_owned())],
    };

    assert_eq!(
        evaluate(&command, 32, &mut context),
        Err(TestError::Unsupported(UnsupportedPart::Command))
    );
    assert_eq!(
        evaluate(&quoted, 32, &mut context),
        Err(TestError::Unsupported(UnsupportedPart::RegexQuotedVariable))
    );
}

#[test]
fn stages_assignment_for_later_parts_of_the_same_expression() {
    let expression = ShellExpression {
        parts: vec![
            ShellPart::Variable {
                name: "VALUE".to_owned(),
                operation: ParameterOperation::AssignIfUnsetOrEmpty(literal("assigned")),
            },
            ShellPart::Literal("/".to_owned()),
            ShellPart::Variable {
                name: "VALUE".to_owned(),
                operation: ParameterOperation::Value,
            },
        ],
    };
    let mut context = TestContext {
        values: BTreeMap::new(),
    };

    let result = evaluate(&expression, 32, &mut context).unwrap();

    assert_eq!(result.bytes, b"assigned/assigned");
    assert_eq!(
        result.assignments,
        [("VALUE".to_owned(), b"assigned".to_vec())]
    );
    assert!(!context.values.contains_key("VALUE"));
}

#[test]
fn discards_staged_assignments_when_later_evaluation_fails() {
    let expression = ShellExpression {
        parts: vec![
            ShellPart::Variable {
                name: "VALUE".to_owned(),
                operation: ParameterOperation::AssignIfUnsetOrEmpty(literal("assigned")),
            },
            ShellPart::Variable {
                name: "MISSING".to_owned(),
                operation: ParameterOperation::Value,
            },
        ],
    };
    let mut context = TestContext {
        values: BTreeMap::new(),
    };

    assert_eq!(
        evaluate(&expression, 32, &mut context),
        Err(TestError::Missing("MISSING".to_owned()))
    );
    assert!(!context.values.contains_key("VALUE"));
}

#[test]
fn required_parameter_does_not_evaluate_its_diagnostic_word() {
    let expression = ShellExpression {
        parts: vec![ShellPart::Variable {
            name: "VALUE".to_owned(),
            operation: ParameterOperation::ErrorIfUnsetOrEmpty(ShellExpression {
                parts: vec![ShellPart::Command("must not run".to_owned())],
            }),
        }],
    };
    let mut context = TestContext {
        values: BTreeMap::new(),
    };

    assert_eq!(
        evaluate(&expression, 32, &mut context),
        Err(TestError::Required("VALUE".to_owned()))
    );

    context.values.insert("VALUE".to_owned(), Vec::new());
    assert_eq!(
        evaluate(&expression, 32, &mut context),
        Err(TestError::Required("VALUE".to_owned()))
    );

    context
        .values
        .insert("VALUE".to_owned(), b"present".to_vec());
    let result = evaluate(&expression, 32, &mut context).unwrap();
    assert_eq!(result.bytes, b"present");
    assert!(result.assignments.is_empty());
}

#[test]
fn length_and_pattern_removal_operate_on_bytes() {
    let expression = ShellExpression {
        parts: vec![
            ShellPart::Variable {
                name: "VALUE".to_owned(),
                operation: ParameterOperation::Length,
            },
            ShellPart::Literal(":".to_owned()),
            ShellPart::Variable {
                name: "VALUE".to_owned(),
                operation: ParameterOperation::RemoveSuffix {
                    pattern: literal(".*"),
                    longest: false,
                },
            },
        ],
    };
    let mut context = TestContext {
        values: BTreeMap::from([("VALUE".to_owned(), b"\xff.bin".to_vec())]),
    };

    assert_eq!(
        evaluate(&expression, 32, &mut context).unwrap().bytes,
        b"5:\xff"
    );
}
