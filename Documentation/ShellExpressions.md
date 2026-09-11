<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Shell-like expressions

procmail-rs uses one bounded expression parser for assignment values,
destinations, lock paths, runtime rc paths, header values, and other documented
rc strings. The syntax resembles shell parameter expansion, but it does not
perform field splitting, globbing, tilde expansion, arithmetic expansion, or
general shell parsing.

## Variable references

`$NAME` and `${NAME}` insert the current byte value. An unbraced reference
consumes the longest valid name, so braces separate a name from following name
characters.

```text
DOMAIN=example.org
FIRST=$DOMAIN
SECOND=${DOMAIN}.archive
```

Names start with an ASCII letter or underscore and continue with ASCII letters,
digits, or underscores. Inserted values are literal data and are not scanned
again for more references.

## Positional parameters

Each repeatable command-line **-a** or **--argument** option supplies the next
positional parameter. `$1` and `${1}` select the first value, `$10` selects the
tenth value rather than `$1` followed by `0`, and `$#` or `${#}` reports the
number supplied. A permitted but absent positional parameter expands to an
empty string.

```text
procmail-rs filter -a work -a /home/user/Mail/work --config rules.rc
```

```text
ACCOUNT=$1

:0
* ACCOUNT ?? ^work$
maildir:${2}/
```

At most 256 arguments are accepted. One value is limited to 64 KiB and their
combined size to 256 KiB. Values are inserted literally without another
expansion pass. They are not exported to child commands under numeric
environment names. `$@`, `$*`, and `SHIFT` are not supported.

## Quotes, escapes, and comments

An assignment value is one shell-like word assembled from adjacent unquoted,
single-quoted, and double-quoted fragments.

```text
NAME=world
VALUE='literal $NAME / '"expanded $NAME"
```

Single quotes make every enclosed byte literal. Double quotes permit variable
and backquoted-command expansion. Within double quotes, backslash quotes only
`$`, backquote, `"`, backslash, and newline; before another byte it remains in
the value. Outside quotes, backslash makes the next byte literal and is removed.

```text
LITERAL=\${NAME}
ONE_BACKSLASH=\\$NAME
HASH=value\#part
QUOTED='# is data here'
```

An unquoted `#` begins a comment only at a word boundary. A second unquoted word
is rejected rather than silently discarded.

## Defaults and alternatives

The `-` forms provide defaults and the `+` forms provide alternatives. A colon
also treats an empty value as missing.

| Form | Selected result |
| --- | --- |
| `${NAME-word}` | `word` when `NAME` is unset, otherwise `NAME` |
| `${NAME:-word}` | `word` when `NAME` is unset or empty, otherwise `NAME` |
| `${NAME+word}` | `word` when `NAME` is set, otherwise empty |
| `${NAME:+word}` | `word` when `NAME` is set and non-empty, otherwise empty |

The selected `word` may be empty or contain nested expressions. An unselected
word is not evaluated, so it cannot run a command or report a missing nested
variable.

```text
ROOT=${MAILDIR:-${HOME}/Mail}
OPTION=${VERBOSE:+--verbose}
EMPTY_DEFAULT=${MISSING:-}
```

## Assignment and required values

`${NAME:=word}` assigns `word` when `NAME` is unset or empty and returns the
new value. It may change only ordinary user variables and is accepted only in
assignment values and destination expressions. All changes are held privately
until the complete outer expression succeeds.

```text
FOLDER=${CATEGORY:=unknown}

:0
maildir:${DESTINATION:=fallback}/
```

`${NAME:?word}` fails when `NAME` is unset or empty. The diagnostic identifies
the variable but neither evaluates nor reveals `word`.

```text
ARCHIVE=${ARCHIVE_ROOT:?private diagnostic text}/old
```

## Length, removal, and ASCII case

`${#NAME}` returns the byte length, not the number of Unicode characters.

```text
SIZE=${#MATCH}
```

Prefix and suffix removal use bounded shell patterns:

| Form | Operation |
| --- | --- |
| `${NAME#pattern}` | Remove the shortest matching prefix |
| `${NAME##pattern}` | Remove the longest matching prefix |
| `${NAME%pattern}` | Remove the shortest matching suffix |
| `${NAME%%pattern}` | Remove the longest matching suffix |

Patterns support `*`, `?`, byte classes, ranges, leading `!` or `^` class
negation, and backslash quoting.

```text
PATHNAME=lists/kernel/archive
FIRST=${PATHNAME%%/*}
LAST=${PATHNAME##*/}
WITHOUT_SUFFIX=${LAST%.archive}
```

ASCII case forms modify matching bytes without consulting the locale:

| Form | Operation |
| --- | --- |
| `${NAME^pattern}` | Uppercase the first byte when it matches |
| `${NAME^^pattern}` | Uppercase every matching byte |
| `${NAME,pattern}` | Lowercase the first byte when it matches |
| `${NAME,,pattern}` | Lowercase every matching byte |

An omitted pattern means `?`. Non-ASCII bytes remain unchanged.

```text
LOWER=${CATEGORY,,}
INITIAL=${CATEGORY^}
ONLY_HEX=${TOKEN^^[a-f]}
```

## Backquoted commands

A backquoted command can contribute bounded stdout to an assignment or
destination. Each command receives the complete current message on stdin, runs
under `SHELL SHELLFLAGS`, and has all trailing LF bytes removed from stdout.

```text
MONTH=`date +%Y-%m`
LABEL="prefix-`printf '%s' "$MATCH1"`"

:0
maildir:archive/`date +%Y`/
```

The child environment is rebuilt from bounded runtime rc variables rather than
inherited from the ambient process. `TIMEOUT` supervises input, output, and
termination; stderr is appended to `LOGFILE` when configured. Output is checked
against `LINEBUF` and the destination variable's fixed ceiling before the
assignment becomes visible.

A command-output recipe action has separate newline and status behavior:

```text
:0 hW
SUBJECT=| extract-subject
```

Here `h` selects headers, `b` selects the body, and neither selects the complete
message. `w` and `W` make a nonzero status fail the action; `W` suppresses its
diagnostic. `i` ignores only a failure while writing input, and `r` preserves
the selected input ending. Exactly one trailing LF is removed from successful
stdout.

## Shell-expanded conditions

A condition beginning with `$` expands its remainder using double-quoted rules
and reparses the bounded result as a condition.

```text
TEST='address From ?? .*@example\.org$'

:0
* $$TEST
from-example/
```

Procmail's `$\NAME` form inserts a variable as regex-quoted bytes:

```text
NEEDLE='literal.with.metacharacters'

:0
* $^Subject: $\NEEDLE$
selected/
```

Runtime-dependent shell-expanded conditions conservatively require replayable
complete input because their final condition type is unknown during planning.

## Limits and errors

`LINEBUF` bounds expanded rc values. Nesting, generated regexes, command output,
assignment values, paths, shell patterns, and child environments also have
fixed ceilings described in [Limits.md](Limits.md). Overflow, unsupported
operators, unterminated quotes, and malformed references fail explicitly;
values are never truncated or partially assigned.
