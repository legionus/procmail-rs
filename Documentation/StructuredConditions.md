<!-- SPDX-License-Identifier: MIT -->
<!-- Copyright (C) 2026  Alexey Gladkov <legion@kernel.org> -->

# Structured header conditions

Raw header regexes operate on transport syntax. A display name, comment,
folding sequence, encoded text, or angle bracket can therefore obscure the
value a recipe intends to test. procmail-rs provides bounded `address` and
`identifier` conditions which parse selected field values before applying a
byte regex.

## Address matching

The syntax is:

```text
address FIELD[,FIELD...] ?? REGEX
```

Supported fields are `From`, `To`, `Cc`, `Sender`, and `Reply-To`. Field names
are ASCII case-insensitive, and repeated or unsupported names are rejected.

```text
:0
* address From ?? ^notifications@example\.org$
notifications/

:0
* address To,Cc ?? .*@lists\.example\.net$
example-lists/

:0
* address Sender,Reply-To ?? ^support(?:\+[^@]+)?@example\.com$
support/
```

Every mailbox is tested separately. Display names, nested comments, groups,
angle brackets, separators, and folding whitespace are discarded. For example,
the regex receives `Local.Part@example.org` from:

```text
From: "Example support" (ticket queue)
    <Local.Part@Example.ORG>
```

The local part retains its spelling and case. ASCII letters in the domain are
converted to lowercase. A quoted local part remains quoted:

```text
To: Group: first@example.org, "two words"@Example.NET;
```

The two regex inputs are `first@example.org` and
`"two words"@example.net`. Matching remains case-insensitive unless the recipe
has the `D` flag.

Malformed mailbox elements are ignored rather than reinterpreted as text. A
bad element does not hide a later valid mailbox when their list boundaries can
still be identified safely; unbalanced quotes, comments, or angle syntax may
make the complete field unusable. At most 4096 valid mailbox values are
presented to one condition; exceeding that ceiling fails message evaluation
explicitly.

## List identifier matching

`List-Id` is not an email address. Its dedicated condition is:

```text
identifier List-Id ?? REGEX
```

Only the identifier inside angle brackets is presented to the regex. ASCII
letters are converted to lowercase.

```text
:0
* identifier List-Id ?? ^linux-kernel\.vger\.kernel\.org$
lists/linux-kernel/
```

The descriptive phrase is ignored:

```text
List-Id: Linux kernel development <linux-kernel.vger.kernel.org>
```

A missing or malformed identifier does not match. `List-Id` is intentionally
separate from `address`; treating it as a mailbox would invent an at-sign and
make its list hierarchy ambiguous.

## Captures and negation

Capturing groups populate `MATCH1`, `MATCH2`, and later numbered variables.
The procmail `\/` marker populates `MATCH`. Captures belong to the first
mailbox or identifier which satisfies the regex.

```text
:0
* address From ?? ^([^+@]+)(?:\+[^@]+)?@example\.org$
maildir:senders/${MATCH1}/
```

Leading `!` negates the overall any-value result:

```text
:0
* ! address From ?? .*@trusted\.example$
untrusted/
```

This matches when none of the valid `From` mailboxes matches the expression.
It also matches a missing field. Use a separate raw header-presence condition
when missing and malformed fields must be distinguished.

## Interaction with header editing

Structured conditions read the current header, not only the message as it
arrived. A successful earlier `headers` action is therefore visible immediately:

```text
:0
headers {
    add Cc archive@example.com
}

:0
* address Cc ?? ^archive@example\.com$
dest/archive/
```

Conversely, a removed or renamed field no longer participates in later
structured matching.

## Planning and logging

Both condition forms require only the header section and do not by themselves
retain the body. The body can continue directly to an already selected
destination when no later operation needs it.

Metadata trace records identify only the condition kind, source line, and
result. Extracted addresses and identifiers are never logged. In values detail,
the bounded regex expression may be shown, following the same policy as other
regex conditions.

The keywords do not reserve ordinary variable names. These remain variable
conditions:

```text
* address ?? ^manual-value$
* identifier ?? ^another-value$
```
