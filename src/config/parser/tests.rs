// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

fn parse_wide(input: &str) -> Result<Config, ParseError> {
    let mut state = RcParseState::default();
    state.limits.linebuf = super::super::MAX_LINEBUF;
    parse_with_state(input, &mut state)
}
use crate::config::MAX_SHELL_SETTING_LEN;

#[test]
fn parses_assignment_and_recipe() {
    let config = parse("MAILDIR=/srv/mail\n\n:0 Bc:\n* ! ^Subject: spam\nmaildir:inbox\n").unwrap();

    assert_eq!(config.statements.len(), 2);
    assert_eq!(
        config.statements[0],
        Statement::Assignment(Assignment {
            line: 1,
            name: "MAILDIR".into(),
            value: "/srv/mail".into(),
            target: crate::config::AssignmentTarget::Maildir,
            expansion: None,
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
            lock: Some(String::new().into()),
            conditions: vec![Condition {
                line: 4,
                negated: true,
                kind: ConditionKind::Regex(RegexCondition {
                    pattern: "^Subject: spam".into(),
                    compiled: build_regex("^Subject: spam", false).unwrap(),
                    match_capture: None,
                    capture_indexes: Vec::new(),
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
fn rejects_known_unsupported_procmail_variables() {
    for name in super::super::UNSUPPORTED_PROCMAIL_VARIABLES {
        let error = parse(&format!("{name}=value\n")).unwrap_err();
        assert_eq!(error.line, 1, "{name}");
        assert_eq!(
            error.message,
            format!("procmail variable {name} is not supported")
        );
    }

    assert!(parse("USER_VARIABLE=value\n").is_ok());
}

#[test]
fn rc_limits_change_only_following_syntax() {
    let raised = format!(
        "LIMIT_RC_REGEXES={}\n{}",
        MAX_RC_REGEXES + 1,
        config_with_regexes(MAX_RC_REGEXES + 1)
    );
    assert!(parse(&raised).is_ok());

    let lowered = "LIMIT_RC_RECIPES=0\n:0\ninbox/\n";
    let error = parse(lowered).unwrap_err();
    assert_eq!(error.line, 2);
    assert_eq!(
        error.message,
        "rc recipe count exceeds the active limit of 0"
    );

    let late_raise = format!(
        "{}LIMIT_RC_REGEXES={}\n",
        config_with_regexes(MAX_RC_REGEXES + 1),
        MAX_RC_REGEXES + 1
    );
    assert_eq!(
        parse(&late_raise).unwrap_err().message,
        format!("rc regex count exceeds the active limit of {MAX_RC_REGEXES}")
    );
}

#[test]
fn zero_assignment_limit_prevents_further_limit_changes() {
    let error = parse("LIMIT_MAX_ASSIGNMENTS=0\nLIMIT_RC_RECIPES=10\n").unwrap_err();

    assert_eq!(error.line, 2);
    assert_eq!(
        error.message,
        "rc assignment count exceeds the active limit of 0"
    );
}

#[test]
fn linebuf_applies_to_following_rc_lines_at_the_boundary() {
    for length in [DEFAULT_LINEBUF - 1, DEFAULT_LINEBUF, DEFAULT_LINEBUF + 1] {
        let source = format!("#{}\n", "x".repeat(length - 1));
        let result = parse(&source);
        if length <= DEFAULT_LINEBUF {
            assert!(result.is_ok(), "rejected {length} bytes");
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.line, 1);
            assert_eq!(
                error.message,
                format!("rc line exceeds the active LINEBUF limit of {DEFAULT_LINEBUF} bytes")
            );
        }
    }

    for length in [MIN_LINEBUF - 1, MIN_LINEBUF, MIN_LINEBUF + 1] {
        let source = format!("LINEBUF={MIN_LINEBUF}\n#{}\n", "x".repeat(length - 1));
        let result = parse(&source);
        if length <= MIN_LINEBUF {
            assert!(result.is_ok(), "rejected {length} bytes");
        } else {
            assert_eq!(result.unwrap_err().line, 2);
        }
    }
}

#[test]
fn linebuf_rejects_invalid_values_and_block_assignments() {
    for value in ["", "127", "1048577", "1k", "18446744073709551616"] {
        assert!(
            parse(&format!("LINEBUF={value}\n")).is_err(),
            "accepted {value:?}"
        );
    }
    assert!(parse(&format!("LINEBUF={MIN_LINEBUF}\n")).is_ok());
    assert!(parse(&format!("LINEBUF={MAX_LINEBUF}\n")).is_ok());

    let error = parse(":0\n{\nLINEBUF=128\n}\n").unwrap_err();
    assert_eq!(error.line, 3);
    assert_eq!(
        error.message,
        "variable LINEBUF cannot be assigned inside a recipe block"
    );
}

#[test]
fn repeated_linebuf_assignments_apply_to_following_lines() {
    let accepted_after_raise = format!(
        "LINEBUF={MIN_LINEBUF}\nLINEBUF={}\n#{}\n",
        MIN_LINEBUF + 1,
        "x".repeat(MIN_LINEBUF - 1)
    );
    assert!(parse(&accepted_after_raise).is_ok());

    let rejected_after_lowering = format!(
        "LINEBUF={}\n#{}\nLINEBUF={MIN_LINEBUF}\n#{}\n",
        MIN_LINEBUF + 1,
        "x".repeat(MIN_LINEBUF - 1),
        "x".repeat(MIN_LINEBUF)
    );
    let error = parse(&rejected_after_lowering).unwrap_err();
    assert_eq!(error.line, 4);
    assert_eq!(
        error.message,
        format!("rc line exceeds the active LINEBUF limit of {MIN_LINEBUF} bytes")
    );
}

#[test]
fn every_structural_limit_rejects_the_next_matching_item() {
    let cases = [
        (
            "LIMIT_RC_STATEMENTS=1\nA=1\n",
            2,
            "rc statement count exceeds the active limit of 1",
        ),
        (
            "LIMIT_RC_CONDITIONS=0\n:0\n* < 1\ninbox/\n",
            3,
            "rc condition count exceeds the active limit of 0",
        ),
        (
            "LIMIT_RC_REGEXES=0\n:0\n* pattern\ninbox/\n",
            3,
            "rc regex count exceeds the active limit of 0",
        ),
        (
            "LIMIT_RECIPE_CONDITIONS=0\n:0\n* < 1\ninbox/\n",
            3,
            "recipe condition count exceeds the active limit of 0",
        ),
        (
            "LIMIT_RECIPE_NESTING=0\n:0\n{\n}\n",
            3,
            "recipe nesting depth 1 exceeds the active limit of 0",
        ),
    ];

    for (source, line, message) in cases {
        let error = parse(source).unwrap_err();
        assert_eq!(error.line, line, "source: {source:?}");
        assert_eq!(error.message, message, "source: {source:?}");
    }
}

#[test]
fn lowering_below_the_used_count_fails_on_the_next_item() {
    let error = parse(":0 c\ninbox/\nLIMIT_RC_RECIPES=0\n:0\ninbox/\n").unwrap_err();

    assert_eq!(error.line, 4);
    assert_eq!(
        error.message,
        "rc recipe count exceeds the active limit of 0"
    );
}

#[test]
fn rejects_rc_limit_assignments_inside_recipe_blocks() {
    let error = parse(":0\n{\nLIMIT_RC_REGEXES=100\n}\n").unwrap_err();

    assert_eq!(error.line, 3);
    assert_eq!(
        error.message,
        "variable LIMIT_RC_REGEXES cannot be assigned inside a recipe block"
    );
}

#[test]
fn rejects_invalid_and_excessive_rc_limits() {
    let invalid = parse("LIMIT_RC_RECIPES=1k\n").unwrap_err();
    assert_eq!(invalid.line, 1);
    assert_eq!(
        invalid.message,
        "LIMIT_RC_RECIPES must be an unsigned decimal integer"
    );

    let cases = [
        ("LIMIT_MAX_ASSIGNMENTS", HARD_MAX_RC_ASSIGNMENTS),
        ("LIMIT_RC_STATEMENTS", HARD_MAX_RC_STATEMENTS),
        ("LIMIT_RC_RECIPES", HARD_MAX_RC_RECIPES),
        ("LIMIT_RC_CONDITIONS", HARD_MAX_RC_CONDITIONS),
        ("LIMIT_RC_REGEXES", HARD_MAX_RC_REGEXES),
        ("LIMIT_RECIPE_CONDITIONS", HARD_MAX_CONDITIONS_PER_RECIPE),
        ("LIMIT_RECIPE_NESTING", HARD_MAX_RECIPE_NESTING_DEPTH),
    ];
    for (name, hard_limit) in cases {
        let error = parse(&format!("{name}={}\n", hard_limit + 1)).unwrap_err();
        assert_eq!(error.line, 1, "limit: {name}");
        assert_eq!(
            error.message,
            format!("{name} exceeds the hard limit of {hard_limit}"),
            "limit: {name}"
        );
    }
}

#[test]
fn parses_bare_host_as_an_empty_assignment() {
    let config = parse("HOST\n").unwrap();
    let Statement::Assignment(assignment) = &config.statements[0] else {
        panic!("expected assignment");
    };

    assert_eq!(assignment.name, "HOST");
    assert_eq!(assignment.value, "");
    assert_eq!(assignment.target, crate::config::AssignmentTarget::Host);
}

#[test]
fn parses_non_empty_host_for_runtime_comparison() {
    let config = parse("HOST=elsewhere\n").unwrap();
    let Statement::Assignment(assignment) = &config.statements[0] else {
        panic!("expected assignment");
    };

    assert_eq!(assignment.name, "HOST");
    assert_eq!(assignment.value, "elsewhere");
    assert_eq!(assignment.target, crate::config::AssignmentTarget::Host);
}

#[test]
fn rejects_assignment_to_the_read_only_program_version() {
    let error = parse("PROCMAIL_VERSION=3.22\n").unwrap_err();

    assert_eq!(error.line, 1);
    assert_eq!(
        error.message,
        "variable PROCMAIL_VERSION cannot be assigned in an rc file"
    );
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
fn parses_pipe_flags_and_preserves_continued_shell_text() {
    let config = parse(
        ":0 HBcfhwir\n| FOO=1 formail \\\n    -I X-Spam: \\\n    -I X-Virus:\n:0\nmaildir:final\n",
    )
    .unwrap();
    let Statement::Recipe(recipe) = &config.statements[0] else {
        panic!("expected pipe recipe");
    };
    let RecipeAction::Pipe(action) = &recipe.action else {
        panic!("expected pipe action");
    };

    assert_eq!(recipe.options.condition_input, ConditionInput::Message);
    assert_eq!(recipe.options.action_input, ActionInput::Headers);
    assert_eq!(recipe.options.action_mode, ActionMode::Filter);
    assert_eq!(recipe.options.continuation, ContinuationMode::Continue);
    assert_eq!(recipe.options.child_status, ChildStatusMode::Wait);
    assert_eq!(recipe.options.write_errors, WriteErrorMode::Ignore);
    assert_eq!(recipe.options.output_ending, OutputEnding::Preserve);
    assert_eq!(
        action.command,
        "FOO=1 formail \\\n    -I X-Spam: \\\n    -I X-Virus:"
    );
    assert!(config.has_pipe_actions());
    assert_eq!(config.statements.len(), 2);
}

#[test]
fn rejects_invalid_pipe_flag_combinations_and_uses() {
    let error = parse(":0 wW\n| command\n").unwrap_err();
    assert_eq!(error.line, 1);
    assert_eq!(error.message, "recipe flags 'w' and 'W' cannot be combined");

    let error = parse(":0 f\nmaildir:target\n").unwrap_err();
    assert_eq!(error.line, 1);
    assert_eq!(
        error.message,
        "flags h, b, and f require a pipe action; flags w and W require a pipe action or program condition"
    );

    for destination in ["mbox:target", "maildir:target"] {
        let error = parse(&format!(":0 i\n{destination}\n")).unwrap_err();
        assert_eq!(error.line, 1);
        assert_eq!(
            error.message,
            "recipe flag 'i' is not supported for filesystem delivery because it may publish an incomplete message"
        );
    }
}

#[test]
fn accepts_ignored_pipe_flags_on_blocks_and_reports_them_in_source_order() {
    let config = parse(":0 ir\n{\n:0 r\n{\n:0\nmaildir:target\n}\n}\n").unwrap();
    let mut warnings = Vec::new();

    config.for_each_compatibility_warning(|line, flag| warnings.push((line, flag)));

    assert_eq!(warnings, [(1, 'i'), (1, 'r'), (3, 'r')]);
}

#[test]
fn documents_filesystem_flag_compatibility() {
    let compatibility = include_str!("../../../Documentation/Compatibility.md");

    assert!(compatibility.contains("`i` on mbox or Maildir"));
    assert!(compatibility.contains("publish a truncated Maildir file"));
    assert!(compatibility.contains("Rejected before message input"));
    assert!(compatibility.contains("`r` on Maildir"));
    assert!(compatibility.contains("`r` on mbox"));
    assert!(compatibility.contains("following postmark starts on a new line"));
}

#[test]
fn bounds_and_validates_pipe_command_text() {
    let accepted = format!(":0\n| {}\n", "x".repeat(MAX_PIPE_COMMAND_LEN));
    assert!(parse_wide(&accepted).is_ok());

    let rejected = format!(":0\n| {}\n", "x".repeat(MAX_PIPE_COMMAND_LEN + 1));
    let error = parse_wide(&rejected).unwrap_err();
    assert_eq!(error.line, 2);
    assert_eq!(
        error.message,
        format!("pipe command exceeds the hard limit of {MAX_PIPE_COMMAND_LEN} bytes")
    );

    for (source, message) in [
        (":0\n|\n", "pipe command is empty"),
        (
            ":0\n| command \\\n",
            "pipe command continuation is incomplete",
        ),
        (":0\n| command\0arg\n", "pipe command contains NUL"),
    ] {
        let error = parse(source).unwrap_err();
        assert_eq!(error.line, 2);
        assert_eq!(error.message, message);
    }
}

#[test]
fn rejects_recipe_without_action() {
    let error = parse(":0\n* ^Subject:\n").unwrap_err();

    assert_eq!(error.line, 1);
    assert_eq!(error.message, "recipe has no action");
}

#[test]
fn rejects_unsupported_flag() {
    let error = parse(":0 q\ninbox/\n").unwrap_err();

    assert_eq!(error.line, 1);
    assert_eq!(error.message, "recipe flag 'q' is not supported yet");
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
fn accepts_error_handler_after_a_block() {
    assert!(parse(":0\n{\n:0 c\nmaildir:child\n}\n:0 e\nmaildir:fallback\n").is_ok());
}

#[test]
fn accepts_only_explicit_local_lockfiles_on_recipe_blocks() {
    let config = parse(":0 : block.lock\n{\n:0\nmaildir:target\n}\n").unwrap();
    let Statement::Recipe(recipe) = &config.statements[0] else {
        panic!("expected block recipe");
    };
    assert_eq!(recipe.lock.as_ref().unwrap().source(), "block.lock");

    let error = parse(":0 :\n{\n:0\nmaildir:target\n}\n").unwrap_err();
    assert_eq!(error.line, 1);
    assert_eq!(
        error.message,
        "an implicit local lockfile cannot be derived for a recipe block"
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
                    "recipe nesting depth {depth} exceeds the active limit of {MAX_RECIPE_NESTING_DEPTH}"
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
fn parses_assignment_inside_recipe_block() {
    let config = parse(":0\n{\nBOX=inbox\n}\n").unwrap();
    let Statement::Recipe(recipe) = &config.statements[0] else {
        panic!("expected recipe");
    };
    let RecipeAction::Block(statements) = &recipe.action else {
        panic!("expected block");
    };
    let Statement::Assignment(assignment) = &statements[0] else {
        panic!("expected assignment");
    };

    assert_eq!(assignment.name, "BOX");
    assert_eq!(assignment.value, "inbox");
}

#[test]
fn parses_program_condition_and_quoted_block_assignment() {
    let config =
        parse(":0 W\n* ? test ! -e $LISTDIR\n{\n    LISTDIR=\"$UNKNOWN_FOLDER\"\n}\n").unwrap();
    let Statement::Recipe(recipe) = &config.statements[0] else {
        panic!("expected recipe");
    };
    let ConditionKind::Program(command) = &recipe.conditions[0].kind else {
        panic!("expected program condition");
    };
    assert_eq!(command, "test ! -e $LISTDIR");
    let RecipeAction::Block(statements) = &recipe.action else {
        panic!("expected block");
    };
    let Statement::Assignment(assignment) = &statements[0] else {
        panic!("expected assignment");
    };
    assert_eq!(assignment.value, "$UNKNOWN_FOLDER");
}

#[test]
fn rejects_unterminated_quoted_assignment() {
    let error = parse("NAME=\"value\n").unwrap_err();

    assert_eq!(error.line, 1);
    assert_eq!(error.message, "unterminated double-quoted assignment value");
}

#[test]
fn bounds_and_validates_program_condition_text() {
    for length in [MAX_PIPE_COMMAND_LEN - 1, MAX_PIPE_COMMAND_LEN] {
        let source = format!(":0\n* ? {}\nmaildir:selected\n", "x".repeat(length));
        assert!(
            parse_wide(&source).is_ok(),
            "length {length} must be accepted"
        );
    }

    let source = format!(
        ":0\n* ? {}\nmaildir:selected\n",
        "x".repeat(MAX_PIPE_COMMAND_LEN + 1)
    );
    let error = parse_wide(&source).unwrap_err();
    assert_eq!(error.line, 2);
    assert_eq!(
        error.message,
        format!("program condition command exceeds the hard limit of {MAX_PIPE_COMMAND_LEN} bytes")
    );

    let error = parse(":0\n* ?\nmaildir:selected\n").unwrap_err();
    assert_eq!(error.line, 2);
    assert_eq!(error.message, "program condition command is empty");
}

#[test]
fn parses_include_and_switch_as_ordered_statements() {
    let config = parse("INCLUDERC=common.rc\n:0\n{\nSWITCHRC=$MATCH\n}\n").unwrap();
    let Statement::Include(include) = &config.statements[0] else {
        panic!("expected include");
    };
    let Statement::Recipe(recipe) = &config.statements[1] else {
        panic!("expected recipe");
    };
    let RecipeAction::Block(statements) = &recipe.action else {
        panic!("expected block");
    };
    let Statement::Switch(switch) = &statements[0] else {
        panic!("expected switch");
    };

    assert_eq!(include.value, "common.rc");
    assert_eq!(switch.value, "$MATCH");
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
fn parses_variable_regex_condition() {
    let config = parse(":0\n* CATEGORY ?? ^alerts$\nmaildir:matched\n").unwrap();
    let [Statement::Recipe(recipe)] = config.statements.as_slice() else {
        panic!("expected one recipe");
    };
    let ConditionKind::VariableRegex { name, regex } = &recipe.conditions[0].kind else {
        panic!("expected a variable regex condition");
    };

    assert_eq!(name, "CATEGORY");
    assert_eq!(regex.pattern(), "^alerts$");
}

#[test]
fn parses_match_marker_without_exposing_its_helper_group() {
    let config = parse(":0\n* ^Subject: ([a-z]+)-\\/([a-z]+)$\nmaildir:matched\n").unwrap();
    let Statement::Recipe(recipe) = &config.statements[0] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };

    assert!(regex.match_capture().is_some());
    assert_eq!(regex.capture_indexes().len(), 2);
}

#[test]
fn bounds_and_validates_capture_syntax() {
    let accepted = format!(
        ":0\n* {}\nmaildir:matched\n",
        "()".repeat(MAX_REGEX_CAPTURES)
    );
    assert!(parse(&accepted).is_ok());

    let rejected = format!(
        ":0\n* {}\nmaildir:matched\n",
        "()".repeat(MAX_REGEX_CAPTURES + 1)
    );
    assert!(
        parse(&rejected)
            .unwrap_err()
            .message
            .contains("capture count exceeds")
    );

    let duplicate = parse(":0\n* left\\/middle\\/right\nmaildir:matched\n").unwrap_err();
    assert!(duplicate.message.contains("more than one '\\/'"));

    let named = parse(":0\n* (?P<name>value)\nmaildir:matched\n").unwrap_err();
    assert_eq!(
        named.message,
        "named regular expression groups are not supported"
    );
}

#[test]
fn translates_supported_procmail_regex_extensions() {
    let start = parse(":0\n* ^^start\nmaildir:matched\n").unwrap();
    let Statement::Recipe(recipe) = &start.statements[0] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };
    assert_eq!(regex.compiled().as_str(), "\\Astart");

    let end = parse(":0\n* end^^\nmaildir:matched\n").unwrap();
    let Statement::Recipe(recipe) = &end.statements[0] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };
    assert_eq!(regex.compiled().as_str(), "end\\z");

    let lines = parse(":0\n* ^line$\nmaildir:matched\n").unwrap();
    let Statement::Recipe(recipe) = &lines.statements[0] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };
    assert_eq!(regex.compiled().as_str(), "(?:\\A|\\n)line(?:\\n|\\z)");

    let words = parse(":0\n* \\<word\\>\nmaildir:matched\n").unwrap();
    let Statement::Recipe(recipe) = &words.statements[0] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };
    assert_eq!(regex.compiled().as_str(), "[^a-zA-Z0-9_]word[^a-zA-Z0-9_]");

    let middle = parse(":0\n* left^^right\nmaildir:matched\n").unwrap_err();
    assert_eq!(
        middle.message,
        "'^^' is supported only at the start or end of a regular expression"
    );
}

#[test]
fn expands_reserved_procmail_regex_forms() {
    for (pattern, matching_header, user_captures) in [
        ("^TOscuba", "To: scuba@example.test\n", 0),
        ("^TO_scuba@example\\.test", "Cc: scuba@example.test\n", 0),
        ("^FROM_DAEMON", "From: MAILER-DAEMON@example.test\n", 0),
        (
            "(^Subject: wanted|^FROM_MAILER)",
            "From: postmaster@example.test\n",
            1,
        ),
    ] {
        let config = parse(&format!(":0\n* {pattern}\nmaildir:matched\n")).unwrap();
        let Statement::Recipe(recipe) = &config.statements[0] else {
            panic!("expected recipe");
        };
        let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
            panic!("expected regex");
        };
        assert!(
            regex.compiled().is_match(matching_header.as_bytes()),
            "{pattern}"
        );
        assert_eq!(
            regex.compiled().captures_len(),
            user_captures + 1,
            "{pattern}"
        );
    }

    let forced_case = parse(":0 D\n* ^FROM_DAEMON\nmaildir:matched\n").unwrap();
    let Statement::Recipe(recipe) = &forced_case.statements[0] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };
    assert!(
        regex
            .compiled()
            .is_match(b"from: mailer-daemon@example.test\n")
    );

    let escaped = parse(":0\n* \\^TOliteral\nmaildir:matched\n").unwrap();
    let Statement::Recipe(recipe) = &escaped.statements[0] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };
    assert!(regex.compiled().is_match(b"^TOliteral"));
}

#[test]
fn bounds_expanded_reserved_procmail_regex_forms() {
    let pattern = "^FROM_DAEMON".repeat(MAX_REGEX_PATTERN_LEN / "^FROM_DAEMON".len());
    let error = parse(&format!(
        "LINEBUF={MAX_LINEBUF}\n:0\n* {pattern}\nmaildir:matched\n"
    ))
    .unwrap_err();
    assert_eq!(error.line, 3);
    assert_eq!(
        error.message,
        format!(
            "expanded regular expression exceeds the hard limit of {MAX_REGEX_PATTERN_LEN} bytes"
        )
    );
}

#[test]
fn linebuf_does_not_limit_reserved_regex_expansion() {
    let source = format!("LINEBUF={MIN_LINEBUF}\n:0\n* ^FROM_DAEMON\nmaildir:matched\n");
    assert!(source.lines().all(|line| line.len() < MIN_LINEBUF));

    let config = parse(&source).unwrap();
    let Statement::Recipe(recipe) = &config.statements[1] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };
    assert!(regex.compiled().as_str().len() > MIN_LINEBUF);
}

#[test]
fn parses_special_procmail_condition_areas() {
    for (name, expected) in [
        ("H", ConditionInput::Headers),
        ("B", ConditionInput::Body),
        ("HB", ConditionInput::Message),
        ("BH", ConditionInput::Message),
    ] {
        let config = parse(&format!(":0\n* {name} ?? pattern\nmaildir:matched\n")).unwrap();
        let Statement::Recipe(recipe) = &config.statements[0] else {
            panic!("expected recipe");
        };
        let ConditionKind::AreaRegex { area, regex } = &recipe.conditions[0].kind else {
            panic!("expected an area regex condition");
        };
        assert_eq!(*area, expected);
        assert_eq!(regex.pattern(), "pattern");
    }
}

#[test]
fn enforces_regex_pattern_length_at_the_boundary() {
    for length in [
        MAX_REGEX_PATTERN_LEN - 1,
        MAX_REGEX_PATTERN_LEN,
        MAX_REGEX_PATTERN_LEN + 1,
    ] {
        let source = format!(":0\n* {}\ninbox/\n", "a".repeat(length));
        let result = parse_wide(&source);

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
    let error = parse_wide(&format!(":0\n* < {value}\ninbox/\n")).unwrap_err();

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
        let source = config_with_regexes(count);
        let result = parse_wide(&source);

        if count <= MAX_RC_REGEXES {
            assert!(result.is_ok());
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                error.message,
                format!("rc regex count exceeds the active limit of {MAX_RC_REGEXES}")
            );
        }
    }
}

fn config_with_regexes(mut count: usize) -> String {
    let mut source = String::new();
    while count > 0 {
        let recipe_regexes = count.min(MAX_CONDITIONS_PER_RECIPE);
        source.push_str(":0 c\n");
        source.push_str(&"* pattern\n".repeat(recipe_regexes));
        source.push_str("inbox/\n");
        count -= recipe_regexes;
    }
    source
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
        format!("rc regex count exceeds the active limit of {MAX_RC_REGEXES}")
    );
}

#[test]
fn rejects_oversized_source_before_splitting_lines() {
    for length in [MAX_RC_SIZE - 1, MAX_RC_SIZE, MAX_RC_SIZE + 1] {
        let source = format!("#{}", "x".repeat(length - 1));
        let result = parse_wide(&source);

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
        let result = parse_wide(&source);

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
        let result = parse_wide(&source);

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
fn enforces_shell_setting_length_at_the_boundary() {
    for name in ["SHELL", "SHELLFLAGS", "PATH"] {
        let accepted = format!("{name}={}\n", "x".repeat(MAX_SHELL_SETTING_LEN));
        assert!(parse_wide(&accepted).is_ok(), "{name}");

        let rejected = format!("{name}={}\n", "x".repeat(MAX_SHELL_SETTING_LEN + 1));
        let error = parse_wide(&rejected).unwrap_err();
        assert_eq!(
            error.message,
            format!(
                "{name} value exceeds the hard limit of {} bytes",
                MAX_SHELL_SETTING_LEN
            )
        );
    }
}

#[test]
fn excludes_assignment_formatting_from_value_length() {
    let source = format!(
        "VALUE=  {}  # ignored\n",
        "x".repeat(MAX_ASSIGNMENT_VALUE_LEN)
    );
    let config = parse_wide(&source).unwrap();
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
        let result = parse_wide(&source);

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
        let error = parse_wide(&format!(":0\n{action}\n")).unwrap_err();
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
        let result = parse_wide(&source);

        if length <= MAX_PATH_EXPRESSION_LEN {
            let config = result.unwrap();
            let Statement::Recipe(recipe) = &config.statements[0] else {
                panic!("expected recipe");
            };
            assert_eq!(recipe.lock.as_ref().unwrap().source().len(), length);
        } else {
            assert_path_limit_error(result.unwrap_err(), 1, "lockfile path");
        }
    }
}

#[test]
fn applies_path_limit_to_maildir_assignment() {
    let source = format!("MAILDIR={}\n", "x".repeat(MAX_PATH_EXPRESSION_LEN + 1));
    let error = parse_wide(&source).unwrap_err();

    assert_path_limit_error(error, 1, "MAILDIR path");
}

#[test]
fn applies_path_limit_to_lockext_assignment() {
    for length in [
        MAX_PATH_EXPRESSION_LEN - 1,
        MAX_PATH_EXPRESSION_LEN,
        MAX_PATH_EXPRESSION_LEN + 1,
    ] {
        let source = format!("LOCKEXT={}\n", "x".repeat(length));
        let result = parse_wide(&source);

        if length <= MAX_PATH_EXPRESSION_LEN {
            assert!(result.is_ok(), "length {length}");
        } else {
            assert_path_limit_error(result.unwrap_err(), 1, "LOCKEXT value");
        }
    }
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
                format!("rc statement count exceeds the active limit of {MAX_RC_STATEMENTS}")
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
                format!("rc recipe count exceeds the active limit of {MAX_RC_RECIPES}")
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
                    "recipe condition count exceeds the active limit of {MAX_CONDITIONS_PER_RECIPE}"
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
                format!("rc condition count exceeds the active limit of {MAX_RC_CONDITIONS}")
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
