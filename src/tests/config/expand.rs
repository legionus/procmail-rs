// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

#[test]
fn expression_analysis_preserves_context_specific_default_reachability() {
    let expression = parse_shell_condition_expression(
        "$KNOWN-${EMPTY:-$DYNAMIC}-${MISSING:-$FALLBACK}-$\\QUOTED-`command`",
        1,
    )
    .unwrap();
    let known = BTreeMap::from([
        (
            "KNOWN".to_owned(),
            ExpandedValue {
                text: "value".to_owned(),
                depth: 0,
            },
        ),
        (
            "EMPTY".to_owned(),
            ExpandedValue {
                text: String::new(),
                depth: 0,
            },
        ),
        (
            "QUOTED".to_owned(),
            ExpandedValue {
                text: String::new(),
                depth: 0,
            },
        ),
    ]);
    let dynamic = BTreeSet::from(["DYNAMIC".to_owned(), "FALLBACK".to_owned()]);

    let analysis = ExpressionAnalysis::new(&expression, &known, &dynamic);

    assert_eq!(analysis.missing_runtime, None);
    assert_eq!(analysis.missing_shell_condition, None);
    assert_eq!(analysis.missing_path, Some("DYNAMIC"));
    assert!(analysis.needs_runtime);
    assert!(analysis.references_dynamic);
    assert!(analysis.has_command);
    assert!(analysis.has_regex_quoted_variable);
    assert!(!analysis.shell_condition_static);
}

#[test]
fn expression_analysis_reports_only_reachable_missing_defaults() {
    let expression = parse_expression("${PRESENT:-$HIDDEN}-$MISSING", 1).unwrap();
    let known = BTreeMap::from([(
        "PRESENT".to_owned(),
        ExpandedValue {
            text: "value".to_owned(),
            depth: 0,
        },
    )]);
    let analysis = ExpressionAnalysis::new(&expression, &known, &BTreeSet::new());

    assert_eq!(analysis.missing_runtime, Some("MISSING"));
    assert_eq!(analysis.missing_path, Some("MISSING"));
    assert!(!analysis.needs_runtime);
    assert!(!analysis.shell_condition_static);
}

#[test]
fn known_assignment_values_are_validated_equally_at_root_and_in_blocks() {
    for assignment in [
        "LOCKEXT=bad/name",
        "TIMEOUT=0",
        "UMASK=8888",
        "LOGABSTRACT=all",
    ] {
        let root = parse(&format!("{assignment}\n"))
            .unwrap()
            .expand(&[])
            .unwrap_err();
        let nested = parse(&format!(":0\n{{\n{assignment}\n}}\n"))
            .unwrap()
            .expand(&[])
            .unwrap_err();

        assert_eq!(root.message, nested.message, "{assignment}");
    }
}

#[test]
fn shared_expression_syntax_has_identical_parts_in_common_modes() {
    let source = r"pre-${EMPTY:-$NAME-\${LITERAL}-`printf x`}-post";
    let ordinary = parse_command_expression(source, 4)
        .unwrap()
        .expect("source contains a command");
    let condition = parse_shell_condition_expression(source, 4).unwrap();

    assert_eq!(ordinary, condition);
}

#[test]
fn expression_modes_keep_their_distinct_escape_and_dollar_rules() {
    assert_eq!(
        parse_assignment_expression(r"\q", 6).unwrap().parts,
        [ShellPart::Literal("q".to_owned())]
    );
    let quoted = [ShellPart::Literal(r"\q".to_owned())];
    assert_eq!(
        parse_assignment_expression(r#""\q""#, 6).unwrap().parts,
        quoted
    );
    assert_eq!(
        parse_shell_condition_expression(r"\q", 6).unwrap().parts,
        quoted
    );

    assert!(parse_assignment_expression("$!", 7).is_err());
    assert_eq!(
        parse_shell_condition_expression("$!", 7).unwrap().parts,
        [ShellPart::Literal("$!".to_owned())]
    );
    assert!(parse_shell_condition_expression("$1", 8).is_err());
    assert_eq!(
        parse_shell_condition_expression(r"$\NAME", 9)
            .unwrap()
            .parts,
        [ShellPart::RegexQuotedVariable("NAME".to_owned())]
    );
}

#[test]
fn single_quotes_are_literal_and_can_be_concatenated_with_other_quote_modes() {
    let expression =
        parse_assignment_expression(r#"pre'$NAME `printf hidden` \ raw'"-$NAME"-post"#, 10)
            .unwrap();

    assert_eq!(
        expression.parts,
        [
            ShellPart::Literal("pre$NAME `printf hidden` \\ raw-".to_owned()),
            ShellPart::Variable {
                name: "NAME".to_owned(),
                operation: ParameterOperation::Value,
            },
            ShellPart::Literal("-post".to_owned()),
        ]
    );
    assert!(!expression.has_commands());
}

#[test]
fn assignment_quotes_report_their_unterminated_mode() {
    for (source, expected) in [
        ("'value", "unterminated single-quoted expression"),
        ("\"value", "unterminated double-quoted expression"),
    ] {
        let error = parse_assignment_expression(source, 11).unwrap_err();
        assert_eq!(error.line, 11);
        assert_eq!(error.message, expected);
    }
}

#[test]
fn quoted_and_escaped_assignment_words_expand_to_one_value() {
    let config = parse(concat!(
        "NAME=world\n",
        "VALUE=hello\\ '$NAME '\"$NAME\"\n",
        "TRAIL=right\\ \n",
    ))
    .unwrap()
    .expand(&[])
    .unwrap();
    let Statement::Assignment(value) = &config.statements[1] else {
        panic!("expected assignment");
    };

    assert_eq!(value.value, "hello $NAME world");
    let Statement::Assignment(trailing) = &config.statements[2] else {
        panic!("expected assignment");
    };
    assert_eq!(trailing.value, "right ");
}

#[test]
fn nested_defaults_preserve_their_surrounding_quote_mode() {
    let config = parse(concat!(
        "NAME=world\n",
        "EMPTY=\n",
        "DOUBLE=\"${EMPTY:-'$NAME'}\"\n",
        "SINGLE=${EMPTY:-'a}b'}\n",
    ))
    .unwrap()
    .expand(&[])
    .unwrap();

    for (index, expected) in [(2, "'world'"), (3, "a}b")] {
        let Statement::Assignment(value) = &config.statements[index] else {
            panic!("expected assignment");
        };
        assert_eq!(value.value, expected);
    }
}

#[test]
fn runtime_byte_expansion_preserves_binary_values_and_defaults() {
    let binary = b"a\xffz";
    let empty = b"";
    let expanded =
        expand_runtime_bytes(
            "pre-$BINARY-${EMPTY:-$BINARY}-post",
            7,
            64,
            |name| match name {
                "BINARY" => Some(&binary[..]),
                "EMPTY" => Some(&empty[..]),
                _ => None,
            },
        )
        .unwrap();

    assert_eq!(expanded, b"pre-a\xffz-a\xffz-post");
}

#[test]
fn runtime_byte_expansion_applies_every_parameter_alternative() {
    let set = b"value";
    let empty = b"";
    for (source, expected) in [
        ("${MISSING-default}", &b"default"[..]),
        ("${EMPTY-default}", &b""[..]),
        ("${SET-default}", &b"value"[..]),
        ("${MISSING:-default}", &b"default"[..]),
        ("${EMPTY:-default}", &b"default"[..]),
        ("${SET:-default}", &b"value"[..]),
        ("${MISSING+alternate}", &b""[..]),
        ("${EMPTY+alternate}", &b"alternate"[..]),
        ("${SET+alternate}", &b"alternate"[..]),
        ("${MISSING:+alternate}", &b""[..]),
        ("${EMPTY:+alternate}", &b""[..]),
        ("${SET:+alternate}", &b"alternate"[..]),
    ] {
        let expanded = expand_runtime_bytes(source, 7, 64, |name| match name {
            "SET" => Some(&set[..]),
            "EMPTY" => Some(&empty[..]),
            _ => None,
        })
        .unwrap();
        assert_eq!(expanded, expected, "expression {source}");
    }
}

#[test]
fn runtime_byte_expansion_checks_the_combined_limit() {
    let value = b"1234";
    for limit in [7usize, 8, 9] {
        let result = expand_runtime_bytes("$VALUE$VALUE", 3, limit, |name| {
            (name == "VALUE").then_some(&value[..])
        });
        if limit < 8 {
            let error = result.unwrap_err();
            assert_eq!(error.line, 3);
            assert!(error.message.contains("exceeds the hard limit"));
        } else {
            assert_eq!(result.unwrap(), b"12341234");
        }
    }
}

#[test]
fn runtime_byte_expansion_rejects_missing_and_excessively_nested_values() {
    let missing = expand_runtime_bytes("$MISSING", 9, 64, |_| None).unwrap_err();
    assert_eq!(missing.line, 9);
    assert_eq!(missing.message, "variable MISSING is not defined");

    let mut source = String::new();
    for index in 0..=MAX_EXPANSION_DEPTH {
        source.push_str(&format!("${{V{index}:-"));
    }
    source.push('x');
    source.push_str(&"}".repeat(MAX_EXPANSION_DEPTH + 1));
    let error = expand_runtime_bytes(&source, 11, MAX_ASSIGNMENT_VALUE_LEN, |_| None).unwrap_err();
    assert_eq!(error.line, 11);
    assert_eq!(
        error.message,
        format!("variable expansion exceeds the hard depth limit of {MAX_EXPANSION_DEPTH}")
    );
}

#[test]
fn shell_condition_expansion_obeys_linebuf_at_the_boundary() {
    for length in [127, 128, 129] {
        let mut runtime = crate::runtime::RuntimeVariables::default();
        runtime.set("LINEBUF", "128");
        runtime.set("VALUE", "x".repeat(length));
        let condition = ShellExpandedCondition {
            source: "$VALUE".to_owned(),
            expansion: None,
        };
        let result = expand_shell_condition(&condition, 7, &runtime);

        if length <= 128 {
            assert_eq!(result.unwrap(), "x".repeat(length));
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.line, 7);
            assert_eq!(
                error.message,
                "expanded value exceeds the active LINEBUF limit of 128 bytes"
            );
        }
    }
}

#[test]
fn shell_condition_escaping_matches_double_quoted_procmail_rules() {
    let mut runtime = crate::runtime::RuntimeVariables::default();
    runtime.set("X", "value");

    for (source, expected) in [
        (r"${X}", "value"),
        (r"\${X}", "${X}"),
        (r"\\${X}", r"\value"),
        (r"\\\${X}", r"\${X}"),
        (r"\\\\${X}", r"\\value"),
        (r"\q", r"\q"),
        (r"\.", r"\."),
        (r"\`", "`"),
    ] {
        let condition = ShellExpandedCondition {
            source: source.to_owned(),
            expansion: None,
        };
        assert_eq!(
            expand_shell_condition(&condition, 7, &runtime).unwrap(),
            expected,
            "source {source:?}",
        );
    }
}

#[test]
fn shell_condition_regex_quoting_accepts_binary_without_reinterpreting_it() {
    let mut runtime = crate::runtime::RuntimeVariables::default();
    runtime.set_bytes("VALUE", b"a.\xff".to_vec());
    let quoted = ShellExpandedCondition {
        source: "$\\VALUE".to_owned(),
        expansion: None,
    };
    let expanded = expand_shell_condition(&quoted, 5, &runtime).unwrap();
    let parsed = super::super::parse_reparsed_condition(&expanded, 5, true).unwrap();
    let ConditionKind::Regex(regex) = parsed.kind else {
        panic!("expected quoted byte regex");
    };
    assert!(regex.compiled().is_match(b"a.\xff"));
    assert!(!regex.compiled().is_match(b"axb\xff"));

    let unquoted = ShellExpandedCondition {
        source: "$VALUE".to_owned(),
        expansion: None,
    };
    let error = expand_shell_condition(&unquoted, 6, &runtime).unwrap_err();
    assert_eq!(error.line, 6);
    assert_eq!(
        error.message,
        "shell-expanded condition contains non-UTF-8 variable data"
    );
}

#[test]
fn static_shell_condition_is_reparsed_before_message_input() {
    let error = parse("BROKEN=[\n:0\n* $$BROKEN\nmaildir:unused\n")
        .unwrap()
        .expand(&[])
        .unwrap_err();
    assert_eq!(error.line, 3);
    assert!(error.message.contains("invalid regular expression"));
}

#[test]
fn nested_static_shell_condition_is_reparsed_before_message_input() {
    let supplied = [SuppliedVariable::from_environment("HOME", "$$INNER".to_owned()).unwrap()];
    let error = parse("INNER=[\n:0\n* $$HOME\nmaildir:unused\n")
        .unwrap()
        .expand(&supplied)
        .unwrap_err();
    assert_eq!(error.line, 3);
    assert!(error.message.contains("invalid regular expression"));
}

fn parse_wide(input: &str) -> Result<Config, super::super::ParseError> {
    let mut state = super::super::ParseBudget::default();
    state.set_linebuf(super::super::MAX_LINEBUF);
    super::super::parse_with_state(input, &mut state)
}
use crate::config::{ConditionKind, DEFAULT_LINEBUF, MAX_SHELL_SETTING_LEN, parse};

fn prepared_header_value(source: &str, known_value: &str) -> Result<String, ExpansionError> {
    let mut action = HeaderAction {
        operations: vec![HeaderOperation::Set {
            line: 9,
            name: "X-Test".into(),
            value: HeaderValue {
                source: source.into(),
                expansion: None,
            },
        }],
    };
    let known = BTreeMap::from([(
        "VALUE".to_owned(),
        ExpandedValue {
            text: known_value.to_owned(),
            depth: 0,
        },
    )]);
    prepare_header_action(&mut action, &known, &BTreeSet::new())?;
    let HeaderOperation::Set { value, .. } = &action.operations[0] else {
        panic!("expected set operation");
    };
    value.resolve_with(9, |name| known.get(name).map(|item| item.text.clone()))
}

#[test]
fn prepares_header_values_with_existing_expansion_syntax() {
    assert_eq!(
        prepared_header_value("prefix:${VALUE:-missing}:tail", "selected").unwrap(),
        "prefix:selected:tail"
    );
    assert_eq!(
        prepared_header_value("${VALUE:-fallback}", "").unwrap(),
        "fallback"
    );
}

#[test]
fn rejects_invalid_or_unsafe_expanded_header_values() {
    let error = prepared_header_value("${VALUE", "text").unwrap_err();
    assert_eq!(error.line, 9);

    let error = prepared_header_value("$VALUE", "before\0after").unwrap_err();
    assert_eq!(error.line, 9);
    assert_eq!(
        error.message,
        "expanded header value contains NUL, CR, or LF"
    );
}

#[test]
fn bounds_expanded_header_values_at_linebuf() {
    for size in [DEFAULT_LINEBUF - 1, DEFAULT_LINEBUF, DEFAULT_LINEBUF + 1] {
        let result = prepared_header_value("$VALUE", &"x".repeat(size));
        if size <= DEFAULT_LINEBUF {
            assert_eq!(result.unwrap().len(), size);
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.line, 9);
            assert_eq!(
                error.message,
                format!(
                    "expanded value exceeds the active LINEBUF limit of {DEFAULT_LINEBUF} bytes"
                )
            );
        }
    }
}

#[test]
fn keeps_runtime_header_references_structured() {
    let mut action = HeaderAction {
        operations: vec![HeaderOperation::Add {
            line: 7,
            name: "X-Match".into(),
            value: HeaderValue {
                source: "${MATCH:-fallback}".into(),
                expansion: None,
            },
        }],
    };

    prepare_header_action(&mut action, &BTreeMap::new(), &BTreeSet::new()).unwrap();
    let HeaderOperation::Add { value, .. } = &action.operations[0] else {
        panic!("expected add operation");
    };
    assert_eq!(
        value.resolve_with(7, |name| (name == "MATCH").then(|| "selected".into())),
        Ok("selected".into())
    );
}

fn resolved_destination(config: &Config, statement_index: usize) -> Destination {
    let mut variables = config
        .initial_variables()
        .iter()
        .map(|(name, value, _)| (name.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    for (index, statement) in config.statements[..=statement_index].iter().enumerate() {
        match statement {
            Statement::Assignment(assignment) => {
                variables.insert(assignment.name.clone(), assignment.value.clone());
            }
            Statement::Recipe(recipe) if index == statement_index => {
                let RecipeAction::Deliver(destination) = &recipe.action else {
                    panic!("expected delivery recipe");
                };
                return destination
                    .resolve_with(|name| {
                        variables.get(name).cloned().or_else(|| {
                            (name == "LINEBUF").then(|| config.initial_linebuf.to_string())
                        })
                    })
                    .unwrap();
            }
            Statement::Recipe(_) => {}
            Statement::CommandAssignment(_) | Statement::Include(_) | Statement::Switch(_) => {}
        }
    }
    panic!("statement is not a recipe");
}

#[test]
fn expands_both_variable_reference_forms_sequentially() {
    let config = parse("ROOT=mail\nBOX=${ROOT}/inbox\nMAILDIR=/srv/$ROOT\n:0\nmaildir:$BOX\n")
        .unwrap()
        .expand(&[])
        .unwrap();

    let Statement::Assignment(box_assignment) = &config.statements[1] else {
        panic!("expected assignment");
    };
    assert_eq!(box_assignment.value, "mail/inbox");
    assert_eq!(config.maildir(), Some("/srv/mail"));
    assert_eq!(
        resolved_destination(&config, 3),
        Destination::Maildir("/srv/mail/mail/inbox".into())
    );
}

#[test]
fn prepares_named_and_implicit_delivery_lockfiles() {
    let config = parse(
        "MAILDIR=/srv/mail\nNAME=selected\n:0 c:named-$NAME.lock\nmaildir:one\n:0 :\nmaildir:two\n",
    )
    .unwrap()
    .expand(&[])
    .unwrap();
    let Statement::Recipe(named) = &config.statements[2] else {
        panic!("expected named-lock recipe");
    };
    assert_eq!(
        named
            .lock
            .as_ref()
            .unwrap()
            .resolve_with(|name| (name == "NAME").then(|| "selected".to_owned()))
            .unwrap(),
        "/srv/mail/named-selected.lock"
    );
    let Statement::Recipe(implicit) = &config.statements[3] else {
        panic!("expected implicit-lock recipe");
    };
    assert_eq!(implicit.lock.as_ref().unwrap().source(), "");

    let error = parse(":0 :\n| command\n").unwrap().expand(&[]).unwrap_err();
    assert_eq!(error.line, 1);
    assert_eq!(
        error.message,
        "an implicit local lockfile requires a filesystem destination"
    );
}

#[test]
fn expands_lockext_from_its_default_and_in_statement_order() {
    let config = parse("DEFAULT_EXT=$LOCKEXT\nLOCKEXT=.next\nSELECTED_EXT=$LOCKEXT\n")
        .unwrap()
        .expand(&[])
        .unwrap();

    let Statement::Assignment(default_ext) = &config.statements[0] else {
        panic!("expected default extension assignment");
    };
    let Statement::Assignment(selected_ext) = &config.statements[2] else {
        panic!("expected selected extension assignment");
    };
    assert_eq!(default_ext.value, ".lock");
    assert_eq!(selected_ext.value, ".next");
}

#[test]
fn validates_logabstract_after_static_and_conditional_expansion() {
    let config = parse("MODE=no\nLOGABSTRACT=$MODE\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let Statement::Assignment(assignment) = &config.statements[1] else {
        panic!("expected LOGABSTRACT assignment");
    };
    assert_eq!(assignment.value, "no");
    assert_eq!(assignment.target, AssignmentTarget::LogAbstract);

    let error = parse("MODE=all\n:0\n{\nLOGABSTRACT=$MODE\n}\n")
        .unwrap()
        .expand(&[])
        .unwrap_err();
    assert_eq!(error.line, 4);
    assert_eq!(
        error.message,
        "LOGABSTRACT supports only 'no'; other values could log sensitive header values"
    );
}

#[test]
fn validates_runtime_logabstract_value_when_the_block_executes() {
    let config = parse(":0\n{\nLOGABSTRACT=$MATCH\n}\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let Statement::Recipe(recipe) = &config.statements[0] else {
        panic!("expected block recipe");
    };
    let RecipeAction::Block(children) = &recipe.action else {
        panic!("expected block action");
    };
    let Statement::Assignment(assignment) = &children[0] else {
        panic!("expected LOGABSTRACT assignment");
    };

    assert_eq!(
        assignment
            .resolve_with(|name| (name == "MATCH").then(|| "no".to_owned()))
            .unwrap(),
        "no"
    );
    let error = assignment
        .resolve_with(|name| (name == "MATCH").then(|| "all".to_owned()))
        .unwrap_err();
    assert_eq!(error.line, 3);
    assert_eq!(
        error.message,
        "LOGABSTRACT supports only 'no'; other values could log sensitive header values"
    );
}

#[test]
fn rejects_lockext_that_adds_a_path_component_after_expansion() {
    let error = parse("SEPARATOR=/\nLOCKEXT=.locks${SEPARATOR}shared\n")
        .unwrap()
        .expand(&[])
        .unwrap_err();

    assert_eq!(error.line, 2);
    assert_eq!(error.message, "LOCKEXT must not contain '/'");
}

#[test]
fn expands_supplied_variables_before_rc_assignments() {
    let supplied = [
        SuppliedVariable::parse("ROOT=old".into()).unwrap(),
        SuppliedVariable::parse("ROOT=cli".into()).unwrap(),
        SuppliedVariable::parse("BOX=$ROOT".into()).unwrap(),
    ];
    let config = parse("FIRST=$BOX\nBOX=rc\nSECOND=$BOX\n:0\nmaildir:$FIRST-$SECOND\n")
        .unwrap()
        .expand(&supplied)
        .unwrap();

    let Statement::Recipe(_) = &config.statements[3] else {
        panic!("expected recipe");
    };
    assert_eq!(
        resolved_destination(&config, 3),
        Destination::Maildir("cli-rc".into())
    );
}

#[test]
fn inserts_passwd_values_without_rescanning_their_text() {
    let supplied = [
        SuppliedVariable::from_environment("HOME", "/home/$literal".into()).unwrap(),
        SuppliedVariable::from_environment("LOGNAME", "user".into()).unwrap(),
    ];
    let config = parse("VALUE=$HOME\n").unwrap().expand(&supplied).unwrap();
    let Statement::Assignment(assignment) = &config.statements[0] else {
        panic!("expected assignment");
    };

    assert_eq!(assignment.value, "/home/$literal");
}

#[test]
fn exposes_the_system_hostname_to_rc_expansion_without_rescanning_it() {
    let supplied = [SuppliedVariable::from_system_hostname("mail-$literal".to_owned()).unwrap()];
    let config = parse("SAVED_HOST=$HOST\nHOST=$SAVED_HOST\n")
        .unwrap()
        .expand(&supplied)
        .unwrap();

    let Statement::Assignment(saved) = &config.statements[0] else {
        panic!("expected SAVED_HOST assignment");
    };
    let Statement::Assignment(host) = &config.statements[1] else {
        panic!("expected HOST assignment");
    };
    assert_eq!(saved.value, "mail-$literal");
    assert_eq!(host.value, "mail-$literal");
}

#[test]
fn exposes_the_program_version_to_rc_expansion() {
    let supplied = [SuppliedVariable::from_program_version().unwrap()];
    let config = parse("VERSION=$PROCMAIL_VERSION\n")
        .unwrap()
        .expand(&supplied)
        .unwrap();
    let Statement::Assignment(assignment) = &config.statements[0] else {
        panic!("expected VERSION assignment");
    };

    assert_eq!(assignment.value, env!("CARGO_PKG_VERSION"));
}

#[test]
fn rejects_self_references_and_cycles_without_recursive_scanning() {
    for source in ["A=$A\n", "A=$B\nB=$A\n"] {
        let error = parse(source).unwrap().expand(&[]).unwrap_err();
        assert_eq!(error.line, 1);
        assert!(error.message.contains("is not defined"));
    }

    let supplied = [SuppliedVariable::parse("A=$A".into()).unwrap()];
    let error = parse("").unwrap().expand(&supplied).unwrap_err();
    assert_eq!(error.line, 0);
    assert_eq!(error.to_string(), "command line: variable A is not defined");
}

#[test]
fn enforces_expansion_depth_at_the_boundary() {
    let mut source = String::from("V0=value\n");
    for depth in 1..=MAX_EXPANSION_DEPTH {
        source.push_str(&format!("V{depth}=$V{}\n", depth - 1));
    }
    assert!(parse(&source).unwrap().expand(&[]).is_ok());

    source.push_str(&format!(
        "V{}=$V{}\n",
        MAX_EXPANSION_DEPTH + 1,
        MAX_EXPANSION_DEPTH
    ));
    let error = parse_wide(&source).unwrap().expand(&[]).unwrap_err();
    assert_eq!(error.line, MAX_EXPANSION_DEPTH + 2);
    assert_eq!(
        error.message,
        format!("variable expansion exceeds the hard depth limit of {MAX_EXPANSION_DEPTH}")
    );
}

#[test]
fn resolves_paths_against_maildir_active_at_each_recipe() {
    let config = parse("MAILDIR=/srv/first\n:0 c\none/\nMAILDIR=second\n:0\nmaildir:two\n")
        .unwrap()
        .expand(&[])
        .unwrap();

    let Statement::Recipe(_) = &config.statements[1] else {
        panic!("expected first recipe");
    };
    let Statement::Recipe(_) = &config.statements[3] else {
        panic!("expected second recipe");
    };
    assert_eq!(
        resolved_destination(&config, 1),
        Destination::Maildir("/srv/first/one/".into())
    );
    assert_eq!(
        resolved_destination(&config, 3),
        Destination::Maildir("/srv/first/second/two".into())
    );
    assert_eq!(config.maildir(), Some("/srv/first/second"));
}

#[test]
fn bare_paths_resolve_to_mbox_except_for_the_null_device() {
    let mbox = parse("MAILDIR=/srv/mail\n:0\ninbox\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    assert_eq!(
        resolved_destination(&mbox, 1),
        Destination::Mbox("/srv/mail/inbox".into())
    );

    let discard = parse("MAILDIR=/dev\n:0\nnull\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    assert_eq!(
        resolved_destination(&discard, 1),
        Destination::Discard("/dev/null".into())
    );
}

#[test]
fn runtime_bare_path_is_classified_after_variable_expansion() {
    let config = parse("MAILDIR=/mail\n:0\n${LASTFOLDER}\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let Statement::Recipe(recipe) = &config.statements[1] else {
        panic!("expected recipe");
    };
    let RecipeAction::Deliver(destination) = &recipe.action else {
        panic!("expected delivery recipe");
    };
    assert_eq!(
        destination
            .resolve_with(|name| (name == "LASTFOLDER").then(|| "/dev/null".to_owned()))
            .unwrap(),
        Destination::Discard("/dev/null".into())
    );
}

#[test]
fn rejects_unmarked_destination_lists_after_expansion() {
    let error = parse("BOX=\"first second\"\n:0\n$BOX\n")
        .unwrap()
        .expand(&[])
        .unwrap_err();
    assert_eq!(
        error.message,
        "multiple unmarked mailbox destinations are not supported"
    );

    let config = parse(":0\n${LASTFOLDER}/\n").unwrap().expand(&[]).unwrap();
    let Statement::Recipe(recipe) = &config.statements[0] else {
        panic!("expected recipe");
    };
    let RecipeAction::Deliver(destination) = &recipe.action else {
        panic!("expected delivery recipe");
    };
    let error = destination
        .resolve_with(|name| (name == "LASTFOLDER").then(|| "first second".to_owned()))
        .unwrap_err();
    assert_eq!(
        error.message,
        "multiple unmarked mailbox destinations are not supported"
    );

    assert!(
        parse("BOX='path with spaces'\n:0\nmbox:$BOX\n")
            .unwrap()
            .expand(&[])
            .is_ok()
    );
}

#[test]
fn rejects_undefined_forward_references() {
    let error = parse("A=$B\nB=value\n").unwrap().expand(&[]).unwrap_err();
    assert_eq!(error.message, "variable B is not defined");
}

#[test]
fn resolves_runtime_path_only_when_it_is_used() {
    let config = parse("MAILDIR=/mail\n:0\nmaildir:${LASTFOLDER}-related\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let Statement::Recipe(recipe) = &config.statements[1] else {
        panic!("expected recipe");
    };
    let RecipeAction::Deliver(destination) = &recipe.action else {
        panic!("expected delivery recipe");
    };
    assert!(destination.needs_runtime_variables());

    let error = destination.resolve_with(|_| None).unwrap_err();
    assert_eq!(error.message, "runtime variable LASTFOLDER is not set");
    let resolved = destination
        .resolve_with(|name| (name == "LASTFOLDER").then(|| "archive/item".to_owned()))
        .unwrap();
    assert_eq!(resolved.path(), "/mail/archive/item-related");
}

#[test]
fn expands_destinations_inside_recipe_blocks() {
    let config = parse("MAILDIR=/mail\nBOX=lists\n:0\n{\n:0\nmaildir:$BOX/inbox\n}\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let Statement::Recipe(parent) = &config.statements[2] else {
        panic!("expected parent recipe");
    };
    let RecipeAction::Block(children) = &parent.action else {
        panic!("expected block action");
    };
    let Statement::Recipe(child) = &children[0] else {
        panic!("expected child recipe");
    };
    let RecipeAction::Deliver(destination) = &child.action else {
        panic!("expected delivery action");
    };

    let resolved = destination
        .resolve_with(|name| (name == "BOX").then(|| "lists".to_owned()))
        .unwrap();
    assert_eq!(resolved.path(), "/mail/lists/inbox");
}

#[test]
fn rejects_unsupported_and_malformed_references() {
    for source in [
        "A=$$\n",
        "A=${#NAME}\n",
        "A=${NAME%pattern}\n",
        "A=${NAME^pattern}\n",
        "A=${NAME\n",
        "A=$\n",
    ] {
        assert!(parse(source).is_err(), "{source:?}");
    }
}

#[test]
fn parameter_assignment_makes_its_target_visible_to_following_statements() {
    let config = parse("VALUE=${SIDE:=first}-$SIDE\nNEXT=$SIDE\n")
        .unwrap()
        .expand(&[])
        .unwrap();

    assert!(matches!(
        config.statements[0],
        Statement::CommandAssignment(_)
    ));
    let Statement::Assignment(next) = &config.statements[1] else {
        panic!("expected deferred assignment");
    };
    assert!(next.expansion.is_some());
}

#[test]
fn destination_parameter_assignment_is_visible_to_later_path_parts() {
    parse(":0\nmbox:${SIDE:=selected}-$SIDE\n")
        .unwrap()
        .expand(&[])
        .unwrap();
}

#[test]
fn parameter_assignment_rejects_protected_targets() {
    for name in ["MAILDIR", "LASTFOLDER", "PROCMAIL_VERSION"] {
        let error = parse(&format!("VALUE=${{{name}:=changed}}\n")).unwrap_err();
        assert!(error.message.contains("protected variable"), "{name}");
        assert!(error.message.contains(name), "{name}");
    }
}

#[test]
fn required_parameter_uses_a_generic_diagnostic_without_evaluating_word() {
    let error = parse("VALUE=${MISSING:?`must-not-run` private-text}\n")
        .unwrap()
        .expand(&[])
        .unwrap_err();

    assert_eq!(error.message, "parameter MISSING is unset or empty");
    assert!(!error.message.contains("must-not-run"));
    assert!(!error.message.contains("private-text"));

    let config = parse("MISSING=present\nVALUE=${MISSING:?ignored}\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let Statement::Assignment(value) = &config.statements[1] else {
        panic!("expected assignment");
    };
    assert_eq!(value.value, "present");
}

#[test]
fn parameter_assignment_is_rejected_where_ordered_evaluation_is_unavailable() {
    for source in [
        "INCLUDERC=${FILE:=child.rc}\n",
        ":0:${LOCK:=mail.lock}\nmbox:mail\n",
        ":0\n* $^Subject: ${VALUE:=text}\nmbox:mail\n",
        ":0\nheaders {\nset X-Test: ${VALUE:=text}\n}\n",
    ] {
        let error = parse(source).unwrap().expand(&[]).unwrap_err();
        assert!(
            error
                .message
                .contains("parameter assignment is not supported"),
            "{source}: {}",
            error.message
        );
    }
}

#[test]
fn rejects_unsupported_procmail_variables_inside_expansions() {
    for name in super::super::UNSUPPORTED_PROCMAIL_VARIABLES {
        let value = format!("${{{name}}}");
        let error = parse(&format!("VALUE={value}\n")).unwrap_err();
        assert_eq!(error.line, 1, "{name}");
        assert_eq!(
            error.message,
            format!("procmail variable {name} is not supported")
        );
    }
}

#[test]
fn follows_shell_like_name_boundaries() {
    let config = parse("NAME=mail\nNAMEsuffix=archive\nA=$NAMEsuffix\nB=${NAME}suffix\n")
        .unwrap()
        .expand(&[])
        .unwrap();

    let Statement::Assignment(a) = &config.statements[2] else {
        panic!("expected assignment");
    };
    let Statement::Assignment(b) = &config.statements[3] else {
        panic!("expected assignment");
    };
    assert_eq!(a.value, "archive");
    assert_eq!(b.value, "mailsuffix");
}

#[test]
fn assignment_escaping_matches_procmail_backslash_parity() {
    let source = r#"X=value
U0=${X}
U1=\${X}
U2=\\${X}
U3=\\\${X}
U4=\\\\${X}
Q0="${X}"
Q1="\${X}"
Q2="\\${X}"
Q3="\\\${X}"
Q4="\\\\${X}"
E0=\q
E1=\.
E2="\q"
E3="\."
T0=\`printf\`
T1="\`printf\`"
"#;
    let config = parse(source).unwrap().expand(&[]).unwrap();
    let values = config
        .statements
        .iter()
        .filter_map(|statement| match statement {
            Statement::Assignment(assignment) => {
                Some((assignment.name.as_str(), assignment.value.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();

    for (name, expected) in [
        ("U0", "value"),
        ("U1", "${X}"),
        ("U2", "\\value"),
        ("U3", "\\${X}"),
        ("U4", "\\\\value"),
        ("Q0", "value"),
        ("Q1", "${X}"),
        ("Q2", "\\value"),
        ("Q3", "\\${X}"),
        ("Q4", "\\\\value"),
        ("E0", "q"),
        ("E1", "."),
        ("E2", "\\q"),
        ("E3", "\\."),
        ("T0", "`printf`"),
        ("T1", "`printf`"),
    ] {
        assert_eq!(values.get(name), Some(&expected), "assignment {name}");
    }
}

#[test]
fn expands_shell_like_defaults_lazily() {
    let config = parse(
            "EMPTY=\nROOT=/mail\nA=${MISSING:-$ROOT/inbox}\nB=${EMPTY:-${MISSING:-fallback}}\nC=${ROOT:-$UNDEFINED}\nD=${ROOT-$UNDEFINED}\nE=${MISSING+$UNDEFINED}\nF=${EMPTY:+$UNDEFINED}\n",
        )
        .unwrap()
        .expand(&[])
        .unwrap();

    for (index, expected) in [
        (2, "/mail/inbox"),
        (3, "fallback"),
        (4, "/mail"),
        (5, "/mail"),
        (6, ""),
        (7, ""),
    ] {
        let Statement::Assignment(assignment) = &config.statements[index] else {
            panic!("expected assignment");
        };
        assert_eq!(assignment.value, expected);
    }
}

#[test]
fn parameter_alternatives_distinguish_unset_empty_and_nonempty_values() {
    let config = parse(concat!(
        "EMPTY=\n",
        "SET=value\n",
        "A=${MISSING-default}\n",
        "B=${EMPTY-default}\n",
        "C=${SET-default}\n",
        "D=${MISSING:-default}\n",
        "E=${EMPTY:-default}\n",
        "F=${SET:-default}\n",
        "G=${MISSING+alternate}\n",
        "H=${EMPTY+alternate}\n",
        "I=${SET+alternate}\n",
        "J=${MISSING:+alternate}\n",
        "K=${EMPTY:+alternate}\n",
        "L=${SET:+alternate}\n",
    ))
    .unwrap()
    .expand(&[])
    .unwrap();

    let values = config
        .statements
        .iter()
        .filter_map(|statement| match statement {
            Statement::Assignment(assignment) => {
                Some((assignment.name.as_str(), assignment.value.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    for (name, expected) in [
        ("A", "default"),
        ("B", ""),
        ("C", "value"),
        ("D", "default"),
        ("E", "default"),
        ("F", "value"),
        ("G", ""),
        ("H", "alternate"),
        ("I", "alternate"),
        ("J", ""),
        ("K", ""),
        ("L", "alternate"),
    ] {
        assert_eq!(values.get(name), Some(&expected), "assignment {name}");
    }
}

#[test]
fn parameter_alternatives_allow_empty_and_nested_words() {
    let config = parse(concat!(
        "EMPTY=\n",
        "SET=value\n",
        "A=${MISSING-}\n",
        "B=${EMPTY:-}\n",
        "C=${MISSING+}\n",
        "D=${SET:+}\n",
        "E=${MISSING-${EMPTY:+bad}${EMPTY+${SET:+nested}}}\n",
    ))
    .unwrap()
    .expand(&[])
    .unwrap();

    for (index, expected) in [(2, ""), (3, ""), (4, ""), (5, ""), (6, "nested")] {
        let Statement::Assignment(assignment) = &config.statements[index] else {
            panic!("expected assignment");
        };
        assert_eq!(assignment.value, expected);
    }
}

#[test]
fn unselected_parameter_words_do_not_execute_commands_or_resolve_variables() {
    let config = parse(concat!(
        "SET=value\n",
        "A=${SET:-`exit 1`$UNDEFINED}\n",
        "B=${SET-`exit 1`$UNDEFINED}\n",
        "C=${MISSING+`exit 1`$UNDEFINED}\n",
        "D=${MISSING:+`exit 1`$UNDEFINED}\n",
    ))
    .unwrap()
    .expand(&[])
    .unwrap();

    for (index, expected) in [(1, "value"), (2, "value"), (3, ""), (4, "")] {
        let Statement::Assignment(assignment) = &config.statements[index] else {
            panic!("expected statically selected assignment");
        };
        assert_eq!(assignment.value, expected);
    }
}

#[test]
fn bounds_nested_default_syntax_depth() {
    let mut within_limit = String::new();
    for index in 0..MAX_EXPANSION_DEPTH {
        within_limit.push_str(&format!("${{MISSING{index}:-"));
    }
    within_limit.push_str("value");
    within_limit.push_str(&"}".repeat(MAX_EXPANSION_DEPTH));
    let source = format!("A={within_limit}\n");
    assert!(parse(&source).unwrap().expand(&[]).is_ok());

    let beyond_limit = format!("${{OUTER:-{within_limit}}}");
    let source = format!("A={beyond_limit}\n");
    let error = parse_wide(&source).unwrap_err();
    assert_eq!(
        error.message,
        format!("variable expansion exceeds the hard depth limit of {MAX_EXPANSION_DEPTH}")
    );
}

#[test]
fn resolves_runtime_defaults_without_rescanning_values() {
    let config = parse("MAILDIR=/mail\n:0\nmaildir:${LASTFOLDER:-$MAILDIR}/next\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let Statement::Recipe(recipe) = &config.statements[1] else {
        panic!("expected recipe");
    };
    let RecipeAction::Deliver(destination) = &recipe.action else {
        panic!("expected delivery recipe");
    };
    let bound = destination
        .bind_with(|name| (name == "MAILDIR").then(|| "/mail".to_owned()))
        .unwrap();

    let fallback = bound.resolve_with(|_| Some(String::new())).unwrap();
    assert_eq!(fallback.path(), "/mail/next");
    let literal = bound
        .resolve_with(|_| Some("archive/$MAILDIR".to_owned()))
        .unwrap();
    assert_eq!(literal.path(), "/mail/archive/$MAILDIR/next");
}

#[test]
fn resolves_deferred_logfile_against_the_active_maildir() {
    let config = parse("MAILDIR=/mail\nNAME=`printf logs/run`\nLOGFILE=$NAME\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let Statement::Assignment(logfile) = &config.statements[2] else {
        panic!("expected LOGFILE assignment");
    };

    let resolved = logfile
        .resolve_with(|name| match name {
            "NAME" => Some("logs/run".to_owned()),
            "MAILDIR" => Some("/mail".to_owned()),
            _ => None,
        })
        .unwrap();
    assert_eq!(resolved, "/mail/logs/run");
}

#[test]
fn bounds_expanded_paths_before_allocation_growth() {
    let source = format!(
        "A={}\nB={}\n:0\nmaildir:$A$B\n",
        "a".repeat(MAX_PATH_EXPRESSION_LEN),
        "b"
    );
    let error = parse_wide(&source).unwrap().expand(&[]).unwrap_err();

    assert_eq!(error.line, 4);
    assert_eq!(
        error.message,
        format!("expanded value exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes")
    );
}

#[test]
fn bounds_expanded_assignment_values_at_the_boundary() {
    let prefix = "a".repeat(MAX_ASSIGNMENT_VALUE_LEN / 2);
    for length in [
        MAX_ASSIGNMENT_VALUE_LEN - 1,
        MAX_ASSIGNMENT_VALUE_LEN,
        MAX_ASSIGNMENT_VALUE_LEN + 1,
    ] {
        let suffix = "b".repeat(length - prefix.len());
        let source = format!("PREFIX={prefix}\nVALUE=${{PREFIX}}{suffix}\n");
        let result = parse_wide(&source).unwrap().expand(&[]);

        if length <= MAX_ASSIGNMENT_VALUE_LEN {
            let config = result.unwrap();
            let Statement::Assignment(value) = &config.statements[1] else {
                panic!("expected assignment");
            };
            assert_eq!(value.value.len(), length);
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.line, 2);
            assert_eq!(
                error.message,
                format!(
                    "expanded value exceeds the hard limit of {MAX_ASSIGNMENT_VALUE_LEN} bytes"
                )
            );
        }
    }
}

#[test]
fn linebuf_rejects_a_following_expansion_before_growth() {
    let prefix = "x".repeat(100);
    let config = parse(&format!(
        "LINEBUF=128\nPREFIX={prefix}\nVALUE=$PREFIX$PREFIX\n"
    ))
    .unwrap();
    let error = config.expand(&[]).unwrap_err();

    assert_eq!(error.line, 3);
    assert_eq!(
        error.message,
        "expanded value exceeds the active LINEBUF limit of 128 bytes"
    );
}

#[test]
fn bounds_expanded_shell_settings() {
    let prefix = "x".repeat(MAX_SHELL_SETTING_LEN / 2 + 1);
    let source = format!("PREFIX={prefix}\nSHELL=$PREFIX$PREFIX\n");
    let error = parse_wide(&source).unwrap().expand(&[]).unwrap_err();

    assert_eq!(error.line, 2);
    assert_eq!(
        error.message,
        format!("expanded value exceeds the hard limit of {MAX_SHELL_SETTING_LEN} bytes")
    );
}

#[test]
fn bounds_expanded_destination_paths_at_the_boundary() {
    let prefix = "a".repeat(MAX_PATH_EXPRESSION_LEN / 2);
    for length in [
        MAX_PATH_EXPRESSION_LEN - 1,
        MAX_PATH_EXPRESSION_LEN,
        MAX_PATH_EXPRESSION_LEN + 1,
    ] {
        let suffix = "b".repeat(length - prefix.len());
        let source = format!("PREFIX={prefix}\n:0\nmaildir:${{PREFIX}}{suffix}\n");
        let result = parse_wide(&source).unwrap().expand(&[]);
        if length <= MAX_PATH_EXPRESSION_LEN {
            let config = result.unwrap();
            let resolved = resolved_destination(&config, 1);
            assert_eq!(resolved.path().len(), length);
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.line, 3);
            assert_eq!(
                error.message,
                format!("expanded value exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes")
            );
        }
    }
}

#[test]
fn bounds_maildir_path_join_before_allocation_growth() {
    let source = format!(
        "MAILDIR=/{}\n:0\nmaildir:child\n",
        "a".repeat(MAX_PATH_EXPRESSION_LEN - 1)
    );
    let error = parse_wide(&source).unwrap().expand(&[]).unwrap_err();

    assert_eq!(error.line, 3);
    assert_eq!(
        error.message,
        format!("expanded value exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes")
    );
}

#[test]
fn bounds_maildir_path_join_at_the_boundary() {
    for length in [
        MAX_PATH_EXPRESSION_LEN - 1,
        MAX_PATH_EXPRESSION_LEN,
        MAX_PATH_EXPRESSION_LEN + 1,
    ] {
        let base_len = length - 2;
        let source = format!("MAILDIR=/{}\n:0\nmaildir:x\n", "a".repeat(base_len - 1));
        let result = parse_wide(&source).unwrap().expand(&[]);

        if length <= MAX_PATH_EXPRESSION_LEN {
            let config = result.unwrap();
            let Statement::Recipe(_) = &config.statements[1] else {
                panic!("expected recipe");
            };
            let resolved = resolved_destination(&config, 1);
            let Destination::Maildir(path) = &resolved else {
                panic!("expected Maildir destination");
            };
            assert_eq!(path.source().len(), length);
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.line, 3);
            assert_eq!(
                error.message,
                format!("expanded value exceeds the hard limit of {MAX_PATH_EXPRESSION_LEN} bytes")
            );
        }
    }
}

#[test]
fn accepts_only_unambiguous_filesystem_path_components() {
    for (path, allows_trailing_slash) in [
        ("relative/mail", false),
        ("/absolute/mail", false),
        ("relative/mail/", true),
        ("/absolute/mail/", true),
    ] {
        validate_filesystem_path(path, 1, "test", allows_trailing_slash).unwrap();
    }

    for (path, allows_trailing_slash, expected) in [
        ("", true, "path is empty"),
        ("/", true, "does not name a filesystem entry"),
        ("a//b", true, "contains an empty component"),
        ("//a", true, "contains an empty component"),
        ("a/./b", true, "must not contain '.'"),
        ("a/../b", true, "must not contain '..'"),
        ("a/", false, "must not end with '/'"),
        ("a\0b", true, "contains NUL"),
    ] {
        let error = validate_filesystem_path(path, 7, "test", allows_trailing_slash).unwrap_err();
        assert_eq!(error.line, 7);
        assert!(error.message.contains(expected), "{path:?}: {error}");
    }
}

#[test]
fn validates_paths_after_variable_expansion() {
    for source in [
        "MAILDIR=\n",
        "EMPTY=\n:0\nmaildir:$EMPTY\n",
        "BAD=../escape\n:0\nmaildir:$BAD\n",
        "BAD=one//two\n:0\nmaildir:$BAD\n",
        "BAD=one/./two\n:0 :$BAD\nmaildir:target\n",
        "BAD=box/\n:0\nmbox:$BAD\n",
    ] {
        assert!(parse(source).unwrap().expand(&[]).is_err(), "{source:?}");
    }
}

#[test]
fn leaves_regex_patterns_unchanged() {
    let config = parse("NAME=value\n:0\n* ^Subject: $NAME$\ninbox/\n")
        .unwrap()
        .expand(&[])
        .unwrap();
    let Statement::Recipe(recipe) = &config.statements[1] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };

    assert_eq!(regex.pattern(), "^Subject: $NAME$");
}
