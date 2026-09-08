<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Resource limits

Every growing input structure has a finite ceiling. Rc-selectable limits take
effect in statement order and cannot exceed the compiled ceiling. A value of
zero is accepted for count and message limits and rejects the next byte or
item of the corresponding kind. The root rc file cannot select its own read
limit because it must be bounded before parsing starts.

Message byte counts accept an unsigned decimal integer with an optional `K`,
`M`, or `G` suffix, case-insensitively. Suffixes are binary multipliers: 1024,
1048576, and 1073741824. Whitespace, signs, fractions, and other suffixes are
rejected.

| Rc variable | Default | Hard ceiling | Counts |
| --- | ---: | ---: | --- |
| `LIMIT_MSG_SIZE` | 64 MiB | 256 MiB | Complete message |
| `LIMIT_MSG_HEADERS` | 256 KiB | 16 MiB | Header section, including separator |
| `LIMIT_MSG_BODY` | 64 MiB | 256 MiB | Body bytes |
| `LIMIT_HEADER_LINE` | 64 KiB | 1 MiB | One physical header line |
| `LIMIT_HEADER_FIELD` | 256 KiB | 16 MiB | One unfolded logical field |

The message limits count bytes as follows:

- `LIMIT_MSG_SIZE` covers the complete message, including raw headers, their
  separator, and the body. Streaming does not exempt body bytes. Replacement
  messages from filters and native header edits are checked again.
- `LIMIT_MSG_HEADERS` covers raw header bytes, line endings, and the empty
  header/body separator. Without a separator, all input through EOF is header
  data.
- `LIMIT_MSG_BODY` covers only bytes after the first empty header line. Without
  a separator, the body is empty.
- `LIMIT_HEADER_LINE` covers one physical line including its LF or CRLF, or all
  bytes of a final unterminated line.
- `LIMIT_HEADER_FIELD` covers the original bytes of one logical field: its
  first physical line plus immediately following space- or tab-led continuation
  lines, including line endings. Every component is also checked against
  `LIMIT_HEADER_LINE`.

Structural rc limits accept only unsigned decimal integers. The assignment
which changes a limit is checked using the preceding assignment limit; its new
value applies only to following syntax. These assignments are rejected inside
recipe blocks.

| Rc variable | Default | Hard ceiling | Counts |
| --- | ---: | ---: | --- |
| `LIMIT_MAX_ASSIGNMENTS` | 4096 | 65536 | Assignments across loaded rc files |
| `LIMIT_RC_STATEMENTS` | 4096 | 65536 | Statements across loaded rc files |
| `LIMIT_RC_RECIPES` | 1024 | 16384 | Recipes across loaded rc files |
| `LIMIT_RC_CONDITIONS` | 4096 | 65536 | Conditions across loaded rc files |
| `LIMIT_RC_REGEXES` | 256 | 1024 | Compiled regular expressions |
| `LIMIT_RECIPE_CONDITIONS` | 256 | 4096 | Conditions in one recipe |
| `LIMIT_RECIPE_NESTING` | 64 | 256 | Nested recipe blocks |

The structural limits count syntax as follows:

- `LIMIT_MAX_ASSIGNMENTS` covers ordinary assignments and `NAME=| command`
  capture actions across every loaded rc file. A limit-changing assignment is
  checked with the preceding value before the new value takes effect.
- `LIMIT_RC_STATEMENTS` covers assignment and recipe statements, including
  those in nested blocks. Conditions, actions, comments, and blank lines are
  not separate statements. A capture action is not a second statement beyond
  its recipe.
- `LIMIT_RC_RECIPES` covers every `:0` recipe. A block-owning recipe and each
  recipe inside that block count separately.
- `LIMIT_RC_CONDITIONS` covers every `*` condition, regardless of whether it is
  a regex, size test, program test, or shell-expanded condition.
- `LIMIT_RC_REGEXES` covers ordinary regex conditions and reserves one entry
  for every shell-expanded `$` condition which may produce a regex at runtime.
  Size and program conditions do not count.
- `LIMIT_RECIPE_CONDITIONS` applies independently to each recipe; those same
  conditions also contribute to `LIMIT_RC_CONDITIONS`.
- `LIMIT_RECIPE_NESTING` covers recipe-action blocks. Root recipes have depth
  zero and entering their `{ ... }` action creates depth one.

All message and structural limits accept zero. Exactly `limit` bytes or items
are permitted and the next byte or item is rejected. Lowering a structural
limit below its already consumed count is accepted; the next matching item is
rejected. Runtime includes and switches retain their caller's accumulated
counts and active settings.

`LINEBUF` defaults to 2048 bytes and accepts a literal decimal value from 128
through 1048576. It bounds following physical rc lines, a continued condition
or pipe command as a whole, and values produced by expansion. Generated
reserved regex text is exempt from `LINEBUF` but remains bounded by the regex
ceilings below.

Fixed ceilings that cannot be raised by an rc file are:

| Resource | Ceiling |
| --- | ---: |
| One rc file | 1 MiB |
| All rc files in one evaluation | 4 MiB |
| Rc files opened | 32 |
| Runtime include depth | 16 |
| Include/switch transitions | 256 |
| Check warnings / runtime rc warnings | 128 / 128 |
| Assignment name / value | 128 bytes / 64 KiB |
| Path expression | 4096 bytes |
| Pipe command | 64 KiB |
| Shell, shell flags, or PATH setting | 4096 bytes |
| Expansion nesting | 32 |
| Regex source / compiled program | 64 KiB / 8 MiB |
| Regex captures / one captured value | 64 / 64 KiB |
| `--set` entries | 256 |
| Child environment | 512 entries and 256 KiB |
| Pending delivery sinks | 256 |
| Trace | 16384 events and 1 MiB total |
| One trace event / detailed value prefix | 1024 / 256 bytes |
| Maildir or staging name attempts | 128 |

Command-output assignments apply two limits at once. Raw stdout is rejected
before newline removal when it exceeds the smaller of the active `LINEBUF` and
the fixed ceiling for the destination variable: 4096 bytes for `MAILDIR`,
`LOGFILE`, `LOCKFILE`, `LOCKEXT`, `SHELL`, `SHELLFLAGS`, and `PATH`, and 64 KiB
for other assignable variables. Literal and expanded fragments in a backquoted
assignment share that same final-value budget. The implementation does not
truncate command output or retain a partial assignment.

`TIMEOUT` defaults to 960 seconds and `LOCKTIMEOUT` to 1024 seconds. Both
accept decimal values from 1 through 86400; zero is rejected because waits
must be finite. `UMASK` accepts octal `0000` through `0777` and defaults to
`077`.
