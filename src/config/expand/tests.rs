// SPDX-License-Identifier: MIT
// Copyright (C) 2026  Alexey Gladkov <legion@kernel.org>

use super::*;

fn parse_wide(input: &str) -> Result<Config, super::super::ParseError> {
    let mut state = super::super::RcParseState::default();
    state.limits.linebuf = super::super::MAX_LINEBUF;
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
            Statement::Include(_) | Statement::Switch(_) => {}
        }
    }
    panic!("statement is not a recipe");
}

#[test]
fn expands_both_variable_reference_forms_sequentially() {
    let config = parse("ROOT=mail\nBOX=${ROOT}/inbox\nMAILDIR=/srv/$ROOT\n:0\nmaildir:$BOX\n")
        .unwrap()
        .expand()
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
    .expand()
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

    let error = parse(":0 :\n| command\n").unwrap().expand().unwrap_err();
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
        .expand()
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
        .expand()
        .unwrap();
    let Statement::Assignment(assignment) = &config.statements[1] else {
        panic!("expected LOGABSTRACT assignment");
    };
    assert_eq!(assignment.value, "no");
    assert_eq!(assignment.target, AssignmentTarget::LogAbstract);

    let error = parse("MODE=all\n:0\n{\nLOGABSTRACT=$MODE\n}\n")
        .unwrap()
        .expand()
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
        .expand()
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
        .expand()
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
        .expand_with(&supplied)
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
    let config = parse("VALUE=$HOME\n")
        .unwrap()
        .expand_with(&supplied)
        .unwrap();
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
        .expand_with(&supplied)
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
        .expand_with(&supplied)
        .unwrap();
    let Statement::Assignment(assignment) = &config.statements[0] else {
        panic!("expected VERSION assignment");
    };

    assert_eq!(assignment.value, env!("CARGO_PKG_VERSION"));
}

#[test]
fn rejects_self_references_and_cycles_without_recursive_scanning() {
    for source in ["A=$A\n", "A=$B\nB=$A\n"] {
        let error = parse(source).unwrap().expand().unwrap_err();
        assert_eq!(error.line, 1);
        assert!(error.message.contains("is not defined"));
    }

    let supplied = [SuppliedVariable::parse("A=$A".into()).unwrap()];
    let error = parse("").unwrap().expand_with(&supplied).unwrap_err();
    assert_eq!(error.line, 0);
    assert_eq!(error.to_string(), "command line: variable A is not defined");
}

#[test]
fn enforces_expansion_depth_at_the_boundary() {
    let mut source = String::from("V0=value\n");
    for depth in 1..=MAX_EXPANSION_DEPTH {
        source.push_str(&format!("V{depth}=$V{}\n", depth - 1));
    }
    assert!(parse(&source).unwrap().expand().is_ok());

    source.push_str(&format!(
        "V{}=$V{}\n",
        MAX_EXPANSION_DEPTH + 1,
        MAX_EXPANSION_DEPTH
    ));
    let error = parse_wide(&source).unwrap().expand().unwrap_err();
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
        .expand()
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
fn rejects_undefined_forward_references() {
    let error = parse("A=$B\nB=value\n").unwrap().expand().unwrap_err();
    assert_eq!(error.message, "variable B is not defined");
}

#[test]
fn resolves_runtime_path_only_when_it_is_used() {
    let config = parse("MAILDIR=/mail\n:0\nmaildir:${LASTFOLDER}-related\n")
        .unwrap()
        .expand()
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
        .expand()
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
    for source in ["A=$$\n", "A=${NAME:=value}\n", "A=${NAME\n", "A=$\n"] {
        assert!(parse(source).unwrap().expand().is_err(), "{source:?}");
    }
}

#[test]
fn rejects_unsupported_procmail_variables_inside_expansions() {
    for name in super::super::UNSUPPORTED_PROCMAIL_VARIABLES {
        let value = format!("${{{name}}}");
        let error = parse(&format!("VALUE={value}\n"))
            .unwrap()
            .expand()
            .unwrap_err();
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
        .expand()
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
fn expands_shell_like_defaults_lazily() {
    let config = parse(
            "EMPTY=\nROOT=/mail\nA=${MISSING:-$ROOT/inbox}\nB=${EMPTY:-${MISSING:-fallback}}\nC=${ROOT:-$UNDEFINED}\n",
        )
        .unwrap()
        .expand()
        .unwrap();

    for (index, expected) in [(2, "/mail/inbox"), (3, "fallback"), (4, "/mail")] {
        let Statement::Assignment(assignment) = &config.statements[index] else {
            panic!("expected assignment");
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
    assert!(parse(&source).unwrap().expand().is_ok());

    let beyond_limit = format!("${{OUTER:-{within_limit}}}");
    let source = format!("A={beyond_limit}\n");
    let error = parse_wide(&source).unwrap().expand().unwrap_err();
    assert_eq!(
        error.message,
        format!("variable expansion exceeds the hard depth limit of {MAX_EXPANSION_DEPTH}")
    );
}

#[test]
fn resolves_runtime_defaults_without_rescanning_values() {
    let config = parse("MAILDIR=/mail\n:0\nmaildir:${LASTFOLDER:-$MAILDIR}/next\n")
        .unwrap()
        .expand()
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
fn bounds_expanded_paths_before_allocation_growth() {
    let source = format!(
        "A={}\nB={}\n:0\nmaildir:$A$B\n",
        "a".repeat(MAX_PATH_EXPRESSION_LEN),
        "b"
    );
    let error = parse_wide(&source).unwrap().expand().unwrap_err();

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
        let result = parse_wide(&source).unwrap().expand();

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
    let error = config.expand().unwrap_err();

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
    let error = parse_wide(&source).unwrap().expand().unwrap_err();

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
        let result = parse_wide(&source).unwrap().expand();
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
    let error = parse_wide(&source).unwrap().expand().unwrap_err();

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
        let result = parse_wide(&source).unwrap().expand();

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
        assert!(parse(source).unwrap().expand().is_err(), "{source:?}");
    }
}

#[test]
fn leaves_regex_patterns_unchanged() {
    let config = parse("NAME=value\n:0\n* ^Subject: $NAME$\ninbox/\n")
        .unwrap()
        .expand()
        .unwrap();
    let Statement::Recipe(recipe) = &config.statements[1] else {
        panic!("expected recipe");
    };
    let ConditionKind::Regex(regex) = &recipe.conditions[0].kind else {
        panic!("expected regex");
    };

    assert_eq!(regex.pattern(), "^Subject: $NAME$");
}
