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

`LINEBUF` defaults to 2048 bytes and accepts a literal decimal value from 128
through 1048576. It bounds following physical rc lines, a continued pipe
command as a whole, and values produced by expansion. Generated reserved regex
text is exempt from `LINEBUF` but remains bounded by the regex ceilings below.

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

`TIMEOUT` defaults to 960 seconds and `LOCKTIMEOUT` to 1024 seconds. Both
accept decimal values from 1 through 86400; zero is rejected because waits
must be finite. `UMASK` accepts octal `0000` through `0777` and defaults to
`077`.
