<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# procmail regex match-selection reference

These observations were generated with the Debian-patched reference binary
from `external/procmail-3.22/src/procmail`, which reports version 3.23pre. They
define the original behavior described in `procmailsc(5)`; the reference
binary is not a test dependency.

Each expression was matched from the start of the body. `MATCH` was assigned
`before` immediately before the condition.

| Body | Expression | Original `MATCH` | procmail-rs `MATCH` |
| --- | --- | --- | --- |
| `aa` | `^^\/(a\|aa)` | `aa` | `a` |
| `aa` | `^^\/(aa\|a)` | `aa` | `aa` |
| `aaa` | `^^\/a*` | `aaa` | `aaa` |
| `aabbb` | `^^(a\|aa)\/b*` | empty | empty |
| `aabbb` | `^^(aa\|a)\/b*` | empty | `bbb` |
| `aaa` | `^^(a\|aa\/a)` | `before` | `before` |
| `aaa` | `^^(aa\/a\|a)` | `before` | `a` |

The reference result does not depend on alternative order. It selects the
leftmost shortest match before the reached `\/` marker and the leftmost
longest suffix after it. A successful shorter alternative which does not
reach a marker preserves the preceding `MATCH` value.

procmail-rs uses the Rust byte-regex engine's leftmost-first selection. It
therefore agrees for unambiguous expressions and for some greedy repetitions,
but the order of ambiguous alternatives can change `MATCH` and whether a
marker participates. Reordering alternatives or changing repetition greediness
in the parsed tree cannot reproduce the reference behavior for every input.
