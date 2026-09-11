<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# procmail-rs extensions

The rc language keeps the supported procmail forms needed for migration and
adds a small set of bounded operations for tasks which otherwise require an
external helper. These forms are intentionally not accepted by original
procmail.

## Native header editing

The `headers { ... }` action removes, replaces, adds, renames, or extracts
fields without passing the complete message through `formail`. Changes are
visible to every following recipe, including structured conditions and
external filters.

```text
:0
headers {
    remove X-Old-Spam-Status
    set X-Spam-Status clean
    add X-Processed-By procmail-rs
}
```

See [HeaderEditing.md](HeaderEditing.md) for every operation, value encoding,
failure behavior, and examples.

## Structured header conditions

`address` extracts mailbox addresses before matching, while `identifier`
extracts the identifier carried by `List-Id`. This avoids regular expressions
which accidentally consume display names, comments, brackets, or folding.

```text
:0
* address From,Reply-To ?? .*@example\.org$
from-example/

:0
* identifier List-Id ?? ^users\.lists\.example\.org$
lists/example-users/
```

See [StructuredConditions.md](StructuredConditions.md) for normalization,
supported fields, malformed input handling, and `MATCH` behavior.

## Extended shell-like expressions

Assignments and destination expressions support bounded parameter operations,
single and double quotes, escaping, and backquoted commands. This is a defined
rc expression language, not unrestricted shell evaluation.

```text
ROOT=${MAILDIR:-$HOME/Mail}
FOLDER="${PROJECT:=unknown}/archive"

:0
maildir:${ROOT}/${FOLDER}/
```

See [ShellExpressions.md](ShellExpressions.md) for the exact grammar and for
examples of lazy defaults, pattern removal, case conversion, quoting, and
command substitution.

## Explicit destination syntax

`maildir:PATH` and `mbox:PATH` select a delivery backend without consulting
mutable filesystem metadata. A trailing slash is also accepted for Maildir,
and an unmarked path selects mbox. The explicit forms are useful when a path
contains whitespace or when a configuration should state its storage format
directly.

```text
:0
maildir:lists/kernel/

:0 c
mbox:archive/all-mail
```

Delivery semantics, path resolution, locking, and durability are described in
[Delivery.md](Delivery.md).

## Numbered regex captures

Ordinary capturing groups populate `MATCH1`, `MATCH2`, and later numbered
variables. This complements procmail's `\/` marker and avoids adding several
markers merely to retain distinct parts of one match.

```text
:0
* ^Subject: \[([^]]+)\][ ]+(.*)$
maildir:projects/${MATCH1}/
```

All captured values share a fixed aggregate ceiling. A later evaluated regex
with captures replaces the preceding numbered values.

## Configurable resource controls

`LIMIT_MSG_*`, `LIMIT_HEADER_*`, and `LIMIT_RC_*` variables let a configuration
lower bounded parser and message limits without recompiling the program. They
take effect in statement order and cannot raise compiled ceilings.

```text
LIMIT_MSG_SIZE=10M
LIMIT_MSG_HEADERS=256K
LIMIT_HEADER_LINE=32K
LIMIT_RC_RECIPES=512
```

See [Limits.md](Limits.md) for the exact unit counted by every setting and its
default and maximum values.

`DURABILITY` selects publication synchronization, while `LOCKMETHOD` selects
the safer default `flock` behavior or the explicitly compatible `dotlock`
behavior:

```text
DURABILITY=full
LOCKMETHOD=flock
```

See [Delivery.md](Delivery.md) for the filesystem consequences of each mode.

## Diagnostic extensions

`filter --dry-run` evaluates rules and external filters but suppresses final
delivery, locks, and `TRAP`. Text traces are intended for interactive diagnosis;
JSON traces are intended for tools.

```text
procmail-rs filter --dry-run --detail values \
    --config ~/.procmailrc <message.eml

procmail-rs filter --dry-run --format json \
    --config ~/.procmailrc <message.eml
```

Dry-run mode does not sandbox commands named by the rc file. Conditions,
filters, and substitutions still run when their results affect later rules.
