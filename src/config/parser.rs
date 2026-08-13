// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use regex::bytes::RegexBuilder;

use super::{
    ActionInput, ActionMode, Assignment, CaseMode, ChildStatusMode, Condition, ConditionInput,
    ConditionKind, Config, ContinuationMode, ControlFlow, Destination, MAX_ASSIGNMENT_NAME_LEN,
    MAX_ASSIGNMENT_VALUE_LEN, MAX_CONDITIONS_PER_RECIPE, MAX_PATH_EXPRESSION_LEN,
    MAX_RC_CONDITIONS, MAX_RC_RECIPES, MAX_RC_REGEXES, MAX_RC_SIZE, MAX_RC_STATEMENTS,
    MAX_RECIPE_NESTING_DEPTH, MAX_REGEX_COMPILED_SIZE, MAX_REGEX_PATTERN_LEN, OutputEnding,
    ParseError, PathExpression, Recipe, RecipeAction, RecipeOptions, RegexCondition, Statement,
    VariableSource, WriteErrorMode, variable_policy,
};

pub fn parse(input: &str) -> Result<Config, ParseError> {
    if input.len() > MAX_RC_SIZE {
        return Err(ParseError::new(
            1,
            format!("rc file exceeds the hard limit of {MAX_RC_SIZE} bytes"),
        ));
    }

    let lines: Vec<&str> = input.lines().collect();
    let mut counts = ParseCounts::default();
    let (statements, _) = parse_statements(&lines, 0, 0, &mut counts)?;
    validate_error_predecessors(&statements)?;

    Ok(Config {
        statements,
        initial_variables: Vec::new(),
    })
}

fn validate_error_predecessors(statements: &[Statement]) -> Result<(), ParseError> {
    let mut previous_action: Option<&RecipeAction> = None;
    for statement in statements {
        let Statement::Recipe(recipe) = statement else {
            continue;
        };
        if recipe.options.control == ControlFlow::AfterPreviousError
            && matches!(previous_action, Some(RecipeAction::Block(_)))
        {
            return Err(ParseError::new(
                recipe.line,
                "error flag 'e' after a recipe block is not supported yet",
            ));
        }
        if let RecipeAction::Block(children) = &recipe.action {
            validate_error_predecessors(children)?;
        }
        previous_action = Some(&recipe.action);
    }
    Ok(())
}

#[derive(Default)]
struct ParseCounts {
    statements: usize,
    recipes: usize,
    conditions: usize,
    regexes: usize,
}

fn parse_statements(
    lines: &[&str],
    mut index: usize,
    depth: usize,
    counts: &mut ParseCounts,
) -> Result<(Vec<Statement>, usize), ParseError> {
    let mut statements = Vec::new();
    while index < lines.len() {
        let line_number = index + 1;
        let line = lines[index].trim();

        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }

        if line == "}" {
            if depth == 0 {
                return Err(ParseError::new(
                    line_number,
                    "closing recipe block has no matching opening block",
                ));
            }
            return Ok((statements, index + 1));
        }

        check_statement_limit(counts.statements, line_number)?;

        if line.starts_with(':') {
            check_recipe_limit(counts.recipes, line_number)?;
            counts.recipes = counts
                .recipes
                .checked_add(1)
                .ok_or_else(|| ParseError::new(line_number, "rc recipe count overflows"))?;
            counts.statements = counts
                .statements
                .checked_add(1)
                .ok_or_else(|| ParseError::new(line_number, "rc statement count overflows"))?;
            let (recipe, next) = parse_recipe(
                lines,
                index,
                depth,
                counts.conditions,
                counts.regexes,
                counts,
            )?;
            statements.push(Statement::Recipe(recipe));
            index = next;
            continue;
        }

        if let Some(assignment) = parse_assignment(line, line_number)? {
            if depth > 0 {
                return Err(ParseError::new(
                    line_number,
                    "assignments inside recipe blocks are not supported yet",
                ));
            }
            statements.push(Statement::Assignment(assignment));
            counts.statements = counts
                .statements
                .checked_add(1)
                .ok_or_else(|| ParseError::new(line_number, "rc statement count overflows"))?;
            index += 1;
            continue;
        }

        return Err(ParseError::new(
            line_number,
            "expected an assignment or a recipe beginning with ':0'",
        ));
    }

    if depth > 0 {
        return Err(ParseError::new(
            lines.len().max(1),
            "recipe block has no closing brace",
        ));
    }
    Ok((statements, index))
}

fn check_recipe_limit(count: usize, line: usize) -> Result<(), ParseError> {
    if count >= MAX_RC_RECIPES {
        return Err(ParseError::new(
            line,
            format!("rc recipe count exceeds the hard limit of {MAX_RC_RECIPES}"),
        ));
    }
    Ok(())
}

fn check_statement_limit(count: usize, line: usize) -> Result<(), ParseError> {
    if count >= MAX_RC_STATEMENTS {
        return Err(ParseError::new(
            line,
            format!("rc statement count exceeds the hard limit of {MAX_RC_STATEMENTS}"),
        ));
    }
    Ok(())
}

fn parse_assignment(line: &str, line_number: usize) -> Result<Option<Assignment>, ParseError> {
    let Some((name, value)) = line.split_once('=') else {
        return Ok(None);
    };
    let name = name.trim();
    if name.len() > MAX_ASSIGNMENT_NAME_LEN {
        return Err(ParseError::new(
            line_number,
            format!("assignment name exceeds the hard limit of {MAX_ASSIGNMENT_NAME_LEN} bytes"),
        ));
    }
    if name.is_empty()
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphanumeric() && (index > 0 || !byte.is_ascii_digit())
        })
    {
        return Ok(None);
    }
    let value = strip_comment(value.trim()).trim();
    if value.len() > MAX_ASSIGNMENT_VALUE_LEN {
        return Err(ParseError::new(
            line_number,
            format!("assignment value exceeds the hard limit of {MAX_ASSIGNMENT_VALUE_LEN} bytes"),
        ));
    }
    if matches!(name, "MAILDIR" | "LOGFILE") {
        check_path_length(value, line_number, &format!("{name} path"))?;
    }
    let target = variable_policy(name)
        .assignment_target(VariableSource::RcFile)
        .ok_or_else(|| {
            ParseError::new(
                line_number,
                format!("variable {name} cannot be assigned in an rc file"),
            )
        })?;

    Ok(Some(Assignment {
        line: line_number,
        name: name.to_owned(),
        value: value.to_owned(),
        target,
    }))
}

fn parse_recipe(
    lines: &[&str],
    start: usize,
    depth: usize,
    prior_conditions: usize,
    prior_regexes: usize,
    counts: &mut ParseCounts,
) -> Result<(Recipe, usize), ParseError> {
    let header = lines[start].trim();
    let rest = header
        .strip_prefix(":0")
        .ok_or_else(|| ParseError::new(start + 1, "only ':0' recipes are supported"))?;
    let (options, lock) = parse_recipe_header(rest, start + 1)?;
    let mut conditions = Vec::new();
    let mut regex_count = 0usize;
    let mut index = start + 1;

    while index < lines.len() {
        let line = lines[index].trim();
        if line.is_empty() || line.starts_with('#') {
            index += 1;
            continue;
        }
        if let Some(condition) = line.strip_prefix('*') {
            // Reject excess conditions before parsing can allocate their text
            // or compile a regular expression. The local and file-wide
            // budgets are separate because either shape can make later plan
            // construction disproportionately expensive.
            check_condition_limits(conditions.len(), prior_conditions, index + 1)?;
            let (condition, is_regex) = parse_condition(
                condition,
                index + 1,
                options.case_mode == CaseMode::Sensitive,
                prior_regexes,
                regex_count,
            )?;
            conditions.push(condition);
            regex_count = regex_count
                .checked_add(usize::from(is_regex))
                .ok_or_else(|| ParseError::new(index + 1, "recipe regex count overflows"))?;
            index += 1;
            continue;
        }
        break;
    }

    let action = lines
        .get(index)
        .map(|line| line.trim())
        .ok_or_else(|| ParseError::new(start + 1, "recipe has no action"))?;

    // Charge the parent recipe before descending into a block so nested
    // parsing cannot temporarily hide conditions or regexes from file-wide
    // limits.
    counts.conditions = counts
        .conditions
        .checked_add(conditions.len())
        .ok_or_else(|| ParseError::new(start + 1, "rc condition count overflows"))?;
    counts.regexes = counts
        .regexes
        .checked_add(regex_count)
        .ok_or_else(|| ParseError::new(start + 1, "rc regex count overflows"))?;

    if action.starts_with('|') {
        return Err(ParseError::new(index + 1, "pipe actions are not supported"));
    }
    if action.starts_with('!') {
        return Err(ParseError::new(
            index + 1,
            "forward actions are not supported",
        ));
    }
    if action == "}" {
        return Err(ParseError::new(
            index + 1,
            "closing recipe block has no matching opening block",
        ));
    }
    if action.starts_with(':') {
        return Err(ParseError::new(start + 1, "recipe has no action"));
    }
    if action.is_empty() {
        return Err(ParseError::new(index + 1, "recipe action is empty"));
    }

    let (action, next) = if action == "{" {
        let next_depth = depth
            .checked_add(1)
            .ok_or_else(|| ParseError::new(index + 1, "recipe nesting depth overflows"))?;
        if next_depth > MAX_RECIPE_NESTING_DEPTH {
            return Err(ParseError::new(
                index + 1,
                format!(
                    "recipe nesting depth {next_depth} exceeds the hard limit of {MAX_RECIPE_NESTING_DEPTH}"
                ),
            ));
        }
        if lock.is_some() {
            return Err(ParseError::new(
                start + 1,
                "local lockfiles on recipe blocks are not supported",
            ));
        }
        if options.continuation == ContinuationMode::Continue {
            return Err(ParseError::new(
                start + 1,
                "copy flag 'c' on recipe blocks is not supported yet",
            ));
        }
        let (statements, next) = parse_statements(lines, index + 1, next_depth, counts)?;
        (RecipeAction::Block(statements), next)
    } else if action.starts_with('{') {
        return Err(ParseError::new(
            index + 1,
            "opening recipe block must be a standalone '{' action",
        ));
    } else if let Some(path) = action.strip_prefix("mbox:") {
        (
            RecipeAction::Deliver(Destination::Mbox(PathExpression {
                source: required_path(path, index + 1, "destination path")?,
                base: None,
                line: index + 1,
                runtime_dependent: false,
                expansion: None,
            })),
            index + 1,
        )
    } else if let Some(path) = action.strip_prefix("maildir:") {
        (
            RecipeAction::Deliver(Destination::Maildir(PathExpression {
                source: required_path(path, index + 1, "destination path")?,
                base: None,
                line: index + 1,
                runtime_dependent: false,
                expansion: None,
            })),
            index + 1,
        )
    } else if action.ends_with('/') {
        (
            RecipeAction::Deliver(Destination::Maildir(PathExpression {
                source: required_path(action, index + 1, "destination path")?,
                base: None,
                line: index + 1,
                runtime_dependent: false,
                expansion: None,
            })),
            index + 1,
        )
    } else {
        check_path_length(action, index + 1, "destination path")?;
        return Err(ParseError::new(
            index + 1,
            "destination type is ambiguous; use an explicit maildir: or mbox: prefix, or a trailing '/' for Maildir",
        ));
    };

    let recipe = Recipe {
        line: start + 1,
        action_line: index + 1,
        options,
        lock,
        conditions,
        action,
    };
    Ok((recipe, next))
}

fn check_condition_limits(
    recipe_count: usize,
    prior_conditions: usize,
    line: usize,
) -> Result<(), ParseError> {
    if recipe_count >= MAX_CONDITIONS_PER_RECIPE {
        return Err(ParseError::new(
            line,
            format!("recipe condition count exceeds the hard limit of {MAX_CONDITIONS_PER_RECIPE}"),
        ));
    }
    let total = prior_conditions
        .checked_add(recipe_count)
        .ok_or_else(|| ParseError::new(line, "rc condition count overflows"))?;
    if total >= MAX_RC_CONDITIONS {
        return Err(ParseError::new(
            line,
            format!("rc condition count exceeds the hard limit of {MAX_RC_CONDITIONS}"),
        ));
    }
    Ok(())
}

fn parse_recipe_header(
    rest: &str,
    line: usize,
) -> Result<(RecipeOptions, Option<String>), ParseError> {
    let rest = strip_comment(rest).trim();
    let (flag_text, lock) = match rest.split_once(':') {
        Some((flags, lock)) => {
            let lock = lock.trim();
            check_path_length(lock, line, "lockfile path")?;
            (flags.trim(), Some(lock.to_owned()))
        }
        None => (rest, None),
    };

    if !flag_text.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(ParseError::new(line, "invalid recipe flags"));
    }
    if let Some(flag) = flag_text
        .chars()
        .find(|flag| !matches!(flag, 'H' | 'B' | 'D' | 'c' | 'A' | 'a' | 'E' | 'e'))
    {
        return Err(ParseError::new(
            line,
            format!("recipe flag '{flag}' is not supported yet"),
        ));
    }
    let control_flags = ['A', 'a', 'E', 'e']
        .into_iter()
        .filter(|flag| flag_text.contains(*flag))
        .collect::<Vec<_>>();
    if control_flags.len() > 1 {
        return Err(ParseError::new(
            line,
            format!(
                "recipe control flags '{}' and '{}' cannot be combined",
                control_flags[0], control_flags[1]
            ),
        ));
    }

    let condition_input = match (flag_text.contains('H'), flag_text.contains('B')) {
        (false, true) => ConditionInput::Body,
        (true, true) => ConditionInput::Message,
        _ => ConditionInput::Headers,
    };
    let control = if flag_text.contains('A') {
        ControlFlow::AfterChainMatch
    } else if flag_text.contains('a') {
        ControlFlow::AfterPreviousSuccess
    } else if flag_text.contains('E') {
        ControlFlow::Else
    } else if flag_text.contains('e') {
        ControlFlow::AfterPreviousError
    } else {
        ControlFlow::Independent
    };
    let continuation = if flag_text.contains('c') {
        ContinuationMode::Continue
    } else {
        ContinuationMode::Stop
    };
    Ok((
        RecipeOptions {
            condition_input,
            case_mode: if flag_text.contains('D') {
                CaseMode::Sensitive
            } else {
                CaseMode::Insensitive
            },
            control,
            action_input: ActionInput::Message,
            action_mode: ActionMode::Deliver,
            continuation,
            child_status: ChildStatusMode::Ignore,
            write_errors: WriteErrorMode::Fail,
            output_ending: OutputEnding::Normalize,
        },
        lock,
    ))
}

fn parse_condition(
    input: &str,
    line: usize,
    case_sensitive: bool,
    prior_regexes: usize,
    recipe_regexes: usize,
) -> Result<(Condition, bool), ParseError> {
    let mut input = input.trim();
    let mut negated = false;
    while let Some(rest) = input.strip_prefix('!') {
        negated = !negated;
        input = rest.trim_start();
    }

    if input.is_empty() {
        return Err(ParseError::new(line, "condition is empty"));
    }

    let (kind, is_regex) = if let Some(value) = input.strip_prefix('<') {
        (ConditionKind::SmallerThan(parse_size(value, line)?), false)
    } else if let Some(value) = input.strip_prefix('>') {
        (ConditionKind::LargerThan(parse_size(value, line)?), false)
    } else {
        let total_regexes = prior_regexes
            .checked_add(recipe_regexes)
            .ok_or_else(|| ParseError::new(line, "rc regex count overflows"))?;
        if total_regexes >= MAX_RC_REGEXES {
            return Err(ParseError::new(
                line,
                format!("rc regex count exceeds the hard limit of {MAX_RC_REGEXES}"),
            ));
        }
        if input.len() > MAX_REGEX_PATTERN_LEN {
            return Err(ParseError::new(
                line,
                format!(
                    "regular expression exceeds the hard limit of {MAX_REGEX_PATTERN_LEN} bytes"
                ),
            ));
        }
        let compiled = build_regex(input, case_sensitive).map_err(|error| {
            ParseError::new(line, format!("invalid regular expression: {error}"))
        })?;
        (
            ConditionKind::Regex(RegexCondition {
                pattern: input.to_owned(),
                compiled,
            }),
            true,
        )
    };

    Ok((
        Condition {
            line,
            negated,
            kind,
        },
        is_regex,
    ))
}

fn parse_size(input: &str, line: usize) -> Result<usize, ParseError> {
    input
        .trim()
        .parse()
        .map_err(|_| ParseError::new(line, "size condition requires a non-negative integer"))
}

pub(crate) fn build_regex(
    pattern: &str,
    case_sensitive: bool,
) -> Result<regex::bytes::Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .multi_line(true)
        .unicode(false)
        .size_limit(MAX_REGEX_COMPILED_SIZE)
        .build()
}

fn required_path(path: &str, line: usize, description: &str) -> Result<String, ParseError> {
    let path = path.trim();
    if path.is_empty() {
        Err(ParseError::new(line, "destination path is empty"))
    } else {
        check_path_length(path, line, description)?;
        Ok(path.to_owned())
    }
}

fn check_path_length(path: &str, line: usize, description: &str) -> Result<(), ParseError> {
    if path.len() > MAX_PATH_EXPRESSION_LEN {
        return Err(ParseError::new(
            line,
            format!("{description} exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes"),
        ));
    }
    Ok(())
}

fn strip_comment(value: &str) -> &str {
    value.split_once('#').map_or(value, |(value, _)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_assignment_and_recipe() {
        let config =
            parse("MAILDIR=/srv/mail\n\n:0 Bc:\n* ! ^Subject: spam\nmaildir:inbox\n").unwrap();

        assert_eq!(config.statements.len(), 2);
        assert_eq!(
            config.statements[0],
            Statement::Assignment(Assignment {
                line: 1,
                name: "MAILDIR".into(),
                value: "/srv/mail".into(),
                target: crate::config::AssignmentTarget::Maildir,
            })
        );
        assert_eq!(
            config.statements[1],
            Statement::Recipe(Recipe {
                line: 3,
                action_line: 5,
                options: RecipeOptions {
                    condition_input: ConditionInput::Body,
                    case_mode: CaseMode::Insensitive,
                    control: ControlFlow::Independent,
                    action_input: ActionInput::Message,
                    action_mode: ActionMode::Deliver,
                    continuation: ContinuationMode::Continue,
                    child_status: ChildStatusMode::Ignore,
                    write_errors: WriteErrorMode::Fail,
                    output_ending: OutputEnding::Normalize,
                },
                lock: Some(String::new()),
                conditions: vec![Condition {
                    line: 4,
                    negated: true,
                    kind: ConditionKind::Regex(RegexCondition {
                        pattern: "^Subject: spam".into(),
                        compiled: build_regex("^Subject: spam", false).unwrap(),
                    }),
                }],
                action: RecipeAction::Deliver(Destination::Maildir("inbox".into())),
            })
        );
    }

    #[test]
    fn trailing_slash_selects_maildir() {
        let config = parse(":0\ninbox/\n").unwrap();
        let Statement::Recipe(recipe) = &config.statements[0] else {
            panic!("expected recipe");
        };

        assert_eq!(
            recipe.action,
            RecipeAction::Deliver(Destination::Maildir("inbox/".into()))
        );
    }

    #[test]
    fn rejects_destination_without_a_stable_type() {
        let error = parse(":0\ninbox\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            "destination type is ambiguous; use an explicit maildir: or mbox: prefix, or a trailing '/' for Maildir"
        );
    }

    #[test]
    fn classifies_user_assignments_for_future_expansion() {
        let config = parse("USER_VALUE=text\n").unwrap();
        let Statement::Assignment(assignment) = &config.statements[0] else {
            panic!("expected assignment");
        };

        assert_eq!(assignment.target, crate::config::AssignmentTarget::User);
    }

    #[test]
    fn rejects_runtime_variable_assignment() {
        let error = parse("LASTFOLDER=forged\n").unwrap_err();

        assert_eq!(error.line, 1);
        assert_eq!(
            error.message,
            "variable LASTFOLDER cannot be assigned in an rc file"
        );
    }

    #[test]
    fn rejects_pipe_action_explicitly() {
        let error = parse(":0\n| command\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(error.message, "pipe actions are not supported");
    }

    #[test]
    fn rejects_recipe_without_action() {
        let error = parse(":0\n* ^Subject:\n").unwrap_err();

        assert_eq!(error.line, 1);
        assert_eq!(error.message, "recipe has no action");
    }

    #[test]
    fn rejects_unsupported_flag() {
        let error = parse(":0 f\ninbox/\n").unwrap_err();

        assert_eq!(error.line, 1);
        assert_eq!(error.message, "recipe flag 'f' is not supported yet");
    }

    #[test]
    fn accepts_chain_flags_but_rejects_their_combination() {
        assert!(parse(":0 A\nmaildir:after-match\n").is_ok());
        assert!(parse(":0 a\nmaildir:after-success\n").is_ok());
        assert!(parse(":0 E\nmaildir:otherwise\n").is_ok());
        assert!(parse(":0 e\nmaildir:on-error\n").is_ok());

        let error = parse(":0 Aa\nmaildir:ambiguous\n").unwrap_err();
        assert_eq!(error.line, 1);
        assert_eq!(
            error.message,
            "recipe control flags 'A' and 'a' cannot be combined"
        );

        let error = parse(":0 Ee\nmaildir:ambiguous\n").unwrap_err();
        assert_eq!(
            error.message,
            "recipe control flags 'E' and 'e' cannot be combined"
        );
    }

    #[test]
    fn rejects_error_handler_after_a_block() {
        let error = parse(":0\n{\n:0\nmaildir:child\n}\n:0 e\nmaildir:fallback\n").unwrap_err();

        assert_eq!(error.line, 6);
        assert_eq!(
            error.message,
            "error flag 'e' after a recipe block is not supported yet"
        );
    }

    #[test]
    fn enforces_recipe_nesting_depth_at_the_boundary() {
        fn nested(depth: usize) -> String {
            let mut source = ":0\n{\n".repeat(depth);
            source.push_str(":0\ninbox/\n");
            source.push_str(&"}\n".repeat(depth));
            source
        }

        assert_eq!(MAX_RECIPE_NESTING_DEPTH, 64);
        for depth in [
            MAX_RECIPE_NESTING_DEPTH - 1,
            MAX_RECIPE_NESTING_DEPTH,
            MAX_RECIPE_NESTING_DEPTH + 1,
        ] {
            let result = parse(&nested(depth));
            if depth <= MAX_RECIPE_NESTING_DEPTH {
                assert!(result.is_ok(), "depth {depth} must be accepted");
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, MAX_RECIPE_NESTING_DEPTH * 2 + 2);
                assert_eq!(
                    error.message,
                    format!(
                        "recipe nesting depth {depth} exceeds the hard limit of {MAX_RECIPE_NESTING_DEPTH}"
                    )
                );
            }
        }
    }

    #[test]
    fn rejects_unclosed_recipe_block() {
        let error = parse(":0\n{\n:0\ninbox/\n").unwrap_err();

        assert_eq!(error.line, 4);
        assert_eq!(error.message, "recipe block has no closing brace");
    }

    #[test]
    fn rejects_assignment_inside_recipe_block() {
        let error = parse(":0\n{\nBOX=inbox\n}\n").unwrap_err();

        assert_eq!(error.line, 3);
        assert_eq!(
            error.message,
            "assignments inside recipe blocks are not supported yet"
        );
    }

    #[test]
    fn parses_typed_nested_block_actions() {
        let config = parse(":0\n* ^List-Id:\n{\n:0\nmaildir:list\n}\n").unwrap();
        let [Statement::Recipe(parent)] = config.statements.as_slice() else {
            panic!("expected one parent recipe");
        };
        let RecipeAction::Block(children) = &parent.action else {
            panic!("expected block action");
        };
        let [Statement::Recipe(child)] = children.as_slice() else {
            panic!("expected one child recipe");
        };
        assert_eq!(
            child.action,
            RecipeAction::Deliver(Destination::Maildir("list".into()))
        );
    }

    #[test]
    fn rejects_unmatched_closing_recipe_block() {
        let error = parse(":0\n}\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            "closing recipe block has no matching opening block"
        );
    }

    #[test]
    fn rejects_invalid_regex_at_condition_line() {
        let error = parse(":0\n* [unterminated\ninbox/\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert!(error.message.starts_with("invalid regular expression:"));
    }

    #[test]
    fn enforces_regex_pattern_length_at_the_boundary() {
        for length in [
            MAX_REGEX_PATTERN_LEN - 1,
            MAX_REGEX_PATTERN_LEN,
            MAX_REGEX_PATTERN_LEN + 1,
        ] {
            let source = format!(":0\n* {}\ninbox/\n", "a".repeat(length));
            let result = parse(&source);

            if length <= MAX_REGEX_PATTERN_LEN {
                let config = result.unwrap();
                let Statement::Recipe(recipe) = &config.statements[0] else {
                    panic!("expected recipe");
                };
                let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
                    panic!("expected regular expression");
                };
                assert_eq!(regex.pattern().len(), length);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 2);
                assert_eq!(
                    error.message,
                    format!(
                        "regular expression exceeds the hard limit of {MAX_REGEX_PATTERN_LEN} bytes"
                    )
                );
            }
        }
    }

    #[test]
    fn size_conditions_do_not_use_regex_pattern_limit() {
        let value = "9".repeat(MAX_REGEX_PATTERN_LEN + 1);
        let error = parse(&format!(":0\n* < {value}\ninbox/\n")).unwrap_err();

        assert_eq!(error.line, 2);
        assert_eq!(
            error.message,
            "size condition requires a non-negative integer"
        );
    }

    #[test]
    fn rejects_regex_above_compiled_size_limit() {
        let error = parse(":0\n* (?:a.){65535}\ninbox/\n").unwrap_err();

        assert_eq!(error.line, 2);
        assert!(error.message.starts_with("invalid regular expression:"));
        assert!(error.message.contains("Compiled regex exceeds size limit"));
    }

    #[test]
    fn enforces_regex_count_at_the_boundary() {
        for count in [MAX_RC_REGEXES - 1, MAX_RC_REGEXES, MAX_RC_REGEXES + 1] {
            let source = format!(":0\n{}inbox/\n", "* pattern\n".repeat(count));
            let result = parse(&source);

            if count <= MAX_RC_REGEXES {
                assert!(result.is_ok());
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, MAX_RC_REGEXES + 2);
                assert_eq!(
                    error.message,
                    format!("rc regex count exceeds the hard limit of {MAX_RC_REGEXES}")
                );
            }
        }
    }

    #[test]
    fn size_conditions_do_not_count_as_regexes() {
        let mut source = format!(":0\n{}", "* pattern\n".repeat(MAX_RC_REGEXES));
        source.push_str(&"* < 100\n".repeat(MAX_CONDITIONS_PER_RECIPE - MAX_RC_REGEXES));
        source.push_str("inbox/\n");

        assert!(parse(&source).is_ok());
    }

    #[test]
    fn accumulates_regex_count_across_recipes() {
        let mut source = ":0 c\n* pattern\ninbox/\n".repeat(MAX_RC_REGEXES);
        source.push_str(":0\n* excess\ninbox/\n");

        let error = parse(&source).unwrap_err();

        assert_eq!(error.line, MAX_RC_REGEXES * 3 + 2);
        assert_eq!(
            error.message,
            format!("rc regex count exceeds the hard limit of {MAX_RC_REGEXES}")
        );
    }

    #[test]
    fn rejects_oversized_source_before_splitting_lines() {
        for length in [MAX_RC_SIZE - 1, MAX_RC_SIZE, MAX_RC_SIZE + 1] {
            let source = format!("#{}", "x".repeat(length - 1));
            let result = parse(&source);

            if length <= MAX_RC_SIZE {
                assert!(result.is_ok());
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 1);
                assert_eq!(
                    error.message,
                    format!("rc file exceeds the hard limit of {MAX_RC_SIZE} bytes")
                );
            }
        }
    }

    #[test]
    fn enforces_assignment_name_length_at_the_boundary() {
        for length in [
            MAX_ASSIGNMENT_NAME_LEN - 1,
            MAX_ASSIGNMENT_NAME_LEN,
            MAX_ASSIGNMENT_NAME_LEN + 1,
        ] {
            let source = format!("{}=value\n", "A".repeat(length));
            let result = parse(&source);

            if length <= MAX_ASSIGNMENT_NAME_LEN {
                let config = result.unwrap();
                let Statement::Assignment(assignment) = &config.statements[0] else {
                    panic!("expected assignment");
                };
                assert_eq!(assignment.name.len(), length);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 1);
                assert_eq!(
                    error.message,
                    format!(
                        "assignment name exceeds the hard limit of {MAX_ASSIGNMENT_NAME_LEN} bytes"
                    )
                );
            }
        }
    }

    #[test]
    fn enforces_assignment_value_length_at_the_boundary() {
        for length in [
            MAX_ASSIGNMENT_VALUE_LEN - 1,
            MAX_ASSIGNMENT_VALUE_LEN,
            MAX_ASSIGNMENT_VALUE_LEN + 1,
        ] {
            let source = format!("VALUE={}\n", "x".repeat(length));
            let result = parse(&source);

            if length <= MAX_ASSIGNMENT_VALUE_LEN {
                let config = result.unwrap();
                let Statement::Assignment(assignment) = &config.statements[0] else {
                    panic!("expected assignment");
                };
                assert_eq!(assignment.value.len(), length);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 1);
                assert_eq!(
                    error.message,
                    format!(
                        "assignment value exceeds the hard limit of {MAX_ASSIGNMENT_VALUE_LEN} bytes"
                    )
                );
            }
        }
    }

    #[test]
    fn excludes_assignment_formatting_from_value_length() {
        let source = format!(
            "VALUE=  {}  # ignored\n",
            "x".repeat(MAX_ASSIGNMENT_VALUE_LEN)
        );
        let config = parse(&source).unwrap();
        let Statement::Assignment(assignment) = &config.statements[0] else {
            panic!("expected assignment");
        };

        assert_eq!(assignment.value.len(), MAX_ASSIGNMENT_VALUE_LEN);
    }

    #[test]
    fn enforces_destination_path_length_at_the_boundary() {
        for length in [
            MAX_PATH_EXPRESSION_LEN - 1,
            MAX_PATH_EXPRESSION_LEN,
            MAX_PATH_EXPRESSION_LEN + 1,
        ] {
            let source = format!(":0\nmaildir:{}\n", "x".repeat(length));
            let result = parse(&source);

            if length <= MAX_PATH_EXPRESSION_LEN {
                let config = result.unwrap();
                let Statement::Recipe(recipe) = &config.statements[0] else {
                    panic!("expected recipe");
                };
                let RecipeAction::Deliver(Destination::Maildir(path)) = &recipe.action else {
                    panic!("expected Maildir destination");
                };
                assert_eq!(path.source().len(), length);
            } else {
                assert_path_limit_error(result.unwrap_err(), 2, "destination path");
            }
        }
    }

    #[test]
    fn applies_path_limit_to_every_destination_form() {
        let oversized = "x".repeat(MAX_PATH_EXPRESSION_LEN + 1);
        for action in [
            format!("mbox:{oversized}"),
            oversized.clone(),
            format!("{oversized}/"),
        ] {
            let error = parse(&format!(":0\n{action}\n")).unwrap_err();
            assert_path_limit_error(error, 2, "destination path");
        }
    }

    #[test]
    fn enforces_lockfile_path_length_at_the_boundary() {
        for length in [
            MAX_PATH_EXPRESSION_LEN - 1,
            MAX_PATH_EXPRESSION_LEN,
            MAX_PATH_EXPRESSION_LEN + 1,
        ] {
            let source = format!(":0 :{}\ninbox/\n", "x".repeat(length));
            let result = parse(&source);

            if length <= MAX_PATH_EXPRESSION_LEN {
                let config = result.unwrap();
                let Statement::Recipe(recipe) = &config.statements[0] else {
                    panic!("expected recipe");
                };
                assert_eq!(recipe.lock.as_ref().unwrap().len(), length);
            } else {
                assert_path_limit_error(result.unwrap_err(), 1, "lockfile path");
            }
        }
    }

    #[test]
    fn applies_path_limit_to_maildir_assignment() {
        let source = format!("MAILDIR={}\n", "x".repeat(MAX_PATH_EXPRESSION_LEN + 1));
        let error = parse(&source).unwrap_err();

        assert_path_limit_error(error, 1, "MAILDIR path");
    }

    fn assert_path_limit_error(error: ParseError, line: usize, description: &str) {
        assert_eq!(error.line, line);
        assert_eq!(
            error.message,
            format!("{description} exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes")
        );
    }

    #[test]
    fn enforces_statement_count_at_the_boundary() {
        for count in [
            MAX_RC_STATEMENTS - 1,
            MAX_RC_STATEMENTS,
            MAX_RC_STATEMENTS + 1,
        ] {
            let source = "A=\n".repeat(count);
            let result = parse(&source);

            if count <= MAX_RC_STATEMENTS {
                assert_eq!(result.unwrap().statements.len(), count);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, MAX_RC_STATEMENTS + 1);
                assert_eq!(
                    error.message,
                    format!("rc statement count exceeds the hard limit of {MAX_RC_STATEMENTS}")
                );
            }
        }
    }

    #[test]
    fn comments_and_blank_lines_do_not_count_as_statements() {
        let mut source = "A=\n".repeat(MAX_RC_STATEMENTS);
        source.push_str("# comment\n\n");

        assert_eq!(parse(&source).unwrap().statements.len(), MAX_RC_STATEMENTS);
    }

    #[test]
    fn enforces_recipe_count_at_the_boundary() {
        for count in [MAX_RC_RECIPES - 1, MAX_RC_RECIPES, MAX_RC_RECIPES + 1] {
            let source = ":0\ninbox/\n".repeat(count);
            let result = parse(&source);

            if count <= MAX_RC_RECIPES {
                assert_eq!(result.unwrap().statements.len(), count);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, 2 * MAX_RC_RECIPES + 1);
                assert_eq!(
                    error.message,
                    format!("rc recipe count exceeds the hard limit of {MAX_RC_RECIPES}")
                );
            }
        }
    }

    #[test]
    fn assignments_do_not_count_as_recipes() {
        let mut source = ":0\ninbox/\n".repeat(MAX_RC_RECIPES);
        source.push_str("MAILDIR=mail\n");

        assert_eq!(parse(&source).unwrap().statements.len(), MAX_RC_RECIPES + 1);
    }

    #[test]
    fn enforces_conditions_per_recipe_at_the_boundary() {
        for count in [
            MAX_CONDITIONS_PER_RECIPE - 1,
            MAX_CONDITIONS_PER_RECIPE,
            MAX_CONDITIONS_PER_RECIPE + 1,
        ] {
            let source = format!(":0\n{}inbox/\n", "* < 1\n".repeat(count));
            let result = parse(&source);

            if count <= MAX_CONDITIONS_PER_RECIPE {
                let config = result.unwrap();
                let Statement::Recipe(recipe) = &config.statements[0] else {
                    panic!("expected recipe");
                };
                assert_eq!(recipe.conditions.len(), count);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.line, MAX_CONDITIONS_PER_RECIPE + 2);
                assert_eq!(
                    error.message,
                    format!(
                        "recipe condition count exceeds the hard limit of {MAX_CONDITIONS_PER_RECIPE}"
                    )
                );
            }
        }
    }

    #[test]
    fn enforces_total_condition_count_at_the_boundary() {
        for count in [
            MAX_RC_CONDITIONS - 1,
            MAX_RC_CONDITIONS,
            MAX_RC_CONDITIONS + 1,
        ] {
            let source = config_with_conditions(count);
            let result = parse(&source);

            if count <= MAX_RC_CONDITIONS {
                assert!(result.is_ok());
            } else {
                let error = result.unwrap_err();
                let full_recipes = MAX_RC_CONDITIONS / MAX_CONDITIONS_PER_RECIPE;
                let expected_line = full_recipes * (MAX_CONDITIONS_PER_RECIPE + 2) + 2;
                assert_eq!(error.line, expected_line);
                assert_eq!(
                    error.message,
                    format!("rc condition count exceeds the hard limit of {MAX_RC_CONDITIONS}")
                );
            }
        }
    }

    fn config_with_conditions(mut count: usize) -> String {
        let mut source = String::new();
        while count > 0 {
            let recipe_conditions = count.min(MAX_CONDITIONS_PER_RECIPE);
            source.push_str(":0\n");
            source.push_str(&"* < 1\n".repeat(recipe_conditions));
            source.push_str("inbox/\n");
            count -= recipe_conditions;
        }
        source
    }
}
