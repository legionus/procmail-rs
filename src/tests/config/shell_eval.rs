// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use std::collections::BTreeMap;

use super::*;

#[derive(Debug, PartialEq, Eq)]
enum TestError {
    Missing(String),
    Unsupported(UnsupportedPart),
    Depth,
    Overflow,
    Length(BoundedBytesError),
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
