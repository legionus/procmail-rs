<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# procmail 3.22 command assignment reference

These observations were generated with the Debian-patched reference binary
from `external/procmail-3.22/src/procmail`. They define expected behavior for
maintained procmail-rs tests; the reference binary is not a test dependency.

The reference message was the following 33-byte input unless stated otherwise:

```text
Subject: test
X-Test: yes

body
```

## Backquoted assignment substitutions

| Rc value | Assigned bytes or result |
| --- | --- |
| `` `printf ''` `` | Empty value |
| `` `printf 'alpha\n\n\n'` `` | `alpha`; every trailing LF is removed |
| `` `wc -c` `` | `33`; each substitution receives the complete message independently |
| `` `printf 'failed-value'; exit 7` `` | `failed-value`; nonzero status does not prevent assignment |
| `` `printf 'stderr-value' >&2; printf 'stdout-value'` `` | `stdout-value`; stderr remains on the logging descriptor |
| `` pre`printf 'middle'`post`printf 'end'` `` | `premiddlepostend` |
| `` "before `printf 'two words'` after" `` | `before two words after` |
| `` `printf '\377'` `` | One byte `ff`; output is not restricted to UTF-8 |
| `` `printf 'left\000right'` `` | `left`; NUL terminates the value in the reference implementation |
| `` `procmail-reference-command-does-not-exist` `` | Empty value; the start failure is written to the logging descriptor |

## `NAME=| command` recipe actions

For the same message, `wc -c` produces these captured values:

| Flags | Assigned value | Selected input |
| --- | --- | --- |
| `h` | `27` | Header section including its empty separator line |
| `b` | `6` | Body only |
| none | `33` | Complete message |

`printf 'alpha\n\n\n'` assigns the bytes `alpha\n\n`: exactly one trailing LF
is removed. With `w`, a command that writes `failed-value` and exits 7 leaves
an absent destination variable absent; when the variable already contains
`old-value`, that earlier value is preserved. The failed command's stdout is
discarded.

A conditionally selected capture assigns its output. A skipped capture leaves
the variable absent. `A` runs after a successful capture, `E` does not, and
`e` runs after a waited command failure. These capture actions continue to the
following recipe after success.
